//! Tauri commands for browsing snapshots and restoring from them.
//!
//! Named `restore_cmd` rather than `restore` to avoid shadowing `backstar_core::restore`,
//! which does the actual work; this module is purely the IPC-facing translation layer --
//! computing the one thing `backstar_core::restore::restore` deliberately leaves to its
//! caller (the exact destination path) and handing the rest straight through.

use std::path::{Path, PathBuf};

use backstar_core::events::Event;
use backstar_core::repo::{Repo, SnapshotEntry, SnapshotManifest};
use serde::Deserialize;

use crate::runner::{find_job, RunHandle, Runner};
use crate::CmdResult;

fn open_repo(job_id: &str) -> CmdResult<Repo> {
    let cfg = crate::load_config()?;
    let job = find_job(&cfg, job_id).ok_or_else(|| format!("no job with id {job_id:?}"))?;
    Repo::open(job.resolve_dest())
        .map_err(|_| format!("{} has no backups yet", job.name))
}

/// List every snapshot for a job, newest first.
///
/// An empty result means the job has never completed a run -- not an error, since that is
/// simply the state of a freshly configured job.
#[tauri::command]
pub fn list_snapshots(job_id: String) -> CmdResult<Vec<SnapshotManifest>> {
    let repo = match open_repo(&job_id) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let mut manifests = repo.manifests();
    // Ids are lexicographically chronological by construction (see
    // `repo::snapshot_id_from`), so a plain string sort in reverse is newest-first.
    manifests.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(manifests)
}

/// List the children of a path inside a snapshot.
///
/// Thin IPC wrapper: the actual listing logic lives in
/// [`backstar_core::repo::Repo::list_snapshot_children`], shared with the standalone
/// `BackStar-Restore` tool so both present identical listings.
#[tauri::command]
pub fn browse_snapshot(
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
) -> CmdResult<Vec<SnapshotEntry>> {
    let repo = open_repo(&job_id)?;
    repo.list_snapshot_children(&snapshot_id, Path::new(&snapshot_rel))
        .map_err(|e| e.to_string())
}

/// Where an item would go if restored to its original location, or `None` if the manifest
/// has no record of it (the source's name changed, or -- unlike the PowerShell app -- this
/// should now be rare, since every snapshot carries this for every source it contains).
#[tauri::command]
pub fn resolve_restore_original(
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
) -> CmdResult<Option<String>> {
    let repo = open_repo(&job_id)?;
    let manifest = repo.read_manifest(&snapshot_id).map_err(|e| e.to_string())?;
    Ok(backstar_core::restore::resolve_original_path(&manifest, Path::new(&snapshot_rel))
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
    repo: &Repo,
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
            let item_path = repo.snapshot_dir(&manifest.id).join(snapshot_rel);
            let name = item_path
                .file_name()
                .ok_or_else(|| "invalid restore path".to_string())?;
            Ok(PathBuf::from(folder).join(name))
        }
    }
}

#[tauri::command]
pub fn start_restore(
    app: tauri::AppHandle,
    runner: tauri::State<'_, Runner>,
    job_id: String,
    snapshot_id: String,
    snapshot_rel: String,
    target: RestoreTarget,
) -> CmdResult<RunHandle> {
    use tauri::Emitter;

    let repo = open_repo(&job_id)?;
    let manifest = repo.read_manifest(&snapshot_id).map_err(|e| e.to_string())?;
    let rel = PathBuf::from(&snapshot_rel);

    let item_path = repo.snapshot_dir(&snapshot_id).join(&rel);
    if !item_path.exists() {
        return Err(format!("nothing at {snapshot_rel:?} in snapshot {snapshot_id:?}"));
    }

    let dest = compute_destination(&repo, &manifest, &rel, &target)?;

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
            if let Err(e) = backstar_core::restore::restore(
                &repo,
                &snapshot_id,
                &rel,
                &dest,
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
