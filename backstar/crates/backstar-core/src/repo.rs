//! The on-disk backup repository.
//!
//! ```text
//! <dest>/
//!   .backstar/
//!     repo.json                    repo identity and schema
//!     snapshots/<id>.json          one manifest per snapshot
//!   snapshots/<id>/<source>/...    the files themselves, as a plain tree
//!   RESTORE-README.txt             how to recover with no software at all
//! ```
//!
//! Two properties are deliberate.
//!
//! **A snapshot is an ordinary folder tree.** Not a pack file, not a database. You can open
//! it in Explorer and drag a file out. Content-addressed formats give better deduplication,
//! but they make the backup unreadable without the tool that wrote it -- and the worst
//! moment to discover your backup needs special software is the moment you need it.
//!
//! **The repository identifies itself.** It carries a UUID in `repo.json`, so the app can
//! find it again by scanning volumes rather than by remembering a drive letter. The
//! PowerShell app stored the destination as a path relative to the app folder
//! (`app/BackStar.Config.ps1:15-32`), which only helped when the destination happened to
//! live inside the app folder, and stored an absolute path -- drive letter and all --
//! otherwise.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::write_atomic;
use crate::events::RunOutcome;
use crate::{Error, Result};

pub const REPO_SCHEMA: u32 = 1;
pub const META_DIR: &str = ".backstar";
pub const SNAPSHOTS_DIR: &str = "snapshots";
pub const README_NAME: &str = "RESTORE-README.txt";
pub const RESTORE_TOOL_NAME: &str = "BackStar-Restore.exe";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoMeta {
    pub id: String,
    pub schema: u32,
    pub created: String,
    pub label: String,
}

/// What one snapshot recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotManifest {
    pub id: String,
    pub job: String,
    pub started: String,
    pub finished: String,
    /// The source roots this snapshot captured, keyed by the subfolder name they occupy
    /// inside the snapshot. This replaces `.backstar-manifest.json`, which held a single
    /// `OriginalSource` string per destination folder and was only written for non-mirror
    /// jobs -- so anyone who used Live Sync had no restore mapping at all.
    pub sources: Vec<SnapshotSource>,
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_failed: u64,
    pub bytes_copied: u64,
    pub bytes_linked: u64,
    pub duration_ms: u64,
    /// A real tagged enum, not a `Debug`-formatted string. It briefly was one
    /// (`format!("{outcome:?}")`), which meant a UI parsing it back out would have been
    /// matching on Rust's `Debug` output -- exactly the "structured value flattened to text
    /// and now something has to re-parse it" shape this whole rewrite exists to get away
    /// from. `RunOutcome` already derives `Serialize`/`Deserialize`, so it goes in directly.
    pub outcome: RunOutcome,
    /// The snapshot this one linked unchanged files against, if any.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSource {
    /// Subfolder inside the snapshot.
    pub name: String,
    /// Where this tree came from, so a restore can offer to put it back.
    pub original: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

/// One entry when listing the children of a path inside a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEntry {
    pub name: String,
    pub is_dir: bool,
    /// `None` for directories, matching how [`crate::presets::Preset`] and [`crate::config::Job`]
    /// use `None` elsewhere for "not applicable" rather than an invented `0`.
    pub size: Option<u64>,
}

/// A backup repository rooted at a destination folder.
#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    pub meta: RepoMeta,
}

