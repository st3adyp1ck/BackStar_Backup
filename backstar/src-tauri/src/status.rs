//! The "am I safe right now?" computation.
//!
//! Deliberately done in Rust rather than in the UI. Whether a backup is trustworthy depends
//! on facts only the backend can establish -- whether the source folders still exist, whether
//! the destination volume is even mounted, whether it can do hardlinks, and whether source
//! and destination sit on the same physical disk. Computing it here means there is one
//! answer, and the UI renders it rather than inferring it.

use std::path::Path;

use backstar_core::config::{Config, History, HistoryEntry};
use backstar_core::volume::{self, VolumeCapability};
use serde::Serialize;

/// How stale a backup has to be before it stops counting as current.
const STALE_AFTER_HOURS: i64 = 48;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    pub job_name: String,
    pub dest: String,
    pub missing_sources: Vec<String>,
    pub source_count: usize,
    pub last_run: Option<HistoryEntry>,
    pub snapshot_count: usize,
    pub volume: Option<VolumeCapability>,
    pub same_volume_as_source: bool,
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
    pub import_notes: Vec<String>,
    pub engine: String,
    pub version: String,
}

fn count_snapshots(dest: &Path) -> usize {
    let snaps = dest.join("snapshots");
    let Ok(rd) = std::fs::read_dir(&snaps) else {
        return 0;
    };
    rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count()
}

/// Whether two paths live on the same volume, compared by serial number rather than by
/// drive letter so that a `subst`ed or remounted drive is still recognised.
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

pub fn compute(cfg: &Config, history: &History) -> AppStatus {
    let entries = history.read().unwrap_or_default();

    let jobs: Vec<JobStatus> = cfg
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .map(|job| {
            let missing: Vec<String> = job
                .sources
                .iter()
                .filter(|s| !s.is_dir())
                .map(|s| s.display().to_string())
                .collect();

            let last_run = entries
                .iter().rfind(|e| e.job == job.name)
                .cloned();

            // Resolved, not literal: after a drive-letter change, the literal `dest` may
            // point at a different, unrelated volume entirely (or nothing), which would
            // make every check below wrong in a way that looks like the backup vanished
            // rather than like the drive just moved.
            let dest = job.resolve_dest();
            let same_vol = job.sources.iter().any(|s| same_volume(s, &dest));

            JobStatus {
                job_id: job.id.clone(),
                job_name: job.name.clone(),
                dest: dest.display().to_string(),
                missing_sources: missing,
                source_count: job.sources.len(),
                last_run,
                snapshot_count: count_snapshots(&dest),
                volume: volume::probe(&dest).ok(),
                same_volume_as_source: same_vol,
            }
        })
        .collect();

    let (protection, headline, detail) = summarise(&jobs);

    AppStatus {
        protection,
        headline,
        detail,
        jobs,
        import_notes: cfg.import_notes.clone(),
        engine: format!("{:?}", cfg.engine).to_lowercase(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Reduce every job's state to one honest sentence.
///
/// The ordering matters: a real problem always outranks a reassuring summary. A tool that
/// says "Protected" while a source folder is missing is worse than one that says nothing.
fn summarise(jobs: &[JobStatus]) -> (Protection, String, String) {
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

    if let Some(j) = jobs.iter().find(|j| j.volume.is_none()) {
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

    let never_run = jobs.iter().all(|j| j.last_run.is_none());
    if never_run {
        return (
            Protection::NeverRun,
            "No backup has run yet".into(),
            "Everything is configured, but nothing has been backed up. Run a job to create \
             your first snapshot."
                .into(),
        );
    }

    let newest = jobs
        .iter()
        .filter_map(|j| j.last_run.as_ref())
        .filter_map(|e| hours_since(&e.timestamp))
        .min();

    match newest {
        Some(h) if h <= STALE_AFTER_HOURS => {
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
                if h < 1 {
                    "Backed up just now".into()
                } else {
                    format!("Backed up {h} hour{} ago", if h == 1 { "" } else { "s" })
                },
                detail,
            )
        }
        Some(h) => (
            Protection::Stale,
            format!("Last backup was {} day{} ago", h / 24, if h / 24 == 1 { "" } else { "s" }),
            "Anything you have changed since then is not backed up.".into(),
        ),
        None => (
            Protection::NeverRun,
            "No backup has run yet".into(),
            "Run a job to create your first snapshot.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backstar_core::config::{Job, JobKind};
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
        let js = JobStatus {
            job_id: "j".into(),
            job_name: "J".into(),
            dest: r"D:\B".into(),
            missing_sources: vec![r"D:\gone".into()],
            source_count: 1,
            last_run: Some(HistoryEntry {
                timestamp: time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
                job: "J".into(),
                snapshot_id: None,
                duration_ms: 1,
                files_copied: 1,
                files_linked: 0,
                files_failed: 0,
                bytes_copied: 1,
                dest: PathBuf::from(r"D:\B"),
                outcome: "ok".into(),
            }),
            snapshot_count: 1,
            volume: None,
            same_volume_as_source: false,
        };
        let (p, head, detail) = summarise(&[js]);
        assert!(matches!(p, Protection::AtRisk));
        assert!(head.contains("missing"));
        assert!(detail.contains(r"D:\gone"));
    }

    #[test]
    fn configured_but_never_run_is_never_run_not_protected() {
        let js = JobStatus {
            job_id: "j".into(),
            job_name: "J".into(),
            dest: r"D:\B".into(),
            missing_sources: vec![],
            source_count: 1,
            last_run: None,
            snapshot_count: 0,
            volume: Some(VolumeCapability {
                root: PathBuf::from(r"D:\"),
                filesystem: "NTFS".into(),
                hard_links: true,
                serial: 1,
                guid_path: None,
            }),
            same_volume_as_source: false,
        };
        let (p, _, _) = summarise(&[js]);
        assert!(matches!(p, Protection::NeverRun));
    }

    #[test]
    fn same_volume_is_called_out_even_when_protected() {
        let ts = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let js = JobStatus {
            job_id: "j".into(),
            job_name: "J".into(),
            dest: r"D:\B".into(),
            missing_sources: vec![],
            source_count: 1,
            last_run: Some(HistoryEntry {
                timestamp: ts,
                job: "J".into(),
                snapshot_id: None,
                duration_ms: 1,
                files_copied: 1,
                files_linked: 0,
                files_failed: 0,
                bytes_copied: 1,
                dest: PathBuf::from(r"D:\B"),
                outcome: "ok".into(),
            }),
            snapshot_count: 1,
            volume: Some(VolumeCapability {
                root: PathBuf::from(r"D:\"),
                filesystem: "NTFS".into(),
                hard_links: true,
                serial: 1,
                guid_path: None,
            }),
            same_volume_as_source: true,
        };
        let (p, _, detail) = summarise(&[js]);
        assert!(matches!(p, Protection::Protected));
        assert!(
            detail.contains("same"),
            "a same-volume backup must say so even when otherwise healthy"
        );
    }

    #[test]
    fn disabled_jobs_are_not_counted() {
        let mut cfg = Config::default();
        let mut j = job_with(vec![], PathBuf::from(r"D:\B"));
        j.enabled = false;
        cfg.jobs.push(j);

        let tmp = tempfile::tempdir().unwrap();
        let status = compute(&cfg, &History::at(tmp.path().join("h.jsonl")));
        assert!(status.jobs.is_empty());
        assert!(matches!(status.protection, Protection::NoJobs));
    }
}
