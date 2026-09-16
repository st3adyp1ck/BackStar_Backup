//! Windows Task Scheduler integration for unattended runs.
//!
//! Chosen over the COM `ITaskService` API for its simplicity: a `schtasks.exe` subprocess
//! call needs no COM initialisation or interface bindings, and its effect is trivially
//! inspectable from an elevated prompt (`schtasks /Query /TN \BackStar\<id>`) while
//! debugging. The cost -- a process spawn on every job save -- is paid only on the save
//! path, never on the run path itself.

use std::path::Path;
use std::process::Command;

use backstar_core::config::{DailySchedule, Job};

/// Every BackStar task lives in one Task Scheduler folder, named by job id. This is also
/// how a stale task is found and removed when its job is deleted.
pub fn task_name(job_id: &str) -> String {
    format!(r"\BackStar\{job_id}")
}

/// The command line Task Scheduler will invoke: this same executable, told which job to
/// run and to skip the GUI entirely. Quoted because an install path containing spaces
/// (`C:\Program Files\BackStar\BackStar.exe`) is the expected case, not an edge case.
fn task_run_command(job_id: &str, exe: &Path) -> String {
    format!(r#""{}" --headless --run-job {job_id}"#, exe.display())
}

/// Build the `schtasks /Create` argument list for a job's daily schedule.
///
/// Split out from the actual process spawn so the exact command line is unit-testable
/// without touching the real Task Scheduler.
fn create_args(job_id: &str, schedule: DailySchedule, exe: &Path) -> Vec<String> {
    vec![
        "/Create".into(),
        "/TN".into(),
        task_name(job_id),
        "/TR".into(),
        task_run_command(job_id, exe),
        "/SC".into(),
        "DAILY".into(),
        "/ST".into(),
        format!("{:02}:{:02}", schedule.hour, schedule.minute),
        // Overwrite silently rather than prompting or erroring: `apply_schedule` is called
        // on every save, so "the task already exists" is the normal case, not a conflict.
        "/F".into(),
    ]
}

fn delete_args(job_id: &str) -> Vec<String> {
    vec!["/Delete".into(), "/TN".into(), task_name(job_id), "/F".into()]
}

/// Reflect a job's `schedule` field into Task Scheduler: create or update its task if a
/// schedule is set, remove any existing task if not. Called every time a job is saved, so
/// the scheduled state can never drift from what the job's own config says -- there is no
/// separate "enable scheduling" step to forget.
pub fn apply_schedule(job: &Job, exe: &Path) -> Result<(), String> {
    match job.schedule {
        Some(sched) => run_schtasks(&create_args(&job.id, sched, exe)),
        None => remove_schedule(&job.id),
    }
}

/// Remove a job's scheduled task, if any. Not an error when none exists -- deleting a job
/// that was never scheduled, or saving one that just had its schedule turned off, must not
/// fail just because there was nothing in Task Scheduler to remove.
pub fn remove_schedule(job_id: &str) -> Result<(), String> {
    match run_schtasks(&delete_args(job_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.to_lowercase().contains("cannot find") => Ok(()),
        Err(e) => Err(e),
    }
}

fn run_schtasks(args: &[String]) -> Result<(), String> {
    let output = Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|e| format!("could not run schtasks.exe: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_names_are_namespaced_under_one_folder_by_job_id() {
        assert_eq!(task_name("abc-123"), r"\BackStar\abc-123");
    }

    #[test]
    fn create_args_build_a_daily_trigger_at_the_given_time() {
        let sched = DailySchedule { hour: 6, minute: 5 };
        let args = create_args("job-1", sched, Path::new(r"C:\Program Files\BackStar\BackStar.exe"));

        assert_eq!(args[0], "/Create");
        assert!(args.contains(&r"\BackStar\job-1".to_string()));
        assert!(args.contains(&"DAILY".to_string()));
        assert!(args.contains(&"06:05".to_string()), "minute must be zero-padded: {args:?}");
        assert!(args.contains(&"/F".to_string()), "must overwrite silently on re-save");
    }

    #[test]
    fn the_run_command_quotes_the_exe_path_and_names_the_job() {
        let cmd = task_run_command("job-1", Path::new(r"C:\Program Files\BackStar\BackStar.exe"));
        assert_eq!(
            cmd,
            r#""C:\Program Files\BackStar\BackStar.exe" --headless --run-job job-1"#
        );
    }

    #[test]
    fn delete_args_target_the_same_task_name_create_would_use() {
        let args = delete_args("job-1");
        assert_eq!(args, vec!["/Delete", "/TN", r"\BackStar\job-1", "/F"]);
    }

    #[test]
    fn a_missing_task_on_delete_is_not_an_error() {
        // schtasks' real wording, in case it changes the exact casing.
        let err = "ERROR: The system cannot find the file specified.".to_string();
        assert!(err.to_lowercase().contains("cannot find"));
    }
}
