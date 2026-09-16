//! Bridges the engine (and restore) to the window.
//!
//! Both a backup run and a restore are synchronous, callback-driven operations on
//! `backstar-core`'s side, so each happens on its own thread and every event is forwarded
//! to the webview. Nothing blocks the UI thread -- which is the structural fix for the
//! PowerShell app, where a `WinForms.Timer` on the UI thread did file IO and
//! regular-expression parsing on every tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use backstar_core::config::{Config, Job};
use backstar_core::events::Event;

/// The channel name the frontend listens on.
pub const EVENT_CHANNEL: &str = "backstar://event";

/// One operation at a time, app-wide -- covering both backup runs and restores.
///
/// The PowerShell app had no such interlock: two concurrent runs writing to one destination
/// was reachable by clicking twice, and the second run replaced the shared run-state object
/// wholesale, orphaning the first robocopy process while it was still deleting. Restore
/// shares the same slot deliberately: restoring from a snapshot while a backup into the
/// same repository is mid-write (creating that very snapshot, or pruning an old one) is the
/// same class of hazard.
#[derive(Default)]
pub struct Runner {
    inner: Arc<Mutex<Option<Active>>>,
}

struct Active {
    label: String,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunHandle {
    pub label: String,
    pub started: bool,
}

impl Runner {
    pub fn running_label(&self) -> Option<String> {
        self.inner.lock().ok()?.as_ref().map(|a| a.label.clone())
    }

    /// Request that the current operation stop. Safe to call when nothing is running.
    pub fn cancel(&self) -> bool {
        let Ok(guard) = self.inner.lock() else { return false };
        match guard.as_ref() {
            Some(active) => {
                active.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Run `work` on a background thread, sending every event it emits to `sink`.
    ///
    /// `work` receives the cancel flag and an emit callback, and is responsible for its own
    /// domain logic entirely -- calling into `engine::run_job`, `restore::restore`, writing
    /// history, whatever the caller needs. Keeping `Runner` ignorant of what it is running
    /// is what let restore reuse this same single-flight slot and thread plumbing without
    /// `Runner` needing to know backup and restore even exist.
    ///
    /// Taking a sink rather than an `AppHandle` keeps this independent of Tauri, which is
    /// what makes it testable without a window.
    ///
    /// Returns an error rather than queueing if an operation is already in flight.
    pub fn start<W, S>(
        &self,
        label: String,
        work: W,
        sink: S,
        on_finished: impl FnOnce() + Send + 'static,
    ) -> Result<RunHandle, String>
    where
        W: FnOnce(&AtomicBool, &mut dyn FnMut(Event)) + Send + 'static,
        S: Fn(Event) + Send + 'static,
    {
        let mut guard = self.inner.lock().map_err(|_| "runner lock poisoned".to_string())?;
        if guard.is_some() {
            return Err("a backup or restore is already running".into());
        }

        let cancel = Arc::new(AtomicBool::new(false));
        *guard = Some(Active { label: label.clone(), cancel: cancel.clone() });
        drop(guard);

        let slot = self.inner.clone();

        let handle = std::thread::Builder::new()
            .name("backstar-run".into())
            .spawn(move || {
                work(&cancel, &mut |ev: Event| sink(ev));

                // Clear the slot however the work ended, including on panic-free error
                // paths inside `work`. Leaving it set would wedge the app into "already
                // running" forever.
                if let Ok(mut g) = slot.lock() {
                    *g = None;
                }
                on_finished();
            })
            .map_err(|e| format!("could not start the run thread: {e}"))?;

        // Not joined: the caller returns to the UI immediately. Dropping the handle
        // detaches the thread, which is what we want.
        drop(handle);

        Ok(RunHandle { label, started: true })
    }
}

/// Look up a job by id in the saved configuration.
pub fn find_job(cfg: &Config, job_id: &str) -> Option<Job> {
    cfg.jobs.iter().find(|j| j.id == job_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use backstar_core::config::{JobKind, SCHEMA_VERSION};
    use backstar_core::engine;
    use backstar_core::events::RunOutcome;
    use backstar_core::exclude::ExcludeRules;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    fn job_at(src: PathBuf, dest: PathBuf) -> Job {
        Job {
            id: "projects".into(),
            name: "Projects".into(),
            sources: vec![src],
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

    /// The real shape callers use: wrap `engine::run_job` as the work closure.
    fn start_backup(
        runner: &Runner,
        job: Job,
        sink: impl Fn(Event) + Send + 'static,
        on_finished: impl FnOnce() + Send + 'static,
    ) -> Result<RunHandle, String> {
        let label = job.name.clone();
        runner.start(
            label,
            move |cancel, emit| {
                if let Err(e) = engine::run_job(&job, cancel, emit) {
                    emit(Event::RunDone {
                        run_id: String::new(),
                        outcome: RunOutcome::Failed { reason: e.to_string() },
                        stats: Default::default(),
                    });
                }
            },
            sink,
            on_finished,
        )
    }

    /// The full path a click takes: start a run, receive events, land a snapshot -- with no
    /// window involved.
    #[test]
    fn a_started_run_emits_events_and_produces_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        touch(&src.join("a.txt"), "aaa");
        touch(&src.join("b.txt"), "bbb");
        let dest = tmp.path().join("backup");

        let runner = Runner::default();
        let (tx, rx) = mpsc::channel::<Event>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let handle = start_backup(
            &runner,
            job_at(src, dest.clone()),
            move |ev| {
                let _ = tx.send(ev);
            },
            move || {
                let _ = done_tx.send(());
            },
        )
        .expect("run should start");

        assert_eq!(handle.label, "Projects");
        assert!(handle.started);

        done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("run should finish");

        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            events.iter().any(|e| matches!(e, Event::RunStarted { .. })),
            "the UI must learn the run began"
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::PlanReady { .. })),
            "the UI must get a denominator before the bar can mean anything"
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::RunDone { .. })),
            "the UI must learn the run ended, or the Cancel button never becomes Done"
        );

        let snaps = std::fs::read_dir(dest.join("snapshots")).unwrap().count();
        assert_eq!(snaps, 1);
    }

