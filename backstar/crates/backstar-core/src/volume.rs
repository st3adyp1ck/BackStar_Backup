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
    /// Filesystem name as reported, e.g. `NTFS`, `exFAT`. Informational only -- decisions
    /// are made on the capability flags, never on this string.
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
}

/// The volume root for a path, e.g. `D:\foo\bar` -> `D:\`.
///
/// Returns `None` for UNC and extended-length paths, which have no drive-letter root.
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    let s = path.as_os_str().to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Some(PathBuf::from(format!("{}:\\", bytes[0] as char)));
    }
    None
}

/// Probe a destination path's volume.
#[cfg(windows)]
pub fn probe(path: &Path) -> crate::Result<VolumeCapability> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    };

    let root = volume_root(path)
        .ok_or_else(|| crate::Error::Unresolvable(path.to_path_buf()))?;

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
        root,
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
        assert_eq!(volume_root(Path::new(r"c:\x")), Some(PathBuf::from(r"c:\")));
        assert_eq!(volume_root(Path::new(r"D:\")), Some(PathBuf::from(r"D:\")));
        // UNC has no drive-letter root.
        assert_eq!(volume_root(Path::new(r"\\server\share\f")), None);
        assert_eq!(volume_root(Path::new("relative")), None);
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
