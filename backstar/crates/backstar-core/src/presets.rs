//! System backup presets -- the well-known user folders offered on the System tab.
//!
//! Ported from `Get-SystemPresetDefinitions` (`app/BackStar.Config.ps1:80-120`).
//!
//! **Paths are resolved fresh on every launch and never persisted.** Only the preset key
//! goes into config. That is what lets one config file work across machines and user
//! accounts, and it is why a preset carries a `found` flag rather than being silently
//! dropped when its folder is absent.
//!
//! **Why not just join `%USERPROFILE%`.** Any of these folders can be relocated by the user
//! (Properties -> Location -> Move), which is common for Downloads when a large downloads
//! folder is kept off a small SSD. Windows leaves an empty stub behind at the old location,
//! so a hardcoded guess passes an existence check, the preset shows as found, and the backup
//! silently copies an empty folder while the real one is never touched. The original worked
//! around this for Downloads specifically by reading the known-folder GUID out of
//! `HKCU\...\Explorer\User Shell Folders`, because .NET's `GetFolderPath` has no Downloads
//! member. `SHGetKnownFolderPath` has no such gap, so every preset here goes through the
//! authoritative API -- the registry key that workaround read is what this API reads anyway.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A backup preset as presented to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    /// Stable identifier. Persisted in config, and doubles as the destination subfolder
    /// name -- so it must stay unique against user-added custom folder names too.
    pub key: String,
    /// Human-readable label.
    pub label: String,
    /// Resolved absolute path, or `None` when the folder could not be resolved.
    pub path: Option<PathBuf>,
    /// Whether the folder exists on this machine right now. A preset that is not found
    /// must be shown but not selectable.
    pub found: bool,
}

impl Preset {
    fn new(key: &str, label: &str, path: Option<PathBuf>) -> Self {
        let found = path.as_ref().is_some_and(|p| p.is_dir());
        Self { key: key.to_string(), label: label.to_string(), path, found }
    }
}

/// Resolve a Windows known folder by its `FOLDERID` GUID.
#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    unsafe {
        let pw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        if pw.is_null() {
            return None;
        }
        let s = pw.to_string().ok();
        // SHGetKnownFolderPath allocates with the COM task allocator; the caller frees it.
        CoTaskMemFree(Some(pw.0 as *const _));
        s.map(PathBuf::from)
    }
}

/// The presets, in display order.
#[cfg(windows)]
pub fn all() -> Vec<Preset> {
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_LocalAppData,
        FOLDERID_Pictures, FOLDERID_RoamingAppData,
    };

    let local = known_folder(&FOLDERID_LocalAppData);
    let roaming = known_folder(&FOLDERID_RoamingAppData);

    vec![
        Preset::new("Desktop", "Desktop", known_folder(&FOLDERID_Desktop)),
        Preset::new("Documents", "Documents", known_folder(&FOLDERID_Documents)),
        Preset::new("Pictures", "Pictures", known_folder(&FOLDERID_Pictures)),
        Preset::new("Downloads", "Downloads", known_folder(&FOLDERID_Downloads)),
        Preset::new("AppData", "App Settings (AppData\\Roaming)", roaming.clone()),
        // Browser presets point at the whole profile container, so every profile is
        // captured rather than just the default one. These are also the presets that are
        // routinely locked while the browser runs -- see the note on consistency below.
        Preset::new(
            "Chrome",
            "Chrome (bookmarks & profile)",
            local.as_ref().map(|p| p.join(r"Google\Chrome\User Data")),
        ),
        Preset::new(
            "Edge",
            "Edge (bookmarks & profile)",
            local.as_ref().map(|p| p.join(r"Microsoft\Edge\User Data")),
        ),
        Preset::new(
            "Firefox",
            "Firefox (bookmarks & profile)",
            roaming.as_ref().map(|p| p.join(r"Mozilla\Firefox\Profiles")),
        ),
    ]
}

#[cfg(not(windows))]
pub fn all() -> Vec<Preset> {
    Vec::new()
}

/// Preset keys whose contents are held open by a running application, so that a copy taken
/// without a volume shadow copy may be incomplete or internally inconsistent.
///
/// Browser profiles are SQLite databases. Copying one while the browser is running can
/// capture a torn write -- the file exists at the destination and looks plausible, but the
/// database is corrupt and the failure only surfaces at restore time. The real fix is VSS,
/// which always requires elevation. Until then the UI must warn when one of these is
/// selected and its application is running.
pub const LOCK_PRONE_PRESETS: &[&str] = &["Chrome", "Edge", "Firefox"];

/// Process names that hold the corresponding preset open.
pub fn locking_processes(key: &str) -> &'static [&'static str] {
    match key {
        "Chrome" => &["chrome"],
        "Edge" => &["msedge"],
        "Firefox" => &["firefox"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn presets_are_the_expected_set_in_order() {
        let keys: Vec<_> = all().into_iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            vec![
                "Desktop",
                "Documents",
                "Pictures",
                "Downloads",
                "AppData",
                "Chrome",
                "Edge",
                "Firefox"
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn preset_keys_are_unique() {
        // The key doubles as the destination subfolder name, so a duplicate would make two
        // presets back up into the same folder.
        let presets = all();
        let mut keys: Vec<_> = presets.iter().map(|p| p.key.as_str()).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "preset keys must be unique");
    }

    #[cfg(windows)]
    #[test]
    fn core_user_folders_resolve_to_real_absolute_directories() {
        let presets = all();
        for key in ["Desktop", "Documents", "Downloads", "AppData"] {
            let p = presets.iter().find(|p| p.key == key).expect("preset present");
            let path = p.path.as_ref().unwrap_or_else(|| panic!("{key} should resolve"));
            assert!(path.is_absolute(), "{key} -> {} should be absolute", path.display());
            assert!(p.found, "{key} -> {} should exist", path.display());
        }
    }

    /// The relocation trap this module exists to avoid: the resolved Downloads folder must
    /// be whatever Windows actually says it is, which is not necessarily under the profile.
    #[cfg(windows)]
    #[test]
    fn downloads_comes_from_the_known_folder_api_not_a_profile_guess() {
        let presets = all();
        let downloads = presets.iter().find(|p| p.key == "Downloads").unwrap();
        let resolved = downloads.path.as_ref().expect("Downloads should resolve");

        // Whatever it is, it must agree with the authoritative API rather than with a
        // hardcoded %USERPROFILE%\Downloads guess.
        use windows::Win32::UI::Shell::FOLDERID_Downloads;
        let authoritative = known_folder(&FOLDERID_Downloads).expect("api should answer");
        assert_eq!(resolved, &authoritative);
    }

    #[cfg(windows)]
    #[test]
    fn a_missing_preset_is_reported_not_found_rather_than_dropped() {
        // Firefox is frequently absent. Whatever the case on this machine, the invariant
        // is that every preset is listed, and `found` reflects reality.
        let presets = all();
        assert_eq!(presets.len(), 8, "all presets are always listed");
        for p in &presets {
            match &p.path {
                Some(path) => assert_eq!(p.found, path.is_dir()),
                None => assert!(!p.found),
            }
        }
    }

    #[test]
    fn lock_prone_presets_name_their_processes() {
        for key in LOCK_PRONE_PRESETS {
            assert!(
                !locking_processes(key).is_empty(),
                "{key} is flagged lock-prone but names no process to check for"
            );
        }
        assert!(locking_processes("Documents").is_empty());
    }
}
