//! Run orchestration: walk, plan, then copy or link.
//!
//! The shape of a run is deliberately two-phase. The whole source tree is walked and diffed
//! against the previous snapshot *before* any bytes move, which is what makes the progress
//! honest -- a real denominator, a real ETA, and a count of what will be written that the
//! user can be shown before agreeing to anything.
//!
//! An unchanged file is not copied. It is hardlinked to the copy already sitting in the
//! previous snapshot, so it appears in the new snapshot at no cost in disk space. This is
//! what makes keeping thirty versions affordable, and it is only safe because
//! [`crate::copy::copy_file`] never writes through an existing link -- and because the
//! "previous snapshot" is only trusted as a link source when its manifest proves it was
//! taken by the SAME job from the SAME canonical source (see [`parent_link_root`]):
//! leaf-name equality is not identity, and linking bytes that never existed in this
//! source would present them as backed-up content.
//!
//! "Unchanged" is decided by size and modification time, where the mtime comparison is
//! EXACT unless a FAT-family volume is involved on either side: FAT stores mtimes at
//! two-second resolution, so the tolerance exists for it -- applied to NTFS->NTFS it
//! would read a same-size edit made a second after the last run as "unchanged" and
//! hardlink the OLD bytes into the new snapshot.
//!
//! A run only reports `Ok` when everything it was asked to protect was actually protected.
//! A configured source that no longer exists is skipped with a warning and forces at least
//! `Partial`; an entry inside a source that cannot be read is counted into
//! `files_failed` and reported by path; and a source ROOT that cannot be listed at all
//! fails the whole run, because an empty "Completed" snapshot manufactured from an
//! unreadable tree would become the link parent every later run trusts.
//!
//! Runs are mutually exclusive per repository ACROSS processes (F3): [`Repo::acquire_run_lock`]
//! holds `.backstar/run.lock` open for the whole run, so the installed app and the headless
//! scheduler -- which share no in-process lock -- cannot interleave writes into one repo.
//! The lock also makes the atomic snapshot-id reservation in [`Repo::allocate_snapshot_id`]
//! the second line of defence rather than the only one.
//!
//! While holding that lock, run start prunes snapshot dirs that have no manifest (F35):
//! the manifest is written last, so a manifest-less dir is a run that died mid-copy --
//! incomplete, yet indistinguishable from a finished snapshot in a plain folder listing.
//! The lock is what makes wholesale pruning safe: no other run can be mid-write in this
//! repository.
//!
//! Two honesty rules govern the end of a run (F36): the recovery README failing to write
//! is a logged warning, never a failed run; and a failed MANIFEST write fails the run with
//! an error that says the snapshot files themselves are complete and restorable on disk.
//! And one governs the destination's capability (F29): a volume that cannot hardlink gets
//! one run-level warning, because otherwise "versioned" backups silently become full
//! recopies of every unchanged file, every run.
//!
//! Empty directories are preserved (decision D8): the walk records them ([`crate::walk`]),
//! and the snapshot tree creates them explicitly -- the copy phase only ever makes
//! directories that have a file in them.
//!
//! Content is verified, not assumed (D1): every file the copy phase writes has its source
//! hashed with blake3 right after the copy (the kernel does the copy's own read, so this
//! is one extra read per changed file), a post-copy pass re-reads each destination and
//! compares, and the manifest records the hashes of exactly the files copied -- linked
//! files' content was verified by the run that first copied them. A verify mismatch is a
//! failure: the bad copy is removed so no later run can link the corrupt bytes forward.
//! And a Link decision that rests on the FAT two-second mtime window (rather than an exact
//! timestamp match) hash-compares the two files first -- "indistinguishable within the
//! tolerance" is not "unchanged".
//!
//! Two advertised knobs do what they say (D2): `git_gc` runs `git gc --auto` on git
//! sources before their walk (best-effort -- housekeeping never fails a run), and
//! `keep_snapshots` prunes this job's oldest snapshots past the keep window after a sound
//! run (per-job, never on Failed/Cancelled, and hardlink reference-counting makes deleting
//! a snapshot drop names rather than shared data). The robocopy engine selector is gone
//! entirely: an unwired fallback mode is a liability, not an option.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use time::OffsetDateTime;

use crate::config::Job;
use crate::events::{Event, JobStats, RunOutcome};
use crate::guards;
use crate::parallel;
use crate::repo::{rfc3339, Repo, SnapshotManifest, SnapshotSource};
use crate::volume::ProbeOutcome;
use crate::walk::{self, Entry};
use crate::{Error, Result};

/// Timestamp tolerance when deciding whether a file changed -- applied ONLY when a
/// FAT-family volume is involved (see [`mtime_tolerance_between`]).
///
/// FAT-family filesystems store modification times at two-second resolution, so the same
/// file copied to a USB stick comes back with a timestamp up to two seconds off. robocopy
/// exposes this as `/FFT`, and the PowerShell app passed it on every run precisely so that
/// exFAT targets did not recopy the entire tree each time. Applied unconditionally,
/// though, it also papers over real sub-two-second edits on NTFS.
pub(crate) const MTIME_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Decide the mtime tolerance for comparing files between two locations, ONCE per source
/// rather than per file: FAT's two seconds when either side sits on a FAT-family volume,
/// zero -- an exact match -- otherwise.
///
/// A UNC/network path counts as coarse (F20): a remote server's timestamp granularity is
/// not ours to know. The remaining unknown case (a local volume that will not probe)
/// resolves to exact: the worst outcome there is recopying files that did not change,
/// while the worst outcome of an undeserved tolerance on a volume where hardlinks SUCCEED
/// is linking stale bytes over a real edit and calling the run green. Wasteful beats wrong.
pub(crate) fn mtime_tolerance_between(a: &Path, b: &Path) -> std::time::Duration {
    let coarse = |p: &Path| crate::volume::probe_coarse_mtimes(p).unwrap_or(false);
    if coarse(a) || coarse(b) {
        MTIME_TOLERANCE
    } else {
        std::time::Duration::ZERO
    }
}

/// What a run decided to do with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// New or changed: write the bytes.
    Copy,
    /// Unchanged: add a name pointing at the previous snapshot's copy.
    Link,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub entry: Entry,
    pub action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub to_copy: u64,
    pub to_link: u64,
    pub bytes_to_copy: u64,
}

/// Decide, for each walked file, whether it must be copied or can be linked.
///
/// `previous` is the matching source folder inside the parent snapshot, if there is one.
/// `mtime_tolerance` comes from the run's per-source probe decision (F8/F20).
///
/// D1's link-time half: when the size matches and the mtime differs but lands INSIDE the
/// FAT tolerance window, the timestamps are merely "indistinguishable", not equal -- so
/// the source and the parent-snapshot copy are hash-compared before `Link` is chosen, and
/// a hash mismatch forces `Copy`. An EXACT mtime match (the only thing zero-tolerance
/// NTFS->NTFS comparisons accept) skips the hash: 100 ns precision plus an equal size is
/// sound, and hashing every unchanged file would double the read IO of the common case.
pub fn plan(
    entries: Vec<Entry>,
    src_root: &Path,
    previous: Option<&Path>,
    mtime_tolerance: std::time::Duration,
) -> Plan {
    let mut p = Plan::default();
    for entry in entries {
        let action = match previous {
            None => Action::Copy,
            Some(prev_root) => {
                let candidate = prev_root.join(&entry.rel);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(md) if md.is_file() && unchanged(&entry, md.len(), md.modified().ok(), mtime_tolerance) => {
                        if needs_hash_check(&entry, md.modified().ok(), mtime_tolerance)
                            && !same_content(&src_root.join(&entry.rel), &candidate)
                        {
                            Action::Copy
                        } else {
                            Action::Link
                        }
                    }
                    _ => Action::Copy,
                }
            }
        };
        match action {
            Action::Copy => {
                p.to_copy += 1;
                p.bytes_to_copy += entry.size;
            }
            Action::Link => p.to_link += 1,
        }
        p.files.push(PlannedFile { entry, action });
    }
    p
}

/// True when the mtime match was inside the tolerance window but NOT exact -- the only
/// shape where "unchanged" rests on FAT's two-second rounding rather than on a real
/// timestamp equality (D1).
fn needs_hash_check(
    entry: &Entry,
    prev_mtime: Option<SystemTime>,
    mtime_tolerance: std::time::Duration,
) -> bool {
    if mtime_tolerance.is_zero() {
        return false;
    }
    let Some(prev) = prev_mtime else { return false };
    match entry.mtime.duration_since(prev).or_else(|_| prev.duration_since(entry.mtime)) {
        Ok(d) => d > std::time::Duration::ZERO && d <= mtime_tolerance,
        Err(_) => false,
    }
}

/// D1 hash-compare for the tolerance window: true only when both files hash equal. Any
/// io failure answers false -- copying when in doubt is always the safe direction (a
/// wasted copy, never a stale link).
fn same_content(a: &Path, b: &Path) -> bool {
    match (crate::copy::blake3_hex(a), crate::copy::blake3_hex(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub(crate) fn unchanged(
    entry: &Entry,
    prev_size: u64,
    prev_mtime: Option<SystemTime>,
    mtime_tolerance: std::time::Duration,
) -> bool {
    if entry.size != prev_size {
        return false;
    }
    let Some(prev) = prev_mtime else {
        // Cannot read the previous timestamp, so cannot claim the file is unchanged.
        // Copying a file that did not need copying is wasteful; linking one that did
        // would silently lose the edit.
        return false;
    };
    match entry.mtime.duration_since(prev).or_else(|_| prev.duration_since(entry.mtime)) {
        Ok(d) => d <= mtime_tolerance,
        Err(_) => false,
    }
}

/// The subfolder name a source occupies inside a snapshot.
fn source_name(src: &Path) -> String {
    src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into())
}

/// The previous snapshot's copy of one source, but ONLY when the parent snapshot provably
/// captured the SAME source for the SAME job: the parent manifest's job name matches, and
/// its recorded original path for this subfolder equals today's canonical source path.
///
/// Leaf-name equality is not identity. Two jobs can share one destination and both have a
/// source whose leaf is `proj`; a job can be edited to point at a different folder with
/// the same leaf. In both cases `<parent>/<leaf>` holds bytes that never existed in THIS
/// source, and hardlinking them into the new snapshot would present them as backed-up
/// content. When in doubt -- no manifest, unreadable manifest, different job, different
/// original -- there is no parent and everything is copied: always correct, merely slower.
fn parent_link_root(
    repo: &Repo,
    parent: Option<&str>,
    parent_manifest: Option<&SnapshotManifest>,
    job_name: &str,
    name: &str,
    canonical_src: &Path,
) -> Option<PathBuf> {
    let manifest = parent_manifest.filter(|m| m.job == job_name)?;
    let recorded = manifest.sources.iter().find(|s| s.name == name)?;
    if recorded.original != canonical_src {
        return None;
    }
    parent.map(|id| repo.snapshot_dir(id).join(name))
}

/// A configured source that could not be used, with enough context for the user to find
/// and fix it. Carried out of [`resolve_sources`] so the run can warn, record, and keep
/// the outcome honest instead of silently backing up half a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSource {
    /// The source as configured: the folder path as written in the job, or the preset key.
    pub configured: String,
    /// Why it was skipped, phrased for display.
    pub reason: String,
}

/// What [`resolve_sources`] found: the sources to back up, and the configured ones that
/// had to be skipped. Both halves matter -- a run that reports success while having
/// silently dropped a configured source is the exact failure shape the PowerShell app
/// shipped with (a missing source was a `continue` and a green checkmark).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSources {
    /// (destination subfolder name, canonical source path) pairs, in job order.
    pub sources: Vec<(String, PathBuf)>,
    /// Configured sources (folders or presets) that no longer exist.
    pub skipped: Vec<SkippedSource>,
}

