//! The golden restore test: back up a tree of deliberately awkward files, restore it, and
//! byte-compare the result against the original.
//!
//! This is the Phase 2 gate from the modernization plan. A backup tool that silently
//! corrupts or drops files is worse than no backup tool, and every case exercised here
//! corresponds to something the original PowerShell app either got wrong once (its own
//! source comments document several) or that `robocopy` handled for free and a naive
//! reimplementation could easily lose:
//!
//! - Unicode file and folder names (the app's log parser decoded robocopy's ANSI-codepage
//!   stdout, so `café.txt` silently failed verification -- see `UPGRADE-PLAN.md` D1).
//! - A path near/over the 260-character MAX_PATH limit.
//! - Read-only and hidden files.
//! - A zero-byte file.
//! - A file with an NTFS alternate data stream.
//! - A file that changes between two runs, to prove restore hands back the RIGHT version.
//! - A moderately large file, to exercise more than one progress callback.
//!
//! Deliberately not covered here: true sparse files (would need raw `FSCTL_SET_SPARSE`
//! device IO to construct) and files under active `SeBackupPrivilege`-only ACLs. Both are
//! inherited from `CopyFileExW`'s own behaviour and are not independently verified by this
//! suite -- noted here rather than silently assumed to work.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use backstar_core::config::{Job, JobKind};
use backstar_core::engine;
use backstar_core::exclude::ExcludeRules;
use backstar_core::repo::Repo;
use backstar_core::restore;

fn job_for(sources: Vec<PathBuf>, dest: PathBuf) -> Job {
    Job {
        id: "golden".into(),
        name: "Golden".into(),
        sources,
        dest,
        dest_portable: None,
        kind: JobKind::Snapshot,
        excludes: ExcludeRules::none(),
        presets: vec![],
        git_gc: false,
        enabled: true,
        schedule: None,
    }
}

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Byte-for-byte comparison of every file under `a` against the matching path under `b`.
/// Panics with the specific mismatching path, not just "trees differ" -- this test exists
/// to pinpoint exactly which awkward case broke, not just to know that something did.
fn assert_trees_identical(a: &Path, b: &Path) {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }

    let mut a_files = Vec::new();
    walk(a, a, &mut a_files);
    a_files.sort();

    let mut b_files = Vec::new();
    walk(b, b, &mut b_files);
    b_files.sort();

    assert_eq!(
        a_files, b_files,
        "the set of restored files does not match the original tree"
    );

    for rel in &a_files {
        let original = std::fs::read(a.join(rel)).unwrap();
        let restored = std::fs::read(b.join(rel)).unwrap();
        assert_eq!(
            original, restored,
            "content mismatch at {rel:?} -- original is {} bytes, restored is {} bytes",
            original.len(),
            restored.len()
        );
    }
}

/// Build the nasty fixture tree and return its root.
fn build_fixture(root: &Path) {
    // Plain ASCII baseline.
    write_file(&root.join("readme.txt"), b"a plain file, for a sanity baseline");

    // Unicode names: an accented character, a Greek letter, and CJK, in both a folder name
    // and a file name -- this is exactly the class of name the ANSI-codepage decode in the
    // old app's log parser silently mishandled.
    write_file(
        &root.join("café — Ω 日本語/notes 笔记.txt"),
        "unicode content: héllo wörld 日本語のテキスト".as_bytes(),
    );

    // Awkward-but-legal ASCII: spaces, ampersand, parens, apostrophe.
    write_file(
        &root.join("Odd Name (v2) & co/it's here.txt"),
        b"awkward ascii path",
    );

    // Zero-byte file.
    write_file(&root.join("empty.txt"), b"");

    // A path close to / over the 260-character MAX_PATH limit, built from nested
    // directories so no single component is unreasonable.
    let mut deep = root.join("deep");
    for i in 0..8 {
        deep = deep.join(format!("level-{i:02}-of-a-deliberately-long-directory-name"));
    }
    write_file(&deep.join("buried.txt"), b"found me");
    assert!(
        deep.join("buried.txt").as_os_str().len() > 260,
        "fixture path should exceed MAX_PATH to actually exercise long-path handling"
    );

    // A moderately large file -- large enough to cross the copy engine's unbuffered-IO
    // threshold and to generate more than one progress callback, without making the test
    // slow.
    let big: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    write_file(&root.join("big.bin"), &big);

    // Read-only and hidden attributes.
    let ro_path = root.join("readonly.txt");
    write_file(&ro_path, b"do not modify");
    #[cfg(windows)]
    {
        let mut perms = std::fs::metadata(&ro_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro_path, perms).unwrap();
    }

    #[cfg(windows)]
    set_hidden(&{
        let p = root.join("hidden.txt");
        write_file(&p, b"you can't see me (from Explorer, anyway)");
        p
    });
}

