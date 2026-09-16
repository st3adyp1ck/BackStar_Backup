//! Native file copying with real byte-level progress.
//!
//! `CopyFileExW` is the API robocopy itself sits on, so this inherits its handling of the
//! NTFS details a naive read/write loop loses: alternate data streams, sparse files,
//! compressed and encrypted attributes, and file times. What it adds is a progress callback
//! that reports actual bytes transferred -- which is the whole reason the PowerShell app had
//! to scrape percentages out of half-written log lines. Nothing here is parsed.
//!
//! **Every write goes to a temporary name and is then renamed into place.** This is not
//! tidiness; it is the safety property that makes hardlinked snapshots possible at all.
//! Snapshots store an unchanged file as an additional name for the same data, so writing
//! *through* one of those names rewrites the bytes every snapshot sees. robocopy overwrites
//! in place and therefore cannot be used to populate a snapshot that hardlinks to its
//! predecessor without corrupting history. Renaming replaces the directory entry instead,
//! leaving the old inode intact and still referenced by the older snapshot.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::guards::extended_path;
use crate::{Error, Result};

/// Outcome of copying one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied { bytes: u64 },
    Cancelled,
}

#[cfg(windows)]
mod win {
    pub const PROGRESS_CONTINUE: u32 = 0;
    pub const PROGRESS_CANCEL: u32 = 1;
    /// Open the source with no read caching. A win for very large files, a loss for small
    /// ones, so it is applied by size rather than always.
    pub const COPY_FILE_NO_BUFFERING: u32 = 0x0000_1000;
    /// Copy a symlink/reparse point as a link rather than following it and duplicating the
    /// target. Without this, a junction pointing at a large tree is silently expanded --
    /// and a junction pointing at an ancestor recurses forever.
    pub const COPY_FILE_COPY_SYMLINK: u32 = 0x0000_0800;
    /// Files at or above this size are copied unbuffered.
    pub const NO_BUFFERING_THRESHOLD: u64 = 16 * 1024 * 1024;
}

#[cfg(windows)]
struct ProgressCtx<'a> {
    cb: &'a mut dyn FnMut(u64, u64),
    cancel: &'a AtomicBool,
}

/// The `LPPROGRESS_ROUTINE` thunk.
///
/// This runs on a thread owned by the kernel copy engine, so it must not unwind across the
/// FFI boundary. Any panic in the user callback is caught and turned into a cancel.
#[cfg(windows)]
unsafe extern "system" fn progress_thunk(
    total_file_size: i64,
    total_bytes_transferred: i64,
    _stream_size: i64,
    _stream_bytes_transferred: i64,
    _stream_number: u32,
    _reason: windows::Win32::Storage::FileSystem::LPPROGRESS_ROUTINE_CALLBACK_REASON,
    _hsource: windows::Win32::Foundation::HANDLE,
    _hdest: windows::Win32::Foundation::HANDLE,
    data: *const c_void,
) -> u32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = &mut *(data as *mut ProgressCtx);
        if ctx.cancel.load(Ordering::Relaxed) {
            return win::PROGRESS_CANCEL;
        }
        (ctx.cb)(total_bytes_transferred.max(0) as u64, total_file_size.max(0) as u64);
        win::PROGRESS_CONTINUE
    }));
    result.unwrap_or(win::PROGRESS_CANCEL)
}

/// A temporary sibling name for an in-progress copy.
///
/// Placed next to the destination so the final rename stays on one volume, and prefixed so
/// an interrupted run leaves something recognisably ours to clean up.
fn temp_sibling(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // The process id keeps two concurrent runs from colliding on the same temp name.
    let unique = std::process::id();
    dest.with_file_name(format!(".backstar-tmp-{unique}-{name}"))
}

