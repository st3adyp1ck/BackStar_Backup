//! Windows Task Scheduler integration for unattended runs.
//!
//! Chosen over the COM `ITaskService` API for its simplicity: a `schtasks.exe` subprocess
//! call needs no COM initialisation or interface bindings, and its effect is trivially
//! inspectable from an elevated prompt (`schtasks /Query /TN \BackStar\<id>`) while
//! debugging. The cost -- a process spawn on every job save -- is paid only on the save
//! path, never on the run path itself.

use std::path::{Path, PathBuf};
use std::process::Command;

use backstar_core::config::{DailySchedule, Job};

/// Every BackStar task lives in one Task Scheduler folder, named by job id. This is also
/// how a stale task is found and removed when its job is deleted.
pub fn task_name(job_id: &str) -> String {
    format!(r"\BackStar\{job_id}")
}

/// The command line Task Scheduler will invoke: this same executable, told which job to
/// run and to skip the GUI entirely. Quoted because an install path containing spaces
/// (`C:\Program Files\BackStar\BackStar.exe`) is the expected case, not an edge case --
/// and the job id is quoted too (F45): ids are uuid-shaped today, but an unquoted id
/// containing a space would silently split the argument list in two.
fn task_run_command(job_id: &str, exe: &Path) -> String {
    format!(r#""{}" --headless --run-job "{job_id}""#, exe.display())
}

/// XML-escape a value interpolated into the task definition (F41): the exe path and job
/// id arrive quoted, and a bare `&`/`<`/`>` would make the document unparseable.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The task definition as Task Scheduler XML (F41). schtasks' `/Create` argument list
/// cannot express `StartWhenAvailable` -- the setting that catches a missed daily run up
/// when the machine was off, which is the whole reason scheduled backups exist. The XML
/// definition can, so creation goes through `/Create /XML` (falling back to the arg form
/// with a warning if the XML path fails).
///
/// `StartBoundary` is deliberately a fixed past date: the trigger is the daily TIME, and
/// a boundary in the past with `StartWhenAvailable` set means "overdue runs fire as soon
/// as the machine is back", never "not yet active".
fn task_xml(job_id: &str, schedule: DailySchedule, exe: &Path) -> String {
    let time = format!("{:02}:{:02}:00", schedule.hour, schedule.minute);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>BackStar scheduled backup</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T{time}</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
  </Settings>
  <Actions>
    <Exec>
      <Command>{}</Command>
      <Arguments>--headless --run-job &quot;{}&quot;</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        xml_escape(&exe.display().to_string()),
        xml_escape(job_id)
    )
}

/// Write the task XML where schtasks can read it: a pid-unique temp file under the state
/// dir (the same uniqueness discipline as the engine's atomic writes -- two instances
/// saving jobs at once must not share one scratch file).
fn write_task_xml(xml: &str) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = backstar_core::config::state_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("task-{}-{n}.xml", std::process::id()));
    // schtasks /XML requires a Unicode (UTF-16 LE) file.
    let mut wide: Vec<u8> = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        wide.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, wide).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Build the `schtasks /Create` argument list for a job's daily schedule -- the fallback
/// when the XML path fails (F41: degraded, and the warning says so).
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

/// Create or update the task from an XML definition (the only way to get
/// StartWhenAvailable), falling back to the arg form if the XML route fails.
fn create_task(job_id: &str, schedule: DailySchedule, exe: &Path) -> Result<(), String> {
    match write_task_xml(&task_xml(job_id, schedule, exe)) {
        Ok(xml_path) => {
            let result = run_schtasks(&[
                "/Create".into(),
                "/XML".into(),
                xml_path.display().to_string(),
                "/TN".into(),
                task_name(job_id),
                "/F".into(),
            ]);
            let _ = std::fs::remove_file(&xml_path);
            match result {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!(
                        "XML task creation failed ({e}); falling back to the arg form, \
                         which cannot express StartWhenAvailable"
                    );
                    run_schtasks(&create_args(job_id, schedule, exe))
                }
            }
        }
        Err(e) => {
            tracing::warn!("could not stage the task XML ({e}); falling back to the arg form");
            run_schtasks(&create_args(job_id, schedule, exe))
        }
    }
}

/// Whether the job's task exists in Task Scheduler -- locale-independent (L19), keyed
/// purely on the schtasks exit code.
pub fn task_exists(job_id: &str) -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", &task_name(job_id)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reflect a job's `schedule` field into Task Scheduler: create or update its task if a
/// schedule is set, remove any existing task if not. Called every time a job is saved, so
/// the scheduled state can never drift from what the job's own config says -- there is no
/// separate "enable scheduling" step to forget.
pub fn apply_schedule(job: &Job, exe: &Path) -> Result<(), String> {
    match job.schedule {
        Some(sched) => create_task(&job.id, sched, exe),
        None => remove_schedule(&job.id),
    }
}

/// Remove a job's scheduled task, if any. Not an error when none exists -- deleting a job
/// that was never scheduled, or saving one that just had its schedule turned off, must not
/// fail just because there was nothing in Task Scheduler to remove.
///
/// L19: query-first, keyed on exit codes -- the old version matched an English error
/// string in schtasks' stderr, which a non-English Windows renders differently.
pub fn remove_schedule(job_id: &str) -> Result<(), String> {
    if !task_exists(job_id) {
        return Ok(());
    }
    run_schtasks(&delete_args(job_id))
}

