//! The engine/UI seam.
//!
//! This is the module that retires the PowerShell architecture's central fragility. There,
//! progress was recovered by redirecting robocopy's stdout to a temp file, tailing it on a
//! 400 ms WinForms timer, splitting on the last complete newline, and regex-parsing
//! human-readable output -- including scraping a percentage out of a line robocopy had not
//! finished writing (`app/BackStar.Engine.ps1:37-163`). That made progress dependent on
//! robocopy's console formatting, its output codepage, and its locale.
//!
//! Here the engine emits typed values. Nothing is parsed, so nothing can be misparsed.
//!
//! Three `Option` fields below are load-bearing and must never be flattened to a zero.
//! In the original, a sync preview whose summary block failed to parse deliberately yielded
//! `$null` rather than `0`, because the confirmation dialog would otherwise have told the
//! user the mirror would "PERMANENTLY DELETE ~0 file(s)" and they would have approved a
//! destructive run believing nothing was at stake (`app/BackStar.Engine.ps1:800-803`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Why a file was not copied. Distinguishing these is what lets the UI collapse routine
/// noise ("41 files locked by another process") into one row while still surfacing the
/// failures that matter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SkipReason {
    /// Unchanged since the previous snapshot -- the common case, and not a problem.
    Unchanged,
    /// Matched an exclusion rule.
    Excluded { rule: String },
    /// Open by another process. Expected for browser profiles and mail stores; the real
    /// fix is a shadow copy, which is elevation-gated.
    Locked,
    /// Access denied.
    Permission,
    /// A reparse point that was not followed.
    Reparse,
}

/// A file-level failure. Unlike [`SkipReason`], every one of these is a real problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileError {
    pub path: PathBuf,
    pub message: String,
}

/// Totals for one completed job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStats {
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    /// Only ever nonzero for a mirror run -- a snapshot never deletes anything a previous
    /// snapshot still has a copy of, so this stays 0 for every `engine::run_job` result.
    pub files_deleted: u64,
    pub bytes_copied: u64,
    /// Bytes that did not have to be written because the file was hardlinked to the
    /// previous snapshot. This is the number that makes versioning affordable.
    pub bytes_linked: u64,
    pub duration_ms: u64,
}

/// How a run ended. Replaces the original's collapsed `$code -lt 8` test, which folded
/// robocopy's exit bitmask into a flat pass/fail: exit 9 (copied AND some failed) reported
/// as plain `FAILED` with no hint that 4,000 of 4,001 files were fine -- and verification
/// only ran on the success branch, so a partially-failed job was never verified at all
/// (`app/BackStar.Engine.ps1:539-559`). A user who sees "Failed" after every run in which
/// one browser file was locked learns to ignore the word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunOutcome {
    /// Everything that was meant to be copied was copied and verified.
    Ok,
    /// Real work succeeded, but some files failed. Still worth verifying and recording.
    Partial { failed: u64 },
    /// Nothing usable was produced.
    Failed { reason: String },
    /// The user cancelled.
    Cancelled,
}

