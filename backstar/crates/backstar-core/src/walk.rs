//! Parallel source-tree walk.
//!
//! Produces the complete file list for a job before any copying starts. That ordering is
//! deliberate: knowing the total up front is what makes an honest percentage and a real ETA
//! possible. The PowerShell app never had a denominator -- its progress bar filled by
//! *completed jobs*, so a single job was 0% until it was 100%, and the only per-file signal
//! was a percentage scraped out of robocopy's partially-written output line.
//!
//! Excluded directories are pruned during the walk rather than filtered afterwards, so a
//! `node_modules` containing 40,000 files costs one `readdir`, not 40,000 stats.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::exclude::CompiledExcludes;
use crate::{Error, Result};

/// One file found in the source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the walk root. This is what the destination layout mirrors.
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkResult {
    pub entries: Vec<Entry>,
    pub total_bytes: u64,
    /// Directories pruned by an exclusion rule.
    pub dirs_excluded: u64,
    /// Files skipped by an exclusion rule.
    pub files_excluded: u64,
    /// Reparse points (junctions and symlinks) not followed.
    pub reparse_skipped: u64,
    /// Entries that could not be read at all -- a permission error, or a path that vanished
    /// mid-walk. Counted rather than silently dropped, because "we could not look" and
    /// "there was nothing there" must not look the same.
    pub unreadable: u64,
}

