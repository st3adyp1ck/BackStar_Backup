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
//! can't leave a half-written file behind either. Entries inside the snapshot that cannot
//! be read are counted into `files_failed` and reported by path: a restore that quietly
//! skips part of a snapshot would look identical to a complete one. Empty directories are
//! recreated too (D8): they are part of the tree's shape, and the copy phase only ever
//! makes directories that have a file in them.
//!
//! Overwriting is a policy decision, not a default (D5/F22): [`restore`] takes an
//! [`OverwritePolicy`], and [`plan_restore`] computes the overwrite facts (how many
//! existing files, how many NEWER than the snapshot) before anything is written, sharing
//! the walk with the execution so the preview cannot drift from what the copy phase would
//! touch. A destination file newer than the snapshot copy is someone's unsaved work --
//! protecting it is the entire point of the policy.

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

/// Where a restored item lands inside a user-chosen destination folder (F23): "put a copy
/// of X in folder F" means `F.join(X's own name)`, whether X is a file or a directory.
/// Restoring file `proj/notes.txt` to `D:\Recovered` writes `D:\Recovered\notes.txt`;
/// restoring directory `proj` to `D:\Recovered` writes `D:\Recovered\proj\...`.
///
/// Shared by the installed app (`restore_cmd`) and the standalone `BackStar-Restore` tool
/// so the two can never drift apart on this rule. `item_rel` is the path inside the
/// snapshot, as passed to [`restore`]; its last component names the restored copy. The
/// empty path -- a whole snapshot root -- names no single item and is refused.
pub fn compute_destination(dest_folder: &Path, item_rel: &Path) -> Result<PathBuf> {
    let name = item_rel.file_name().ok_or_else(|| {
        Error::other(
            "a whole snapshot root cannot be restored into a folder -- pick a source folder \
             inside it (browse the snapshot to see them)",
        )
    })?;
    Ok(dest_folder.join(name))
}

/// D5/F22: what a restore may do to a destination file that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Refuse the whole restore if ANY destination file would be overwritten -- nothing
    /// is written at all. The safe default for unattended/CLI restores.
    Fail,
    /// Overwrite files whose destination is older-or-equal; a destination file NEWER
    /// than the snapshot copy is kept (counted in `files_skipped`, and the outcome is at
    /// least Partial -- "restored N, kept M newer files" is not a clean Ok).
    IfOlder,
    /// Overwrite unconditionally -- the pre-D5 behavior, available for callers that have
    /// already obtained explicit confirmation.
    Always,
}

/// Overwrite facts for one file a restore would write (D5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverwriteInfo {
    /// Run-relative path, matching the restore's own reporting.
    pub rel: PathBuf,
    /// The exact destination path that would be written.
    pub dest: PathBuf,
    /// A file already exists at the destination.
    pub exists: bool,
    /// The existing destination file is NEWER than the snapshot copy. The comparison is
    /// meaningful because copies preserve mtimes (the golden suite verifies it). An
    /// existing destination whose mtime cannot be read counts as newer: "cannot prove it
    /// is safe to overwrite" must not read as "safe".
    pub dest_newer: bool,
    /// The snapshot copy's modification time (RFC 3339) -- the version a restore would
    /// put back. Copies preserve mtimes, so this is also when the source file was last
    /// modified before the run. `None` only if the snapshot's own mtime is unreadable.
    pub src_mtime: Option<String>,
    /// The existing destination file's modification time (RFC 3339), so a confirmation
    /// dialog can show HOW new the at-risk work is, not just that it is newer. `None`
    /// when nothing exists at the destination, or when the mtime is unreadable (in which
    /// case `dest_newer` is already true -- see above).
    pub dest_mtime: Option<String>,
}

/// What a restore would write and overwrite, computed before anything is touched (D5).
/// The app renders `would_overwrite_newer` for its confirmation dialog; the CLI prints
/// both counts and requires `--yes` when either is nonzero.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestorePlan {
    /// One entry per file the restore would write (not just the overwrites).
    pub entries: Vec<OverwriteInfo>,
    pub would_overwrite: u64,
    /// The dangerous subset: destination newer than the snapshot copy.
    pub would_overwrite_newer: u64,
}