fn delete_args(job_id: &str) -> Vec<String> {
    vec!["/Delete".into(), "/TN".into(), task_name(job_id), "/F".into()]
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

/// F15 startup reconciliation: for every enabled scheduled job, verify its task exists
/// and recreate it once if missing. Returns notices for the ones that could not be
/// recreated (the UI banners them; re-saving the job retries).
///
/// The schtasks calls are injected seams so the logic is testable without touching the
/// real Task Scheduler.
pub fn reconcile_schedules(
    jobs: &[Job],
    task_exists: &dyn Fn(&str) -> bool,
    apply: &dyn Fn(&Job) -> Result<(), String>,
) -> Vec<String> {
    let mut notices = Vec::new();
    for job in jobs.iter().filter(|j| j.enabled && j.schedule.is_some()) {
        if !task_exists(&job.id) {
            match apply(job) {
                Ok(()) => {
                    tracing::info!("recreated the missing scheduled task for {:?}", job.name)
                }
                Err(e) => notices.push(format!(
                    "The scheduled run for {:?} is missing and could not be recreated: {e}. \
                     Re-save the job to retry.",
                    job.name
                )),
            }
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use backstar_core::exclude::ExcludeRules;
    use std::path::PathBuf;

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
            r#""C:\Program Files\BackStar\BackStar.exe" --headless --run-job "job-1""#
        );
    }

    /// F45: an id containing a space must not be able to split the scheduled command line
    /// in two -- quoting keeps it one argument. (`save_job` rejects such ids outright;
    /// this is the belt to that brace.)
    #[test]
    fn the_job_id_is_quoted_in_the_run_command() {
        let cmd = task_run_command("weird id", Path::new(r"C:\x\BackStar.exe"));
        assert_eq!(cmd, r#""C:\x\BackStar.exe" --headless --run-job "weird id""#);
    }

    #[test]
    fn delete_args_target_the_same_task_name_create_would_use() {
        let args = delete_args("job-1");
        assert_eq!(args, vec!["/Delete", "/TN", r"\BackStar\job-1", "/F"]);
    }

    // ------------------------------------------------------- F41: XML task definition

    /// The XML definition is the only way to express StartWhenAvailable, so it is the
    /// primary creation path. The template must be well-formed by construction, carry the
    /// schedule, and escape anything interpolated.
    #[test]
    fn task_xml_carries_the_schedule_and_start_when_available() {
        let xml = task_xml("job-1", DailySchedule { hour: 6, minute: 5 }, Path::new(r"C:\Program Files\BackStar\BackStar.exe"));
        assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"), "{xml}");
        assert!(xml.contains("<StartBoundary>2000-01-01T06:05:00</StartBoundary>"), "{xml}");
        assert!(xml.contains("<ScheduleByDay>"), "{xml}");
        assert!(xml.contains(r"<Command>C:\Program Files\BackStar\BackStar.exe</Command>"), "{xml}");
        assert!(xml.contains("<Arguments>--headless --run-job &quot;job-1&quot;</Arguments>"), "{xml}");
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"), "{xml}");
        assert!(xml.contains(r#"xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task""#), "{xml}");
    }

    #[test]
    fn task_xml_escapes_interpolated_values() {
        let xml = task_xml("a&<b>", DailySchedule { hour: 1, minute: 2 }, Path::new(r"C:\A&B\x.exe"));
        assert!(xml.contains("<Arguments>--headless --run-job &quot;a&amp;&lt;b&gt;&quot;</Arguments>"), "{xml}");
        assert!(xml.contains(r"<Command>C:\A&amp;B\x.exe</Command>"), "{xml}");
        assert!(!xml.contains("A&B"), "{xml}");
    }

    #[test]
    fn xml_escape_covers_all_five_xml_metacharacters() {
        assert_eq!(xml_escape(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(xml_escape("plain"), "plain");
    }

    // ------------------------------------------------------- F15: reconciliation

    fn scheduled_job(id: &str, enabled: bool) -> Job {
        Job {
            id: id.into(),
            name: id.into(),
            sources: vec![],
            dest: PathBuf::from(r"D:\d"),
            dest_portable: None,
            kind: backstar_core::config::JobKind::Snapshot,
            excludes: ExcludeRules::none(),
            presets: vec![],
            git_gc: false,
            enabled,
            schedule: Some(DailySchedule { hour: 6, minute: 0 }),
        }
    }

    #[test]
    fn reconciliation_recreates_missing_tasks_once_and_reports_failures() {
        let jobs = vec![scheduled_job("has-task", true), scheduled_job("missing-ok", true), scheduled_job("missing-bad", true)];

        let exists = |id: &str| id == "has-task";
        let applied = std::cell::RefCell::new(Vec::new());
        let apply = |job: &Job| {
            applied.borrow_mut().push(job.id.clone());
            if job.id == "missing-bad" { Err("access denied".to_string()) } else { Ok(()) }
        };

        let notices = reconcile_schedules(&jobs, &exists, &apply);

        assert_eq!(*applied.borrow(), vec!["missing-ok".to_string(), "missing-bad".to_string()],
            "only missing tasks are recreated, each exactly once");
        assert_eq!(notices.len(), 1, "the failure becomes a notice");
        assert!(notices[0].contains("missing-bad"), "{}", notices[0]);
        assert!(notices[0].contains("access denied"), "{}", notices[0]);
        assert!(notices[0].contains("Re-save the job"), "{}", notices[0]);
    }

    #[test]
    fn reconciliation_skips_disabled_and_unscheduled_jobs() {
        let mut jobs = vec![scheduled_job("off", false), scheduled_job("no-sched", true)];
        jobs[1].schedule = None;
        let applied = std::cell::RefCell::new(Vec::new());
        let notices = reconcile_schedules(&jobs, &|_| false, &|j| {
            applied.borrow_mut().push(j.id.clone());
            Ok(())
        });
        assert!(notices.is_empty());
        assert!(applied.borrow().is_empty(), "disabled or unscheduled jobs are untouched");
    }
}
