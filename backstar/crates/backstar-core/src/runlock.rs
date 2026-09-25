//! Cross-process exclusion for runs against one destination (F3, engine side).
//!
//! Two binaries drive this engine: the installed app (which serialises its own runs
//! in-process through a `Runner` mutex) and the headless `--run-job` mode (a separate
//! process entirely, which the app's single-instance plugin never sees). The GUI running
//! a job at the moment Task Scheduler fires the same job is the common collision, and
//! in-process locks can do nothing about it -- the exclusion has to live on the
//! filesystem, next to the data it protects. Two concurrent runs against one destination
//! would interleave snapshot ids and manifest writes (or, for a mirror, deletions and
//! copies) into a state neither run planned.
//!
//! The lock is a lock FILE, not a named kernel mutex, because a file is observable (you
//! can see it in the folder, and read which pid wrote it) and debuggable (a user can
//! delete it by hand, and the error message can say so). Snapshot runs lock
//! `<repo>/.backstar/run.lock`; mirror runs -- which have no repository by design -- lock
//! `<dest>/.backstar-mirror.lock`.
//!
//! On Windows the file is opened requesting write access while sharing only READ, and
//! HELD OPEN for the whole run, so the kernel itself guarantees both halves of the
//! contract:
//!
//! - a second writer (from any process, or a second guard in the same process) fails its
//!   open with a sharing violation, surfaced as [`Error::RunInProgress`] -- while the pid
//!   banner stays readable, so a held lock is inspectable;
//! - a holder that dies, even a hard kill, has its handle closed by the kernel, so the
//!   lock can never go stale. There is deliberately no pid-liveness/timestamp takeover
//!   machinery: OS-managed handle lifetime makes it unnecessary, and stale-lock takeover
//!   logic is exactly the kind of code that gets a race wrong.
//!
//! The leftover FILE after a crash is not the lock; the open handle is. A later run
//! simply reuses the file (truncate, rewrite the pid banner), and a read-only or
//! ACL-awkward leftover is deleted before the retry -- deleting it succeeds only when no
//! process holds it open, because a live holder grants no DELETE sharing and so the
//! delete fails. The cleanup can therefore never break a running holder.
//!
//! Non-Windows builds (a development convenience; the shipped product is Windows) fall
//! back to create-exclusive semantics: the file's mere existence is the lock, `Drop`
//! removes it on the happy path, and a stale one from a crash must be removed by hand.
//!
//! The guard is NOT re-entrant: a second acquire of the same destination fails even in
//! the same process and thread. The engine takes it exactly once per run, at the top.
//! Restores never take it from the engine side -- the portable restore tool opens
//! repositories read-only and must not create files in them; the installed app may gate
//! its restores on this guard from the shell side, which is why the type is public.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The lock file inside a repository's `.backstar/` metadata directory.
pub const REPO_RUN_LOCK_NAME: &str = "run.lock";

/// The lock file at a mirror destination's root. A mirror has no `.backstar/` directory
/// (it is not a repository -- see `mirror.rs`), so the lock sits beside the mirrored
/// trees. It is never inside a walked destination subfolder, so it can never be
/// enumerated into a deletion plan.
pub const MIRROR_RUN_LOCK_NAME: &str = ".backstar-mirror.lock";

/// How long to keep retrying a contended lock before giving up. Short-lived transient
/// opens of the lock file (an indexer or antivirus scanner reading it) trip the same
/// sharing rule as a real run; riding them out for a moment keeps them from reporting a
/// phantom "run in progress". A genuine holder fails every attempt regardless.
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_ATTEMPTS: u32 = 5;

