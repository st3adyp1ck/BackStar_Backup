mod lockcheck;
mod restore_cmd;
mod runner;
mod schedule;
mod status;
mod tray;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use backstar_core::config::{Config, History, HistoryEntry, Job, JobKind};
use backstar_core::events::{Event, RunOutcome};
use backstar_core::mirror::MirrorPreview;
use backstar_core::presets::{self, Preset};
use backstar_core::volume::{self, VolumeCapability};
use runner::{RunHandle, Runner};
use status::AppStatus;
use tauri::{Emitter, Manager};

/// Tauri commands must return a serialisable error, so map the core error to a string at
/// the boundary rather than leaking `backstar_core::Error` into the IPC layer.
pub(crate) type CmdResult<T> = std::result::Result<T, String>;

/// How long a stashed mirror preview unlocks a run for. The delete set is pinned into the
/// run (F13's drift guard); a preview older than this could describe a destination that no
/// longer exists in that shape, so the UI must re-preview.
const MIRROR_PREVIEW_TTL: Duration = Duration::from_secs(600);

/// A mirror preview captured at confirmation time (F2/F13). Single-use: `start_run`
/// takes it out of the stash.
struct StashedPreview {
    preview: MirrorPreview,
    stashed_at: Instant,
}

/// Shared application state behind Tauri's `.manage`.
#[derive(Default)]
pub struct AppState {
    /// The corrupt-config quarantine notice captured at startup (F19). The UI shows it as
    /// a dismissible banner via `config_notice`. `None` when the config loaded clean.
    config_notice: Mutex<Option<String>>,
    /// Mirror previews stashed by the `preview_mirror` command, keyed by job id.
    mirror_previews: Mutex<HashMap<String, StashedPreview>>,
    /// Scheduled tasks that were missing at startup and could not be recreated (F15).
    /// Surfaced via `schedule_notices`; the UI renders them as warnings.
    schedule_notices: Mutex<Vec<String>>,
}

impl AppState {
    fn take_mirror_preview(&self, job_id: &str) -> Option<StashedPreview> {
        self.mirror_previews.lock().ok()?.remove(job_id)
    }

    fn stash_mirror_preview(&self, job_id: String, preview: MirrorPreview) {
        if let Ok(mut map) = self.mirror_previews.lock() {
            map.insert(job_id, StashedPreview { preview, stashed_at: Instant::now() });
        }
    }

    fn drop_mirror_preview(&self, job_id: &str) {
        if let Ok(mut map) = self.mirror_previews.lock() {
            map.remove(job_id);
        }
    }
}

/// Load the config from disk (creating a default file on first run), and LOG any
/// corruption-quarantine notice (F19). The UI banner copy is captured once, at startup,
/// into [`AppState::config_notice`] -- repeating it from every command would make the
/// banner reappear forever after dismissal.
pub(crate) fn load_config() -> CmdResult<Config> {
    let path = Config::path().map_err(|e| e.to_string())?;
    let load = Config::load_from(&path).map_err(|e| e.to_string())?;
    if let Some(notice) = &load.notice {
        tracing::warn!("{notice}");
    }
    if !path.exists() {
        // First run: persist the defaults so the file exists from here on.
        load.config.save_to(&path).map_err(|e| e.to_string())?;
    }
    Ok(load.config)
}

/// Capture the quarantine notice (if any) of the startup config load. Split from
/// [`load_config`] so the one-time capture at startup and the per-command loads share
/// exactly one loader.
fn capture_config_notice(state: &AppState) {
    let Ok(path) = Config::path() else { return };
    if !path.exists() {
        return;
    }
    match Config::load_from(&path) {
        Ok(load) => {
            if let Ok(mut slot) = state.config_notice.lock() {
                *slot = load.notice;
            }
        }
        // A hard error here (a newer schema version, an unreadable file) surfaces through
        // the commands' own error paths -- startup must not abort over it.
        Err(e) => tracing::warn!("could not load the config at startup: {e}"),
    }
}

/// Heavy or IO-bound commands run off the UI thread: `#[tauri::command(async)]` puts the
/// body on Tauri's blocking thread pool (F14). Command names and payload shapes are
/// unchanged; only the thread they run on moved.
#[tauri::command(async)]
fn get_status() -> CmdResult<AppStatus> {
    let cfg = load_config()?;
    let history = History::at(History::default_path().map_err(|e| e.to_string())?);
    Ok(status::compute(&cfg, &history))
}

