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
//! folder, and its argument surface is four commands and a handful of flags.

use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

use backstar_core::events::Event;
use backstar_core::repo::{Repo, SnapshotListing, SnapshotManifest};
use backstar_core::restore::{self, OverwritePolicy};

pub const HELP_TEXT: &str = "\
BackStar-Restore -- browse and restore BackStar snapshots. Read-only: cannot back anything up.

USAGE:
  BackStar-Restore list
  BackStar-Restore browse <snapshot-id> [path]
  BackStar-Restore restore <snapshot-id> <path> <destination-folder> [--yes]
  BackStar-Restore restore <snapshot-id> <path> --original [--yes]

OPTIONS:
  --repo <dir>   Use this repository instead of the folder this exe is sitting in.
                 The form --repo=<dir> works too.
  --json         Print `list`/`browse` output as JSON instead of a table.
  --original     For `restore`: put the item back where it was originally backed up from,
                 as recorded in the snapshot. Fails if that is not recorded.
  --yes          For `restore`: overwrite EVERYTHING at the destination, including files
                 newer than the snapshot copy. Without it, a destination file newer than
                 the snapshot is never touched silently: you are asked interactively, or,
                 when stdin is not a terminal, the restore is refused.
  --version      Print the version and exit.
  -h, --help     Show this text.

DESTINATIONS:
  A restore destination is a FOLDER the item is restored INTO, keeping its own name:
  restoring file `proj/notes.txt` to D:\\Recovered writes D:\\Recovered\\notes.txt, and
  restoring folder `proj` to D:\\Recovered writes D:\\Recovered\\proj\\... (the same rule
  the BackStar app's restore dialog applies).

EXIT CODES:
  0  success.
  1  the operation failed, or completed only partially (some files could not be
     restored, or newer destination files were kept rather than overwritten).
  2  usage error -- unknown command or option, or missing/conflicting arguments.
     The error message says what to do next.

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
    Version,
    List,
    Browse { snapshot_id: String, path: String },
    Restore { snapshot_id: String, path: String, dest: RestoreDest, overwrite_all: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub repo: Option<PathBuf>,
    pub json: bool,
    pub command: Command,
}

/// F51: an argument the command has no slot for is a hard error, never silently ignored --
/// a surplus token usually means a typo somewhere else in the line.
fn no_surplus_arguments(positional: &[String], from: usize) -> Result<(), String> {
    if let Some(extra) = positional.get(from) {
        Err(format!("unexpected extra argument {extra:?}. Run with --help for usage."))
    } else {
        Ok(())
    }
}

/// Parse `argv` (not including the program name).
pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut repo = None;
    let mut json = false;
    let mut original = false;
    let mut overwrite_all = false;
    let mut version = false;
    let mut positional = Vec::new();
    let mut help = false;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--repo" => {
                i += 1;
                let v = argv.get(i).ok_or("--repo needs a path")?;
                // F52: `--repo --json` must not silently swallow the flag as a path.
                if v.starts_with("--") {
                    return Err(format!(
                        "--repo needs a path, but the next argument looks like a flag ({v:?}) \
                         -- use --repo=<dir> if the path really begins with dashes"
                    ));
                }
                repo = Some(PathBuf::from(v));
            }
            // F52: the `=` form, so paths that look like flags can still be passed.
            _ if arg.starts_with("--repo=") => {
                let v = &arg["--repo=".len()..];
                if v.is_empty() {
                    return Err("--repo needs a path (got an empty --repo= value)".to_string());
                }
                repo = Some(PathBuf::from(v));
            }
            "--json" => json = true,
            "--original" => original = true,
            "--yes" => overwrite_all = true,
            "--version" => version = true,
            "-h" | "--help" | "help" => help = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?}. Run with --help for usage."))
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    if help || (!version && positional.is_empty()) {
        return Ok(Args { repo, json, command: Command::Help });
    }
    if version {
        return Ok(Args { repo, json, command: Command::Version });
    }

    let command = match positional[0].as_str() {
        "list" => {
            no_surplus_arguments(&positional, 1)?;
            Command::List
        }
        "browse" => {
            let snapshot_id = positional
                .get(1)
                .ok_or("browse needs a snapshot id -- see BackStar-Restore list")?
                .clone();
            let path = positional.get(2).cloned().unwrap_or_default();
            no_surplus_arguments(&positional, 3)?;
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
            let dest = match (positional.get(3), original) {
                // F51: the two ways to choose a destination contradict each other.
                (Some(_), true) => {
                    return Err(
                        "restore takes a destination folder OR --original, not both -- drop \
                         one of them"
                            .to_string(),
                    )
                }
                (Some(d), false) => RestoreDest::Path(PathBuf::from(d)),
                (None, true) => RestoreDest::Original,
                (None, false) => {
                    return Err("restore needs a destination folder, or pass --original"
                        .to_string())
                }
            };
            no_surplus_arguments(&positional, 4)?;
            Command::Restore { snapshot_id, path, dest, overwrite_all }
        }
        other => {
            return Err(format!(
                "unknown command {other:?}. Run with no arguments or --help for usage."
            ))
        }
    };

    // F51: flags that only mean something for `restore` must not be silently ignored.
    if original && !matches!(command, Command::Restore { .. }) {
        return Err("--original only applies to restore".to_string());
    }
    if overwrite_all && !matches!(command, Command::Restore { .. }) {
        return Err("--yes only applies to restore".to_string());
    }

    Ok(Args { repo, json, command })
}

