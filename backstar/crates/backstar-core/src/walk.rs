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
//!
//! Two anomaly channels are reported alongside the file list, because "we could not look"
//! and "there was nothing there" must never look the same:
//!
//! * **unreadable** -- a directory that would not list, or an entry whose metadata failed
//!   (a permission error, or a path that vanished mid-walk). Callers decide what to do with
//!   the hole, but they must decide *knowing it exists*: the engine forces at least a
//!   `Partial` outcome, and a mirror refuses to build a deletion plan at all, because a
//!   tree that could not be fully read looks exactly like one whose files were deleted.
//! * **reparse_skipped** -- junctions and symlinks, which are never followed (decision D7:
//!   a file symlink skipped is indistinguishable from a junction skip at this level, and
//!   following either can escape the source tree or recurse forever). One shared counter,
//!   always surfaced in run stats so the skip is visible rather than silent. The walk ROOT
//!   is held to the same rule: a reparse point offered as the root is refused with an
//!   error, not followed into its target -- `is_dir()` would follow it silently.
//!
//! A third channel is quieter because it can only ever contain our own artifacts:
//! **temp_files** -- in-progress copy temps (`.bstmp-*`, and the legacy `.backstar-tmp-*`
//! from before A1c) left behind by a killed run. They are never content, so they are never
//! enumerated into a snapshot, a mirror plan, or a restore (F39) -- but their paths are
//! collected so a mirror run can sweep the destination-side ones (snapshot-side orphans
//! are pruned wholesale along with the interrupted run's directory; F35, `engine.rs`).
//!
//! Finally, the walk records **empty directories** (decision D8): a directory that contains
//! no files at any depth -- after exclusions -- is part of the tree's shape, so snapshots,
//! restores and mirrors all preserve it. Only directories are ever recorded here; a
//! directory is computed as empty from the visited set and the file entries' ancestors, so
//! this costs no extra filesystem reads. (Our own `.backstar-trash-*` staging dirs (D3)
//! are likewise never descended into: they are engine artifacts, and the mirror purges
//! them itself.)
//!
//! Both channels carry an exact count plus a bounded sample of the offending paths -- the
//! first [`MAX_RECORDED_ANOMALIES`], with the rest folded into the overflow count. Exact
//! paths let callers *name names*; the bound keeps a pathological tree from flooding
//! memory.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::exclude::CompiledExcludes;
use crate::{Error, Result};

/// How many anomaly paths are kept verbatim per channel before the rest collapse into the
/// count alone. A few hundred paths is plenty for a UI to show and for an error message to
/// quote; beyond that the count is what matters.
pub const MAX_RECORDED_ANOMALIES: usize = 256;

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
    /// Reparse points (junctions and symlinks) not followed -- the exact total.
    pub reparse_skipped: u64,
    /// A sample of the skipped reparse-point paths, up to [`MAX_RECORDED_ANOMALIES`].
    pub reparse_paths: Vec<PathBuf>,
    /// Entries that could not be read at all -- a permission error, or a path that vanished
    /// mid-walk. The exact total. Counted rather than silently dropped, because "we could
    /// not look" and "there was nothing there" must not look the same.
    pub unreadable: u64,
    /// A sample of the unreadable paths, up to [`MAX_RECORDED_ANOMALIES`]. The walk root
    /// itself, if it could not be listed, is always the first entry recorded.
    pub unreadable_paths: Vec<PathBuf>,
    /// BackStar's own crash-orphaned copy temp files found along the way (absolute paths).
    /// Never enumerated as entries: they are engine artifacts, not content (F39). The
    /// mirror run deletes the destination-side ones; source-side ones are only reported
    /// here -- a backup tool never deletes from the source.
    pub temp_files: Vec<PathBuf>,
    /// Directories that contain no files at any depth after exclusions (relative paths,
    /// sorted). Decision D8: empty directories are part of the tree's shape -- snapshots
    /// create them, restores recreate them, and a mirror keeps destination ones that
    /// exist in the source. Computed from the visited set and the file entries, so
    /// collecting them costs no extra filesystem reads.
    pub empty_dirs: Vec<PathBuf>,
}

impl WalkResult {
    fn record_reparse(&mut self, path: &Path) {
        self.reparse_skipped += 1;
        if self.reparse_paths.len() < MAX_RECORDED_ANOMALIES {
            self.reparse_paths.push(path.to_path_buf());
        }
    }

    fn record_unreadable(&mut self, path: &Path) {
        self.unreadable += 1;
        if self.unreadable_paths.len() < MAX_RECORDED_ANOMALIES {
            self.unreadable_paths.push(path.to_path_buf());
        }
    }

    /// Reparse points counted but not individually recorded, because the sample was full.
    pub fn reparse_overflow(&self) -> u64 {
        self.reparse_skipped - self.reparse_paths.len() as u64
    }