/// Format a timestamp as a filename-safe snapshot id: `2026-09-15T14-03-22Z`.
///
/// Colons are illegal in Windows filenames, so the usual RFC 3339 rendering cannot be used
/// as a directory name.
pub fn snapshot_id_from(ts: OffsetDateTime) -> String {
    let t = ts.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn rfc3339(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

impl Repo {
    fn meta_path(root: &Path) -> PathBuf {
        root.join(META_DIR).join("repo.json")
    }

    /// Open an existing repository, or initialise one if the folder is empty of repo data.
    pub fn open_or_init(root: impl AsRef<Path>, label: &str) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let meta_path = Self::meta_path(&root);

        if meta_path.is_file() {
            let text = std::fs::read_to_string(&meta_path).map_err(|e| Error::io(&meta_path, e))?;
            let meta: RepoMeta = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
            if meta.schema > REPO_SCHEMA {
                return Err(Error::RepoSchema { found: meta.schema, understood: REPO_SCHEMA });
            }
            return Ok(Self { root, meta });
        }

        std::fs::create_dir_all(root.join(META_DIR).join(SNAPSHOTS_DIR))
            .map_err(|e| Error::io(&root, e))?;
        std::fs::create_dir_all(root.join(SNAPSHOTS_DIR)).map_err(|e| Error::io(&root, e))?;

        let meta = RepoMeta {
            id: uuid::Uuid::new_v4().to_string(),
            schema: REPO_SCHEMA,
            created: rfc3339(OffsetDateTime::now_utc()),
            label: label.to_string(),
        };
        write_atomic(&meta_path, serde_json::to_string_pretty(&meta)?.as_bytes())?;

        let repo = Self { root, meta };
        repo.write_readme()?;
        Ok(repo)
    }

    /// Open an existing repository, failing if there is none.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let meta_path = Self::meta_path(&root);
        if !meta_path.is_file() {
            return Err(Error::NotARepo(root));
        }
        let text = std::fs::read_to_string(&meta_path).map_err(|e| Error::io(&meta_path, e))?;
        let meta: RepoMeta = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
        if meta.schema > REPO_SCHEMA {
            return Err(Error::RepoSchema { found: meta.schema, understood: REPO_SCHEMA });
        }
        Ok(Self { root, meta })
    }

    pub fn snapshots_root(&self) -> PathBuf {
        self.root.join(SNAPSHOTS_DIR)
    }

    pub fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.snapshots_root().join(id)
    }

    /// List the immediate children of a path inside a snapshot. An empty `snapshot_rel`
    /// means the snapshot's own root, whose children are exactly the backed-up sources.
    ///
    /// Directories sort before files, each alphabetically -- matches
    /// `Add-RestoreTreeChildren` (`app/BackStar.UI.HistoryRestore.ps1:118-138`), minus the
    /// special-cased skip for `.backstar-manifest.json`: that file no longer lives inside
    /// the copied tree at all, so there is nothing left to hide from the listing.
    ///
    /// Shared by the installed app's restore browser and the standalone
    /// `BackStar-Restore` tool, so both present identical listings from one implementation.
    pub fn list_snapshot_children(
        &self,
        snapshot_id: &str,
        snapshot_rel: &Path,
    ) -> Result<Vec<SnapshotEntry>> {
        let dir = if snapshot_rel.as_os_str().is_empty() {
            self.snapshot_dir(snapshot_id)
        } else {
            self.snapshot_dir(snapshot_id).join(snapshot_rel)
        };

        let read = std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in read {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let md = entry.metadata().map_err(|e| Error::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if md.is_dir() {
                dirs.push(SnapshotEntry { name, is_dir: true, size: None });
            } else {
                files.push(SnapshotEntry { name, is_dir: false, size: Some(md.len()) });
            }
        }
        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        dirs.extend(files);
        Ok(dirs)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(META_DIR).join(SNAPSHOTS_DIR).join(format!("{id}.json"))
    }

    /// Snapshot ids present on disk, oldest first.
    ///
    /// Read from the snapshot directories rather than from the manifests, because the
    /// directories are the actual backup. A manifest that failed to write must not make a
    /// real snapshot invisible.
    pub fn snapshot_ids(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.snapshots_root()) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // Ids are lexicographically ordered by construction.
        ids.sort();
        ids
    }

    /// The most recent snapshot, which is what a new run links unchanged files against.
    pub fn latest_snapshot(&self) -> Option<String> {
        self.snapshot_ids().pop()
    }

    /// Reserve an unused snapshot id for a run starting at `ts`.
    ///
    /// Ids are second-resolution so they stay readable in Explorer, which means two runs
    /// started within the same second collide. Reusing a colliding id is not a cosmetic
    /// problem: the second run would write into the first run's directory, merging two
    /// points in time into one and making edited files appear inside a snapshot taken
    /// before the edit. A disambiguating suffix is appended instead.
    ///
    /// The suffix is zero-padded so that string ordering stays chronological -- `-002`
    /// before `-010`, which a bare counter would get backwards.
    pub fn allocate_snapshot_id(&self, ts: OffsetDateTime) -> Result<String> {
        let base = snapshot_id_from(ts);
        if !self.snapshot_dir(&base).exists() {
            return Ok(base);
        }
        for n in 2..1000u32 {
            let candidate = format!("{base}-{n:03}");
            if !self.snapshot_dir(&candidate).exists() {
                return Ok(candidate);
            }
        }
        Err(Error::other(format!(
            "could not allocate a snapshot id for {base}: 999 snapshots already exist for \
             that second"
        )))
    }

    pub fn read_manifest(&self, id: &str) -> Result<SnapshotManifest> {
        let p = self.manifest_path(id);
        let text = std::fs::read_to_string(&p).map_err(|e| Error::io(&p, e))?;
        Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
    }

    pub fn write_manifest(&self, m: &SnapshotManifest) -> Result<()> {
        write_atomic(&self.manifest_path(&m.id), serde_json::to_string_pretty(m)?.as_bytes())
    }

    pub fn manifests(&self) -> Vec<SnapshotManifest> {
        self.snapshot_ids().iter().filter_map(|id| self.read_manifest(id).ok()).collect()
    }

    /// Remove a snapshot and its manifest.
    ///
    /// Deleting a snapshot directory only removes *names*. Any file still hardlinked from
    /// another snapshot keeps its data; the filesystem does the reference counting.
    pub fn delete_snapshot(&self, id: &str) -> Result<()> {
        let dir = self.snapshot_dir(id);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        }
        let m = self.manifest_path(id);
        if m.is_file() {
            std::fs::remove_file(&m).map_err(|e| Error::io(&m, e))?;
        }
        Ok(())
    }

    /// Which snapshots to delete to honour a keep-last-N policy, oldest first.
    ///
    /// Returns ids rather than deleting, so the caller can show the user what will go.
    pub fn snapshots_to_prune(&self, keep: u32) -> Vec<String> {
        let ids = self.snapshot_ids();
        if keep == 0 || ids.len() <= keep as usize {
            return Vec::new();
        }
        ids[..ids.len() - keep as usize].to_vec()
    }

    /// Write the plain-text recovery instructions that sit in the backup root.
    ///
    /// This is the bottom tier of the restore story. The portable restore executable can be
    /// missing, blocked by antivirus, or built for the wrong architecture; a text file
    /// explaining that the snapshots are ordinary folders cannot.
    pub fn write_readme(&self) -> Result<()> {
        let text = format!(
            "BackStar backup\n\
             ===============\n\n\
             Repository id : {}\n\
             Created       : {}\n\n\
             HOW TO GET YOUR FILES BACK WITHOUT ANY SOFTWARE\n\
             -----------------------------------------------\n\
             Every backup is a plain folder of ordinary files. There is no archive to\n\
             unpack and no database to repair.\n\n\
               1. Open the `{snapshots}` folder next to this file.\n\
               2. Each subfolder is one point in time, named by date and time in UTC,\n\
                  newest last. For example: 2026-09-15T14-03-22Z\n\
               3. Inside it, your files sit in the same folder structure they had\n\
                  originally.\n\
               4. Copy what you need out with File Explorer, or any file manager.\n\n\
             That is the whole recovery procedure.\n\n\
             A NOTE ON DISK SPACE\n\
             --------------------\n\
             Snapshots share storage: a file that did not change between two snapshots is\n\
             stored once and appears in both. This is why many snapshots take far less room\n\
             than their combined size suggests, and why deleting one older snapshot may free\n\
             less space than you expect.\n\n\
             Because the copies share storage, editing a file inside a snapshot changes it in\n\
             every snapshot that contains it. Copy files OUT before editing them.\n\n\
             WHAT IS IN `{meta}`\n\
             ---------------------\n\
             Bookkeeping only: which snapshots exist, where each source folder came from, and\n\
             how each run went. Your files are never stored there. If that folder is lost or\n\
             damaged, the snapshots above are still complete and still readable.\n",
            self.meta.id,
            self.meta.created,
            snapshots = SNAPSHOTS_DIR,
            meta = META_DIR,
        );
        write_atomic(&self.root.join(README_NAME), text.as_bytes())
    }

    /// Copy `tool_exe` into the repo root as [`RESTORE_TOOL_NAME`], atomically and
    /// overwriting any existing copy -- "a copy of the restore tool ships inside every
    /// backup, refreshed on every run" in practice. Called once per successful run, from
    /// the installed app (which knows where its own sibling `BackStar-Restore.exe` lives;
    /// `backstar-core` itself has no notion of "the currently running executable").
    ///
    /// A missing `tool_exe` is not an error: it means this build has no portable restore
    /// tool to ship -- a dev build, most likely, or an unsupported platform -- and the
    /// repository is still fully recoverable via [`Self::write_readme`] alone.
    pub fn refresh_restore_tool(&self, tool_exe: &Path) -> Result<()> {
        if !tool_exe.is_file() {
            return Ok(());
        }
        let bytes = std::fs::read(tool_exe).map_err(|e| Error::io(tool_exe, e))?;
        write_atomic(&self.root.join(RESTORE_TOOL_NAME), &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_ids_are_filename_safe_and_sort_chronologically() {
        let a = snapshot_id_from(
            OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap(),
        );
        let b = snapshot_id_from(
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        );
        // Colons are illegal in Windows filenames; the id must not contain one.
        assert!(!a.contains(':'), "snapshot id must be a legal filename: {a}");
        assert!(a < b, "ids must sort chronologically as strings: {a} vs {b}");
        assert_eq!(a.len(), b.len(), "fixed width keeps string sorting correct");
    }

    #[test]
    fn init_creates_the_layout_and_a_readme() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "Test").unwrap();

        assert!(tmp.path().join(META_DIR).join("repo.json").is_file());
        assert!(tmp.path().join(SNAPSHOTS_DIR).is_dir());
        assert!(tmp.path().join(README_NAME).is_file());
        assert_eq!(repo.meta.schema, REPO_SCHEMA);
        assert!(!repo.meta.id.is_empty());
    }

    #[test]
    fn reopening_keeps_the_same_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Repo::open_or_init(tmp.path(), "Test").unwrap();
        let b = Repo::open_or_init(tmp.path(), "Different Label").unwrap();
        assert_eq!(a.meta.id, b.meta.id, "repo id must be stable across opens");
        assert_eq!(a.meta.created, b.meta.created);
    }

    #[test]
    fn opening_a_plain_folder_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(Repo::open(tmp.path()), Err(Error::NotARepo(_))));
    }

    #[test]
    fn a_newer_repo_schema_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let mut meta = repo.meta.clone();
        meta.schema = 99;
        write_atomic(
            &Repo::meta_path(tmp.path()),
            serde_json::to_string_pretty(&meta).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(matches!(Repo::open(tmp.path()), Err(Error::RepoSchema { found: 99, .. })));
    }

    #[test]
    fn snapshots_are_listed_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        for id in ["2026-01-02T00-00-00Z", "2025-06-01T00-00-00Z", "2026-09-15T14-03-22Z"] {
            std::fs::create_dir_all(repo.snapshot_dir(id)).unwrap();
        }
        assert_eq!(
            repo.snapshot_ids(),
            vec!["2025-06-01T00-00-00Z", "2026-01-02T00-00-00Z", "2026-09-15T14-03-22Z"]
        );
        assert_eq!(repo.latest_snapshot().unwrap(), "2026-09-15T14-03-22Z");
    }

    #[test]
    fn listing_children_puts_directories_before_files_both_alphabetical() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let snap = repo.snapshot_dir("2026-01-01T00-00-00Z");
        std::fs::create_dir_all(snap.join("zzz_dir")).unwrap();
        std::fs::create_dir_all(snap.join("aaa_dir")).unwrap();
        std::fs::write(snap.join("bbb.txt"), "b").unwrap();
        std::fs::write(snap.join("aaa.txt"), "a").unwrap();

        let entries = repo
            .list_snapshot_children("2026-01-01T00-00-00Z", Path::new(""))
            .unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["aaa_dir", "zzz_dir", "aaa.txt", "bbb.txt"]);
        assert!(entries[0].is_dir && entries[0].size.is_none());
        assert!(!entries[2].is_dir && entries[2].size == Some(1));
    }

    #[test]
    fn listing_a_nested_path_inside_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let snap = repo.snapshot_dir("2026-01-01T00-00-00Z");
        std::fs::create_dir_all(snap.join("proj/sub")).unwrap();
        std::fs::write(snap.join("proj/sub/deep.txt"), "deep").unwrap();

        let entries = repo
            .list_snapshot_children("2026-01-01T00-00-00Z", Path::new("proj/sub"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "deep.txt");
        assert_eq!(entries[0].size, Some(4));
    }

    #[test]
    fn listing_a_path_that_does_not_exist_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        assert!(repo
            .list_snapshot_children("nope", Path::new(""))
            .is_err());
    }

    /// A snapshot directory with no manifest must still be listed. The directory is the
    /// backup; the manifest is bookkeeping.
    #[test]
    fn a_snapshot_without_a_manifest_is_still_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        std::fs::create_dir_all(repo.snapshot_dir("2026-09-15T14-03-22Z")).unwrap();
        assert_eq!(repo.snapshot_ids().len(), 1);
        assert!(repo.read_manifest("2026-09-15T14-03-22Z").is_err());
        assert!(repo.manifests().is_empty());
    }

    #[test]
    fn manifests_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let m = SnapshotManifest {
            id: "2026-09-15T14-03-22Z".into(),
            job: "Projects".into(),
            started: "2026-09-15T14:03:22Z".into(),
            finished: "2026-09-15T14:05:00Z".into(),
            sources: vec![SnapshotSource {
                name: "northstar_coding".into(),
                original: PathBuf::from(r"D:\northstar_coding"),
                files: 2888,
                bytes: 65_000_000,
            }],
            files_copied: 2888,
            files_linked: 0,
            files_failed: 0,
            bytes_copied: 65_000_000,
            bytes_linked: 0,
            duration_ms: 98_000,
            outcome: RunOutcome::Ok,
            parent: None,
        };
        repo.write_manifest(&m).unwrap();
        assert_eq!(repo.read_manifest(&m.id).unwrap(), m);
        assert_eq!(repo.manifests().len(), 0, "no snapshot dir yet, so nothing is listed");

        std::fs::create_dir_all(repo.snapshot_dir(&m.id)).unwrap();
        assert_eq!(repo.manifests(), vec![m]);
    }

    /// The manifest's `outcome` field must reach disk (and, from there, the frontend) as
    /// structured JSON -- `{"partial":{"failed":2}}`, the same externally-tagged shape
    /// `RunPanel.tsx` already parses off the live event stream -- not as a `Debug`-formatted
    /// string like `"Partial { failed: 2 }"`. The two look similar enough in a terminal that
    /// this regressed silently once already; lock the wire shape down explicitly.
    #[test]
    fn outcome_serialises_as_a_tagged_object_not_a_debug_string() {
        let json = serde_json::to_string(&RunOutcome::Partial { failed: 2 }).unwrap();
        assert_eq!(json, r#"{"partial":{"failed":2}}"#);
        assert!(!json.contains("Partial {"), "a Debug string leaked into JSON: {json}");

        assert_eq!(serde_json::to_string(&RunOutcome::Ok).unwrap(), r#""ok""#);
        assert_eq!(serde_json::to_string(&RunOutcome::Cancelled).unwrap(), r#""cancelled""#);

        let back: RunOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RunOutcome::Partial { failed: 2 });
    }

    /// Regression test: two runs in the same second must not share a snapshot directory.
    ///
    /// When they did, the second run wrote into the first run's snapshot -- so a file
    /// edited between the two appeared, with its NEW content, inside the snapshot taken
    /// BEFORE the edit. Point-in-time recovery silently became a lie.
    #[test]
    fn a_second_run_in_the_same_second_gets_its_own_id() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

        let first = repo.allocate_snapshot_id(ts).unwrap();
        std::fs::create_dir_all(repo.snapshot_dir(&first)).unwrap();

        let second = repo.allocate_snapshot_id(ts).unwrap();
        assert_ne!(first, second, "the same timestamp must not yield the same id twice");
        std::fs::create_dir_all(repo.snapshot_dir(&second)).unwrap();

        let third = repo.allocate_snapshot_id(ts).unwrap();
        assert_ne!(third, first);
        assert_ne!(third, second);
    }

    /// The suffix must keep string ordering chronological, which a bare counter would not:
    /// `-10` sorts before `-2`.
    #[test]
    fn disambiguated_ids_still_sort_chronologically() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

        let mut made = Vec::new();
        for _ in 0..12 {
            let id = repo.allocate_snapshot_id(ts).unwrap();
            std::fs::create_dir_all(repo.snapshot_dir(&id)).unwrap();
            made.push(id);
        }

        // Allocation order is the true chronological order; listing must agree.
        assert_eq!(repo.snapshot_ids(), made);
        assert_eq!(repo.latest_snapshot().as_ref(), made.last());

        // And a later second still sorts after every suffixed id from the earlier one.
        let later = repo
            .allocate_snapshot_id(OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap())
            .unwrap();
        assert!(
            made.iter().all(|m| m.as_str() < later.as_str()),
            "a later timestamp must sort after every suffixed id: {made:?} vs {later}"
        );
    }

    #[test]
    fn pruning_keeps_the_newest_n() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let ids = [
            "2026-01-01T00-00-00Z",
            "2026-02-01T00-00-00Z",
            "2026-03-01T00-00-00Z",
            "2026-04-01T00-00-00Z",
        ];
        for id in ids {
            std::fs::create_dir_all(repo.snapshot_dir(id)).unwrap();
        }

        assert_eq!(
            repo.snapshots_to_prune(2),
            vec!["2026-01-01T00-00-00Z", "2026-02-01T00-00-00Z"],
            "the OLDEST are pruned"
        );
        assert!(repo.snapshots_to_prune(4).is_empty());
        assert!(repo.snapshots_to_prune(10).is_empty());
        // keep = 0 means "keep everything", not "delete everything". Getting this backwards
        // would silently destroy every backup.
        assert!(repo.snapshots_to_prune(0).is_empty());
    }

    /// Deleting a snapshot removes names, not data that another snapshot still references.
    #[test]
    fn deleting_a_snapshot_leaves_shared_data_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();

        let old = repo.snapshot_dir("2026-01-01T00-00-00Z");
        let new = repo.snapshot_dir("2026-02-01T00-00-00Z");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();

        let old_file = old.join("notes.txt");
        let new_file = new.join("notes.txt");
        std::fs::write(&old_file, "shared content").unwrap();
        std::fs::hard_link(&old_file, &new_file).unwrap();

        repo.delete_snapshot("2026-01-01T00-00-00Z").unwrap();

        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(&new_file).unwrap(),
            "shared content",
            "the surviving snapshot must still hold the data"
        );
        assert_eq!(repo.snapshot_ids(), vec!["2026-02-01T00-00-00Z"]);
    }

    #[test]
    fn the_readme_explains_recovery_without_software() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        let text = std::fs::read_to_string(tmp.path().join(README_NAME)).unwrap();

        assert!(text.contains(&repo.meta.id));
        assert!(text.contains("File Explorer"));
        // The shared-storage warning is the one thing a user can get badly wrong by
        // editing a file in place inside a snapshot.
        assert!(text.to_lowercase().contains("copy files out"));
    }

    #[test]
    fn refresh_restore_tool_copies_the_exe_into_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();

        let fake_exe = tmp.path().join("source").join("BackStar-Restore.exe");
        std::fs::create_dir_all(fake_exe.parent().unwrap()).unwrap();
        std::fs::write(&fake_exe, b"not really a PE file, just bytes").unwrap();

        repo.refresh_restore_tool(&fake_exe).unwrap();

        let copied = tmp.path().join(RESTORE_TOOL_NAME);
        assert_eq!(std::fs::read(&copied).unwrap(), b"not really a PE file, just bytes");
    }

    #[test]
    fn refresh_restore_tool_overwrites_a_stale_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        std::fs::write(tmp.path().join(RESTORE_TOOL_NAME), b"old version").unwrap();

        let fake_exe = tmp.path().join("source.exe");
        std::fs::write(&fake_exe, b"new version, longer than the old one").unwrap();

        repo.refresh_restore_tool(&fake_exe).unwrap();

        assert_eq!(
            std::fs::read(tmp.path().join(RESTORE_TOOL_NAME)).unwrap(),
            b"new version, longer than the old one"
        );
    }

    /// A dev build (or an unsupported platform) has no restore tool to ship. That must not
    /// fail the run it's called from -- the README-only recovery path is still complete.
    #[test]
    fn refresh_restore_tool_is_a_silent_no_op_when_the_source_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path(), "T").unwrap();
        repo.refresh_restore_tool(Path::new(r"Z:\does\not\exist.exe")).unwrap();
        assert!(!tmp.path().join(RESTORE_TOOL_NAME).exists());
    }
}
