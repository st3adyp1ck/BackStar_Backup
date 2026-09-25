// Release builds use the Windows subsystem so launching the app never flashes a console.
// This is the fix for the .vbs -> .bat -> powershell.exe launcher chain the PowerShell app
// needed for the same purpose, which AV heuristics reasonably treat as a dropper pattern.
//
// A `--headless` scheduled run has no window either way -- it never reaches the point
// where the subsystem choice would matter -- so this attribute only affects the normal
// double-click launch path.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Exit-code contract for `--headless --run-job <id>` (F43 -- Task Scheduler's history
//! shows these):
//!
//! - `0` -- the run succeeded.
//! - `1` -- the run failed or was interrupted, or the job could not be started (usage
//!   errors exit 2, below).
//! - `2` -- usage error (`--headless` without `--run-job <id>`).
//! - `3` -- the run completed but some files failed (engine outcome `Partial`). A
//!   partially-failed scheduled run must not exit 0, or it reads as full success.

/// Initialise logging for a headless run (F43): the GUI logs to stderr, which goes
/// nowhere without a console -- a scheduled run's failures were invisible until now.
/// Append to `backstar-headless.log` under the state dir, restarting the file when it
/// exceeds ~1 MiB so the log cannot grow without bound. Best-effort: if the log cannot
/// be opened, the run still happens, just unlogged (the exit code still tells the truth).
#[cfg(windows)]
fn init_headless_logging() {
    let Ok(dir) = backstar_core::config::state_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("backstar-headless.log");

    let over_cap = std::fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false);
    let file = if over_cap {
        std::fs::File::create(&path)
    } else {
        std::fs::OpenOptions::new().create(true).append(true).open(&path)
    };
    let Ok(file) = file else { return };

    // A File is itself a MakeWriter (writing through &File), so no guard plumbing.
    let _ = tracing_subscriber::fmt()
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}

/// A Task Scheduler run passes `--headless --run-job <id>`: run exactly that one job to
/// completion with no window, no WebView2, and no event loop, then exit with a status code
/// Task Scheduler can see. Anything else falls through to the normal GUI.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--headless") {
        let job_id = args
            .iter()
            .position(|a| a == "--run-job")
            .and_then(|i| args.get(i + 1))
            .cloned();

        let Some(job_id) = job_id else {
            eprintln!("--headless requires --run-job <job-id>");
            std::process::exit(2);
        };

        init_headless_logging();
        std::process::exit(backstar_app_lib::run_headless(&job_id));
    }

    backstar_app_lib::run()
}