#[tauri::command(async)]
fn get_config() -> CmdResult<Config> {
    load_config()
}

/// The corrupt-config quarantine notice captured at startup (F19), if any. Dismissal is
/// the UI's business; this is a plain read of the startup capture.
#[tauri::command]
fn config_notice(state: tauri::State<'_, AppState>) -> Option<String> {
    state.config_notice.lock().ok().and_then(|g| g.clone())
}

/// Scheduled tasks found missing at startup that could not be recreated (F15). Surfaced
/// as warnings; re-saving the job retries the creation.
#[tauri::command]
fn schedule_notices(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.schedule_notices.lock().map(|g| g.clone()).unwrap_or_default()
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

/// Validate a job before it is persisted (F45). Every failure is a specific, actionable
/// sentence -- the editor shows it next to the field that is wrong. Pure and separate so
/// it is unit-testable without a config file.
fn validate_job(cfg: &Config, job: &Job) -> CmdResult<()> {
    if job.name.trim().is_empty() {
        return Err("A job name is required.".into());
    }
    if job.sources.is_empty() && job.presets.is_empty() {
        return Err("Add at least one source folder or system preset.".into());
    }
    let dest_text = job.dest.as_os_str().to_string_lossy();
    if dest_text.trim().is_empty() {
        return Err("A destination folder is required.".into());
    }
    // Environment-variable spellings (%BACKUP_DRIVE%\Backups) are legitimate; judge
    // absoluteness on the expansion.
    let expanded = backstar_core::guards::expand_env(&dest_text);
    if !Path::new(&expanded).is_absolute() {
        return Err(format!(
            "The destination must be an absolute path (like D:\\\\Backups or \
             \\\\server\\share\\backups) -- got {dest_text:?}."
        ));
    }
    if let Some(sched) = job.schedule {
        if sched.hour > 23 || sched.minute > 59 {
            return Err(format!(
                "The schedule must be a valid 24-hour time -- got {:02}:{:02}.",
                sched.hour, sched.minute
            ));
        }
    }
    // History and (until F40) snapshot linking are keyed by job NAME: two jobs sharing a
    // name would merge their records invisibly.
    let name = job.name.trim();
    if cfg
        .jobs
        .iter()
        .any(|j| j.id != job.id && j.name.trim().eq_ignore_ascii_case(name))
    {
        return Err(format!("A job named {name:?} already exists."));
    }
    // A provided id is an existing job's id (kept as-is); a new job's id is generated
    // server-side by `upsert_job`. Reject characters that would break the scheduled
    // task's command line or its Task Scheduler folder name even under quoting.
    if !job.id.is_empty()
        && job.id.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '/'))
    {
        return Err("The job id contains characters that are not allowed.".into());
    }
    Ok(())
}

/// The `save_job` response (F15): the persisted job plus what Task Scheduler actually did
/// with its schedule. A silently-failed apply used to leave the UI showing a schedule that
/// did not exist; now the failure rides the response.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveJobResponse {
    pub job: backstar_core::config::Job,
    /// The Task Scheduler state now matches the job's `schedule` field.
    pub schedule_applied: bool,
    /// Why not, when it isn't. `None` when applied.
    pub schedule_error: Option<String>,
}

/// Create or update a job. Returns the saved job (with its id filled in for a new one) plus
/// the schedule outcome, so the caller does not have to guess what got persisted.
#[tauri::command(async)]
fn save_job(
    state: tauri::State<'_, AppState>,
    job: backstar_core::config::Job,
) -> CmdResult<SaveJobResponse> {
    let mut cfg = load_config()?;
    validate_job(&cfg, &job)?;
    let saved = upsert_job(&mut cfg, job);
    let path = Config::path().map_err(|e| e.to_string())?;
    cfg.save_to(&path).map_err(|e| e.to_string())?;

    // F13: a saved job may differ from what its stashed preview described -- a preview
    // taken before the edit must not unlock a run of the edited job.
    state.drop_mirror_preview(&saved.id);

    // The schedule result reaches the UI (F15). Best-effort in that a job must still SAVE
    // even when Task Scheduler cannot be reached -- the error is reported, not fatal.
    let (schedule_applied, schedule_error) = match std::env::current_exe() {
        Ok(exe) => match schedule::apply_schedule(&saved, &exe) {
            Ok(()) => (true, None),
            Err(e) => {
                tracing::warn!("could not update the scheduled task for {:?}: {e}", saved.name);
                (false, Some(e))
            }
        },
        Err(e) => (false, Some(format!("could not locate this executable: {e}"))),
    };
    Ok(SaveJobResponse { job: saved, schedule_applied, schedule_error })
}

