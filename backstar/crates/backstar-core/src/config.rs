//! Configuration and run history.
//!
//! Two changes from the PowerShell implementation, both correctness fixes:
//!
//! **Writes are atomic.** `Save-Config` used `Set-Content`, which truncates and then writes
//! (`app/BackStar.Config.ps1:213`), and it ran at the *start of every run*
//! (`app/BackStar.Engine.ps1:266`). A crash or a yanked USB drive mid-write left a truncated
//! file. Worse, the recovery hid the damage: `Get-BackupHistory` returned an empty array from
//! a bare catch, and the next append rewrote the file with a single entry -- permanently
//! discarding the history. Here every write goes to a temp file, is flushed, and is renamed
//! over the target; a corrupt file is quarantined loudly rather than silently reset.
//!
//! **History is append-only JSONL.** The original re-serialised the whole array on every run,
//! which is both the truncation window above and the cause of a quirk the reader had to
//! tolerate: piping a single object through `ConvertTo-Json` produced a bare object rather
//! than a one-element array. One JSON object per line has neither problem.

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exclude::ExcludeRules;
use crate::volume::PortableDest;
use crate::{Error, Result};

pub const SCHEMA_VERSION: u32 = 2;

/// Strip a UTF-8 byte order mark.
///
/// PowerShell 5.1's `Set-Content -Encoding UTF8` writes a BOM, so every JSON file the old
/// app produced -- config, history and `.backstar-manifest.json` -- starts with one.
/// `serde_json` treats it as an unexpected character and fails at line 1 column 1, which
/// reads like a corrupt file rather than an encoding difference.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Where the installed edition keeps its state: `%LOCALAPPDATA%\BackStar`.
///
/// Deliberately *not* next to the executable. The original stored config, history and the
/// default `Backups\` folder beside the scripts so the whole folder could live on a USB
/// stick, but that couples "where the app lives" to "where state lives" and breaks the
/// moment the app is installed to Program Files. The portable edition solves portability
/// properly instead: it carries no state at all and reads the repository it sits inside.
pub fn state_dir() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA")
        .map_err(|_| Error::other("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(base).join("BackStar"))
}

/// What kind of work a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    /// Versioned snapshot. Never deletes anything the user still has a snapshot of.
    Snapshot,
    /// One-way mirror: the destination is made to match the source exactly, which means
    /// deleting files at the destination. This is the only destructive mode.
    Mirror,
}

/// A backup job: what to copy, where to, and under which rules.
///
/// This is the unification of the original's three separate modes. "Project Backup",
/// "Live Sync" and "System Backup" were three tabs re-teaching the same concepts; they are
/// one shape -- sources, a destination, a kind, and a rule set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub name: String,
    pub sources: Vec<PathBuf>,
    /// The last-known-good literal path. Always present, always kept up to date, and
    /// always the fallback when `dest_portable` is unset or its volume is not currently
    /// reachable -- so a job that has never met a drive-letter change behaves exactly as
    /// if this feature did not exist.
    pub dest: PathBuf,
    /// The destination's volume identity, captured the first time a run resolves `dest`
    /// successfully. `None` for a job that has never completed a run since this field was
    /// introduced (`#[serde(default)]` means an old config loads with this unset, not a
    /// parse error).
    #[serde(default)]
    pub dest_portable: Option<PortableDest>,
    pub kind: JobKind,
    #[serde(default)]
    pub excludes: ExcludeRules,
    /// System preset keys included in this job, if any.
    #[serde(default)]
    pub presets: Vec<String>,
    /// Run `git gc --auto` on any source that is a git repository, before copying it.
    #[serde(default)]
    pub git_gc: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When to run this job unattended, if at all. `backstar-core` itself never reads the
    /// system clock or touches Task Scheduler -- this field is purely a record of intent
    /// that the installed app's scheduling layer turns into a real scheduled task. Keeping
    /// it here rather than as OS-only state means the schedule survives an export/import of
    /// this config and is visible to anything that inspects a job, not just Task Scheduler.
    #[serde(default)]
    pub schedule: Option<DailySchedule>,
}

