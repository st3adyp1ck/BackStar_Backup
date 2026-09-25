//! The "am I safe right now?" computation.
//!
//! Deliberately done in Rust rather than in the UI. Whether a backup is trustworthy depends
//! on facts only the backend can establish -- whether the source folders still exist, whether
//! the destination volume is even mounted, whether it can do hardlinks, and whether source
//! and destination sit on the same physical disk. Computing it here means there is one
//! answer, and the UI renders it rather than inferring it.
//!
//! Honesty rules that changed in wave B2:
//!
//! - History matches jobs by `jobId`, falling back to name for entries written before that
//!   field existed (F40) -- a renamed job keeps its history, and two legacy same-named jobs
//!   can no longer cross-attribute NEW entries.
//! - Only successful entries count as protection (F17): a cancelled or failed run is not a
//!   backup. The latest attempt of any kind is reported separately so a card can say "last
//!   run failed".
//! - The headline is driven by the WORST enabled job (F16): one job stale or never run
//!   drags it down, naming that job.
//! - A network destination is a network location, not a missing drive (F20): `ProbeOutcome`
//!   from the engine keeps UNC honest -- only a genuinely unmounted local volume earns
//!   "Backup drive unavailable".
//! - Snapshot counts are manifest-backed only (F47); partial and damaged leftovers are
//!   counted as their own signals, never as backups.
//! - Disabled jobs are listed (marked `enabled: false`) so their backups stay browsable
//!   (F62), but they never enter the protection math.

use std::path::Path;

use backstar_core::config::{Config, History, HistoryEntry, Job, JobKind};
use backstar_core::volume::{self, ProbeOutcome};
use serde::Serialize;

/// How stale a backup has to be before it stops counting as current.
const STALE_AFTER_HOURS: i64 = 48;

/// A daily-scheduled job whose last protection is older than this counts as a missed fire
/// (F41) -- one day plus a grace window for a machine that was briefly off.
const MISSED_AFTER_HOURS: i64 = 26;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    pub job_name: String,
    /// Snapshot or mirror (F2's status half: Home must not label a mirror "Back up now").
    pub kind: JobKind,
    /// Disabled jobs are listed for browsing (F62) but excluded from protection math.
    pub enabled: bool,
    pub dest: String,
    pub missing_sources: Vec<String>,
    pub source_count: usize,
    /// The latest run that counts as protection (F17: never a Cancelled or Failed entry).
    pub last_run: Option<HistoryEntry>,
    /// The latest attempt regardless of outcome (F17): when this is newer than
    /// `last_run`, the card must say the last run failed or was cancelled.
    pub last_attempt: Option<HistoryEntry>,
    /// Manifest-backed snapshots only (F47).
    pub snapshot_count: usize,
    /// Manifest-less dirs (an interrupted run's leftovers, pruned by the next run) --
    /// counted as their own signal, never as backups (F47).
    pub incomplete_snapshots: usize,
    /// Snapshots whose manifests will not parse -- the tree may be intact; counted
    /// separately so damage is visible (F19/F47).
    pub unreadable_snapshots: usize,
    /// The destination volume, as the engine's honest three-way probe answers it (F20).
    pub volume: ProbeOutcome,
    pub same_volume_as_source: bool,
    /// A scheduled job whose last protection is more than a day-plus-grace old (F41).
    pub scheduled_missed: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protection {
    Protected,
    Stale,
    AtRisk,
    NeverRun,
    NoJobs,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub protection: Protection,
    pub headline: String,
    pub detail: String,
    pub jobs: Vec<JobStatus>,
    pub version: String,
}

/// Snapshot counts for the status surface (F47): manifest-backed only, with the
/// interrupted-run leftovers and damaged manifests counted as their own signals.
fn snapshot_counts(dest: &Path) -> (usize, usize, usize) {
    let Ok(repo) = backstar_core::repo::Repo::open(dest) else {
        return (0, 0, 0);
    };
    let listing = repo.list_snapshots_full();
    (listing.manifests.len(), listing.incomplete.len(), listing.unreadable.len())
}

