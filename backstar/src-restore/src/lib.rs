//! `BackStar-Restore` -- the portable half of BackStar.
//!
//! This binary is copied into every backup repository's root and refreshed on every backup
//! run (see `Repo::refresh_restore_tool`), so a copy of it always sits right next to the
//! snapshots it can read. It needs no configuration, no installation, and no state
//! directory: everything it needs to know is either passed on the command line or read
//! from the repository it is pointed at (by default, wherever it is currently sitting).
//!
//! **It cannot create a backup.** This is a structural property, not a UI restriction: the
//! crate depends only on `backstar-core`'s read side (`repo`, `restore`, `guards`,
//! `events`), never on `engine` -- the only module that can write a new snapshot. There is
//! no code path in this binary that could accidentally start a destructive operation on
//! files it was only asked to browse.
//!
//! Argument parsing is hand-rolled rather than pulling in a CLI-argument crate: this tool's
//! entire reason to exist is being small enough to live comfortably inside someone's backup
//! folder, and its argument surface is four commands and two flags.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use backstar_core::events::Event;
use backstar_core::repo::{Repo, SnapshotManifest};
use backstar_core::restore;

pub const HELP_TEXT: &str = "\
BackStar-Restore -- browse and restore BackStar snapshots. Read-only: cannot back anything up.

USAGE:
  BackStar-Restore list
  BackStar-Restore browse <snapshot-id> [path]
  BackStar-Restore restore <snapshot-id> <path> <destination>
  BackStar-Restore restore <snapshot-id> <path> --original

OPTIONS:
  --repo <dir>   Use this repository instead of the folder this exe is sitting in.
  --json         Print `list`/`browse` output as JSON instead of a table.
  --original     For `restore`: put the item back where it was originally backed up from,
                 as recorded in the snapshot. Fails if that is not recorded.
  -h, --help     Show this text.

EXAMPLES:
  BackStar-Restore list
  BackStar-Restore browse 2026-09-15T14-03-22Z
  BackStar-Restore browse 2026-09-15T14-03-22Z proj/src
  BackStar-Restore restore 2026-09-15T14-03-22Z proj/notes.txt D:\\Recovered
  BackStar-Restore restore 2026-09-15T14-03-22Z proj --original
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreDest {
    Original,
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    List,
    Browse { snapshot_id: String, path: String },
    Restore { snapshot_id: String, path: String, dest: RestoreDest },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub repo: Option<PathBuf>,
    pub json: bool,
    pub command: Command,
}

/// Parse `argv` (not including the program name).
pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut repo = None;
    let mut json = false;
    let mut original = false;
    let mut positional = Vec::new();
    let mut help = false;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--repo" => {
                i += 1;
                let v = argv.get(i).ok_or("--repo needs a path")?;
                repo = Some(PathBuf::from(v));
            }
            "--json" => json = true,
            "--original" => original = true,
            "-h" | "--help" | "help" => help = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?}. Run with --help for usage."))
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    if help || positional.is_empty() {
        return Ok(Args { repo, json, command: Command::Help });
    }

    let command = match positional[0].as_str() {
        "list" => Command::List,
        "browse" => {
            let snapshot_id = positional
                .get(1)
                .ok_or("browse needs a snapshot id -- see BackStar-Restore list")?
                .clone();
            let path = positional.get(2).cloned().unwrap_or_default();
            Command::Browse { snapshot_id, path }
        }
        "restore" => {
            let snapshot_id = positional
                .get(1)
                .ok_or("restore needs a snapshot id -- see BackStar-Restore list")?
                .clone();
            let path = positional
                .get(2)
                .ok_or("restore needs a path inside the snapshot -- see BackStar-Restore browse")?
                .clone();
            let dest = if original {
                RestoreDest::Original
            } else {
                let d = positional
                    .get(3)
                    .ok_or("restore needs a destination path, or pass --original")?;
                RestoreDest::Path(PathBuf::from(d))
            };
            Command::Restore { snapshot_id, path, dest }
        }
        other => {
            return Err(format!(
                "unknown command {other:?}. Run with no arguments or --help for usage."
            ))
        }
    };

    Ok(Args { repo, json, command })
}