fn default_true() -> bool {
    true
}

/// Run a job once a day, at a fixed local time.
///
/// Deliberately the only recurrence shape offered in this phase: a personal backup tool's
/// most common need by far is "every day around the same time", and a full cron-style
/// recurrence editor is a lot of UI for a case that rarely comes up. Task Scheduler itself
/// supports far more than this; nothing here stops a later version from offering more
/// without a breaking change, since this is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySchedule {
    /// 0-23, local time.
    pub hour: u8,
    /// 0-59.
    pub minute: u8,
}

impl Job {
    /// The destination to actually use: the portable (volume-GUID) location when one has
    /// been captured and its volume is currently mounted, otherwise the literal `dest`.
    ///
    /// "Currently mounted" is checked against the volume itself, not the specific
    /// subfolder -- a job's destination folder may not exist yet on its very first run
    /// against a freshly reattached drive, and that must not force a fallback to a stale
    /// drive letter that may now point at an entirely different, unrelated volume.
    pub fn resolve_dest(&self) -> PathBuf {
        if let Some(pd) = &self.dest_portable {
            if Path::new(&pd.volume_guid_path).exists() {
                return pd.resolve();
            }
        }
        self.dest.clone()
    }

    /// Capture (or refresh) the portable record for wherever `dest` currently resolves to.
    ///
    /// Intended to run once, right after a destination has been used successfully -- there
    /// is no point recording a volume identity for a path that just failed. A no-op on
    /// failure (a UNC destination, for instance): the job simply keeps relying on the
    /// literal path, exactly as it did before this field existed.
    pub fn backfill_portable_dest(&mut self) {
        if let Ok(pd) = PortableDest::from_absolute(&self.resolve_dest()) {
            self.dest_portable = Some(pd);
        }
    }
}