/// Validate a job before touching anything.
///
/// Returns the resolved sources in job order: `job.sources` first, then `job.presets`
/// expanded against the machine's current known-folder locations -- plus the configured
/// sources that had to be skipped, which callers must surface (warning event, run stats,
/// manifest) and must treat as disqualifying for an `Ok` outcome.
///
/// Three of these checks exist because the PowerShell app shipped without them and the
/// consequences were silent. Two sources with the same leaf name both target the same
/// destination subfolder, quietly merging two unrelated projects into one folder. A
/// destination inside its own source recurses. And a custom source folder that happens to
/// share a name with an enabled preset would silently merge into it too.
pub fn resolve_sources(job: &Job) -> Result<ResolvedSources> {
    resolve_sources_against(job, &crate::presets::all())
}

/// The actual implementation, taking the preset list as a parameter so it is testable
/// without touching real Windows known-folder APIs -- `presets::all()` reads this
/// machine's actual Desktop/Documents/Chrome-profile locations, which a unit test has no
/// business depending on.
fn resolve_sources_against(
    job: &Job,
    available_presets: &[crate::presets::Preset],
) -> Result<ResolvedSources> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = ResolvedSources::default();
    // Canonicalise the destination once here, and again at the top of each run function
    // (canonicalisation is idempotent): every guard check, repo open and copy path in a
    // run must work on the same spelling, or a `%VAR%`/`..`/8.3 spelling passes the guard
    // while the write lands somewhere else.
    let dest = guards::canonical_path(job.resolve_dest())?;

    for src in &job.sources {
        // Canonicalise BEFORE the existence check: a source spelled with an environment
        // variable, `..` or an 8.3 alias is a real folder and must be found, not silently
        // skipped as missing.
        let canon = guards::canonical_path(src)?;
        if !canon.is_dir() {
            out.skipped.push(SkippedSource {
                configured: src.to_string_lossy().to_string(),
                reason: "the folder does not exist (any more), or is not a folder".into(),
            });
            continue;
        }
        let name = source_name(&canon);

        if !seen.insert(name.to_lowercase()) {
            return Err(Error::other(format!(
                "two source folders are both named {name:?}, so they would back up into the \
                 same folder. Rename or remove one."
            )));
        }

        guards::ensure_dest_safe(&dest, &canon)?;
        out.sources.push((name, canon));
    }

    // System presets are named by their stable key, never by the resolved path's own leaf
    // component: the Chrome preset resolves to ".../Google/Chrome/User Data", and the
    // destination subfolder needs to stay "Chrome" on every machine and every Chrome
    // version, not follow whatever that leaf folder happens to be called.
    for key in &job.presets {
        let Some(preset) = available_presets.iter().find(|p| &p.key == key) else {
            // A stale key from an older config (a preset that no longer exists) or a
            // config edited by hand. Nothing to resolve it to; skip rather than fail the
            // whole run over one dangling reference -- but say so, because the config
            // claims something is being backed up that is not.
            out.skipped.push(SkippedSource {
                configured: key.clone(),
                reason: "no such preset exists on this machine (stale config entry)".into(),
            });
            continue;
        };
        let path = match (&preset.path, preset.found) {
            (Some(p), true) => p,
            _ => {
                out.skipped.push(SkippedSource {
                    configured: key.clone(),
                    reason: "the folder for this preset no longer exists on this machine"
                        .into(),
                });
                continue;
            }
        };

        let canon = guards::canonical_path(path)?;
        let name = preset.key.clone();

        if !seen.insert(name.to_lowercase()) {
            return Err(Error::other(format!(
                "'{name}' is used by both a preset and a custom source folder, so they \
                 would back up into the same folder. Rename the custom folder, or disable \
                 the preset."
            )));
        }

        guards::ensure_dest_safe(&dest, &canon)?;
        out.sources.push((name, canon));
    }

    if out.sources.is_empty() {
        return Err(Error::other(
            "no valid source folders to back up -- check that the configured folders and \
             presets still exist on this machine",
        ));
    }
    Ok(out)
}

/// Run `git gc --auto` on a source directory that is a git repository (D2). Best-effort
/// by contract: spawn failure (git not on PATH), a non-zero exit, or a 60-second timeout
/// all come back as `Err`, and the caller warns and continues -- repository housekeeping
/// must never fail a backup. Output is discarded (dev-null) so a chatty gc cannot block
/// on a full pipe.
fn git_gc_auto(src: &Path) -> Result<()> {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["gc", "--auto"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::other(format!("could not start git: {e}")))?;
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(Error::other(format!("git gc --auto exited with {status}")));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::other("git gc --auto timed out after 60s"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(Error::other(format!("waiting for git gc failed: {e}"))),
        }
    }
}

/// The git-gc collaborator (D2): runs `git gc --auto` on one git source. Boxed (not a
/// bare fn) so tests can capture a spy.
pub(crate) type GcFn = Box<dyn Fn(&Path) -> Result<()> + Send + Sync>;

/// The collaborators a run consults at start and finish, injectable so tests can exercise
/// "the destination cannot hardlink" (F29), "the manifest/README write failed" (F36),
/// deterministic byte progress (F34), corruption between copy and verify (D1), and
/// `git gc` calls (D2) -- without a real exFAT drive, permissions tricks, timing luck, or
/// a git binary. Production runs use [`RunHooks::production`]; this is crate-visible, not
/// public API.
pub(crate) struct RunHooks {
    /// Probe the destination volume (F20's honest three-way answer). Drives both the
    /// hardlink warning (F29) and the mtime tolerance decision (F8/D1).
    pub probe: fn(&Path) -> ProbeOutcome,
    pub write_manifest: fn(&Repo, &SnapshotManifest) -> Result<()>,
    pub write_readme: fn(&Repo) -> Result<()>,
    /// Producer-side throttle for byte-level progress events; tests set `ZERO` so every
    /// copy-callback chunk emits deterministically.
    pub progress_interval: std::time::Duration,
    /// D1 corruption-test seam: tampers with a copied file's destination between the copy
    /// and the verify pass. Production is always `None`.
    pub tamper_dest_after_copy: Option<fn(&Path)>,
    /// D2: `git gc --auto` on one git source. Production calls [`git_gc_auto`].
    pub git_gc: GcFn,
}

impl RunHooks {
    fn production() -> Self {
        Self {
            probe: crate::volume::probe_destination,
            write_manifest: |repo, m| repo.write_manifest(m),
            write_readme: |repo| repo.write_readme(),
            progress_interval: crate::parallel::DEFAULT_PROGRESS_INTERVAL,
            tamper_dest_after_copy: None,
            git_gc: Box::new(git_gc_auto),
        }
    }
}

/// The run-level warning a destination deserves when unchanged files cannot be
/// hardlinked into new snapshots (F29): `None` when linking is possible or the probe
/// could not answer (an unavailable destination fails on its own io errors; a warning
/// here would be noise over a run that is about to error out).
fn link_degraded_message(cap: &ProbeOutcome) -> Option<String> {
    match cap {
        ProbeOutcome::Local(c) => c.degraded_reason(),
        ProbeOutcome::Network { path } => Some(format!(
            "{} is a network destination. If the server does not support hard links, \
             unchanged files will be fully recopied on every run instead of shared \
             between snapshots -- backups still work, they just cost their full size \
             each time.",
            path.display()
        )),
        ProbeOutcome::Unavailable { .. } => None,
    }
}

