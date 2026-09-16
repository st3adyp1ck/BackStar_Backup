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
- Never deletes. A snapshot destination only ever grows; nothing is pruned out from under you.
- Every snapshot records the original location of everything in it, so restoring "to where it came from" doesn't depend on a separate manifest surviving.

**Live Sync (Mirror mode)**
- A real, destructive mirror — the destination ends up matching the source exactly, deletions included.
- Always previewed first: BackStar walks both trees and shows the real add/update/delete counts before you confirm. Cancel is the default and the focused control, so a stray Enter can never trigger a sync.
- Refuses to mirror into a snapshot repository — mixing the two modes at one destination would delete backup history, not create a backup.

**Recovery that doesn't depend on BackStar**
- Every backup destination is self-describing: a `.backstar/` repository folder with plain, browsable snapshot directories underneath. Explorer alone can recover files with no software at all.
- Each run also refreshes a portable `BackStar-Restore.exe` inside the destination itself — a standalone, read-only CLI tool with no backup capability, compiled without write paths except an explicit restore. Your backups don't need the main app installed anywhere to come back from.
- Destinations are identified by volume GUID, not drive letter — plugging your backup drive in as `E:` instead of `D:` doesn't strand it.

**System presets**
- One-click sources for Desktop, Documents, Pictures, Downloads, and App Settings (`%AppData%\Roaming`) — resolved through the real Windows *Known Folder* API, not a hardcoded path, so a relocated Downloads folder is found correctly instead of silently backing up an empty stub.

**Unattended and background-friendly**
- Once-a-day scheduling per job, wired straight into Windows Task Scheduler.
- Lives in the system tray; minimizes and closes to tray while idle, and refuses to close mid-run.
- Single-instance enforced at the OS level — a second launch focuses the existing window instead of racing it for the same destination.

**Safety by construction, not by convention**
- Every copy lands via a temp file and an atomic rename — an interrupted process can leave a stray temp file, never a corrupted snapshot.
- Destination-safety guards canonicalise both source and destination (expanding 8.3 short names, environment variables, and symlinks) before any destructive check runs, and fail *closed* — an unresolvable path is refused, not allowed through.
- Exclusion rules are rooted against both the source and destination side, closing a bug class where an exclude only worked from one direction.

## How it works

```
<destination>/
  .backstar/
    repo.json                     repo identity + schema version
    snapshots/<id>.json           one manifest per snapshot
  snapshots/2026-09-16T03-00-00Z/<job-name>/...   the files, as a plain tree
  BackStar-Restore.exe            refreshed on every run
```

A snapshot run walks the source tree in parallel, diffs it against the previous snapshot's manifest by size and modification time, copies anything new or changed into a fresh timestamped folder, and hardlinks everything unchanged into that same folder. The result is a complete, independently browsable tree at every snapshot — not a base backup plus a chain of diffs you have to replay to get your files back.

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

## Quality

BackStar's engine carries 180+ Rust tests, including:

- A **golden restore test** — backs up a fixture tree with unicode names, near-`MAX_PATH` paths, alternate data streams, read-only/hidden files, and a zero-byte file, then restores it and byte-compares every file against the original.
- A **hardlink safety test** — writes into a file inside snapshot *N* and asserts snapshot *N-1* is untouched, guarding the single most dangerous failure mode a hardlinked snapshot store can have.
- Regression tests for every documented bug in the original PowerShell implementation, so none of them can come back.

Every feature that touches the real OS — the system tray, single-instance enforcement, and Windows Task Scheduler integration — was verified against the actual OS behavior (real process counts, real window state, a real `schtasks.exe` round trip) rather than trusted on inspection alone.

## License

MIT — see [LICENSE](LICENSE).