/// Overwrite facts for one would-be-written file. Shared by [`plan_restore`] and
/// [`restore`] so the preview and the execution can never disagree (F22).
///
/// Both mtimes ride along as RFC 3339 strings (the crate's one formatter, L2): the
/// confirmation dialog shows the at-risk file's actual date, not just "newer".
fn overwrite_info(rel: PathBuf, src_mtime: std::time::SystemTime, to: PathBuf) -> OverwriteInfo {
    let (exists, dest_newer, dest_mtime) = match std::fs::symlink_metadata(&to) {
        Ok(md) if md.is_file() => match md.modified() {
            Ok(t) => (true, t > src_mtime, Some(crate::repo::rfc3339(t.into()))),
            // An existing file whose mtime is unreadable cannot be proven safe to
            // overwrite -- protected as "newer", and its date is simply absent.
            Err(_) => (true, true, None),
        },
        // Absent, or not a plain file: no file content at stake.
        _ => (false, false, None),
    };
    OverwriteInfo {
        rel,
        dest: to,
        exists,
        dest_newer,
        src_mtime: Some(crate::repo::rfc3339(src_mtime.into())),
        dest_mtime,
    }
}

/// D5 pre-flight: compute what a restore would write and overwrite WITHOUT writing
/// anything. Shares the walk and the fact computation with [`restore`], so the plan
/// cannot drift from what the copy phase would touch.
///
/// Fails on an unreadable snapshot subtree, same as the mirror's diff does: a plan
/// computed over a tree with a hole would understate the overwrite count, and an
/// understated confirmation is worse than none.
pub fn plan_restore(
    repo: &Repo,
    snapshot_id: &str,
    snapshot_rel: &Path,
    dest: &Path,
) -> Result<RestorePlan> {
    let source = repo.snapshot_dir(snapshot_id).join(snapshot_rel);
    let source_meta = std::fs::symlink_metadata(&source).map_err(|_| {
        Error::other(format!("nothing at {} in snapshot {snapshot_id}", snapshot_rel.display()))
    })?;

    let mut plan = RestorePlan::default();
    if source_meta.is_file() {
        let info = overwrite_info(
            snapshot_rel.to_path_buf(),
            source_meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            dest.to_path_buf(),
        );
        plan.would_overwrite += info.exists as u64;
        plan.would_overwrite_newer += info.dest_newer as u64;
        plan.entries.push(info);
        return Ok(plan);
    }

    let excludes = ExcludeRules::none().compile(&source, dest);
    let walked = walk::walk(&source, &excludes, |_, _| {})?;
    if walked.unreadable > 0 {
        return Err(Error::SourceUnreadable {
            root: source.clone(),
            count: walked.unreadable,
            samples: walked
                .unreadable_paths
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    for entry in &walked.entries {
        let info = overwrite_info(entry.rel.clone(), entry.mtime, dest.join(&entry.rel));
        plan.would_overwrite += info.exists as u64;
        plan.would_overwrite_newer += info.dest_newer as u64;
        plan.entries.push(info);
    }
    Ok(plan)
}

/// Restore a file or directory out of a snapshot to an exact destination path.
///
/// `snapshot_rel` is relative to the snapshot's root (as [`resolve_original_path`] takes).
/// If it names a file, `dest` is the exact file path to write. If it names a directory,
/// `dest` is the directory its contents are restored into -- the caller decides whether
/// that is the original location itself, or a subfolder it has already computed; this
/// function does not invent a subfolder name on its own.
///
/// `policy` decides what happens to files that already exist at the destination
/// (D5/F22); see [`OverwritePolicy`]. `Fail` refuses before a single byte is written;
/// `IfOlder` keeps newer destination files (counted in `files_skipped`, outcome at least
/// Partial, plus one summarizing `RunWarning`); `Always` is the pre-D5 behavior. The
/// pre-flight view for a confirmation prompt is [`plan_restore`].
///
/// Never deletes anything at the destination: existing files are overwritten (via
/// temp-then-rename, same as backup) or kept per policy, but nothing absent from the
/// snapshot is removed.
pub fn restore(
    repo: &Repo,
    snapshot_id: &str,
    snapshot_rel: &Path,
    dest: &Path,
    policy: OverwritePolicy,
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
    let mut skipped_newer: Vec<PathBuf> = Vec::new();

    if source_meta.is_file() {
        // D5: the policy applies to a single-file restore too, before anything is
        // written.
        let info = overwrite_info(
            snapshot_rel.to_path_buf(),
            source_meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            dest.to_path_buf(),
        );
        if policy == OverwritePolicy::Fail && info.exists {
            return Err(Error::RestoreWouldOverwrite {
                dest: dest.to_path_buf(),
                count: 1,
                newer: info.dest_newer as u64,
            });
        }
        if policy == OverwritePolicy::IfOlder && info.dest_newer {
            skipped_newer.push(info.rel.clone());
        }

        emit(Event::RunStarted { run_id: run_id.clone(), jobs: 1 });
        emit(Event::PlanReady {
            to_copy: if skipped_newer.is_empty() { 1 } else { 0 },
            to_link: 0,
            bytes_to_copy: if skipped_newer.is_empty() { source_meta.len() } else { 0 },
            // A restore never deletes.
            deletes: Some(0),
        });

        if skipped_newer.is_empty() {
            let items = vec![CopyItem {
                rel: snapshot_rel.to_path_buf(),
                from: source,
                to: dest.to_path_buf(),
                size: source_meta.len(),
                link_from: None,
            }];
            let counts = parallel::run_parallel(&items, &parallel::ParallelOptions::unverified(), cancel, emit);

            stats.files_copied = counts.copied;
            stats.files_failed += counts.failed;
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
    } else {
        // A directory: no exclusions apply on restore. Excluding `node_modules` at backup
        // time was a choice about what to protect; a restore hands back exactly what was
        // protected, not a further-filtered subset of it.
        let excludes = ExcludeRules::none().compile(&source, dest);
        let walked = walk::walk(&source, &excludes, |files, bytes| {
            emit(Event::ScanProgress { files_seen: files, bytes_seen: bytes });
        })?;

        // Anything in the snapshot the walk could not read is a real restore failure:
        // counted (which makes the outcome Partial below) and named, never silently
        // skipped -- a restore that drops entries quietly looks identical to a complete
        // one.
        if walked.unreadable > 0 {
            stats.files_failed += walked.unreadable;
            for path in &walked.unreadable_paths {
                emit(Event::SourceUnreadable {
                    path: path.clone(),
                    message: "could not be read inside the snapshot -- it was NOT restored"
                        .into(),
                });
            }
        }
        stats.reparse_skipped += walked.reparse_skipped;

        // D5/F22: overwrite facts from the SAME walk the copy phase consumes -- the plan
        // and the execution cannot drift apart. `Fail` refuses the whole restore before
        // a single byte is written.
        let facts: Vec<OverwriteInfo> = walked
            .entries
            .iter()
            .map(|entry| overwrite_info(entry.rel.clone(), entry.mtime, dest.join(&entry.rel)))
            .collect();
        if policy == OverwritePolicy::Fail {
            let count = facts.iter().filter(|f| f.exists).count() as u64;
            if count > 0 {
                let newer = facts.iter().filter(|f| f.dest_newer).count() as u64;
                return Err(Error::RestoreWouldOverwrite { dest: dest.to_path_buf(), count, newer });
            }
        }

        // D8: recreate the tree's empty directories (and the destination root itself, in
        // case the subtree is entirely empty) -- the copy phase below only ever makes
        // directories that have a file in them. Best-effort: a failure loses tree shape,
        // not content, so it is logged rather than counted as a file failure.
        if let Err(e) = std::fs::create_dir_all(dest) {
            tracing::warn!("could not create {}: {e}", dest.display());
        }
        for rel in &walked.empty_dirs {
            if let Err(e) = std::fs::create_dir_all(dest.join(rel)) {
                tracing::warn!("could not create empty directory {}: {e}", dest.join(rel).display());
            }
        }

        // L1: `jobs` counts restore operations (one), not files -- the file count is a
        // walk result, visible in the PlanReady that follows.
        emit(Event::RunStarted { run_id: run_id.clone(), jobs: 1 });

        let mut items = Vec::new();
        for (entry, info) in walked.entries.iter().zip(&facts) {
            if policy == OverwritePolicy::IfOlder && info.dest_newer {
                skipped_newer.push(info.rel.clone());
                continue;
            }
            items.push(CopyItem {
                from: source.join(&entry.rel),
                to: dest.join(&entry.rel),
                size: entry.size,
                rel: entry.rel.clone(),
                link_from: None,
            });
        }

        emit(Event::PlanReady {
            to_copy: items.len() as u64,
            to_link: 0,
            bytes_to_copy: items.iter().map(|i| i.size).sum(),
            deletes: Some(0),
        });

        let counts = parallel::run_parallel(&items, &parallel::ParallelOptions::unverified(), cancel, emit);

        // `+=`, not `=`: the walk above may already have counted unreadable snapshot
        // entries into files_failed, and those must survive alongside copy failures.
        stats.files_copied = counts.copied;
        stats.files_failed += counts.failed;
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

    stats.files_skipped = skipped_newer.len() as u64;
    if !skipped_newer.is_empty() {
        emit(Event::RunWarning {
            message: format!(
                "kept {} destination file(s) that are newer than the snapshot -- they were \
                 NOT overwritten. Restore what you need from them first, or rerun with a \
                 different destination or policy.",
                skipped_newer.len()
            ),
        });
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    let outcome = if cancel.load(Ordering::Relaxed) {
        RunOutcome::Cancelled
    } else if stats.files_failed > 0 || stats.files_skipped > 0 {
        // Files failed or deliberately kept: the destination does not match the snapshot
        // either way, and a clean Ok would say it does.
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
            skipped_sources: vec![],
            copied_hashes: Default::default(),
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

    /// F23: the destination rule both tools share -- the item keeps its own name inside
    /// the chosen folder, file or directory alike.
    #[test]
    fn compute_destination_joins_the_items_own_name() {
        assert_eq!(
            compute_destination(Path::new(r"D:\Recovered"), Path::new("proj/notes.txt")).unwrap(),
            PathBuf::from(r"D:\Recovered\notes.txt"),
            "a file restores INTO the folder under its own name"
        );
        assert_eq!(
            compute_destination(Path::new(r"D:\Recovered"), Path::new("proj")).unwrap(),
            PathBuf::from(r"D:\Recovered\proj"),
            "a directory restores as a same-named subfolder -- never scattered"
        );
        assert_eq!(
            compute_destination(Path::new("out"), Path::new("proj/sub")).unwrap(),
            PathBuf::from("out/sub"),
            "nested items keep their leaf name"
        );
        assert!(
            compute_destination(Path::new("out"), Path::new("")).is_err(),
            "a whole snapshot root names no single item"
        );
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();

        let repo = Repo::open(&backup).unwrap();
        let restore_to = tmp.path().join("restored/notes.txt");
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj/notes.txt"),
            &restore_to,
            OverwritePolicy::Always,
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();

        let repo = Repo::open(&backup).unwrap();
        let restore_to = tmp.path().join("restored-proj");
        let mut events = Vec::new();
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj"),
            &restore_to,
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(stats.files_copied, 3);
        assert_eq!(std::fs::read_to_string(restore_to.join("a.txt")).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(restore_to.join("sub/b.txt")).unwrap(), "bbb");
        assert_eq!(std::fs::read_to_string(restore_to.join("sub/deep/c.txt")).unwrap(), "ccc");

        // D1 scope note: restores do NOT verify (their truth is the snapshot bytes), so
        // no VerifyDone fires and FileDone carries no hash -- but per-file progress does.
        let mut saw_file_done = false;
        for e in &events {
            match e {
                Event::VerifyDone { .. } => panic!("restores do not verify"),
                Event::FileDone { hash, .. } => {
                    saw_file_done = true;
                    assert_eq!(*hash, None, "no hash without verification");
                }
                _ => {}
            }
        }
        assert!(saw_file_done, "restores report per-file progress (F34)");
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();

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
            OverwritePolicy::Always,
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
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing at"));
    }

    /// F39: a crash-orphaned temp file inside a snapshot is an engine artifact, not
    /// content -- a restore must never hand it back, and (being strictly read-only on
    /// the repository) must leave it in place.
    #[test]
    fn restore_never_copies_temp_files_out_of_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup");
        let repo = Repo::open_or_init(&backup, "T").unwrap();
        let snap = repo.snapshot_dir("2026-01-01T00-00-00Z");
        touch(&snap.join("proj/real.txt"), "real");
        touch(&snap.join("proj/.bstmp-1-2-fragment"), "junk");
        touch(&snap.join("proj/sub/.backstar-tmp-9-dead"), "junk");

        let restore_to = tmp.path().join("out");
        let stats = restore(
            &repo,
            "2026-01-01T00-00-00Z",
            Path::new("proj"),
            &restore_to,
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(stats.files_copied, 1, "only real content is restored");
        assert!(restore_to.join("real.txt").is_file());
        assert!(!restore_to.join(".bstmp-1-2-fragment").exists());
        assert!(!restore_to.join("sub/.backstar-tmp-9-dead").exists());
        assert!(
            snap.join("proj/.bstmp-1-2-fragment").is_file(),
            "restore is read-only on the repository -- sweeping is the runs' job"
        );
    }

    /// D8/F37: empty directories in the snapshot are recreated on restore -- they are
    /// part of the tree's shape, and the copy phase only makes dirs that have files.
    /// Also L1: a directory restore reports itself as ONE operation, not a file count.
    #[test]
    fn restoring_a_directory_recreates_empty_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup");
        let repo = Repo::open_or_init(&backup, "T").unwrap();
        let snap = repo.snapshot_dir("2026-01-01T00-00-00Z");
        touch(&snap.join("proj/real.txt"), "real");
        std::fs::create_dir_all(snap.join("proj/empty")).unwrap();
        std::fs::create_dir_all(snap.join("proj/nested/empty-child")).unwrap();

        let restore_to = tmp.path().join("out");
        let mut events = Vec::new();
        let stats = restore(
            &repo,
            "2026-01-01T00-00-00Z",
            Path::new("proj"),
            &restore_to,
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(stats.files_copied, 1);
        assert!(restore_to.join("real.txt").is_file());
        assert!(restore_to.join("empty").is_dir(), "the empty dir is recreated");
        assert!(restore_to.join("nested/empty-child").is_dir(), "nested empty dir too");
        let started = events
            .iter()
            .find_map(|e| match e {
                Event::RunStarted { jobs, .. } => Some(*jobs),
                _ => None,
            })
            .expect("RunStarted");
        assert_eq!(started, 1, "one restore operation, not a file count");
    }

    /// F9: the restore guard relies on the same canonicalisation as everything else --
    /// a destination spelled through a junction INTO the repository must be refused, not
    /// written into the repo's storage. Before the longest-existing-prefix fix, the
    /// junction spelling compared lexically and slipped past the guard.
    #[cfg(windows)]
    #[test]
    fn restoring_through_a_junction_into_the_repo_is_refused() {
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        let junc = tmp.path().join("junc");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junc)
            .arg(&backup)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }

        // `junc` resolves into the repository, so `junc\sneaky` is repo storage spelled
        // differently -- and must be refused exactly as the direct spelling is.
        let err = restore(
            &repo,
            &manifest.id,
            Path::new("proj/a.txt"),
            &junc.join("sneaky"),
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, Error::Overlap { .. }), "expected Overlap, got: {err}");
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        // Try to restore back into the repo's own snapshots folder.
        let bad_dest = backup.join("snapshots").join("sneaky");
        let err = restore(
            &repo,
            &manifest.id,
            Path::new("proj/a.txt"),
            &bad_dest,
            OverwritePolicy::Always,
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        let restore_to = tmp.path().join("restored");
        let cancel = AtomicBool::new(true);
        let mut events = Vec::new();
        let stats = restore(
            &repo,
            &manifest.id,
            Path::new("proj"),
            &restore_to,
            OverwritePolicy::Always,
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

    /// F1: an unreadable entry inside the snapshot tree must be counted and reported,
    /// never skipped silently -- a restore that drops entries quietly looks identical to
    /// a complete one.
    #[cfg(windows)]
    #[test]
    fn an_unreadable_entry_inside_the_snapshot_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup");
        let repo = Repo::open_or_init(&backup, "T").unwrap();
        let snap = repo.snapshot_dir("2026-01-01T00-00-00Z");
        touch(&snap.join("proj/good.txt"), "good");
        // The deterministic stand-in for a permission error: a trailing-dot directory can
        // be created via the \\?\ prefix but not stat'ed through a normal path.
        let weird = format!(r"\\?\{}\bad.", snap.join("proj").display());
        if std::fs::create_dir(&weird).is_err() {
            eprintln!("skipping: cannot create a trailing-dot directory here");
            return;
        }

        let restore_to = tmp.path().join("out");
        let mut events = Vec::new();
        let stats = restore(
            &repo,
            "2026-01-01T00-00-00Z",
            Path::new("proj"),
            &restore_to,
            OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(stats.files_copied, 1, "the readable file restores fine");
        assert_eq!(stats.files_failed, 1, "the unreadable entry must be counted");
        assert!(
            events.iter().any(|e| matches!(e, Event::SourceUnreadable { .. })),
            "and it must be named in an event"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::RunDone { outcome: RunOutcome::Partial { .. }, .. }
        )));

        let _ = std::fs::remove_dir(&weird);
    }

    // ------------------------------------------------------- F22/D5: overwrite policy

    /// A backup of two files, then a destination with one OLDER and one NEWER file.
    /// Returns (repo, snapshot_id, dest). Dest `older.txt` predates the snapshot's
    /// mtimes; `newer.txt` postdates them.
    fn overwrite_fixture(tmp: &tempfile::TempDir) -> (Repo, String, PathBuf) {
        let src = tmp.path().join("proj");
        touch(&src.join("older.txt"), "snapshot older");
        touch(&src.join("newer.txt"), "snapshot newer");
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
        let manifest = engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let repo = Repo::open(&backup).unwrap();

        // mtimes preserved by the copy: the snapshot's files carry the SOURCE mtimes.
        let snap_older_mtime = std::fs::metadata(
            repo.snapshot_dir(&manifest.id).join("proj/older.txt"),
        )
        .unwrap()
        .modified()
        .unwrap();

        let dest = tmp.path().join("live");
        touch(&dest.join("older.txt"), "stale local");
        touch(&dest.join("newer.txt"), "LOCAL WORK -- newer than the snapshot");
        let set_mtime = |p: &Path, t: std::time::SystemTime| {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        };
        set_mtime(&dest.join("older.txt"), snap_older_mtime - std::time::Duration::from_secs(3600));
        set_mtime(&dest.join("newer.txt"), snap_older_mtime + std::time::Duration::from_secs(3600));
        (repo, manifest.id, dest)
    }

    /// The pre-flight probe: counts and per-file facts, with no writes.
    #[test]
    fn plan_restore_reports_overwrite_facts_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let plan = plan_restore(&repo, &id, Path::new("proj"), &dest).unwrap();

        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.would_overwrite, 2, "both dest files exist");
        assert_eq!(plan.would_overwrite_newer, 1, "only the newer one is dangerous");
        let newer = plan.entries.iter().find(|e| e.rel == Path::new("newer.txt")).unwrap();
        assert!(newer.dest_newer);
        let older = plan.entries.iter().find(|e| e.rel == Path::new("older.txt")).unwrap();
        assert!(older.exists && !older.dest_newer);
        // Wire shape for the app/CLI.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"wouldOverwriteNewer\":1"), "{json}");

        // Nothing was written.
        assert_eq!(std::fs::read_to_string(dest.join("older.txt")).unwrap(), "stale local");
    }

    /// Both mtimes ride every entry (the confirmation dialog shows the at-risk file's
    /// actual date, not just "newer"), and they agree with the `dest_newer` verdict the
    /// fixture was built around.
    #[test]
    fn plan_restore_carries_real_src_and_dest_mtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let plan = plan_restore(&repo, &id, Path::new("proj"), &dest).unwrap();
        let parse = |s: &str| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| panic!("not RFC 3339: {s:?}"))
        };
        for entry in &plan.entries {
            let src = parse(entry.src_mtime.as_deref().expect("the snapshot copy's date"));
            let dest_t = parse(entry.dest_mtime.as_deref().expect("existing file's date"));
            assert_eq!(
                dest_t > src,
                entry.dest_newer,
                "the displayed dates must tell the same story as the verdict ({:?})",
                entry.rel
            );
        }
        // The fixture's hour offsets survive the round trip: no degenerate epoch strings.
        let newer = plan.entries.iter().find(|e| e.rel == Path::new("newer.txt")).unwrap();
        let delta =
            parse(newer.dest_mtime.as_deref().unwrap()) - parse(newer.src_mtime.as_deref().unwrap());
        assert!(delta >= time::Duration::minutes(59), "the real skew, not a fallback: {delta}");
    }

    /// A destination that exists nowhere: no `dest_mtime`, and the snapshot's own date
    /// still rides along (the dialog lists what WILL be written, not just overwrites).
    #[test]
    fn plan_restore_on_a_fresh_destination_has_no_dest_mtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, _) = overwrite_fixture(&tmp);
        let fresh = tmp.path().join("fresh");
        let _ = std::fs::remove_dir_all(&fresh);

        let plan = plan_restore(&repo, &id, Path::new("proj"), &fresh).unwrap();
        assert_eq!(plan.would_overwrite, 0);
        for entry in &plan.entries {
            assert!(!entry.exists && !entry.dest_newer);
            assert_eq!(entry.dest_mtime, None, "nothing there -- no date to show");
            assert!(entry.src_mtime.is_some(), "the snapshot copy's date always rides");
        }
    }

    /// The wire names are the camelCase the UI's `OverwriteInfo` type mirrors.
    #[test]
    fn overwrite_info_serialises_mtimes_camel_case() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let plan = plan_restore(&repo, &id, Path::new("proj"), &dest).unwrap();
        let json = serde_json::to_value(&plan.entries[0]).unwrap();
        assert!(json.get("srcMtime").and_then(|v| v.as_str()).is_some(), "{json}");
        assert!(json.get("destMtime").and_then(|v| v.as_str()).is_some(), "{json}");
        assert!(json.get("src_mtime").is_none(), "{json}");
        assert!(json.get("dest_mtime").is_none(), "{json}");
    }

    /// Fail policy: refuses before a single byte moves, and the destination is untouched.
    #[test]
    fn fail_policy_refuses_the_whole_restore_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let err = restore(
            &repo, &id, Path::new("proj"), &dest,
            OverwritePolicy::Fail, &AtomicBool::new(false), &mut |_| {},
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::RestoreWouldOverwrite { count: 2, newer: 1, .. }),
            "got: {err}"
        );
        assert_eq!(std::fs::read_to_string(dest.join("older.txt")).unwrap(), "stale local");
        assert_eq!(
            std::fs::read_to_string(dest.join("newer.txt")).unwrap(),
            "LOCAL WORK -- newer than the snapshot"
        );
    }

    /// IfOlder: the newer destination file is kept (counted, warned, Partial), the older
    /// one is overwritten with the snapshot's bytes.
    #[test]
    fn if_older_keeps_newer_files_and_overwrites_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let mut events = Vec::new();
        let stats = restore(
            &repo, &id, Path::new("proj"), &dest,
            OverwritePolicy::IfOlder, &AtomicBool::new(false), &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("newer.txt")).unwrap(),
            "LOCAL WORK -- newer than the snapshot",
            "the newer file is protected"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("older.txt")).unwrap(),
            "snapshot older",
            "the older file is overwritten"
        );
        assert_eq!(stats.files_copied, 1);
        assert_eq!(stats.files_skipped, 1);
        assert!(events.iter().any(|e| matches!(e, Event::RunWarning { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::RunDone { outcome: RunOutcome::Partial { .. }, .. }
        )), "a restore that kept newer files is not a clean Ok");
    }

    /// Always: the pre-D5 behavior -- everything overwritten.
    #[test]
    fn always_overwrites_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);

        let stats = restore(
            &repo, &id, Path::new("proj"), &dest,
            OverwritePolicy::Always, &AtomicBool::new(false), &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files_copied, 2);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(std::fs::read_to_string(dest.join("newer.txt")).unwrap(), "snapshot newer");
    }

    /// A destination that exists nowhere: every policy restores fully.
    #[test]
    fn a_fresh_destination_ignores_the_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, _) = overwrite_fixture(&tmp);
        for policy in [OverwritePolicy::Fail, OverwritePolicy::IfOlder, OverwritePolicy::Always] {
            let dest = tmp.path().join("fresh");
            let _ = std::fs::remove_dir_all(&dest);
            let stats = restore(
                &repo, &id, Path::new("proj"), &dest, policy, &AtomicBool::new(false), &mut |_| {},
            )
            .unwrap();
            assert_eq!(stats.files_copied, 2);
            assert_eq!(stats.files_skipped, 0);
        }
    }

    /// The single-file branch applies the policy too: Fail on an existing target refuses
    /// before writing.
    #[test]
    fn fail_policy_applies_to_single_file_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);
        let target = dest.join("newer.txt");
        let err = restore(
            &repo, &id, Path::new("proj/newer.txt"), &target,
            OverwritePolicy::Fail, &AtomicBool::new(false), &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, Error::RestoreWouldOverwrite { count: 1, newer: 1, .. }));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "LOCAL WORK -- newer than the snapshot"
        );

        // And IfOlder on the same single file keeps it, counted.
        let stats = restore(
            &repo, &id, Path::new("proj/newer.txt"), &target,
            OverwritePolicy::IfOlder, &AtomicBool::new(false), &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_copied, 0);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "LOCAL WORK -- newer than the snapshot");
    }

    /// The plan and the execution share one walk (F22): the set of files the probe lists
    /// is exactly the set a full restore writes.
    #[test]
    fn the_plan_lists_exactly_what_the_restore_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, id, dest) = overwrite_fixture(&tmp);
        let _ = std::fs::remove_dir_all(&dest);

        let plan = plan_restore(&repo, &id, Path::new("proj"), &dest).unwrap();
        let mut planned: Vec<PathBuf> = plan.entries.iter().map(|e| e.rel.clone()).collect();
        planned.sort();

        restore(&repo, &id, Path::new("proj"), &dest, OverwritePolicy::Always, &AtomicBool::new(false), &mut |_| {})
            .unwrap();
        let mut written = Vec::new();
        let mut stack = vec![dest.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() { stack.push(p); } else { written.push(p.strip_prefix(&dest).unwrap().to_path_buf()); }
            }
        }
        written.sort();

        assert_eq!(planned, written, "the plan and the copy phase must agree exactly");
    }
}
