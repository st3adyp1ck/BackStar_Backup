//! Exclusion rules.
//!
//! Ported from `app/BackStar.psm1:101-115` and `Get-RobocopyArgs`
//! (`app/BackStar.Engine.ps1:277-328`). Three independent kinds:
//!
//! * **dir names** -- matched by name at any depth (`node_modules`, `Code Cache`).
//! * **dir paths** -- source-relative paths, because the leaf name alone is too common to
//!   ban globally. Capacitor regenerates `apps/mobile/ios/App/App/public` on every
//!   `npx cap sync`, and it is named `public` like a real asset folder.
//! * **file patterns** -- globs against the file name (`Thumbs.db`, `*.tmp`), matched
//!   case-insensitively: robocopy's `/XF` is case-insensitive, NTFS is case-insensitive,
//!   and a user who excludes `*.tmp` means `FILE.TMP` too.
//!
//! Two rules here exist because of shipped bugs. Both have regression tests below.
//!
//! **Multi-word names.** robocopy splits `/XD` on whitespace, so an unquoted `Code Cache`
//! arrived as two directory names, `Code` and `Cache`. The exclusion broke in *both*
//! directions at once: the cache folders it named were copied anyway, while every unrelated
//! folder actually called Code, Service, Worker, System, Volume or Information was silently
//! skipped -- including `%APPDATA%\Code`, VS Code's entire user profile, which sits inside
//! the shipped "App Settings" preset. robocopy still exited 0, so the run reported success
//! and history recorded OK. The gap only surfaced at restore time.
//!
//! **Both-ends rooting.** A dir path must be excluded relative to the source *and* the
//! destination. Rooting only at the source protects the folder while it is the source, but a
//! reverse sync makes the backup the source and the live project folder the destination --
//! and a mirror then deletes the project's own copy of exactly the folders this list exists
//! to leave alone, because they were deliberately never copied into the backup.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// Directory names excluded from project backups, at any depth.
pub const PROJECT_EXCLUDE_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    ".next",
    "__pycache__",
    ".venv",
    "venv",
    "target",
    ".gradle",
    ".dart_tool",
    ".expo",
];

/// Source-relative directory paths excluded from project backups.
pub const PROJECT_EXCLUDE_PATHS: &[&str] = &[
    r"apps\mobile\ios\App\App\public",
    r"apps\mobile\android\app\src\main\assets\public",
];

/// Directory names excluded from system backups. The browser-cache entries are what make
/// backing up a Chrome or Edge profile tractable at all.
pub const SYSTEM_EXCLUDE_DIRS: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "cache2",
    "Service Worker",
    "CacheStorage",
    "blob_storage",
    "Crashpad",
    "IndexedDB",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// File-name globs excluded from system backups.
pub const SYSTEM_EXCLUDE_FILES: &[&str] = &["Thumbs.db", "desktop.ini", "*.tmp"];

/// A resolved set of exclusion rules, ready to match against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludeRules {
    /// Directory names, matched at any depth, case-insensitively.
    pub dir_names: Vec<String>,
    /// Source-relative directory paths.
    pub dir_paths: Vec<String>,
    /// File-name globs, matched case-insensitively against the file name.
    pub file_patterns: Vec<String>,
}

impl ExcludeRules {
    pub fn project() -> Self {
        Self {
            dir_names: PROJECT_EXCLUDE_DIRS.iter().map(|s| s.to_string()).collect(),
            dir_paths: PROJECT_EXCLUDE_PATHS.iter().map(|s| s.to_string()).collect(),
            file_patterns: Vec::new(),
        }
    }