#[tauri::command(async)]
fn delete_job(state: tauri::State<'_, AppState>, job_id: String) -> CmdResult<()> {
    let mut cfg = load_config()?;
    remove_job(&mut cfg, &job_id)?;
    state.drop_mirror_preview(&job_id);
    let path = Config::path().map_err(|e| e.to_string())?;
    cfg.save_to(&path).map_err(|e| e.to_string())?;

    if let Err(e) = schedule::remove_schedule(&job_id) {
        tracing::warn!("could not remove the scheduled task for job {job_id:?}: {e}");
    }
    Ok(())
}

#[tauri::command(async)]
fn list_presets() -> Vec<Preset> {
    presets::all()
}

/// The `default_excludes` response (L11): the engine's canonical exclusion sets, grouped
/// exactly the way the job editor's two toggles combine them. The inner rules keep
/// `ExcludeRules`' snake_case wire shape, matching the UI's `ExcludeRules` type one for
/// one -- renaming here would silently break the profile matching that decides whether a
/// saved job's rules are a standard profile or a custom set to round-trip untouched.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultExcludesDto {
    /// The "skip build/cache folders" set: project dir names and source-relative paths.
    pub project: backstar_core::exclude::ExcludeRules,
    /// The "skip browser caches and temp files" set: system dir names and file globs.
    pub system: backstar_core::exclude::ExcludeRules,
}

/// Serve the canonical exclusion sets to the job editor (L11). The editor used to
/// hardcode a copy of these lists, and the two sides could drift: a stale UI copy would
/// rebuild a job's rules from lists the engine no longer applies, or misclassify a real
/// profile job as "custom" (F59). `backstar_core::exclude` is the single source of
/// truth; this command is the only way the UI reads it.
#[tauri::command(async)]
fn default_excludes() -> DefaultExcludesDto {
    DefaultExcludesDto {
        project: backstar_core::exclude::ExcludeRules::project(),
        system: backstar_core::exclude::ExcludeRules::system(),
    }
}

#[tauri::command(async)]
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

/// Append a history entry, best-effort. A history write failure must never fail the run
/// that just succeeded -- it is logged and the run's own result stands.
fn append_history(history_path: &PathBuf, entry: &HistoryEntry) {
    if let Err(e) = History::at(history_path).append(entry) {
        tracing::warn!("could not record history: {e}");
    }
}

