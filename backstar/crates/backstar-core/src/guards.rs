//! Path canonicalisation and containment guards.
//!
//! These replace `Test-DestNotInsideSource` / `Test-JobDestNotSystemBackup`
//! (`app/BackStar.UI.ProjectTab.ps1:165-191`), which had two defects the port must not inherit:
//!
//! 1. **8.3 short names bypassed every guard.** PowerShell's `Resolve-Path` does not expand
//!    them, so `guard(dest = C:\Users\ST3ADY~1\x\inner, src = C:\Users\st3adypick\x)` compared
//!    two unrelated-looking strings and returned "safe" -- permitting a mirror of a folder into
//!    its own subtree. Not hypothetical: the app wrote `C:\Users\ST3ADY~1\...` into its own
//!    `BackStar.history.json` and offers those paths back to Restore.
//! 2. **They failed OPEN.** `Test-JobDestNotSystemBackup` returned `$true` (= allow the
//!    destructive operation) whenever a path could not be resolved.
//!
//! Both are inverted here: canonicalisation expands short names, and an unresolvable path is
//! reported as overlapping, so the caller refuses.

use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Expand `%VAR%` references using the process environment.
///
/// An unset or malformed reference is left verbatim, matching
/// `ExpandEnvironmentStringsW` rather than erroring.
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            // `%%` -- emit one literal percent and continue.
            Some(0) => {
                out.push('%');
                rest = &after[1..];
            }
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            // Unterminated percent: literal.
            None => {
                out.push('%');
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Expand an 8.3 short path to its long form. Returns the input unchanged when the path
/// does not exist or the call fails -- callers still get a lexically normalised path, and
/// [`canonical_path`] layers a real filesystem resolve on top.
#[cfg(windows)]
pub fn expand_short_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetLongPathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // 32_768 is the documented ceiling for an extended-length path.
    let mut buf = vec![0u16; 32_768];
    let len = unsafe { GetLongPathNameW(PCWSTR(wide.as_ptr()), Some(buf.as_mut_slice())) };
    if len == 0 || len as usize >= buf.len() {
        return path.to_path_buf();
    }
    buf.truncate(len as usize);
    PathBuf::from(std::ffi::OsString::from_wide(&buf))
}

#[cfg(not(windows))]
pub fn expand_short_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Lexically normalise: resolve `.` and `..` without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop past the prefix/root.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Strip a trailing separator, but never from a bare root.
///
/// A trailing backslash is not cosmetic on Windows: passed to a quoted command line it
/// escapes the closing quote and fuses every subsequent argument into one. The PowerShell
/// app defended against this with a `TrimEnd` repeated at thirteen call sites, and two of
/// those sites were already missing it.
pub fn trim_trailing_sep(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    let trimmed = s.trim_end_matches(['\\', '/']);
    // A bare `D:` means "the current directory on D:", which is NOT the same as `D:\`.
    // Re-attach the separator rather than hand back the drive-relative form.
    if trimmed.is_empty() || (trimmed.len() == 2 && trimmed.ends_with(':')) {
        return PathBuf::from(format!("{trimmed}\\"));
    }
    PathBuf::from(trimmed)
}

/// Fully canonicalise a path for comparison: expand env vars, absolutise, resolve `..`,
/// expand 8.3 short names, then resolve links if the path exists.
///
/// Errors only when the path is empty or cannot be absolutised. A path that simply does not
/// exist yet still canonicalises -- backup destinations are routinely created after this check.
pub fn canonical_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let raw = path.as_ref();
    if raw.as_os_str().is_empty() {
        return Err(Error::Unresolvable(raw.to_path_buf()));
    }

    let expanded = PathBuf::from(expand_env(&raw.to_string_lossy()));

    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|_| Error::Unresolvable(raw.to_path_buf()))?
            .join(expanded)
    };

    let normalized = lexical_normalize(&absolute);
    let long = expand_short_path(&normalized);

    // `dunce::canonicalize` resolves links and gives back a plain path rather than an
    // extended-length one, so comparisons against user-supplied paths line up.
    let resolved = dunce::canonicalize(&long).unwrap_or(long);

    Ok(trim_trailing_sep(&resolved))
}

