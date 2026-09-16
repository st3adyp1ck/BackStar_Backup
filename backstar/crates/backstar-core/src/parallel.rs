//! Shared parallel copy execution.
//!
//! Both [`crate::engine::run_job`] (backup: copy-or-link against a plan) and
//! [`crate::restore::restore`] (restore: always copy) need to push a list of files through
//! a bounded thread pool with throttled progress. That logic lives here exactly once --
//! duplicating it risked one path getting a fix (the thread-count tuning, the progress
//! throttle, the camelCase event contract) that the other silently missed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::copy::{copy_file, hard_link, CopyOutcome};
use crate::events::{Event, FileError};

/// One file to place at a destination.
///
/// If `link_from` is set, a hardlink to it is attempted first; only on failure (or when
/// `link_from` is `None`) is `from` actually copied. This is what lets backup's "unchanged"
/// files and restore's "always copy" files share one execution path.
pub(crate) struct CopyItem {
    /// Path relative to the job/restore root, used only for reporting.
    pub rel: PathBuf,
    pub from: PathBuf,
    pub to: PathBuf,
    pub size: u64,
    pub link_from: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub copied: u64,
    pub linked: u64,
    pub failed: u64,
    pub bytes_copied: u64,
    pub bytes_linked: u64,
    pub cancelled: bool,
}

