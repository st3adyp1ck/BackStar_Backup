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
//! Three protections exist because this is the one destructive mode in the whole app:
//!
//! - [`run_mirror`] refuses outright if the destination IS a BackStar snapshot
//!   repository, sits INSIDE one (any ancestor holds `.backstar`), or CONTAINS one
//!   anywhere in the tree being mirrored. Mirroring over a repository in any direction
//!   would delete backup history, not create a backup.
//! - The source walk must be COMPLETE. Any unreadable entry fails the preview and the run
//!   outright: a tree that could not be fully read looks exactly like one whose missing
//!   files were deleted, and building a deletion plan over that view would permanently
//!   delete real destination files.
//! - Deletion candidates are computed by walking the destination with the SAME exclusion
//!   rules used to walk the source, rooted at both ends (`ExcludeRules::compile`,
//!   `exclude.rs`). This is what stops a mirror from purging a folder the exclusion list
//!   deliberately never copied in the first place -- a `node_modules` or `dist` the
//!   destination happens to have from being built there directly, for instance.
//!
//! Smaller guards with the same theme:
//!
//! - Reparse points are never walked or deleted THROUGH: not inside the tree (the walk
//!   skips them), not at the destination subfolder root ([`diff_source`] refuses it), and
//!   not by [`prune_empty_dirs`], which uses `symlink_metadata` and an explicit stack so
//!   a junction-to-ancestor can neither redirect pruning outside the destination nor
//!   overflow the call stack.
//! - The interactive run can be pinned to the preview the user confirmed:
//!   [`run_mirror_with_expected_deletes`] aborts before touching anything when the fresh
//!   delete set contains entries the preview never showed, because files created at the
//!   destination between confirmation and execution must not be deleted unreviewed.
//! - Runs against one destination are mutually exclusive ACROSS processes (F3): the GUI's
//!   in-process lock cannot see the headless scheduler, so [`crate::runlock::RunLock`]
//!   holds `.backstar-mirror.lock` at the destination root for the whole run (a mirror
//!   has no repository whose `.backstar/` could hold it).
//! - Crash-orphaned copy temp files (`.bstmp-*`, legacy `.backstar-tmp-*`) found during
//!   the destination walk are swept as housekeeping (F39) -- never enumerated into the
//!   delete plan, never counted as user deletions, and source-side ones are never touched
//!   (a backup tool never deletes from the source).
//! - Empty directories mirror the source's shape (D8/F37): destination dirs that are empty
//!   but exist in the source are created/kept; only empty dirs the source does NOT have
//!   are pruned.
//! - A mid-loop error (an unreadable source, a drift abort) emits a `RunDone` with a
//!   `Failed` outcome and the ACCUMULATED stats before returning the error (F30), so the
//!   shell's history records the copies and deletions that actually happened rather than
//!   losing the whole run's work with the error.
//! - Deletions never destroy on interruption (D3/F65): files slated for deletion are
//!   MOVED into `.backstar-trash-<run-id>` at the destination root (same volume, so one
//!   cheap rename each), the copies then run, and the trash is purged at the end of the
//!   run -- or at the start of the NEXT one, if this one died first. A cancel moves the
//!   trash contents back to their original places. The old order (delete first, copy
//!   second) could leave the destination missing files it held before the run; staged
//!   trash cannot. Peak extra space: the deleted set for the length of the run, plus the
//!   usual in-flight copy temps for updated files.
//!
//! Copied content is verified exactly as backups are (D1): every file copied is hashed and
//! re-read at the destination in a post-copy pass, and a mismatch is a failure whose bad
//! copy is removed -- a mirror must not keep proven-corrupt bytes as the live file.
//!
//! A subtler correctness rule lives in [`diff_source`]: delete-set membership compares
//! case-insensitively, because NTFS does. The unchanged-check (`dest_root.join(rel)`)
//! already resolves case-insensitively, so a case-only rename (`README.txt` vs
//! `readme.txt`) is never queued for a recopy -- and with a case-sensitive delete-set
//! comparison it would then be deleted without being recopied: file gone, outcome `Ok`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use time::OffsetDateTime;

use crate::config::Job;
use crate::engine::{self, unchanged};
use crate::events::{Event, FileError, JobStats, RunOutcome};
use crate::exclude::{normalize_for_compare, CompiledExcludes};
use crate::guards;
use crate::parallel::{self, CopyItem};
use crate::walk::{self, Entry};
use crate::{Error, Result};

/// The name prefix of the per-run staged-trash directory at a mirror destination's root
/// (D3): `.backstar-trash-<run-id>`. It lives at the destination ROOT -- a sibling of the
/// per-source subfolders -- so it is never inside a walked destination tree, never in a
/// delete plan, and never in a preview count. The walker also skips these names anywhere
/// (a stale trash inside a tree that some OTHER job backs up must not be snapshotted).
pub(crate) const TRASH_PREFIX: &str = ".backstar-trash-";

/// Remove `.backstar-trash-*` directories left at the destination root by a run that died
/// between staging deletions and purging them (D3).
///
/// The trash's contents are, by construction, files that run intended to delete -- and
/// this run's fresh diff re-copies anything still wanted from the source -- so a stale
/// trash is purged, not restored. The accepted edge: a CANCELLED run whose move-back
/// failed, plus a source that has since lost those files, means the trash held the last
/// copies; mirror semantics (the destination follows the source) purge them here rather
/// than resurrect them. Best-effort per dir: one that refuses deletion stays (the walker
/// never enumerates it into a plan) and is retried next run.
fn purge_stale_trash(dest: &Path) {
    let Ok(rd) = std::fs::read_dir(dest) else { return };
    for entry in rd.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_trash_dir = name.starts_with(TRASH_PREFIX)
            && entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false);
        if is_trash_dir {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!("could not purge stale trash {}: {e}", entry.path().display());
            }
        }
    }
}

/// What a mirror of one source against its destination subfolder would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SourceDiff {
    to_copy: Vec<Entry>,
    /// Relative paths, present at the destination but absent from the source.
    to_delete: Vec<PathBuf>,
    /// Reparse points skipped on both sides. Never followed (D7), always counted.
    reparse_skipped: u64,
    /// Destination entries that could not be read. A dest-side hole can only UNDER-delete
    /// (the safe direction -- the file simply stays), but it is counted into
    /// `files_failed` so the run reports Partial rather than hiding it.
    dest_unreadable: u64,
    /// Sample of the unreadable destination paths, for the warning events.
    dest_unreadable_paths: Vec<PathBuf>,
    /// Crash-orphaned BackStar copy temp files found at the destination during the walk
    /// (absolute paths). Never part of the delete plan -- the walk withholds them from
    /// `entries` -- so the run sweeps them as housekeeping (F39), outside the preview's
    /// expected-delete set and outside `files_deleted`.
    dest_temp_files: Vec<PathBuf>,
    /// Empty directories in the SOURCE (relative paths; D8/F37). A mirror makes the
    /// destination match the source's SHAPE too: these are created at the destination
    /// and protected from the empty-dir prune, which still removes empty dirs the
    /// source does not have.
    src_empty_dirs: Vec<PathBuf>,
}