    pub fn system() -> Self {
        Self {
            dir_names: SYSTEM_EXCLUDE_DIRS.iter().map(|s| s.to_string()).collect(),
            dir_paths: Vec::new(),
            file_patterns: SYSTEM_EXCLUDE_FILES.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// No exclusions at all. Restores use this: a restore copies back exactly what was
    /// backed up, and re-applying backup-time filters would silently drop files.
    pub fn none() -> Self {
        Self::default()
    }

    /// Compile into a fast matcher bound to one source/destination pair.
    pub fn compile(&self, source: &Path, dest: &Path) -> CompiledExcludes {
        let mut files = GlobSetBuilder::new();
        for p in &self.file_patterns {
            // A plain name like `Thumbs.db` is a valid glob matching itself, so both
            // literal names and wildcards go through the same path. Case-insensitive
            // like robocopy's /XF: excluding `*.tmp` must also catch `FILE.TMP`.
            if let Ok(g) = GlobBuilder::new(p).case_insensitive(true).build() {
                files.add(g);
            }
        }

        // Root each relative dir path at BOTH ends. See the module docs.
        let mut abs_dirs = Vec::with_capacity(self.dir_paths.len() * 2);
        for rel in &self.dir_paths {
            abs_dirs.push(normalize_for_compare(&source.join(rel)));
            abs_dirs.push(normalize_for_compare(&dest.join(rel)));
        }

        CompiledExcludes {
            dir_names: self.dir_names.iter().map(|n| n.to_lowercase()).collect(),
            abs_dir_paths: abs_dirs,
            file_globs: files.build().unwrap_or_else(|_| GlobSet::empty()),
        }
    }
}

/// Normalise a path for comparison: lowercase, backslash-separated, no trailing separator.
///
/// Crate-visible because the mirror's delete-set construction (F4) must compare relative
/// paths with the exact same case rules NTFS applies -- a case-only rename (`README.txt`
/// vs `readme.txt`) must compare equal, or a delete gets planned for a file whose recopy
/// was never queued.
pub(crate) fn normalize_for_compare(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase().replace('/', "\\")
}

/// Compiled exclusion matcher for one source/destination pair.
#[derive(Debug, Clone)]
pub struct CompiledExcludes {
    dir_names: Vec<String>,
    abs_dir_paths: Vec<String>,
    file_globs: GlobSet,
}

impl CompiledExcludes {
    /// Should this directory be skipped?
    ///
    /// `full_path` is the absolute path of the directory; `name` is its final component.
    /// Name matching is exact and case-insensitive -- never a substring or prefix test.
    /// A folder genuinely called `Code` must survive an exclusion list containing
    /// `Code Cache`, which is precisely what the original bug got wrong.
    pub fn is_dir_excluded(&self, full_path: &Path, name: &str) -> bool {
        let lname = name.to_lowercase();
        if self.dir_names.contains(&lname) {
            return true;
        }
        let lpath = normalize_for_compare(full_path);
        self.abs_dir_paths.contains(&lpath)
    }

    /// Should this file be skipped? Matched against the file name only, not the full path,
    /// and case-insensitively -- see [`ExcludeRules::file_patterns`].
    pub fn is_file_excluded(&self, name: &str) -> bool {
        if self.file_globs.is_empty() {
            return false;
        }
        self.file_globs.is_match(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sys() -> CompiledExcludes {
        ExcludeRules::system().compile(Path::new(r"C:\src"), Path::new(r"D:\dest"))
    }

    /// Regression test for the shipped `/XD` whitespace-splitting bug.
    ///
    /// Every one of these folder names is a word from a multi-word exclusion entry. All of
    /// them must be KEPT. When the bug was live, all of them were silently skipped.
    #[test]
    fn single_words_from_multi_word_excludes_are_not_themselves_excluded() {
        let e = sys();
        for name in ["Code", "Service", "Worker", "System", "Volume", "Information"] {
            assert!(
                !e.is_dir_excluded(&PathBuf::from(format!(r"C:\src\{name}")), name),
                "folder {name:?} must be kept -- it is only a WORD inside a multi-word \
                 exclusion entry, not an entry itself"
            );
        }
    }

    /// The other half of the same bug: the multi-word entries must actually match.
    #[test]
    fn multi_word_excludes_match_as_whole_names() {
        let e = sys();
        for name in ["Code Cache", "Service Worker", "System Volume Information"] {
            assert!(
                e.is_dir_excluded(&PathBuf::from(format!(r"C:\src\{name}")), name),
                "folder {name:?} must be excluded"
            );
        }
    }

    #[test]
    fn dir_name_matching_is_case_insensitive_and_exact() {
        let e = sys();
        assert!(e.is_dir_excluded(Path::new(r"C:\src\CACHE"), "CACHE"));
        assert!(e.is_dir_excluded(Path::new(r"C:\src\cache"), "cache"));
        // Exact, not prefix: `Cache` must not swallow `CacheThings`.
        assert!(!e.is_dir_excluded(Path::new(r"C:\src\CacheThings"), "CacheThings"));
        // ...nor suffix.
        assert!(!e.is_dir_excluded(Path::new(r"C:\src\MyCache"), "MyCache"));
    }

    /// Regression test for the reverse-sync deletion bug: a relative dir path must be
    /// excluded when rooted at the destination too, not just the source.
    #[test]
    fn relative_dir_paths_are_rooted_at_both_source_and_dest() {
        let rules = ExcludeRules::project();
        let src = Path::new(r"C:\proj");
        let dest = Path::new(r"D:\backup\proj");
        let e = rules.compile(src, dest);

        let under_src = src.join(r"apps\mobile\ios\App\App\public");
        let under_dest = dest.join(r"apps\mobile\ios\App\App\public");

        assert!(e.is_dir_excluded(&under_src, "public"), "source-rooted path must be excluded");
        assert!(
            e.is_dir_excluded(&under_dest, "public"),
            "dest-rooted path must be excluded too -- a reverse sync makes the backup the \
             source, and a mirror would otherwise delete the project's own generated folder"
        );

        // An unrelated folder that merely happens to be called `public` must survive,
        // which is the entire reason these are paths rather than names.
        assert!(!e.is_dir_excluded(Path::new(r"C:\proj\src\public"), "public"));
    }

    #[test]
    fn file_globs_match_names_only() {
        let e = sys();
        assert!(e.is_file_excluded("Thumbs.db"));
        assert!(e.is_file_excluded("desktop.ini"));
        assert!(e.is_file_excluded("anything.tmp"));
        assert!(!e.is_file_excluded("notes.txt"));
        // `*.tmp` must not match a file that merely contains the text.
        assert!(!e.is_file_excluded("tmp.txt"));
    }

    /// Regression test for F32: robocopy's `/XF` is case-insensitive and so is NTFS, so a
    /// glob written `Thumbs.db` must catch `THUMBS.DB` -- a case-sensitive glob silently
    /// let every uppercase variant through.
    #[test]
    fn file_globs_are_case_insensitive() {
        let e = sys();
        assert!(e.is_file_excluded("THUMBS.DB"));
        assert!(e.is_file_excluded("DESKTOP.INI"));
        assert!(e.is_file_excluded("FILE.TMP"));
        assert!(e.is_file_excluded("Anything.TmP"));
        assert!(!e.is_file_excluded("keep.txt"));

        // Custom patterns go through the same path.
        let rules = ExcludeRules {
            dir_names: vec![],
            dir_paths: vec![],
            file_patterns: vec!["*.bak".into(), "secret.key".into()],
        };
        let e = rules.compile(Path::new(r"C:\a"), Path::new(r"D:\b"));
        assert!(e.is_file_excluded("BACKUP.BAK"));
        assert!(e.is_file_excluded("SECRET.KEY"));
        assert!(!e.is_file_excluded("public.key.txt"));
    }

    #[test]
    fn restore_rules_exclude_nothing() {
        let e = ExcludeRules::none().compile(Path::new(r"C:\a"), Path::new(r"D:\b"));
        assert!(!e.is_dir_excluded(Path::new(r"C:\a\node_modules"), "node_modules"));
        assert!(!e.is_file_excluded("Thumbs.db"));
    }

    #[test]
    fn project_rules_do_not_carry_system_file_patterns() {
        // Project backups historically applied no file patterns at all. Preserving that
        // matters: `desktop.ini` in a project folder can be meaningful content.
        let rules = ExcludeRules::project();
        assert!(rules.file_patterns.is_empty());
        let e = rules.compile(Path::new(r"C:\a"), Path::new(r"D:\b"));
        assert!(!e.is_file_excluded("desktop.ini"));
    }
}
