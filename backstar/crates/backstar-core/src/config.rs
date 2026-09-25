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
//! over the target; a corrupt file is quarantined loudly rather than silently reset. The
//! temp file's name is unique to the writer -- `<name>.<pid>.<counter>.tmp` -- because two
//! processes writing the same target concurrently (the GUI and a headless scheduled run,
//! say) would otherwise interleave their bytes into one shared temp file (F3).
//!
//! **History is append-only JSONL.** The original re-serialised the whole array on every run,
//! which is both the truncation window above and the cause of a quirk the reader had to
//! tolerate: piping a single object through `ConvertTo-Json` produced a bare object rather
//! than a one-element array. One JSON object per line has neither problem.

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
///
/// D2 note: there used to be an `Engine::{Native, Robocopy}` selector on `Config`. The
/// robocopy fallback was never wired to anything, and a backup mode that behaves
/// differently from the tested one is a liability -- it was removed outright rather than
/// left as a dead dial. Old config files carrying `"engine": "native"` (or `"robocopy"`)
/// still load: unknown fields are ignored on deserialize.
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
    /// Run `git gc --auto` on any source that is a git repository, before copying it
    /// (D2: the knob was advertised in the UI from day one, so it does what it says).
    /// Best-effort: git missing, a non-zero exit, or a timeout warns and never fails the
    /// run. See `engine::run_job`.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// Keep this many snapshots per job before pruning the oldest at the end of a run
    /// (D2). `None` means keep everything. Pruning is per-JOB: one job's retention never
    /// deletes another job's snapshots, even at a shared destination. Note what this
    /// means honestly: a backup with retention DOES delete old snapshots -- the README's
    /// old "never deletes" claim was corrected accordingly.
    #[serde(default)]
    pub keep_snapshots: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            jobs: Vec::new(),
            keep_snapshots: Some(30),
        }
    }
}

/// The result of loading a config file: the config itself, plus anything the user must
/// be told (F19). The shell surfaces `notice` as a banner.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLoad {
    pub config: Config,
    /// A loud, human-readable notice about something that went wrong but was recovered
    /// from -- a corrupt config quarantined and restarted from defaults. `None` when the
    /// load was uneventful (including "no file yet").
    pub notice: Option<String>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(state_dir()?.join("config.json"))
    }

    /// Load the config at `path`, never losing information silently:
    ///
    /// - **Absent** -- defaults, no notice. A fresh install is not a warning.
    /// - **Unparseable JSON** -- the file is renamed to `<name>.corrupt-<timestamp>`
    ///   (best-effort) so it is preserved for manual recovery, and defaults come back
    ///   with a notice naming the quarantine file. The PowerShell app did the opposite:
    ///   a bare catch returned an empty config and the next save OVERWROTE the damaged
    ///   file, destroying the evidence.
    /// - **Unreadable file (io error)** -- a hard error. A permission or transient IO
    ///   problem must not quarantine a config that may be perfectly fine.
    /// - **Newer schema version** -- a hard error, always: loading a newer config with
    ///   an older binary is downgrade corruption, and recovery must not paper over it.
    pub fn load_from(path: &Path) -> Result<ConfigLoad> {
        if !path.exists() {
            return Ok(ConfigLoad { config: Self::default(), notice: None });
        }
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let cfg: Self = match serde_json::from_str(strip_bom(&text)) {
            Ok(cfg) => cfg,
            Err(parse_error) => return Ok(Self::quarantine_corrupt(path, &parse_error)),
        };
        if cfg.schema_version > SCHEMA_VERSION {
            return Err(Error::RepoSchema {
                found: cfg.schema_version,
                understood: SCHEMA_VERSION,
            });
        }
        Ok(ConfigLoad { config: cfg, notice: None })
    }

    /// Rename an unparseable config aside and return defaults plus the notice. The rename
    /// is best-effort: if it fails, the defaults still come back and the notice says the
    /// original could not be moved (so the next save will overwrite it -- stated plainly,
    /// because that is exactly the old app's silent-history-loss shape).
    fn quarantine_corrupt(path: &Path, parse_error: &serde_json::Error) -> ConfigLoad {
        let quarantined = path.with_file_name(format!(
            "{}.corrupt-{}",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            crate::repo::snapshot_id_from(OffsetDateTime::now_utc())
        ));
        let moved = std::fs::rename(path, &quarantined).is_ok();
        tracing::warn!(
            "config at {} is corrupt ({parse_error}); {}",
            path.display(),
            if moved { format!("quarantined to {}", quarantined.display()) } else { "could not be quarantined".into() }
        );
        let notice = if moved {
            format!(
                "Your BackStar configuration at {} was corrupt and could not be read \
                 ({parse_error}). It was preserved as {} and BackStar started from a fresh \
                 configuration -- your backups on disk are unaffected; re-create your jobs.",
                path.display(),
                quarantined.display()
            )
        } else {
            format!(
                "Your BackStar configuration at {} was corrupt and could not be read \
                 ({parse_error}), and it could not be moved aside -- saving will overwrite \
                 it. BackStar started from a fresh configuration; your backups on disk are \
                 unaffected.",
                path.display()
            )
        };
        ConfigLoad { config: Self::default(), notice: Some(notice) }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Write bytes to `path` atomically: temp file, flush to the device, rename over, then
/// flush the containing DIRECTORY so the rename itself is durable (L5).
///
/// `std::fs::rename` on Windows replaces an existing file atomically, so a reader either
/// sees the whole old file or the whole new one -- never a truncated middle.
///
/// The temp file sits next to the target (so the rename stays on one volume) under a name
/// unique to this write: `<name>.<pid>.<counter>.tmp`. The old fixed `<name>.tmp` let two
/// concurrent writers open and truncate the SAME temp file, interleaving two payloads into
/// one torn result -- which then renamed perfectly atomically into place, valid JSON
/// replaced by splice (F3).
///
/// Residual window (documented, not hidden): the directory flush is best-effort. On
/// Windows a directory can be opened with `FILE_FLAG_BACKUP_SEMANTICS` and flushed like a
/// file; if that open or flush fails -- a filesystem that does not support it, say -- the
/// write is still atomic (readers never see a torn file) but a power loss at exactly the
/// wrong moment could lose the rename, leaving the pre-write file in place. No write
/// ordering can do better without transactions, which NTFS retired.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backstar".into());
    let tmp = path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        let f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        let mut w = BufWriter::new(f);
        w.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
        w.flush().map_err(|e| Error::io(&tmp, e))?;
        // Flush the OS cache to the device, not just to the filesystem. Without this the
        // rename can land before the data does, which is exactly the USB-yank case.
        w.get_ref().sync_all().map_err(|e| Error::io(&tmp, e))?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(path, e)
    })?;
    sync_parent_dir(path);
    Ok(())
}

