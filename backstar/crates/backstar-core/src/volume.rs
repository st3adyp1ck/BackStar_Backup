//! Volume capability probing and stable volume identity.
//!
//! Two jobs, both load-bearing for the snapshot design:
//!
//! **Can this volume do hardlinks?** Snapshots store unchanged files as additional names for
//! the same data, so N snapshots cost roughly one snapshot's worth of disk. exFAT and FAT32
//! have no hardlinks at all -- `CreateHardLinkW` fails with `ERROR_NOT_SUPPORTED`. That is a
//! hard capability boundary, not a slow path, and a backup drive formatted exFAT must fall
//! back to a visibly degraded single-version mode rather than silently pretend.
//!
//! The probe asks the filesystem for `FILE_SUPPORTS_HARD_LINKS`, never the filesystem *name*.
//! Matching on "NTFS" would wrongly exclude ReFS (which supports links) and would not account
//! for network redirectors that report unusual names.
//!
//! **Which volume is this, really?** Drive letters move. A USB drive that was `E:` last week
//! can be `F:` today, and the original app's answer -- storing the destination as a path
//! relative to the app folder (`app/BackStar.Config.ps1:15-32`) -- only worked when the
//! destination happened to live *inside* the app folder, and stored an absolute path
//! otherwise. The volume GUID is stable across remounts and relettering.
//!
//! Two boundary cases are handled honestly rather than by assumption (F20):
//!
//! - **NTFS folder mount-points.** A USB volume can be mounted with no letter at all, at
//!   e.g. `C:\mnt\usb`. Truncating such a path to its drive letter hands it C:'s identity:
//!   the mounted-check then passes with the drive REMOVED, and writes land silently on the
//!   system drive. [`volume_root`] therefore asks the filesystem for the true mount point
//!   (`GetVolumePathNameW`) and only falls back to the lexical drive letter when the API
//!   cannot answer.
//! - **UNC/network paths** (`\\server\share`). They have no volume GUID and no capability
//!   flags to ask locally. [`probe_destination`] answers with a dedicated
//!   [`ProbeOutcome::Network`] variant -- conservative capabilities (no hard links, coarse
//!   timestamps, since remote filesystem semantics vary by server) and NO volume identity
//!   -- instead of an error that the shell would render as a false "drive unavailable"
//!   alarm for a healthy NAS.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `FILE_SUPPORTS_HARD_LINKS`. Defined here rather than imported so the meaning of the bit
/// is visible at the point of use.
#[cfg(windows)]
const FILE_SUPPORTS_HARD_LINKS: u32 = 0x0040_0000;

/// What a destination volume can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeCapability {
    /// The volume root, e.g. `D:\`.
    pub root: PathBuf,
    /// Filesystem name as reported, e.g. `NTFS`, `exFAT`. Decisions are made on the
    /// capability flags, never on this string -- with one deliberate exception:
    /// [`VolumeCapability::stores_coarse_mtimes`], because no capability flag exists
    /// for timestamp granularity.
    pub filesystem: String,
    /// Whether hardlinked snapshots are possible here.
    pub hard_links: bool,
    /// Volume serial number. A weaker identity than the GUID but always available.
    pub serial: u32,
    /// Stable volume GUID path, e.g. `\\?\Volume{...}\`. `None` for network paths, which
    /// have no volume GUID.
    pub guid_path: Option<String>,
}

impl VolumeCapability {
    /// The user-facing consequence of `hard_links == false`.
    pub fn degraded_reason(&self) -> Option<String> {
        if self.hard_links {
            return None;
        }
        Some(format!(
            "{} is formatted {}, which does not support hard links. Versioned snapshots are \
             not possible here, so each backup costs its full size. Format the drive as NTFS \
             to enable space-free versioning.",
            self.root.display(),
            self.filesystem
        ))
    }

    /// Whether this volume's filesystem keeps modification times at FAT's two-second
    /// resolution (FAT12/16/32, exFAT).
    ///
    /// This one decision IS made on the filesystem name, deliberately: there is no
    /// capability flag for timestamp granularity, and the FAT family is a closed list.
    /// Everything else in this module keys off capability flags, never the name.
    pub fn stores_coarse_mtimes(&self) -> bool {
        matches!(
            self.filesystem.to_uppercase().as_str(),
            "FAT" | "FAT12" | "FAT16" | "FAT32" | "EXFAT"
        )
    }
}