/// True when `outer` is `inner`, or contains it.
///
/// Compares whole path components, so `D:\Backup` does not "contain" `D:\Backup2`.
fn contains_or_equals(outer: &Path, inner: &Path) -> bool {
    let o: Vec<_> = outer.components().collect();
    let i: Vec<_> = inner.components().collect();
    if i.len() < o.len() {
        return false;
    }
    o.iter().zip(i.iter()).all(|(a, b)| {
        a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    })
}

/// True when the two paths overlap in either direction -- same path, `a` inside `b`,
/// or `b` inside `a`.
///
/// **Fails closed.** If either path cannot be canonicalised, this returns `true`
/// (= treat as overlapping = refuse), because no containment guarantee can be made.
/// The PowerShell original did the opposite and allowed the operation.
pub fn paths_overlap(a: impl AsRef<Path>, b: impl AsRef<Path>) -> bool {
    let (Ok(ca), Ok(cb)) = (canonical_path(a), canonical_path(b)) else {
        return true;
    };
    contains_or_equals(&ca, &cb) || contains_or_equals(&cb, &ca)
}

/// Guard a copy job: the destination must not be, sit inside, or contain the source.
pub fn ensure_dest_safe(dest: impl AsRef<Path>, src: impl AsRef<Path>) -> Result<()> {
    if paths_overlap(dest.as_ref(), src.as_ref()) {
        return Err(Error::Overlap {
            src: src.as_ref().to_path_buf(),
            dest: dest.as_ref().to_path_buf(),
        });
    }
    Ok(())
}