/// Flush a file's containing directory so the rename that produced it is durable (L5).
/// Best-effort by contract: any failure is ignored and documented on [`write_atomic`].
fn sync_parent_dir(path: &Path) {
    let Some(parent) = path.parent() else { return };
    if parent.as_os_str().is_empty() {
        return;
    }
    #[cfg(windows)]
    let opened = {
        use std::os::windows::fs::OpenOptionsExt;
        // Required to open a directory handle at all.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)
    };
    #[cfg(not(windows))]
    let opened = std::fs::File::open(parent);
    if let Ok(dir) = opened {
        // File::sync_all is FlushFileBuffers on this handle.
        let _ = dir.sync_all();
    }
}

/// One completed run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// RFC 3339 with offset.
    pub timestamp: String,
    pub job: String,
    /// The job's id when the run was recorded (F40). Empty for entries written before
    /// this field existed -- those match by name (the pre-F40 keying), which is why a
    /// renamed job keeps its history while two legacy same-named jobs may share it.
    #[serde(default)]
    pub job_id: String,
    /// `None` for mirror runs and cancelled runs (a cancelled run's partial snapshot is
    /// deleted, so there is no id to point at).
    pub snapshot_id: Option<String>,
    pub duration_ms: u64,
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_failed: u64,
    /// Files a mirror run deleted (staged through the trash, D3). Zero for snapshot
    /// runs. (F54: it now reaches history.)
    #[serde(default)]
    pub files_deleted: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

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


    /// Every JSON file the PowerShell app wrote carries a UTF-8 BOM, because
    /// `Set-Content -Encoding UTF8` emits one on PS 5.1 -- and a hand editor can add one
    /// to our own files. Without stripping it, parsing fails at "line 1 column 1", which
    /// reads like corruption rather than an encoding difference.
    #[test]
    fn a_utf8_bom_does_not_break_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let json = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.notice, None, "a clean load has nothing to report");
    }

    /// D4/F46: the legacy import machinery is gone, and its vestiges in old config files
    /// are inert -- an `importNotes` array in a written config is ignored on load.
    #[test]
    fn configs_carrying_import_notes_from_a_legacy_import_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"schemaVersion": 2, "jobs": [], "importNotes": ["Imported settings from D:\\app"]}"#,
        )
        .unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert!(loaded.config.jobs.is_empty());
        assert_eq!(loaded.config.keep_snapshots, None, "absent field takes the serde default");
        assert_eq!(loaded.notice, None);
    }

    #[test]
    fn config_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");

        let mut cfg = Config::default();
        cfg.jobs.push(job_at(PathBuf::from(r"D:\Backups")));
        cfg.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.config, cfg);
        assert_eq!(back.notice, None);
    }

    /// F19: a corrupt config must be quarantined loudly -- preserved under a new name,
    /// with a notice for the UI banner -- never silently overwritten from a bare catch
    /// the way the PowerShell app lost people's history.
    #[test]
    fn a_corrupt_config_is_quarantined_and_reported_not_silently_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, b"{ \"schemaVersion\": 2, not json at all").unwrap();

        let loaded = Config::load_from(&path).unwrap();

        assert_eq!(loaded.config, Config::default(), "defaults keep the app usable");
        let notice = loaded.notice.expect("a corrupt config must produce a notice");
        assert!(notice.contains("corrupt"), "{notice}");
        assert!(!path.exists(), "the corrupt file was moved aside");

        // The original bytes are preserved under the quarantine name.
        let quarantined: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("config.json.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file: {quarantined:?}");
        let preserved = std::fs::read_to_string(tmp.path().join(&quarantined[0])).unwrap();
        assert!(preserved.contains("not json at all"), "evidence preserved");
        assert!(notice.contains(&quarantined[0]), "the notice names the quarantine file");

        // And the next load is a clean fresh start -- no repeated alarm.
        let again = Config::load_from(&path).unwrap();
        assert_eq!(again.config, Config::default());
        assert_eq!(again.notice, None);
    }

    /// An io error is NOT corruption: a transient read failure (or a permissions
    /// problem) must hard-error rather than quarantine a config that may be fine.
    #[test]
    fn an_unreadable_config_is_an_error_not_a_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::create_dir(&path).unwrap(); // a directory cannot be read as a file

        assert!(Config::load_from(&path).is_err());
        assert!(path.is_dir(), "nothing was moved aside");
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

    /// D2: the `engine` knob is gone, but config files written while it existed must keep
    /// loading -- unknown fields are ignored on deserialize, so both spellings load with
    /// no quarantine false-alarm.
    #[test]
    fn configs_carrying_the_removed_engine_field_still_load() {
        for engine_value in [r#""native""#, r#""robocopy""#] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                format!(r#"{{"schemaVersion": 2, "engine": {engine_value}, "jobs": []}}"#),
            )
            .unwrap();
            let loaded = Config::load_from(&path).unwrap();
            assert!(loaded.config.jobs.is_empty());
            assert_eq!(
                loaded.config.keep_snapshots, None,
                "an absent keepSnapshots takes the field's serde default (None = keep everything)"
            );
            assert_eq!(loaded.notice, None, "an inert removed field is not corruption");
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "x.json")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    /// F3: two writers racing `write_atomic` on the SAME target must never produce a torn
    /// file. With the old fixed `<name>.tmp` temp name, both writers truncated and filled
    /// one shared temp file, and whichever rename landed last installed a splice of two
    /// payloads -- atomically, so the corruption was invisible to readers.
    #[test]
    fn concurrent_atomic_writes_never_leave_torn_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        // Large enough that an interleaved write is obviously invalid JSON.
        let payload = |tag: u32| format!("{{\"writer\":{tag},\"pad\":\"{}\"}}", "x".repeat(4096));

        std::thread::scope(|s| {
            for w in 0..2u32 {
                let path = &path;
                let payload = &payload;
                s.spawn(move || {
                    for i in 0..200 {
                        write_atomic(path, payload(w * 1000 + i).as_bytes()).unwrap();
                    }
                });
            }
        });

        // Whatever survived the last rename must be ONE writer's whole payload.
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text)
            .expect("the surviving file must be valid JSON, not a splice of two writes");
        assert!(value.get("writer").and_then(|w| w.as_u64()).is_some());
        assert_eq!(value["pad"].as_str().unwrap().len(), 4096);

        let leftovers: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.json")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[test]
    fn history_appends_and_survives_a_corrupt_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let h = History::at(&path);

        let entry = HistoryEntry {
            timestamp: "2026-09-15T14:03:22-06:00".into(),
            job: "Projects".into(),
            job_id: "proj-1".into(),
            snapshot_id: Some("2026-09-15T14-03-22Z".into()),
            duration_ms: 1234,
            files_copied: 10,
            files_linked: 90,
            files_failed: 0,
            files_deleted: 0,
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

    /// F40/F54: history lines written before `jobId`/`filesDeleted` existed must still
    /// load -- `job_id` empty (match by name, the old keying) and `files_deleted` zero.
    #[test]
    fn old_history_lines_without_job_id_or_files_deleted_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-09-15T14:03:22-06:00","job":"Projects","snapshotId":"2026-09-15T14-03-22Z","durationMs":1,"filesCopied":1,"filesLinked":0,"filesFailed":0,"bytesCopied":1,"dest":"D:\\B","outcome":"ok"}"#,
        )
        .unwrap();
        let read = History::at(&path).read().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].job_id, "", "legacy lines have no id -- they match by name");
        assert_eq!(read[0].files_deleted, 0);
        // And the wire names of the new fields are camelCase when written.
        let entry = HistoryEntry {
            timestamp: "2026-09-15T14:03:22-06:00".into(),
            job: "Projects".into(),
            job_id: "proj-1".into(),
            snapshot_id: None,
            duration_ms: 1,
            files_copied: 1,
            files_linked: 0,
            files_failed: 0,
            files_deleted: 2,
            bytes_copied: 1,
            dest: PathBuf::from(r"D:\B"),
            outcome: "ok".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"jobId\":\"proj-1\""), "{json}");
        assert!(json.contains("\"filesDeleted\":2"), "{json}");
    }
}