/// Run one job to completion, emitting progress as it goes.
///
/// `keep_snapshots` is the retention policy from `Config.keep_snapshots` (D2): after a
/// run that produced a sound snapshot (Ok or Partial -- never Failed or Cancelled), this
/// job's oldest snapshots beyond the keep window are pruned. `None` keeps everything.
pub fn run_job(
    job: &Job,
    keep_snapshots: Option<u32>,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<SnapshotManifest> {
    run_job_with(job, keep_snapshots, &RunHooks::production(), cancel, emit)
}

pub(crate) fn run_job_with(
    job: &Job,
    keep_snapshots: Option<u32>,
    hooks: &RunHooks,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<SnapshotManifest> {
    let started_at = OffsetDateTime::now_utc();
    let clock = Instant::now();

    let resolved = resolve_sources(job)?;
    // Canonicalise the destination ONCE, up front, and use this spelling for everything --
    // repo open, snapshot dir, exclusion compilation, copies. A `%VAR%` dest must be
    // expanded rather than created as a literal folder, and the guard checks in
    // resolve_sources must be checking the same path the writes land on.
    let dest = guards::canonical_path(job.resolve_dest())?;
    // Preferring the volume-GUID location over the literal `dest` here is what makes a
    // portable destination actually portable: this is the one call that decides where the
    // repository -- and therefore every snapshot -- physically lives.
    let repo = Repo::open_or_init(&dest, &job.name)?;

    // One run per repository at a time, across processes: the GUI's in-process Runner
    // mutex cannot see the headless `--run-job` process (and vice versa). The lock file
    // under `.backstar/` is held for the rest of the run; a contender gets
    // Error::RunInProgress before anything is written.
    let _run_lock = repo.acquire_run_lock()?;

    // F35: a snapshot dir with no manifest is the remains of a run that died before
    // finishing (the manifest is written last) -- incomplete, but browsable and counted
    // as if it were complete, and its orphaned temp files are junk (F39). The run lock
    // is held, so no other run can be mid-write in this repository: prune such dirs
    // wholesale. Best-effort per dir -- a dir that refuses deletion stays and is
    // reported as `incomplete` by the listing API (F19).
    for id in repo.snapshot_ids() {
        if !repo.has_manifest(&id) {
            match std::fs::remove_dir_all(repo.snapshot_dir(&id)) {
                Ok(()) => tracing::info!(
                    "pruned incomplete snapshot {id} (no manifest -- an interrupted run)"
                ),
                Err(e) => {
                    tracing::warn!("could not prune incomplete snapshot {id}: {e}")
                }
            }
        }
    }

    // Probed once per run: the mtime tolerance and the link-capability warning (F29)
    // both key off this one answer (F20's three-way outcome).
    let dest_capability = (hooks.probe)(&dest);
    // D1/F34 behaviour of the copy phase: verify on (hash-on-copy + post-copy verify),
    // progress throttle from hooks (tests pin it to ZERO for determinism).
    let parallel_options = crate::parallel::ParallelOptions {
        verify: true,
        progress_interval: hooks.progress_interval,
        tamper_dest_after_copy: hooks.tamper_dest_after_copy,
    };
    // blake3 digests of files copied this run, keyed `<source-name>/<rel>` (D1).
    let mut copied_hashes: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    let parent = repo.latest_snapshot();
    // The parent is only a link source for the SAME job capturing the SAME source -- the
    // manifest is the only place that can prove either, so read it once up front. A
    // missing or unreadable parent manifest simply means "no parent": copy everything.
    let parent_manifest = parent.as_deref().and_then(|id| repo.read_manifest(id).ok());
    // Reserve the id up front. `allocate_snapshot_id` CREATES the snapshot directory as
    // the reservation (create_dir, AlreadyExists -> next suffix), so two runs started in
    // the same second cannot share a directory even if the run lock was somehow bypassed.
    let snapshot_id = repo.allocate_snapshot_id(started_at)?;
    let snapshot_dir = repo.snapshot_dir(&snapshot_id);

    emit(Event::RunStarted { run_id: snapshot_id.clone(), jobs: resolved.sources.len() });

    // F29: a destination that cannot hardlink turns every unchanged file into a full
    // recopy on every run -- "versioned" backup silently losing versioning. Warn exactly
    // once per run; the run itself still works (a failed link falls back to copying).
    if let Some(message) = link_degraded_message(&dest_capability) {
        emit(Event::RunWarning { message });
    }

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
    let mut manifest_sources = Vec::new();
    let mut cancelled = false;

    for (name, src) in &resolved.sources {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let src_clock = Instant::now();

        let dest_root = snapshot_dir.join(name);
        emit(Event::JobStarted {
            job: name.clone(),
            source: src.clone(),
            dest: dest_root.clone(),
        });

        // D2: `git gc --auto` on git sources, BEFORE the walk (packing while copying
        // would move files mid-scan). A `.git` DIRECTORY marks a normal checkout, a
        // `.git` FILE marks a worktree/submodule -- both count. Best-effort: a failure
        // warns and the backup proceeds; housekeeping must never fail a run.
        if job.git_gc && std::fs::symlink_metadata(src.join(".git")).is_ok() {
            if let Err(e) = (hooks.git_gc)(src) {
                tracing::warn!("git gc --auto on {} failed: {e}", src.display());
                emit(Event::RunWarning {
                    message: format!(
                        "git gc --auto on {} failed ({e}) -- the backup itself is unaffected",
                        src.display()
                    ),
                });
            }
        }

        let excludes = job.excludes.compile(src, &dest_root);
        let walked = walk::walk(src, &excludes, |files, bytes| {
            emit(Event::ScanProgress { files_seen: files, bytes_seen: bytes });
        })?;

        // A source root that could not be listed at all: the walk saw nothing because
        // there was nothing it was ALLOWED to see. Writing an empty "Completed" snapshot
        // here would manufacture success out of a tree that was never read -- and worse,
        // that snapshot becomes the link parent the next run trusts. Fail the run and
        // drop the partial snapshot, exactly as a cancel does.
        if walked.entries.is_empty() && walked.unreadable_paths.iter().any(|p| p == src) {
            let _ = repo.delete_snapshot(&snapshot_id);
            return Err(Error::UnreadableSourceRoot(src.clone()));
        }

        // Anything else the walk could not read is a real hole in this snapshot: count it
        // as failed (which forces at least Partial below) and name every recorded path.
        let src_unreadable = walked.unreadable;
        if src_unreadable > 0 {
            stats.files_failed += src_unreadable;
            for path in &walked.unreadable_paths {
                emit(Event::SourceUnreadable {
                    path: path.clone(),
                    message: "could not be read (locked, access denied, or it vanished \
                              mid-scan) -- it is NOT in this snapshot"
                        .into(),
                });
            }
        }
        let src_reparse = walked.reparse_skipped;
        stats.reparse_skipped += src_reparse;

        // D8: empty directories are part of the tree's shape. Create the source's own
        // subfolder and every empty dir beneath it explicitly -- the copy phase below
        // only ever makes directories that have a file in them. Best-effort: a failure
        // here loses tree shape, not content, so it is logged rather than counted as a
        // file failure.
        if let Err(e) = std::fs::create_dir_all(&dest_root) {
            tracing::warn!("could not create {}: {e}", dest_root.display());
        }
        for rel in &walked.empty_dirs {
            if let Err(e) = std::fs::create_dir_all(dest_root.join(rel)) {
                tracing::warn!("could not create empty directory {}: {e}", dest_root.join(rel).display());
            }
        }

        let prev_root = parent_link_root(
            &repo,
            parent.as_deref(),
            parent_manifest.as_ref(),
            &job.name,
            name,
            src,
        )
        .filter(|p| p.is_dir());
        // The mtime tolerance is a property of the two volumes involved, not of any
        // file: decided once per source, then applied to every comparison below. Both
        // sides come from the run's injected probe (F20's three-way answer), so tests
        // can pin the FAT window without a real FAT volume.
        let coarse_src = (hooks.probe)(src).stores_coarse_mtimes();
        let mtime_tolerance = if coarse_src || dest_capability.stores_coarse_mtimes() {
            MTIME_TOLERANCE
        } else {
            std::time::Duration::ZERO
        };
        let planned = plan(walked.entries, src, prev_root.as_deref(), mtime_tolerance);

        emit(Event::PlanReady {
            to_copy: planned.to_copy,
            to_link: planned.to_link,
            bytes_to_copy: planned.bytes_to_copy,
            // A snapshot job never deletes anything, so the count is known and it is zero.
            // This is Some(0), not None: "nothing will be deleted" and "we could not work
            // out what will be deleted" must never render the same way.
            deletes: Some(0),
        });

        let counts = execute(
            &planned,
            src,
            &dest_root,
            prev_root.as_deref(),
            &parallel_options,
            cancel,
            emit,
        );

        // D1: the manifest records the hashes of the files copied this run (only copied
        // files -- linked files' content was verified by the run that first copied
        // them), keyed so a later verify pass or restore can map them back.
        for f in &counts.copied_files {
            copied_hashes.insert(
                format!("{name}/{}", f.rel.to_string_lossy().replace('\\', "/")),
                f.hash.clone(),
            );
        }

        stats.files_copied += counts.copied;
        stats.files_linked += counts.linked;
        stats.files_failed += counts.failed;
        stats.bytes_copied += counts.bytes_copied;
        stats.bytes_linked += counts.bytes_linked;
        let src_files = counts.copied + counts.linked;
        let src_bytes = counts.bytes_copied + counts.bytes_linked;
        if counts.cancelled {
            cancelled = true;
        }

        manifest_sources.push(SnapshotSource {
            name: name.clone(),
            original: src.clone(),
            files: src_files,
            bytes: src_bytes,
        });

        // L1: JobDone carries what THIS source did, not the run so far -- a consumer
        // summing them must land exactly on the RunDone totals.
        emit(Event::JobDone {
            job: name.clone(),
            stats: JobStats {
                files_copied: counts.copied,
                files_linked: counts.linked,
                files_failed: counts.failed + src_unreadable,
                bytes_copied: counts.bytes_copied,
                bytes_linked: counts.bytes_linked,
                reparse_skipped: src_reparse,
                duration_ms: src_clock.elapsed().as_millis() as u64,
                ..Default::default()
            },
        });
        if cancelled {
            break;
        }
    }

    stats.duration_ms = clock.elapsed().as_millis() as u64;

    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else if stats.files_failed > 0 {
        // Deliberately not "failed". A run that copied 4,000 files and could not read one
        // locked browser database has not failed; reporting it as a flat failure is how a
        // user learns to ignore the word.
        RunOutcome::Partial { failed: stats.files_failed }
    } else if stats.sources_skipped > 0 {
        // Nothing failed at the file level, but the job was asked to protect more than it
        // did -- that must not read as a clean run. The skipped sources themselves are in
        // the manifest and in the SourceSkipped events.
        RunOutcome::Partial { failed: 0 }
    } else {
        RunOutcome::Ok
    };

    let manifest = SnapshotManifest {
        id: snapshot_id.clone(),
        job: job.name.clone(),
        // One helper, one fallback (L2): repo::rfc3339 renders RFC 3339 and degrades an
        // unformattable time to the epoch, never to a blank string.
        started: rfc3339(started_at),
        finished: rfc3339(OffsetDateTime::now_utc()),
        sources: manifest_sources,
        files_copied: stats.files_copied,
        files_linked: stats.files_linked,
        files_failed: stats.files_failed,
        bytes_copied: stats.bytes_copied,
        bytes_linked: stats.bytes_linked,
        duration_ms: stats.duration_ms,
        outcome: outcome.clone(),
        parent: parent.clone(),
        skipped_sources: resolved.skipped.iter().map(|s| s.configured.clone()).collect(),
        copied_hashes,
    };

    // A cancelled run leaves a partial snapshot directory. Remove it: a half-captured
    // snapshot that looks like a complete one is worse than no snapshot, because a later
    // run would link against it and propagate the gap forward.
    if cancelled {
        let _ = repo.delete_snapshot(&snapshot_id);
    } else {
        if let Err(e) = (hooks.write_manifest)(&repo, &manifest) {
            // F36: the snapshot itself is complete and restorable on disk -- only the
            // bookkeeping failed. The error must say exactly that, so a user (or a
            // support doc) can recover manually; a flat "run failed" would hide it.
            // Emitting RunDone first gives the shell's history a truthful record of the
            // work that DID happen.
            let reason = format!(
                "the snapshot files are complete and restorable at {} -- only the manifest \
                 (the run's bookkeeping) could not be written: {e}. It will be listed as \
                 incomplete and pruned by the next run unless its manifest is rewritten.",
                snapshot_dir.display()
            );
            emit(Event::RunDone {
                run_id: snapshot_id.clone(),
                outcome: RunOutcome::Failed { reason: reason.clone() },
                stats: stats.clone(),
            });
            return Err(Error::other(reason));
        }
        if let Err(e) = (hooks.write_readme)(&repo) {
            // F36: the README is the bottom tier of the restore story, but a finished
            // backup must not report failure over it. Log it and warn the run; the
            // outcome stands.
            tracing::warn!(
                "could not refresh {}: {e}",
                repo.root.join(crate::repo::README_NAME).display()
            );
            emit(Event::RunWarning {
                message: format!(
                    "the backup completed, but the recovery-instructions file {} could not \
                     be written: {e}. Snapshots remain fully restorable via the portable \
                     restore tool or File Explorer.",
                    crate::repo::README_NAME
                ),
            });
        }

        // D2: retention. Only now -- the manifest safely written, the run Ok or Partial
        // -- does this job's history get thinned. A Failed run returns before this
        // point and a Cancelled one is the other branch, and neither may delete
        // anything. Per-JOB: snapshots belonging to other jobs at the same destination
        // are never candidates (see Repo::snapshots_to_prune_for_job).
        if let Some(keep) = keep_snapshots {
            let pruned = repo.prune_job_snapshots(&job.name, keep);
            if pruned > 0 {
                stats.snapshots_pruned += pruned;
            }
        }
    }

    emit(Event::RunDone {
        run_id: snapshot_id,
        outcome,
        stats: stats.clone(),
    });

    Ok(manifest)
}

/// Turn a plan into the parallel copy/link execution and back into engine-shaped counts.
///
/// The actual threading, progress throttling, link-then-fallback logic and (D1) the
/// hash-and-verify pass live in [`crate::parallel`], shared with restore.
fn execute(
    planned: &Plan,
    src: &Path,
    dest_root: &Path,
    prev_root: Option<&Path>,
    options: &crate::parallel::ParallelOptions,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> parallel::Counts {
    let items: Vec<parallel::CopyItem> = planned
        .files
        .iter()
        .map(|pf| {
            let link_from = match pf.action {
                Action::Link => prev_root.map(|p| p.join(&pf.entry.rel)),
                Action::Copy => None,
            };
            parallel::CopyItem {
                rel: pf.entry.rel.clone(),
                from: src.join(&pf.entry.rel),
                to: dest_root.join(&pf.entry.rel),
                size: pf.entry.size,
                link_from,
            }
        })
        .collect();

    parallel::run_parallel(&items, options, cancel, emit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JobKind;
    use crate::exclude::ExcludeRules;

    fn touch(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn job(sources: Vec<PathBuf>, dest: PathBuf) -> Job {
        Job {
            id: "t".into(),
            name: "Test".into(),
            sources,
            dest,
            dest_portable: None,
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        }
    }

    fn run(j: &Job) -> (SnapshotManifest, Vec<Event>) {
        let mut events = Vec::new();
        let m = run_job(j, None, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
        (m, events)
    }

    #[test]
    fn a_first_run_copies_everything_and_links_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("sub/b.txt"), "bbb");
        let dest = tmp.path().join("backup");

        let (m, _) = run(&job(vec![src], dest.clone()));

        assert_eq!(m.files_copied, 2);
        assert_eq!(m.files_linked, 0);
        assert_eq!(m.parent, None);
        assert_eq!(m.outcome, RunOutcome::Ok);

        let snap = dest.join("snapshots").join(&m.id).join("proj");
        assert_eq!(std::fs::read_to_string(snap.join("a.txt")).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(snap.join("sub/b.txt")).unwrap(), "bbb");
    }

    /// The core of the snapshot design: a second run of an unchanged tree must write no
    /// bytes at all, yet produce a complete second snapshot.
    #[test]
    fn a_second_run_links_unchanged_files_instead_of_copying_them() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bbb");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let (first, _) = run(&j);
        let (second, _) = run(&j);

        assert_ne!(first.id, second.id, "each run is its own snapshot");
        assert_eq!(second.files_copied, 0, "nothing changed, so nothing should be copied");
        assert_eq!(second.files_linked, 2);
        assert_eq!(second.bytes_copied, 0);
        assert_eq!(second.parent.as_deref(), Some(first.id.as_str()));

        // Both snapshots are complete and readable.
        for id in [&first.id, &second.id] {
            let snap = dest.join("snapshots").join(id).join("proj");
            assert_eq!(std::fs::read_to_string(snap.join("a.txt")).unwrap(), "aaa");
            assert_eq!(std::fs::read_to_string(snap.join("b.txt")).unwrap(), "bbb");
        }
    }

    /// Point-in-time recovery, stated as a test: editing a file must not reach back into
    /// the snapshot taken before the edit.
    #[test]
    fn editing_a_file_leaves_the_older_snapshot_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        let file = src.join("notes.txt");
        touch(&file, "ORIGINAL");
        let dest = tmp.path().join("backup");
        let j = job(vec![src.clone()], dest.clone());

        let (first, _) = run(&j);

        // Change the content AND the length, so size alone settles it.
        std::fs::write(&file, "EDITED - LONGER CONTENT").unwrap();
        let (second, _) = run(&j);

        assert_eq!(second.files_copied, 1, "the edited file must be copied, not linked");

        let old = dest.join("snapshots").join(&first.id).join("proj").join("notes.txt");
        let new = dest.join("snapshots").join(&second.id).join("proj").join("notes.txt");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "ORIGINAL");
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "EDITED - LONGER CONTENT");
    }

    /// The gap that snapshots exist to close. The PowerShell app's project backup used a
    /// plain recursive copy, so a file deleted from the source stayed in the destination
    /// forever with no indication of when it had been live. Here, deleting a file simply
    /// means the next snapshot does not contain it -- while every earlier snapshot still does.
    #[test]
    fn deleting_a_source_file_keeps_it_in_earlier_snapshots_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        touch(&src.join("doomed.txt"), "doomed");
        let dest = tmp.path().join("backup");
        let j = job(vec![src.clone()], dest.clone());

        let (first, _) = run(&j);
        std::fs::remove_file(src.join("doomed.txt")).unwrap();
        let (second, _) = run(&j);

        let old = dest.join("snapshots").join(&first.id).join("proj");
        let new = dest.join("snapshots").join(&second.id).join("proj");

        assert!(old.join("doomed.txt").is_file(), "history must retain the deleted file");
        assert!(!new.join("doomed.txt").exists(), "the new snapshot reflects the source");
        assert!(new.join("keep.txt").is_file());
    }

    #[test]
    fn exclusions_are_applied_to_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.js"), "k");
        touch(&src.join("node_modules/pkg/junk.js"), "j");
        let dest = tmp.path().join("backup");

        let mut j = job(vec![src], dest.clone());
        j.excludes = ExcludeRules::project();
        let (m, _) = run(&j);

        let snap = dest.join("snapshots").join(&m.id).join("proj");
        assert!(snap.join("keep.js").is_file());
        assert!(!snap.join("node_modules").exists());
        assert_eq!(m.files_copied, 1);
    }

    #[test]
    fn two_sources_with_the_same_leaf_name_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("one/shared");
        let b = tmp.path().join("two/shared");
        touch(&a.join("f.txt"), "a");
        touch(&b.join("f.txt"), "b");

        let j = job(vec![a, b], tmp.path().join("backup"));
        let err = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("same folder"),
            "expected a collision error, got: {err}"
        );
    }

    #[test]
    fn a_destination_inside_its_source_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        let dest = src.join("backup");

        let j = job(vec![src], dest);
        assert!(run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn a_job_with_no_existing_sources_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let j = job(vec![tmp.path().join("nope")], tmp.path().join("backup"));
        let err = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("no valid source"));
    }

    // ---------------------------------------------------------------- preset expansion

    fn fake_preset(key: &str, path: &Path, found: bool) -> crate::presets::Preset {
        crate::presets::Preset {
            key: key.into(),
            label: key.into(),
            path: Some(path.to_path_buf()),
            found,
        }
    }

    #[test]
    fn presets_are_expanded_and_named_by_their_key_not_their_leaf_folder() {
        let tmp = tempfile::tempdir().unwrap();
        // The resolved path's leaf is "User Data", exactly like the real Chrome preset --
        // the destination subfolder must still come out "Chrome".
        let chrome_profile = tmp.path().join("AppData/Local/Google/Chrome/User Data");
        touch(&chrome_profile.join("bookmarks.json"), "{}");

        let mut j = job(vec![], tmp.path().join("backup"));
        j.presets = vec!["Chrome".into()];

        let presets = vec![fake_preset("Chrome", &chrome_profile, true)];
        let resolved = resolve_sources_against(&j, &presets).unwrap();

        assert_eq!(resolved.sources.len(), 1);
        assert_eq!(resolved.sources[0].0, "Chrome", "must be named by the preset key");
        assert_eq!(resolved.sources[0].1, guards::canonical_path(&chrome_profile).unwrap());
        assert!(resolved.skipped.is_empty());
    }

    #[test]
    fn a_preset_marked_not_found_is_skipped_not_errored() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");

        let mut j = job(vec![], tmp.path().join("backup"));
        j.presets = vec!["Ghost".into()];
        let presets = vec![fake_preset("Ghost", &missing, false)];

        let err = resolve_sources_against(&j, &presets).unwrap_err();
        // With nothing else in the job, this correctly falls through to the same
        // "nothing to back up" error a job with only a missing plain source would get --
        // not a hard failure over the one missing preset.
        assert!(err.to_string().contains("no valid source"));
    }

    /// ...but a skipped preset alongside a REAL source must be reported, not dropped:
    /// the config claims something is being backed up that is not.
    #[test]
    fn a_preset_whose_folder_vanished_is_reported_as_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        touch(&real.join("f.txt"), "x");
        let gone = tmp.path().join("chrome-profile-gone");

        let mut j = job(vec![real], tmp.path().join("backup"));
        j.presets = vec!["Chrome".into()];
        let presets = vec![fake_preset("Chrome", &gone, false)];

        let resolved = resolve_sources_against(&j, &presets).unwrap();
        assert_eq!(resolved.sources.len(), 1, "the real source still backs up");
        assert_eq!(resolved.skipped.len(), 1);
        assert_eq!(resolved.skipped[0].configured, "Chrome");
    }

    #[test]
    fn a_stale_preset_key_with_no_matching_preset_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        touch(&real.join("f.txt"), "x");

        let mut j = job(vec![real], tmp.path().join("backup"));
        // "Firefox" was disabled/never enabled, or this config predates a preset that was
        // since removed -- either way, it should not exist in `available_presets`.
        j.presets = vec!["SomePresetThatDoesNotExist".into()];

        let resolved = resolve_sources_against(&j, &[]).unwrap();
        assert_eq!(resolved.sources.len(), 1, "the real source must still back up fine");
        assert_eq!(
            resolved.skipped.len(),
            1,
            "the dangling key is reported, not silently dropped"
        );
        assert_eq!(resolved.skipped[0].configured, "SomePresetThatDoesNotExist");
    }

    #[test]
    fn presets_and_plain_sources_combine_in_one_job() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        touch(&proj.join("f.txt"), "x");
        let desktop = tmp.path().join("Desktop");
        touch(&desktop.join("notes.txt"), "x");

        let mut j = job(vec![proj], tmp.path().join("backup"));
        j.presets = vec!["Desktop".into()];
        let presets = vec![fake_preset("Desktop", &desktop, true)];

        let resolved = resolve_sources_against(&j, &presets).unwrap();
        let names: Vec<&str> =
            resolved.sources.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["proj", "Desktop"], "sources first, then presets, in order");
    }

    /// The collision this preset-naming design exists to prevent: a plain custom source
    /// folder that happens to be named exactly like an enabled preset's key would silently
    /// merge the two into one destination subfolder without this guard.
    #[test]
    fn a_custom_source_colliding_with_a_preset_name_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let custom_desktop = tmp.path().join("custom/Desktop");
        touch(&custom_desktop.join("f.txt"), "x");
        let real_desktop = tmp.path().join("real-desktop");
        touch(&real_desktop.join("g.txt"), "x");

        let mut j = job(vec![custom_desktop], tmp.path().join("backup"));
        j.presets = vec!["Desktop".into()];
        let presets = vec![fake_preset("Desktop", &real_desktop, true)];

        let err = resolve_sources_against(&j, &presets).unwrap_err();
        assert!(err.to_string().contains("Desktop"), "error should name the collision: {err}");
    }

    /// `run_job` must not choke merely because a job has preset keys set -- it calls the
    /// real `crate::presets::all()` internally (not the injectable test seam above, which
    /// is deliberately private to keep real known-folder resolution out of this test
    /// suite). A key with no match in the real preset list is simply skipped, so a job
    /// that also has a real plain source still backs that up successfully.
    #[test]
    fn run_job_does_not_fail_on_a_preset_key_with_no_real_match() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        touch(&proj.join("f.txt"), "x");

        let mut j = job(vec![proj], tmp.path().join("backup"));
        j.presets = vec!["ThisPresetKeyDoesNotExistOnAnyMachine".into()];

        let manifest = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(manifest.files_copied, 1);
    }

    #[test]
    fn a_cancelled_run_leaves_no_partial_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        // Comfortably more than the 64-file progress throttle, so a cancel can land while
        // there is still work left to abandon.
        for i in 0..400 {
            touch(&src.join(format!("f{i}.txt")), "content");
        }
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let cancel = AtomicBool::new(false);
        let m = run_job(&j, None, &cancel, &mut |e| {
            if matches!(e, Event::OverallProgress { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

        assert_eq!(m.outcome, RunOutcome::Cancelled);
        assert!(
            !dest.join("snapshots").join(&m.id).exists(),
            "a half-captured snapshot must not survive -- the next run would link against \
             it and carry the gap forward"
        );
        let repo = Repo::open(&dest).unwrap();
        assert!(repo.snapshot_ids().is_empty());
    }

    // ------------------------------------------------------- F3: cross-process run lock

    /// F3, engine side: while a run lock is held on the repository -- by this process
    /// standing in for the headless scheduler, say -- `run_job` must fail with
    /// `RunInProgress` BEFORE anything is written: no new snapshot dir, no manifest, and
    /// the existing snapshots untouched.
    #[test]
    fn run_job_refuses_to_start_while_the_repo_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let (first, _) = run(&j);
        let repo = Repo::open(&dest).unwrap();
        let _held = repo.acquire_run_lock().unwrap();

        let err = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::RunInProgress(_)),
            "expected RunInProgress, got: {err}"
        );
        assert_eq!(
            repo.snapshot_ids(),
            vec![first.id.clone()],
            "a contended run must not create or change any snapshot"
        );
        assert!(
            dest.join(".backstar").join(crate::runlock::REPO_RUN_LOCK_NAME).is_file(),
            "the held lock is observable on disk"
        );

        drop(_held);
        // And once the holder finishes, the next run proceeds -- every two-run test in
        // this suite also proves release works, this one checks it after a contention.
        let (second, _) = run(&j);
        assert_eq!(second.parent.as_deref(), Some(first.id.as_str()));
    }

    // ------------------------------------------------------- F35: incomplete snapshots

    /// A run killed mid-copy leaves a manifest-less snapshot dir holding partial files
    /// and `.bstmp-*` temp junk. The next run -- holding the repo lock, so nothing can be
    /// mid-write -- prunes it wholesale (F35). Separately, a temp file sitting in the
    /// live SOURCE must be neither backed up nor deleted (F39 walker exclusion).
    #[test]
    fn an_interrupted_runs_leftover_snapshot_is_pruned_at_the_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("real.txt"), "real");
        let dest = tmp.path().join("backup");
        let j = job(vec![src.clone()], dest.clone());

        let (first, _) = run(&j);
        let repo = Repo::open(&dest).unwrap();

        // Fabricate the remains of a killed run: a manifest-less snapshot dir with
        // partial content and temp orphans of both prefixes, at two depths.
        let crashed = repo.snapshot_dir("2020-01-01T00-00-00Z");
        touch(&crashed.join("proj/.bstmp-999-0-fragment"), "junk");
        touch(&crashed.join("proj/sub/.backstar-tmp-42-deadfile"), "junk");
        touch(&crashed.join("proj/sub/copied-before-the-kill.txt"), "half a backup");

        // And a temp file in the live source, as a crashed reverse-mirror would leave.
        let src_orphan = src.join(".backstar-tmp-42-sourcejunk");
        touch(&src_orphan, "junk");

        let (second, _) = run(&j);

        assert!(
            !crashed.exists(),
            "the whole manifest-less dir is pruned -- partial files, temp orphans and all"
        );
        assert!(repo.read_manifest(&first.id).is_ok(), "the healthy snapshot is intact");
        assert!(dest.join("snapshots").join(&first.id).join("proj/real.txt").is_file());
        assert!(
            src_orphan.is_file(),
            "a backup tool never deletes from the source -- the walker only skips it"
        );
        let snap = dest.join("snapshots").join(&second.id).join("proj");
        assert!(snap.join("real.txt").is_file());
        assert!(
            !snap.join(".backstar-tmp-42-sourcejunk").exists(),
            "temp files must never be enumerated into a snapshot"
        );
        // The pruned dir must not disturb id allocation or the parent chain: the new run
        // chains to the last COMPLETE snapshot.
        assert_ne!(second.id, first.id);
        assert_eq!(second.parent.as_deref(), Some(first.id.as_str()));
        assert_eq!(second.files_linked, 1, "real.txt links against the healthy parent");
    }

    // ------------------------------------------------------- F29: link-capability warning

    /// A destination volume that cannot hardlink turns every unchanged file into a full
    /// recopy, every run, with no signal (F29). The run must say so -- once -- while
    /// still completing correctly (a failed link falls back to copying; here the real
    /// volume DOES link, so the second run links as usual -- the warning is about the
    /// probed capability, not the individual link attempts).
    #[test]
    fn a_destination_that_cannot_hardlink_warns_once_per_run_and_still_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest);

        // Capability injection: report the (actually NTFS) temp volume as exFAT.
        let mut hooks = RunHooks::production();
        hooks.probe = |p| ProbeOutcome::Local(crate::volume::VolumeCapability {
            root: p.to_path_buf(),
            filesystem: "exFAT".into(),
            hard_links: false,
            serial: 1,
            guid_path: None,
        });

        for run_no in 1..=2 {
            let mut events = Vec::new();
            let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
                .unwrap();
            let warnings: Vec<_> = events
                .iter()
                .filter(|e| matches!(e, Event::RunWarning { .. }))
                .collect();
            assert_eq!(warnings.len(), 1, "exactly one warning per run (run {run_no})");
            let Event::RunWarning { message } = warnings[0] else { unreachable!() };
            assert!(message.contains("hard link"), "{message}");
            assert_eq!(m.outcome, RunOutcome::Ok);
        }
    }

    /// The network-destination variant of the same warning (F20+F29): a UNC path probes
    /// as Network, and the warning says what that means rather than crying "drive
    /// unavailable".
    #[test]
    fn a_network_destination_warns_about_versioning_without_crying_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest);

        let mut hooks = RunHooks::production();
        hooks.probe = |p| ProbeOutcome::Network { path: p.to_path_buf() };

        let mut events = Vec::new();
        let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap();
        assert_eq!(m.outcome, RunOutcome::Ok);
        let warnings: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::RunWarning { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("network"), "{}", warnings[0]);
        assert!(!warnings[0].contains("unavailable"), "no false alarm: {}", warnings[0]);
    }

    // ------------------------------------------------------- F36: run-outcome honesty

    /// A README write failure must never fail a finished run: logged and surfaced as a
    /// warning, outcome untouched.
    #[test]
    fn a_readme_write_failure_warns_but_never_fails_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let mut hooks = RunHooks::production();
        hooks.write_readme = |_| Err(Error::other("injected: disk full"));

        let mut events = Vec::new();
        let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
            .expect("a README failure must not fail the run");
        assert_eq!(m.outcome, RunOutcome::Ok);
        assert_eq!(m.files_copied, 1);
        assert!(
            events.iter().any(|e| matches!(e, Event::RunWarning { message } if message.contains(crate::repo::README_NAME))),
            "a RunWarning names the README"
        );
        // The manifest -- the actual bookkeeping -- was still written.
        assert!(Repo::open(&dest).unwrap().read_manifest(&m.id).is_ok());
    }

    /// A manifest write failure: the snapshot is complete and restorable on disk, and the
    /// error must SAY so -- with the path -- instead of reporting a flat failure.
    #[test]
    fn a_manifest_write_failure_names_the_complete_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let mut hooks = RunHooks::production();
        hooks.write_manifest = |_, _| Err(Error::other("injected: access denied"));

        let mut events = Vec::new();
        let err = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("complete and restorable"), "{text}");
        assert!(text.contains("manifest"), "{text}");
        assert!(text.contains(&dest.join("snapshots").display().to_string()), "{text}");

        // The snapshot really is complete on disk...
        let repo = Repo::open(&dest).unwrap();
        let ids = repo.snapshot_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            std::fs::read_to_string(repo.snapshot_dir(&ids[0]).join("proj/a.txt")).unwrap(),
            "a"
        );
        // ...and the run's record is honest: a Failed RunDone carrying the work done.
        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { outcome, stats, .. } => Some((outcome.clone(), stats.clone())),
                _ => None,
            })
            .expect("a RunDone must be emitted before the error returns");
        assert!(matches!(done.0, RunOutcome::Failed { .. }), "{:?}", done.0);
        assert_eq!(done.1.files_copied, 1);
        // And the listing API reports the dir as incomplete, not as a good backup.
        assert_eq!(repo.list_snapshots_full().incomplete, ids);
    }

    // ------------------------------------------------------- L1: per-source JobDone

    /// JobDone carries PER-SOURCE stats (L1): the two sources' reports must sum exactly
    /// to the RunDone totals, and the second source's report must not carry the first's.
    #[test]
    fn job_done_stats_are_per_source_and_sum_to_the_run_totals() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        touch(&one.join("a.txt"), "aa");
        touch(&one.join("b.txt"), "bb");
        touch(&two.join("c.txt"), "ccc");
        let dest = tmp.path().join("backup");

        let (m, events) = run(&job(vec![one, two], dest));

        let dones: Vec<JobStats> = events
            .iter()
            .filter_map(|e| match e {
                Event::JobDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(dones.len(), 2, "one JobDone per source");
        assert_eq!(dones[0].files_copied, 2, "the FIRST source's count");
        assert_eq!(dones[1].files_copied, 1, "not run-cumulative");
        assert_eq!(dones[1].bytes_copied, 3);
        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(dones[0].files_copied + dones[1].files_copied, done.files_copied);
        assert_eq!(done.files_copied, m.files_copied);
    }

    // ------------------------------------------- D1/F7: hash-on-copy + verify pass

    /// End to end: the manifest carries blake3 hashes for exactly the files copied this
    /// run, VerifyDone reports examining exactly those, and FileDone carries the hash.
    /// The second run copies nothing (all linked) -- and says so honestly with
    /// VerifyDone{0, []} and an empty hash map.
    #[test]
    fn the_manifest_records_hashes_of_copied_files_and_verify_reports_them() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("sub/b.txt"), "bbb");
        let dest = tmp.path().join("backup");
        let j = job(vec![src.clone()], dest.clone());

        let mut events = Vec::new();
        let m = run_job_with(&j, None, &RunHooks::production(), &AtomicBool::new(false), &mut |e| {
            events.push(e)
        })
        .unwrap();

        assert_eq!(m.files_copied, 2);
        assert_eq!(m.copied_hashes.len(), 2, "one hash per copied file");
        let expect_a = crate::copy::blake3_hex(&src.join("a.txt")).unwrap();
        let expect_b = crate::copy::blake3_hex(&src.join("sub/b.txt")).unwrap();
        assert_eq!(m.copied_hashes.get("proj/a.txt").unwrap(), &expect_a);
        assert_eq!(m.copied_hashes.get("proj/sub/b.txt").unwrap(), &expect_b);
        // Wire format: camelCase field, /-separated keys.
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"copiedHashes\""), "{json}");
        assert!(json.contains("\"proj/sub/b.txt\""), "{json}");

        let verify: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(verify.len(), 1, "one VerifyDone per source phase");
        assert_eq!(verify[0].0, 2, "examined == copied count");
        assert!(verify[0].1.is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            Event::FileDone { path, hash: Some(h), .. }
                if path == Path::new("a.txt") && h == &expect_a
        )));

        // Second run: everything links -- nothing to hash, nothing to verify, and the
        // manifest says so rather than re-recording the parent's hashes.
        let mut events2 = Vec::new();
        let m2 = run_job_with(&j, None, &RunHooks::production(), &AtomicBool::new(false), &mut |e| {
            events2.push(e)
        })
        .unwrap();
        assert_eq!(m2.files_copied, 0);
        assert_eq!(m2.files_linked, 2);
        assert!(m2.copied_hashes.is_empty(), "linked files carry no hashes (their content was verified when first copied)");
        let verify2: Vec<_> = events2
            .iter()
            .filter_map(|e| match e {
                Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.is_empty())),
                _ => None,
            })
            .collect();
        assert_eq!(verify2, vec![(0, true)], "a zero-copy phase verifies nothing and says so");
    }

    /// D1 corruption injection: the dest bytes are flipped between copy and verify (test
    /// seam). The run must report Partial, count the failure, name the file, leave the
    /// bad copy OUT of the snapshot and out of the manifest hashes -- so the next run
    /// cannot hardlink the corrupt bytes forward.
    #[test]
    fn a_corrupted_copy_fails_verification_and_never_enters_the_snapshot() {
        fn corrupt_a_txt(path: &Path) {
            if path.file_name().map(|n| n == "a.txt").unwrap_or(false) {
                let mut data = std::fs::read(crate::guards::extended_path(path)).unwrap();
                if let Some(first) = data.first_mut() {
                    *first ^= 0xFF;
                }
                std::fs::write(crate::guards::extended_path(path), data).unwrap();
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bbb");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let mut hooks = RunHooks::production();
        hooks.tamper_dest_after_copy = Some(corrupt_a_txt);
        let mut events = Vec::new();
        let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap();

        assert_eq!(m.outcome, RunOutcome::Partial { failed: 1 });
        assert_eq!(m.files_copied, 1, "only the good file counts as copied");
        assert_eq!(m.files_failed, 1);
        assert_eq!(m.copied_hashes.len(), 1);
        assert!(m.copied_hashes.contains_key("proj/b.txt"), "the bad file's hash is NOT recorded");

        let snap = dest.join("snapshots").join(&m.id).join("proj");
        assert!(!snap.join("a.txt").exists(), "the bad copy was removed from the snapshot");
        assert_eq!(std::fs::read_to_string(snap.join("b.txt")).unwrap(), "bbb");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::FileFailed(f) if f.path == Path::new("a.txt") && f.message.contains("verification failed")
        )));
        let verify = events.iter().find_map(|e| match e {
            Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
            _ => None,
        }).expect("VerifyDone");
        assert_eq!(verify.0, 2);
        assert_eq!(verify.1, vec![PathBuf::from("a.txt")]);

        // And the NEXT run repairs: a.txt is absent from the parent snapshot, so it is
        // planned as a fresh copy -- corruption does not propagate.
        let m3 = run_job_with(&j, None, &RunHooks::production(), &AtomicBool::new(false), &mut |_| {})
            .unwrap();
        assert_eq!(m3.files_copied, 1, "a.txt is re-copied fresh");
        assert_eq!(m3.files_linked, 1, "b.txt links");
        assert_eq!(m3.outcome, RunOutcome::Ok);
        let snap3 = dest.join("snapshots").join(&m3.id).join("proj");
        assert_eq!(std::fs::read_to_string(snap3.join("a.txt")).unwrap(), "aaa");
    }

    /// D1's link-time half, end to end: with the FAT tolerance window active (injected),
    /// a same-size file whose mtime moved one second and whose CONTENT changed must be
    /// copied (hash mismatch), while one whose content is unchanged links.
    #[cfg(windows)]
    #[test]
    fn a_tolerance_window_match_is_hash_checked_before_linking() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        let file = src.join("notes.txt");
        touch(&file, "ORIGINAL"); // 8 bytes
        let dest = tmp.path().join("backup");
        let j = job(vec![src.clone()], dest.clone());

        // Pin the FAT window without a FAT volume: coarse mtimes on both sides.
        let mut hooks = RunHooks::production();
        hooks.probe = |p| ProbeOutcome::Local(crate::volume::VolumeCapability {
            root: p.to_path_buf(),
            filesystem: "exFAT".into(),
            hard_links: true, // keep the capability warning out of this test's way
            serial: 1,
            guid_path: None,
        });

        let first = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let prev_copy = dest.join("snapshots").join(&first.id).join("proj/notes.txt");
        let prev_mtime = std::fs::metadata(&prev_copy).unwrap().modified().unwrap();

        // Same length, different content, mtime exactly one second on: inside the FAT
        // window. Pre-D1 this linked the OLD bytes forward.
        std::fs::write(&file, "EDITED!!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(prev_mtime + std::time::Duration::from_secs(1))
            .unwrap();
        let second = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(second.files_copied, 1, "hash mismatch inside the window must copy");
        assert_eq!(second.files_linked, 0);
        let new_copy = dest.join("snapshots").join(&second.id).join("proj/notes.txt");
        assert_eq!(std::fs::read_to_string(&new_copy).unwrap(), "EDITED!!");

        // Same content, mtime bumped one second: the hash check passes and the file links.
        let second_mtime = std::fs::metadata(&new_copy).unwrap().modified().unwrap();
        std::fs::write(&file, "EDITED!!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(second_mtime + std::time::Duration::from_secs(1))
            .unwrap();
        let third = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(third.files_linked, 1, "same content inside the window links");
        assert_eq!(third.files_copied, 0);
    }

    /// MT7 at the run level: OverallProgress fires and reaches the plan total, and
    /// FileStarted/FileDone bracket each copied file -- with the progress throttle pinned
    /// to zero so nothing depends on timing.
    #[test]
    fn a_run_reports_progress_events_and_brackets_each_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bb");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest);

        let mut hooks = RunHooks::production();
        hooks.progress_interval = std::time::Duration::ZERO;
        let mut events = Vec::new();
        run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();

        let last_overall = events.iter().filter_map(|e| match e {
            Event::OverallProgress { files_done, files_total, .. } => Some((*files_done, *files_total)),
            _ => None,
        }).next_back().expect("OverallProgress must fire");
        assert_eq!(last_overall, (2, 2), "progress reaches the plan total");

        for rel in ["a.txt", "b.txt"] {
            let started = events.iter().position(|e| {
                matches!(e, Event::FileStarted { path, .. } if path == Path::new(rel))
            }).expect("FileStarted");
            let done = events.iter().position(|e| {
                matches!(e, Event::FileDone { path, .. } if path == Path::new(rel))
            }).expect("FileDone");
            assert!(started < done, "{rel} must be bracketed");
        }
    }

    #[test]
    fn the_manifest_records_where_each_source_came_from() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");

        let (m, _) = run(&job(vec![src.clone()], dest));
        assert_eq!(m.sources.len(), 1);
        assert_eq!(m.sources[0].name, "proj");
        assert_eq!(m.sources[0].files, 1);
        // This is what a restore uses to offer "put it back where it came from" -- and,
        // unlike the old per-folder manifest, it is written for every job kind.
        assert_eq!(m.sources[0].original, guards::canonical_path(&src).unwrap());
    }

    #[test]
    fn events_report_a_plan_before_any_file_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bb");
        let dest = tmp.path().join("backup");

        let (_, events) = run(&job(vec![src], dest));

        let plan_idx = events
            .iter()
            .position(|e| matches!(e, Event::PlanReady { .. }))
            .expect("a plan must be emitted");
        let first_progress = events
            .iter()
            .position(|e| matches!(e, Event::OverallProgress { .. }))
            .expect("progress must be emitted");
        assert!(
            plan_idx < first_progress,
            "the plan must arrive before the first progress report, or the bar has no \
             denominator"
        );

        match &events[plan_idx] {
            Event::PlanReady { to_copy, bytes_to_copy, deletes, .. } => {
                assert_eq!(*to_copy, 2);
                assert_eq!(*bytes_to_copy, 5);
                // Known to be zero, not unknown.
                assert_eq!(*deletes, Some(0));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn unchanged_compares_mtimes_within_the_tolerance_it_is_given() {
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let e = Entry { rel: PathBuf::from("x"), size: 10, mtime: base };
        let fat = MTIME_TOLERANCE;
        let exact = std::time::Duration::ZERO;

        // With the FAT tolerance (only ever passed when a FAT volume is involved):
        assert!(unchanged(&e, 10, Some(base), fat));
        assert!(unchanged(&e, 10, Some(base + std::time::Duration::from_secs(2)), fat));
        assert!(unchanged(&e, 10, Some(base - std::time::Duration::from_secs(2)), fat));
        // Beyond the FAT resolution window, treat it as changed.
        assert!(!unchanged(&e, 10, Some(base + std::time::Duration::from_secs(3)), fat));

        // With an exact comparison (NTFS -> NTFS), even one second is a real change --
        // the old unconditional tolerance would have called this "unchanged" and
        // hardlinked the OLD bytes into the new snapshot.
        assert!(unchanged(&e, 10, Some(base), exact));
        assert!(!unchanged(&e, 10, Some(base + std::time::Duration::from_secs(1)), exact));

        // A size difference settles it regardless of timestamps.
        assert!(!unchanged(&e, 11, Some(base), fat));
        // An unreadable previous timestamp must never be read as "unchanged".
        assert!(!unchanged(&e, 10, None, fat));
    }

    /// F8, at the plan level: the same file one second newer is a Link under the FAT
    /// tolerance and a Copy under an exact comparison. And D1's link-time half: inside
    /// the FAT window the mtime match is merely "indistinguishable", so the two files are
    /// hash-compared -- same content links, different content copies.
    #[test]
    fn plan_links_or_copies_according_to_the_tolerance() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let prev = tmp.path().join("prev");
        touch(&src.join("f.txt"), "same size!");
        touch(&prev.join("f.txt"), "same size!");
        let prev_mtime = std::fs::metadata(prev.join("f.txt")).unwrap().modified().unwrap();

        let entry = Entry {
            rel: PathBuf::from("f.txt"),
            size: 10,
            mtime: prev_mtime + std::time::Duration::from_secs(1),
        };

        let fat = plan(vec![entry.clone()], &src, Some(&prev), MTIME_TOLERANCE);
        assert_eq!(
            fat.to_link, 1,
            "within the FAT window the mtimes are indistinguishable AND the hashes match"
        );

        // Same window, different content: the hash check must force a copy -- this is the
        // case where the pre-D1 code would have linked stale bytes over a real edit.
        touch(&src.join("f.txt"), "DIFFERENT!");
        let fat_changed = plan(vec![entry.clone()], &src, Some(&prev), MTIME_TOLERANCE);
        assert_eq!(fat_changed.to_copy, 1, "a hash mismatch inside the window forces Copy");
        assert_eq!(fat_changed.to_link, 0);

        // Zero tolerance (NTFS->NTFS): the one-second difference is a real edit, and no
        // hash is even consulted (the source content here would match anyway).
        touch(&src.join("f.txt"), "same size!");
        let exact = plan(vec![entry], &src, Some(&prev), std::time::Duration::ZERO);
        assert_eq!(exact.to_copy, 1, "on NTFS the one-second difference is a real edit");
        assert_eq!(exact.to_link, 0);
    }

    /// F8, end to end: on NTFS -> NTFS, a same-size edit made one second after the last
    /// run must be COPIED into the new snapshot -- not hardlinked away, which is what the
    /// old unconditional two-second tolerance did with it.
    #[cfg(windows)]
    #[test]
    fn a_same_size_edit_one_second_later_is_copied_not_linked_on_ntfs() {
        let tmp = tempfile::tempdir().unwrap();
        if crate::volume::probe_coarse_mtimes(tmp.path()) != Some(false) {
            eprintln!("skipping: temp volume is FAT-family or unprobeable");
            return;
        }
        let src = tmp.path().join("proj");
        let file = src.join("notes.txt");
        touch(&file, "ORIGINAL"); // 8 bytes
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let (first, _) = run(&j);
        let prev_copy = dest.join("snapshots").join(&first.id).join("proj").join("notes.txt");
        let prev_mtime = std::fs::metadata(&prev_copy).unwrap().modified().unwrap();

        // Same length, different content, mtime exactly one second after the snapshot
        // copy -- inside the old tolerance window, outside any honest one.
        std::fs::write(&file, "EDITED!!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(prev_mtime + std::time::Duration::from_secs(1))
            .unwrap();

        let (second, _) = run(&j);

        assert_eq!(second.files_linked, 0, "a one-second-old edit must never be linked");
        assert_eq!(second.files_copied, 1);
        let new_copy = dest.join("snapshots").join(&second.id).join("proj").join("notes.txt");
        assert_eq!(std::fs::read_to_string(&new_copy).unwrap(), "EDITED!!");
        assert_eq!(std::fs::read_to_string(&prev_copy).unwrap(), "ORIGINAL");
    }

    // ------------------------------------------- F6 / MT10: hardlink parent validation

    /// MT10, case one: two jobs sharing ONE destination, each with a source whose leaf is
    /// `proj`. The second job's first run must copy everything -- never hardlink bytes
    /// out of the first job's snapshot, whose `proj` is a different folder entirely.
    #[test]
    fn a_job_does_not_hardlink_from_another_jobs_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("alice/proj");
        touch(&src_a.join("f.txt"), "identical bytes, identical mtime");
        let src_b = tmp.path().join("bob/proj");
        // Byte- and timestamp-identical, so a leaf-name-only parent choice WOULD link --
        // the exact trap this test guards.
        std::fs::create_dir_all(&src_b).unwrap();
        std::fs::copy(src_a.join("f.txt"), src_b.join("f.txt")).unwrap();
        let dest = tmp.path().join("backup");

        let mut job_a = job(vec![src_a], dest.clone());
        job_a.name = "Alice".into();
        let mut job_b = job(vec![src_b.clone()], dest.clone());
        job_b.name = "Bob".into();

        run(&job_a);
        let (b_run, _) = run(&job_b);

        assert_eq!(b_run.files_linked, 0, "Bob's proj must not link against Alice's proj");
        assert_eq!(b_run.files_copied, 1);
        let b_snap = dest.join("snapshots").join(&b_run.id).join("proj").join("f.txt");
        assert_eq!(
            std::fs::read_to_string(&b_snap).unwrap(),
            "identical bytes, identical mtime",
            "the snapshot must hold BOB's copy, reached by copying"
        );

        // And Bob's SECOND run links against Bob's own first snapshot, as designed.
        let (b_second, _) = run(&job_b);
        assert_eq!(b_second.files_linked, 1, "same job, same source: linking resumes");
        assert_eq!(b_second.parent.as_deref(), Some(b_run.id.as_str()));
    }

    /// MT10, case two: a job edited to point at a DIFFERENT folder with the same leaf
    /// name. The stale parent's `proj` came from the old folder; nothing may be linked
    /// from it, even for byte-identical files.
    #[test]
    fn an_edited_source_with_the_same_leaf_does_not_link_from_the_stale_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let old_src = tmp.path().join("old/proj");
        touch(&old_src.join("f.txt"), "same bytes");
        let new_src = tmp.path().join("new/proj");
        // Identical content AND timestamp -- the shape that would compare "unchanged"
        // against the stale parent's copy if the guard let it look there.
        std::fs::create_dir_all(&new_src).unwrap();
        std::fs::copy(old_src.join("f.txt"), new_src.join("f.txt")).unwrap();
        let dest = tmp.path().join("backup");

        let j = job(vec![old_src], dest.clone());
        let (first, _) = run(&j);

        let j = job(vec![new_src], dest.clone());
        let (second, _) = run(&j);

        assert_eq!(
            second.parent.as_deref(),
            Some(first.id.as_str()),
            "the new run still chains to the latest snapshot"
        );
        assert_eq!(
            second.files_linked, 0,
            "but nothing may be LINKED from a parent that captured a different folder"
        );
        assert_eq!(second.files_copied, 1);
    }

    // ------------------------------------------------------- F1 / MT9: unreadable entries

    /// The deterministic stand-in for a permission error: a directory whose name ends in a
    /// dot can be created through the `\\?\` prefix but cannot be stat'ed through a normal
    /// path (Win32 path parsing strips the trailing dot). Returns the extended-form path
    /// for cleanup, or None if the filesystem refuses the trick.
    #[cfg(windows)]
    fn make_unreadable_subdir(parent: &Path, name: &str) -> Option<String> {
        let weird = format!(r"\\?\{}\{}.", parent.display(), name);
        std::fs::create_dir(&weird).ok().map(|_| weird)
    }

    /// MT9: something inside the source that cannot be read must turn the run Partial and
    /// be reported by path -- never dropped into a silent hole in a "Completed" snapshot.
    #[cfg(windows)]
    #[test]
    fn an_unreadable_entry_inside_a_source_forces_partial_and_names_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "keep");
        let Some(weird) = make_unreadable_subdir(&src, "locked") else {
            eprintln!("skipping: cannot create a trailing-dot directory here");
            return;
        };
        let dest = tmp.path().join("backup");

        let (m, events) = run(&job(vec![src], dest.clone()));

        assert!(
            matches!(m.outcome, RunOutcome::Partial { .. }),
            "an unreadable source entry must not report Ok: {:?}",
            m.outcome
        );
        assert_eq!(m.files_failed, 1, "the unreadable entry is counted as a failure");
        let named = events
            .iter()
            .filter(|e| matches!(e, Event::SourceUnreadable { .. }))
            .count();
        assert_eq!(named, 1, "the hole must be reported by path, not just counted");
        // The readable file is still safely in the snapshot.
        let snap = dest.join("snapshots").join(&m.id).join("proj");
        assert_eq!(std::fs::read_to_string(snap.join("keep.txt")).unwrap(), "keep");

        let _ = std::fs::remove_dir(&weird);
    }

    /// Deny read-data/list-directory on `path` for the current user via icacls, restoring
    /// inherited ACLs on drop. A deny ACE blocks even the owner (and even an elevated
    /// admin -- std::fs never enables SeBackupPrivilege), while leaving attribute reads
    /// intact, so `is_dir` still passes but `read_dir` fails: exactly the "root exists but
    /// cannot be listed" shape.
    #[cfg(windows)]
    struct AclDenyRead(PathBuf);

    #[cfg(windows)]
    impl AclDenyRead {
        fn new(path: &Path) -> Option<Self> {
            let user = std::env::var("USERNAME").ok()?;
            let ok = std::process::Command::new("icacls")
                .arg(path)
                .arg("/deny")
                .arg(format!("{user}:(RD)"))
                .output()
                .ok()?
                .status
                .success();
            ok.then(|| Self(path.to_path_buf()))
        }
    }

    #[cfg(windows)]
    impl Drop for AclDenyRead {
        fn drop(&mut self) {
            let _ = std::process::Command::new("icacls").arg(&self.0).arg("/reset").output();
        }
    }

    /// F1: a source ROOT that cannot be listed must fail the run outright. The empty
    /// "Completed" snapshot the old code wrote here was doubly poisonous: it read as
    /// success, and it became the link parent every later snapshot trusted.
    #[cfg(windows)]
    #[test]
    fn an_unreadable_source_root_fails_the_run_instead_of_writing_an_empty_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "a");
        let dest = tmp.path().join("backup");

        let Some(_deny) = AclDenyRead::new(&src) else {
            eprintln!("skipping: icacls unavailable on this machine");
            return;
        };

        let err = run_job(&job(vec![src], dest.clone()), None, &AtomicBool::new(false), &mut |_| {})
            .expect_err("a source that cannot be listed at all must fail the run");
        assert!(
            matches!(err, Error::UnreadableSourceRoot(_)),
            "expected UnreadableSourceRoot, got: {err}"
        );
        assert!(
            Repo::open(&dest).unwrap().snapshot_ids().is_empty(),
            "no snapshot may survive a run that never read its source"
        );
    }

    // ------------------------------------------------------------- F5: missing sources

    /// F5: a configured source that no longer exists must be reported -- warning event,
    /// run stats, manifest -- and must keep the run from claiming a clean Ok.
    #[test]
    fn a_missing_source_is_reported_and_makes_the_run_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        touch(&real.join("f.txt"), "x");
        let missing = tmp.path().join("vanished");
        let dest = tmp.path().join("backup");

        let (m, events) = run(&job(vec![real, missing.clone()], dest.clone()));

        assert_eq!(m.files_copied, 1, "the real source is still backed up");
        assert_eq!(m.sources.len(), 1);
        assert!(
            matches!(m.outcome, RunOutcome::Partial { .. }),
            "a skipped source must not report Ok: {:?}",
            m.outcome
        );
        assert_eq!(
            m.skipped_sources,
            vec![missing.to_string_lossy().to_string()],
            "the manifest must record which configured source was skipped"
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::SourceSkipped { .. })),
            "a SourceSkipped warning event must fire"
        );
        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .expect("RunDone must be emitted");
        assert_eq!(done.sources_skipped, 1);
    }

    // ------------------------------------------------- F33: guard/write path canonicality

    /// F33: a source or destination spelled through an environment variable must be
    /// resolved BEFORE the existence check and the repo open -- not silently skipped, and
    /// never created as a literal `%VAR%` folder.
    #[test]
    fn env_var_spellings_are_resolved_for_both_source_and_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        let dest = tmp.path().join("backup");
        // Unique variable names: tests in this binary run in parallel threads and the
        // process environment is shared.
        std::env::set_var("BACKSTAR_A1A_SRC", &src);
        std::env::set_var("BACKSTAR_A1A_DEST", &dest);

        let j = job(
            vec![PathBuf::from(r"%BACKSTAR_A1A_SRC%")],
            PathBuf::from(r"%BACKSTAR_A1A_DEST%"),
        );
        let result = run(&j);

        std::env::remove_var("BACKSTAR_A1A_SRC");
        std::env::remove_var("BACKSTAR_A1A_DEST");

        let (m, _) = result;
        assert_eq!(m.files_copied, 1, "the env-var source must resolve to the real folder");
        assert!(
            dest.join("snapshots").join(&m.id).join("proj").join("f.txt").is_file(),
            "the snapshot must land under the expanded destination"
        );
        assert!(
            !PathBuf::from(r"%BACKSTAR_A1A_DEST%").exists(),
            "a literal %VAR% folder must never be created"
        );
    }

    // ------------------------------------------------------ F32 / MT5: exclusion casing

    /// MT5: exclusion file globs are case-insensitive end-to-end through a real backup
    /// run -- robocopy's `/XF` always was, and `THUMBS.DB` must be excluded exactly like
    /// `Thumbs.db`.
    #[test]
    fn case_variant_system_files_are_excluded_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("keep.txt"), "k");
        touch(&src.join("THUMBS.DB"), "junk");
        touch(&src.join("FILE.TMP"), "junk");
        let dest = tmp.path().join("backup");

        let mut j = job(vec![src], dest.clone());
        j.excludes = ExcludeRules::system();
        let (m, _) = run(&j);

        assert_eq!(m.files_copied, 1, "only the real content is backed up");
        let snap = dest.join("snapshots").join(&m.id).join("proj");
        assert!(snap.join("keep.txt").is_file());
        assert!(!snap.join("THUMBS.DB").exists(), "uppercase Thumbs.db must be excluded");
        assert!(!snap.join("FILE.TMP").exists(), "uppercase *.tmp must be excluded");
    }

    // ---------------------------------------------------- F31: reparse points in stats

    /// F31 / D7: reparse points are never followed -- but they must be COUNTED where the
    /// UI can see them, because a skipped junction is something in the tree that was not
    /// backed up.
    #[cfg(windows)]
    #[test]
    fn a_skipped_junction_is_counted_in_the_run_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("real.txt"), "real");
        let link = src.join("loop");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&src)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }
        let dest = tmp.path().join("backup");

        let (m, events) = run(&job(vec![src], dest));

        assert_eq!(m.outcome, RunOutcome::Ok, "a skipped junction is not a failure");
        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { stats, .. } => Some(stats.clone()),
                _ => None,
            })
            .expect("RunDone must be emitted");
        assert_eq!(done.reparse_skipped, 1, "the junction must reach the run stats");
    }

    // ------------------------------------------------------- D2: git gc + retention

    /// D2: with `git_gc` on, `git gc --auto` runs for sources that are git repositories
    /// (a `.git` directory OR a worktree's `.git` FILE) and only those.
    #[test]
    fn git_gc_runs_for_git_sources_only() {
        let tmp = tempfile::tempdir().unwrap();
        let git_src = tmp.path().join("repo-src");
        touch(&git_src.join("f.txt"), "x");
        std::fs::create_dir_all(git_src.join(".git")).unwrap();
        let worktree_src = tmp.path().join("worktree-src");
        touch(&worktree_src.join("f.txt"), "x");
        std::fs::write(worktree_src.join(".git"), "gitdir: elsewhere").unwrap();
        let plain_src = tmp.path().join("plain");
        touch(&plain_src.join("f.txt"), "x");

        let mut j = job(vec![git_src.clone(), worktree_src.clone(), plain_src], tmp.path().join("backup"));
        j.git_gc = true;

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut hooks = RunHooks::production();
        {
            let calls = calls.clone();
            hooks.git_gc = Box::new(move |p: &Path| {
                calls.lock().unwrap().push(p.to_path_buf());
                Ok(())
            });
        }

        let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(m.outcome, RunOutcome::Ok);

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "git sources only: {calls:?}");
        assert!(calls.contains(&git_src));
        assert!(calls.contains(&worktree_src), "a worktree's .git FILE counts");
    }

    /// Ordering, proven on a single-source run (a shared log is unambiguous only then):
    /// gc must complete before the walk of ITS source starts -- packing while copying
    /// would move pack files mid-scan.
    #[test]
    fn git_gc_runs_before_the_walk_of_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("repo-src");
        touch(&src.join("f.txt"), "x");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        let mut j = job(vec![src], tmp.path().join("backup"));
        j.git_gc = true;

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut hooks = RunHooks::production();
        {
            let log = log.clone();
            hooks.git_gc = Box::new(move |_: &Path| {
                log.lock().unwrap().push("gc");
                Ok(())
            });
        }
        {
            let log = log.clone();
            run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| {
                if matches!(e, Event::ScanProgress { .. } | Event::OverallProgress { .. }) {
                    log.lock().unwrap().push("scan-or-copy");
                }
            })
            .unwrap();
        }

        let log = log.lock().unwrap();
        assert_eq!(log.first(), Some(&"gc"), "gc must come first: {log:?}");
    }

    /// The knob off means no calls at all; a gc failure warns and never fails the run.
    #[test]
    fn git_gc_honors_the_flag_and_warns_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        std::fs::create_dir_all(src.join(".git")).unwrap();

        // Flag off: no call.
        let mut j = job(vec![src.clone()], tmp.path().join("backup"));
        let called = std::sync::Arc::new(AtomicBool::new(false));
        let mut hooks = RunHooks::production();
        {
            let called = called.clone();
            hooks.git_gc = Box::new(move |_: &Path| {
                called.store(true, Ordering::Relaxed);
                Ok(())
            });
        }
        run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(!called.load(Ordering::Relaxed));

        // Flag on, gc fails: warning, not failure.
        j.git_gc = true;
        hooks.git_gc = Box::new(|_: &Path| Err(Error::other("git is not installed")));
        let mut events = Vec::new();
        let m = run_job_with(&j, None, &hooks, &AtomicBool::new(false), &mut |e| events.push(e))
            .unwrap();
        assert_eq!(m.outcome, RunOutcome::Ok, "gc failure must not fail the run");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::RunWarning { message } if message.contains("git gc")
        )));
    }

    /// Smoke test against the real git binary (the unit tests use a spy). Ignored by
    /// default: requires git on PATH.
    #[test]
    #[ignore = "requires real git on PATH; run explicitly"]
    fn git_gc_smoke_test_with_real_git() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("repo");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        touch(&src.join("f.txt"), "x");
        let mut j = job(vec![src], tmp.path().join("backup"));
        j.git_gc = true;
        run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
    }

    /// D2 retention: keep-newest-N per job, wired end to end. Four runs keep the newest
    /// two (dirs AND manifests), the newest stays restorable, and another job's snapshot
    /// at the same destination is never a candidate.
    #[test]
    fn retention_prunes_this_jobs_oldest_snapshots_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(run_job(&j, Some(2), &AtomicBool::new(false), &mut |_| {}).unwrap().id);
        }

        let repo = Repo::open(&dest).unwrap();
        assert_eq!(
            repo.snapshot_ids(),
            vec![ids[2].clone(), ids[3].clone()],
            "the newest two survive; the oldest two are gone, dirs and manifests"
        );
        assert!(repo.read_manifest(&ids[0]).is_err(), "pruned manifest is gone");
        assert!(repo.read_manifest(&ids[3]).is_ok(), "the newest manifest survives");

        // The newest survivor still restores.
        let restore_to = tmp.path().join("restored");
        crate::restore::restore(
            &repo,
            &ids[3],
            Path::new("proj/f.txt"),
            &restore_to.join("f.txt"),
            crate::restore::OverwritePolicy::Always,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(restore_to.join("f.txt")).unwrap(), "x");

        // Another job at the same destination keeps its own history.
        let mut other = job(vec![tmp.path().join("other-src")], dest.clone());
        other.name = "Other".into();
        touch(&tmp.path().join("other-src/g.txt"), "g");
        let other_run = run_job(&other, Some(2), &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(
            repo.snapshot_ids().contains(&other_run.id),
            "the other job's snapshot must survive this job's pruning"
        );

        // The stats wire carries the count.
        let mut events = Vec::new();
        run_job(&j, Some(2), &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
        let done = events
            .iter()
            .find_map(|e| match e {
                Event::RunDone { stats, .. } => Some(stats.snapshots_pruned),
                _ => None,
            })
            .unwrap();
        assert_eq!(done, 1, "one more snapshot crossed the keep window");
    }

    /// Pruning never runs on a Failed run: seed more history than the keep window allows
    /// with keep=None, then fail a run with keep=Some(1) -- the excess must survive.
    #[test]
    fn a_failed_run_never_prunes_history() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        let a = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap().id;
        let b = run_job(&j, None, &AtomicBool::new(false), &mut |_| {}).unwrap().id;

        let mut hooks = RunHooks::production();
        hooks.write_manifest = |_, _| Err(Error::other("injected"));
        let err = run_job_with(&j, Some(1), &hooks, &AtomicBool::new(false), &mut |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("manifest"), "{err}");

        let repo = Repo::open(&dest).unwrap();
        assert!(
            repo.read_manifest(&a).is_ok() && repo.read_manifest(&b).is_ok(),
            "a failed run must not thin history"
        );
        // (The failed run's own manifest-less dir is F35's business, not retention's.)
    }

    /// Nor does a cancelled run prune: history is exactly as it was.
    #[test]
    fn a_cancelled_run_never_prunes_history() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("f.txt"), "x");
        let dest = tmp.path().join("backup");
        let j = job(vec![src], dest.clone());

        run_job(&j, Some(1), &AtomicBool::new(false), &mut |_| {}).unwrap();
        let before = Repo::open(&dest).unwrap().snapshot_ids();

        let cancel = AtomicBool::new(true); // cancelled before anything starts
        let m = run_job(&j, Some(1), &cancel, &mut |_| {}).unwrap();
        assert_eq!(m.outcome, RunOutcome::Cancelled);
        assert_eq!(Repo::open(&dest).unwrap().snapshot_ids(), before);
    }
}