/// Run a job to completion and record it in history, dispatching on its kind. Shared by the
/// threaded Tauri run path (below) and the synchronous headless path (`main.rs`'s
/// `--run-job`) -- a scheduled run with no window watching must get exactly the same
/// history entry, portable-destination backfill, retention pruning, and restore-tool
/// refresh as a click ever would, not a stripped-down copy of that behaviour.
///
/// `keep_snapshots` is the config's retention policy (D2). `expected_deletes` is the
/// confirmed mirror preview's delete set (F13): `Some` only for an interactive mirror run,
/// `None` for snapshots and for headless/scheduled mirror runs (which have no preview --
/// documented at `run_mirror_with_expected_deletes`).
///
/// History honesty (F17/F40/F54): every terminal state is recorded, including start
/// failures; entries carry the job's id (not just its renameable name) and a mirror's
/// deletion count; a cancelled snapshot run records `snapshot_id: None` because its
/// partial directory is deleted and there is no id to point at.
///
/// Takes the job by value so the headless caller, which has no `Runner` thread to keep it
/// alive on, can simply move its owned copy in.
fn execute_job(
    job: Job,
    job_id: String,
    keep_snapshots: Option<u32>,
    expected_deletes: Option<Vec<backstar_core::mirror::MirrorDelete>>,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) {
    let history_path = match History::default_path() {
        Ok(p) => p,
        Err(e) => return emit_start_failure(emit, e),
    };
    let started = time::OffsetDateTime::now_utc();
    let timestamp =
        started.format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
    let job_dest = job.dest.clone();

    // The F17 failure entry: a run that could not even start reaches history as Failed,
    // or status would keep showing the last success as if nothing went wrong.
    let failed_entry = |e: &backstar_core::Error| HistoryEntry {
        timestamp: timestamp.clone(),
        job: job.name.clone(),
        job_id: job_id.clone(),
        snapshot_id: None,
        duration_ms: (time::OffsetDateTime::now_utc() - started).whole_milliseconds() as u64,
        files_copied: 0,
        files_linked: 0,
        files_failed: 0,
        files_deleted: 0,
        bytes_copied: 0,
        dest: job_dest.clone(),
        outcome: outcome_summary(&RunOutcome::Failed { reason: e.to_string() }),
    };

    match job.kind {
        JobKind::Snapshot => match backstar_core::engine::run_job(&job, keep_snapshots, cancel, emit) {
            Ok(m) => {
                let entry = HistoryEntry {
                    timestamp,
                    job: m.job.clone(),
                    job_id: job_id.clone(),
                    // A cancelled run's partial snapshot is deleted -- no id to point at.
                    snapshot_id: if matches!(m.outcome, RunOutcome::Cancelled) {
                        None
                    } else {
                        Some(m.id.clone())
                    },
                    duration_ms: m.duration_ms,
                    files_copied: m.files_copied,
                    files_linked: m.files_linked,
                    files_failed: m.files_failed,
                    files_deleted: 0, // a snapshot never deletes
                    bytes_copied: m.bytes_copied,
                    dest: job_dest,
                    outcome: outcome_summary(&m.outcome),
                };
                append_history(&history_path, &entry);
                backfill_portable_dest(&job_id);
                refresh_restore_tool(&job.resolve_dest());
            }
            Err(e) => {
                append_history(&history_path, &failed_entry(&e));
                emit_start_failure(emit, e);
            }
        },

        // A mirror job has no repository and no manifest, so there is nothing to back-fill
        // a portable destination record from having just written, and no repo to copy the
        // restore tool into -- both of those are properties of the snapshot/repository
        // machinery this mode deliberately does not use.
        JobKind::Mirror => match backstar_core::mirror::run_mirror_with_expected_deletes(
            &job,
            expected_deletes.as_deref(),
            cancel,
            emit,
        ) {
            Ok(r) => {
                let entry = HistoryEntry {
                    timestamp,
                    job: job.name.clone(),
                    job_id: job_id.clone(),
                    snapshot_id: None,
                    duration_ms: r.stats.duration_ms,
                    files_copied: r.stats.files_copied,
                    files_linked: r.stats.files_linked,
                    files_failed: r.stats.files_failed,
                    files_deleted: r.stats.files_deleted,
                    bytes_copied: r.stats.bytes_copied,
                    dest: job_dest,
                    outcome: outcome_summary(&r.outcome),
                };
                append_history(&history_path, &entry);
                backfill_portable_dest(&job_id);
            }
            Err(e) => {
                append_history(&history_path, &failed_entry(&e));
                emit_start_failure(emit, e);
            }
        },
    }
}

/// The mirror-run gate (F2/F13), factored pure so it is testable without a window: a
/// mirror job needs `confirm_mirror: true` and a fresh stashed preview (single-use -- the
/// stash is taken, so a second run attempt needs a fresh preview).
fn mirror_delete_set_for_run(
    state: &AppState,
    job: &Job,
    job_id: &str,
    confirm_mirror: bool,
) -> CmdResult<Option<Vec<backstar_core::mirror::MirrorDelete>>> {
    match job.kind {
        JobKind::Snapshot => Ok(None),
        JobKind::Mirror => {
            if !confirm_mirror {
                return Err(format!(
                    "{:?} is a mirror job: it deletes whatever is absent from the source. \
                     Preview it and confirm the deletion list first.",
                    job.name
                ));
            }
            match state.take_mirror_preview(job_id) {
                Some(s) if s.stashed_at.elapsed() <= MIRROR_PREVIEW_TTL => {
                    Ok(Some(s.preview.delete_set))
                }
                Some(_) => Err(
                    "the mirror preview is more than 10 minutes old -- preview it again \
                     before running"
                        .into(),
                ),
                None => Err(
                    "mirror jobs must be previewed before they can run -- open the preview \
                     first"
                        .into(),
                ),
            }
        }
    }
}

