//! Tauri commands for browsing snapshots and restoring from them.
//!
//! Named `restore_cmd` rather than `restore` to avoid shadowing `backstar_core::restore`,
//! which does the actual work; this module is purely the IPC-facing translation layer --
//! computing the one thing `backstar_core::restore::restore` deliberately leaves to its
//! caller (the exact destination path), validating everything that arrives over IPC (F21),
//! and handing the rest straight through.

use std::path::{Component, Path, PathBuf};

use backstar_core::config::Job;
use backstar_core::events::Event;
use backstar_core::repo::{Repo, SnapshotEntry, SnapshotListing, SnapshotManifest};
use backstar_core::restore::OverwritePolicy;
use serde::Deserialize;

use crate::runner::{find_job, RunHandle, Runner};
use crate::CmdResult;

/// Load the job and (maybe) its repository, distinguishing "no backups yet" from real
/// damage (F19): `Ok(None)` is a freshly configured job whose destination simply has no
/// repository; an unreadable or damaged repository propagates as an error instead of
/// flattening into the same empty list.
fn load_job_and_repo(job_id: &str) -> CmdResult<(Job, Option<Repo>)> {
    let cfg = crate::load_config()?;
    let job = find_job(&cfg, job_id).ok_or_else(|| format!("no job with id {job_id:?}"))?;
    match Repo::open(job.resolve_dest()) {
        Ok(r) => Ok((job, Some(r))),
        Err(backstar_core::Error::NotARepo(_)) => Ok((job, None)),
        Err(e) => Err(e.to_string()),
    }
}

/// Same, for commands that need an existing repository.
fn require_repo(job_id: &str) -> CmdResult<(Job, Repo)> {
    match load_job_and_repo(job_id)? {
        (job, Some(repo)) => Ok((job, repo)),
        (job, None) => Err(format!("{} has no backups yet", job.name)),
    }
}

/// Validate a snapshot id arriving over IPC (F21): exactly the shape
/// `allocate_snapshot_id` produces -- `2026-09-15T14-03-22Z` with an optional `-002`
/// suffix. Anything else (path separators, dots, drive letters) is rejected before it
/// can be joined onto a filesystem path.
fn validate_snapshot_id(id: &str) -> CmdResult<()> {
    let ok = match id.len() {
        20 => true,
        // "<id>-002": dash plus exactly three digits.
        24 => {
            let sfx = &id[20..];
            sfx.starts_with('-') && sfx[1..].bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    };
    let shaped = ok && {
        let stem = &id.as_bytes()[..20];
        stem[4] == b'-' && stem[7] == b'-' && stem[10] == b'T' && stem[13] == b'-'
            && stem[16] == b'-' && stem[19] == b'Z'
            && stem
                .iter()
                .enumerate()
                .filter(|(i, _)| ![4, 7, 10, 13, 16, 19].contains(i))
                .all(|(_, c)| c.is_ascii_digit())
    };
    if shaped {
        Ok(())
    } else {
        Err(format!("invalid snapshot id {id:?}"))
    }
}

/// Validate a path inside a snapshot arriving over IPC (F21): only plain relative
/// components -- no absolutes, no `..`, no prefixes, no roots, no `.`. The empty path is
/// valid: it names the snapshot's own root.
fn validate_snapshot_rel(rel: &str) -> CmdResult<PathBuf> {
    let p = PathBuf::from(rel);
    let ok = p.components().all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(p)
    } else {
        Err(format!("invalid path inside a snapshot: {rel:?}"))
    }
}