fn diff_source(
    source: &Path,
    dest_root: &Path,
    excludes: &CompiledExcludes,
    mtime_tolerance: std::time::Duration,
) -> Result<SourceDiff> {
    let walked_src = walk::walk(source, excludes, |_, _| {})?;

    // Fail-closed: no diff is ever computed over a source tree that could not be fully
    // read. The unreadable entries are indistinguishable from source-side deletions, and
    // mirroring that view would permanently delete real destination files. This is the
    // single guard that makes "no deletion plan over an unreadable tree" structural
    // rather than a convention -- preview and run both go through here.
    if walked_src.unreadable > 0 {
        return Err(Error::SourceUnreadable {
            root: source.to_path_buf(),
            count: walked_src.unreadable,
            samples: walked_src
                .unreadable_paths
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    // A destination subfolder that is itself a reparse point must never be walked or
    // deleted through: `is_dir()` follows it into its target, and the delete pass would
    // then empty a directory that is not the destination at all.
    if let Ok(md) = std::fs::symlink_metadata(dest_root) {
        if md.file_type().is_symlink() {
            return Err(Error::ReparseRoot(dest_root.to_path_buf()));
        }
    }

    // A snapshot repository directly AT the destination subfolder (checked here, before
    // the walk, so even an empty one is caught); nested ones are caught after the walk
    // below. Mirroring over either purges backup history.
    if dest_root.join(crate::repo::META_DIR).is_dir() {
        return Err(Error::RepoInsideDestination(dest_root.to_path_buf()));
    }

    let mut to_copy = Vec::new();
    for entry in &walked_src.entries {
        let at_dest = dest_root.join(&entry.rel);
        match std::fs::symlink_metadata(&at_dest) {
            Ok(md) if md.is_file() && unchanged(entry, md.len(), md.modified().ok(), mtime_tolerance) => {}
            _ => to_copy.push(entry.clone()),
        }
    }

    let mut diff = SourceDiff {
        to_copy,
        reparse_skipped: walked_src.reparse_skipped,
        src_empty_dirs: walked_src.empty_dirs,
        ..Default::default()
    };

    if dest_root.is_dir() {
        // Excludes applied here too, and rooted at both ends by `ExcludeRules::compile` --
        // this is what stops a mirror from deleting a folder the source side never sent in
        // the first place, exactly the reverse-sync hazard the PowerShell app's own
        // `both-ends` exclusion rooting existed to close.
        let walked_dest = walk::walk(dest_root, excludes, |_, _| {})?;
        diff.reparse_skipped += walked_dest.reparse_skipped;
        diff.dest_unreadable = walked_dest.unreadable;
        diff.dest_unreadable_paths = walked_dest.unreadable_paths;
        diff.dest_temp_files = walked_dest.temp_files;

        // A `.backstar` ANYWHERE under the destination subfolder means a repository is
        // nested inside the tree being mirrored. `repo.json` makes a real repository
        // detectable by its files; refusing the run (rather than merely skipping the
        // folder) is deliberate -- a mirror whose destination contains a repository is a
        // configuration mistake, and quietly "succeeding" around it is the silent shape
        // this mode exists to avoid.
        if let Some(hit) = walked_dest
            .entries
            .iter()
            .find(|e| e.rel.components().any(|c| c.as_os_str() == crate::repo::META_DIR))
        {
            return Err(Error::RepoInsideDestination(dest_root.join(&hit.rel)));
        }

        // Keyed on the case-folded rel path: the unchanged-check above resolves
        // case-insensitively on NTFS, so a case-only rename is never queued for a recopy
        // -- and must therefore never look absent from the source either.
        let src_set: HashSet<String> =
            walked_src.entries.iter().map(|e| normalize_for_compare(&e.rel)).collect();
        for e in &walked_dest.entries {
            if !src_set.contains(&normalize_for_compare(&e.rel)) {
                diff.to_delete.push(e.rel.clone());
            }
        }
    }

    Ok(diff)
}

/// Remove now-empty directories under `root`, deepest first -- except those in `keep`
/// (case-folded relative paths of empty dirs that exist in the SOURCE; D8: a mirror keeps
/// the source's shape, so a dest dir the source also has is not "left behind", it is
/// correct). Kept dirs are still descended into: an empty dir nested INSIDE a kept one
/// that the source does not have is still pruned.
///
/// Rewritten to be safe by construction, because the old recursive version had three
/// failure modes, each of them quiet:
///
/// - it used `is_dir()`, which FOLLOWS junctions -- a junction at the destination
///   pointing outside the tree had its TARGET pruned;
/// - it ignored the exclusion rules, so a protected folder (one the mirror is forbidden
///   to delete) was still removed once empty;
/// - it recursed, so a deep or adversarial tree could overflow the call stack.
///
/// Now: an explicit stack, `symlink_metadata` throughout, reparse points neither
/// descended into nor removed, exclusion-protected directories never removed, and
/// `.backstar` directories never removed (a repository's metadata dir is not the
/// mirror's business, whatever the delete set says). Best-effort as before: a directory
/// that cannot be removed (still has something in it, or is locked) is left behind
/// rather than failing the whole run over it.
fn prune_empty_dirs(root: &Path, excludes: &CompiledExcludes, keep: &HashSet<String>) {
    let Ok(root_md) = std::fs::symlink_metadata(root) else { return };
    if !root_md.is_dir() || root_md.file_type().is_symlink() {
        return;
    }

    // Collect candidate directories first, then prune deepest-first: removing a child
    // can make its parent removable, and a reversed pre-order visits every descendant
    // before its ancestors.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            if !md.is_dir() || md.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == crate::repo::META_DIR || excludes.is_dir_excluded(&path, &name) {
                continue;
            }
            dirs.push(path.clone());
            stack.push(path);
        }
    }

    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in dirs {
        if let Ok(rel) = dir.strip_prefix(root) {
            if keep.contains(&normalize_for_compare(rel)) {
                continue;
            }
        }
        // Fails harmlessly when the directory is not (or no longer) empty.
        let _ = std::fs::remove_dir(&dir);
    }
}

