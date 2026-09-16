mod restore_cmd;
mod runner;
mod schedule;
mod status;
mod tray;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use backstar_core::config::{Config, History, HistoryEntry, Job, JobKind};
use backstar_core::events::{Event, RunOutcome};
use backstar_core::presets::{self, Preset};
use backstar_core::volume::{self, VolumeCapability};
use runner::{RunHandle, Runner};
use status::AppStatus;
use tauri::{Emitter, Manager};

/// Tauri commands must return a serialisable error, so map the core error to a string at
/// the boundary rather than leaking `backstar_core::Error` into the IPC layer.
pub(crate) type CmdResult<T> = std::result::Result<T, String>;

pub(crate) fn load_config() -> CmdResult<Config> {
    let path = Config::path().map_err(|e| e.to_string())?;
    if path.exists() {
        return Config::load_from(&path).map_err(|e| e.to_string());
    }

    // First run. If the PowerShell app left a config behind, carry it forward rather than
    // starting empty -- and carry its problems forward visibly too (a source folder that no
    // longer exists becomes an import note, not a silent omission).
    let cfg = match legacy_config_path() {
        Some(legacy) => {
            let text = std::fs::read_to_string(&legacy).map_err(|e| e.to_string())?;
            let app_dir = legacy.parent().unwrap_or(&legacy).to_path_buf();
            let mut cfg =
                backstar_core::config::import_legacy(&text, &app_dir).map_err(|e| e.to_string())?;
            cfg.import_notes.insert(
                0,
                format!("Imported settings from {}.", legacy.display()),
            );
            tracing::info!("imported legacy config from {}", legacy.display());
            cfg
        }
        None => Config::default(),
    };

    cfg.save_to(&path).map_err(|e| e.to_string())?;
    Ok(cfg)
}