    /// The interlock the PowerShell app lacked. Two concurrent runs at one destination were
    /// reachable by clicking twice, and the second replaced the shared run state outright,
    /// orphaning the first robocopy process while it was still deleting.
    #[test]
    fn a_second_run_is_refused_while_one_is_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        // Enough files that the first run is still going when the second is attempted.
        for i in 0..2000 {
            touch(&src.join(format!("f{i}.txt")), "some content here");
        }
        let dest = tmp.path().join("backup");

        let runner = Runner::default();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        start_backup(&runner, job_at(src.clone(), dest.clone()), |_| {}, move || {
            let _ = done_tx.send(());
        })
        .expect("first run starts");

        let second = start_backup(&runner, job_at(src, dest), |_| {}, || {});
        assert!(second.is_err(), "a second concurrent run must be refused");
        assert!(second.unwrap_err().contains("already running"));

        done_rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap();
    }

    /// A restore attempted while a backup is in flight must also be refused -- they share
    /// one slot on purpose.
    #[test]
    fn a_restore_is_refused_while_a_backup_is_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        for i in 0..2000 {
            touch(&src.join(format!("f{i}.txt")), "content");
        }
        let dest = tmp.path().join("backup");

        let runner = Runner::default();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        start_backup(&runner, job_at(src, dest), |_| {}, move || {
            let _ = done_tx.send(());
        })
        .unwrap();

        let restore_attempt = runner.start("Restore".into(), |_, _| {}, |_| {}, || {});
        assert!(restore_attempt.is_err());

        done_rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap();
    }

    /// Whatever happens, the slot must clear -- otherwise the app wedges into
    /// "already running" for the rest of the session.
    #[test]
    fn the_slot_clears_after_a_run_that_fails_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Runner::default();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        // A source that does not exist makes the engine refuse the job outright.
        let bad = job_at(tmp.path().join("nope"), tmp.path().join("backup"));

        start_backup(&runner, bad, |_| {}, move || {
            let _ = done_tx.send(());
        })
        .expect("start itself succeeds; the failure happens on the thread");

        done_rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        assert_eq!(runner.running_label(), None, "the slot must be free again");
    }

    #[test]
    fn a_failed_run_still_tells_the_ui_it_ended() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Runner::default();
        let (tx, rx) = mpsc::channel::<Event>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let bad = job_at(tmp.path().join("nope"), tmp.path().join("backup"));
        start_backup(
            &runner,
            bad,
            move |ev| {
                let _ = tx.send(ev);
            },
            move || {
                let _ = done_tx.send(());
            },
        )
        .unwrap();

        done_rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        let failed = events
            .iter()
            .any(|e| matches!(e, Event::RunDone { outcome: RunOutcome::Failed { .. }, .. }));
        assert!(failed, "a run that could not start must still emit RunDone, got: {events:?}");
    }

    #[test]
    fn cancel_on_an_idle_runner_is_harmless() {
        let runner = Runner::default();
        assert!(!runner.cancel());
        assert_eq!(runner.running_label(), None);
    }

    #[test]
    fn jobs_are_found_by_id() {
        let mut cfg = Config { schema_version: SCHEMA_VERSION, ..Default::default() };
        cfg.jobs.push(job_at(PathBuf::from(r"D:\a"), PathBuf::from(r"D:\b")));
        assert!(find_job(&cfg, "projects").is_some());
        assert!(find_job(&cfg, "nope").is_none());
    }
}