/// One file the preview plans to delete, tied to its source subfolder so a later
/// [`run_mirror_with_expected_deletes`] call can match it per source.
///
/// "Delete" is the user-facing semantics (and the preview wording); mechanically the run
/// MOVES the file into the per-run staged trash (D3) and purges it once the copies have
/// landed. The names stay "delete" because that is what the user confirmed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorDelete {
    /// The destination subfolder (source name) the deletion lives under.
    pub source: String,
    /// Path relative to that subfolder.
    pub rel: PathBuf,
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
    /// The exact delete set behind `to_delete`, for handing to
    /// [`run_mirror_with_expected_deletes`] once the user confirms. Counts alone cannot
    /// catch preview/execute drift; the paths can.
    pub delete_set: Vec<MirrorDelete>,
}

pub fn preview_mirror(job: &Job) -> Result<MirrorPreview> {
    // Canonicalise once, up front: the guard, the diff roots and (in run_mirror) every
    // delete/copy must all work on the same spelling of the destination.
    let dest = guards::canonical_path(job.resolve_dest())?;
    ensure_not_a_repo(&dest)?;
    let resolved = engine::resolve_sources(job)?;

    let mut preview = MirrorPreview::default();
    for (name, src) in &resolved.sources {
        let dest_root = dest.join(name);
        let excludes = job.excludes.compile(src, &dest_root);
        let mtime_tolerance = engine::mtime_tolerance_between(src, &dest_root);
        let diff = diff_source(src, &dest_root, &excludes, mtime_tolerance)?;
        preview.to_add_or_update += diff.to_copy.len() as u64;
        preview.bytes_to_write += diff.to_copy.iter().map(|e| e.size).sum::<u64>();
        preview.to_delete += diff.to_delete.len() as u64;
        preview.delete_set.extend(
            diff.to_delete.into_iter().map(|rel| MirrorDelete { source: name.clone(), rel }),
        );
    }
    Ok(preview)
}