/// Where to look for a repository when `--repo` was not given: the folder this executable
/// is sitting in. This is what "the tool ships inside the repository it can read" means in
/// practice -- double-click or run it from wherever it was copied, and it finds its own
/// snapshots with no configuration at all.
pub fn default_repo_dir(exe_path: &Path) -> PathBuf {
    exe_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn human_bytes(bytes: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.0} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

fn outcome_text(o: &backstar_core::events::RunOutcome) -> String {
    use backstar_core::events::RunOutcome;
    match o {
        RunOutcome::Ok => "ok".into(),
        RunOutcome::Partial { failed } => format!("partial ({failed} failed)"),
        RunOutcome::Failed { reason } => format!("failed: {reason}"),
        RunOutcome::Cancelled => "cancelled".into(),
    }
}

fn print_list(manifests: &[SnapshotManifest], json: bool, out: &mut dyn Write) {
    if json {
        let _ = writeln!(out, "{}", serde_json::to_string_pretty(manifests).unwrap_or_default());
        return;
    }
    if manifests.is_empty() {
        let _ = writeln!(out, "No snapshots yet.");
        return;
    }
    for m in manifests {
        let _ = writeln!(
            out,
            "{}  {:>8} files  {:>10}  {}",
            m.id,
            m.files_copied + m.files_linked,
            human_bytes(m.bytes_copied + m.bytes_linked),
            outcome_text(&m.outcome)
        );
    }
}

/// Run the parsed command. `exe_path` is used only to compute the default repo location
/// (see [`default_repo_dir`]) and is ignored when `args.repo` is set -- tests pass a dummy
/// path in that case, since nothing on disk needs to exist at it.
pub fn run(args: &Args, exe_path: &Path, out: &mut dyn Write) -> Result<i32, String> {
    if args.command == Command::Help {
        let _ = write!(out, "{HELP_TEXT}");
        return Ok(0);
    }

    let repo_dir = args.repo.clone().unwrap_or_else(|| default_repo_dir(exe_path));
    let repo = Repo::open(&repo_dir).map_err(|e| {
        format!(
            "{}: {e}\n(pass --repo <folder> to point at a different backup)",
            repo_dir.display()
        )
    })?;

    match &args.command {
        Command::Help => unreachable!("handled above"),

        Command::List => {
            let mut manifests = repo.manifests();
            manifests.sort_by(|a, b| b.id.cmp(&a.id));
            print_list(&manifests, args.json, out);
            Ok(0)
        }

        Command::Browse { snapshot_id, path } => {
            let entries = repo
                .list_snapshot_children(snapshot_id, Path::new(path))
                .map_err(|e| e.to_string())?;
            if args.json {
                let _ =
                    writeln!(out, "{}", serde_json::to_string_pretty(&entries).unwrap_or_default());
            } else if entries.is_empty() {
                let _ = writeln!(out, "(empty)");
            } else {
                for e in &entries {
                    if e.is_dir {
                        let _ = writeln!(out, "  {}/", e.name);
                    } else {
                        let _ = writeln!(
                            out,
                            "  {}  ({})",
                            e.name,
                            human_bytes(e.size.unwrap_or(0))
                        );
                    }
                }
            }
            Ok(0)
        }

        Command::Restore { snapshot_id, path, dest } => {
            let manifest = repo.read_manifest(snapshot_id).map_err(|e| e.to_string())?;
            let rel = PathBuf::from(path);

            let dest_path = match dest {
                RestoreDest::Original => restore::resolve_original_path(&manifest, &rel)
                    .ok_or_else(|| {
                        "no record of where this item came from -- pass a destination path \
                         instead of --original"
                            .to_string()
                    })?,
                RestoreDest::Path(p) => p.clone(),
            };

            backstar_core::guards::ensure_dest_safe(&dest_path, &repo.root)
                .map_err(|e| e.to_string())?;

            let _ = writeln!(out, "Restoring to {}", dest_path.display());

            let cancel = AtomicBool::new(false);
            let mut last_pct: i64 = -1;
            let mut failures = 0u64;

            let stats = restore::restore(&repo, snapshot_id, &rel, &dest_path, &cancel, &mut |ev| {
                match ev {
                    Event::OverallProgress { files_done, files_total, .. } if files_total > 0 => {
                        let pct = (files_done * 100 / files_total) as i64;
                        if pct != last_pct {
                            last_pct = pct;
                            let _ = write!(out, "\r  {files_done}/{files_total} files ({pct}%)");
                            let _ = out.flush();
                        }
                    }
                    Event::FileFailed(err) => {
                        failures += 1;
                        let _ = writeln!(out, "\n  FAILED {}: {}", err.path.display(), err.message);
                    }
                    _ => {}
                }
            })
            .map_err(|e| e.to_string())?;

            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "Restored {} file(s){}.",
                stats.files_copied,
                if failures > 0 { format!(", {failures} failed") } else { String::new() }
            );

            Ok(if stats.files_failed > 0 { 1 } else { 0 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_shows_help() {
        let parsed = parse_args(&[]).unwrap();
        assert_eq!(parsed.command, Command::Help);
    }

    #[test]
    fn help_flags_are_recognised_anywhere() {
        for v in [vec!["--help"], vec!["-h"], vec!["help"], vec!["list", "--help"]] {
            let parsed = parse_args(&args(&v)).unwrap();
            assert_eq!(parsed.command, Command::Help, "failed for {v:?}");
        }
    }

    #[test]
    fn list_parses_with_repo_and_json_flags() {
        let parsed = parse_args(&args(&["--repo", r"D:\Backups", "--json", "list"])).unwrap();
        assert_eq!(parsed.repo, Some(PathBuf::from(r"D:\Backups")));
        assert!(parsed.json);
        assert_eq!(parsed.command, Command::List);
    }

    #[test]
    fn browse_defaults_the_path_to_the_snapshot_root() {
        let parsed = parse_args(&args(&["browse", "2026-01-01"])).unwrap();
        assert_eq!(
            parsed.command,
            Command::Browse { snapshot_id: "2026-01-01".into(), path: String::new() }
        );
    }

    #[test]
    fn browse_accepts_an_explicit_path() {
        let parsed = parse_args(&args(&["browse", "2026-01-01", "proj/src"])).unwrap();
        assert_eq!(
            parsed.command,
            Command::Browse { snapshot_id: "2026-01-01".into(), path: "proj/src".into() }
        );
    }

    #[test]
    fn browse_without_a_snapshot_id_is_a_clear_error() {
        assert!(parse_args(&args(&["browse"])).is_err());
    }

    #[test]
    fn restore_with_a_destination_path() {
        let parsed =
            parse_args(&args(&["restore", "2026-01-01", "proj/a.txt", r"D:\Out"])).unwrap();
        assert_eq!(
            parsed.command,
            Command::Restore {
                snapshot_id: "2026-01-01".into(),
                path: "proj/a.txt".into(),
                dest: RestoreDest::Path(PathBuf::from(r"D:\Out")),
            }
        );
    }

    #[test]
    fn restore_with_original_needs_no_destination_argument() {
        let parsed =
            parse_args(&args(&["restore", "2026-01-01", "proj", "--original"])).unwrap();
        assert_eq!(
            parsed.command,
            Command::Restore {
                snapshot_id: "2026-01-01".into(),
                path: "proj".into(),
                dest: RestoreDest::Original,
            }
        );
    }

    /// `--original` can appear before the positional arguments it modifies -- flags and
    /// positionals are parsed independently of order.
    #[test]
    fn flag_order_does_not_matter() {
        let parsed =
            parse_args(&args(&["--original", "restore", "2026-01-01", "proj"])).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Restore { dest: RestoreDest::Original, .. }
        ));
    }

    #[test]
    fn restore_without_a_destination_or_original_is_a_clear_error() {
        let err = parse_args(&args(&["restore", "2026-01-01", "proj"])).unwrap_err();
        assert!(err.contains("--original"), "unhelpful error: {err}");
    }

    #[test]
    fn an_unknown_command_is_a_clear_error() {
        let err = parse_args(&args(&["frobnicate"])).unwrap_err();
        assert!(err.contains("frobnicate"));
    }

    #[test]
    fn an_unknown_flag_is_a_clear_error() {
        let err = parse_args(&args(&["--bogus", "list"])).unwrap_err();
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn repo_without_a_value_is_a_clear_error() {
        assert!(parse_args(&args(&["--repo"])).is_err());
    }

    #[test]
    fn default_repo_dir_is_the_exe_parent() {
        assert_eq!(
            default_repo_dir(Path::new(r"D:\Backups\BackStar-Restore.exe")),
            PathBuf::from(r"D:\Backups")
        );
    }

    // ---------------------------------------------------------------- run(), end to end

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    /// Build a real repo with one real snapshot, exactly as `engine::run_job` would,
    /// without depending on the `engine` crate module (which this crate never links).
    /// Constructs the snapshot directly through `backstar_core::repo`.
    ///
    /// `repo_root` and `originals_root` must be two genuinely separate trees, matching
    /// reality: a snapshot's manifest records where its sources originally lived, and that
    /// is never inside the backup repository itself. Nesting both under one temp dir (an
    /// earlier version of this fixture did) tripped the repo-overlap safety guard on every
    /// restore -- correctly, since that guard exists precisely to catch this shape.
    fn fixture_repo(repo_root: &Path, originals_root: &Path) -> (Repo, String) {
        let repo = Repo::open_or_init(repo_root, "Test").unwrap();
        let id = repo.allocate_snapshot_id(time_now()).unwrap();
        let snap = repo.snapshot_dir(&id).join("proj");
        touch(&snap.join("notes.txt"), "hello from the fixture");
        touch(&snap.join("sub/deep.txt"), "deep content");

        let manifest = backstar_core::repo::SnapshotManifest {
            id: id.clone(),
            job: "Test".into(),
            started: "2026-01-01T00:00:00Z".into(),
            finished: "2026-01-01T00:01:00Z".into(),
            sources: vec![backstar_core::repo::SnapshotSource {
                name: "proj".into(),
                original: originals_root.join("original-proj"),
                files: 2,
                bytes: 20,
            }],
            files_copied: 2,
            files_linked: 0,
            files_failed: 0,
            bytes_copied: 20,
            bytes_linked: 0,
            duration_ms: 10,
            outcome: backstar_core::events::RunOutcome::Ok,
            parent: None,
        };
        repo.write_manifest(&manifest).unwrap();
        (repo, id)
    }

    fn time_now() -> time::OffsetDateTime {
        // Tests never run concurrently against the same repo, so any fixed instant works;
        // avoiding `OffsetDateTime::now_utc()` keeps this crate's tests reproducible.
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn out_string(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn list_reports_the_real_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        let code = run(
            &Args { repo: Some(tmp.path().to_path_buf()), json: false, command: Command::List },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0);
        let text = out_string(out);
        assert!(text.contains(&id), "expected the snapshot id in: {text}");
        assert!(text.contains("2 "), "expected the file count in: {text}");
    }

    #[test]
    fn list_json_round_trips_as_real_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        run(
            &Args { repo: Some(tmp.path().to_path_buf()), json: true, command: Command::List },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();

        let manifests: Vec<SnapshotManifest> = serde_json::from_slice(&out).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].id, id);
    }

    #[test]
    fn browse_lists_the_source_at_the_snapshot_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        run(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: id, path: String::new() },
            },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();

        assert!(out_string(out).contains("proj/"));
    }

    #[test]
    fn browse_descends_into_a_subpath() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        run(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: id, path: "proj".into() },
            },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();

        let text = out_string(out);
        assert!(text.contains("notes.txt"));
        assert!(text.contains("sub/"));
    }

    #[test]
    fn restore_writes_the_real_file_to_an_explicit_destination() {
        // Two SEPARATE trees, matching reality: a restore destination is never inside the
        // backup repository. Nesting both under one temp dir trips the repo-overlap safety
        // guard (correctly) rather than exercising a normal restore.
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let dest = elsewhere.path().join("out/notes.txt");

        let mut out = Vec::new();
        let code = run(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(dest.clone()),
                },
            },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello from the fixture");
        assert!(out_string(out).contains("Restored 1 file"));
    }

    /// This is the CLI-level proof that the manifest-carried original location actually
    /// works end to end, not just at the `backstar_core::restore` unit level.
    #[test]
    fn restore_original_uses_the_manifest_recorded_location() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let expected = elsewhere.path().join("original-proj").join("notes.txt");

        run(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Original,
                },
            },
            Path::new("ignored"),
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&expected).unwrap(), "hello from the fixture");
    }

    /// The safety guard, exercised at the CLI layer specifically (not just inside
    /// `backstar_core::restore`'s own tests): restoring into the repository's own storage
    /// must be refused, not silently allowed to write ordinary files into a tree other
    /// snapshots hardlink into.
    #[test]
    fn restoring_into_the_repo_itself_is_refused_at_the_cli_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());
        let bad_dest = tmp.path().join("snapshots").join("sneaky.txt");

        let err = run(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(bad_dest.clone()),
                },
            },
            Path::new("ignored"),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(err.contains("overlaps"), "expected an overlap error, got: {err}");
        assert!(!bad_dest.exists(), "nothing should have been written");
    }

    #[test]
    fn opening_a_folder_that_is_not_a_repo_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(
            &Args { repo: Some(tmp.path().to_path_buf()), json: false, command: Command::List },
            Path::new("ignored"),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.contains("--repo"), "should hint at the fix: {err}");
    }

    #[test]
    fn help_needs_no_repo_at_all() {
        // Deliberately points --repo nowhere real; help must still work, since a user's
        // very first run is often just "what do I do with this exe".
        let mut out = Vec::new();
        let code = run(
            &Args {
                repo: Some(PathBuf::from("Z:\\does\\not\\exist")),
                json: false,
                command: Command::Help,
            },
            Path::new("ignored"),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(out_string(out).contains("USAGE"));
    }

    /// The structural safety property, checked as directly as a test can: this crate's own
    /// PRODUCTION source never references the one module that can create or mutate a
    /// snapshot. Cargo has no "forbid this dependency" primitive, so this is a deliberate
    /// self-check -- grepping this crate's own source for the exact API that would make it
    /// a backup tool rather than a restore tool.
    ///
    /// Scoped to the text before `#[cfg(test)]`: this very test's own source necessarily
    /// names the forbidden strings in order to check for them, so scanning the whole file
    /// verbatim would make the check fail against itself.
    #[test]
    fn this_crate_never_references_the_backup_engine() {
        let forbidden = ["backstar_core :: engine", "backstar_core::engine", "run_job"];
        for (name, full_src) in [
            ("lib.rs", include_str!("lib.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            let production_src =
                full_src.split("#[cfg(test)]").next().expect("split always yields at least one part");
            for needle in forbidden {
                assert!(
                    !production_src.contains(needle),
                    "{name} references {needle:?} outside its test module -- this tool must \
                     stay read-only"
                );
            }
        }
    }
}