/// Whether two paths live on the same volume, compared by serial number rather than by
/// drive letter so that a `subst`ed or remounted drive is still recognised. Network and
/// unprobeable paths answer false (no serial to compare).
fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume::probe(a), volume::probe(b)) {
        (Ok(va), Ok(vb)) => va.serial == vb.serial && va.serial != 0,
        _ => false,
    }
}

fn hours_since(iso: &str) -> Option<i64> {
    let then = time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
        .ok()?;
    let now = time::OffsetDateTime::now_utc();
    Some((now - then).whole_hours())
}

/// Whether a history entry counts as protection (F17). The outcome strings are written
/// by `outcome_summary` in lib.rs (`"OK"`, `"OK (N files failed)"`, `"Failed: ..."`,
/// `"Cancelled"`), and positive matching is deliberate: a future outcome shape must not
/// silently count until someone decides it should.
fn counts_as_protection(e: &HistoryEntry) -> bool {
    e.outcome.starts_with("OK")
}

/// The latest history entry for this job. Matches by job id (F40), falling back to the
/// name for entries written before `jobId` existed -- legacy name-keyed lines can still
/// cross-attribute between two same-named jobs, which is exactly the ambiguity F45's
/// duplicate-name validation now prevents going forward.
fn last_entry_for<'a>(
    entries: &'a [HistoryEntry],
    job: &Job,
    protection_only: bool,
) -> Option<&'a HistoryEntry> {
    entries.iter().rev().find(|e| {
        let job_matches = if e.job_id.is_empty() { e.job == job.name } else { e.job_id == job.id };
        job_matches && (!protection_only || counts_as_protection(e))
    })
}