/// Prefix a path for Win32 calls that would otherwise hit the 260-character limit.
///
/// robocopy handles long paths natively, which is precisely why it creates files the
/// PowerShell app could then no longer see: `Get-Item` threw `PathTooLongException`, a bare
/// empty catch swallowed it, and verification reported "clean".
pub fn extended_path(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    if s.len() < 248 {
        return path.to_path_buf();
    }
    PathBuf::from(format!(r"\\?\{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion_handles_unset_and_literals() {
        std::env::set_var("BACKSTAR_TEST_VAR", "expanded");
        assert_eq!(expand_env("a%BACKSTAR_TEST_VAR%b"), "aexpandedb");
        // An unset variable is left verbatim rather than becoming an empty string --
        // silently collapsing would turn `%NOPE%\Backups` into `\Backups`.
        assert_eq!(expand_env(r"%BACKSTAR_NOT_SET%\x"), r"%BACKSTAR_NOT_SET%\x");
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("no vars here"), "no vars here");
    }

    #[test]
    fn trailing_separator_is_trimmed_but_roots_keep_theirs() {
        assert_eq!(trim_trailing_sep(Path::new(r"D:\proj\")), PathBuf::from(r"D:\proj"));
        assert_eq!(trim_trailing_sep(Path::new(r"D:\proj")), PathBuf::from(r"D:\proj"));
        // `D:` alone means "current directory on D:" -- a silently-wrong-folder backup.
        assert_eq!(trim_trailing_sep(Path::new(r"D:\")), PathBuf::from(r"D:\"));
    }

    #[test]
    fn dotdot_is_resolved_without_escaping_the_root() {
        assert_eq!(lexical_normalize(Path::new(r"D:\a\b\..\c")), PathBuf::from(r"D:\a\c"));
        assert_eq!(lexical_normalize(Path::new(r"D:\a\.\b")), PathBuf::from(r"D:\a\b"));
        assert_eq!(lexical_normalize(Path::new(r"D:\..\..")), PathBuf::from(r"D:\"));
    }

    #[test]
    fn containment_compares_components_not_string_prefixes() {
        // The bug this prevents: `D:\Backup` string-prefixes `D:\Backup2`.
        assert!(!contains_or_equals(Path::new(r"D:\Backup"), Path::new(r"D:\Backup2\x")));
        assert!(contains_or_equals(Path::new(r"D:\Backup"), Path::new(r"D:\Backup\x")));
        assert!(contains_or_equals(Path::new(r"D:\Backup"), Path::new(r"D:\Backup")));
        // Case-insensitive, as NTFS is in practice.
        assert!(contains_or_equals(Path::new(r"d:\backup"), Path::new(r"D:\BACKUP\x")));
    }

    #[test]
    fn overlap_is_symmetric() {
        assert!(paths_overlap(r"D:\a", r"D:\a\b"));
        assert!(paths_overlap(r"D:\a\b", r"D:\a"));
        assert!(paths_overlap(r"D:\a", r"D:\a"));
        assert!(!paths_overlap(r"D:\a", r"D:\b"));
    }

    #[test]
    fn overlap_fails_closed_on_an_unresolvable_path() {
        // An empty path cannot be canonicalised, so it must read as unsafe.
        assert!(paths_overlap("", r"D:\a"));
        assert!(paths_overlap(r"D:\a", ""));
        assert!(ensure_dest_safe("", r"D:\a").is_err());
    }

    /// Regression test for the shipped 8.3-short-name guard bypass, exercised against the
    /// real filesystem rather than lexically.
    ///
    /// `Resolve-Path` in the PowerShell original did not expand short names, so a
    /// destination expressed in 8.3 form compared unequal to its own parent and the
    /// containment guard returned "safe" -- permitting a mirror of a folder into its own
    /// subtree. The app demonstrably produced such paths: `BackStar.history.json` contains
    /// `C:\Users\ST3ADY~1\...`, written by the app and offered back to Restore.
    #[cfg(windows)]
    #[test]
    fn short_names_cannot_bypass_the_containment_guard() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetShortPathNameW;

        fn short_path(p: &Path) -> Option<PathBuf> {
            let wide: Vec<u16> =
                p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            let mut buf = vec![0u16; 32_768];
            let len =
                unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(buf.as_mut_slice())) };
            if len == 0 || len as usize >= buf.len() {
                return None;
            }
            buf.truncate(len as usize);
            Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
        }

        let tmp = tempfile::tempdir().expect("temp dir");
        // A name long enough (and spaced enough) to force a distinct 8.3 alias.
        let parent = tmp.path().join("A Very Long Project Folder Name");
        let child = parent.join("inner");
        std::fs::create_dir_all(&child).expect("create dirs");

        let Some(short_parent) = short_path(&parent) else {
            // 8.3 generation can be disabled per-volume; nothing to prove if so.
            eprintln!("skipping: no 8.3 alias available on this volume");
            return;
        };
        if short_parent == parent {
            eprintln!("skipping: volume returned the long name unchanged");
            return;
        }

        // The whole point: the two spellings must canonicalise to the same path.
        assert_eq!(
            canonical_path(&short_parent).unwrap(),
            canonical_path(&parent).unwrap(),
            "the 8.3 alias and the long name must canonicalise identically"
        );

        // And the guard must therefore refuse a destination inside its own source, even
        // when the two are spelled differently.
        let short_child = short_parent.join("inner");
        assert!(
            paths_overlap(&short_child, &parent),
            "a destination expressed in 8.3 form must still be caught as inside its source"
        );
        assert!(ensure_dest_safe(&short_child, &parent).is_err());
    }

    #[test]
    fn extended_prefix_applied_only_where_needed() {
        assert_eq!(extended_path(Path::new(r"D:\short")), PathBuf::from(r"D:\short"));
        let long = format!(r"D:\{}", "x".repeat(300));
        assert!(extended_path(Path::new(&long)).to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(
            extended_path(Path::new(r"\\server\share\f")),
            PathBuf::from(r"\\?\UNC\server\share\f")
        );
        // Already-prefixed paths are left alone rather than double-prefixed.
        assert_eq!(extended_path(Path::new(r"\\?\D:\x")), PathBuf::from(r"\\?\D:\x"));
    }
}