/// Start a job on the Runner's single-flight thread.
///
/// Mirror jobs are gated (F2/F13): `confirm_mirror` must be true AND a fresh (≤ 10 min)
/// preview for this job must be stashed by `preview_mirror`. The stashed delete set is
/// then pinned into the run via `run_mirror_with_expected_deletes`, so anything the
/// destination gained between confirmation and execution aborts the run back to preview
/// instead of being deleted unreviewed. Snapshot jobs ignore `confirm_mirror`.
#[tauri::command]
fn start_run(
    app: tauri::AppHandle,
    runner: tauri::State<'_, Runner>,
    state: tauri::State<'_, AppState>,
    job_id: String,
    confirm_mirror: bool,
) -> CmdResult<RunHandle> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, &job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;

    // Refuse up front rather than part-way through. A run that cannot be completed should
    // not create a snapshot directory first.
    if !job.enabled {
        return Err(format!("job {:?} is disabled", job.name));
    }

    let expected_deletes = mirror_delete_set_for_run(&state, &job, &job_id, confirm_mirror)?;

    let keep_snapshots = cfg.keep_snapshots;
    let label = job.name.clone();
    let sink_app = app.clone();
    let done_app = app.clone();

    runner.start(
        label,
        move |cancel, emit| execute_job(job, job_id, keep_snapshots, expected_deletes, cancel, emit),
        move |ev| {
            let _ = sink_app.emit(runner::EVENT_CHANNEL, &ev);
        },
        move || {
            let _ = done_app.emit("backstar://run-finished", ());
        },
    )
}

/// Headless exit codes (F43), documented at the top of `main.rs` where the contract
/// lives: a partially-failed scheduled run must not exit 0 (Task Scheduler and any caller
/// would read that as full success).
pub const HEADLESS_EXIT_OK: i32 = 0;
pub const HEADLESS_EXIT_FAILED: i32 = 1;
pub const HEADLESS_EXIT_PARTIAL: i32 = 3;

/// Map a terminal run outcome to a headless exit code. Cancelled counts as failed: a
/// scheduled run cannot be user-cancelled, so a Cancelled outcome there means something
/// external interrupted it.
fn exit_code_for_outcome(outcome: &RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Ok => HEADLESS_EXIT_OK,
        RunOutcome::Partial { .. } => HEADLESS_EXIT_PARTIAL,
        RunOutcome::Failed { .. } | RunOutcome::Cancelled => HEADLESS_EXIT_FAILED,
    }
}

/// The synchronous path a scheduled Task Scheduler run takes: no window, no `Runner`
/// thread, no event channel to emit onto -- just run the job and let it write its own
/// history entry. Called from `main.rs` under `--headless --run-job <id>`, and returns the
/// process exit code (F43: 0 ok, 1 failed, 3 partial) so the result is visible in Task
/// Scheduler's own history rather than silently swallowed.
///
/// Headless mirror runs have no confirmed preview, so they pass no expected delete set --
/// the documented behavior of `run_mirror_with_expected_deletes` with `None`.
pub fn run_headless(job_id: &str) -> i32 {
    let outcome = run_headless_inner(job_id);
    match outcome {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("scheduled run failed: {e}");
            eprintln!("scheduled run failed: {e}");
            HEADLESS_EXIT_FAILED
        }
    }
}

fn run_headless_inner(job_id: &str) -> Result<i32, String> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;
    if !job.enabled {
        return Err(format!("job {:?} is disabled", job.name));
    }

    let keep_snapshots = cfg.keep_snapshots;
    let cancel = AtomicBool::new(false);
    let mut final_outcome: Option<RunOutcome> = None;
    execute_job(job, job_id.to_string(), keep_snapshots, None, &cancel, &mut |ev| {
        if let Event::RunDone { outcome, .. } = &ev {
            final_outcome = Some(outcome.clone());
        }
    });

    match final_outcome {
        Some(o) => Ok(exit_code_for_outcome(&o)),
        // execute_job always emits a terminal RunDone; its absence means something in the
        // plumbing itself broke.
        None => {
            tracing::error!("scheduled run produced no terminal event");
            Ok(HEADLESS_EXIT_FAILED)
        }
    }
}