/// Whether a path is a UNC/network path (`\\server\share`, including the verbatim
/// `\\?\UNC\server\share` form canonicalisation produces). Purely lexical -- performs no
/// network IO, which matters: probing a share's reachability can block for seconds.
fn is_unc_path(path: &Path) -> bool {
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => matches!(
            p.kind(),
            std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)
        ),
        _ => false,
    }
}

/// The volume root for a path: the volume's true mount point, e.g. `D:\` -- or
/// `C:\mnt\usb\` for a volume mounted only as an NTFS folder mount-point (F20).
///
/// On Windows this asks the filesystem (`GetVolumePathNameW`), because truncating to the
/// drive letter would hand a folder-mounted volume its HOST volume's identity -- the
/// mounted-check then passes with the drive removed and writes land on the wrong volume.
/// The lexical drive-letter rule remains as a fallback for paths the API cannot answer
/// (a drive that is not mounted at all, for instance).
///
/// Returns `None` for UNC paths, which have no local volume root.
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(root) = volume_root_via_api(path) {
        return Some(root);
    }
    volume_root_lexical(path)
}

/// The lexical fallback: drive letter, or drive letter under a verbatim prefix.
fn volume_root_lexical(path: &Path) -> Option<PathBuf> {
    let s = path.as_os_str().to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Some(PathBuf::from(format!("{}:\\", bytes[0] as char)));
    }
    None
}

/// `GetVolumePathNameW`: resolves the deepest mount point above `path`, which is exactly
/// the folder-mount case the lexical rule gets wrong. The path's leaf need not exist --
/// the API resolves through the longest existing prefix -- but it must be fully
/// qualified, so a relative input is absolutised against the current directory first.
#[cfg(windows)]
fn volume_root_via_api(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumePathNameW;

    if is_unc_path(path) {
        return None;
    }
    let owned;
    let full = if path.is_absolute() {
        path
    } else {
        owned = std::env::current_dir().ok()?.join(path);
        &owned
    };
    let wide: Vec<u16> = full.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 32_768];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut buf) }.ok()?;
    let end = buf.iter().position(|&c| c == 0)?;
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..end])))
}

/// Probe a destination path's volume.
#[cfg(windows)]
pub fn probe(path: &Path) -> crate::Result<VolumeCapability> {
    let root = volume_root(path)
        .ok_or_else(|| crate::Error::Unresolvable(path.to_path_buf()))?;
    probe_root(&root)
}

/// Probe the volume mounted at `root` (e.g. `D:\` or `\\?\Volume{...}\`).
#[cfg(windows)]
fn probe_root(root: &Path) -> crate::Result<VolumeCapability> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    };

    let wide: Vec<u16> =
        root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let mut fs_name = [0u16; 64];
    let mut serial: u32 = 0;
    let mut flags: u32 = 0;

    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut serial),
            None,
            Some(&mut flags),
            Some(&mut fs_name),
        )
    }
    .map_err(|e| crate::Error::other(format!("GetVolumeInformationW({}): {e}", root.display())))?;

    let filesystem = String::from_utf16_lossy(&fs_name)
        .trim_end_matches('\0')
        .to_string();

    // The GUID path is best-effort: a network drive has none, and that is not an error.
    let guid_path = {
        let mut buf = [0u16; 64];
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut buf).is_ok()
        };
        if ok {
            Some(String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string())
        } else {
            None
        }
    };

    Ok(VolumeCapability {
        root: root.to_path_buf(),
        filesystem,
        hard_links: flags & FILE_SUPPORTS_HARD_LINKS != 0,
        serial,
        guid_path,
    })
}

#[cfg(not(windows))]
pub fn probe(path: &Path) -> crate::Result<VolumeCapability> {
    Err(crate::Error::other(format!(
        "volume probing is Windows-only (asked about {})",
        path.display()
    )))
}

