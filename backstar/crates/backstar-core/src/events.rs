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

/// A file-level failure. Every one of these is a real problem.
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
    /// Files a restore deliberately did NOT write because the destination copy was newer
    /// than the snapshot's (D5 `IfOlder` policy). Not failures -- the user's newer data
    /// was protected on purpose -- but a nonzero value keeps the restore's outcome honest
    /// (Partial, not Ok).
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
    /// Reparse points (junctions and symlinks) the walk refused to follow. Policy D7:
    /// never follow them -- but always report the count, because a skipped reparse point
    /// is something in the tree that was not backed up, and an invisible skip reads
    /// exactly like "there was nothing there".
    #[serde(default)]
    pub reparse_skipped: u64,
    /// Configured sources (folders or presets) skipped because they no longer existed at
    /// run time. Zero is the only value compatible with an `Ok` outcome.
    #[serde(default)]
    pub sources_skipped: u64,
    /// Snapshots pruned by the retention policy (`keep_snapshots`) at the end of the run
    /// (D2). Only ever nonzero for a snapshot run; a mirror has no history to thin.
    #[serde(default)]
    pub snapshots_pruned: u64,
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
    /// Real work succeeded, but some files failed or some configured sources were
    /// skipped. Still worth verifying and recording. `failed` counts files only --
    /// a run that skipped a source but lost no files reports `failed: 0`, and the
    /// skipped sources are on [`JobStats::sources_skipped`] instead.
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
    /// `jobs` counts the units of work the run will report on: sources for a backup or
    /// mirror, and `1` for a restore (one restore operation -- even a directory restore,
    /// whose file count is a walk result, not a job count; L1).
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
    /// Emitted every 64 completed files AND time-throttled during long copies (F34), with
    /// `bytes_done` including the in-flight bytes of files still being written -- so a
    /// three-file 20 GB job shows a moving bytes bar instead of sitting at 0% until the
    /// first file lands.
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
    /// Time-throttled at the producer (several copy threads run at once, so `path` says
    /// WHICH in-flight file this sample belongs to); always emitted at 100% of a file.
    FileProgress { path: PathBuf, done: u64, total: u64 },

    /// `hash` is the blake3 hex digest of the source content as copied, present for every
    /// copy made under content verification (D1) and `None` where verification is off
    /// (restores). Linked files are not reported here at all: no bytes move for a link,
    /// and their content was verified by the run that first copied it.
    FileDone { path: PathBuf, bytes: u64, hash: Option<String> },

    FileFailed(FileError),

    /// A configured source (a folder path, or a preset key) was skipped because it no
    /// longer exists. This is a warning, not a hard failure -- the remaining sources still
    /// run -- but it always forces at least a `Partial` outcome, because a job reporting
    /// clean success over half its configured sources is exactly the silent-failure shape
    /// the PowerShell app shipped with.
    SourceSkipped { source: String, reason: String },

    /// Something inside a source tree could not be read at all: a directory that would
    /// not list, or an entry whose metadata failed. Distinct from [`Event::FileFailed`],
    /// which means a known file failed to copy -- this event means the walk itself found
    /// a hole, so the destination is missing content the run never even attempted.
    SourceUnreadable { path: PathBuf, message: String },

    /// A run-level warning that belongs to no single file or source: the destination
    /// volume cannot hardlink, so unchanged files are fully recopied every run (F29);
    /// the recovery README could not be refreshed (F36); and similar. One general kind
    /// rather than one variant per cause keeps the wire contract small -- the UI renders
    /// it as a warning row exactly like [`Event::SourceSkipped`], and the message text is
    /// written for display, not for parsing.
    RunWarning { message: String },

    /// One source finished. `stats` are PER SOURCE -- the work this one source did -- not
    /// run-cumulative (L1): a consumer summing them must arrive at the [`Event::RunDone`]
    /// totals, and a consumer showing progress per source gets an honest delta rather than
    /// a number that starts at the previous source's total.
    JobDone { job: String, stats: JobStats },

    /// The post-copy verification pass finished (D1): every file copied in this phase was
    /// re-read at the destination and its hash compared against the source hash recorded
    /// at copy time. Emitted once per copy phase (i.e. per source) of runs that verify
    /// (backups and mirrors; restores do not verify). A phase that copied nothing emits
    /// `examined: 0` -- honest: nothing needed verifying. A cancelled run skips the pass
    /// and emits no `VerifyDone` at all: its snapshot is deleted (backup) or left as-is
    /// (mirror), and a false "all clean" would be the worst message to send.
    ///
    /// `examined` exists so that "checked nothing" is structurally distinguishable from
    /// "all clean". The original had no such counter, and when a filter bug made the check
    /// skip every file it reported success for months (`app/BackStar.Engine.ps1:165-202`).
    /// `mismatches` carries the run-relative paths of files that failed verification --
    /// each of them is also counted in `files_failed` and reported via `FileFailed`, and
    /// its destination copy has been removed so the next run cannot link or keep the bad
    /// bytes forward.
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
        let e = Event::FileDone {
            path: PathBuf::from(r"C:\x\big.bin"),
            bytes: 42,
            hash: Some("a".repeat(64)),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""type":"fileDone""#));
        assert!(json.contains("\"hash\""), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);

        let progress = Event::FileProgress { path: PathBuf::from("big.bin"), done: 5, total: 10 };
        let json = serde_json::to_string(&progress).unwrap();
        assert!(json.contains(r#""type":"fileProgress""#), "{json}");
        assert!(json.contains("\"path\""), "the path says WHICH in-flight file: {json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(progress, back);
    }

    /// The warning events added for source truthfulness, and the JobStats fields they feed,
    /// are part of the same camelCase wire contract as everything else in this module.
    #[test]
    fn source_warning_events_and_new_stats_fields_serialise_camel_case() {
        let skipped = Event::SourceSkipped {
            source: r"D:\vanished".into(),
            reason: "the folder does not exist".into(),
        };
        let json = serde_json::to_string(&skipped).unwrap();
        assert!(json.contains(r#""type":"sourceSkipped""#), "{json}");
        assert!(json.contains("\"source\""), "{json}");
        assert!(json.contains("\"reason\""), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(skipped, back);

        let unreadable = Event::SourceUnreadable {
            path: PathBuf::from(r"D:\proj\locked"),
            message: "access denied".into(),
        };
        let json = serde_json::to_string(&unreadable).unwrap();
        assert!(json.contains(r#""type":"sourceUnreadable""#), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(unreadable, back);

        let stats = JobStats { reparse_skipped: 3, sources_skipped: 1, ..Default::default() };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"reparseSkipped\""), "{json}");
        assert!(json.contains("\"sourcesSkipped\""), "{json}");
        assert!(!json.contains("reparse_skipped"), "snake_case leaked: {json}");

        // Stats serialised before these fields existed must still deserialise -- the
        // fields are additive, not a schema break.
        let old = r#"{"filesCopied":1,"filesLinked":0,"filesSkipped":0,"filesFailed":0,"filesDeleted":0,"bytesCopied":1,"bytesLinked":0,"durationMs":1}"#;
        let back: JobStats = serde_json::from_str(old).unwrap();
        assert_eq!(back.reparse_skipped, 0);
        assert_eq!(back.sources_skipped, 0);
    }

    /// The general run-level warning (F29/F36): part of the same tagged camelCase
    /// contract, rendered by the UI as a warning row.
    #[test]
    fn run_warning_serialises_camel_case() {
        let w = Event::RunWarning { message: "destination cannot hardlink".into() };
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains(r#""type":"runWarning""#), "{json}");
        assert!(json.contains("\"message\""), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }
}