/// List the snapshots for a job, with the damage visible (F19): `manifests` newest first,
/// plus any snapshots whose manifests are unreadable (tree intact, bookkeeping damaged)
/// and any incomplete ones (an interrupted run's leftovers, pruned by the next run).
/// "No repository yet" is an empty listing, never an error.
///
/// Deliberately NOT filtered by job name: until history/manifests key on job id (F40),
/// hiding a renamed job's older snapshots would look like data loss.
#[tauri::command(async)]
pub fn list_snapshots(job_id: String) -> CmdResult<SnapshotListing> {
    let Some(repo) = load_job_and_repo(&job_id)?.1 else {
        return Ok(SnapshotListing::default());
    };
    let mut listing = repo.list_snapshots_full();
    // Ids sort chronologically by construction; the UI wants newest first.
    listing.manifests.reverse();
    listing.unreadable.reverse();
    listing.incomplete.reverse();
    Ok(listing)
}

/// List the children of a path inside a snapshot.
///
/// Thin IPC wrapper: the actual listing logic lives in
/// [`backstar_core::repo::Repo::list_snapshot_children`], shared with the standalone
/// `BackStar-Restore` tool so both present identical listings.
#[tauri::command(async)]
pub fn browse_snapshot(
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
) -> CmdResult<Vec<SnapshotEntry>> {
    validate_snapshot_id(&snapshot_id)?;
    let rel = validate_snapshot_rel(&snapshot_rel)?;
    let (_job, repo) = require_repo(&job_id)?;
    repo.list_snapshot_children(&snapshot_id, &rel)
        .map_err(|e| e.to_string())
}

/// Where an item would go if restored to its original location, or `None` if the manifest
/// has no record of it (the source's name changed, or -- unlike the PowerShell app -- this
/// should now be rare, since every snapshot carries this for every source it contains).
#[tauri::command(async)]
pub fn resolve_restore_original(
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
) -> CmdResult<Option<String>> {
    validate_snapshot_id(&snapshot_id)?;
    let rel = validate_snapshot_rel(&snapshot_rel)?;
    let (_job, repo) = require_repo(&job_id)?;
    let manifest = repo.read_manifest(&snapshot_id).map_err(|e| e.to_string())?;
    Ok(backstar_core::restore::resolve_original_path(&manifest, &rel)
        .map(|p| p.display().to_string()))
}

/// Where to put a restored item, as chosen by the user.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", tag = "mode")]
pub enum RestoreTarget {
    /// Wherever the manifest says this item came from.
    Original,
    /// Into this folder -- as a same-named subfolder for a directory, or directly for a
    /// file. Both cases are the same join, since "put a copy of X in folder F" means
    /// `F.join(X's own name)` either way.
    Custom { folder: String },
}

fn compute_destination(
    manifest: &SnapshotManifest,
    snapshot_rel: &Path,
    target: &RestoreTarget,
) -> CmdResult<PathBuf> {
    match target {
        RestoreTarget::Original => {
            backstar_core::restore::resolve_original_path(manifest, snapshot_rel).ok_or_else(|| {
                "no record of where this item came from on this machine -- choose a folder \
                 to restore it to instead"
                    .to_string()
            })
        }
        RestoreTarget::Custom { folder } => {
            // F23: the destination rule lives in backstar-core, shared with the standalone
            // BackStar-Restore tool, so app and CLI can never drift apart on it.
            backstar_core::restore::compute_destination(Path::new(folder), snapshot_rel)
                .map_err(|e| e.to_string())
        }
    }
}

/// What the user allows a restore to do about existing destination files (D5/F22).
/// Absent means `Fail` -- overwriting is never the silent default.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverwriteArg {
    /// Refuse the whole restore if anything would be overwritten.
    #[default]
    Fail,
    /// Overwrite only files the destination holds an older copy of.
    IfOlder,
    /// Overwrite unconditionally (the user was shown the overwrite counts and confirmed).
    Always,
}

impl From<OverwriteArg> for OverwritePolicy {
    fn from(arg: OverwriteArg) -> Self {
        match arg {
            OverwriteArg::Fail => OverwritePolicy::Fail,
            OverwriteArg::IfOlder => OverwritePolicy::IfOlder,
            OverwriteArg::Always => OverwritePolicy::Always,
        }
    }
}