/// The honest answer to "what can the destination's volume do" for EVERY shape of
/// destination (F20) -- including network paths, which have no local volume to interrogate.
///
/// Serialised for the shell: the internal `kind` tag distinguishes the variants on the
/// wire (`{"kind":"network",...}`), never an ambiguous bare object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ProbeOutcome {
    /// A local volume, fully probed.
    Local(VolumeCapability),
    /// A UNC/network path (`\\server\share`). There is no volume GUID to pin an identity
    /// to and no local capability flags to ask; assume the conservative capabilities --
    /// NO hard links and COARSE timestamps, because remote filesystem semantics vary by
    /// server. This is not an error: a NAS is a perfectly good destination, it just must
    /// not be reported as "drive unavailable" (the old false alarm) nor trusted with
    /// versioning assumptions it may not honour.
    Network { path: PathBuf },
    /// A local path whose volume cannot be interrogated at all -- a drive letter with
    /// nothing mounted behind it, most commonly. This is the only genuinely unavailable
    /// case, and it is what the shell may render as "drive unavailable".
    Unavailable { path: PathBuf, reason: String },
}

impl ProbeOutcome {
    /// Whether unchanged files can be hardlinked into new snapshots here. `Network`
    /// conservatively answers no (remote semantics vary); `Unavailable` answers yes --
    /// claiming degradation we cannot prove would be a false alarm, and the run will
    /// fail on its own io errors soon enough.
    pub fn supports_hard_links(&self) -> bool {
        match self {
            ProbeOutcome::Local(cap) => cap.hard_links,
            ProbeOutcome::Network { .. } => false,
            ProbeOutcome::Unavailable { .. } => true,
        }
    }

    /// Whether the destination should be treated as storing coarse (two-second) mtimes.
    /// Local volumes answer from the filesystem name (the only source of that fact);
    /// network paths answer yes, because a remote server's timestamp granularity is not
    /// ours to know.
    pub fn stores_coarse_mtimes(&self) -> bool {
        match self {
            ProbeOutcome::Local(cap) => cap.stores_coarse_mtimes(),
            ProbeOutcome::Network { .. } => true,
            ProbeOutcome::Unavailable { .. } => false,
        }
    }
}

/// Probe the volume behind a destination path, answering honestly for every shape
/// (F20). UNC paths short-circuit to [`ProbeOutcome::Network`] BEFORE any network IO --
/// probing a share's reachability can block for seconds, and reachability is the
/// caller's business, not this function's.
pub fn probe_destination(path: &Path) -> ProbeOutcome {
    if is_unc_path(path) {
        return ProbeOutcome::Network { path: path.to_path_buf() };
    }
    match probe(path) {
        Ok(cap) => ProbeOutcome::Local(cap),
        Err(e) => ProbeOutcome::Unavailable { path: path.to_path_buf(), reason: e.to_string() },
    }
}

/// Whether `path` sits on a FAT-family volume (two-second mtime resolution), or `None`
/// when that cannot be determined.
///
/// Unlike [`probe`], this also understands volume-GUID roots, because portable
/// destinations resolve to exactly that shape (`\\?\Volume{...}\...`) -- an exFAT USB
/// drive reached through its portable record must still be recognised as FAT-family,
/// or the mtime tolerance that keeps it from recopying everything every run vanishes.
///
/// UNC/network paths answer `Some(true)` (F20): a remote server's timestamp granularity
/// is not ours to know, so the FAT-family tolerance is applied -- the cost is recopying
/// a changed-within-two-seconds file never, versus the cost of an exact comparison being
/// recopying files whose mtimes a server rounded. See [`ProbeOutcome::stores_coarse_mtimes`].
#[cfg(windows)]
pub(crate) fn probe_coarse_mtimes(path: &Path) -> Option<bool> {
    if is_unc_path(path) {
        return Some(true);
    }
    if let Ok(cap) = probe(path) {
        return Some(cap.stores_coarse_mtimes());
    }
    let s = path.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\Volume{") {
        if let Some(end) = s.find("}\\") {
            let root = PathBuf::from(&s[..end + 2]);
            if let Ok(cap) = probe_root(&root) {
                return Some(cap.stores_coarse_mtimes());
            }
        }
    }
    None
}

#[cfg(not(windows))]
pub(crate) fn probe_coarse_mtimes(_path: &Path) -> Option<bool> {
    None
}