/// Which copy implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    /// The native copier: real byte-level progress, hash-on-copy, hardlink-safe writes.
    #[default]
    Native,
    /// Drive `robocopy.exe`, as the PowerShell app did. Retained as an escape hatch while
    /// the native engine earns trust through differential testing.
    Robocopy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub engine: Engine,
    /// Keep this many snapshots per job before pruning. `None` means keep everything.
    #[serde(default)]
    pub keep_snapshots: Option<u32>,
    /// Notes carried forward from a legacy import, surfaced once in the UI.
    #[serde(default)]
    pub import_notes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            jobs: Vec::new(),
            engine: Engine::default(),
            keep_snapshots: Some(30),
            import_notes: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(state_dir()?.join("config.json"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let cfg: Self = serde_json::from_str(strip_bom(&text))?;
        if cfg.schema_version > SCHEMA_VERSION {
            return Err(Error::RepoSchema {
                found: cfg.schema_version,
                understood: SCHEMA_VERSION,
            });
        }
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Write bytes to `path` atomically: temp file, flush to the device, rename over.
///
/// `std::fs::rename` on Windows replaces an existing file atomically, so a reader either
/// sees the whole old file or the whole new one -- never a truncated middle.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        let mut w = BufWriter::new(f);
        w.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
        w.flush().map_err(|e| Error::io(&tmp, e))?;
        // Flush the OS cache to the device, not just to the filesystem. Without this the
        // rename can land before the data does, which is exactly the USB-yank case.
        w.get_ref().sync_all().map_err(|e| Error::io(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

/// One completed run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// RFC 3339 with offset.
    pub timestamp: String,
    pub job: String,
    pub snapshot_id: Option<String>,
    pub duration_ms: u64,
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_failed: u64,
    pub bytes_copied: u64,
    pub dest: PathBuf,
    pub outcome: String,
}

/// Append-only run history.
pub struct History {
    path: PathBuf,
}

impl History {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(state_dir()?.join("history.jsonl"))
    }

    /// Append one entry. Never rewrites existing lines, so an interrupted append can at
    /// worst leave one malformed trailing line -- which [`read`] skips.
    pub fn append(&self, entry: &HistoryEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| Error::io(&self.path, e))?;
        let line = serde_json::to_string(entry)?;
        writeln!(f, "{line}").map_err(|e| Error::io(&self.path, e))?;
        f.sync_all().map_err(|e| Error::io(&self.path, e))
    }

    /// Read all entries, newest last.
    ///
    /// A line that will not parse is skipped rather than failing the whole read, and
    /// rather than the original's behaviour of returning an empty list from a bare catch
    /// -- which caused the next write to overwrite the file and lose everything.
    pub fn read(&self) -> Result<Vec<HistoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let f = std::fs::File::open(&self.path).map_err(|e| Error::io(&self.path, e))?;
        let mut out = Vec::new();
        for line in std::io::BufReader::new(f).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(strip_bom(&line)) {
                Ok(e) => out.push(e),
                Err(e) => tracing::warn!("skipping unreadable history line: {e}"),
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- legacy import

/// The PowerShell app's `BackStar.config.json` (schema 1).
#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(rename = "SchemaVersion")]
    _schema_version: Option<u32>,
    #[serde(rename = "Destination")]
    destination: Option<String>,
    #[serde(rename = "Sources")]
    sources: Option<Vec<String>>,
    #[serde(rename = "GitGc")]
    git_gc: Option<bool>,
    #[serde(rename = "SystemBackup")]
    system: Option<LegacySystem>,
}

#[derive(Debug, Deserialize)]
struct LegacySystem {
    #[serde(rename = "Destination")]
    destination: Option<String>,
    #[serde(rename = "EnabledPresets")]
    enabled_presets: Option<Vec<String>>,
    #[serde(rename = "CustomSources")]
    custom_sources: Option<Vec<String>>,
}

/// Resolve a legacy stored destination.
///
/// The original stored a destination as `.` or `.\sub` when it sat inside the app folder,
/// so a USB install kept working when the drive letter changed
/// (`app/BackStar.Config.ps1:15-32`). Anything outside the app folder was stored absolute,
/// so the trick only ever helped for destinations under the app itself.
fn resolve_legacy_dest(stored: &str, app_dir: &Path) -> PathBuf {
    let s = stored.trim();
    if s == "." {
        return app_dir.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix(r".\").or_else(|| s.strip_prefix("./")) {
        return app_dir.join(rest);
    }
    PathBuf::from(s)
}

/// Import a legacy config into schema v2.
///
/// `app_dir` is the folder the PowerShell scripts live in, which is what relative
/// destinations were stored relative to.
///
/// Sources that no longer exist are imported anyway and reported in `import_notes`: the
/// original's `[missing]` display prefix existed for the same reason. Silently dropping
/// them would hide a stale configuration rather than surface it.
pub fn import_legacy(legacy_json: &str, app_dir: &Path) -> Result<Config> {
    let legacy: LegacyConfig = serde_json::from_str(strip_bom(legacy_json))?;
    let mut cfg = Config::default();
    let mut notes = Vec::new();

    let project_sources: Vec<PathBuf> = legacy
        .sources
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect();

    for s in &project_sources {
        if !s.is_dir() {
            notes.push(format!(
                "Imported source {} does not exist on this machine.",
                s.display()
            ));
        }
    }

    if !project_sources.is_empty() {
        let dest = legacy
            .destination
            .as_deref()
            .map(|d| resolve_legacy_dest(d, app_dir))
            .unwrap_or_else(|| app_dir.join("Backups"));

        cfg.jobs.push(Job {
            id: "project".into(),
            name: "Projects".into(),
            sources: project_sources,
            dest,
            dest_portable: None,
            // Imported as a snapshot job on purpose. The original's Full Backup used a
            // plain recursive copy that never deleted, so its destination accumulated
            // every file ever deleted from the source, with no way to tell live files
            // from years-dead ones. Snapshots give the same never-lose-anything property
            // with a point in time attached.
            kind: JobKind::Snapshot,
            excludes: ExcludeRules::project(),
            presets: Vec::new(),
            git_gc: legacy.git_gc.unwrap_or(false),
            enabled: true,
            schedule: None,
        });
    }

    if let Some(sys) = legacy.system {
        let presets = sys.enabled_presets.unwrap_or_default();
        let custom: Vec<PathBuf> = sys
            .custom_sources
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .collect();

        if !presets.is_empty() || !custom.is_empty() {
            let dest = sys
                .destination
                .as_deref()
                .map(|d| resolve_legacy_dest(d, app_dir))
                .unwrap_or_else(|| app_dir.join(r"Backups\System"));

            cfg.jobs.push(Job {
                id: "system".into(),
                name: "System".into(),
                sources: custom,
                dest,
                dest_portable: None,
                kind: JobKind::Snapshot,
                excludes: ExcludeRules::system(),
                presets,
                git_gc: false,
                enabled: true,
            schedule: None,
            });
        }
    }

    cfg.import_notes = notes;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_LEGACY: &str = r#"{
        "SchemaVersion": 1,
        "Destination": ".\\Backups",
        "Sources": ["D:\\Dev-Ops\\northstar_coding"],
        "GitGc": true,
        "SystemBackup": {
            "Destination": ".\\Backups\\System",
            "EnabledPresets": ["Desktop","Documents","Pictures","Downloads","AppData","Chrome","Edge"],
            "CustomSources": []
        }
    }"#;

    fn job_at(dest: PathBuf) -> Job {
        Job {
            id: "t".into(),
            name: "Test".into(),
            sources: vec![],
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

    #[cfg(windows)]
    #[test]
    fn resolve_dest_falls_back_to_the_literal_path_with_no_portable_record() {
        let job = job_at(PathBuf::from(r"D:\Backups"));
        assert_eq!(job.resolve_dest(), PathBuf::from(r"D:\Backups"));
    }

    /// The actual point of this whole feature, proven directly: once a portable record is
    /// captured, resolution goes through the volume -- not the drive letter -- so it keeps
    /// working even when the literal `dest` field is stale or simply wrong.
    #[cfg(windows)]
    #[test]
    fn resolve_dest_prefers_the_portable_record_over_a_stale_literal_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "still here").unwrap();

        let mut job = job_at(tmp.path().to_path_buf());
        assert!(job.dest_portable.is_none());

        job.backfill_portable_dest();
        assert!(job.dest_portable.is_some(), "backfill should have captured a portable record");

        // Corrupt the literal path -- simulating exactly what a drive-letter reassignment
        // does: the string on disk in config.json no longer points anywhere real.
        job.dest = PathBuf::from(r"Z:\this\drive\letter\does\not\exist");

        let resolved = job.resolve_dest();
        assert_ne!(resolved, job.dest, "must not have fallen back to the broken literal path");
        assert_eq!(
            std::fs::read_to_string(resolved.join("marker.txt")).unwrap(),
            "still here",
            "resolution via the volume GUID must still reach the real destination"
        );
    }

    /// The reverse: if the portable record's volume is not currently reachable at all
    /// (unplugged), fall back to the literal path rather than erroring -- the literal path
    /// might still be right (nothing moved), and even if it isn't, failing later with a
    /// clear "could not open" error beats failing here with no path to show the user at all.
    #[cfg(windows)]
    #[test]
    fn resolve_dest_falls_back_when_the_portable_volume_is_unreachable() {
        let mut job = job_at(PathBuf::from(r"D:\Backups"));
        job.dest_portable = Some(crate::volume::PortableDest {
            volume_guid_path: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".into(),
            relative: PathBuf::from("Backups"),
        });
        assert_eq!(job.resolve_dest(), PathBuf::from(r"D:\Backups"));
    }

    #[test]
    fn legacy_relative_destinations_resolve_against_the_app_folder() {
        let app = Path::new(r"D:\Backstar_Backup\app");
        assert_eq!(resolve_legacy_dest(r".\Backups", app), app.join("Backups"));
        assert_eq!(resolve_legacy_dest("./Backups", app), app.join("Backups"));
        assert_eq!(resolve_legacy_dest(".", app), app.to_path_buf());
        // Absolute values were stored verbatim and must stay verbatim.
        assert_eq!(resolve_legacy_dest(r"E:\Elsewhere", app), PathBuf::from(r"E:\Elsewhere"));
    }

    #[test]
    fn importing_the_real_legacy_config_produces_two_jobs() {
        let app = Path::new(r"D:\Backstar_Backup\app");
        let cfg = import_legacy(REAL_LEGACY, app).expect("import");

        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert_eq!(cfg.jobs.len(), 2);

        let project = &cfg.jobs[0];
        assert_eq!(project.id, "project");
        assert_eq!(project.dest, app.join("Backups"));
        assert!(project.git_gc);
        assert_eq!(project.kind, JobKind::Snapshot);
        assert!(!project.excludes.dir_names.is_empty());

        let system = &cfg.jobs[1];
        assert_eq!(system.dest, app.join(r"Backups\System"));
        assert_eq!(system.presets.len(), 7);
        // Firefox was not enabled in the real config and must not be invented.
        assert!(!system.presets.iter().any(|p| p == "Firefox"));
    }

    /// The configured source in the real file is stale: it points at
    /// `D:\Dev-Ops\northstar_coding`, which does not exist. The import must carry it
    /// forward *and say so*, rather than dropping it and leaving a job that silently
    /// backs up nothing.
    #[test]
    fn a_stale_source_is_imported_and_reported() {
        let cfg = import_legacy(REAL_LEGACY, Path::new(r"D:\Backstar_Backup\app")).unwrap();
        assert_eq!(cfg.jobs[0].sources.len(), 1);
        assert_eq!(cfg.import_notes.len(), 1);
        assert!(cfg.import_notes[0].contains("northstar_coding"));
        assert!(cfg.import_notes[0].contains("does not exist"));
    }

    /// Every JSON file the PowerShell app wrote carries a UTF-8 BOM, because
    /// `Set-Content -Encoding UTF8` emits one on PS 5.1. Without stripping it, importing
    /// the user's real config fails at "line 1 column 1" -- which looks like corruption
    /// rather than an encoding difference.
    #[test]
    fn a_utf8_bom_does_not_break_parsing() {
        let with_bom = format!("\u{feff}{REAL_LEGACY}");
        let cfg = import_legacy(&with_bom, Path::new(r"D:\app"))
            .expect("a BOM-prefixed legacy config must import");
        assert_eq!(cfg.jobs.len(), 2);

        // And the same for our own config files, in case one is hand-edited in an editor
        // that adds a BOM.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let json = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn an_empty_legacy_config_imports_to_no_jobs() {
        let cfg = import_legacy("{}", Path::new(r"D:\app")).unwrap();
        assert!(cfg.jobs.is_empty());
        assert!(cfg.import_notes.is_empty());
    }

    #[test]
    fn config_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");

        let cfg = import_legacy(REAL_LEGACY, Path::new(r"D:\app")).unwrap();
        cfg.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn loading_a_newer_schema_is_refused_rather_than_misread() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"schemaVersion": 99, "jobs": []}"#).unwrap();
        assert!(matches!(
            Config::load_from(&path),
            Err(Error::RepoSchema { found: 99, .. })
        ));
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn history_appends_and_survives_a_corrupt_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let h = History::at(&path);

        let entry = HistoryEntry {
            timestamp: "2026-09-15T14:03:22-06:00".into(),
            job: "Projects".into(),
            snapshot_id: Some("2026-09-15T14-03-22Z".into()),
            duration_ms: 1234,
            files_copied: 10,
            files_linked: 90,
            files_failed: 0,
            bytes_copied: 4096,
            dest: PathBuf::from(r"D:\Backups"),
            outcome: "ok".into(),
        };
        h.append(&entry).unwrap();
        h.append(&entry).unwrap();

        // Simulate an interrupted append leaving a partial trailing line.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{{\"timestamp\": \"trunc").unwrap();
        }

        let read = h.read().unwrap();
        assert_eq!(read.len(), 2, "good lines survive a corrupt trailing line");
        assert_eq!(read[0], entry);
    }

    #[test]
    fn reading_absent_history_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let h = History::at(tmp.path().join("nope.jsonl"));
        assert!(h.read().unwrap().is_empty());
    }
}
