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
//! [`crate::copy::copy_file`] never writes through an existing link.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use time::OffsetDateTime;

use crate::config::Job;
use crate::events::{Event, JobStats, RunOutcome};
use crate::guards;
use crate::parallel;
use crate::repo::{Repo, SnapshotManifest, SnapshotSource};
use crate::walk::{self, Entry};
use crate::{Error, Result};

/// Timestamp tolerance when deciding whether a file changed.
///
/// FAT-family filesystems store modification times at two-second resolution, so the same
/// file copied to a USB stick comes back with a timestamp up to two seconds off. robocopy
/// exposes this as `/FFT`, and the PowerShell app passed it on every run precisely so that
/// exFAT targets did not recopy the entire tree each time.
pub(crate) const MTIME_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(2);

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
pub fn plan(entries: Vec<Entry>, previous: Option<&Path>) -> Plan {
    let mut p = Plan::default();
    for entry in entries {
        let action = match previous {
            None => Action::Copy,
            Some(prev_root) => {
                let candidate = prev_root.join(&entry.rel);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(md) if md.is_file() && unchanged(&entry, md.len(), md.modified().ok()) => {
                        Action::Link
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

pub(crate) fn unchanged(entry: &Entry, prev_size: u64, prev_mtime: Option<SystemTime>) -> bool {
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
        Ok(d) => d <= MTIME_TOLERANCE,
        Err(_) => false,
    }
}

/// The subfolder name a source occupies inside a snapshot.
fn source_name(src: &Path) -> String {
    src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into())
}

/// Validate a job before touching anything.
///
/// Returns the resolved (name, path) pairs in job order: `job.sources` first, then
/// `job.presets` expanded against the machine's current known-folder locations.
///
/// Three of these checks exist because the PowerShell app shipped without them and the
/// consequences were silent. Two sources with the same leaf name both target the same
/// destination subfolder, quietly merging two unrelated projects into one folder. A
/// destination inside its own source recurses. And a custom source folder that happens to
/// share a name with an enabled preset would silently merge into it too.
pub fn resolve_sources(job: &Job) -> Result<Vec<(String, PathBuf)>> {
    resolve_sources_against(job, &crate::presets::all())
}

/// The actual implementation, taking the preset list as a parameter so it is testable
/// without touching real Windows known-folder APIs -- `presets::all()` reads this
/// machine's actual Desktop/Documents/Chrome-profile locations, which a unit test has no
/// business depending on.
fn resolve_sources_against(
    job: &Job,
    available_presets: &[crate::presets::Preset],
) -> Result<Vec<(String, PathBuf)>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let dest = job.resolve_dest();

    for src in &job.sources {
        if !src.is_dir() {
            continue;
        }
        let canon = guards::canonical_path(src)?;
        let name = source_name(&canon);

        if !seen.insert(name.to_lowercase()) {
            return Err(Error::other(format!(
                "two source folders are both named {name:?}, so they would back up into the \
                 same folder. Rename or remove one."
            )));
        }

        guards::ensure_dest_safe(&dest, &canon)?;
        out.push((name, canon));
    }

    // System presets are named by their stable key, never by the resolved path's own leaf
    // component: the Chrome preset resolves to ".../Google/Chrome/User Data", and the
    // destination subfolder needs to stay "Chrome" on every machine and every Chrome
    // version, not follow whatever that leaf folder happens to be called.
    for key in &job.presets {
        let Some(preset) = available_presets.iter().find(|p| &p.key == key) else {
            // A stale key from an older config (a preset that no longer exists) or a
            // config edited by hand. Nothing to resolve it to; skip rather than fail the
            // whole run over one dangling reference.
            continue;
        };
        if !preset.found {
            continue;
        }
        let Some(path) = &preset.path else { continue };

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
        out.push((name, canon));
    }

    if out.is_empty() {
        return Err(Error::other(
            "no valid source folders to back up -- check that the configured folders and \
             presets still exist on this machine",
        ));
    }
    Ok(out)
}

/// Run one job to completion, emitting progress as it goes.
pub fn run_job(
    job: &Job,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<SnapshotManifest> {
    let started_at = OffsetDateTime::now_utc();
    let clock = Instant::now();

    let sources = resolve_sources(job)?;
    // Preferring the volume-GUID location over the literal `dest` here is what makes a
    // portable destination actually portable: this is the one call that decides where the
    // repository -- and therefore every snapshot -- physically lives.
    let repo = Repo::open_or_init(job.resolve_dest(), &job.name)?;
    let parent = repo.latest_snapshot();
    // Reserve the id up front. Two runs started in the same second must not share a
    // directory -- see Repo::allocate_snapshot_id.
    let snapshot_id = repo.allocate_snapshot_id(started_at)?;
    let snapshot_dir = repo.snapshot_dir(&snapshot_id);
    std::fs::create_dir_all(&snapshot_dir).map_err(|e| Error::io(&snapshot_dir, e))?;

    emit(Event::RunStarted { run_id: snapshot_id.clone(), jobs: sources.len() });

    let mut stats = JobStats::default();
    let mut manifest_sources = Vec::new();
    let mut cancelled = false;

    for (name, src) in &sources {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        let dest_root = snapshot_dir.join(name);
        emit(Event::JobStarted {
            job: name.clone(),
            source: src.clone(),
            dest: dest_root.clone(),
        });

        let excludes = job.excludes.compile(src, &dest_root);
        let walked = walk::walk(src, &excludes, |files, bytes| {
            emit(Event::ScanProgress { files_seen: files, bytes_seen: bytes });
        })?;

        let prev_root = parent.as_ref().map(|p| repo.snapshot_dir(p).join(name));
        let prev_root = prev_root.filter(|p| p.is_dir());
        let planned = plan(walked.entries, prev_root.as_deref());

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
            cancel,
            emit,
        );

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

        emit(Event::JobDone { job: name.clone(), stats: stats.clone() });
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
    } else {
        RunOutcome::Ok
    };

    let manifest = SnapshotManifest {
        id: snapshot_id.clone(),
        job: job.name.clone(),
        started: started_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        finished: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        sources: manifest_sources,
        files_copied: stats.files_copied,
        files_linked: stats.files_linked,
        files_failed: stats.files_failed,
        bytes_copied: stats.bytes_copied,
        bytes_linked: stats.bytes_linked,
        duration_ms: stats.duration_ms,
        outcome: outcome.clone(),
        parent: parent.clone(),
    };

    // A cancelled run leaves a partial snapshot directory. Remove it: a half-captured
    // snapshot that looks like a complete one is worse than no snapshot, because a later
    // run would link against it and propagate the gap forward.
    if cancelled {
        let _ = repo.delete_snapshot(&snapshot_id);
    } else {
        repo.write_manifest(&manifest)?;
        repo.write_readme()?;
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
/// The actual threading, progress throttling and link-then-fallback logic lives in
/// [`crate::parallel`], shared with restore.
fn execute(
    planned: &Plan,
    src: &Path,
    dest_root: &Path,
    prev_root: Option<&Path>,
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

    parallel::run_parallel(&items, cancel, emit)
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
        let m = run_job(j, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
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
        let err = run_job(&j, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
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
        assert!(run_job(&j, &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn a_job_with_no_existing_sources_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let j = job(vec![tmp.path().join("nope")], tmp.path().join("backup"));
        let err = run_job(&j, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
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

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "Chrome", "must be named by the preset key");
        assert_eq!(resolved[0].1, guards::canonical_path(&chrome_profile).unwrap());
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
        assert_eq!(resolved.len(), 1, "the real source must still back up fine");
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
        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
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

        let manifest = run_job(&j, &AtomicBool::new(false), &mut |_| {}).unwrap();
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
        let m = run_job(&j, &cancel, &mut |e| {
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
    fn unchanged_tolerates_a_two_second_timestamp_difference() {
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let e = Entry { rel: PathBuf::from("x"), size: 10, mtime: base };

        assert!(unchanged(&e, 10, Some(base)));
        assert!(unchanged(&e, 10, Some(base + std::time::Duration::from_secs(2))));
        assert!(unchanged(&e, 10, Some(base - std::time::Duration::from_secs(2))));
        // Beyond the FAT resolution window, treat it as changed.
        assert!(!unchanged(&e, 10, Some(base + std::time::Duration::from_secs(3))));
        // A size difference settles it regardless of timestamps.
        assert!(!unchanged(&e, 11, Some(base)));
        // An unreadable previous timestamp must never be read as "unchanged".
        assert!(!unchanged(&e, 10, None));
    }
}