/// A destination expressed as a volume identity plus a path within it, so it survives a
/// drive-letter change.
///
/// Windows lets any mounted volume be addressed by its GUID path
/// (`\\?\Volume{...}\`) regardless of which drive letter -- if any -- is currently
/// assigned to it. That means resolving a `PortableDest` never has to search drive letters
/// at all: the GUID path itself is a valid root for ordinary file APIs as long as the
/// volume is mounted somewhere. This is the actual fix for the problem the PowerShell app's
/// app-folder-relative-path trick only solved when the destination happened to live inside
/// the app's own folder (`app/BackStar.Config.ps1:15-32`) -- everywhere else it fell back to
/// an absolute, drive-lettered path that a re-plugged USB drive would silently invalidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableDest {
    /// e.g. `\\?\Volume{c3344fd7-1234-...}\`. Always ends in a separator.
    pub volume_guid_path: String,
    /// Path within that volume, with no drive letter.
    pub relative: PathBuf,
}

impl PortableDest {
    /// Capture an absolute path as a volume identity plus a within-volume path.
    ///
    /// Fails for paths with no drive-letter volume root (UNC shares) and for volumes that
    /// report no GUID path (also, in practice, network redirectors).
    #[cfg(windows)]
    pub fn from_absolute(path: &Path) -> crate::Result<Self> {
        let canon = crate::guards::canonical_path(path)?;
        let root = volume_root(&canon).ok_or_else(|| crate::Error::Unresolvable(canon.clone()))?;
        let cap = probe(&canon)?;
        let volume_guid_path = cap.guid_path.ok_or_else(|| {
            crate::Error::other(format!(
                "{} has no volume GUID path -- likely a network location, which a portable \
                 destination cannot be pinned to",
                root.display()
            ))
        })?;
        let relative = canon
            .strip_prefix(&root)
            .map_err(|_| crate::Error::Unresolvable(canon.clone()))?
            .to_path_buf();
        Ok(Self { volume_guid_path, relative })
    }

    #[cfg(not(windows))]
    pub fn from_absolute(path: &Path) -> crate::Result<Self> {
        Err(crate::Error::other(format!(
            "portable destinations are Windows-only (asked about {})",
            path.display()
        )))
    }