/// Copy `src` to `dest`, reporting `(bytes_done, bytes_total)` as it goes.
///
/// The destination is written under a temporary name and renamed into place, so `dest` is
/// either the old file or the new one and never a partial mixture -- and an existing
/// hardlink at `dest` is replaced rather than written through.
#[cfg(windows)]
pub fn copy_file(
    src: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<CopyOutcome> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::CopyFileExW;

    if cancel.load(Ordering::Relaxed) {
        return Ok(CopyOutcome::Cancelled);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let size = std::fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?.len();
    let tmp = temp_sibling(dest);

    let src_w: Vec<u16> = extended_path(src)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let tmp_w: Vec<u16> = extended_path(&tmp)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut flags = win::COPY_FILE_COPY_SYMLINK;
    if size >= win::NO_BUFFERING_THRESHOLD {
        flags |= win::COPY_FILE_NO_BUFFERING;
    }

    let mut ctx = ProgressCtx { cb: &mut on_progress, cancel };

    let result = unsafe {
        CopyFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(tmp_w.as_ptr()),
            Some(progress_thunk),
            Some(&mut ctx as *mut _ as *const c_void),
            None,
            flags,
        )
    };

    if let Err(e) = result {
        // A cancel surfaces here as ERROR_REQUEST_ABORTED; the kernel already removed the
        // partial temp file, but sweep anyway in case it did not.
        let _ = std::fs::remove_file(&tmp);
        if cancel.load(Ordering::Relaxed) {
            return Ok(CopyOutcome::Cancelled);
        }
        return Err(Error::other(format!(
            "copy {} -> {}: {e}",
            src.display(),
            dest.display()
        )));
    }

    // Replace the directory entry. A read-only destination blocks the rename, so clear the
    // attribute first -- backup destinations routinely inherit read-only from the source.
    if dest.exists() {
        if let Ok(md) = std::fs::metadata(dest) {
            let mut perms = md.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(dest, perms);
            }
        }
    }

    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(dest, e)
    })?;

    Ok(CopyOutcome::Copied { bytes: size })
}

#[cfg(not(windows))]
pub fn copy_file(
    src: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<CopyOutcome> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(CopyOutcome::Cancelled);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = temp_sibling(dest);
    let bytes = std::fs::copy(src, &tmp).map_err(|e| Error::io(src, e))?;
    on_progress(bytes, bytes);
    std::fs::rename(&tmp, dest).map_err(|e| Error::io(dest, e))?;
    Ok(CopyOutcome::Copied { bytes })
}

/// Create `link` as an additional name for the data at `existing`.
///
/// This is how an unchanged file joins a new snapshot without costing any disk.
#[cfg(windows)]
pub fn hard_link(existing: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::hard_link(existing, link).map_err(|e| Error::io(link, e))
}