/// Refuse to mirror into, or INTO THE MIDDLE OF, a snapshot repository.
///
/// Two directions, both fatal before anything is planned:
///
/// - the destination IS a repository or sits inside one: any ancestor -- the
///   destination itself included -- holds a `.backstar` folder. Mirroring into
///   `<repo>/snapshots` would interleave live files with backup history and then
///   "reconcile" the difference by deleting the history.
/// - the destination CONTAINS a repository below it: detected in [`diff_source`]
///   (immediate `.backstar` child, and any `.backstar` component in the walked
///   destination tree), because that check needs the per-source walk this function
///   does not have.
fn ensure_not_a_repo(dest: &Path) -> Result<()> {
    for ancestor in dest.ancestors() {
        if ancestor.join(crate::repo::META_DIR).is_dir() {
            return Err(Error::other(format!(
                "{} is or sits inside a BackStar snapshot repository -- mirroring into it \
                 would delete backup history, not create a backup. Point this job at a \
                 different folder.",
                dest.display()
            )));
        }
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

/// F30: a mid-loop error used to discard every stat accumulated so far, leaving no
/// truthful record of the files already copied or deleted. Emit the run's end with what
/// actually happened BEFORE returning the error, so the shell's history records reality.
fn emit_failed_run(stats: &JobStats, started: Instant, emit: &mut dyn FnMut(Event), reason: String) {
    let mut stats = stats.clone();
    stats.duration_ms = started.elapsed().as_millis() as u64;
    emit(Event::RunDone {
        run_id: String::new(),
        outcome: RunOutcome::Failed { reason },
        stats,
    });
}

/// Run a mirror job to completion, emitting progress as it goes.
///
/// This is the headless/scheduled entry point: that path has no preview, so there is no
/// confirmed delete set to check the run against and it passes `None` -- the behaviour is
/// exactly what it always was, documented here so the absence of the drift check in this
/// path is a decision rather than an oversight.
///
/// The interactive path must instead call [`run_mirror_with_expected_deletes`] with the
/// delete set from the preview the user confirmed, so files created at the destination
/// between confirmation and execution are never deleted unreviewed.
pub fn run_mirror(
    job: &Job,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<MirrorResult> {
    run_mirror_with_expected_deletes(job, None, cancel, emit)
}

/// Run a mirror job, pinned to the delete set the user confirmed at preview time.
///
/// `expected_deletes` is [`MirrorPreview::delete_set`] from the preview that was shown
/// for confirmation, or `None` when there was no preview (headless/scheduled runs). When
/// it is provided and the FRESH delete set -- computed from the destination as it is
/// now, moments before mutation -- contains anything the preview did not show, the run
/// aborts with [`Error::DestinationChangedSincePreview`] before a single file is touched
/// on that source. Each source is validated right after its (read-only) diff, so every
/// file ever deleted was present in a confirmed preview.
pub fn run_mirror_with_expected_deletes(
    job: &Job,
    expected_deletes: Option<&[MirrorDelete]>,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<MirrorResult> {
    let started = Instant::now();
    // Canonicalise once, up front, and use this spelling for the repo guard, the diff
    // roots, and every delete and copy the run performs.
    let dest = guards::canonical_path(job.resolve_dest())?;
    ensure_not_a_repo(&dest)?;
    let resolved = engine::resolve_sources(job)?;

    // One run per destination at a time, across processes: the GUI's in-process Runner
    // mutex cannot see the headless `--run-job` process (and vice versa). Held for the
    // rest of the run; a contender gets Error::RunInProgress before anything is touched.
    // Acquired only after the guards above pass, so a refused run leaves no file behind.
    let _run_lock = crate::runlock::RunLock::for_mirror_dest(&dest)?;

    // D3: purge any stale trash left by a run that died mid-flight, then stage this
    // run's deletions into a fresh per-run trash dir (created lazily by the first move).
    purge_stale_trash(&dest);
    let trash_dir = dest.join(format!(
        "{TRASH_PREFIX}{}",
        crate::repo::snapshot_id_from(OffsetDateTime::now_utc())
    ));
    // Every file moved into the trash this run, for the move-back on cancel.
    let mut moved_to_trash: Vec<(String, PathBuf)> = Vec::new();

    emit(Event::RunStarted { run_id: String::new(), jobs: resolved.sources.len() });

    let mut stats = JobStats {
        sources_skipped: resolved.skipped.len() as u64,
        ..Default::default()
    };
    for skipped in &resolved.skipped {
        emit(Event::SourceSkipped {
            source: skipped.configured.clone(),
            reason: skipped.reason.clone(),
        });
    }
    let mut cancelled = false;

    for (name, src) in &resolved.sources {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let src_clock = Instant::now();

        let dest_root = dest.join(name);
        emit(Event::JobStarted { job: name.clone(), source: src.clone(), dest: dest_root.clone() });

        let excludes = job.excludes.compile(src, &dest_root);
        let mtime_tolerance = engine::mtime_tolerance_between(src, &dest_root);
        // F30: on a mid-loop error, the accumulated stats (earlier sources' copies and
        // deletions) are emitted as a Failed RunDone before the error returns.
        let diff = match diff_source(src, &dest_root, &excludes, mtime_tolerance) {
            Ok(d) => d,
            Err(e) => {
                // D3: earlier sources' staged deletions are completed work -- their
                // replacements landed -- so the trash is purged even on the way out.
                let _ = std::fs::remove_dir_all(&trash_dir);
                emit_failed_run(&stats, started, emit, e.to_string());
                return Err(e);
            }
        };

        // The drift check sits between the read-only diff and the first mutation of this
        // source, so an abort provably means "nothing was touched" for this source --
        // and every deletion already performed for earlier sources was in the confirmed
        // set by the same check applied there.
        if let Some(expected) = expected_deletes {
            let expected_keys: HashSet<String> = expected
                .iter()
                .filter(|d| d.source == *name)
                .map(|d| normalize_for_compare(&d.rel))
                .collect();
            let surprise: Vec<&PathBuf> = diff
                .to_delete
                .iter()
                .filter(|rel| !expected_keys.contains(&normalize_for_compare(rel)))
                .collect();
            if !surprise.is_empty() {
                let e = Error::DestinationChangedSincePreview {
                    count: surprise.len() as u64,
                    samples: surprise
                        .iter()
                        .take(3)
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                // D3: the abort touched nothing on THIS source, but earlier sources'
                // staged deletions are completed work -- purge their trash.
                let _ = std::fs::remove_dir_all(&trash_dir);
                emit_failed_run(&stats, started, emit, e.to_string());
                return Err(e);
            }
        }

        let src_reparse = diff.reparse_skipped;
        stats.reparse_skipped += src_reparse;

        // F39: sweep crash-orphaned temp files of ours that the destination walk found.
        // They were never in the delete plan, so this is housekeeping -- not counted in
        // files_deleted, not subject to the preview's expected-delete set. Best-effort:
        // an orphan that refuses removal is inert anyway (the walk never plans it).
        let mut swept = 0u64;
        for path in &diff.dest_temp_files {
            if crate::copy::remove_file_clearing_readonly(path).is_ok() {
                swept += 1;
            }
        }
        if swept > 0 {
            tracing::info!("swept {swept} crash-orphaned temp file(s) under {}", dest_root.display());
        }

        let src_dest_unreadable = diff.dest_unreadable;
        if src_dest_unreadable > 0 {
            stats.files_failed += src_dest_unreadable;
            for path in &diff.dest_unreadable_paths {
                emit(Event::SourceUnreadable {
                    path: path.clone(),
                    message: "could not be read at the destination -- it was left in place \
                              rather than risk deleting the wrong thing"
                        .into(),
                });
            }
        }

        emit(Event::PlanReady {
            to_copy: diff.to_copy.len() as u64,
            to_link: 0,
            bytes_to_copy: diff.to_copy.iter().map(|e| e.size).sum(),
            // Always known for a mirror -- it is exactly what was just walked and diffed,
            // never the "could not determine" case that leaves this `None`.
            deletes: Some(diff.to_delete.len() as u64),
        });

        // D3: the delete phase is a MOVE phase. Each file slated for deletion is renamed
        // into the per-run trash dir at the destination root -- same volume, so one cheap
        // rename each -- preserving its relative path. The trash is purged only after the
        // copies have landed (or at the start of the next run if this one dies first), so
        // an interruption can never leave the destination missing a file it held before
        // the run: it is in place, freshly copied, or in the trash. A failed rename LEAVES
        // the file in place and counts as a failure -- nothing is ever force-deleted.
        let mut src_deleted = 0u64;
        for rel in &diff.to_delete {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let from = dest_root.join(rel);
            let to = trash_dir.join(name).join(rel);
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => {
                    stats.files_deleted += 1;
                    src_deleted += 1;
                    moved_to_trash.push((name.clone(), rel.clone()));
                }
                Err(e) => {
                    stats.files_failed += 1;
                    emit(Event::FileFailed(FileError {
                        path: rel.clone(),
                        message: format!("could not move to the trash ({e}) -- left in place"),
                    }));
                }
            }
        }

        // D8: empty directories that exist in the source are part of its shape -- create
        // them at the destination, and protect them from the prune below, which still
        // removes every empty dir the source does NOT have.
        let keep_empty: HashSet<String> =
            diff.src_empty_dirs.iter().map(|p| normalize_for_compare(p)).collect();
        for rel in &diff.src_empty_dirs {
            if let Err(e) = std::fs::create_dir_all(dest_root.join(rel)) {
                tracing::warn!("could not create empty directory {}: {e}", dest_root.join(rel).display());
            }
        }
        prune_empty_dirs(&dest_root, &excludes, &keep_empty);

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
        let counts = parallel::run_parallel(&items, &parallel::ParallelOptions::verifying(), cancel, emit);

        stats.files_copied += counts.copied;
        stats.files_failed += counts.failed;
        stats.bytes_copied += counts.bytes_copied;
        if counts.cancelled {
            cancelled = true;
        }

        emit(Event::JobDone {
            job: name.clone(),
            // L1: per-source stats, not run-cumulative -- summing them must land exactly
            // on the RunDone totals.
            stats: JobStats {
                files_copied: counts.copied,
                files_failed: counts.failed + src_dest_unreadable,
                files_deleted: src_deleted,
                bytes_copied: counts.bytes_copied,
                reparse_skipped: src_reparse,
                duration_ms: src_clock.elapsed().as_millis() as u64,
                ..Default::default()
            },
        });
        if cancelled {
            break;
        }
    }

    // D3: end of run. On success/partial the copies have landed, so the staged
    // deletions are purged for good. On cancel, the trash goes BACK: a cancelled mirror
    // must lose nothing the destination held before the run. (If a move-back itself
    // fails, the file stays in the trash -- which the next run will purge; the warning
    // says so loudly because that is the one place D3 can still lose a file.)
    if cancelled {
        let mut stuck = 0u64;
        for (name, rel) in &moved_to_trash {
            let from = trash_dir.join(name).join(rel);
            let to = dest.join(name).join(rel);
            // The prune above may have removed the original parent dir.
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::rename(&from, &to).is_err() {
                stuck += 1;
            }
        }
        if stuck > 0 {
            emit(Event::RunWarning {
                message: format!(
                    "the run was cancelled and {stuck} deleted file(s) could not be moved \
                     back out of {} -- move them back by hand, or rerun the job (which \
                     purges the trash)",
                    trash_dir.display()
                ),
            });
        } else {
            let _ = std::fs::remove_dir_all(&trash_dir);
        }
    } else if let Err(e) = std::fs::remove_dir_all(&trash_dir) {
        // No deletions at all means the trash dir was never created -- not an error.
        if trash_dir.exists() {
            tracing::warn!("could not purge trash {}: {e}", trash_dir.display());
        }
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else if stats.files_failed > 0 {
        RunOutcome::Partial { failed: stats.files_failed }
    } else if stats.sources_skipped > 0 {
        // The destination was faithfully mirrored for the sources that exist -- but the
        // job was asked for more than that, and a clean Ok would say otherwise.
        RunOutcome::Partial { failed: 0 }
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
        engine::run_job(&snapshot_job, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
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

    /// F4: a case-only rename must be neither deleted nor recopied. The unchanged-check
    /// resolves case-insensitively on NTFS (so no copy is queued), and the delete-set
    /// comparison must use the same case rules -- otherwise the differently-cased twin is
    /// deleted without its recopy ever having been planned: file gone, outcome Ok.
    #[test]
    fn a_case_only_rename_is_neither_copied_nor_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("README.txt"), "readme content");
        let dest = tmp.path().join("out");
        // The destination holds the same file under a different-case name, with the same
        // content and (via copy) the same timestamp.
        std::fs::create_dir_all(dest.join("proj")).unwrap();
        std::fs::copy(src.join("README.txt"), dest.join("proj/readme.txt")).unwrap();
        let job = mirror_job(vec![src], dest.clone());

        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_delete, 0, "no deletion may ever be planned for a case twin");
        assert_eq!(preview.to_add_or_update, 0);

        let result = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert_eq!(result.stats.files_deleted, 0, "the case twin must not be deleted");
        assert_eq!(result.stats.files_copied, 0, "and nothing needs recopying");
        assert_eq!(result.outcome, RunOutcome::Ok);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/readme.txt")).unwrap(),
            "readme content"
        );
    }

    /// MT9-mirror: when the source tree could not be fully read, the mirror must fail --
    /// preview AND run -- and delete nothing. A tree with an unreadable hole looks exactly
    /// like one whose files were deleted; building a deletion plan over it would
    /// permanently destroy real destination files.
    #[cfg(windows)]
    #[test]
    fn an_unreadable_source_means_no_deletions_ever() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        // The deterministic stand-in for a permission error: a trailing-dot directory can
        // be created via the \\?\ prefix but not stat'ed through a normal path.
        let weird = format!(r"\\?\{}\locked.", src.display());
        if std::fs::create_dir(&weird).is_err() {
            eprintln!("skipping: cannot create a trailing-dot directory here");
            return;
        }
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/old.txt"), "old");
        let job = mirror_job(vec![src], dest.clone());

        let err = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::SourceUnreadable { .. }),
            "expected SourceUnreadable, got: {err}"
        );
        assert!(
            dest.join("proj/old.txt").is_file(),
            "no deletion may happen when the source walk was incomplete"
        );

        assert!(
            preview_mirror(&job).is_err(),
            "preview must fail too -- never show a destructive plan over an unreadable tree"
        );
        assert!(dest.join("proj/old.txt").is_file(), "and a failed preview deletes nothing");

        let _ = std::fs::remove_dir(&weird);
    }

    /// A mirror over a partially-missing source list must not claim a clean Ok either --
    /// the remaining sources are mirrored, but the skip is reported and the outcome is
    /// Partial.
    #[test]
    fn a_missing_source_is_reported_and_makes_the_mirror_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        touch(&real.join("f.txt"), "x");
        let missing = tmp.path().join("vanished");
        let dest = tmp.path().join("out");

        let mut events = Vec::new();
        let result = run_mirror(
            &mirror_job(vec![real, missing], dest.clone()),
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap();

        assert_eq!(result.stats.files_copied, 1, "the real source is still mirrored");
        assert_eq!(std::fs::read_to_string(dest.join("proj/f.txt")).unwrap(), "x");
        assert_eq!(result.stats.sources_skipped, 1);
        assert!(
            matches!(result.outcome, RunOutcome::Partial { .. }),
            "a skipped source must not report Ok: {:?}",
            result.outcome
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::SourceSkipped { .. })),
            "a SourceSkipped warning event must fire"
        );
    }

    // ------------------------------------------------------- F10 / MT3: repo guard depth

    /// Build a real repository (one snapshot, one file) at `repo_root` via the engine.
    fn make_repo(repo_root: &Path, originals_root: &Path) -> String {
        let src = originals_root.join("repo-src");
        touch(&src.join("history.txt"), "backup history");
        let job = crate::config::Job {
            id: "s".into(),
            name: "Snapshots".into(),
            sources: vec![src],
            dest: repo_root.to_path_buf(),
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        };
        engine::run_job(&job, None, &AtomicBool::new(false), &mut |_| {}).unwrap().id
    }

    /// MT3, first direction: a mirror destination nested INSIDE a repository must be
    /// refused -- the old guard only looked at `dest/.backstar`, so `<repo>/snapshots`
    /// (or any subfolder) passed as "safe" and the mirror then purged history.
    #[test]
    fn mirroring_into_a_folder_inside_a_repository_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("backup");
        make_repo(&repo_root, tmp.path());

        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");

        for dest in [repo_root.join("snapshots"), repo_root.join("sub")] {
            let job = mirror_job(vec![src.clone()], dest.clone());
            assert!(
                run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).is_err(),
                "mirror into {} must be refused",
                dest.display()
            );
            assert!(preview_mirror(&job).is_err(), "and so must its preview");
        }

        // The repository must be exactly as it was.
        let repo = crate::repo::Repo::open(&repo_root).unwrap();
        assert_eq!(repo.snapshot_ids().len(), 1);
    }

    /// MT3, second direction: a mirror destination that CONTAINS a repository anywhere in
    /// its tree must be refused before anything is deleted -- and the repository must
    /// survive untouched. Refusing outright (rather than silently excluding the repo
    /// folder) is deliberate: this is a configuration mistake, and a quiet "success"
    /// around it would read exactly like a good run.
    #[test]
    fn a_mirror_onto_a_folder_containing_a_repository_leaves_it_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        let nested_repo = dest.join("proj/old-backup");
        make_repo(&nested_repo, tmp.path());

        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let job = mirror_job(vec![src], dest.clone());

        let err = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::RepoInsideDestination(_)),
            "expected RepoInsideDestination, got: {err}"
        );
        assert!(preview_mirror(&job).is_err(), "preview must refuse too");

        let repo = crate::repo::Repo::open(&nested_repo).unwrap();
        assert_eq!(repo.snapshot_ids().len(), 1, "history must not be purged");
        let snap = repo.snapshot_dir(&repo.snapshot_ids()[0]);
        assert!(snap.join("repo-src/history.txt").is_file(), "backup data must survive");
    }

    // ------------------------------------------------------------ F11: safe pruning

    /// A junction inside the destination pointing OUTSIDE the tree: the prune must never
    /// follow it. The old recursive `is_dir()`-based prune descended into the target and
    /// removed its empty subdirectories -- pruning a directory that was never the
    /// mirror's to touch.
    #[cfg(windows)]
    #[test]
    fn pruning_never_follows_a_junction_out_of_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("empty-sub")).unwrap();
        touch(&outside.join("keep.txt"), "outside the mirror");

        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(dest.join("proj")).unwrap();
        let link = dest.join("proj/link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }

        run_mirror(&mirror_job(vec![src], dest), &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(outside.join("empty-sub").is_dir(), "the target's subdir must survive");
        assert!(outside.join("keep.txt").is_file(), "the target's files must survive");
    }

    /// An excluded folder is protected from deletion -- and that protection must cover
    /// the empty-dir prune too, or the protection silently ends the moment the folder
    /// has nothing in it.
    #[test]
    fn an_excluded_empty_directory_survives_the_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(dest.join("proj/node_modules")).unwrap();

        let mut job = mirror_job(vec![src], dest.clone());
        job.excludes = ExcludeRules::project();

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(
            dest.join("proj/node_modules").is_dir(),
            "an excluded folder is never pruned, even when empty"
        );
    }

    // ------------------------------------------- F12: reparse point at the dest root

    /// A destination subfolder that is itself a junction must be refused, never walked
    /// and deleted through -- `is_dir()` follows it, and the delete pass would then empty
    /// a directory that is not the destination at all.
    #[cfg(windows)]
    #[test]
    fn a_junction_at_the_destination_subfolder_is_never_deleted_through() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        touch(&outside.join("precious.txt"), "do not touch");

        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let link = dest.join("proj");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }
        let job = mirror_job(vec![src], dest);

        let err = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::ReparseRoot(_)),
            "expected ReparseRoot, got: {err}"
        );
        assert!(preview_mirror(&job).is_err(), "preview must refuse too");
        assert!(
            outside.join("precious.txt").is_file(),
            "the junction target must be completely untouched"
        );
    }

    // -------------------------------------------- F13: preview/execute drift guard

    /// Files created at the destination BETWEEN the confirmed preview and the run must
    /// not be deleted unreviewed: the run aborts before touching anything, and a fresh
    /// preview is what unblocks it.
    #[test]
    fn a_run_aborts_when_the_destination_changed_since_the_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/doomed.txt"), "x");
        let job = mirror_job(vec![src], dest.clone());

        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_delete, 1);
        assert_eq!(preview.delete_set.len(), 1);
        assert_eq!(preview.delete_set[0].source, "proj");

        // The destination changes after the user confirms.
        touch(&dest.join("proj/sneaky.txt"), "new since preview");

        let err = run_mirror_with_expected_deletes(
            &job,
            Some(&preview.delete_set),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::DestinationChangedSincePreview { .. }),
            "expected DestinationChangedSincePreview, got: {err}"
        );
        assert!(dest.join("proj/sneaky.txt").is_file(), "nothing may be deleted");
        assert!(dest.join("proj/doomed.txt").is_file(), "nothing may be touched at all");
        assert!(!dest.join("proj/a.txt").exists(), "nor copied");

        // Re-preview (which now sees the new file) and the confirmed run proceeds.
        let fresh = preview_mirror(&job).unwrap();
        assert_eq!(fresh.to_delete, 2);
        let result = run_mirror_with_expected_deletes(
            &job,
            Some(&fresh.delete_set),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(result.stats.files_deleted, 2);
        assert!(!dest.join("proj/sneaky.txt").exists());
        assert!(dest.join("proj/a.txt").is_file());
    }

    /// The headless entry point has no preview to check against: `run_mirror` keeps its
    /// historical behaviour by passing no expected set.
    #[test]
    fn the_headless_run_deletes_without_an_expected_set() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/doomed.txt"), "x");

        let result = run_mirror(
            &mirror_job(vec![src], dest.clone()),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(result.stats.files_deleted, 1);
        assert!(!dest.join("proj/doomed.txt").exists());
    }

    // ------------------------------------------------------- F3: cross-process run lock

    /// F3, mirror side: while the destination's lock is held -- by the headless
    /// scheduler in another process, say -- a mirror run must fail with `RunInProgress`
    /// before copying or deleting anything.
    #[test]
    fn run_mirror_refuses_while_the_destination_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        // The engine canonicalises the destination before locking; the test must hold
        // the SAME spelling, or the two would lock different files.
        let dest = guards::canonical_path(tmp.path().join("out")).unwrap();
        touch(&dest.join("proj/existing.txt"), "x");

        let _held = crate::runlock::RunLock::for_mirror_dest(&dest).unwrap();
        let err = run_mirror(&mirror_job(vec![src.clone()], dest.clone()), &AtomicBool::new(false), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(err, Error::RunInProgress(_)),
            "expected RunInProgress, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/existing.txt")).unwrap(),
            "x",
            "a contended mirror must not have touched the destination"
        );
        assert!(!dest.join("proj/a.txt").exists());

        drop(_held);
        // Once released, the same job runs.
        let result = run_mirror(&mirror_job(vec![src], dest.clone()), &AtomicBool::new(false), &mut |_| {})
            .unwrap();
        assert_eq!(result.stats.files_copied, 1);
        assert!(dest.join("proj/a.txt").is_file());
        assert!(
            !dest.join(crate::runlock::MIRROR_RUN_LOCK_NAME).exists(),
            "a finished run cleans its lock file up"
        );
    }

    // ------------------------------------------------------- F39: crash-orphan sweep

    /// Orphan temp files at the destination (from a killed run) must be swept by the next
    /// run -- never enumerated into the delete plan (they are not user content), never
    /// counted as deletions, and a temp file in the SOURCE must be neither mirrored nor
    /// deleted.
    #[test]
    fn a_mirror_run_sweeps_orphan_temps_and_never_plans_them() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("real.txt"), "real");
        let src_orphan = src.join(".bstmp-5-5-sourcejunk");
        touch(&src_orphan, "junk");
        let dest = tmp.path().join("out");
        let orphan_new = dest.join("proj/.bstmp-999-0-fragment");
        let orphan_old = dest.join("proj/sub/.backstar-tmp-42-deadfile");
        touch(&orphan_new, "junk");
        touch(&orphan_old, "junk");
        let job = mirror_job(vec![src], dest.clone());

        // The preview sees the orphans (for a later sweep) but must not touch them.
        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_delete, 0, "orphans are never deletion candidates");
        assert!(orphan_new.is_file(), "preview deletes nothing");

        let result = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(!orphan_new.exists(), "swept (new prefix)");
        assert!(!orphan_old.exists(), "swept (legacy prefix, nested)");
        assert_eq!(
            result.stats.files_deleted, 0,
            "swept temp files are housekeeping, not user deletions"
        );
        assert_eq!(result.stats.files_copied, 1, "only the real file is mirrored");
        assert!(dest.join("proj/real.txt").is_file());
        assert!(
            !dest.join("proj/.bstmp-5-5-sourcejunk").exists(),
            "a temp file in the source must not be mirrored"
        );
        assert!(src_orphan.is_file(), "and never deleted from the source");
        assert_eq!(result.outcome, RunOutcome::Ok);
    }

    // ------------------------------------------------------- F30: truthful failure record

    /// A mid-loop error must not discard the run's accumulated work: the first source
    /// mirrored successfully before the second proved unreadable, and the Failed RunDone
    /// must carry the first source's stats -- otherwise history records nothing at all.
    #[cfg(windows)]
    #[test]
    fn a_mid_loop_error_emits_a_failed_run_done_with_the_accumulated_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        touch(&good.join("a.txt"), "aaa");
        touch(&good.join("b.txt"), "bbb");
        let bad = tmp.path().join("bad");
        touch(&bad.join("keep.txt"), "keep");
        // The deterministic stand-in for a permission error: a trailing-dot directory can
        // be created via the \\?\ prefix but not stat'ed through a normal path.
        let weird = format!(r"\\?\{}\locked.", bad.display());
        if std::fs::create_dir(&weird).is_err() {
            eprintln!("skipping: cannot create a trailing-dot directory here");
            return;
        }
        let dest = tmp.path().join("out");
        // Also give the destination something to delete in the GOOD source, so the
        // accumulated record includes a deletion, not just copies.
        touch(&dest.join("good/doomed.txt"), "x");
        let job = mirror_job(vec![good, bad], dest.clone());

        let mut events = Vec::new();
        let err = run_mirror(&job, &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap_err();
        assert!(matches!(err, Error::SourceUnreadable { .. }), "got: {err}");

        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { outcome, stats, .. } => Some((outcome.clone(), stats.clone())),
                _ => None,
            })
            .expect("a RunDone must be emitted before the error returns");
        assert!(matches!(done.0, RunOutcome::Failed { .. }), "{:?}", done.0);
        assert_eq!(done.1.files_copied, 2, "the good source's copies are recorded");
        assert_eq!(done.1.files_deleted, 1, "and its deletion");
        assert!(!dest.join("good/doomed.txt").exists(), "the work really happened");

        let _ = std::fs::remove_dir(&weird);
    }

    /// The preview-pinned path gets the same treatment: an abort after earlier sources
    /// mutated is a Failed run with those stats, not a silent error.
    #[test]
    fn a_drift_abort_after_earlier_work_records_the_accumulated_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        touch(&one.join("a.txt"), "a");
        let two = tmp.path().join("two");
        touch(&two.join("b.txt"), "b");
        let dest = tmp.path().join("out");
        touch(&dest.join("one/doomed.txt"), "x");
        touch(&dest.join("two/doomed.txt"), "y");
        let job = mirror_job(vec![one, two], dest.clone());

        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_delete, 2);
        // The destination drifts after confirmation, in the SECOND source's tree.
        touch(&dest.join("two/sneaky.txt"), "new");

        let mut events = Vec::new();
        let err = run_mirror_with_expected_deletes(
            &job,
            Some(&preview.delete_set),
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap_err();
        assert!(matches!(err, Error::DestinationChangedSincePreview { .. }));

        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { outcome, stats, .. } => Some((outcome.clone(), stats.clone())),
                _ => None,
            })
            .expect("RunDone must be emitted");
        assert!(matches!(done.0, RunOutcome::Failed { .. }));
        assert_eq!(done.1.files_deleted, 1, "source one's deletion happened and is recorded");
        assert!(dest.join("two/sneaky.txt").is_file(), "the drifted file survives");
        assert!(dest.join("two/doomed.txt").is_file(), "source two was never touched");
    }

    // ------------------------------------------------------- F37/D8: empty directories

    /// A mirror keeps the source's SHAPE: an empty dir present in the source is created
    /// and kept at the destination; an empty dir the source does not have is pruned.
    #[test]
    fn a_mirror_keeps_source_empty_dirs_and_prunes_dest_only_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        std::fs::create_dir_all(src.join("empty-from-source")).unwrap();
        std::fs::create_dir_all(src.join("nested/empty-child")).unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(dest.join("proj/dest-only-empty")).unwrap();
        let job = mirror_job(vec![src], dest.clone());

        let result = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert_eq!(result.outcome, RunOutcome::Ok);
        assert!(dest.join("proj/empty-from-source").is_dir(), "source's empty dir mirrored");
        assert!(
            dest.join("proj/nested/empty-child").is_dir(),
            "an empty dir nested under a populated one is mirrored"
        );
        assert!(
            !dest.join("proj/dest-only-empty").exists(),
            "an empty dir absent from the source is pruned"
        );
        assert!(dest.join("proj/keep.txt").is_file());

        // And a second run over the same tree neither deletes nor recreates them
        // wrongly: nothing to copy, nothing to delete.
        let second = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(second.stats.files_copied, 0);
        assert_eq!(second.stats.files_deleted, 0);
        assert!(dest.join("proj/empty-from-source").is_dir());
    }

    /// The interplay that used to eat the dir: the prune runs AFTER the mirror creates
    /// the source's empty dirs -- the keep set must protect the just-created dirs too.
    #[test]
    fn a_freshly_created_empty_dir_survives_the_same_runs_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        std::fs::create_dir_all(src.join("brand-new-empty")).unwrap();
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src], dest.clone());

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(dest.join("proj/brand-new-empty").is_dir());
    }

    // ------------------------------------------------------- L1: per-source JobDone

    /// Mirror JobDone stats are per-source too (L1): they must sum to the run totals,
    /// and a later source's report must not carry an earlier source's numbers.
    #[test]
    fn mirror_job_done_stats_are_per_source() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        touch(&one.join("a.txt"), "aa");
        let two = tmp.path().join("two");
        touch(&two.join("b.txt"), "b");
        touch(&two.join("c.txt"), "c");
        let dest = tmp.path().join("out");
        touch(&dest.join("two/doomed.txt"), "x");

        let mut events = Vec::new();
        run_mirror(&mirror_job(vec![one, two], dest), &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap();

        let dones: Vec<JobStats> = events
            .iter()
            .filter_map(|e| match e {
                Event::JobDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(dones.len(), 2);
        assert_eq!(dones[0].files_copied, 1);
        assert_eq!(dones[0].files_deleted, 0);
        assert_eq!(dones[1].files_copied, 2, "not run-cumulative");
        assert_eq!(dones[1].files_deleted, 1, "the deletion belongs to source two");

        let run = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(dones[0].files_copied + dones[1].files_copied, run.files_copied);
        assert_eq!(dones[0].files_deleted + dones[1].files_deleted, run.files_deleted);
    }

    // ------------------------------------------------------- D1: verification wiring

    /// Mirrors verify exactly like backups: FileDone carries the copied file's blake3
    /// hash, and the run's verify pass reports examining exactly the copied files. A
    /// second, unchanged run verifies nothing -- and says so with examined: 0.
    #[test]
    fn a_mirror_hashes_and_verifies_what_it_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bbb");
        let dest = tmp.path().join("out");
        let job = mirror_job(vec![src.clone()], dest);

        let mut events = Vec::new();
        let result = run_mirror(&job, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
        assert_eq!(result.stats.files_copied, 2);
        assert_eq!(result.outcome, RunOutcome::Ok);

        let verify: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(verify.len(), 1);
        assert_eq!(verify[0].0, 2, "both copied files were re-read and compared");
        assert!(verify[0].1.is_empty());
        let expect = crate::copy::blake3_hex(&src.join("a.txt")).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::FileDone { path, hash: Some(h), .. } if path == Path::new("a.txt") && *h == expect
        )));

        // Second run: nothing changed, nothing copied, nothing to verify.
        let mut events2 = Vec::new();
        let result2 = run_mirror(&job, &AtomicBool::new(false), &mut |e| events2.push(e)).unwrap();
        assert_eq!(result2.stats.files_copied, 0);
        let verify2: Vec<_> = events2
            .iter()
            .filter_map(|e| match e {
                Event::VerifyDone { examined, .. } => Some(*examined),
                _ => None,
            })
            .collect();
        assert_eq!(verify2, vec![0], "a zero-copy run reports examined: 0 honestly");
    }

    // ------------------------------------------------------- D3/F65: staged trash

    /// The staged flow, end state: the deleted file is gone from the destination, the
    /// copies landed, and NO trash remains -- the per-run staging dir is purged at the
    /// end of a successful run.
    #[test]
    fn deletions_are_staged_through_a_trash_that_is_purged_at_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/doomed.txt"), "soon gone");
        let job = mirror_job(vec![src], dest.clone());

        let result = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert_eq!(result.stats.files_deleted, 1, "a staged move counts as a deletion");
        assert!(!dest.join("proj/doomed.txt").exists());
        assert!(dest.join("proj/keep.txt").is_file());
        let leftovers: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(TRASH_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "the trash must be purged at end of run: {leftovers:?}");
    }

    /// The interruption guarantee, proven directly: cancel mid-copy (after the deletions
    /// were staged) and the moved files come BACK -- a cancelled mirror loses nothing the
    /// destination held before the run.
    #[test]
    fn a_cancelled_run_moves_the_trash_contents_back() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        for i in 0..200 {
            touch(&src.join(format!("f{i}.txt")), "content");
        }
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/precious.txt"), "was here before the run");
        let job = mirror_job(vec![src], dest.clone());

        let cancel = AtomicBool::new(false);
        let result = run_mirror(&job, &cancel, &mut |e| {
            if matches!(e, Event::OverallProgress { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

        assert_eq!(result.outcome, RunOutcome::Cancelled);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/precious.txt")).unwrap(),
            "was here before the run",
            "the staged deletion was moved BACK on cancel"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(TRASH_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "move-back succeeded, so the trash is gone: {leftovers:?}");
    }

    /// A crash leaves the trash behind (no move-back, no purge). It must be invisible to
    /// the preview (never counted, never a deletion candidate) and purged at the start of
    /// the next run -- its contents were by construction files a run meant to delete.
    #[test]
    fn a_stale_trash_is_invisible_to_preview_and_purged_by_the_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        // The remains of a crashed run.
        let stale = dest.join(".backstar-trash-2020-01-01T00-00-00Z");
        touch(&stale.join("proj/deleted-long-ago.txt"), "junk");
        let job = mirror_job(vec![src], dest.clone());

        let preview = preview_mirror(&job).unwrap();
        assert_eq!(preview.to_delete, 0, "trash contents are never deletion candidates");
        assert_eq!(preview.to_add_or_update, 1, "only the real source file is planned");
        assert!(stale.join("proj/deleted-long-ago.txt").is_file(), "preview touches nothing");

        run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(!stale.exists(), "the next run purges the stale trash at start");
        assert!(dest.join("proj/keep.txt").is_file());
    }

    /// A file that cannot be moved into the trash (held open here) stays exactly where it
    /// was and counts as a failure -- never force-deleted, and the rest of the run
    /// completes.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_moved_to_the_trash_is_left_in_place_and_counted() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        let dest = tmp.path().join("out");
        touch(&dest.join("proj/pinned.txt"), "cannot be renamed while open");
        let job = mirror_job(vec![src], dest.clone());

        // Hold the doomed file open with no sharing: the rename into the trash fails.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dest.join("proj/pinned.txt"))
            .unwrap();

        let mut events = Vec::new();
        let result = run_mirror(&job, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();

        assert_eq!(result.stats.files_deleted, 0, "no force-delete");
        assert_eq!(result.stats.files_failed, 1, "the failed move is counted");
        assert_eq!(result.outcome, RunOutcome::Partial { failed: 1 });
        // The held file cannot even be stat'ed, so prove its presence via the parent
        // listing while the handle is open...
        let names: Vec<String> = std::fs::read_dir(dest.join("proj"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"pinned.txt".to_string()), "left exactly in place");
        assert!(names.contains(&"keep.txt".to_string()), "the rest of the run completed");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::FileFailed(f) if f.path == Path::new("pinned.txt") && f.message.contains("trash")
        )));

        // And once the handle is released, the NEXT run retries and finishes the job.
        drop(_held);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/pinned.txt")).unwrap(),
            "cannot be renamed while open"
        );
        let second = run_mirror(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(second.stats.files_deleted, 1);
        assert!(!dest.join("proj/pinned.txt").exists());
    }
}