/// Everything the engine reports while a run is in flight.
///
/// `rename_all` renames the variant tags; `rename_all_fields` renames the fields *inside*
/// struct variants. Both are required. With only the first, `files_done` reaches the
/// frontend as `files_done` while the TypeScript reads `filesDone` -- the events arrive,
/// deserialise into `undefined`, and the progress bar sits at zero for the whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Event {
    RunStarted { run_id: String, jobs: usize },

    JobStarted { job: String, source: PathBuf, dest: PathBuf },

    /// Walking the source tree. The total is unknown until the walk finishes, which is why
    /// the UI must show an indeterminate state here rather than a fake percentage.
    ScanProgress { files_seen: u64, bytes_seen: u64 },

    /// The walk is done and the work is known. From here a percentage is honest.
    ///
    /// `deletes` is `None` when the count could not be established. The UI must then show
    /// its generic destructive warning, never "~0 files".
    PlanReady { to_copy: u64, to_link: u64, bytes_to_copy: u64, deletes: Option<u64> },

    /// Aggregate progress across all copy threads, throttled.
    ///
    /// Copying runs in parallel, so there is no single "current file" -- several are in
    /// flight at once. `current` is a sample of one of them, purely so the UI has something
    /// moving to show; the honest numbers are the counts.
    ///
    /// This is deliberately the only per-file-ish event on the hot path. Emitting one
    /// event per file would mean roughly half a million messages for a 224,000-file job,
    /// all of which would have to cross the IPC boundary and be rendered.
    OverallProgress {
        files_done: u64,
        files_total: u64,
        bytes_done: u64,
        bytes_total: u64,
        current: Option<PathBuf>,
    },

    FileStarted { path: PathBuf, bytes: u64 },

    /// Real byte counts from the copy callback -- not a number scraped from a log line.
    FileProgress { done: u64, total: u64 },

    FileDone { path: PathBuf, bytes: u64, hash: Option<String> },

    FileSkipped { path: PathBuf, reason: SkipReason },

    FileFailed(FileError),

    JobDone { job: String, stats: JobStats },

    /// Verification finished.
    ///
    /// `examined` exists so that "checked nothing" is structurally distinguishable from
    /// "all clean". The original had no such counter, and when a filter bug made the check
    /// skip every file it reported success for months (`app/BackStar.Engine.ps1:165-202`).
    VerifyDone { examined: u64, mismatches: Vec<PathBuf> },

    RunDone { run_id: String, outcome: RunOutcome, stats: JobStats },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_distinguishes_unknown_deletes_from_zero() {
        let unknown = Event::PlanReady {
            to_copy: 1,
            to_link: 0,
            bytes_to_copy: 1,
            deletes: None,
        };
        let none_to_delete = Event::PlanReady {
            to_copy: 1,
            to_link: 0,
            bytes_to_copy: 1,
            deletes: Some(0),
        };
        // If these ever compare equal, the destructive-confirmation dialog has lost the
        // distinction between "nothing will be deleted" and "we could not find out".
        assert_ne!(unknown, none_to_delete);

        let j = serde_json::to_string(&unknown).unwrap();
        assert!(j.contains("null"), "unknown delete count must survive serialisation as null");
    }

    #[test]
    fn partial_is_not_failure() {
        let p = RunOutcome::Partial { failed: 1 };
        assert_ne!(p, RunOutcome::Ok);
        assert_ne!(p, RunOutcome::Failed { reason: "x".into() });
    }

    /// The frontend reads these fields by name, so the exact JSON keys are part of the
    /// contract. `rename_all` on an enum renames the *variants*; struct-variant *fields*
    /// need `rename_all_fields`, and forgetting it is invisible in Rust -- it shows up only
    /// as a progress bar that never moves.
    #[test]
    fn struct_variant_fields_serialise_as_camel_case() {
        let e = Event::OverallProgress {
            files_done: 5,
            files_total: 10,
            bytes_done: 100,
            bytes_total: 200,
            current: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        for key in ["filesDone", "filesTotal", "bytesDone", "bytesTotal"] {
            assert!(json.contains(&format!("\"{key}\"")), "missing {key} in {json}");
        }
        assert!(!json.contains("files_done"), "snake_case leaked into the wire format: {json}");

        let plan = Event::PlanReady {
            to_copy: 1,
            to_link: 2,
            bytes_to_copy: 3,
            deletes: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        for key in ["toCopy", "toLink", "bytesToCopy", "deletes"] {
            assert!(json.contains(&format!("\"{key}\"")), "missing {key} in {json}");
        }

        let scan = Event::ScanProgress { files_seen: 1, bytes_seen: 2 };
        let json = serde_json::to_string(&scan).unwrap();
        assert!(json.contains("\"filesSeen\""), "{json}");
        assert!(json.contains("\"bytesSeen\""), "{json}");

        let done = Event::RunDone {
            run_id: "x".into(),
            outcome: RunOutcome::Ok,
            stats: JobStats::default(),
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains("\"runId\""), "{json}");
        // JobStats is a plain struct, so its own rename_all applies.
        assert!(json.contains("\"filesCopied\""), "{json}");
    }

    #[test]
    fn events_round_trip_as_tagged_json() {
        let e = Event::FileSkipped {
            path: PathBuf::from(r"C:\x\locked.db"),
            reason: SkipReason::Locked,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""type":"fileSkipped""#));
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }
}
