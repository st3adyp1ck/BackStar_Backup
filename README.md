<p align="center">
  <img src="backstar/src-tauri/icons/128x128.png" width="96" height="96" alt="BackStar" />
</p>

<h1 align="center">BackStar</h1>
<p align="center"><strong>A fast, versioned, trust-first backup tool for Windows.</strong></p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-Windows-0078D6">
  <img alt="Built with Tauri v2" src="https://img.shields.io/badge/built%20with-Tauri%20v2-24C8DB">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-DE7B00">
  <img alt="Frontend" src="https://img.shields.io/badge/frontend-React%20%2B%20TypeScript-3178C6">
</p>

---

BackStar backs up what you tell it to, the way it told you it would. Point it at a folder, and it keeps **versioned, hardlinked snapshots** — every run is a full point-in-time recovery point, but unchanged files cost no extra disk space. Point it at a folder you want mirrored instead, and it will show you exactly what would be added, updated, and deleted *before* it touches anything.

It's a native desktop app — a small Rust engine driving a React UI through Tauri — built to replace a PowerShell + WinForms tool of the same name after that tool quietly proved it could lie about what it had protected.

## Why it exists

The original BackStar worked, mostly. It also had four silent-correctness bugs discovered during a full audit: unicode filenames broke its ANSI-codepage log parser, its own history file already contained an unresolved 8.3 short path (`C:\Users\ST3ADY~1\...`) that defeated its destination-safety guard, its verification pass compared file *metadata* instead of content, and a save could be interrupted mid-write with no atomic fallback. None of those are edge cases in a tool whose entire job is "tell me the truth about whether my files are safe."

Rather than patch around a WinForms timer parsing `robocopy.exe`'s stdout, BackStar was rebuilt from the ground up: a real Rust copy engine with byte-level progress instead of regex-scraped percentages, path canonicalisation that resolves 8.3 names and symlinks before any safety check runs, and a golden-restore test suite that backs up a deliberately awkward file tree — unicode names, >260-character paths, alternate data streams, read-only/hidden attributes, a zero-byte file — and byte-compares the restored result against the original on every test run.

## Features

**Snapshots**
- Versioned, point-in-time backups. Every run creates a new, complete snapshot.
- Unchanged files are hardlinked, not recopied — N snapshots cost roughly one snapshot's worth of disk, on any filesystem that supports hardlinks.
- Verified, not assumed: every copied file is BLAKE3-hashed as it is written, then re-read and compared once the copy phase finishes. A mismatch counts as a failed file — a run never reports a clean success over a bad copy.
- Retention you control. Each job keeps its newest 30 snapshots by default and prunes its own older ones — only after a successful run, never on a failed or cancelled one, and never another job's snapshots at a shared destination. Set `keepSnapshots` to `null` in the config file to keep everything.
- Every snapshot records the original location of everything in it, in its own manifest inside the repository, so "restore to where it came from" needs no state from the app that took the backup.

**Live Sync (Mirror mode)**
- A real mirror — the destination ends up matching the source exactly, deletions included. But deletions never run first: they are staged into a `.backstar-trash-<run>` folder at the destination and purged only once the copies have succeeded (any leftover trash is cleaned at the start of the next run), so an interrupted sync can't leave the destination missing files it held before.
- Interactive runs are always previewed first: BackStar walks both trees and shows the real add/update/delete counts before you confirm — Cancel is the default and the focused control, so a stray Enter can never trigger a sync, from the Home screen or the Jobs screen alike. Files that appear at the destination between preview and run abort the run back to the preview instead of being deleted unreviewed.
- Refuses to mirror into a snapshot repository — mixing the two modes at one destination would delete backup history, not create a backup.

**Recovery that doesn't depend on BackStar**
- Every backup destination is self-describing: a `.backstar/` repository folder with plain, browsable snapshot directories underneath. Explorer alone can recover files with no software at all.
- Each run also refreshes a portable `BackStar-Restore.exe` inside the destination itself — a standalone, read-only CLI tool with no backup capability, compiled without write paths except an explicit restore. Your backups don't need the main app installed anywhere to come back from. It confirms before overwriting a destination file newer than the snapshot copy (`--yes` answers yes up front; without a terminal to ask on, it refuses rather than guesses), and its exit codes are a contract: `0` success, `1` failure or partial restore, `2` usage error.
- Destinations are identified by volume GUID, not drive letter — plugging your backup drive in as `E:` instead of `D:` doesn't strand it.

**System presets**
- One-click sources for Desktop, Documents, Pictures, Downloads, and App Settings (`%AppData%\Roaming`) — resolved through the real Windows *Known Folder* API, not a hardcoded path, so a relocated Downloads folder is found correctly instead of silently backing up an empty stub.

**Updates**
- Checks GitHub Releases for a newer version on startup, and on demand from the sidebar's "Check for updates". Downloads with progress shown, then closes and restarts into the new version — jobs, config, and history are untouched. Update packages are signed; the app verifies the signature against a public key baked in at build time before installing anything, and an offline or failed check never interrupts the app.

