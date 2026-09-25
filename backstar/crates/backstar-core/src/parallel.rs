//! Shared parallel copy execution.
//!
//! Both [`crate::engine::run_job`] (backup: copy-or-link against a plan) and
//! [`crate::restore::restore`] (restore: always copy) need to push a list of files through
//! a bounded thread pool with throttled progress. That logic lives here exactly once --
//! duplicating it risked one path getting a fix (the thread-count tuning, the progress
//! throttle, the camelCase event contract) that the other silently missed.
//!
//! Two more behaviors live here for the same reason:
//!
//! - **Content verification (D1).** When enabled (backups and mirrors; restores opt out --
//!   their truth is the snapshot bytes themselves), every copied file's SOURCE is hashed
//!   with blake3 immediately after its copy completes (`CopyFileExW` does the actual read
//!   in the kernel, so "hash-on-copy" means this one extra read per changed file), and a
//!   second pass then re-reads each destination file and compares. A mismatch -- or a
//!   destination that will not re-read -- is a failure: the bad copy is removed so the next
//!   run cannot hardlink it into a new snapshot or keep it as the live mirror file, it is
//!   counted in `files_failed`, reported via `FileFailed`, and the phase ends with
//!   `VerifyDone`. Linked files are never hashed or re-read: a hardlink is the same inode
//!   whose content was verified by the run that first copied it.
//! - **Byte-level progress (F34).** `FileStarted`/`FileProgress`/`FileDone` bracket every
//!   copy, and `OverallProgress` is emitted both every 64 completed files and
//!   time-throttled during long copies with in-flight bytes included in `bytes_done`, so a
//!   three-file 20 GB job shows a moving bytes bar instead of sitting at 0% until the
//!   first file lands. Throttling is producer-side, per the shared last-emission stamp:
//!   the crossbeam channel is unbounded, so without it eight workers could flood it with
//!   one event per copy chunk.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

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
    /// Files copied this phase, with their source-content hashes (D1). Empty unless
    /// verification was on. Verify failures are removed from this list -- they count as
    /// failed, not copied -- so the engine can store it in the manifest as-is.
    pub copied_files: Vec<CopiedFile>,
}

/// A file copied this phase, with the blake3 hex digest of its source content at copy
/// time.
#[derive(Debug)]
pub(crate) struct CopiedFile {
    pub rel: PathBuf,
    /// The destination path -- what the verify pass re-reads.
    pub to: PathBuf,
    pub bytes: u64,
    pub hash: String,
}

/// How a copy phase behaves beyond the copying itself.
pub(crate) struct ParallelOptions {
    /// D1: hash each copied file's source and re-verify each destination against it in a
    /// post-copy pass.
    pub verify: bool,
    /// Producer-side minimum interval between byte-level progress events (F34). Tests
    /// set `ZERO` to make every copy-callback chunk emit deterministically.
    pub progress_interval: std::time::Duration,
    /// Test seam for the D1 corruption test: runs against a copied file's destination
    /// between the copy and the verify pass. Production is always `None`.
    pub tamper_dest_after_copy: Option<fn(&Path)>,
}

impl ParallelOptions {
    /// Backups and mirrors: verified, throttled.
    pub(crate) fn verifying() -> Self {
        Self {
            verify: true,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            tamper_dest_after_copy: None,
        }
    }

    /// Restores: throttled progress, no verification.
    pub(crate) fn unverified() -> Self {
        Self {
            verify: false,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            tamper_dest_after_copy: None,
        }
    }
}

/// The production byte-progress throttle: ~10 events/s at most across all workers.
pub(crate) const DEFAULT_PROGRESS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

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