/// The confirmation step for a mirror ("Live Sync") job: what it would add, update, and
/// delete, without touching anything. The frontend must show this before offering to run a
/// mirror job at all -- the whole point is that the destructive count is real, never
/// invented, before the user commits to it.
///
/// The preview is also STASHED (F2/F13): `start_run` refuses a mirror job without a fresh
/// stashed preview, and pins exactly this delete set into the run, so files created at the
/// destination between confirmation and execution abort the run rather than being deleted
/// unreviewed.
#[tauri::command(async)]
fn preview_mirror(
    state: tauri::State<'_, AppState>,
    job_id: String,
) -> CmdResult<backstar_core::mirror::MirrorPreview> {
    let cfg = load_config()?;
    let job = runner::find_job(&cfg, &job_id)
        .ok_or_else(|| format!("no job with id {job_id:?}"))?;
    let preview = backstar_core::mirror::preview_mirror(&job).map_err(|e| e.to_string())?;
    state.stash_mirror_preview(job_id, preview.clone());
    Ok(preview)
}

#[tauri::command]
fn cancel_run(runner: tauri::State<'_, Runner>) -> bool {
    runner.cancel()
}

#[tauri::command]
fn running_job(runner: tauri::State<'_, Runner>) -> Option<String> {
    runner.running_label()
}