/// Where to look for a repository when `--repo` was not given: the folder this executable
/// is sitting in. This is what "the tool ships inside the repository it can read" means in
/// practice -- double-click or run it from wherever it was copied, and it finds its own
/// snapshots with no configuration at all.
pub fn default_repo_dir(exe_path: &Path) -> PathBuf {
    exe_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

/// Validate a snapshot id from the command line (F21): exactly the shape
/// `Repo::allocate_snapshot_id` produces -- `2026-09-15T14-03-22Z` with an optional
/// `-002` suffix. Anything else (path separators, dots, drive letters) is rejected before
/// it can be joined onto a filesystem path.
///
/// Duplicated from the app shell's `restore_cmd::validate_snapshot_id` (a private free
/// function there) rather than shared through backstar-core: the sanctioned core surface
/// for this work is `restore::compute_destination` alone, and fifteen lines of duplication
/// is the cheaper coupling. Keep the two in sync.
fn validate_snapshot_id(id: &str) -> Result<(), String> {
    let ok = match id.len() {
        20 => true,
        // "<id>-002": dash plus exactly three digits.
        24 => {
            let sfx = &id[20..];
            sfx.starts_with('-') && sfx[1..].bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    };
    let shaped = ok && {
        let stem = &id.as_bytes()[..20];
        stem[4] == b'-' && stem[7] == b'-' && stem[10] == b'T' && stem[13] == b'-'
            && stem[16] == b'-' && stem[19] == b'Z'
            && stem
                .iter()
                .enumerate()
                .filter(|(i, _)| ![4, 7, 10, 13, 16, 19].contains(i))
                .all(|(_, c)| c.is_ascii_digit())
    };
    if shaped {
        Ok(())
    } else {
        Err(format!("invalid snapshot id {id:?}"))
    }
}

/// Validate a path inside a snapshot (F21): only plain relative components -- no
/// absolutes, no `..`, no prefixes, no roots, no leading `.`. The empty path is valid: it
/// names the snapshot's own root. Same duplication note as [`validate_snapshot_id`].
fn validate_snapshot_rel(rel: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(rel);
    let ok = p.components().all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(p)
    } else {
        Err(format!("invalid path inside a snapshot: {rel:?}"))
    }
}

/// F21/F50: after the (cheap) shape check, the stronger membership check -- a typo'd id
/// gets a plain sentence with the next step, not a raw OS error naming an internal
/// manifest path.
fn require_snapshot(repo: &Repo, snapshot_id: &str) -> Result<(), String> {
    if repo.snapshot_ids().iter().any(|id| id == snapshot_id) {
        Ok(())
    } else {
        Err(format!("no snapshot named {snapshot_id} -- run BackStar-Restore list"))
    }
}

/// F50: the manifest read, with the common mistakes translated. The id was
/// membership-checked by the time this runs, so a missing manifest means the snapshot
/// directory is an interrupted run's leftover (F35) -- say that, rather than claiming the
/// snapshot does not exist.
fn read_manifest_friendly(repo: &Repo, snapshot_id: &str) -> Result<SnapshotManifest, String> {
    match repo.read_manifest(snapshot_id) {
        Ok(m) => Ok(m),
        Err(backstar_core::Error::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(format!(
                "snapshot {snapshot_id} has no manifest -- it is incomplete (the leftover \
                 of an interrupted run). You can still browse it and restore from it with \
                 an explicit destination."
            ))
        }
        Err(e) => Err(format!("could not read the manifest of snapshot {snapshot_id}: {e}")),
    }
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

/// F49: a damaged repository must never look empty. Readable manifests print normally;
/// snapshots whose manifests are unreadable (tree intact, bookkeeping damaged) and
/// incomplete leftovers of interrupted runs get explicit rows.
fn print_list(listing: &SnapshotListing, json: bool, out: &mut dyn Write) {
    if json {
        let _ = writeln!(out, "{}", serde_json::to_string_pretty(listing).unwrap_or_default());
        return;
    }
    if listing.manifests.is_empty() && listing.unreadable.is_empty() && listing.incomplete.is_empty()
    {
        let _ = writeln!(out, "No snapshots yet.");
        return;
    }
    for m in &listing.manifests {
        let _ = writeln!(
            out,
            "{}  {:>8} files  {:>10}  {}",
            m.id,
            m.files_copied + m.files_linked,
            human_bytes(m.bytes_copied + m.bytes_linked),
            outcome_text(&m.outcome)
        );
    }
    for (id, err) in &listing.unreadable {
        let _ = writeln!(out, "{id}  (manifest unreadable: {err})");
    }
    for id in &listing.incomplete {
        let _ = writeln!(out, "{id}  (incomplete -- likely interrupted run)");
    }
}

/// D5/F22: map the pre-flight plan and the user's confirmation to an overwrite policy.
///
/// The default is `IfOlder` -- a restore never clobbers a destination file NEWER than the
/// snapshot copy without an explicit yes. When the plan found such files and `--yes` was
/// not given, the user is asked interactively (stdin is a terminal); answering anything
/// but "y" keeps the newer files and restores everything else. Without a terminal there
/// is no one to ask, so the restore is refused outright, naming the flag: a scripted run
/// must not silently destroy newer work, and must not silently skip it either.
fn overwrite_policy(
    plan: &restore::RestorePlan,
    overwrite_all: bool,
    stdin_is_tty: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<OverwritePolicy, String> {
    if overwrite_all {
        return Ok(OverwritePolicy::Always);
    }
    if plan.would_overwrite_newer == 0 {
        return Ok(OverwritePolicy::IfOlder);
    }
    if !stdin_is_tty {
        return Err(format!(
            "{} destination file(s) are NEWER than the snapshot copy -- refusing to \
             overwrite them without confirmation. Rerun with --yes to overwrite \
             everything, or choose a different destination.",
            plan.would_overwrite_newer
        ));
    }
    let _ = write!(
        out,
        "Overwrite {} file(s) that are NEWER than the snapshot copy? [y/N] \
         (anything but y keeps those files and restores the rest) ",
        plan.would_overwrite_newer
    );
    let _ = out.flush();
    let mut answer = String::new();
    // A closed or broken input reads as the safe answer.
    let _ = input.read_line(&mut answer);
    if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        Ok(OverwritePolicy::Always)
    } else {
        Ok(OverwritePolicy::IfOlder)
    }
}

/// Run the parsed command. `exe_path` is used only to compute the default repo location
/// (see [`default_repo_dir`]) and is ignored when `args.repo` is set -- tests pass a dummy
/// path in that case, since nothing on disk needs to exist at it.
///
/// `input`/`stdin_is_tty` exist for the D5 overwrite confirmation: the prompt is asked
/// only when stdin is a terminal, and tests inject both.
pub fn run(
    args: &Args,
    exe_path: &Path,
    out: &mut dyn Write,
    input: &mut dyn BufRead,
    stdin_is_tty: bool,
) -> Result<i32, String> {
    match &args.command {
        Command::Help => {
            let _ = write!(out, "{HELP_TEXT}");
            return Ok(0);
        }
        Command::Version => {
            let _ = writeln!(out, "BackStar-Restore {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        _ => {}
    }

    let repo_dir = args.repo.clone().unwrap_or_else(|| default_repo_dir(exe_path));
    let repo = Repo::open(&repo_dir).map_err(|e| {
        format!(
            "{}: {e}\n(pass --repo <folder> to point at a different backup)",
            repo_dir.display()
        )
    })?;

    match &args.command {
        Command::Help | Command::Version => unreachable!("handled above"),

        Command::List => {
            // F49: the full listing, newest first -- damaged snapshots included.
            let mut listing = repo.list_snapshots_full();
            listing.manifests.reverse();
            listing.unreadable.reverse();
            listing.incomplete.reverse();
            print_list(&listing, args.json, out);
            Ok(0)
        }

        Command::Browse { snapshot_id, path } => {
            validate_snapshot_id(snapshot_id)?;
            let rel = validate_snapshot_rel(path)?;
            require_snapshot(&repo, snapshot_id)?;
            let entries = match repo.list_snapshot_children(snapshot_id, &rel) {
                Ok(entries) => entries,
                // F50: a typo'd in-snapshot path gets a friendly sentence, not a raw OS
                // error carrying an internal path.
                Err(backstar_core::Error::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Err(format!("nothing at {path} in snapshot {snapshot_id}"));
                }
                Err(e) => return Err(e.to_string()),
            };
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

        Command::Restore { snapshot_id, path, dest, overwrite_all } => {
            validate_snapshot_id(snapshot_id)?;
            let rel = validate_snapshot_rel(path)?;
            require_snapshot(&repo, snapshot_id)?;

            // F24: the manifest is read ONLY for --original. An explicit-destination
            // restore never needs it, so a snapshot whose manifest is gone (damaged or
            // incomplete) is still restorable.
            let dest_path = match dest {
                RestoreDest::Original => {
                    let manifest = read_manifest_friendly(&repo, snapshot_id)?;
                    restore::resolve_original_path(&manifest, &rel).ok_or_else(|| {
                        "no record of where this item came from -- pass a destination path \
                         instead of --original"
                            .to_string()
                    })?
                }
                RestoreDest::Path(p) => restore::compute_destination(p, &rel)
                    .map_err(|e| e.to_string())?,
            };

            backstar_core::guards::ensure_dest_safe(&dest_path, &repo.root)
                .map_err(|e| e.to_string())?;

            // D5/F22: the overwrite facts, computed before a single byte moves. The plan
            // shares its walk shape with the execution inside backstar-core, so these
            // counts describe what the copy phase below will actually touch.
            let plan = restore::plan_restore(&repo, snapshot_id, &rel, &dest_path)
                .map_err(|e| e.to_string())?;
            if plan.would_overwrite > 0 {
                let _ = writeln!(
                    out,
                    "{} existing file(s) will be overwritten, {} of them newer than the \
                     snapshot copy",
                    plan.would_overwrite, plan.would_overwrite_newer
                );
            }
            let policy =
                overwrite_policy(&plan, *overwrite_all, stdin_is_tty, input, out)?;

            let _ = writeln!(out, "Restoring to {}", dest_path.display());

            let cancel = AtomicBool::new(false);
            let mut last_pct: i64 = -1;
            let mut failures = 0u64;

            let stats = restore::restore(&repo, snapshot_id, &rel, &dest_path,
                policy, &cancel, &mut |ev| {
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
                    Event::RunWarning { message } => {
                        let _ = writeln!(out, "\n  WARNING: {message}");
                    }
                    _ => {}
                }
            })
            .map_err(|e| e.to_string())?;

            let _ = writeln!(out);
            let mut summary = format!("Restored {} file(s)", stats.files_copied);
            if stats.files_skipped > 0 {
                summary.push_str(&format!(
                    ", kept {} newer file(s)",
                    stats.files_skipped
                ));
            }
            if stats.files_failed > 0 {
                summary.push_str(&format!(", {} failed", stats.files_failed));
            }
            let _ = writeln!(out, "{summary}.");
            if stats.files_skipped > 0 {
                let _ = writeln!(
                    out,
                    "  (rerun with --yes to overwrite the kept file(s) too)"
                );
            }

            // A partial restore -- files failed, or newer files kept -- is not a clean
            // success: exit 1 (see the EXIT CODES section of --help).
            Ok(if stats.files_failed > 0 || stats.files_skipped > 0 { 1 } else { 0 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The run() shape most tests want: no terminal on stdin, nothing to read.
    fn run_cli(args: &Args, out: &mut Vec<u8>) -> Result<i32, String> {
        run(args, Path::new("ignored"), out, &mut &[][..], false)
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
                overwrite_all: false,
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
                overwrite_all: false,
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

    // ------------------------------------------------------------ F51: strictness

    #[test]
    fn restore_with_both_a_destination_and_original_is_rejected() {
        let err = parse_args(&args(&[
            "restore", "2026-01-01T00-00-00Z", "proj", r"D:\Out", "--original",
        ]))
        .unwrap_err();
        assert!(err.contains("not both"), "unhelpful error: {err}");
    }

    #[test]
    fn surplus_positional_arguments_are_rejected() {
        for v in [
            vec!["list", "extra"],
            vec!["browse", "2026-01-01T00-00-00Z", "proj", "extra"],
            vec!["restore", "2026-01-01T00-00-00Z", "proj", r"D:\Out", "extra"],
        ] {
            let err = parse_args(&args(&v)).unwrap_err();
            assert!(err.contains("unexpected extra argument"), "failed for {v:?}: {err}");
            assert!(err.contains("extra"), "names the surplus token for {v:?}: {err}");
        }
    }

    #[test]
    fn restore_only_flags_are_rejected_for_other_commands() {
        let err = parse_args(&args(&["list", "--original"])).unwrap_err();
        assert!(err.contains("--original"), "{err}");
        let err = parse_args(&args(&["browse", "2026-01-01T00-00-00Z", "--yes"])).unwrap_err();
        assert!(err.contains("--yes"), "{err}");
    }

    // ------------------------------------------------------------ F52: --repo forms

    #[test]
    fn repo_flag_does_not_swallow_another_flag_as_its_value() {
        let err = parse_args(&args(&["--repo", "--json", "list"])).unwrap_err();
        assert!(err.contains("--repo"), "{err}");
        assert!(err.contains("--json"), "names the offending token: {err}");
    }

    #[test]
    fn repo_flag_supports_the_equals_form() {
        let parsed = parse_args(&args(&["--repo=D:\\Backups", "list"])).unwrap();
        assert_eq!(parsed.repo, Some(PathBuf::from(r"D:\Backups")));
        assert_eq!(parsed.command, Command::List);

        let err = parse_args(&args(&["--repo=", "list"])).unwrap_err();
        assert!(err.contains("--repo"), "{err}");
    }

    // ------------------------------------------------------------ F53: version + help

    #[test]
    fn version_prints_the_crate_version() {
        let parsed = parse_args(&args(&["--version"])).unwrap();
        assert_eq!(parsed.command, Command::Version);

        // No repo anywhere near: --version must work regardless.
        let mut out = Vec::new();
        let code = run_cli(&parsed, &mut out).unwrap();
        assert_eq!(code, 0);
        let text = out_string(out);
        let version = text
            .trim()
            .strip_prefix("BackStar-Restore ")
            .unwrap_or_else(|| panic!("unexpected --version output: {text}"));
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "a semver-ish version: {version}");
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()), "{version}");
    }

    #[test]
    fn help_documents_the_flags_destinations_and_exit_codes() {
        for needle in [
            "--yes",
            "--version",
            "--repo=<dir>",
            "EXIT CODES",
            "0  success",
            "2  usage error",
            // F23: the real destination rule.
            "keeping its own name",
        ] {
            assert!(HELP_TEXT.contains(needle), "help is missing {needle:?}");
        }
    }

    #[test]
    fn default_repo_dir_is_the_exe_parent() {
        assert_eq!(
            default_repo_dir(Path::new(r"D:\Backups\BackStar-Restore.exe")),
            PathBuf::from(r"D:\Backups")
        );
    }

    // ------------------------------------------------- F21: validation, unit level

    /// Mirrors the app shell's `restore_cmd` test: snapshot ids from the command line
    /// must match the shape the engine produces -- anything else is rejected before it
    /// touches the filesystem.
    #[test]
    fn snapshot_ids_are_validated_by_shape() {
        assert!(validate_snapshot_id("2026-09-15T14-03-22Z").is_ok());
        assert!(validate_snapshot_id("2026-09-15T14-03-22Z-002").is_ok());

        for bad in [
            "",
            "2026-09-15",
            "2026-09-15T14-03-22Z-2",     // suffix must be exactly three digits
            "2026-09-15T14-03-22Z-0022",
            "2026-09-15T14-03-22Z-abc",
            "..",
            "../..",
            "2026/09/15",
            r"C:\Windows",
            "2026-09-15T14-03-22",        // missing the Z
            "2026-09-15T14-03-22Z/extra",
            "2026-09-15T14-03-22Z-extra",
        ] {
            assert!(validate_snapshot_id(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// Mirrors the app shell's test: snapshot-relative paths must be plain relative
    /// components only. (`Path::components` normalises interior/trailing `.` away, so
    /// `proj/.` is provably `proj` -- the dangerous shapes are parents, roots, prefixes,
    /// and a LEADING `./`, which is preserved and hence rejected.)
    #[test]
    fn snapshot_rel_rejects_traversal_and_absolute_paths() {
        assert_eq!(validate_snapshot_rel("").unwrap(), PathBuf::from(""));
        assert_eq!(
            validate_snapshot_rel("proj/sub/file.txt").unwrap(),
            PathBuf::from("proj/sub/file.txt")
        );
        assert_eq!(validate_snapshot_rel(r"proj\sub").unwrap(), PathBuf::from(r"proj\sub"));
        assert_eq!(validate_snapshot_rel("proj/.").unwrap(), PathBuf::from("proj"));
        assert_eq!(validate_snapshot_rel("proj/./sub").unwrap(), PathBuf::from("proj/sub"));

        for bad in [
            "..",
            "../escape",
            "proj/../escape",
            r"..\escape",
            "/absolute",
            r"\absolute",
            r"C:\abs",
            r"\\server\share",
            "./proj",
        ] {
            assert!(validate_snapshot_rel(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    // ---------------------------------------------------------------- run(), end to end

    fn touch(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    /// Build a real repo with one real snapshot, exactly as a backup run would,
    /// without depending on the engine crate module (which this crate never links).
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
            skipped_sources: vec![],
            copied_hashes: Default::default(),
        };
        repo.write_manifest(&manifest).unwrap();
        (repo, id)
    }

    /// Add a bare snapshot directory (no manifest written) to a fixture repo.
    fn add_snapshot_dir(repo: &Repo, id: &str) {
        let dir = repo.snapshot_dir(id).join("proj");
        touch(&dir.join("f.txt"), "x");
    }

    /// Where `Repo::write_manifest` puts the manifest for `id`.
    fn manifest_path(repo: &Repo, id: &str) -> PathBuf {
        repo.root
            .join(backstar_core::repo::META_DIR)
            .join(backstar_core::repo::SNAPSHOTS_DIR)
            .join(format!("{id}.json"))
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
        let code = run_cli(
            &Args { repo: Some(tmp.path().to_path_buf()), json: false, command: Command::List },
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0);
        let text = out_string(out);
        assert!(text.contains(&id), "expected the snapshot id in: {text}");
        assert!(text.contains("2 "), "expected the file count in: {text}");
    }

    #[test]
    fn list_json_round_trips_as_a_full_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        run_cli(
            &Args { repo: Some(tmp.path().to_path_buf()), json: true, command: Command::List },
            &mut out,
        )
        .unwrap();

        let listing: SnapshotListing = serde_json::from_slice(&out).unwrap();
        assert_eq!(listing.manifests.len(), 1);
        assert_eq!(listing.manifests[0].id, id);
        assert!(listing.unreadable.is_empty());
        assert!(listing.incomplete.is_empty());
    }

    /// F49: a damaged repository must never look empty -- the unreadable-manifest and
    /// incomplete snapshots get explicit rows alongside the healthy one.
    #[test]
    fn list_shows_damaged_snapshots_instead_of_hiding_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, good_id) = fixture_repo(tmp.path(), tmp.path());
        let incomplete_id = "2026-01-02T00-00-00Z";
        let corrupt_id = "2026-01-03T00-00-00Z";
        add_snapshot_dir(&repo, incomplete_id);
        add_snapshot_dir(&repo, corrupt_id);
        std::fs::write(manifest_path(&repo, corrupt_id), "{ this is not json").unwrap();

        let mut out = Vec::new();
        let code = run_cli(
            &Args { repo: Some(tmp.path().to_path_buf()), json: false, command: Command::List },
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0);
        let text = out_string(out);
        assert!(text.contains(&good_id), "the healthy snapshot: {text}");
        assert!(
            text.contains(&format!("{incomplete_id}  (incomplete -- likely interrupted run)")),
            "the manifest-less snapshot is shown as incomplete: {text}"
        );
        assert!(
            text.contains(corrupt_id) && text.contains("manifest unreadable"),
            "the corrupt-manifest snapshot is shown with its damage: {text}"
        );
    }

    /// F49, machine-readable half: the JSON gains the two damage lists alongside the
    /// readable manifests.
    #[test]
    fn list_json_carries_the_damage_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _good_id) = fixture_repo(tmp.path(), tmp.path());
        let incomplete_id = "2026-01-02T00-00-00Z";
        let corrupt_id = "2026-01-03T00-00-00Z";
        add_snapshot_dir(&repo, incomplete_id);
        add_snapshot_dir(&repo, corrupt_id);
        std::fs::write(manifest_path(&repo, corrupt_id), "{ this is not json").unwrap();

        let mut out = Vec::new();
        run_cli(
            &Args { repo: Some(tmp.path().to_path_buf()), json: true, command: Command::List },
            &mut out,
        )
        .unwrap();

        let listing: SnapshotListing = serde_json::from_slice(&out).unwrap();
        assert_eq!(listing.manifests.len(), 1);
        assert_eq!(listing.incomplete, vec![incomplete_id.to_string()]);
        assert_eq!(listing.unreadable.len(), 1);
        assert_eq!(listing.unreadable[0].0, corrupt_id);
    }

    #[test]
    fn browse_lists_the_source_at_the_snapshot_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let mut out = Vec::new();
        run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: id, path: String::new() },
            },
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
        run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: id, path: "proj".into() },
            },
            &mut out,
        )
        .unwrap();

        let text = out_string(out);
        assert!(text.contains("notes.txt"));
        assert!(text.contains("sub/"));
    }

    // ------------------------------------------- F21/F50: validation + friendly errors

    #[test]
    fn browse_rejects_a_malformed_snapshot_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, _id) = fixture_repo(tmp.path(), tmp.path());

        let err = run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: "../..".into(), path: String::new() },
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.contains("invalid snapshot id"), "{err}");
    }

    #[test]
    fn browse_rejects_a_traversal_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        for bad in ["../escape", r"..\escape", r"C:\abs", "./proj"] {
            let err = run_cli(
                &Args {
                    repo: Some(tmp.path().to_path_buf()),
                    json: false,
                    command: Command::Browse { snapshot_id: id.clone(), path: bad.into() },
                },
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(err.contains("invalid path inside a snapshot"), "{bad:?}: {err}");
        }
    }

    /// F21+F50: a well-formed but unknown id gets the membership error with the next
    /// step -- and no raw OS error text.
    #[test]
    fn an_unknown_snapshot_id_is_friendly_and_points_at_list() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, _id) = fixture_repo(tmp.path(), tmp.path());

        let err = run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse {
                    snapshot_id: "2026-01-01T00-00-00Z".into(),
                    path: String::new(),
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "no snapshot named 2026-01-01T00-00-00Z -- run BackStar-Restore list"
        );
        assert!(!err.contains("io error"), "raw OS error leaked: {err}");
    }

    /// F50: browsing a path the snapshot does not contain is a friendly sentence.
    #[test]
    fn browsing_a_missing_path_is_friendly() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(tmp.path(), tmp.path());

        let err = run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Browse { snapshot_id: id.clone(), path: "ghost".into() },
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(err, format!("nothing at ghost in snapshot {id}"));
        assert!(!err.contains("io error"), "raw OS error leaked: {err}");
    }

    /// F50: the same friendly shape for restore (this one comes from backstar-core's
    /// restore pre-flight).
    #[test]
    fn restoring_a_missing_path_is_friendly() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());

        let err = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id.clone(),
                    path: "ghost".into(),
                    dest: RestoreDest::Path(elsewhere.path().join("out")),
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.contains(&format!("nothing at ghost in snapshot {id}")), "{err}");
        assert!(!err.contains("io error"), "raw OS error leaked: {err}");
    }

    #[test]
    fn restore_rejects_a_traversal_path() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());

        let err = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/../escape".into(),
                    dest: RestoreDest::Path(elsewhere.path().join("out")),
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.contains("invalid path inside a snapshot"), "{err}");
        assert!(!elsewhere.path().join("out").exists(), "nothing should have been written");
    }

    // ------------------------------------------------------------ F23: destinations

    /// F23: the destination is a FOLDER the item is restored INTO under its own name --
    /// the same rule the app's restore dialog applies (both tools share
    /// `restore::compute_destination`, so they cannot drift).
    #[test]
    fn a_file_restores_into_the_destination_folder_keeping_its_name() {
        // Two SEPARATE trees, matching reality: a restore destination is never inside the
        // backup repository. Nesting both under one temp dir trips the repo-overlap safety
        // guard (correctly) rather than exercising a normal restore.
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let dest = elsewhere.path().join("out");

        let mut out = Vec::new();
        let code = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(dest.clone()),
                    overwrite_all: false,
                },
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(dest.join("notes.txt")).unwrap(),
            "hello from the fixture",
            "the file lands at <folder>/notes.txt, never AS a file named after the folder"
        );
        assert!(out_string(out).contains("Restored 1 file"));
    }

    /// F23: restoring a directory creates a same-named subfolder under the destination --
    /// its contents are never scattered across the destination itself.
    #[test]
    fn a_directory_restores_as_a_named_subfolder_of_the_destination() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let dest = elsewhere.path().join("out");

        let code = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj".into(),
                    dest: RestoreDest::Path(dest.clone()),
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(code, 0);
        let target = dest.join("proj");
        assert_eq!(std::fs::read_to_string(target.join("notes.txt")).unwrap(), "hello from the fixture");
        assert_eq!(std::fs::read_to_string(target.join("sub/deep.txt")).unwrap(), "deep content");
        assert!(!dest.join("notes.txt").exists(), "contents must not scatter into the destination");
    }

    /// This is the CLI-level proof that the manifest-carried original location actually
    /// works end to end, not just at the `backstar_core::restore` unit level.
    #[test]
    fn restore_original_uses_the_manifest_recorded_location() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (_repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let expected = elsewhere.path().join("original-proj").join("notes.txt");

        run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Original,
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&expected).unwrap(), "hello from the fixture");
    }

    // ------------------------------------------------------------ F24: lazy manifest

    /// F24: restoring with an explicit destination never needs the manifest -- a snapshot
    /// whose manifest is gone (deleted, or an interrupted run's leftover) is still
    /// restorable.
    #[test]
    fn restore_with_an_explicit_destination_needs_no_manifest() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        std::fs::remove_file(manifest_path(&repo, &id)).unwrap();

        let mut out = Vec::new();
        let code = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(elsewhere.path().join("out")),
                    overwrite_all: false,
                },
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(code, 0, "{}", out_string(out));
        assert_eq!(
            std::fs::read_to_string(elsewhere.path().join("out/notes.txt")).unwrap(),
            "hello from the fixture"
        );
    }

    /// F24/F50: --original is the one path that genuinely needs the manifest, and on a
    /// manifest-less snapshot it fails with a clear, honest error (not a raw OS error).
    #[test]
    fn restore_original_without_a_manifest_is_a_clear_error() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        std::fs::remove_file(manifest_path(&repo, &id)).unwrap();

        let err = run_cli(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Original,
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(err.contains("no manifest"), "{err}");
        assert!(err.contains("incomplete"), "{err}");
        assert!(!err.contains("io error"), "raw OS error leaked: {err}");
    }

    // ------------------------------------------------------- F22/D5: overwrite safety

    /// A real snapshot of proj/{notes.txt,sub/deep.txt}, plus a LIVE destination tree
    /// holding a NEWER copy of notes.txt (someone's unsaved work) and an OLDER copy of
    /// sub/deep.txt (stale). Returns (snapshot id, destination folder the user would pass).
    fn overwrite_fixture(repo_root: &Path, elsewhere: &Path) -> (String, PathBuf) {
        let (repo, id) = fixture_repo(repo_root, elsewhere);
        let snap_notes_mtime = std::fs::metadata(repo.snapshot_dir(&id).join("proj/notes.txt"))
            .unwrap()
            .modified()
            .unwrap();

        // Restoring `proj` to `live` targets live/proj/... (compute_destination).
        let dest = elsewhere.join("live");
        touch(&dest.join("proj/notes.txt"), "LOCAL WORK -- newer than the snapshot");
        touch(&dest.join("proj/sub/deep.txt"), "stale local");
        let set_mtime = |p: &Path, t: std::time::SystemTime| {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        };
        set_mtime(
            &dest.join("proj/notes.txt"),
            snap_notes_mtime + std::time::Duration::from_secs(3600),
        );
        set_mtime(
            &dest.join("proj/sub/deep.txt"),
            snap_notes_mtime - std::time::Duration::from_secs(3600),
        );
        (id, dest)
    }

    fn restore_proj_args(repo_root: &Path, id: &str, dest: &Path, overwrite_all: bool) -> Args {
        Args {
            repo: Some(repo_root.to_path_buf()),
            json: false,
            command: Command::Restore {
                snapshot_id: id.to_string(),
                path: "proj".into(),
                dest: RestoreDest::Path(dest.to_path_buf()),
                overwrite_all,
            },
        }
    }

    /// D5: the default NEVER clobbers newer work. At a terminal, answering anything but
    /// "y" keeps the newer file and restores everything else -- a partial restore, exit 1.
    #[test]
    fn newer_destination_files_are_kept_by_default_and_the_rest_is_restored() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (id, dest) = overwrite_fixture(repo_tmp.path(), elsewhere.path());

        let mut out = Vec::new();
        let code = run(
            &restore_proj_args(repo_tmp.path(), &id, &dest, false),
            Path::new("ignored"),
            &mut out,
            &mut &b"n\n"[..],
            true, // stdin is a terminal: the prompt fires, answered "n"
        )
        .unwrap();

        assert_eq!(code, 1, "a partial restore exits non-zero");
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/notes.txt")).unwrap(),
            "LOCAL WORK -- newer than the snapshot",
            "the newer file is never clobbered without an explicit yes"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/sub/deep.txt")).unwrap(),
            "deep content",
            "the merely-old file is restored"
        );
        let text = out_string(out);
        assert!(
            text.contains("2 existing file(s) will be overwritten, 1 of them newer"),
            "the plan line: {text}"
        );
        assert!(text.contains("kept 1 newer file(s)"), "the summary: {text}");
    }

    /// D5: `--yes` is the explicit confirmation -- everything is overwritten.
    #[test]
    fn yes_flag_overwrites_even_newer_files() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (id, dest) = overwrite_fixture(repo_tmp.path(), elsewhere.path());

        let mut out = Vec::new();
        let code = run(
            &restore_proj_args(repo_tmp.path(), &id, &dest, true),
            Path::new("ignored"),
            &mut out,
            &mut &[][..],
            false, // no terminal needed: --yes IS the confirmation
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/notes.txt")).unwrap(),
            "hello from the fixture",
            "the newer file is overwritten with the snapshot copy"
        );
        assert_eq!(std::fs::read_to_string(dest.join("proj/sub/deep.txt")).unwrap(), "deep content");
        let text = out_string(out);
        assert!(text.contains("2 existing file(s) will be overwritten, 1 of them newer"), "{text}");
    }

    /// D5: newer files at stake, no --yes, and no terminal to ask on -- refuse, naming
    /// the flag. Nothing is written at all.
    #[test]
    fn newer_files_without_confirmation_and_without_a_terminal_are_refused() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (id, dest) = overwrite_fixture(repo_tmp.path(), elsewhere.path());

        let err = run(
            &restore_proj_args(repo_tmp.path(), &id, &dest, false),
            Path::new("ignored"),
            &mut Vec::new(),
            &mut &[][..],
            false, // piped stdin: there is no one to ask
        )
        .unwrap_err();

        assert!(err.contains("--yes"), "the refusal names the way through: {err}");
        assert!(err.contains("NEWER"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/notes.txt")).unwrap(),
            "LOCAL WORK -- newer than the snapshot",
            "untouched"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/sub/deep.txt")).unwrap(),
            "stale local",
            "a refusal writes nothing at all -- not even the safe files"
        );
    }

    /// D5: answering "y" at the interactive prompt is the other explicit confirmation.
    #[test]
    fn answering_yes_at_the_prompt_overwrites() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (id, dest) = overwrite_fixture(repo_tmp.path(), elsewhere.path());

        let code = run(
            &restore_proj_args(repo_tmp.path(), &id, &dest, false),
            Path::new("ignored"),
            &mut Vec::new(),
            &mut &b"y\n"[..],
            true,
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/notes.txt")).unwrap(),
            "hello from the fixture"
        );
    }

    /// D5: when NOTHING at the destination is newer than the snapshot, overwriting needs
    /// no confirmation at all -- the printed counts are informational.
    #[test]
    fn overwriting_older_files_needs_no_confirmation() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let snap_mtime = std::fs::metadata(repo.snapshot_dir(&id).join("proj/notes.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let dest = elsewhere.path().join("live");
        touch(&dest.join("proj/notes.txt"), "stale local notes");
        std::fs::File::options()
            .write(true)
            .open(dest.join("proj/notes.txt"))
            .unwrap()
            .set_modified(snap_mtime - std::time::Duration::from_secs(3600))
            .unwrap();

        let mut out = Vec::new();
        let code = run(
            &restore_proj_args(repo_tmp.path(), &id, &dest, false),
            Path::new("ignored"),
            &mut out,
            &mut &[][..],
            false,
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(dest.join("proj/notes.txt")).unwrap(),
            "hello from the fixture"
        );
        let text = out_string(out);
        assert!(
            text.contains("1 existing file(s) will be overwritten, 0 of them newer"),
            "{text}"
        );
    }

    /// D5, single-file shape: the gate applies to a one-file restore too (the plan's
    /// file branch), and the destination is still folder-joined first (F23).
    #[test]
    fn a_single_file_restore_is_gated_the_same_way() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (repo, id) = fixture_repo(repo_tmp.path(), elsewhere.path());
        let snap_mtime = std::fs::metadata(repo.snapshot_dir(&id).join("proj/notes.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let dest = elsewhere.path().join("live");
        // Restoring proj/notes.txt to `live` targets live/notes.txt (compute_destination).
        touch(&dest.join("notes.txt"), "LOCAL WORK -- newer than the snapshot");
        std::fs::File::options()
            .write(true)
            .open(dest.join("notes.txt"))
            .unwrap()
            .set_modified(snap_mtime + std::time::Duration::from_secs(3600))
            .unwrap();

        let err = run(
            &Args {
                repo: Some(repo_tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(dest.clone()),
                    overwrite_all: false,
                },
            },
            Path::new("ignored"),
            &mut Vec::new(),
            &mut &[][..],
            false,
        )
        .unwrap_err();

        assert!(err.contains("--yes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dest.join("notes.txt")).unwrap(),
            "LOCAL WORK -- newer than the snapshot"
        );
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

        let err = run_cli(
            &Args {
                repo: Some(tmp.path().to_path_buf()),
                json: false,
                command: Command::Restore {
                    snapshot_id: id,
                    path: "proj/notes.txt".into(),
                    dest: RestoreDest::Path(bad_dest.clone()),
                    overwrite_all: false,
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(err.contains("overlaps"), "expected an overlap error, got: {err}");
        assert!(!bad_dest.exists(), "nothing should have been written");
    }

    #[test]
    fn opening_a_folder_that_is_not_a_repo_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_cli(
            &Args { repo: Some(tmp.path().to_path_buf()), json: false, command: Command::List },
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
        let code = run_cli(
            &Args {
                repo: Some(PathBuf::from("Z:\\does\\not\\exist")),
                json: false,
                command: Command::Help,
            },
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