    /// Unreadable entries counted but not individually recorded, because the sample was
    /// full.
    pub fn unreadable_overflow(&self) -> u64 {
        self.unreadable - self.unreadable_paths.len() as u64
    }
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
    // `symlink_metadata`, not `is_dir()`: `is_dir()` follows reparse points, and a
    // junction offered as the walk ROOT would then be silently expanded into its target
    // -- the one place the reparse-skip below cannot catch it, exactly as a junction at a
    // mirror destination subfolder would be walked (and deleted) through.
    let root_md =
        std::fs::symlink_metadata(root).map_err(|_| Error::Unresolvable(root.to_path_buf()))?;
    if root_md.file_type().is_symlink() {
        return Err(Error::ReparseRoot(root.to_path_buf()));
    }
    if !root_md.is_dir() {
        return Err(Error::Unresolvable(root.to_path_buf()));
    }

    let mut out = WalkResult::default();
    // Directories still to visit. An explicit stack rather than recursion: a deep tree
    // should not be able to blow the stack.
    let mut stack = vec![root.to_path_buf()];
    // Directories whose listing succeeded (relative paths; the root excluded). This is
    // the raw material for the empty-directory computation at the end (D8).
    let mut visited_dirs: Vec<PathBuf> = Vec::new();
    let mut since_report = 0u64;

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                // When this is the walk root itself, the whole source is unreadable --
                // recorded first, so a caller can tell "empty tree" apart from "tree that
                // was never read" by checking for the root in `unreadable_paths`.
                out.record_unreadable(&dir);
                continue;
            }
        };

        // The directory listed, so its contents are known -- it is a candidate for the
        // empty-directory set. An unreadable one is NOT recorded: its emptiness is
        // unknown, and the unreadable channel already reported it.
        if dir != root {
            if let Ok(rel) = dir.strip_prefix(root) {
                visited_dirs.push(rel.to_path_buf());
            }
        }

        for entry in rd {
            let Ok(entry) = entry else {
                out.record_unreadable(&dir);
                continue;
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // `symlink_metadata` does not follow the link, which is what lets a reparse
            // point be recognised rather than silently expanded.
            let Ok(md) = entry.metadata() else {
                out.record_unreadable(&path);
                continue;
            };

            if md.file_type().is_symlink() {
                // Matches robocopy's /XJ. A junction pointing at an ancestor would
                // otherwise recurse until the path length gives out.
                out.record_reparse(&path);
                continue;
            }

            if md.is_dir() {
                // Our own staged-trash dirs (mirror, D3) are engine artifacts, never
                // content: a stale one inside a tree that some other job backs up must
                // never be snapshotted. Not counted or recorded -- the mirror purges
                // them itself at the start of its next run.
                if name.starts_with(crate::mirror::TRASH_PREFIX) {
                    continue;
                }
                if excludes.is_dir_excluded(&path, &name) {
                    out.dirs_excluded += 1;
                    continue;
                }
                stack.push(path);
                continue;
            }

            // Our own in-progress or crash-orphaned copy temps are artifacts, not
            // content: never backed up, mirrored, deleted as user data, or restored.
            // Recorded by path so the mirror can sweep the destination-side ones (F39).
            if crate::copy::is_temp_file_name(&name) {
                out.temp_files.push(path);
                continue;
            }

            if excludes.is_file_excluded(&name) {
                out.files_excluded += 1;
                continue;
            }

            let rel = match path.strip_prefix(root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => {
                    out.record_unreadable(&path);
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

    // D8: a visited directory is empty when no walked file has it as an ancestor.
    // Ancestor-marking walks up each file's rel path once -- no extra filesystem reads.
    let mut dirs_with_files: std::collections::HashSet<&Path> = std::collections::HashSet::new();
    for entry in &out.entries {
        let mut p: &Path = entry.rel.as_path();
        while let Some(parent) = p.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            dirs_with_files.insert(parent);
            p = parent;
        }
    }
    out.empty_dirs = visited_dirs
        .into_iter()
        .filter(|d| !dirs_with_files.contains(d.as_path()))
        .collect();
    out.empty_dirs.sort();

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

    /// F12: a junction offered as the walk ROOT must be refused, not followed -- `is_dir()`
    /// would silently expand it into its target, and the reparse-skip inside the loop can
    /// never catch the root itself.
    #[cfg(windows)]
    #[test]
    fn a_junction_as_the_walk_root_is_refused_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        touch(&target.join("precious.txt"), "precious");
        let link = tmp.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create a junction here");
            return;
        }

        let ex = ExcludeRules::none().compile(&link, Path::new(r"D:\d"));
        let err = walk(&link, &ex, |_, _| {}).unwrap_err();
        assert!(
            matches!(err, Error::ReparseRoot(_)),
            "a reparse-point walk root must be refused, got: {err}"
        );
        assert!(
            target.join("precious.txt").is_file(),
            "and the target must never have been entered"
        );
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
        assert_eq!(
            r.reparse_paths,
            vec![link.clone()],
            "and named, not just counted -- a skipped reparse point is something in the \
             tree that was not backed up, which must stay visible"
        );
    }

    /// The deterministic stand-in for a permission error: a directory whose name ends in a
    /// dot can be created through the `\\?\` prefix but cannot be stat'ed through a normal
    /// path (Win32 path parsing strips the trailing dot, so the lookup misses). The walk
    /// must count it AND hand back its path.
    #[cfg(windows)]
    #[test]
    fn unreadable_entries_are_counted_with_their_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        touch(&root.join("real.txt"), "real");
        let weird = format!(r"\\?\{}\locked.", root.display());
        if std::fs::create_dir(&weird).is_err() {
            eprintln!("skipping: cannot create a trailing-dot directory here");
            return;
        }

        let ex = ExcludeRules::none().compile(&root, Path::new(r"D:\d"));
        let r = walk(&root, &ex, |_, _| {}).unwrap();

        assert_eq!(r.entries.len(), 1, "the readable file is still walked");
        assert_eq!(r.unreadable, 1, "the un-stat-able entry must be counted");
        assert_eq!(r.unreadable_paths.len(), 1);
        assert_eq!(
            r.unreadable_paths[0].file_name().unwrap().to_string_lossy(),
            "locked.",
            "the recorded path must name the unreadable entry: {:?}",
            r.unreadable_paths
        );

        let _ = std::fs::remove_dir(&weird);
    }

    /// Counts stay exact even when the recorded path sample hits its cap -- the sample is
    /// for naming names, the count is the truth.
    #[test]
    fn anomaly_paths_are_bounded_but_counts_stay_exact() {
        let mut r = WalkResult::default();
        for i in 0..(MAX_RECORDED_ANOMALIES + 10) {
            r.record_unreadable(Path::new(&format!("p{i}")));
            r.record_reparse(Path::new(&format!("j{i}")));
        }
        assert_eq!(r.unreadable, MAX_RECORDED_ANOMALIES as u64 + 10);
        assert_eq!(r.unreadable_paths.len(), MAX_RECORDED_ANOMALIES);
        assert_eq!(r.unreadable_overflow(), 10);
        assert_eq!(r.reparse_skipped, MAX_RECORDED_ANOMALIES as u64 + 10);
        assert_eq!(r.reparse_paths.len(), MAX_RECORDED_ANOMALIES);
        assert_eq!(r.reparse_overflow(), 10);
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

    /// D8/F37: directories holding no files at any depth are part of the tree's shape --
    /// recorded so snapshots, restores and mirrors can preserve them. Excluded
    /// directories are pruned before recording, so an excluded empty dir is NOT
    /// preserved (the exclusion is a deliberate "not part of the backup").
    #[test]
    fn empty_directories_are_recorded_and_excluded_ones_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        touch(&r.join("real.txt"), "x");
        touch(&r.join("populated/sub/file.txt"), "x");
        std::fs::create_dir_all(r.join("empty")).unwrap();
        std::fs::create_dir_all(r.join("chain/a/b")).unwrap(); // an all-empty chain
        std::fs::create_dir_all(r.join("node_modules")).unwrap(); // excluded below

        let none = ExcludeRules::none().compile(r, Path::new(r"D:\d"));
        let walked = walk(r, &none, |_, _| {}).unwrap();
        assert_eq!(
            walked.empty_dirs,
            vec![
                PathBuf::from("chain"),
                PathBuf::from(r"chain\a"),
                PathBuf::from(r"chain\a\b"),
                PathBuf::from("empty"),
                PathBuf::from("node_modules"),
            ],
            "every fileless directory, deepest chains included"
        );

        let project = ExcludeRules::project().compile(r, Path::new(r"D:\d"));
        let walked = walk(r, &project, |_, _| {}).unwrap();
        assert!(
            !walked.empty_dirs.iter().any(|d| d.as_os_str() == "node_modules"),
            "an excluded directory is never preserved: {:?}",
            walked.empty_dirs
        );
        assert!(walked.empty_dirs.iter().any(|d| d.as_os_str() == "empty"));
    }

    /// F39: our own temp files -- current and legacy prefix, at any depth -- are never
    /// enumerated into the walk's entries (so no snapshot, mirror plan, or restore can
    /// include them), but their paths are handed back so a run can sweep them.
    #[test]
    fn temp_files_are_never_enumerated_but_are_reported_for_sweeping() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("real.txt"), "real");
        touch(&root.join("sub/nested.txt"), "nested");
        let orphan_new = root.join("sub/.bstmp-1234-0-fragment");
        let orphan_old = root.join(".backstar-tmp-99-deadfile.txt");
        touch(&orphan_new, "junk");
        touch(&orphan_old, "junk");

        let ex = ExcludeRules::none().compile(root, Path::new(r"D:\d"));
        let r = walk(root, &ex, |_, _| {}).unwrap();

        let names: Vec<String> =
            r.entries.iter().map(|e| e.rel.to_string_lossy().to_string()).collect();
        assert_eq!(names.len(), 2, "only real content is walked: {names:?}");
        assert!(!names.iter().any(|n| n.contains("bstmp") || n.contains("backstar-tmp")));
        assert_eq!(
            r.temp_files.len(),
            2,
            "both orphans are reported for sweeping: {:?}",
            r.temp_files
        );
        assert!(r.temp_files.contains(&orphan_new));
        assert!(r.temp_files.contains(&orphan_old));
    }
}
