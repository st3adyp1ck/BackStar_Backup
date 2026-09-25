//! F27: which lock-prone preset apps are running right now.
//!
//! Browser profiles are SQLite databases held open while the browser runs; backing one up
//! without a volume shadow copy can capture a torn write that only surfaces as a corrupt
//! database at restore time (see `presets.rs` in the engine). The JobEditor renders a
//! warning when a selected preset is live-locked; this module is the backend for it.
//!
//! Process enumeration goes through the Toolhelp snapshot API, not a `tasklist`
//! subprocess: no console window, no text scraping, no locale dependence.

use backstar_core::presets::{locking_processes, LOCK_PRONE_PRESETS};

/// The pure matching core, testable off-OS: preset keys whose locking processes intersect
/// the running-process name list (case-insensitive, with or without the `.exe`).
pub(crate) fn lock_prone_running(running_names: &[String]) -> Vec<String> {
    LOCK_PRONE_PRESETS
        .iter()
        .filter(|key| {
            locking_processes(key).iter().any(|proc| {
                running_names
                    .iter()
                    .any(|r| r.trim_end_matches(".exe").eq_ignore_ascii_case(proc))
            })
        })
        .map(|s| s.to_string())
        .collect()
}

/// Every running process's image name (e.g. `chrome.exe`), via a Toolhelp snapshot.
/// Returns empty on failure -- the warning simply doesn't show, which is the pre-F27
/// behavior and never a false alarm.
#[cfg(windows)]
fn running_process_names() -> Vec<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut names = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return names;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_string();
                names.push(name);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    names
}

/// The preset keys whose owning application is currently running (F27). The JobEditor
/// warns inline when a selected preset appears here.
#[tauri::command(async)]
pub fn list_lock_prone_running() -> Vec<String> {
    #[cfg(windows)]
    {
        lock_prone_running(&running_process_names())
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_case_insensitive_and_exe_suffix_tolerant() {
        let running: Vec<String> = vec!["chrome.exe".into(), "explorer.exe".into()];
        assert_eq!(lock_prone_running(&running), vec!["Chrome".to_string()]);

        let running: Vec<String> = vec!["FIREFOX".into()];
        assert_eq!(lock_prone_running(&running), vec!["Firefox".to_string()]);

        let running: Vec<String> = vec!["explorer.exe".into()];
        assert!(lock_prone_running(&running).is_empty());

        // A name that merely CONTAINS the process name must not match.
        let running: Vec<String> = vec!["chromedriver.exe".into()];
        assert!(lock_prone_running(&running).is_empty());
    }

    /// The Toolhelp snapshot plumbing itself: must not panic, and this test process is
    /// necessarily in the list.
    #[cfg(windows)]
    #[test]
    fn process_snapshot_smoke_test() {
        let names = running_process_names();
        assert!(!names.is_empty(), "a process snapshot must list something");
        assert!(names.iter().any(|n| n.to_lowercase().contains("backstar")));
    }
}