/// Walk `root`, returning every file that should be backed up.
///
/// `on_progress` is called periodically with running `(files, bytes)` totals so the UI can
/// show that the scan is alive; the walk has no denominator of its own.
pub fn walk(
    root: &Path,
    excludes: &CompiledExcludes,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<WalkResult> {
    if !root.is_dir() {
        return Err(Error::Unresolvable(root.to_path_buf()));
    }

    let mut out = WalkResult::default();
    // Directories still to visit. An explicit stack rather than recursion: a deep tree
    // should not be able to blow the stack.
    let mut stack = vec![root.to_path_buf()];
    let mut since_report = 0u64;

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                out.unreadable += 1;
                continue;
            }
        };

        for entry in rd {
            let Ok(entry) = entry else {
                out.unreadable += 1;
                continue;
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // `symlink_metadata` does not follow the link, which is what lets a reparse
            // point be recognised rather than silently expanded.
            let Ok(md) = entry.metadata() else {
                out.unreadable += 1;
                continue;
            };

            if md.file_type().is_symlink() {
                // Matches robocopy's /XJ. A junction pointing at an ancestor would
                // otherwise recurse until the path length gives out.
                out.reparse_skipped += 1;
                continue;
            }

            if md.is_dir() {
                if excludes.is_dir_excluded(&path, &name) {
                    out.dirs_excluded += 1;
                    continue;
                }
                stack.push(path);
                continue;
            }

            if excludes.is_file_excluded(&name) {
                out.files_excluded += 1;
                continue;
            }

            let rel = match path.strip_prefix(root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => {
                    out.unreadable += 1;
                    continue;
                }
            };

            let size = md.len();
            out.total_bytes += size;
            out.entries.push(Entry {
                rel,
                size,
                mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });

            since_report += 1;
            if since_report >= 512 {
                since_report = 0;
                on_progress(out.entries.len() as u64, out.total_bytes);
            }
        }
    }

    on_progress(out.entries.len() as u64, out.total_bytes);

    // Stable ordering so a run is reproducible and two walks of the same tree can be
    // compared directly.
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exclude::ExcludeRules;

    fn touch(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        touch(&r.join("a.txt"), "aaa");
        touch(&r.join("src/main.rs"), "fn main() {}");
        touch(&r.join("src/deep/nested/x.txt"), "x");
        touch(&r.join("node_modules/pkg/index.js"), "junk");
        touch(&r.join("node_modules/pkg/deep/more.js"), "junk");
        touch(&r.join("Thumbs.db"), "junk");
        touch(&r.join("notes.tmp"), "junk");
        tmp
    }

    #[test]
    fn walks_every_file_when_nothing_is_excluded() {
        let tmp = fixture();
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        assert_eq!(r.entries.len(), 7);
        assert_eq!(r.dirs_excluded, 0);
        assert_eq!(r.files_excluded, 0);
    }

    #[test]
    fn excluded_directories_are_pruned_not_descended() {
        let tmp = fixture();
        let ex = ExcludeRules::project().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();

        let names: Vec<String> =
            r.entries.iter().map(|e| e.rel.to_string_lossy().to_string()).collect();
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "node_modules should be pruned, got {names:?}"
        );
        // Pruned as ONE directory, not as two separate files -- proving the walk never
        // descended into it.
        assert_eq!(r.dirs_excluded, 1);
    }

    #[test]
    fn system_file_patterns_are_applied() {
        let tmp = fixture();
        let ex = ExcludeRules::system().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        let names: Vec<String> =
            r.entries.iter().map(|e| e.rel.to_string_lossy().to_string()).collect();
        assert!(!names.iter().any(|n| n.contains("Thumbs.db")));
        assert!(!names.iter().any(|n| n.ends_with(".tmp")));
        assert_eq!(r.files_excluded, 2);
    }

    #[test]
    fn relative_paths_are_rooted_at_the_walk_root() {
        let tmp = fixture();
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        assert!(r.entries.iter().all(|e| e.rel.is_relative()));
        assert!(r.entries.iter().any(|e| e.rel == *"a.txt"));
        assert!(r
            .entries
            .iter()
            .any(|e| e.rel == PathBuf::from("src").join("deep").join("nested").join("x.txt")));
    }

    #[test]
    fn totals_match_the_entries() {
        let tmp = fixture();
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        let summed: u64 = r.entries.iter().map(|e| e.size).sum();
        assert_eq!(r.total_bytes, summed);
    }

    #[test]
    fn results_are_sorted_so_two_walks_compare_directly() {
        let tmp = fixture();
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let a = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        let b = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        assert_eq!(a.entries, b.entries);
        let mut sorted = a.entries.clone();
        sorted.sort_by(|x, y| x.rel.cmp(&y.rel));
        assert_eq!(a.entries, sorted);
    }

    /// A junction pointing at an ancestor is the classic infinite-recursion trap, and the
    /// reason robocopy is always invoked with /XJ.
    #[cfg(windows)]
    #[test]
    fn a_junction_loop_does_not_hang_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        touch(&root.join("real.txt"), "real");

        let link = root.join("loop");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&root)
            .output();
        let made = status.map(|o| o.status.success()).unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }

        let ex = ExcludeRules::none().compile(&root, Path::new(r"D:\d"));
        let r = walk(&root, &ex, |_, _| {}).unwrap();

        assert_eq!(r.entries.len(), 1, "only the real file should be walked");
        assert_eq!(r.reparse_skipped, 1, "the junction should be counted as skipped");
    }

    #[test]
    fn unicode_and_awkward_names_are_walked() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("café — Ω.txt"), "u");
        touch(&tmp.path().join("Odd Name (v2) & co/it's here.txt"), "u");
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let r = walk(tmp.path(), &ex, |_, _| {}).unwrap();
        assert_eq!(r.entries.len(), 2);
    }

    #[test]
    fn walking_a_non_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file.txt");
        touch(&f, "x");
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        assert!(walk(&f, &ex, |_, _| {}).is_err());
        assert!(walk(&tmp.path().join("missing"), &ex, |_, _| {}).is_err());
    }

    #[test]
    fn progress_is_reported_at_least_once() {
        let tmp = fixture();
        let ex = ExcludeRules::none().compile(tmp.path(), Path::new(r"D:\d"));
        let mut calls = 0;
        let r = walk(tmp.path(), &ex, |_, _| calls += 1).unwrap();
        assert!(calls >= 1);
        assert_eq!(r.entries.len(), 7);
    }
}
