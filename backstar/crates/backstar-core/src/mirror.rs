//! Mirror jobs -- the replacement for the PowerShell app's Live Sync.
//!
//! A mirror job is deliberately NOT a snapshot job. It has no repository, no `.backstar/`
//! folder, no manifest, and no history: the destination is made to look exactly like the
//! source, right now, with anything at the destination that is absent from the source
//! **deleted**. That is what "Live Sync" always meant -- a live, working mirror of a folder
//! (into a second location, or back out of a backup folder onto a live project directory),
//! not a version of it. Bolting deletion onto the snapshot/repository machinery would
//! either purge history other snapshots hardlink into, or silently stop being a mirror at
//! all; keeping the two modes structurally separate is what lets each one be simple and
//! trustworthy at what it actually does.
//!
//! Two protections exist because this is the one destructive mode in the whole app:
//!
//! - [`run_mirror`] refuses outright if the destination is itself a BackStar snapshot
//!   repository (`.backstar/` present). Mirroring into one by mistake would delete backup
//!   history, not create one.
//! - Deletion candidates are computed by walking the destination with the SAME exclusion
//!   rules used to walk the source, rooted at both ends (`ExcludeRules::compile`,
//!   `exclude.rs`). This is what stops a mirror from purging a folder the exclusion list
//!   deliberately never copied in the first place -- a `node_modules` or `dist` the
//!   destination happens to have from being built there directly, for instance.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::config::Job;
use crate::engine::{self, unchanged};
use crate::events::{Event, FileError, JobStats, RunOutcome};
use crate::exclude::CompiledExcludes;
use crate::parallel::{self, CopyItem};
use crate::walk::{self, Entry};
use crate::{Error, Result};

/// What a mirror of one source against its destination subfolder would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SourceDiff {
    to_copy: Vec<Entry>,
    /// Relative paths, present at the destination but absent from the source.
    to_delete: Vec<PathBuf>,
}

fn diff_source(source: &Path, dest_root: &Path, excludes: &CompiledExcludes) -> Result<SourceDiff> {
    let walked_src = walk::walk(source, excludes, |_, _| {})?;

    let mut to_copy = Vec::new();
    for entry in &walked_src.entries {
        let at_dest = dest_root.join(&entry.rel);
        match std::fs::symlink_metadata(&at_dest) {
            Ok(md) if md.is_file() && unchanged(entry, md.len(), md.modified().ok()) => {}
            _ => to_copy.push(entry.clone()),
        }
    }

    let mut to_delete = Vec::new();
    if dest_root.is_dir() {
        // Excludes applied here too, and rooted at both ends by `ExcludeRules::compile` --
        // this is what stops a mirror from deleting a folder the source side never sent in
        // the first place, exactly the reverse-sync hazard the PowerShell app's own
        // `both-ends` exclusion rooting existed to close.
        let walked_dest = walk::walk(dest_root, excludes, |_, _| {})?;
        let src_set: HashSet<&Path> =
            walked_src.entries.iter().map(|e| e.rel.as_path()).collect();
        for e in &walked_dest.entries {
            if !src_set.contains(e.rel.as_path()) {
                to_delete.push(e.rel.clone());
            }
        }
    }

    Ok(SourceDiff { to_copy, to_delete })
}

/// Remove now-empty directories under `root`, deepest first. Best-effort: a directory that
/// cannot be removed (still has something in it this walk did not know to delete, or is
/// locked) is simply left behind rather than failing the whole run over it.
fn prune_empty_dirs(root: &Path) {
    fn walk_and_prune(dir: &Path) -> bool {
        let Ok(read) = std::fs::read_dir(dir) else { return false };
        let mut empty = true;
        for entry in read.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if walk_and_prune(&path) {
                    let _ = std::fs::remove_dir(&path);
                } else {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        empty
    }
    if root.is_dir() {
        walk_and_prune(root);
    }
}

/// A read-only summary of what a mirror run would do, without touching anything.
///
/// This is the confirmation step: computing it costs a full walk of both trees, same as
/// actually running the mirror would, but nothing is written. Errors are propagated rather
/// than papered over with a plausible-looking partial answer -- a confirmation dialog that
/// silently undercounts deletions ("will delete ~0 files") is far more dangerous than one
/// that fails outright and falls back to a generic warning.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPreview {
    pub to_add_or_update: u64,
    pub bytes_to_write: u64,
    pub to_delete: u64,
}

pub fn preview_mirror(job: &Job) -> Result<MirrorPreview> {
    ensure_not_a_repo(&job.resolve_dest())?;
    let sources = engine::resolve_sources(job)?;
    let dest = job.resolve_dest();

    let mut preview = MirrorPreview::default();
    for (name, src) in &sources {
        let dest_root = dest.join(name);
        let excludes = job.excludes.compile(src, &dest_root);
        let diff = diff_source(src, &dest_root, &excludes)?;
        preview.to_add_or_update += diff.to_copy.len() as u64;
        preview.bytes_to_write += diff.to_copy.iter().map(|e| e.size).sum::<u64>();
        preview.to_delete += diff.to_delete.len() as u64;
    }
    Ok(preview)
}

