// Release builds use the Windows subsystem so launching the app never flashes a console.
// This is the fix for the .vbs -> .bat -> powershell.exe launcher chain the PowerShell app
// needed for the same purpose, which AV heuristics reasonably treat as a dropper pattern.
//
// A `--headless` scheduled run has no window either way -- it never reaches the point
// where the subsystem choice would matter -- so this attribute only affects the normal
// double-click launch path.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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

        match backstar_app_lib::run_headless(&job_id) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("scheduled run failed: {e}");
                std::process::exit(1);
            }
        }
    }

    backstar_app_lib::run()
}