/// A held run lock for one destination. Exclusion lasts exactly as long as this value:
/// dropping it closes the handle (releasing the lock) and removes the lock file.
///
/// Acquired through [`crate::repo::Repo::acquire_run_lock`] for snapshot repositories or
/// [`RunLock::for_mirror_dest`] for mirror destinations.
#[derive(Debug)]
pub struct RunLock {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl RunLock {
    /// Lock a mirror destination: `<dest>/.backstar-mirror.lock`.
    ///
    /// `dest` must be the CANONICAL spelling of the destination
    /// ([`crate::guards::canonical_path`]) -- `run_mirror` canonicalises before
    /// acquiring, and two spellings of one folder must not produce two lock files.
    pub fn for_mirror_dest(dest: &Path) -> Result<Self> {
        Self::acquire(dest.join(MIRROR_RUN_LOCK_NAME), dest)
    }

    /// The lock file's path on disk, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn acquire(lock_path: PathBuf, subject: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let mut attempt = 0;
        loop {
            attempt += 1;
            match open_exclusive(&lock_path) {
                Ok(mut file) => {
                    // Informational only -- the open HANDLE is the lock, this text is for
                    // whoever goes looking at the file while deciding whether it is safe
                    // to delete. A failure to write it must not cost the lock.
                    let banner = format!(
                        "BackStar run lock -- a backup or mirror of this destination is in \
                         progress.\npid={}\nstarted={}\n",
                        std::process::id(),
                        crate::repo::rfc3339(time::OffsetDateTime::now_utc()),
                    );
                    let _ = file.write_all(banner.as_bytes());
                    return Ok(Self { file: Some(file), path: lock_path });
                }
                Err(e) if !is_contention(&e) => return Err(Error::io(&lock_path, e)),
                Err(e) if attempt >= MAX_ATTEMPTS => {
                    return Err(final_contention_error(e, &lock_path, subject));
                }
                Err(_) => {
                    probe_stale(&lock_path);
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Close the handle FIRST -- that is what releases the exclusion -- then remove
        // the file. Best-effort: a leftover file is harmless (the next run truncates and
        // reuses it), and on Windows nobody else could have deleted it while held,
        // because the holder's sharing mode grants no DELETE access.
        if let Some(file) = self.file.take() {
            drop(file);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open the lock file exclusively. On Windows this creates/truncates it requesting write
/// access while sharing READ only: a second writer -- from any process, or a second guard
/// in this one -- fails the open with a sharing violation, while the pid banner stays
/// readable for anyone inspecting the folder, and the kernel releases the lock
/// automatically when the holder dies. Elsewhere, a create-exclusive open makes the
/// file's existence the lock.
#[cfg(windows)]
fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

#[cfg(not(windows))]
fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(windows)]
fn is_contention(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION (32) and ERROR_LOCK_VIOLATION (33) surface from std as
    // ErrorKind::Uncategorized, so match the raw codes; ERROR_ACCESS_DENIED (5) arrives
    // as PermissionDenied. A 5 can also be a genuine ACL/readonly problem -- the retry
    // loop and `final_contention_error` sort those apart.
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    ) || e.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(not(windows))]
fn is_contention(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists
}

/// What to report after the final retry. On Windows a genuine holder fails the open with
/// a sharing violation (32, or lock violation 33); anything else that survived the
/// retries -- a read-only leftover that refused deletion, an ACL problem -- is an honest
/// io error rather than a phantom contention report. Elsewhere, contention only ever
/// means the lock file exists.
#[cfg(windows)]
fn final_contention_error(e: std::io::Error, lock_path: &Path, subject: &Path) -> Error {
    if matches!(e.raw_os_error(), Some(32) | Some(33)) {
        Error::RunInProgress(subject.to_path_buf())
    } else {
        Error::io(lock_path, e)
    }
}

#[cfg(not(windows))]
fn final_contention_error(_e: std::io::Error, _lock_path: &Path, subject: &Path) -> Error {
    Error::RunInProgress(subject.to_path_buf())
}

/// Best-effort disposal of a stale leftover before retrying. On Windows a LIVE lock
/// cannot be removed -- the holder's sharing mode grants no DELETE access -- so a
/// successful removal is proof the file was crash-leftover junk. A read-only leftover
/// first has the attribute cleared (a run lock inherits nothing, but a hand-copied or
/// restored file can carry it).
#[cfg(windows)]
fn probe_stale(path: &Path) {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.permissions().readonly() {
            let mut perms = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Not on non-Windows: unlinking an open file SUCCEEDS there, so "deletable" proves
/// nothing about whether the lock is live, and probing would break a real holder.
#[cfg(not(windows))]
fn probe_stale(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;

    #[test]
    fn a_second_guard_on_the_same_repo_is_refused_even_in_process() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();

        let first = repo.acquire_run_lock().unwrap();
        let err = repo.acquire_run_lock().unwrap_err();
        assert!(
            matches!(err, Error::RunInProgress(_)),
            "a second guard must report contention clearly, got: {err}"
        );
        assert!(err.to_string().contains("another BackStar run"), "got: {err}");

        drop(first);
        let _second = repo.acquire_run_lock()
            .expect("the lock must be acquirable again once the first guard is dropped");
    }

    /// The GUI and the headless scheduler are separate processes sharing nothing but the
    /// filesystem; two threads stand in for them here (sharing violation checks are
    /// per-handle, so in-process threads exercise the same OS rule).
    #[test]
    fn a_concurrent_thread_cannot_take_a_held_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let first = repo.acquire_run_lock().unwrap();

        let root = tmp.path().to_path_buf();
        let outcome = std::thread::spawn(move || {
            let repo = Repo::open(&root).unwrap();
            repo.acquire_run_lock().map(|_| ())
        })
        .join()
        .unwrap();
        assert!(matches!(outcome, Err(Error::RunInProgress(_))));

        drop(first);
    }

    #[test]
    fn the_lock_file_is_observable_while_held_and_gone_after_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let lock_path = tmp.path().join(crate::repo::META_DIR).join(REPO_RUN_LOCK_NAME);

        let guard = repo.acquire_run_lock().unwrap();
        assert_eq!(guard.path(), lock_path.as_path());
        let content = std::fs::read_to_string(&lock_path)
            .expect("the lock file must exist and be readable while held");
        assert!(content.contains(&std::process::id().to_string()), "got: {content}");

        drop(guard);
        assert!(!lock_path.exists(), "a released lock cleans its file up");
    }

    #[test]
    fn locks_are_per_destination_not_global() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let repo_a = Repo::open_or_init(a.path(), "A").unwrap();
        let repo_b = Repo::open_or_init(b.path(), "B").unwrap();

        let _ga = repo_a.acquire_run_lock().unwrap();
        let _gb = repo_b.acquire_run_lock()
            .expect("a run on one repository must not block a run on another");
    }

    #[test]
    fn mirror_locks_are_per_destination() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let da = crate::guards::canonical_path(a.path().join("out")).unwrap();
        let db = crate::guards::canonical_path(b.path().join("out")).unwrap();

        let _ga = RunLock::for_mirror_dest(&da).unwrap();
        assert!(matches!(
            RunLock::for_mirror_dest(&da),
            Err(Error::RunInProgress(_))
        ));
        let _gb = RunLock::for_mirror_dest(&db).unwrap();
        assert!(a.path().join("out").join(MIRROR_RUN_LOCK_NAME).exists());
    }

    /// A crashed run leaves the lock FILE behind with nobody holding it (the kernel
    /// released the handle when the process died). The next run must reclaim it -- even
    /// if the leftover is read-only -- rather than reporting contention forever.
    /// Windows-only: on other platforms the file's existence IS the lock, and a stale
    /// one is deliberately a manual fix (see module docs).
    #[cfg(windows)]
    #[test]
    fn a_stale_leftover_lock_file_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let lock_path = tmp.path().join(crate::repo::META_DIR).join(REPO_RUN_LOCK_NAME);

        std::fs::write(&lock_path, "pid=1\nstarted=2001-01-01T00:00:00Z\n").unwrap();
        let mut perms = std::fs::metadata(&lock_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&lock_path, perms).unwrap();

        let guard = repo.acquire_run_lock()
            .expect("an orphaned leftover must not block a new run");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&std::process::id().to_string()), "got: {content}");
        drop(guard);
    }
}