fn ensure_not_a_repo(dest: &Path) -> Result<()> {
    if dest.join(".backstar").is_dir() {
        return Err(Error::other(format!(
            "{} is a BackStar snapshot repository -- mirroring into it would delete backup \
             history, not create a backup. Point this job at a different folder.",
            dest.display()
        )));
    }
    Ok(())
}

/// What a completed mirror run produced.
///
/// Unlike [`crate::engine::run_job`], this carries [`JobStats`] and a bare [`RunOutcome`]
/// directly rather than a [`crate::repo::SnapshotManifest`] -- there is no snapshot, no
/// repository, and no "parent" to record, because a mirror job produces none of those. The
/// outcome is still surfaced explicitly (not just inferrable from `stats.files_failed`) so
/// a caller building a history entry never has to re-derive logic this function already
/// worked out once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorResult {
    pub stats: JobStats,
    pub outcome: RunOutcome,
}

/// Run a mirror job to completion, emitting progress as it goes.
pub fn run_mirror(
    job: &Job,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<MirrorResult> {
    let started = Instant::now();
    let dest = job.resolve_dest();
    ensure_not_a_repo(&dest)?;
    let sources = engine::resolve_sources(job)?;

    emit(Event::RunStarted { run_id: String::new(), jobs: sources.len() });

    let mut stats = JobStats::default();
    let mut cancelled = false;

    for (name, src) in &sources {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        let dest_root = dest.join(name);
        emit(Event::JobStarted { job: name.clone(), source: src.clone(), dest: dest_root.clone() });

        let excludes = job.excludes.compile(src, &dest_root);
        let diff = diff_source(src, &dest_root, &excludes)?;

        emit(Event::PlanReady {
            to_copy: diff.to_copy.len() as u64,
            to_link: 0,
            bytes_to_copy: diff.to_copy.iter().map(|e| e.size).sum(),
            // Always known for a mirror -- it is exactly what was just walked and diffed,
            // never the "could not determine" case that leaves this `None`.
            deletes: Some(diff.to_delete.len() as u64),
        });

        // Delete before copying. Cheap enough to do serially (each deletion is one syscall,
        // and mirror deletions are rarely the dominant cost of a run the way file content
        // is), and doing it first means a file that changed type from directory to plain
        // file (or vice versa) between runs never collides with the copy that replaces it.
        for rel in &diff.to_delete {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let p = dest_root.join(rel);
            match std::fs::remove_file(&p) {
                Ok(()) => stats.files_deleted += 1,
                Err(e) => {
                    stats.files_failed += 1;
                    emit(Event::FileFailed(FileError {
                        path: rel.clone(),
                        message: e.to_string(),
                    }));
                }
            }
        }
        prune_empty_dirs(&dest_root);

        if cancelled {
            break;
        }

        let items: Vec<CopyItem> = diff
            .to_copy
            .iter()
            .map(|e| CopyItem {
                rel: e.rel.clone(),
                from: src.join(&e.rel),
                to: dest_root.join(&e.rel),
                size: e.size,
                link_from: None,
            })
            .collect();
        let counts = parallel::run_parallel(&items, cancel, emit);

        stats.files_copied += counts.copied;
        stats.files_failed += counts.failed;
        stats.bytes_copied += counts.bytes_copied;
        if counts.cancelled {
            cancelled = true;
        }

        emit(Event::JobDone { job: name.clone(), stats: stats.clone() });
        if cancelled {
            break;
        }
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else if stats.files_failed > 0 {
        RunOutcome::Partial { failed: stats.files_failed }
    } else {
        RunOutcome::Ok
    };

    emit(Event::RunDone { run_id: String::new(), outcome: outcome.clone(), stats: stats.clone() });
    Ok(MirrorResult { stats, outcome })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JobKind;
    use crate::exclude::ExcludeRules;

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    fn mirror_job(sources: Vec<PathBuf>, dest: PathBuf) -> Job {
        Job {
            id: "m".into(),
            name: "Mirror".into(),
            sources,
            dest,
            dest_portable: None,
            kind: JobKind::Mirror,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        }
    }

    #[test]
    fn a_first_mirror_copies_everything_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("sub/b.txt"), "bbb");
        let dest = tmp.path().join("out");

        let stats = run_mirror(&mirror_job(vec![src], dest.clone()), &AtomicBool::new(false), &mut |_| {})
            .unwrap()
            .stats;

        assert_eq!(stats.files_copied, 2);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(std::fs::read_to_string(dest.join("proj/a.txt")).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(dest.join("proj/sub/b.txt")).unwrap(), "bbb");
    }

    /// The defining property of a mirror, proven directly: a file removed from the source
    /// is removed from the destination on the next run.
    #[test]
    fn a_file_deleted_from_the_source_is_deleted_from_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        touch(&src.join("doomed.txt"), "doomed");
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src.clone()], dest.clone());

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(dest.join("proj/doomed.txt").is_file());

        std::fs::remove_file(src.join("doomed.txt")).unwrap();
        let stats = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap().stats;

        assert_eq!(stats.files_deleted, 1);
        assert!(!dest.join("proj/doomed.txt").exists(), "deleted at the source must be gone at dest");
        assert!(dest.join("proj/keep.txt").is_file(), "the untouched file must survive");
    }

    #[test]
    fn an_unchanged_file_is_neither_copied_nor_deleted_on_a_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src], dest);

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let stats = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap().stats;

        assert_eq!(stats.files_copied, 0, "nothing changed, so nothing should be rewritten");
        assert_eq!(stats.files_deleted, 0);
    }

    #[test]
    fn a_changed_file_is_recopied_and_the_old_bytes_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        let file = src.join("doc.txt");
        touch(&file, "ORIGINAL");
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src.clone()], dest.clone());

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        std::fs::write(&file, "EDITED - LONGER").unwrap();
        let stats = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap().stats;

        assert_eq!(stats.files_copied, 1);
        // A mirror has exactly one copy of each file -- unlike a snapshot, there is no
        // older version left behind to check; the destination itself now holds the edit.
        assert_eq!(std::fs::read_to_string(dest.join("proj/doc.txt")).unwrap(), "EDITED - LONGER");
    }

    /// Excluded folders at the destination are protected from deletion, not just from
    /// being copied -- otherwise a reverse mirror (backup -> live project) would delete a
    /// project's own generated `node_modules` the moment it noticed the backup never had one.
    #[test]
    fn excluded_folders_at_the_destination_are_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        // Simulate a pre-existing generated folder at the destination that the source
        // side was never meant to send (or purge).
        touch(&dest.join("proj/node_modules/pkg/index.js"), "generated");

        let mut job = mirror_job(vec![src], dest.clone());
        job.excludes = ExcludeRules::project();

        let stats = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap().stats;

        assert_eq!(stats.files_deleted, 0, "the excluded folder must not be touched");
        assert!(dest.join("proj/node_modules/pkg/index.js").is_file());
    }

    #[test]
    fn empty_directories_left_behind_by_deletion_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src.clone()], dest.clone());

        // Seed a dest-side folder that will become entirely empty once its one file is
        // purged, to prove the folder itself gets cleaned up too.
        touch(&dest.join("proj/empty-me/leaf.txt"), "gone soon");
        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        // Constructing the "before" state directly (bypassing a first mirror run) so the
        // folder genuinely predates this run rather than having been created by it.
        std::fs::create_dir_all(dest.join("proj/empty-me")).unwrap();
        std::fs::write(dest.join("proj/empty-me/leaf.txt"), "gone soon").unwrap();

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(!dest.join("proj/empty-me").exists(), "the emptied directory should be pruned");
        assert!(dest.join("proj/keep.txt").is_file());
    }

    #[test]
    fn preview_matches_what_run_mirror_actually_does() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bb");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/doomed.txt"), "x");
        let job = mirror_job(vec![src], dest);

        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_add_or_update, 2);
        assert_eq!(preview.bytes_to_write, 5);
        assert_eq!(preview.to_delete, 1);

        let stats = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap().stats;
        assert_eq!(stats.files_copied, preview.to_add_or_update);
        assert_eq!(stats.files_deleted, preview.to_delete);
    }

    #[test]
    fn preview_touches_nothing_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/doomed.txt"), "x");
        let job = mirror_job(vec![src], dest.clone());

        preview_mirror(&job).unwrap();

        assert!(dest.join("proj/doomed.txt").exists(), "preview must not delete anything");
        assert!(!dest.join("proj/a.txt").exists(), "preview must not copy anything");
    }

    /// The safety guard this module exists to enforce: a mirror job pointed at what is
    /// actually a snapshot repository must be refused outright, both for a real run and
    /// for a preview -- purging a repository other snapshots hardlink into is exactly the
    /// failure mode `ensure_not_a_repo` closes.
    #[test]
    fn mirroring_into_a_snapshot_repository_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");

        let snapshot_job = crate::config::Job {
            id: "s".into(),
            name: "Snapshots".into(),
            sources: vec![src.clone()],
            dest: dest.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        engine::run_job(&snapshot_job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(dest.join(".backstar").is_dir(), "fixture should have created a real repo");

        let mirror = mirror_job(vec![src], dest);
        assert!(run_mirror(&mirror, &AtomicBool::new(false), &mut |_| {}).is_err());
        assert!(preview_mirror(&mirror).is_err());
    }

    #[test]
    fn a_cancel_mid_run_stops_further_deletion_and_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        for i in 0..400 {
            touch(&src.join(format!("f{i}.txt")), "content");
        }
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src], dest);

        let cancel = AtomicBool::new(false);
        let stats = run_mirror(&job, &cancel, &mut |e| {
            if matches!(e, Event::OverallProgress { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap().stats;

        assert!(stats.files_copied < 400, "a cancel mid-run must stop before finishing");
    }
}