**Unattended and background-friendly**
- Once-a-day scheduling per job, wired straight into Windows Task Scheduler with `StartWhenAvailable` — a run missed because the machine was off or asleep catches up the next time it's awake. If Windows refuses a schedule change, the app says so instead of showing you a schedule that doesn't exist.
- Lives in the system tray. Minimizing tucks it away; closing the window mid-run hides it to the tray and lets the run finish, while closing it with nothing running exits. The tray's Exit item won't kill a run in progress — it shows the window (and its Cancel button) instead.
- Single-instance enforced at the OS level — a second launch focuses the existing window instead of racing it for the same destination — and a lock file inside each destination keeps a scheduled run and an interactive one from ever writing the same backup at once.

**Safety by construction, not by convention**
- Every copy lands via a temp file and an atomic rename — an interrupted process can leave a stray temp file, never a corrupted snapshot.
- Destination-safety guards canonicalise both source and destination (expanding 8.3 short names, environment variables, and symlinks) before any destructive check runs, and fail *closed* — an unresolvable path is refused, not allowed through.
- Exclusion rules match case-insensitively, the way NTFS does, and are rooted against both the source and destination side, closing a bug class where an exclude only worked from one direction.

## How it works

```
<destination>/
  .backstar/
    repo.json                     repo identity + schema version
    snapshots/<id>.json           one manifest per snapshot
  snapshots/2026-09-16T03-00-00Z/<job-name>/...   the files, as a plain tree
  BackStar-Restore.exe            refreshed on every run
```

A snapshot run walks the source tree, diffs it against this job's own previous snapshot by size and modification time — hash-comparing any file that matches only within the FAT two-second timestamp tolerance, because "indistinguishable" is not "unchanged" — then copies anything new or changed (on a bounded thread pool, BLAKE3-hashing every copied file as it goes) into a fresh timestamped folder and hardlinks everything unchanged into that same folder. After the copy phase, the copies are re-read and their hashes compared; a mismatch is counted as a failed file and the run's outcome says so. The result is a complete, independently browsable tree at every snapshot — not a base backup plus a chain of diffs you have to replay to get your files back.

## Project layout

```
backstar/
├── crates/backstar-core/   the engine — walking, diffing, copying, snapshots, guards,
│                           presets, restore. No UI code; fully unit-testable.
├── src-tauri/              BackStar.exe — the installed app (Tauri + React)
├── src-restore/            BackStar-Restore.exe — the portable recovery tool
└── ui/                     React + TypeScript + Tailwind frontend
```

Two binaries share one engine crate. `BackStar-Restore.exe` is compiled without any path that can create, mutate, or delete a backup — restore capability by construction, not by UI omission.

## Getting started

**Download:** grab the latest installer from [Releases](../../releases) — a single NSIS setup that installs `BackStar.exe` and `BackStar-Restore.exe` side by side, no admin rights required.

**Build from source:**

```powershell
git clone https://github.com/st3adyp1ck/BackStar_Backup.git
cd BackStar_Backup/backstar
pnpm install
pnpm tauri build
```

Prerequisites: Rust (stable, MSVC toolchain), Node.js + pnpm, and the WebView2 runtime (preinstalled on current Windows).

## Development

```powershell
cd backstar
pnpm install
pnpm tauri dev      # run the app with hot reload
cargo test --workspace   # run the engine + app test suite
cargo clippy --workspace --all-targets
```

## Releasing

Bump the version in three places — `backstar/Cargo.toml` (workspace), `backstar/src-tauri/tauri.conf.json`, and `backstar/package.json` — commit, tag `vX.Y.Z`, and push the tag. `.github/workflows/release.yml` then builds the NSIS installer on Windows, signs the updater package, and publishes the GitHub release along with the `latest.json` manifest that installed apps poll for updates.

The update-signing private key lives in the `TAURI_SIGNING_PRIVATE_KEY` repo secret (plus `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`), with a local copy at `~/.tauri/backstar.key`. It is never committed. Losing it means minting a new keypair and having users reinstall once, so keep a backup of it. The repository must stay public — installed apps fetch the update manifest without credentials.

## Quality

BackStar carries 348 Rust tests across the engine, the app shell, and the restore CLI, including:

- A **golden restore test** — backs up a fixture tree with unicode names, near-`MAX_PATH` paths, alternate data streams, read-only/hidden files, and a zero-byte file, then restores it and byte-compares every file against the original.
- A **hardlink safety test** — writes into a file inside snapshot *N* and asserts snapshot *N-1* is untouched, guarding the single most dangerous failure mode a hardlinked snapshot store can have.
- Regression tests for every documented bug in the original PowerShell implementation, so none of them can come back.

Every feature that touches the real OS — the system tray, single-instance enforcement, and Windows Task Scheduler integration — was verified against the actual OS behavior (real process counts, real window state, a real `schtasks.exe` round trip) rather than trusted on inspection alone.

## License

MIT — see [LICENSE](LICENSE).