/// How many threads to copy with.
///
/// Measured on a 32-core machine copying 2,888 files (62 MB) across volumes: 4 threads
/// gave 1.02s at 61 MB/s, 8 threads gave 0.81s at 77 MB/s (the best), 16 threads gave 0.86s
/// at 72 MB/s, and 32 threads gave 1.03s at 60 MB/s.
///
/// Copying is IO-bound, so past a point more threads simply contend for the same disk and
/// throughput falls back toward the single-threaded figure. Eight is also what robocopy's
/// /MT:8 default picks, and the PowerShell app passed that on every run.
///
/// BACKSTAR_COPY_THREADS overrides it, mostly so this table can be re-measured on other
/// hardware.
pub(crate) fn copy_threads() -> usize {
    if let Some(n) = std::env::var("BACKSTAR_COPY_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4)
}

/// Copy or link every item in `items`, in parallel, emitting throttled progress.
///
/// Destination parent directories are created serially up front: doing it inside the
/// workers means every thread races to create the same parents, which on Windows surfaces
/// as spurious sharing violations rather than the benign AlreadyExists.
pub(crate) fn run_parallel(
    items: &[CopyItem],
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Counts {
    let files_total = items.len() as u64;
    let bytes_total: u64 = items.iter().map(|i| i.size).sum();

    let copied = AtomicU64::new(0);
    let linked = AtomicU64::new(0);
    let failed = AtomicU64::new(0);
    let bytes_copied = AtomicU64::new(0);
    let bytes_linked = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let cancelled = AtomicBool::new(false);

    let mut dirs: Vec<&Path> = items.iter().filter_map(|i| i.to.parent()).collect();
    dirs.sort_unstable();
    dirs.dedup();
    for d in dirs {
        let _ = std::fs::create_dir_all(d);
    }

    let (tx, rx) = crossbeam_channel::unbounded::<Event>();

    // Bind shared references explicitly: the worker closure is `move`, so without these it
    // would take the counters by value and leave nothing to read afterwards.
    let (copied, linked, failed) = (&copied, &linked, &failed);
    let (bytes_copied, bytes_linked) = (&bytes_copied, &bytes_linked);
    let (done, cancelled) = (&done, &cancelled);

    // A dedicated pool rather than rayon's global one, which sizes itself to the core count
    // and is therefore badly wrong for disk IO on a many-core machine. See copy_threads().
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(copy_threads())
        .thread_name(|i| format!("backstar-copy-{i}"))
        .build()
        .ok();

    std::thread::scope(|scope| {
        let worker_tx = tx.clone();
        scope.spawn(move || {
            let work = || {
                items.par_iter().for_each(|item| {
                    if cancel.load(Ordering::Relaxed) {
                        cancelled.store(true, Ordering::Relaxed);
                        return;
                    }

                    // A link that fails falls back to a copy: NTFS caps a file at 1024
                    // links, and a destination can turn out not to support them at all.
                    let did_link = item
                        .link_from
                        .as_ref()
                        .map(|existing| hard_link(existing, &item.to).is_ok())
                        .unwrap_or(false);

                    if did_link {
                        linked.fetch_add(1, Ordering::Relaxed);
                        bytes_linked.fetch_add(item.size, Ordering::Relaxed);
                    } else {
                        match copy_file(&item.from, &item.to, cancel, |_, _| {}) {
                            Ok(CopyOutcome::Copied { bytes }) => {
                                copied.fetch_add(1, Ordering::Relaxed);
                                bytes_copied.fetch_add(bytes, Ordering::Relaxed);
                            }
                            Ok(CopyOutcome::Cancelled) => {
                                cancelled.store(true, Ordering::Relaxed);
                                return;
                            }
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                let _ = worker_tx.send(Event::FileFailed(FileError {
                                    path: item.rel.clone(),
                                    message: e.to_string(),
                                }));
                            }
                        }
                    }

                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    // Throttle: one progress event per 64 files, plus the last one. At
                    // 224,000 files that is ~3,500 messages instead of ~450,000.
                    if n % 64 == 0 || n == files_total {
                        let _ = worker_tx.send(Event::OverallProgress {
                            files_done: n,
                            files_total,
                            bytes_done: bytes_copied.load(Ordering::Relaxed),
                            bytes_total,
                            current: Some(item.rel.clone()),
                        });
                    }
                });
            };

            match &pool {
                Some(p) => p.install(work),
                // If the pool could not be built, fall back to rayon's global pool rather
                // than failing the run. Slower on a many-core box, but correct.
                None => work(),
            }
        });

        // Drop this thread's sender so the loop below ends once the workers finish.
        drop(tx);
        while let Ok(ev) = rx.recv() {
            emit(ev);
        }
    });

    Counts {
        copied: copied.load(Ordering::Relaxed),
        linked: linked.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        bytes_copied: bytes_copied.load(Ordering::Relaxed),
        bytes_linked: bytes_linked.load(Ordering::Relaxed),
        cancelled: cancelled.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    #[test]
    fn copies_items_with_no_link_source() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        touch(&src, "hello");
        let dest = tmp.path().join("out/a.txt");

        let items = vec![CopyItem {
            rel: PathBuf::from("a.txt"),
            from: src,
            to: dest.clone(),
            size: 5,
            link_from: None,
        }];

        let mut events = Vec::new();
        let counts = run_parallel(&items, &AtomicBool::new(false), &mut |e| events.push(e));

        assert_eq!(counts.copied, 1);
        assert_eq!(counts.linked, 0);
        assert_eq!(counts.bytes_copied, 5);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[test]
    fn links_when_a_link_source_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("prev/a.txt");
        touch(&existing, "shared");
        let dest = tmp.path().join("new/a.txt");

        let items = vec![CopyItem {
            rel: PathBuf::from("a.txt"),
            from: tmp.path().join("irrelevant.txt"),
            to: dest.clone(),
            size: 6,
            link_from: Some(existing.clone()),
        }];

        let counts = run_parallel(&items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(counts.linked, 1);
        assert_eq!(counts.copied, 0);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "shared");
    }

    /// A link source that no longer exists (or a destination that rejects links) must fall
    /// back to a real copy rather than losing the file.
    #[test]
    fn a_failed_link_falls_back_to_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_link_source = tmp.path().join("does-not-exist.txt");
        let real_source = tmp.path().join("real.txt");
        touch(&real_source, "fallback content");
        let dest = tmp.path().join("out/a.txt");

        let items = vec![CopyItem {
            rel: PathBuf::from("a.txt"),
            from: real_source,
            to: dest.clone(),
            size: 17,
            link_from: Some(missing_link_source),
        }];

        let counts = run_parallel(&items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(counts.linked, 0, "the link must have failed");
        assert_eq!(counts.copied, 1, "and fallen back to a copy");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "fallback content");
    }

    #[test]
    fn a_missing_source_is_reported_as_failed_not_silently_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let items = vec![CopyItem {
            rel: PathBuf::from("nope.txt"),
            from: tmp.path().join("nope.txt"),
            to: tmp.path().join("out/nope.txt"),
            size: 0,
            link_from: None,
        }];

        let mut events = Vec::new();
        let counts = run_parallel(&items, &AtomicBool::new(false), &mut |e| events.push(e));
        assert_eq!(counts.failed, 1);
        assert!(events.iter().any(|e| matches!(e, Event::FileFailed(_))));
    }

    #[test]
    fn progress_is_emitted_and_reaches_the_final_count() {
        let tmp = tempfile::tempdir().unwrap();
        let items: Vec<CopyItem> = (0..200)
            .map(|i| {
                let src = tmp.path().join(format!("src/{i}.txt"));
                touch(&src, "x");
                CopyItem {
                    rel: PathBuf::from(format!("{i}.txt")),
                    from: src,
                    to: tmp.path().join(format!("out/{i}.txt")),
                    size: 1,
                    link_from: None,
                }
            })
            .collect();

        let mut last_progress = None;
        run_parallel(&items, &AtomicBool::new(false), &mut |e| {
            if let Event::OverallProgress { files_done, files_total, .. } = e {
                last_progress = Some((files_done, files_total));
            }
        });

        let (done, total) = last_progress.expect("at least one progress event");
        assert_eq!(done, 200);
        assert_eq!(total, 200);
    }
}
