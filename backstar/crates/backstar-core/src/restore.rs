//! Restoring files and folders out of a snapshot.
//!
//! Replaces `Start-RestoreRun` and `Resolve-RestoreOriginalPath`
//! (`app/BackStar.UI.HistoryRestore.ps1:169-183, 308-372`), with one structural
//! improvement: **every snapshot carries the original location of every source it
//! contains**, in [`crate::repo::SnapshotManifest::sources`]. The PowerShell app wrote a
//! `.backstar-manifest.json` only for non-mirror, non-restore copy jobs
//! (`app/BackStar.Engine.ps1:396-408`), so a folder that had only ever been backed up
//! through Live Sync -- `Mirror = $true` -- had no manifest at all, and every restore of it
//! fell through to "browse for a folder to put this in" (`HistoryRestore.ps1:337-353`).
//! Snapshots have no such gap: the manifest is written once, for the whole run, regardless
//! of job kind.
//!
//! A restore never deletes. It walks the requested subtree inside the snapshot and copies
//! it to the destination with [`crate::parallel::run_parallel`] -- the same temp-name-then-
//! rename discipline as backup, so restoring INTO a live, otherwise-untouched directory
//! can't leave a half-written file behind either.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::events::{Event, JobStats, RunOutcome};
use crate::exclude::ExcludeRules;
use crate::guards;
use crate::parallel::{self, CopyItem};
use crate::repo::{Repo, SnapshotManifest};
use crate::walk;
use crate::{Error, Result};

/// Given a manifest and a path relative to the snapshot root (e.g. `proj/sub/file.txt`,
/// where `proj` is a [`crate::repo::SnapshotSource::name`]), return where that item
/// originally lived on disk, if the manifest still names its source.
///
/// Returns `None` when `snapshot_rel` is empty (there is no single original for a whole
/// snapshot spanning multiple sources) or names a source the manifest does not have --
/// which can happen if a job's sources changed after this snapshot was taken.
pub fn resolve_original_path(manifest: &SnapshotManifest, snapshot_rel: &Path) -> Option<PathBuf> {
    let mut components = snapshot_rel.components();
    let source_name = components.next()?.as_os_str().to_str()?;
    let rest: PathBuf = components.collect();

    let source = manifest.sources.iter().find(|s| s.name == source_name)?;
    if rest.as_os_str().is_empty() {
        Some(source.original.clone())
    } else {
        Some(source.original.join(rest))
    }
}