#[cfg(windows)]
fn set_hidden(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN);
    }
}

/// Write to an NTFS alternate data stream on an existing file, and return whether the
/// filesystem actually supports it (some CI/container filesystems do not).
#[cfg(windows)]
fn try_write_ads(file: &Path, stream_name: &str, contents: &[u8]) -> bool {
    let ads_path = format!("{}:{stream_name}", file.display());
    std::fs::write(&ads_path, contents).is_ok()
}

#[cfg(windows)]
fn read_ads(file: &Path, stream_name: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(format!("{}:{stream_name}", file.display()))
}

#[test]
fn backup_then_restore_reproduces_the_fixture_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    build_fixture(&src);

    let backup = tmp.path().join("backup");
    let job = job_for(vec![src.clone()], backup.clone());
    let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {})
        .expect("backup should succeed over the awkward fixture");

    assert_eq!(manifest.files_failed, 0, "no file in the fixture should fail to back up");

    let repo = Repo::open(&backup).unwrap();
    let restored = tmp.path().join("restored");
    let stats = restore::restore(
        &repo,
        &manifest.id,
        Path::new("source"),
        &restored,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .expect("restore should succeed");

    assert_eq!(stats.files_failed, 0, "no file should fail to restore");
    assert_trees_identical(&src, &restored);
}

/// Point-in-time recovery, exercised through the public restore API rather than reaching
/// into engine internals: edit a file after the first snapshot, take a second snapshot,
/// then restore from the FIRST snapshot's id and confirm it hands back the ORIGINAL bytes
/// -- not whatever is on disk now, and not the second snapshot's version.
#[test]
fn restoring_an_older_snapshot_returns_that_version_not_the_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    write_file(&src.join("doc.txt"), b"version one");
    let backup = tmp.path().join("backup");

    let job = job_for(vec![src.clone()], backup.clone());
    let first = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

    write_file(&src.join("doc.txt"), b"version two -- much longer than the first");
    let _second = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

    let repo = Repo::open(&backup).unwrap();
    let restored = tmp.path().join("restored-old.txt");
    restore::restore(
        &repo,
        &first.id,
        Path::new("source/doc.txt"),
        &restored,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&restored).unwrap(),
        "version one",
        "restoring the FIRST snapshot must return the FIRST version, regardless of what \
         later snapshots (or the live source) now contain"
    );
}

#[cfg(windows)]
#[test]
fn an_alternate_data_stream_survives_backup_and_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    let file = src.join("host.txt");
    write_file(&file, b"visible content");

    if !try_write_ads(&file, "secret.txt", b"hidden stream content") {
        eprintln!("skipping: this filesystem does not support alternate data streams");
        return;
    }

    let backup = tmp.path().join("backup");
    let job = job_for(vec![src], backup.clone());
    let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

    // The stream must already be present in the snapshot itself -- CopyFileExW carries
    // secondary data streams by default between NTFS volumes.
    let snapshot_file = backup.join("snapshots").join(&manifest.id).join("source/host.txt");
    let in_snapshot = read_ads(&snapshot_file, "secret.txt")
        .expect("the alternate data stream should have been copied into the snapshot");
    assert_eq!(in_snapshot, b"hidden stream content");

    let repo = Repo::open(&backup).unwrap();
    let restored = tmp.path().join("restored/host.txt");
    restore::restore(
        &repo,
        &manifest.id,
        Path::new("source/host.txt"),
        &restored,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(std::fs::read(&restored).unwrap(), b"visible content");
    let after_restore = read_ads(&restored, "secret.txt")
        .expect("the alternate data stream should survive a restore too");
    assert_eq!(after_restore, b"hidden stream content");
}

#[cfg(windows)]
#[test]
fn readonly_and_hidden_attributes_do_not_block_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    build_fixture(&src);

    let backup = tmp.path().join("backup");
    let job = job_for(vec![src], backup.clone());
    let manifest = engine::run_job(&job, &AtomicBool::new(false), &mut |_| {}).unwrap();

    let repo = Repo::open(&backup).unwrap();
    let restored = tmp.path().join("restored");

    // Restore TWICE onto the same destination. The second restore must succeed even
    // though the read-only file it is about to overwrite already exists and is read-only
    // -- this is exactly the case `copy_file` clears the attribute for before renaming.
    for attempt in 1..=2 {
        let stats = restore::restore(
            &repo,
            &manifest.id,
            Path::new("source"),
            &restored,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_or_else(|e| panic!("restore attempt {attempt} failed: {e}"));
        assert_eq!(stats.files_failed, 0, "attempt {attempt} had failures");
    }

    assert_eq!(std::fs::read(restored.join("readonly.txt")).unwrap(), b"do not modify");
}