    /// Resolve back to a real, usable path -- valid as long as the volume is mounted
    /// *somewhere*, under any drive letter or none.
    ///
    /// This does not check the path actually exists; callers that need to distinguish "the
    /// volume isn't plugged in" from "the volume is plugged in but this particular folder
    /// is gone" should stat the result themselves.
    pub fn resolve(&self) -> PathBuf {
        Path::new(&self.volume_guid_path).join(&self.relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_root_extracts_the_drive() {
        assert_eq!(volume_root(Path::new(r"D:\foo\bar")), Some(PathBuf::from(r"D:\")));
        // The API answer normalises the letter's case -- `c:\x` lives on `C:\`.
        #[cfg(windows)]
        assert_eq!(volume_root(Path::new(r"c:\x")), Some(PathBuf::from(r"C:\")));
        #[cfg(not(windows))]
        assert_eq!(volume_root(Path::new(r"c:\x")), Some(PathBuf::from(r"c:\")));
        assert_eq!(volume_root(Path::new(r"D:\")), Some(PathBuf::from(r"D:\")));
        // UNC has no local volume root.
        assert_eq!(volume_root(Path::new(r"\\server\share\f")), None);
    }

    /// F20: `GetVolumePathNameW` must resolve a real path to its actual volume root, which
    /// for a drive-lettered path is that drive's root -- and, for a folder-mounted volume,
    /// the mount point rather than the host drive's letter (that case needs a second
    /// volume, so it is not constructible here; the drive-letter case is the regression
    /// net for the API plumbing itself).
    #[cfg(windows)]
    #[test]
    fn volume_root_resolves_a_real_path_through_the_api() {
        let tmp = tempfile::tempdir().unwrap();
        let root = volume_root(tmp.path()).expect("a real local path must resolve");
        let expected_letter = tmp.path().to_string_lossy().chars().next().unwrap();
        assert_eq!(
            root,
            PathBuf::from(format!("{}:\\", expected_letter.to_ascii_uppercase())),
            "the temp dir's own drive root"
        );
        // A not-yet-existing leaf below an existing prefix still resolves.
        let future = tmp.path().join("not-here-yet").join("ever");
        assert_eq!(volume_root(&future), Some(root));
    }

    /// F20: a UNC destination is a NETWORK answer, never an error and never a phantom
    /// local probe. Deterministic: the check is lexical and performs no network IO.
    #[test]
    fn a_unc_destination_probes_as_network_not_as_unavailable() {
        let unc = Path::new(r"\\server\share\backups");
        match probe_destination(unc) {
            ProbeOutcome::Network { path } => assert_eq!(path, unc),
            other => panic!("expected Network, got: {other:?}"),
        }
        // The verbatim-UNC spelling canonicalisation produces must agree.
        assert!(matches!(
            probe_destination(Path::new(r"\\?\UNC\server\share\backups")),
            ProbeOutcome::Network { .. }
        ));
        // Conservative capabilities, by policy: no hard links, coarse mtimes.
        assert!(!probe_destination(unc).supports_hard_links());
        assert!(probe_destination(unc).stores_coarse_mtimes());
    }

    #[cfg(windows)]
    #[test]
    fn a_local_destination_probes_as_local() {
        let tmp = tempfile::tempdir().unwrap();
        match probe_destination(tmp.path()) {
            ProbeOutcome::Local(cap) => {
                assert!(!cap.filesystem.is_empty());
                assert!(tmp.path().starts_with(&cap.root) || cap.root.starts_with(tmp.path()));
            }
            other => panic!("expected Local, got: {other:?}"),
        }
    }

    /// The genuinely unavailable case -- a drive letter with nothing mounted behind it --
    /// is the ONLY one the shell may render as "drive unavailable".
    #[cfg(windows)]
    #[test]
    fn a_missing_drive_probes_as_unavailable() {
        let missing = (b'A'..=b'Z')
            .rev()
            .map(|c| format!(r"{}:\definitely\not\here", c as char))
            .find(|p| std::fs::symlink_metadata(Path::new(&p[..3])).is_err())
            .expect("some drive letter must be free");
        assert!(matches!(
            probe_destination(Path::new(&missing)),
            ProbeOutcome::Unavailable { .. }
        ));
    }

    /// F20/F8: the mtime tolerance consults the same picture -- UNC answers coarse,
    /// because a remote server's timestamp granularity is not ours to know.
    #[cfg(windows)]
    #[test]
    fn unc_paths_are_treated_as_coarse_mtime_volumes() {
        assert_eq!(probe_coarse_mtimes(Path::new(r"\\server\share\x")), Some(true));
    }

    #[test]
    fn probe_outcome_serialises_with_a_kind_tag() {
        let net = ProbeOutcome::Network { path: PathBuf::from(r"\\nas\backups") };
        let json = serde_json::to_string(&net).unwrap();
        assert!(json.contains(r#""kind":"network""#), "{json}");
        let back: ProbeOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(net, back);
    }

    /// Probes the volume this checkout lives on. Asserts the probe answers coherently
    /// rather than asserting a specific filesystem, so it holds on any machine.
    #[cfg(windows)]
    #[test]
    fn probing_a_real_volume_reports_coherent_capabilities() {
        let here = std::env::current_dir().expect("cwd");
        let cap = probe(&here).expect("probe should succeed for a local path");

        assert!(!cap.filesystem.is_empty(), "filesystem name should be reported");
        assert_ne!(cap.serial, 0, "a real volume should have a serial");

        // The capability bit and the filesystem name should agree for the two cases we
        // can reason about confidently. This is the assertion that would catch a probe
        // reading the wrong flag bit.
        match cap.filesystem.to_uppercase().as_str() {
            "NTFS" | "REFS" => assert!(
                cap.hard_links,
                "{} reports no hard-link support, which means the flag test is wrong",
                cap.filesystem
            ),
            "EXFAT" | "FAT32" | "FAT" => assert!(
                !cap.hard_links,
                "{} cannot support hard links",
                cap.filesystem
            ),
            _ => {}
        }

        // A capable volume must not advertise a degraded reason, and vice versa.
        assert_eq!(cap.hard_links, cap.degraded_reason().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn probe_rejects_paths_with_no_volume_root() {
        assert!(probe(Path::new(r"\\server\share\x")).is_err());
    }

    /// The load-bearing proof: a path captured as a `PortableDest` and resolved back via
    /// its volume GUID -- never via the drive letter it started with -- must point at the
    /// SAME real file on disk. If this ever fails, the entire "survives a drive-letter
    /// change" property is fiction.
    #[cfg(windows)]
    #[test]
    fn a_portable_dest_resolves_to_the_same_real_file_via_its_guid_path() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let marker = tmp.path().join("marker.txt");
        std::fs::write(&marker, "identity check").unwrap();

        let pd = PortableDest::from_absolute(&marker).expect("should capture a local path");
        assert!(
            pd.volume_guid_path.starts_with(r"\\?\Volume{"),
            "unexpected guid path shape: {}",
            pd.volume_guid_path
        );
        assert!(pd.volume_guid_path.ends_with('\\'));

        let resolved = pd.resolve();
        // Not the same spelling as the original (no drive letter at all)...
        assert_ne!(resolved, marker);
        // ...but the same file, reached a completely different way.
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "identity check",
            "resolving via the volume GUID must reach the same file the drive letter did"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_portable_dest_captures_the_relative_path_without_a_drive_letter() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let pd = PortableDest::from_absolute(&nested).unwrap();
        // A drive letter anywhere in `relative` would mean re-plugging the same drive at a
        // different letter breaks resolution right back down to the very thing this type
        // exists to avoid.
        let rel_str = pd.relative.to_string_lossy();
        assert!(!rel_str.contains(':'), "relative path must not carry a drive letter: {rel_str}");
        assert!(rel_str.ends_with("a\\b") || rel_str.ends_with("a/b"), "got: {rel_str}");
    }

    #[cfg(windows)]
    #[test]
    fn a_unc_path_cannot_become_a_portable_dest() {
        assert!(PortableDest::from_absolute(Path::new(r"\\server\share\x")).is_err());
    }

    /// The FAT family is a closed list -- the coarse-mtime decision must recognise every
    /// member and nothing else.
    #[test]
    fn fat_family_names_are_recognised_exactly() {
        let cap = |fs: &str| VolumeCapability {
            root: PathBuf::from(r"D:\"),
            filesystem: fs.into(),
            hard_links: true,
            serial: 1,
            guid_path: None,
        };
        for fs in ["FAT", "FAT12", "FAT16", "FAT32", "exFAT", "ExFat"] {
            assert!(cap(fs).stores_coarse_mtimes(), "{fs} must read as coarse");
        }
        for fs in ["NTFS", "ReFS", "CDFS", "UDF", ""] {
            assert!(!cap(fs).stores_coarse_mtimes(), "{fs} must not read as coarse");
        }
    }

    /// `probe_coarse_mtimes` must answer for the temp volume -- and, crucially, also for
    /// the same place spelled through its volume-GUID root, the shape portable
    /// destinations actually resolve to.
    #[cfg(windows)]
    #[test]
    fn coarse_mtimes_is_detected_through_drive_letter_and_volume_guid() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = probe_coarse_mtimes(tmp.path())
            .expect("a drive-lettered local path must probe");

        let pd = PortableDest::from_absolute(tmp.path()).unwrap();
        let guid_spelling = pd.resolve();
        let via_guid = probe_coarse_mtimes(&guid_spelling)
            .expect("a volume-GUID spelling of the same place must probe");

        assert_eq!(
            plain, via_guid,
            "drive-letter and volume-GUID spellings of one volume must agree"
        );
    }

    #[test]
    fn portable_dest_round_trips_through_json() {
        let pd = PortableDest {
            volume_guid_path: r"\\?\Volume{11111111-2222-3333-4444-555555555555}\".into(),
            relative: PathBuf::from(r"Backups\Projects"),
        };
        let json = serde_json::to_string(&pd).unwrap();
        assert!(json.contains("volumeGuidPath"), "expected camelCase field, got: {json}");
        let back: PortableDest = serde_json::from_str(&json).unwrap();
        assert_eq!(pd, back);
    }
}