#[cfg(not(windows))]
pub fn hard_link(existing: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::hard_link(existing, link).map_err(|e| Error::io(link, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn copies_content_and_reports_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dest = tmp.path().join("sub/b.txt");
        write(&src, "hello world");

        let cancel = AtomicBool::new(false);
        let mut last = (0u64, 0u64);
        let out = copy_file(&src, &dest, &cancel, |done, total| last = (done, total)).unwrap();

        assert_eq!(out, CopyOutcome::Copied { bytes: 11 });
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello world");
        assert_eq!(last, (11, 11), "final progress should report the whole file");
    }

    #[test]
    fn creates_missing_destination_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dest = tmp.path().join("x/y/z/a.txt");
        write(&src, "x");
        copy_file(&src, &dest, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert!(dest.is_file());
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dest = tmp.path().join("a.txt.copy");
        write(&src, "content");
        copy_file(&src, &dest, &AtomicBool::new(false), |_, _| {}).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".backstar-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    /// **The load-bearing test for the whole snapshot design.**
    ///
    /// Snapshot N-1 and snapshot N share one inode for an unchanged file. When the file
    /// later changes and is copied into snapshot N+1 at the same relative path, the older
    /// snapshots must not change. Writing in place -- which is what robocopy does, and what
    /// a naive `File::create` would do -- rewrites the bytes behind every name at once and
    /// silently rewrites history.
    #[test]
    fn copying_over_a_hardlink_does_not_rewrite_the_other_names() {
        let tmp = tempfile::tempdir().unwrap();

        let snap1 = tmp.path().join("snap1/notes.txt");
        write(&snap1, "VERSION ONE");

        // snap2 shares snap1's data, exactly as an unchanged file in a new snapshot would.
        let snap2 = tmp.path().join("snap2/notes.txt");
        hard_link(&snap1, &snap2).unwrap();
        assert_eq!(std::fs::read_to_string(&snap2).unwrap(), "VERSION ONE");

        // The user edits the file; the next run copies the new content to snap2's path.
        let source = tmp.path().join("live/notes.txt");
        write(&source, "VERSION TWO - MUCH LONGER CONTENT");
        copy_file(&source, &snap2, &AtomicBool::new(false), |_, _| {}).unwrap();

        assert_eq!(
            std::fs::read_to_string(&snap2).unwrap(),
            "VERSION TWO - MUCH LONGER CONTENT",
            "the new snapshot should hold the new content"
        );
        assert_eq!(
            std::fs::read_to_string(&snap1).unwrap(),
            "VERSION ONE",
            "the PREVIOUS snapshot must be untouched -- if this fails, every historical \
             snapshot is being rewritten in place and point-in-time recovery is a lie"
        );
    }

    #[test]
    fn overwrites_a_read_only_destination() {
        // Backup destinations inherit read-only from their sources, and a read-only file
        // blocks the rename. The original relied on robocopy handling this.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dest = tmp.path().join("b.txt");
        write(&src, "new");
        write(&dest, "old");

        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dest, perms).unwrap();

        copy_file(&src, &dest, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
    }

    #[test]
    fn a_cancel_requested_up_front_copies_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dest = tmp.path().join("b.txt");
        write(&src, "data");

        let cancel = AtomicBool::new(true);
        let out = copy_file(&src, &dest, &cancel, |_, _| {}).unwrap();
        assert_eq!(out, CopyOutcome::Cancelled);
        assert!(!dest.exists(), "a cancelled copy must not leave a destination file");
    }

    /// A cancel raised from inside the progress callback must abort the copy and leave no
    /// partial file at the destination.
    #[test]
    fn a_cancel_mid_copy_leaves_no_partial_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        // Large enough that the copy engine reports progress more than once.
        let data = vec![7u8; 24 * 1024 * 1024];
        std::fs::write(&src, &data).unwrap();
        let dest = tmp.path().join("out.bin");

        let cancel = AtomicBool::new(false);
        let out = copy_file(&src, &dest, &cancel, |done, _| {
            if done > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

        assert_eq!(out, CopyOutcome::Cancelled);
        assert!(!dest.exists(), "no partial file at the destination");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".backstar-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left: {leftovers:?}");
    }

    #[test]
    fn progress_is_monotonic_and_ends_at_the_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        let size = 20 * 1024 * 1024;
        std::fs::write(&src, vec![3u8; size]).unwrap();
        let dest = tmp.path().join("out.bin");

        let mut samples = Vec::new();
        copy_file(&src, &dest, &AtomicBool::new(false), |done, total| {
            samples.push((done, total));
        })
        .unwrap();

        assert!(!samples.is_empty(), "a 20 MB copy should report progress");
        for w in samples.windows(2) {
            assert!(w[1].0 >= w[0].0, "progress must not go backwards");
        }
        let (done, total) = *samples.last().unwrap();
        assert_eq!(total, size as u64);
        assert_eq!(done, size as u64);
    }

    #[test]
    fn a_zero_byte_file_copies_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("empty.txt");
        std::fs::File::create(&src).unwrap();
        let dest = tmp.path().join("empty.copy");
        let out = copy_file(&src, &dest, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(out, CopyOutcome::Copied { bytes: 0 });
        assert!(dest.is_file());
    }

    #[test]
    fn unicode_names_survive_the_round_trip() {
        // The original decoded robocopy's output with the ANSI codepage, so a file called
        // `café.txt` came back as `caf,.txt` and verification skipped it entirely.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("café — Ω 日本語.txt");
        write(&src, "unicode");
        let dest = tmp.path().join("out/café — Ω 日本語.txt");
        copy_file(&src, &dest, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert!(dest.is_file());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "unicode");
    }

    #[test]
    fn missing_source_is_an_error_not_a_silent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let out = copy_file(
            &tmp.path().join("nope.txt"),
            &tmp.path().join("dest.txt"),
            &AtomicBool::new(false),
            |_, _| {},
        );
        assert!(out.is_err());
    }
}