/// Restore a file or directory out of a snapshot to an exact destination path.
///
/// `snapshot_rel` is relative to the snapshot's root (as [`resolve_original_path`] takes).
/// If it names a file, `dest` is the exact file path to write. If it names a directory,
/// `dest` is the directory its contents are restored into -- the caller decides whether
/// that is the original location itself, or a subfolder it has already computed; this
/// function does not invent a subfolder name on its own.
///
/// Never deletes anything at the destination: existing files are overwritten (via
/// temp-then-rename, same as backup), but nothing absent from the snapshot is removed.
pub fn restore(
    repo: &Repo,
    snapshot_id: &str,
    snapshot_rel: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<JobStats> {
    let started = Instant::now();
    let source = repo.snapshot_dir(snapshot_id).join(snapshot_rel);

    let source_meta = std::fs::symlink_metadata(&source)
        .map_err(|_| Error::other(format!(
            "nothing at {} in snapshot {snapshot_id}",
            snapshot_rel.display()
        )))?;

    // A restore target must not land inside the backup repository itself. Restoring a
    // snapshot into its own (or a sibling's) storage would write ordinary files into a tree
    // that other snapshots hardlink into -- the write itself is still safe (copy_file never
    // writes through a link), but the result is a backup repository polluted with restored
    // content, which is its own kind of corruption.
    guards::ensure_dest_safe(dest, &repo.root)?;

    let run_id = format!("restore:{snapshot_id}");
    let mut stats = JobStats::default();

    if source_meta.is_file() {
        emit(Event::RunStarted { run_id: run_id.clone(), jobs: 1 });
        emit(Event::PlanReady {
            to_copy: 1,
            to_link: 0,
            bytes_to_copy: source_meta.len(),
            // A restore never deletes.
            deletes: Some(0),
        });

        let items = vec![CopyItem {
            rel: snapshot_rel.to_path_buf(),
            from: source,
            to: dest.to_path_buf(),
            size: source_meta.len(),
            link_from: None,
        }];
        let counts = parallel::run_parallel(&items, cancel, emit);

        stats.files_copied = counts.copied;
        stats.files_failed = counts.failed;
        stats.bytes_copied = counts.bytes_copied;

        if counts.cancelled {
            stats.duration_ms = started.elapsed().as_millis() as u64;
            emit(Event::RunDone {
                run_id,
                outcome: RunOutcome::Cancelled,
                stats: stats.clone(),
            });
            return Ok(stats);
        }
    } else {
        // A directory: no exclusions apply on restore. Excluding `node_modules` at backup
        // time was a choice about what to protect; a restore hands back exactly what was
        // protected, not a further-filtered subset of it.
        let excludes = ExcludeRules::none().compile(&source, dest);
        let walked = walk::walk(&source, &excludes, |files, bytes| {
            emit(Event::ScanProgress { files_seen: files, bytes_seen: bytes });
        })?;

        emit(Event::RunStarted { run_id: run_id.clone(), jobs: walked.entries.len() });
        emit(Event::PlanReady {
            to_copy: walked.entries.len() as u64,
            to_link: 0,
            bytes_to_copy: walked.total_bytes,
            deletes: Some(0),
        });

        let items: Vec<CopyItem> = walked
            .entries
            .into_iter()
            .map(|entry| CopyItem {
                from: source.join(&entry.rel),
                to: dest.join(&entry.rel),
                size: entry.size,
                rel: entry.rel,
                link_from: None,
            })
            .collect();

        let counts = parallel::run_parallel(&items, cancel, emit);

        stats.files_copied = counts.copied;
        stats.files_failed = counts.failed;
        stats.bytes_copied = counts.bytes_copied;

        if counts.cancelled {
            stats.duration_ms = started.elapsed().as_millis() as u64;
            emit(Event::RunDone {
                run_id,
                outcome: RunOutcome::Cancelled,
                stats: stats.clone(),
            });
            return Ok(stats);
        }
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    let outcome = if cancel.load(Ordering::Relaxed) {
        RunOutcome::Cancelled
    } else if stats.files_failed > 0 {
        RunOutcome::Partial { failed: stats.files_failed }
    } else {
        RunOutcome::Ok
    };

    emit(Event::RunDone { run_id, outcome, stats: stats.clone() });
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Job, JobKind};
    use crate::engine;
    use crate::repo::SnapshotSource;

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    fn manifest_with(sources: Vec<SnapshotSource>) -> SnapshotManifest {
        SnapshotManifest {
            id: "2026-01-01T00-00-00Z".into(),
            job: "Test".into(),
            started: String::new(),
            finished: String::new(),
            sources,
            files_copied: 0,
            files_linked: 0,
            files_failed: 0,
            bytes_copied: 0,
            bytes_linked: 0,
            duration_ms: 0,
            outcome: RunOutcome::Ok,
            parent: None,
        }
    }

    #[test]
    fn resolves_a_file_inside_a_named_source() {
        let m = manifest_with(vec![SnapshotSource {
            name: "proj".into(),
            original: PathBuf::from(r"D:\work\proj"),
            files: 1,
            bytes: 1,
        }]);
        let got = resolve_original_path(&m, Path::new("proj/sub/file.txt"));
        assert_eq!(got, Some(PathBuf::from(r"D:\work\proj\sub\file.txt")));
    }

    #[test]
    fn resolves_the_source_root_itself() {
        let m = manifest_with(vec![SnapshotSource {
            name: "proj".into(),
            original: PathBuf::from(r"D:\work\proj"),
            files: 1,
            bytes: 1,
        }]);
        assert_eq!(
            resolve_original_path(&m, Path::new("proj")),
            Some(PathBuf::from(r"D:\work\proj"))
        );
    }

    #[test]
    fn an_unknown_source_name_resolves_to_nothing() {
        let m = manifest_with(vec![]);
        assert_eq!(resolve_original_path(&m, Path::new("ghost/file.txt")), None);
    }

    /// End-to-end: back a real tree up, then restore one file from the snapshot to a fresh
    /// location, and check the bytes match.
    #[test]
    fn restoring_a_single_file_reproduces_its_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("notes.txt"), "important notes");
        let backup = tmp.path().join("backup");

        let job = Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![src],
            dest: backup.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        let repo = Repo::open(&backup).unwrap();
        let restore_to = tmp.path().join("restored/notes.txt");
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj/notes.txt"),
            &restore_to,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(stats.files_copied, 1);
        assert_eq!(stats.files_failed, 0);
        assert_eq!(std::fs::read_to_string(&restore_to).unwrap(), "important notes");
    }

    /// The whole-tree case: restoring a directory must reproduce every file underneath it,
    /// preserving the relative structure.
    #[test]
    fn restoring_a_directory_reproduces_the_whole_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("sub/b.txt"), "bbb");
        touch(&src.join("sub/deep/c.txt"), "ccc");
        let backup = tmp.path().join("backup");

        let job = Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![src],
            dest: backup.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        let repo = Repo::open(&backup).unwrap();
        let restore_to = tmp.path().join("restored-proj");
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj"),
            &restore_to,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(stats.files_copied, 3);
        assert_eq!(std::fs::read_to_string(restore_to.join("a.txt")).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(restore_to.join("sub/b.txt")).unwrap(), "bbb");
        assert_eq!(std::fs::read_to_string(restore_to.join("sub/deep/c.txt")).unwrap(), "ccc");
    }

    /// The end-to-end version of [`resolve_original_path`]: after a real run, the
    /// manifest's own record of where a source came from must round-trip through restore.
    #[test]
    fn restore_to_original_location_uses_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "original content");
        let backup = tmp.path().join("backup");

        let job = Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![src.clone()],
            dest: backup.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        // Simulate the file being deleted from the source, then restored back.
        std::fs::remove_file(src.join("f.txt")).unwrap();

        let original = resolve_original_path(&manifest, Path::new("proj/f.txt")).unwrap();
        assert_eq!(original, src.join("f.txt"));

        let repo = Repo::open(&backup).unwrap();
        restore(
            &repo,
            &manifest.id,
            Path::new("proj/f.txt"),
            &original,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original content");
    }

    #[test]
    fn restoring_a_nonexistent_snapshot_path_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup");
        let repo = Repo::open_or_init(&backup, "Test").unwrap();
        std::fs::create_dir_all(repo.snapshot_dir("2026-01-01T00-00-00Z")).unwrap();

        let err = restore(
            &repo,
            "2026-01-01T00-00-00Z",
            Path::new("proj/ghost.txt"),
            &tmp.path().join("out.txt"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing at"));
    }

    /// The safety guard: restoring into the backup repository's own storage must be
    /// refused, not silently allowed to pollute snapshot data.
    #[test]
    fn restoring_into_the_repo_itself_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let backup = tmp.path().join("backup");

        let job = Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![src],
            dest: backup.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        // Try to restore back into the repo's own snapshots folder.
        let bad_dest = backup.join("snapshots").join("sneaky");
        let err = restore(
            &repo,
            &manifest.id,
            Path::new("proj/a.txt"),
            &bad_dest,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, Error::Overlap { .. }));
    }

    /// A cancel raised before any bytes move must produce zero copies and a Cancelled
    /// outcome, not a partial file.
    #[test]
    fn a_cancel_before_starting_produces_no_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        for i in 0..20 {
            touch(&src.join(format!("f{i}.txt")), "content");
        }
        let backup = tmp.path().join("backup");

        let job = Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![src],
            dest: backup.clone(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        let restore_to = tmp.path().join("restored");
        let cancel = AtomicBool::new(true);
        let mut events = Vec::new();
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj"),
            &restore_to,
            &cancel,
            &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(stats.files_copied, 0);
        assert!(events.iter().any(|e| matches!(
            e,
            Event::RunDone { outcome: RunOutcome::Cancelled, .. }
        )));
    }
}