/// Locate the PowerShell app's config, relative to this executable's checkout.
///
/// Only used on first run. Returns `None` when there is nothing to import.
fn legacy_config_path() -> Option<PathBuf> {
    let candidates = [
        // Running from the repo during development: backstar/src-tauri/target/debug/.
        PathBuf::from(r"D:\Backstar_Backup\app\BackStar.config.json"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

#[tauri::command]
fn get_status() -> CmdResult<AppStatus> {
    let cfg = load_config()?;
    let history = History::at(History::default_path().map_err(|e| e.to_string())?);
    Ok(status::compute(&cfg, &history))
}

#[tauri::command]
fn get_config() -> CmdResult<Config> {
    load_config()
}

/// Insert or replace a job by id, generating a fresh id for an empty one ("new job" --
/// server-side, rather than trusting the frontend to invent a stable, collision-free one).
///
/// Pure and separate from the `#[tauri::command]` wrapper so it is testable without
/// touching a real config file on disk.
fn upsert_job(cfg: &mut Config, mut job: backstar_core::config::Job) -> backstar_core::config::Job {
    if job.id.trim().is_empty() {
        job.id = uuid::Uuid::new_v4().to_string();
    }
    match cfg.jobs.iter_mut().find(|j| j.id == job.id) {
        Some(existing) => *existing = job.clone(),
        None => cfg.jobs.push(job.clone()),
    }
    job
}

fn remove_job(cfg: &mut Config, job_id: &str) -> Result<(), String> {
    let before = cfg.jobs.len();
    cfg.jobs.retain(|j| j.id != job_id);
    if cfg.jobs.len() == before {
        return Err(format!("no job with id {job_id:?}"));
    }
    Ok(())
}

/// Create or update a job. Returns the saved job (with its id filled in for a new one) so
/// the caller does not have to guess what got persisted.
#[tauri::command]
fn save_job(job: backstar_core::config::Job) -> CmdResult<backstar_core::config::Job> {
    let mut cfg = load_config()?;
    let saved = upsert_job(&mut cfg, job);
    let path = Config::path().map_err(|e| e.to_string())?;
    cfg.save_to(&path).map_err(|e| e.to_string())?;

    // Best-effort: a job must still save even if Task Scheduler cannot be reached (no
    // permission, service disabled). The schedule stays recorded in the config either way
    // and will be reconciled the next time this job is saved.
    if let Ok(exe) = std::env::current_exe() {
        if let Err(e) = schedule::apply_schedule(&saved, &exe) {
            tracing::warn!("could not update the scheduled task for {:?}: {e}", saved.name);
        }
    }
    Ok(saved)
}

#[tauri::command]
fn delete_job(job_id: String) -> CmdResult<()> {
    let mut cfg = load_config()?;
    remove_job(&mut cfg, &job_id)?;
    let path = Config::path().map_err(|e| e.to_string())?;
    cfg.save_to(&path).map_err(|e| e.to_string())?;

    if let Err(e) = schedule::remove_schedule(&job_id) {
        tracing::warn!("could not remove the scheduled task for job {job_id:?}: {e}");
    }
    Ok(())
}

#[tauri::command]
fn list_presets() -> Vec<Preset> {
    presets::all()
}

#[tauri::command]
fn probe_volume(path: String) -> CmdResult<VolumeCapability> {
    volume::probe(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

/// Render a [`RunOutcome`] as the short summary [`HistoryEntry::outcome`] stores.
///
/// `HistoryEntry` is a flat, display-oriented log line -- unlike
/// [`backstar_core::repo::SnapshotManifest::outcome`], nothing branches on this value, so a
/// plain string is the right shape for it here, not a second copy of the structured enum.
fn outcome_summary(o: &RunOutcome) -> String {
    match o {
        RunOutcome::Ok => "OK".into(),
        RunOutcome::Partial { failed } => {
            format!("OK ({failed} file{} failed)", if *failed == 1 { "" } else { "s" })
        }
        RunOutcome::Failed { reason } => format!("Failed: {reason}"),
        RunOutcome::Cancelled => "Cancelled".into(),
    }
}

/// A run that could not even start (a bad job configuration, a guard refusing it) still has
/// to reach the UI as a `RunDone`, or the Cancel/running indicator simply never resolves and
/// the button goes dead with no explanation. Shared by both the snapshot and mirror paths.
fn emit_start_failure(emit: &mut dyn FnMut(Event), e: backstar_core::Error) {
    emit(Event::RunDone {
        run_id: String::new(),
        outcome: RunOutcome::Failed { reason: e.to_string() },
        stats: Default::default(),
    });
}

/// After a successful run, capture the destination's volume identity if this job doesn't
/// have one recorded yet -- so a future drive-letter change doesn't strand it.
///
/// Reloads and re-saves the config rather than reusing the one already in hand: the run may
/// have taken a while, and writing back a stale copy of everything else in the config would
/// silently discard any change made elsewhere while it was running.
fn backfill_portable_dest(job_id: &str) {
    let Ok(mut cfg) = load_config() else { return };
    let Some(job) = cfg.jobs.iter_mut().find(|j| j.id == job_id) else { return };
    if job.dest_portable.is_some() {
        return;
    }
    job.backfill_portable_dest();
    if job.dest_portable.is_none() {
        // The destination doesn't support a portable record (a UNC path, most likely).
        // Nothing to persist; the job keeps working exactly as it did before this feature.
        return;
    }
    match Config::path() {
        Ok(path) => {
            if let Err(e) = cfg.save_to(&path) {
                tracing::warn!("could not persist the destination's volume identity: {e}");
            }
        }
        Err(e) => tracing::warn!("could not locate config to persist volume identity: {e}"),
    }
}

/// Where the sibling `BackStar-Restore.exe` would be, given this executable's own path.
///
/// In both a development build and the installed layout, the two executables live in the
/// same directory: a Cargo workspace builds every binary crate into one `target/` folder,
/// and the NSIS installer places both in one install folder. "Next to me" is therefore the
/// entire lookup -- no path configuration needed anywhere.
///
/// Split out from [`sibling_restore_tool`] as a pure function so the path arithmetic is
/// unit-testable without needing a real `current_exe()` (which cargo test itself makes
/// pointless to assert anything about: it always resolves to the test harness binary, not
/// to any BackStar executable).
fn restore_tool_path(exe_path: &Path) -> Option<PathBuf> {
    Some(exe_path.parent()?.join("BackStar-Restore.exe"))
}

/// The sibling `BackStar-Restore.exe` next to this executable, if one actually exists.
fn sibling_restore_tool() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = restore_tool_path(&exe)?;
    candidate.is_file().then_some(candidate)
}

/// Copy the sibling restore tool into the repository at `dest`, if one is available.
///
/// Best-effort and silent on failure beyond a log line: a backup that just succeeded must
/// not be reported as failed because a convenience copy step could not run. The
/// `RESTORE-README.txt` recovery path never depends on this succeeding.
fn refresh_restore_tool(dest: &std::path::Path) {
    let Some(tool) = sibling_restore_tool() else { return };
    match backstar_core::repo::Repo::open(dest) {
        Ok(repo) => {
            if let Err(e) = repo.refresh_restore_tool(&tool) {
                tracing::warn!("could not refresh the portable restore tool: {e}");
            }
        }
        Err(e) => tracing::warn!("could not reopen the repository to refresh the restore tool: {e}"),
    }
}

/// Run a job to completion and record it in history, dispatching on its kind. Shared by the
/// threaded Tauri run path (below) and the synchronous headless path (`main.rs`'s
/// `--run-job`) -- a scheduled run with no window watching must get exactly the same
/// history entry, portable-destination backfill, and restore-tool refresh as a click ever
/// would, not a stripped-down copy of that behaviour.
///
/// Takes the job by value so the headless caller, which has no `Runner` thread to keep it
/// alive on, can simply move its owned copy in.
fn execute_job(job: Job, job_id: String, cancel: &AtomicBool, emit: &mut dyn FnMut(Event)) {
    let history_path = match History::default_path() {
        Ok(p) => p,
        Err(e) => return emit_start_failure(emit, e),
    };
    let started = time::OffsetDateTime::now_utc();
    let timestamp =
        started.format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
    let job_dest = job.dest.clone();

    match job.kind {
        JobKind::Snapshot => match backstar_core::engine::run_job(&job, cancel, emit) {
            Ok(m) => {
                let entry = HistoryEntry {
                    timestamp,
                    job: m.job.clone(),
                    snapshot_id: Some(m.id.clone()),
                    duration_ms: m.duration_ms,
                    files_copied: m.files_copied,
                    files_linked: m.files_linked,
                    files_failed: m.files_failed,
                    bytes_copied: m.bytes_copied,
                    dest: job_dest,
                    outcome: outcome_summary(&m.outcome),
                };
                if let Err(e) = History::at(&history_path).append(&entry) {
                    tracing::warn!("could not record history: {e}");
                }
                backfill_portable_dest(&job_id);
                refresh_restore_tool(&job.resolve_dest());
            }
            Err(e) => emit_start_failure(emit, e),
        },

        // A mirror job has no repository and no manifest, so there is nothing to back-fill
        // a portable destination record from having just written, and no repo to copy the
        // restore tool into -- both of those are properties of the snapshot/repository
        // machinery this mode deliberately does not use.
        JobKind::Mirror => match backstar_core::mirror::run_mirror(&job, cancel, emit) {
            Ok(r) => {
                let entry = HistoryEntry {
                    timestamp,
                    job: job.name.clone(),
                    snapshot_id: None,
                    duration_ms: r.stats.duration_ms,
                    files_copied: r.stats.files_copied,
                    files_linked: r.stats.files_linked,
                    files_failed: r.stats.files_failed,
                    bytes_copied: r.stats.bytes_copied,
                    dest: job_dest,
                    outcome: outcome_summary(&r.outcome),
                };
                if let Err(e) = History::at(&history_path).append(&entry) {
                    tracing::warn!("could not record history: {e}");
                }
                backfill_portable_dest(&job_id);
            }
            Err(e) => emit_start_failure(emit, e),
        },
    }
}

#[tauri::command]
fn start_run(
    app: tauri::AppHandle,
    runner: tauri::State<'_, Runner>,
    job_id: String,
) -> CmdResult<RunHandle> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, &job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;

    // Refuse up front rather than part-way through. A run that cannot be completed should
    // not create a snapshot directory first.
    if !job.enabled {
        return Err(format!("job {:?} is disabled", job.name));
    }

    let label = job.name.clone();
    let sink_app = app.clone();
    let done_app = app.clone();

    runner.start(
        label,
        move |cancel, emit| execute_job(job, job_id, cancel, emit),
        move |ev| {
            let _ = sink_app.emit(runner::EVENT_CHANNEL, &ev);
        },
        move || {
            let _ = done_app.emit("backstar://run-finished", ());
        },
    )
}

/// The synchronous path a scheduled Task Scheduler run takes: no window, no `Runner`
/// thread, no event channel to emit onto -- just run the job and let it write its own
/// history entry. Called from `main.rs` under `--headless --run-job <id>`, and returns an
/// error string suitable for a non-zero process exit so a failed scheduled run is visible
/// in Task Scheduler's own history rather than silently swallowed.
pub fn run_headless(job_id: &str) -> Result<(), String> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;
    if !job.enabled {
        return Err(format!("job {:?} is disabled", job.name));
    }

    let cancel = AtomicBool::new(false);
    let mut failure: Option<String> = None;
    execute_job(job, job_id.to_string(), &cancel, &mut |ev| {
        if let Event::RunDone { outcome: RunOutcome::Failed { reason }, .. } = &ev {
            failure = Some(reason.clone());
        }
    });

    match failure {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// The confirmation step for a mirror ("Live Sync") job: what it would add, update, and
/// delete, without touching anything. The frontend must show this before offering to run a
/// mirror job at all -- the whole point is that the destructive count is real, never
/// invented, before the user commits to it.
#[tauri::command]
fn preview_mirror(job_id: String) -> CmdResult<backstar_core::mirror::MirrorPreview> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, &job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;
    backstar_core::mirror::preview_mirror(&job).map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_run(runner: tauri::State<'_, Runner>) -> bool {
    runner.cancel()
}

#[tauri::command]
fn running_job(runner: tauri::State<'_, Runner>) -> Option<String> {
    runner.running_label()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backstar=info".into()),
        )
        .init();

    tauri::Builder::default()
        // Must be the first plugin registered -- it needs to win the race for the
        // single-instance lock before anything else in startup can run. Without this, two
        // copies of BackStar can both open the same repository at once: two backup runs
        // racing to create the same snapshot id, or one mirroring while the other restores.
        // `Runner`'s own in-process lock (see runner.rs) only ever protects ONE process
        // against itself.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch means the user tried to open BackStar again -- most likely
            // from a shortcut, not realising it is already running (possibly minimised to
            // the tray). Surface the existing window instead of silently no-op'ing, which
            // would look like the second launch simply did nothing.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(Runner::default());
            tray::setup(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            save_job,
            delete_job,
            list_presets,
            probe_volume,
            start_run,
            preview_mirror,
            cancel_run,
            running_job,
            restore_cmd::list_snapshots,
            restore_cmd::browse_snapshot,
            restore_cmd::resolve_restore_original,
            restore_cmd::start_restore,
        ])
        .run(tauri::generate_context!())
        .expect("error while running BackStar");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_summary_reads_naturally_for_every_case() {
        assert_eq!(outcome_summary(&RunOutcome::Ok), "OK");
        assert_eq!(outcome_summary(&RunOutcome::Cancelled), "Cancelled");
        assert_eq!(
            outcome_summary(&RunOutcome::Partial { failed: 1 }),
            "OK (1 file failed)"
        );
        assert_eq!(
            outcome_summary(&RunOutcome::Partial { failed: 3 }),
            "OK (3 files failed)"
        );
        assert_eq!(
            outcome_summary(&RunOutcome::Failed { reason: "disk full".into() }),
            "Failed: disk full"
        );
    }

    #[test]
    fn restore_tool_sits_next_to_whatever_exe_is_asking() {
        assert_eq!(
            restore_tool_path(Path::new(r"D:\Program Files\BackStar\BackStar.exe")),
            Some(PathBuf::from(r"D:\Program Files\BackStar\BackStar-Restore.exe"))
        );
        // A Cargo workspace builds every binary crate into the same target directory, so
        // a dev build resolves the sibling exactly the same way an installed one does.
        assert_eq!(
            restore_tool_path(Path::new(r"D:\proj\target\debug\BackStar.exe")),
            Some(PathBuf::from(r"D:\proj\target\debug\BackStar-Restore.exe"))
        );
    }

    #[test]
    fn restore_tool_path_for_a_bare_filename_is_relative_to_the_current_directory() {
        // `Path::parent()` on a single relative component returns `Some("")`, not `None` --
        // an empty parent, which is exactly right here: no directory prefix means "resolve
        // relative to wherever the process's current directory is", the same way the OS
        // would resolve a bare "BackStar.exe" typed at a shell.
        assert_eq!(
            restore_tool_path(Path::new("BackStar.exe")),
            Some(PathBuf::from("BackStar-Restore.exe"))
        );
        // `None` is reserved for paths that are already at the root and have nothing left
        // to strip -- there is no directory "above" empty or "/" to place a sibling in.
        assert_eq!(restore_tool_path(Path::new("")), None);
    }

    // ---------------------------------------------------------------- job CRUD

    fn bare_job(name: &str) -> backstar_core::config::Job {
        backstar_core::config::Job {
            id: String::new(),
            name: name.into(),
            sources: vec![],
            dest: PathBuf::from(r"D:\dest"),
            dest_portable: None,
            kind: backstar_core::config::JobKind::Snapshot,
            excludes: backstar_core::exclude::ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled: true,
            schedule: None,
        }
    }

    #[test]
    fn upsert_generates_an_id_for_a_new_job_and_appends_it() {
        let mut cfg = Config::default();
        let saved = upsert_job(&mut cfg, bare_job("Projects"));

        assert!(!saved.id.is_empty(), "a new job must get a real id, not an empty one");
        assert_eq!(cfg.jobs.len(), 1);
        assert_eq!(cfg.jobs[0].id, saved.id);
    }

    #[test]
    fn upsert_with_an_existing_id_replaces_in_place_rather_than_duplicating() {
        let mut cfg = Config::default();
        let first = upsert_job(&mut cfg, bare_job("Projects"));

        let mut edited = first.clone();
        edited.name = "Projects (renamed)".into();
        let second = upsert_job(&mut cfg, edited);

        assert_eq!(second.id, first.id, "the id must be preserved across an edit");
        assert_eq!(cfg.jobs.len(), 1, "editing must not create a second job");
        assert_eq!(cfg.jobs[0].name, "Projects (renamed)");
    }

    #[test]
    fn two_new_jobs_get_two_different_ids() {
        let mut cfg = Config::default();
        let a = upsert_job(&mut cfg, bare_job("A"));
        let b = upsert_job(&mut cfg, bare_job("B"));
        assert_ne!(a.id, b.id);
        assert_eq!(cfg.jobs.len(), 2);
    }

    #[test]
    fn remove_job_deletes_the_matching_job_only() {
        let mut cfg = Config::default();
        let a = upsert_job(&mut cfg, bare_job("A"));
        let b = upsert_job(&mut cfg, bare_job("B"));

        remove_job(&mut cfg, &a.id).unwrap();

        assert_eq!(cfg.jobs.len(), 1);
        assert_eq!(cfg.jobs[0].id, b.id, "the other job must be untouched");
    }

    #[test]
    fn removing_an_unknown_job_id_is_a_clear_error() {
        let mut cfg = Config::default();
        let err = remove_job(&mut cfg, "no-such-id").unwrap_err();
        assert!(err.contains("no-such-id"));
    }
}