pub fn compute(cfg: &Config, history: &History) -> AppStatus {
    let entries = history.read().unwrap_or_default();

    let jobs: Vec<JobStatus> = cfg
        .jobs
        .iter()
        .map(|job| {
            let missing: Vec<String> = job
                .sources
                .iter()
                .filter(|s| !s.is_dir())
                .map(|s| s.display().to_string())
                .collect();

            // F17: protection and recency are separate questions. `last_run` only counts
            // runs that actually protected something; `last_attempt` is whatever happened
            // most recently, so a failure is visible instead of hidden behind the last
            // success.
            let last_attempt = last_entry_for(&entries, job, false).cloned();
            let last_run = last_entry_for(&entries, job, true).cloned();

            // Resolved, not literal: after a drive-letter change, the literal `dest` may
            // point at a different, unrelated volume entirely (or nothing), which would
            // make every check below wrong in a way that looks like the backup vanished
            // rather than like the drive just moved.
            let dest = job.resolve_dest();
            let volume = volume::probe_destination(&dest);
            let same_vol = job.sources.iter().any(|s| same_volume(s, &dest));
            let (snapshot_count, incomplete_snapshots, unreadable_snapshots) =
                snapshot_counts(&dest);

            // F41: a scheduled job whose last protection is older than the interval plus
            // grace missed a fire (the machine was off, or the task never existed).
            // Never-run scheduled jobs are covered by the never-run headline instead.
            let scheduled_missed = job.enabled
                && job.schedule.is_some()
                && match &last_run {
                    Some(e) => hours_since(&e.timestamp).map(|h| h > MISSED_AFTER_HOURS).unwrap_or(true),
                    None => false,
                };

            JobStatus {
                job_id: job.id.clone(),
                job_name: job.name.clone(),
                kind: job.kind,
                enabled: job.enabled,
                dest: dest.display().to_string(),
                missing_sources: missing,
                source_count: job.sources.len(),
                last_run,
                last_attempt,
                snapshot_count,
                incomplete_snapshots,
                unreadable_snapshots,
                volume,
                same_volume_as_source: same_vol,
                scheduled_missed,
            }
        })
        .collect();

    let (protection, headline, detail) = summarise(&jobs);

    AppStatus {
        protection,
        headline,
        detail,
        jobs,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Reduce every job's state to one honest sentence.
///
/// The ordering matters: a real problem always outranks a reassuring summary. A tool that
/// says "Protected" while a source folder is missing is worse than one that says nothing.
/// And the headline is driven by the WORST enabled job (F16): a single stale or never-run
/// job drags it down, by name -- never "the best job wins" wishful framing.
fn summarise(jobs: &[JobStatus]) -> (Protection, String, String) {
    // Disabled jobs are listed for browsing (F62) but never enter the protection math.
    let jobs: Vec<&JobStatus> = jobs.iter().filter(|j| j.enabled).collect();
    if jobs.is_empty() {
        return (
            Protection::NoJobs,
            "No backups configured".into(),
            "Add a job to choose what gets backed up and where it goes.".into(),
        );
    }

    let missing: usize = jobs.iter().map(|j| j.missing_sources.len()).sum();
    if missing > 0 {
        let names: Vec<&str> = jobs
            .iter()
            .filter(|j| !j.missing_sources.is_empty())
            .flat_map(|j| j.missing_sources.iter().map(String::as_str))
            .take(3)
            .collect();
        return (
            Protection::AtRisk,
            format!("{missing} source folder{} missing", if missing == 1 { "" } else { "s" }),
            format!(
                "These folders are configured for backup but do not exist: {}. \
                 Until this is fixed, those backups protect nothing.",
                names.join(", ")
            ),
        );
    }

    // F20: only a genuinely Unavailable local volume is "drive unavailable". A Network
    // destination is a network location, not an alarm.
    if let Some(j) = jobs.iter().find(|j| matches!(j.volume, ProbeOutcome::Unavailable { .. })) {
        return (
            Protection::AtRisk,
            "Backup drive unavailable".into(),
            format!(
                "The destination for {} could not be reached. If it is a removable drive, \
                 check that it is plugged in.",
                j.job_name
            ),
        );
    }

    let never_run: Vec<&&JobStatus> = jobs.iter().filter(|j| j.last_run.is_none()).collect();
    if never_run.len() == jobs.len() {
        return (
            Protection::NeverRun,
            "No backup has run yet".into(),
            "Everything is configured, but nothing has been backed up. Run a job to create \
             your first snapshot."
                .into(),
        );
    }
    // F16: one never-run job among otherwise-protected ones is not "protected".
    if let Some(j) = never_run.first() {
        return (
            Protection::AtRisk,
            format!("{} has never been backed up", j.job_name),
            "Every other configured job is fine, but this one protects nothing yet. Run it \
             to create its first snapshot."
                .into(),
        );
    }

    // Worst-freshness-first: the stalest job is the headline. An unparseable timestamp
    // counts as infinitely stale -- "cannot prove fresh" is not "fresh".
    let stalest = jobs
        .iter()
        .filter_map(|j| {
            j.last_run
                .as_ref()
                .and_then(|e| hours_since(&e.timestamp))
                .map(|h| (*j, h))
                .or(Some((*j, i64::MAX)))
                .filter(|(_, h)| *h > STALE_AFTER_HOURS)
        })
        .max_by_key(|(_, h)| *h);

    match stalest {
        Some((j, h)) => (
            Protection::Stale,
            if h == i64::MAX {
                format!("{}: last backup time is unreadable", j.job_name)
            } else {
                format!(
                    "{}: last backup was {} day{} ago",
                    j.job_name,
                    h / 24,
                    if h / 24 == 1 { "" } else { "s" }
                )
            },
            "Anything changed in it since then is not backed up. The other jobs are fine."
                .into(),
        ),
        None => {
            // Every enabled job is fresh.
            let worst = jobs
                .iter()
                .filter_map(|j| j.last_run.as_ref().and_then(|e| hours_since(&e.timestamp)))
                .max()
                .unwrap_or(0);
            let same_vol = jobs.iter().any(|j| j.same_volume_as_source);
            let detail = if same_vol {
                "Your files are backed up. Note that at least one job writes to the same \
                 physical drive as its source, which protects against accidental deletion \
                 but not against drive failure."
                    .to_string()
            } else {
                "Every configured job has run recently and its destination is reachable."
                    .to_string()
            };
            (
                Protection::Protected,
                if worst < 1 {
                    "Backed up just now".into()
                } else {
                    format!("Backed up {worst} hour{} ago", if worst == 1 { "" } else { "s" })
                },
                detail,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backstar_core::config::Job;
    use backstar_core::exclude::ExcludeRules;
    use std::path::PathBuf;

    fn job_with(sources: Vec<PathBuf>, dest: PathBuf) -> Job {
        Job {
            id: "j".into(),
            name: "J".into(),
            sources,
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

    fn entry(job_name: &str, job_id: &str, outcome: &str, hours_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: (time::OffsetDateTime::now_utc() - time::Duration::hours(hours_ago))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            job: job_name.into(),
            job_id: job_id.into(),
            snapshot_id: None,
            duration_ms: 1,
            files_copied: 1,
            files_linked: 0,
            files_failed: 0,
            files_deleted: 0,
            bytes_copied: 1,
            dest: PathBuf::from(r"D:\B"),
            outcome: outcome.into(),
        }
    }

    /// Build a JobStatus with sensible defaults for the summary tests.
    fn js(name: &str, id: &str, last_run: Option<HistoryEntry>) -> JobStatus {
        JobStatus {
            job_id: id.into(),
            job_name: name.into(),
            kind: JobKind::Snapshot,
            enabled: true,
            dest: r"D:\B".into(),
            missing_sources: vec![],
            source_count: 1,
            last_run,
            last_attempt: None,
            snapshot_count: 0,
            incomplete_snapshots: 0,
            unreadable_snapshots: 0,
            volume: ProbeOutcome::Local(backstar_core::volume::VolumeCapability {
                root: PathBuf::from(r"D:\"),
                filesystem: "NTFS".into(),
                hard_links: true,
                serial: 1,
                guid_path: None,
            }),
            same_volume_as_source: false,
            scheduled_missed: false,
        }
    }

    #[test]
    fn no_jobs_reads_as_not_set_up_not_as_protected() {
        let (p, head, _) = summarise(&[]);
        assert!(matches!(p, Protection::NoJobs));
        assert!(!head.to_lowercase().contains("protected"));
    }

    #[test]
    fn a_missing_source_outranks_a_recent_successful_run() {
        // The failure mode this guards: a job that ran fine last night, but whose source
        // folder has since been renamed, would otherwise report "Protected".
        let mut j = js("J", "j", Some(entry("J", "j", "OK", 1)));
        j.missing_sources = vec![r"D:\gone".into()];
        let (p, head, detail) = summarise(&[j]);
        assert!(matches!(p, Protection::AtRisk));
        assert!(head.contains("missing"));
        assert!(detail.contains(r"D:\gone"));
    }

    #[test]
    fn configured_but_never_run_is_never_run_not_protected() {
        let (p, _, _) = summarise(&[js("J", "j", None)]);
        assert!(matches!(p, Protection::NeverRun));
    }

    /// F16: the worst job drives the headline. One fresh job plus one never-run job must
    /// not read Protected, and the never-run job is named.
    #[test]
    fn a_never_run_job_drags_the_headline_down_and_is_named() {
        let (p, head, _) =
            summarise(&[js("Fresh", "a", Some(entry("Fresh", "a", "OK", 1))), js("Ghost", "b", None)]);
        assert!(matches!(p, Protection::AtRisk), "{p:?}");
        assert!(head.contains("Ghost"), "{head}");
    }

    /// F16: one stale job among fresh ones makes the headline Stale, naming it.
    #[test]
    fn a_stale_job_drags_the_headline_and_is_named() {
        let jobs = vec![
            js("Fresh", "a", Some(entry("Fresh", "a", "OK", 1))),
            js("Old", "b", Some(entry("Old", "b", "OK", 24 * 5))),
        ];
        let (p, head, detail) = summarise(&jobs);
        assert!(matches!(p, Protection::Stale), "{p:?}");
        assert!(head.contains("Old"), "{head}");
        assert!(head.contains("5 days"), "{head}");
        assert!(detail.contains("other jobs are fine"), "{detail}");
    }

    /// F16/F62: a DISABLED stale job does not drag the headline down.
    #[test]
    fn a_disabled_stale_job_does_not_drag_the_headline() {
        let mut disabled = js("Off", "b", Some(entry("Off", "b", "OK", 24 * 30)));
        disabled.enabled = false;
        let jobs = vec![js("Fresh", "a", Some(entry("Fresh", "a", "OK", 1))), disabled];
        let (p, head, _) = summarise(&jobs);
        assert!(matches!(p, Protection::Protected), "{p:?}");
        assert!(head.contains("1 hour"), "{head}");
    }

    /// F17: a cancelled or failed run is not protection. A job whose only history is a
    /// cancel reads as never-run, not fresh.
    #[test]
    fn cancelled_and_failed_runs_never_count_as_protection() {
        assert!(!counts_as_protection(&entry("J", "j", "Cancelled", 1)));
        assert!(!counts_as_protection(&entry("J", "j", "Failed: disk full", 1)));
        assert!(counts_as_protection(&entry("J", "j", "OK", 1)));
        assert!(counts_as_protection(&entry("J", "j", "OK (2 files failed)", 1)));
        // Positive matching: an unknown future shape must not silently count.
        assert!(!counts_as_protection(&entry("J", "j", "Completed", 1)));
    }

    /// F40: history matches by job id; a legacy name-keyed line still matches by name,
    /// and an id-keyed line for a DIFFERENT job with the same name does not cross over.
    #[test]
    fn history_matches_by_id_with_a_legacy_name_fallback() {
        let job = job_with(vec![], PathBuf::from(r"D:\B"));
        let entries = vec![
            entry("J", "", "OK", 50),          // legacy, name-only
            entry("J", "other-id", "OK", 10),  // same name, different id: not ours
            entry("J", "j", "OK", 20),         // ours
        ];
        let found = last_entry_for(&entries, &job, true).unwrap();
        assert_eq!(found.job_id, "j", "id-keyed entry wins");

        // Without our id-keyed entry, the legacy name line matches (the fallback).
        let found = last_entry_for(&entries[..2], &job, true).unwrap();
        assert_eq!(found.job_id, "", "legacy lines fall back to name matching");

        // And with only the other job's id-keyed line, nothing matches.
        assert!(last_entry_for(&entries[1..2], &job, true).is_none());
    }

    /// F17: last_attempt is the newest entry of ANY outcome even when last_run filters
    /// to the last success.
    #[test]
    fn last_attempt_surfaces_a_failure_behind_the_last_success() {
        let job = job_with(vec![], PathBuf::from(r"D:\B"));
        let entries = vec![
            entry("J", "j", "OK", 20),
            entry("J", "j", "Failed: disk full", 2),
        ];
        let last_run = last_entry_for(&entries, &job, true).unwrap();
        let last_attempt = last_entry_for(&entries, &job, false).unwrap();
        assert!(last_run.outcome.starts_with("OK"));
        assert!(last_attempt.outcome.starts_with("Failed"), "the failure is visible");
    }

    /// F20: a network destination is not "Backup drive unavailable".
    #[test]
    fn a_network_destination_is_not_at_risk_by_itself() {
        let mut j = js("J", "j", Some(entry("J", "j", "OK", 1)));
        j.volume = ProbeOutcome::Network { path: PathBuf::from(r"\\nas\backups") };
        let (p, head, _) = summarise(&[j]);
        assert!(matches!(p, Protection::Protected), "{p:?}");
        assert!(!head.contains("unavailable"), "{head}");
    }

    /// An unmounted local drive keeps the AtRisk verdict.
    #[test]
    fn an_unavailable_volume_is_at_risk() {
        let mut j = js("J", "j", Some(entry("J", "j", "OK", 1)));
        j.volume = ProbeOutcome::Unavailable { path: PathBuf::from(r"E:\B"), reason: "gone".into() };
        let (p, head, _) = summarise(&[j]);
        assert!(matches!(p, Protection::AtRisk));
        assert!(head.contains("unavailable"), "{head}");
    }

    /// F47: only manifest-backed snapshots count; the leftovers are their own counts.
    #[test]
    fn snapshot_counts_count_manifest_backed_only() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = backstar_core::repo::Repo::open_or_init(tmp.path(), "T").unwrap();
        // One real snapshot with a manifest...
        let id = "2026-01-01T00-00-00Z";
        std::fs::create_dir_all(repo.snapshot_dir(id)).unwrap();
        repo.write_manifest(&backstar_core::repo::SnapshotManifest {
            id: id.into(),
            job: "T".into(),
            started: "2026-01-01T00:00:00Z".into(),
            finished: "2026-01-01T00:01:00Z".into(),
            sources: vec![],
            files_copied: 0,
            files_linked: 0,
            files_failed: 0,
            bytes_copied: 0,
            bytes_linked: 0,
            duration_ms: 0,
            outcome: backstar_core::events::RunOutcome::Ok,
            parent: None,
            skipped_sources: vec![],
            copied_hashes: Default::default(),
        })
        .unwrap();
        // ...one interrupted run's leftover, and one corrupt manifest.
        std::fs::create_dir_all(repo.snapshot_dir("2026-02-01T00-00-00Z")).unwrap();
        std::fs::create_dir_all(repo.snapshot_dir("2026-03-01T00-00-00Z")).unwrap();
        std::fs::write(
            tmp.path().join(".backstar/snapshots/2026-03-01T00-00-00Z.json"),
            b"{ nope",
        )
        .unwrap();

        let (count, incomplete, unreadable) = snapshot_counts(tmp.path());
        assert_eq!(count, 1, "manifest-backed only");
        assert_eq!(incomplete, 1);
        assert_eq!(unreadable, 1);
        // And a plain folder that merely contains a "snapshots" dir counts nothing.
        let plain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plain.path().join("snapshots/whatever")).unwrap();
        assert_eq!(snapshot_counts(plain.path()), (0, 0, 0));
    }

    #[test]
    fn same_volume_is_called_out_even_when_protected() {
        let mut j = js("J", "j", Some(entry("J", "j", "OK", 1)));
        j.same_volume_as_source = true;
        let (p, _, detail) = summarise(&[j]);
        assert!(matches!(p, Protection::Protected));
        assert!(
            detail.contains("same"),
            "a same-volume backup must say so even when otherwise healthy"
        );
    }

    #[test]
    fn disabled_jobs_are_listed_but_not_counted() {
        let mut cfg = Config::default();
        let mut j = job_with(vec![], PathBuf::from(r"D:\B"));
        j.enabled = false;
        cfg.jobs.push(j);

        let tmp = tempfile::tempdir().unwrap();
        let status = compute(&cfg, &History::at(tmp.path().join("h.jsonl")));
        // F62: listed (marked disabled) so its backups stay browsable...
        assert_eq!(status.jobs.len(), 1);
        assert!(!status.jobs[0].enabled);
        // ...but the protection math sees no enabled job.
        assert!(matches!(status.protection, Protection::NoJobs));
    }

    /// F41: a daily-scheduled job whose last protection is more than a day-plus-grace old
    /// missed a fire.
    #[test]
    fn a_scheduled_job_past_its_interval_is_flagged_missed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let mut j = job_with(vec![], tmp.path().join("dest"));
        j.schedule = Some(backstar_core::config::DailySchedule { hour: 6, minute: 0 });
        let stale = entry("J", "j", "OK", 30);
        let history = History::at(tmp.path().join("h.jsonl"));
        history.append(&stale).unwrap();
        cfg.jobs.push(j);

        let status = compute(&cfg, &history);
        assert!(status.jobs[0].scheduled_missed, "30h > 24h+2h grace");

        // A fresh run clears it.
        let history2 = History::at(tmp.path().join("h2.jsonl"));
        history2.append(&entry("J", "j", "OK", 2)).unwrap();
        let status2 = compute(&cfg, &history2);
        assert!(!status2.jobs[0].scheduled_missed);
    }
}