/// The full run history, oldest first -- the Activity view's data source (F61). Reading is
/// IO, so it runs off the UI thread like every other command that touches disk.
#[tauri::command(async)]
fn get_history() -> CmdResult<Vec<backstar_core::config::HistoryEntry>> {
    History::at(History::default_path().map_err(|e| e.to_string())?)
        .read()
        .map_err(|e| e.to_string())
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let state = AppState::default();
            // F19: capture a corrupt-config quarantine notice once, at startup, so the UI
            // can banner it without every command re-raising it.
            capture_config_notice(&state);
            app.manage(state);
            app.manage(Runner::default());
            tray::setup(app.handle())?;

            // F15: reconcile recorded schedules with Task Scheduler -- off the UI thread,
            // since each check is a schtasks subprocess. A task missing for an enabled
            // scheduled job is recreated once; failure becomes a notice the UI can show.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let Ok(cfg) = load_config() else { return };
                let notices = schedule::reconcile_schedules(&cfg.jobs, &schedule::task_exists, &|job| {
                    std::env::current_exe()
                        .map_err(|e| format!("could not locate this executable: {e}"))
                        .and_then(|exe| schedule::apply_schedule(job, &exe))
                });
                if !notices.is_empty() {
                    if let Some(state) = handle.try_state::<AppState>() {
                        if let Ok(mut slot) = state.schedule_notices.lock() {
                            slot.extend(notices);
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            config_notice,
            schedule_notices,
            save_job,
            delete_job,
            list_presets,
            default_excludes,
            probe_volume,
            lockcheck::list_lock_prone_running,
            start_run,
            preview_mirror,
            cancel_run,
            running_job,
            get_history,
            restore_cmd::list_snapshots,
            restore_cmd::browse_snapshot,
            restore_cmd::resolve_restore_original,
            restore_cmd::plan_restore_overwrites,
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

    // ------------------------------------------------------- F45: save_job validation

    #[test]
    fn validation_rejects_an_empty_name() {
        let cfg = Config::default();
        let mut j = bare_job("   ");
        j.sources = vec![PathBuf::from(r"D:\src")];
        let err = validate_job(&cfg, &j).unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn validation_rejects_a_job_with_nothing_to_back_up() {
        let cfg = Config::default();
        let j = bare_job("Projects"); // no sources, no presets
        let err = validate_job(&cfg, &j).unwrap_err();
        assert!(err.contains("source"), "{err}");
    }

    #[test]
    fn validation_rejects_missing_and_relative_destinations() {
        let cfg = Config::default();
        let mut j = bare_job("Projects");
        j.sources = vec![PathBuf::from(r"D:\src")];

        j.dest = PathBuf::from("");
        assert!(validate_job(&cfg, &j).unwrap_err().contains("destination"));

        j.dest = PathBuf::from("backups");
        let err = validate_job(&cfg, &j).unwrap_err();
        assert!(err.contains("absolute"), "{err}");

        // UNC and %VAR% spellings are legitimate.
        j.dest = PathBuf::from(r"\\server\share\backups");
        validate_job(&cfg, &j).unwrap();
        j.dest = PathBuf::from(r"%BACKSTAR_TEST_DEST%\backups");
        std::env::set_var("BACKSTAR_TEST_DEST", r"D:\dest");
        validate_job(&cfg, &j).unwrap();
        std::env::remove_var("BACKSTAR_TEST_DEST");
    }

    #[test]
    fn validation_rejects_an_impossible_schedule_time() {
        let cfg = Config::default();
        let mut j = bare_job("Projects");
        j.sources = vec![PathBuf::from(r"D:\src")];
        j.schedule = Some(backstar_core::config::DailySchedule { hour: 25, minute: 61 });
        let err = validate_job(&cfg, &j).unwrap_err();
        assert!(err.contains("25:61"), "{err}");

        j.schedule = Some(backstar_core::config::DailySchedule { hour: 23, minute: 59 });
        validate_job(&cfg, &j).unwrap();
    }

    /// History is keyed by job name (until F40), so a duplicate name would merge two
    /// jobs' records invisibly. Rejected case-insensitively, but a job may keep its own
    /// name on edit.
    #[test]
    fn validation_rejects_a_duplicate_job_name_but_allows_keeping_yours() {
        let mut cfg = Config::default();
        let existing = upsert_job(&mut cfg, {
            let mut j = bare_job("Projects");
            j.sources = vec![PathBuf::from(r"D:\src")];
            j
        });

        let mut dupe = bare_job("projects"); // same name, different case, new id
        dupe.sources = vec![PathBuf::from(r"D:\other")];
        let err = validate_job(&cfg, &dupe).unwrap_err();
        assert!(err.contains("already exists"), "{err}");

        // Editing the existing job while keeping its name is fine.
        let mut edited = existing.clone();
        edited.sources = vec![PathBuf::from(r"D:\new")];
        validate_job(&cfg, &edited).unwrap();
    }

    #[test]
    fn validation_rejects_ids_that_would_break_the_scheduled_command_line() {
        let cfg = Config::default();
        for bad in ["has space", "quote\"id", "apostrophe'id", r"back\slash", "slash/id"] {
            let mut j = bare_job("Projects");
            j.sources = vec![PathBuf::from(r"D:\src")];
            j.id = bad.into();
            assert!(validate_job(&cfg, &j).is_err(), "id {bad:?} must be rejected");
        }
        // A fresh uuid-shaped id is fine.
        let mut j = bare_job("Projects");
        j.sources = vec![PathBuf::from(r"D:\src")];
        j.id = uuid::Uuid::new_v4().to_string();
        validate_job(&cfg, &j).unwrap();
    }

    // ------------------------------------------- F2/F13: the mirror preview stash

    fn mirror_job_named(name: &str) -> backstar_core::config::Job {
        let mut j = bare_job(name);
        j.kind = JobKind::Mirror;
        j
    }

    #[test]
    fn a_mirror_run_requires_confirmation_and_a_fresh_stashed_preview() {
        let state = AppState::default();
        let job = mirror_job_named("Mirror");

        // No preview stashed at all.
        let err = mirror_delete_set_for_run(&state, &job, "m", true).unwrap_err();
        assert!(err.contains("previewed"), "{err}");

        // Stash one -- but confirmation is still required.
        state.stash_mirror_preview("m".into(), MirrorPreview::default());
        let err = mirror_delete_set_for_run(&state, &job, "m", false).unwrap_err();
        assert!(err.contains("mirror job"), "{err}");

        // Confirmed with a fresh stash: the delete set comes out, once.
        state.stash_mirror_preview(
            "m".into(),
            MirrorPreview {
                delete_set: vec![backstar_core::mirror::MirrorDelete {
                    source: "proj".into(),
                    rel: PathBuf::from("doomed.txt"),
                }],
                ..Default::default()
            },
        );
        let set = mirror_delete_set_for_run(&state, &job, "m", true).unwrap().unwrap();
        assert_eq!(set.len(), 1);
        // Single-use: a second attempt without re-previewing is refused.
        assert!(mirror_delete_set_for_run(&state, &job, "m", true).is_err());

        // A snapshot job needs none of this.
        let snap = bare_job("Projects");
        assert!(mirror_delete_set_for_run(&state, &snap, "s", false).unwrap().is_none());
    }

    #[test]
    fn a_stale_preview_does_not_unlock_a_run() {
        let state = AppState::default();
        let job = mirror_job_named("Mirror");
        // Stash one already past the TTL (fields are in-module).
        if let Ok(mut map) = state.mirror_previews.lock() {
            map.insert(
                "m".into(),
                StashedPreview {
                    preview: MirrorPreview::default(),
                    stashed_at: Instant::now() - MIRROR_PREVIEW_TTL - Duration::from_secs(1),
                },
            );
        }
        let err = mirror_delete_set_for_run(&state, &job, "m", true).unwrap_err();
        assert!(err.contains("10 minutes"), "{err}");
    }

    #[test]
    fn saving_or_deleting_a_job_evicts_its_stashed_preview() {
        let state = AppState::default();
        state.stash_mirror_preview("m".into(), MirrorPreview::default());
        state.drop_mirror_preview("m");
        assert!(state.mirror_previews.lock().unwrap().is_empty());
    }

    // ------------------------------------------------------- F43: headless exit codes

    #[test]
    fn headless_exit_codes_distinguish_partial_from_failure() {
        assert_eq!(exit_code_for_outcome(&RunOutcome::Ok), HEADLESS_EXIT_OK);
        assert_eq!(exit_code_for_outcome(&RunOutcome::Partial { failed: 2 }), HEADLESS_EXIT_PARTIAL);
        assert_eq!(
            exit_code_for_outcome(&RunOutcome::Failed { reason: "x".into() }),
            HEADLESS_EXIT_FAILED
        );
        assert_eq!(exit_code_for_outcome(&RunOutcome::Cancelled), HEADLESS_EXIT_FAILED);
        assert_ne!(HEADLESS_EXIT_PARTIAL, HEADLESS_EXIT_OK);
    }

    // ------------------------------------------------------- F15: save_job response shape

    #[test]
    fn save_job_response_carries_the_schedule_outcome_camel_case() {
        let resp = SaveJobResponse {
            job: bare_job("Projects"),
            schedule_applied: false,
            schedule_error: Some("schtasks failed".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"scheduleApplied\":false"), "{json}");
        assert!(json.contains("\"scheduleError\":\"schtasks failed\""), "{json}");
        assert!(json.contains("\"job\":"), "{json}");
        let ok = SaveJobResponse { job: bare_job("P"), schedule_applied: true, schedule_error: None };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"scheduleError\":null"), "{json}");
    }

    // ------------------------------------------------------- L11: default_excludes DTO

    /// The DTO is the engine's canonical sets, unaltered -- this is the parity guarantee
    /// that replaces the deleted hardcoded copy in the UI.
    #[test]
    fn default_excludes_dto_carries_the_engine_sets_unaltered() {
        let dto = default_excludes();
        assert_eq!(dto.project, backstar_core::exclude::ExcludeRules::project());
        assert_eq!(dto.system, backstar_core::exclude::ExcludeRules::system());
        // Spot-check against the raw constants too, so a remapping slipped into the
        // constructors themselves is caught here rather than in the UI.
        assert!(
            dto.project.dir_names.iter().any(|n| n == "node_modules"),
            "the project set is the build/cache folders set"
        );
        assert_eq!(
            dto.project.dir_paths.len(),
            backstar_core::exclude::PROJECT_EXCLUDE_PATHS.len()
        );
        assert!(dto.system.dir_names.iter().any(|n| n == "Cache"));
        assert!(dto.system.file_patterns.iter().any(|p| p == "*.tmp"));
        assert!(
            dto.project.file_patterns.is_empty() && dto.system.dir_paths.is_empty(),
            "the two groups are not interchangeable -- project carries paths, system carries globs"
        );
    }

    /// Wire shape: two camelCase-named groups whose inner rules keep `ExcludeRules`'
    /// snake_case fields -- exactly the shape the UI's `DefaultExcludes`/`ExcludeRules`
    /// types mirror. A rename here would silently break the editor's profile matching.
    #[test]
    fn default_excludes_dto_serialises_as_two_groups_of_snake_case_rules() {
        let json = serde_json::to_value(default_excludes()).unwrap();
        let project = &json["project"];
        let system = &json["system"];
        assert!(project.is_object() && system.is_object(), "{json}");
        for group in [project, system] {
            assert!(group.get("dir_names").is_some(), "{json}");
            assert!(group.get("dir_paths").is_some(), "{json}");
            assert!(group.get("file_patterns").is_some(), "{json}");
            assert!(group.get("dirNames").is_none(), "snake_case, not camelCase: {json}");
        }
    }
}