/// Copy or link every item in `items`, in parallel, emitting throttled progress, and --
/// when `options.verify` -- hashing and re-verifying each copied file (D1).
///
/// Destination parent directories are created serially up front: doing it inside the
/// workers means every thread races to create the same parents, which on Windows surfaces
/// as spurious sharing violations rather than the benign AlreadyExists.
pub(crate) fn run_parallel(
    items: &[CopyItem],
    options: &ParallelOptions,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Counts {
    let files_total = items.len() as u64;
    let bytes_total: u64 = items.iter().map(|i| i.size).sum();
    let run_started = Instant::now();

    let copied = AtomicU64::new(0);
    let linked = AtomicU64::new(0);
    let failed = AtomicU64::new(0);
    let bytes_copied = AtomicU64::new(0);
    let bytes_linked = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let cancelled = AtomicBool::new(false);
    // Bytes already written for files still in flight -- the live part of the bytes bar
    // (F34). Completed files are counted in `bytes_copied`.
    let bytes_in_flight = AtomicU64::new(0);
    // Last byte-level progress emission, in millis since phase start. One shared stamp
    // across workers, so N workers together cannot exceed the throttle rate.
    let last_byte_progress = AtomicU64::new(0);
    let copied_files: Mutex<Vec<CopiedFile>> = Mutex::new(Vec::new());

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
    let (bytes_in_flight, last_byte_progress) = (&bytes_in_flight, &last_byte_progress);
    let copied_files_ref = &copied_files;

    // A dedicated pool rather than rayon's global one, which sizes itself to the core count
    // and is therefore badly wrong for disk IO on a many-core machine. See copy_threads().
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(copy_threads())
        .thread_name(|i| format!("backstar-copy-{i}"))
        .build()
        .ok();

    let interval_ms = options.progress_interval.as_millis() as u64;

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
                        // F34: per-file events bracket every copy; byte progress is
                        // throttled at the producer against one shared stamp.
                        let _ = worker_tx.send(Event::FileStarted {
                            path: item.rel.clone(),
                            bytes: item.size,
                        });
                        let mut file_done = 0u64;
                        let outcome = copy_file(&item.from, &item.to, cancel, |done_b, total| {
                            let delta = done_b.saturating_sub(file_done);
                            file_done = done_b;
                            if delta > 0 {
                                bytes_in_flight.fetch_add(delta, Ordering::Relaxed);
                            }
                            let final_chunk = done_b >= total;
                            let now_ms = run_started.elapsed().as_millis() as u64;
                            let prev = last_byte_progress.load(Ordering::Relaxed);
                            if !final_chunk {
                                if now_ms.saturating_sub(prev) < interval_ms {
                                    return;
                                }
                                if last_byte_progress
                                    .compare_exchange(
                                        prev,
                                        now_ms,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    )
                                    .is_err()
                                {
                                    return; // another worker emitted just now
                                }
                            }
                            let _ = worker_tx.send(Event::FileProgress {
                                path: item.rel.clone(),
                                done: done_b,
                                total,
                            });
                            let _ = worker_tx.send(Event::OverallProgress {
                                files_done: done.load(Ordering::Relaxed),
                                files_total,
                                bytes_done: bytes_copied.load(Ordering::Relaxed)
                                    + bytes_in_flight.load(Ordering::Relaxed),
                                bytes_total,
                                current: Some(item.rel.clone()),
                            });
                        });
                        bytes_in_flight.fetch_sub(file_done, Ordering::Relaxed);

                        match outcome {
                            Ok(CopyOutcome::Copied { bytes }) if options.verify => {
                                // D1: hash the source right after its copy completed -- the
                                // kernel did the copy's own read, so this is the
                                // "hash-on-copy" read. A source that cannot be hashed
                                // cannot be verified: count it failed and remove the
                                // destination copy, because an unverified file must never
                                // become a link parent or a live mirror byte.
                                match crate::copy::blake3_hex(&item.from) {
                                    Ok(hash) => {
                                        if let Some(tamper) = options.tamper_dest_after_copy {
                                            tamper(&item.to);
                                        }
                                        copied.fetch_add(1, Ordering::Relaxed);
                                        bytes_copied.fetch_add(bytes, Ordering::Relaxed);
                                        copied_files_ref.lock().unwrap().push(CopiedFile {
                                            rel: item.rel.clone(),
                                            to: item.to.clone(),
                                            bytes,
                                            hash: hash.clone(),
                                        });
                                        let _ = worker_tx.send(Event::FileDone {
                                            path: item.rel.clone(),
                                            bytes,
                                            hash: Some(hash),
                                        });
                                    }
                                    Err(e) => {
                                        failed.fetch_add(1, Ordering::Relaxed);
                                        let _ = crate::copy::remove_file_clearing_readonly(&item.to);
                                        let _ = worker_tx.send(Event::FileFailed(FileError {
                                            path: item.rel.clone(),
                                            message: format!(
                                                "could not hash the source after copying, so \
                                                 the copy cannot be verified: {e}"
                                            ),
                                        }));
                                    }
                                }
                            }
                            Ok(CopyOutcome::Copied { bytes }) => {
                                copied.fetch_add(1, Ordering::Relaxed);
                                bytes_copied.fetch_add(bytes, Ordering::Relaxed);
                                let _ = worker_tx.send(Event::FileDone {
                                    path: item.rel.clone(),
                                    bytes,
                                    hash: None,
                                });
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
                            bytes_done: bytes_copied.load(Ordering::Relaxed)
                                + bytes_in_flight.load(Ordering::Relaxed),
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

            // D1 phase two: verify. Re-read every destination file copied in this phase
            // and compare against the source hash recorded at copy time. A cancelled
            // phase skips this and emits no VerifyDone -- its output is deleted (backup)
            // or abandoned mid-state (mirror), and a false "all clean" is the worst
            // message verification could send.
            if options.verify && !cancelled.load(Ordering::Relaxed) {
                let examined = AtomicU64::new(0);
                let mismatches: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
                let verify_work = || {
                    let list = copied_files_ref.lock().unwrap();
                    list.par_iter().for_each(|cf| {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        examined.fetch_add(1, Ordering::Relaxed);
                        let matches = match crate::copy::blake3_hex(&cf.to) {
                            Ok(h) => h == cf.hash,
                            // A destination file that will not re-read cannot be proven
                            // good -- treat it exactly like a hash mismatch.
                            Err(_) => false,
                        };
                        if !matches {
                            // Remove the bad copy: left in place, the next run's
                            // unchanged-check (same size, preserved mtime) would hardlink
                            // the corrupt bytes into every later snapshot, or a mirror
                            // would keep them as the live file. Verification failures must
                            // not propagate forward.
                            let _ = crate::copy::remove_file_clearing_readonly(&cf.to);
                            copied.fetch_sub(1, Ordering::Relaxed);
                            bytes_copied.fetch_sub(cf.bytes, Ordering::Relaxed);
                            failed.fetch_add(1, Ordering::Relaxed);
                            mismatches.lock().unwrap().push(cf.rel.clone());
                            let _ = worker_tx.send(Event::FileFailed(FileError {
                                path: cf.rel.clone(),
                                message: "verification failed: the destination copy does not \
                                          match the source content -- the bad copy was \
                                          removed so it cannot be carried into later runs"
                                    .into(),
                            }));
                        }
                    });
                };
                match &pool {
                    Some(p) => p.install(verify_work),
                    None => verify_work(),
                }
                let mut mismatches = mismatches.into_inner().unwrap();
                mismatches.sort();
                // Verify failures are failed, not copied -- drop them from the record the
                // engine will store in the manifest.
                copied_files_ref.lock().unwrap().retain(|cf| !mismatches.contains(&cf.rel));
                let _ = worker_tx.send(Event::VerifyDone {
                    examined: examined.load(Ordering::Relaxed),
                    mismatches,
                });
            }
        });

        // Drop this thread's sender so the loop below ends once the workers finish.
        drop(tx);
        while let Ok(ev) = rx.recv() {
            emit(ev);
        }
    });

    let copied_files = std::mem::take(&mut *copied_files.lock().unwrap());
    Counts {
        copied: copied.load(Ordering::Relaxed),
        linked: linked.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        bytes_copied: bytes_copied.load(Ordering::Relaxed),
        bytes_linked: bytes_linked.load(Ordering::Relaxed),
        cancelled: cancelled.load(Ordering::Relaxed),
        copied_files,
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
        let counts = run_parallel(&items, &ParallelOptions::unverified(), &AtomicBool::new(false), &mut |e| events.push(e));

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

        let counts = run_parallel(&items, &ParallelOptions::unverified(), &AtomicBool::new(false), &mut |_| {});
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

        let counts = run_parallel(&items, &ParallelOptions::unverified(), &AtomicBool::new(false), &mut |_| {});
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
        let counts = run_parallel(&items, &ParallelOptions::unverified(), &AtomicBool::new(false), &mut |e| events.push(e));
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
        run_parallel(&items, &ParallelOptions::unverified(), &AtomicBool::new(false), &mut |e| {
            if let Event::OverallProgress { files_done, files_total, .. } = e {
                last_progress = Some((files_done, files_total));
            }
        });

        let (done, total) = last_progress.expect("at least one progress event");
        assert_eq!(done, 200);
        assert_eq!(total, 200);
    }

    fn fast_verifying() -> ParallelOptions {
        ParallelOptions {
            verify: true,
            progress_interval: std::time::Duration::ZERO,
            tamper_dest_after_copy: None,
        }
    }

    /// MT7/F34: FileStarted and FileDone bracket every copied file; a multi-MB file
    /// produces several byte-level FileProgress samples ending at 100%; and with
    /// verification on, FileDone carries the source's blake3 hash.
    #[test]
    fn per_file_events_bracket_copies_and_byte_progress_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big.bin");
        // Large enough that CopyFileExW reports progress more than once.
        let data = vec![7u8; 24 * 1024 * 1024];
        std::fs::write(&big, &data).unwrap();
        let small = tmp.path().join("small.txt");
        touch(&small, "small");

        let items = vec![
            CopyItem {
                rel: PathBuf::from("big.bin"),
                from: big.clone(),
                to: tmp.path().join("out/big.bin"),
                size: data.len() as u64,
                link_from: None,
            },
            CopyItem {
                rel: PathBuf::from("small.txt"),
                from: small.clone(),
                to: tmp.path().join("out/small.txt"),
                size: 5,
                link_from: None,
            },
        ];

        let mut events = Vec::new();
        let counts = run_parallel(&items, &fast_verifying(), &AtomicBool::new(false), &mut |e| {
            events.push(e)
        });
        assert_eq!(counts.copied, 2);

        // Bracketing: each file's FileStarted precedes its FileDone.
        for rel in ["big.bin", "small.txt"] {
            let started = events
                .iter()
                .position(|e| matches!(e, Event::FileStarted { path, .. } if path == Path::new(rel)))
                .expect("FileStarted");
            let done = events
                .iter()
                .position(|e| matches!(e, Event::FileDone { path, .. } if path == Path::new(rel)))
                .expect("FileDone");
            assert!(started < done, "{rel}: FileStarted must precede FileDone");
        }

        // Byte progress on the big file: multiple samples, monotonic, ending at 100%.
        let samples: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|e| match e {
                Event::FileProgress { path, done, total } if path == Path::new("big.bin") => {
                    Some((*done, *total))
                }
                _ => None,
            })
            .collect();
        assert!(samples.len() >= 2, "a 24 MB copy must report byte progress: {samples:?}");
        for w in samples.windows(2) {
            assert!(w[1].0 >= w[0].0, "byte progress must not go backwards");
        }
        assert_eq!(*samples.last().unwrap(), (data.len() as u64, data.len() as u64));

        // The hash on FileDone is the real blake3 digest of the source content.
        let done_big = events.iter().find_map(|e| match e {
            Event::FileDone { path, hash, .. } if path == Path::new("big.bin") => hash.clone(),
            _ => None,
        }).expect("FileDone for big.bin");
        assert_eq!(done_big, crate::copy::blake3_hex(&big).unwrap());
    }

    /// D1: the verify pass re-reads every copied destination file; a phase where all is
    /// well reports exactly the copied count with no mismatches.
    #[test]
    fn the_verify_pass_examines_exactly_the_copied_files() {
        let tmp = tempfile::tempdir().unwrap();
        let items: Vec<CopyItem> = (0..3)
            .map(|i| {
                let src = tmp.path().join(format!("s/{i}.txt"));
                touch(&src, &format!("content {i}"));
                CopyItem {
                    rel: PathBuf::from(format!("{i}.txt")),
                    from: src,
                    to: tmp.path().join(format!("out/{i}.txt")),
                    size: 9,
                    link_from: None,
                }
            })
            .collect();

        let mut events = Vec::new();
        let counts = run_parallel(&items, &fast_verifying(), &AtomicBool::new(false), &mut |e| {
            events.push(e)
        });
        assert_eq!(counts.copied, 3);
        assert_eq!(counts.copied_files.len(), 3);

        let verify = events.iter().find_map(|e| match e {
            Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
            _ => None,
        }).expect("VerifyDone must fire");
        assert_eq!(verify.0, 3, "every copied file was re-read and compared");
        assert!(verify.1.is_empty());
    }

    /// D1: corruption between copy and verify (here: injected by the tamper seam) must
    /// surface as a failure naming the file -- and the bad destination copy must be
    /// REMOVED so the next run cannot link or keep the corrupt bytes.
    #[test]
    fn a_tampered_destination_fails_verification_and_is_removed() {
        fn flip_first_byte_of_a(path: &Path) {
            if path.file_name().map(|n| n == "a.txt").unwrap_or(false) {
                let mut data = std::fs::read(path).unwrap();
                if let Some(first) = data.first_mut() {
                    *first ^= 0xFF;
                }
                std::fs::write(path, data).unwrap();
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("s/a.txt");
        let src_b = tmp.path().join("s/b.txt");
        touch(&src_a, "content-a");
        touch(&src_b, "content-b");
        let items = vec![
            CopyItem {
                rel: PathBuf::from("a.txt"),
                from: src_a,
                to: tmp.path().join("out/a.txt"),
                size: 9,
                link_from: None,
            },
            CopyItem {
                rel: PathBuf::from("b.txt"),
                from: src_b,
                to: tmp.path().join("out/b.txt"),
                size: 9,
                link_from: None,
            },
        ];

        let mut options = fast_verifying();
        options.tamper_dest_after_copy = Some(flip_first_byte_of_a);
        let mut events = Vec::new();
        let counts = run_parallel(&items, &options, &AtomicBool::new(false), &mut |e| events.push(e));

        assert_eq!(counts.copied, 1, "only the good file stays counted as copied");
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.copied_files.len(), 1, "the bad file is out of the record");
        assert_eq!(counts.copied_files[0].rel, PathBuf::from("b.txt"));
        assert!(!tmp.path().join("out/a.txt").exists(), "the bad copy was removed");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/b.txt")).unwrap(),
            "content-b"
        );

        let verify = events.iter().find_map(|e| match e {
            Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
            _ => None,
        }).expect("VerifyDone");
        assert_eq!(verify.1, vec![PathBuf::from("a.txt")], "the mismatch names the file");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::FileFailed(f) if f.path == Path::new("a.txt") && f.message.contains("verification failed")
        )));
    }

    /// Linked files are not re-read (D1's cost is proportional to churn): a link-only
    /// phase verifies nothing, and says so honestly with examined: 0.
    #[test]
    fn a_link_only_phase_verifies_nothing_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("prev/a.txt");
        touch(&existing, "shared");
        let items = vec![CopyItem {
            rel: PathBuf::from("a.txt"),
            from: tmp.path().join("irrelevant.txt"),
            to: tmp.path().join("out/a.txt"),
            size: 6,
            link_from: Some(existing),
        }];

        let mut events = Vec::new();
        let counts = run_parallel(&items, &fast_verifying(), &AtomicBool::new(false), &mut |e| {
            events.push(e)
        });
        assert_eq!(counts.linked, 1);
        assert_eq!(counts.copied, 0);
        let verify = events.iter().find_map(|e| match e {
            Event::VerifyDone { examined, mismatches } => Some((*examined, mismatches.clone())),
            _ => None,
        }).expect("VerifyDone still fires -- checked-nothing must be visible");
        assert_eq!(verify.0, 0);
        assert!(verify.1.is_empty());
    }

    /// A cancelled phase runs no verify pass and emits no VerifyDone: its output is
    /// abandoned, and "0 examined" would read as "all clean".
    #[test]
    fn a_cancelled_phase_skips_verification_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        touch(&src, "x");
        let items = vec![CopyItem {
            rel: PathBuf::from("a.txt"),
            from: src,
            to: tmp.path().join("out/a.txt"),
            size: 1,
            link_from: None,
        }];

        let mut events = Vec::new();
        let counts = run_parallel(
            &items,
            &fast_verifying(),
            &AtomicBool::new(true),
            &mut |e| events.push(e),
        );
        assert!(counts.cancelled);
        assert_eq!(counts.copied, 0);
        assert!(
            !events.iter().any(|e| matches!(e, Event::VerifyDone { .. })),
            "no false all-clean after a cancel"
        );
    }
}