/// D5 pre-flight for the restore UI: how many existing files a restore would overwrite,
/// and how many of those are NEWER than the snapshot (someone's unsaved work). The UI
/// shows this before offering to confirm; nothing is written.
#[tauri::command(async)]
pub fn plan_restore_overwrites(
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
    target: RestoreTarget,
) -> CmdResult<backstar_core::restore::RestorePlan> {
    validate_snapshot_id(&snapshot_id)?;
    let rel = validate_snapshot_rel(&snapshot_rel)?;
    let (_job, repo) = require_repo(&job_id)?;
    let manifest = repo.read_manifest(&snapshot_id).map_err(|e| e.to_string())?;
    let dest = compute_destination(&manifest, &rel, &target)?;
    backstar_core::restore::plan_restore(&repo, &snapshot_id, &rel, &dest)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start_restore(
    app: tauri::AppHandle,
    runner: tauri::State<'_, Runner>,
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
    target: RestoreTarget,
    overwrite: Option<OverwriteArg>,
) -> CmdResult<RunHandle> {
    use tauri::Emitter;

    validate_snapshot_id(&snapshot_id)?;
    let rel = validate_snapshot_rel(&snapshot_rel)?;
    let (_job, repo) = require_repo(&job_id)?;
    let manifest = repo.read_manifest(&snapshot_id).map_err(|e| e.to_string())?;
    let policy = OverwritePolicy::from(overwrite.unwrap_or_default());

    let item_path = repo.snapshot_dir(&snapshot_id).join(&rel);
    if !item_path.exists() {
        return Err(format!("nothing at {snapshot_rel:?} in snapshot {snapshot_id:?}"));
    }

    let dest = compute_destination(&manifest, &rel, &target)?;

    // Guard here too, not only inside `backstar_core::restore::restore`: a clear error
    // before a thread even spawns beats one that only shows up in the activity feed after
    // the button has already flipped to "running".
    backstar_core::guards::ensure_dest_safe(&dest, &repo.root).map_err(|e| e.to_string())?;

    let label = format!("Restore: {}", rel.display());
    let sink_app = app.clone();
    let done_app = app.clone();

    runner.start(
        label,
        move |cancel, emit| {
            // F3: hold the repository's run lock for the restore's duration. Restore is
            // read-only on the repo, but it must not race a run's retention prune deleting
            // this snapshot mid-read. Contention surfaces as an honest Failed RunDone.
            let _run_lock = match repo.acquire_run_lock() {
                Ok(lock) => lock,
                Err(e) => {
                    emit(Event::RunDone {
                        run_id: format!("restore:{snapshot_id}"),
                        outcome: backstar_core::events::RunOutcome::Failed {
                            reason: e.to_string(),
                        },
                        stats: Default::default(),
                    });
                    return;
                }
            };
            if let Err(e) = backstar_core::restore::restore(
                &repo,
                &snapshot_id,
                &rel,
                &dest,
                policy,
                cancel,
                emit,
            ) {
                emit(Event::RunDone {
                    run_id: format!("restore:{snapshot_id}"),
                    outcome: backstar_core::events::RunOutcome::Failed { reason: e.to_string() },
                    stats: Default::default(),
                });
            }
        },
        move |ev| {
            let _ = sink_app.emit(crate::runner::EVENT_CHANNEL, &ev);
        },
        move || {
            let _ = done_app.emit("backstar://run-finished", ());
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MT4 (shell half): snapshot ids from IPC must match the shape the engine produces --
    /// anything else is rejected before it touches the filesystem.
    #[test]
    fn snapshot_ids_are_validated_by_shape() {
        assert!(validate_snapshot_id("2026-09-15T14-03-22Z").is_ok());
        assert!(validate_snapshot_id("2026-09-15T14-03-22Z-002").is_ok());

        for bad in [
            "",
            "2026-09-15",
            "2026-09-15T14-03-22Z-2",     // suffix must be exactly three digits
            "2026-09-15T14-03-22Z-0022",
            "2026-09-15T14-03-22Z-abc",
            "..",
            "../..",
            "2026/09/15",
            r"C:\Windows",
            "2026-09-15T14-03-22",        // missing the Z
            "2026-09-15T14-03-22Z/extra",
            "2026-09-15T14-03-22Z-extra",
        ] {
            assert!(validate_snapshot_id(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// MT4 (shell half): snapshot-relative paths must be plain relative components only.
    /// (`Path::components` normalises `.` away, so `./proj` and `proj/.` are provably the
    /// same as `proj` -- the dangerous shapes are parents, roots, and prefixes.)
    #[test]
    fn snapshot_rel_rejects_traversal_and_absolute_paths() {
        assert_eq!(validate_snapshot_rel("").unwrap(), PathBuf::from(""));
        assert_eq!(validate_snapshot_rel("proj/sub/file.txt").unwrap(), PathBuf::from("proj/sub/file.txt"));
        assert_eq!(validate_snapshot_rel(r"proj\sub").unwrap(), PathBuf::from(r"proj\sub"));
        // CurDir is normalised out by `components` when interior or trailing, so these
        // are exactly `proj` -- and a LEADING `./` is preserved, hence rejected (the UI
        // never produces it, and strictness here is the point).
        assert_eq!(validate_snapshot_rel("proj/.").unwrap(), PathBuf::from("proj"));
        assert_eq!(validate_snapshot_rel("proj/./sub").unwrap(), PathBuf::from("proj/sub"));

        for bad in [
            "..",
            "../escape",
            "proj/../escape",
            r"..\escape",
            "/absolute",
            r"\absolute",
            r"C:\abs",
            r"\\server\share",
            "./proj",
        ] {
            assert!(validate_snapshot_rel(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn overwrite_arg_defaults_to_the_safe_policy() {
        assert_eq!(OverwriteArg::default(), OverwriteArg::Fail);
        assert!(matches!(
            OverwritePolicy::from(OverwriteArg::IfOlder),
            OverwritePolicy::IfOlder
        ));
        assert!(matches!(
            OverwritePolicy::from(OverwriteArg::Always),
            OverwritePolicy::Always
        ));
        // The wire spelling, for the UI calling start_restore.
        let parsed: OverwriteArg = serde_json::from_str(r#""ifOlder""#).unwrap();
        assert_eq!(parsed, OverwriteArg::IfOlder);
    }

    /// F23: the app's custom-folder destination IS the core helper both tools share, so
    /// the CLI's `restore <id> <path> <folder>` and the app's "restore to folder" can
    /// never disagree about where the item lands.
    #[test]
    fn custom_destinations_come_from_the_shared_core_helper() {
        let manifest = SnapshotManifest {
            id: "2026-01-01T00-00-00Z".into(),
            job: "Test".into(),
            started: String::new(),
            finished: String::new(),
            sources: vec![],
            files_copied: 0,
            files_linked: 0,
            files_failed: 0,
            bytes_copied: 0,
            bytes_linked: 0,
            duration_ms: 0,
            outcome: backstar_core::events::RunOutcome::Ok,
            parent: None,
            skipped_sources: vec![],
            copied_hashes: Default::default(),
        };
        let target = RestoreTarget::Custom { folder: r"D:\Recovered".to_string() };

        assert_eq!(
            compute_destination(&manifest, Path::new("proj/notes.txt"), &target).unwrap(),
            PathBuf::from(r"D:\Recovered\notes.txt"),
            "a file lands in the chosen folder under its own name"
        );
        assert_eq!(
            compute_destination(&manifest, Path::new("proj"), &target).unwrap(),
            PathBuf::from(r"D:\Recovered\proj"),
            "a directory becomes a same-named subfolder"
        );
        assert!(
            compute_destination(&manifest, Path::new(""), &target).is_err(),
            "a whole snapshot root has no single name to restore under"
        );
    }
}
