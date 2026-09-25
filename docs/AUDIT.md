# BackStar full-codebase audit — findings register

Produced by a 7-pass deep review (engine, I/O paths, guards/tests, Tauri shell, restore CLI, UI, cross-layer contract). Mechanical state at audit time: clippy clean, 156 tests passing, `tsc --noEmit` clean. Status of each finding is tracked in the fix log at the bottom.

## Locked design decisions (from the user)

- **D1 Verification:** implement hash-on-copy (blake3 while copying, per-file hash in manifest) + post-run verify pass re-reading copied files; emit `VerifyDone`; mismatches count as failures. Hash-compare before hardlinking when mtime is inside the tolerance window.
- **D2 Dead knobs:** implement `git_gc` (best-effort `git gc --auto` on git sources pre-walk) and `keep_snapshots` (per-job pruning post-run; README "Never deletes" must be corrected); **remove** `Engine::Robocopy` everywhere (config, status footer, types).
- **D3 Mirror ordering:** staged trash — deletions move to `.backstar-trash-<id>` first, copies run, trash purged at end of run and at start of next run. Interruption can never leave the destination missing files it held.
- **D4 Legacy import:** remove `import_legacy`, the hardcoded path, and the `importNotes` banner entirely.
- **D5 Restore overwrite (agent-decided):** app probes destination and requires explicit confirmation when overwriting files newer than the snapshot copy; CLI prints overwrite count/newer count and requires `--yes` or interactive confirmation.
- **D6 Tray (agent-decided):** keep code behavior (exit when idle, hide-to-tray mid-run), correct the README claim; tray Exit must confirm/refuse while a run is active.
- **D7 File symlinks (agent-decided):** do not follow (safe default); surface `reparse_skipped` count + paths in stats/UI.
- **D8 Empty dirs (agent-decided):** preserve empty directories in snapshots and restore; mirror keeps empty dirs that exist in source, prunes only those absent from source (with reparse-point and exclusion fixes).

---

## P0 — Critical

- **F1** Walk anomaly counters are dropped by every caller (`walk.rs:40`; `engine.rs:235-241,288-297`; `mirror.rs:46,57-70`; `restore.rs:125`). Unreadable subfolder → hole in snapshot reported "Completed"; unreadable source root → empty snapshot reported "Completed" and becomes link parent; in mirror, unreadable source dir makes dest files look source-deleted → permanently deleted. Fix: `walk` returns unreadable paths (not just counts); engine folds into `files_failed`/forces at least `Partial` (root read failure = run failure); mirror fails the run or suppresses all deletions when `unreadable > 0`; restore surfaces the count.
- **F2** Home runs mirror jobs unpreviewed, labeled "Back up now" (`App.tsx:197-211,331-337`; `status.rs:20-30` has no `kind`; `lib.rs:306-336` requires no confirmation). Fix: `kind`+`enabled` on `JobStatus`, shared `MirrorConfirm` used by Home and Jobs, `start_run` requires `confirmMirror: true` for mirror jobs.
- **F3** No cross-process run exclusion: headless path bypasses single-instance (`main.rs:15-33`); `Runner` lock in-process only (`runner.rs:18-25`); `allocate_snapshot_id` check-then-create (`repo.rs:267-282`); `write_atomic` fixed temp name (`config.rs:224-239`). Fix: inter-process lock file under `.backstar/` held for a whole run in both GUI and headless paths; atomic id reservation via `create_dir` AlreadyExists retry; pid-unique temp names in `write_atomic`.
- **F4** Mirror case-only rename deletes without recopying (`mirror.rs:50-70`): unchanged-check is case-insensitive (NTFS) but `src_set` is a case-sensitive `HashSet<&Path>`; delete runs, copy was never queued, outcome `Ok`, file missing. Fix: key source set and dest comparison on normalized (lowercased) rel path.
- **F5** Missing sources silently skipped, run reports full success (`engine.rs:136-139,164-167`; only errors when ALL sources gone at :185). Fix: warning event per skipped source, `skippedSources` recorded in manifest, outcome at least `Partial` when any configured source/preset was skipped.

## P1 — High

### Engine & guards
- **F6** Hardlink parent chosen by folder leaf name only; repo queries are job-blind (`engine.rs:239-241`; `repo.rs:253-255,84-88`). Editing a job's source (same leaf) can link bytes that never existed in the new source; two jobs at one dest cross-link/cross-display history. Fix: read parent manifest; only link against `<parent>/<name>` when `sources[name].original` equals the current canonical source AND `parent.job == job.name`; otherwise copy all.
- **F7** No content verification anywhere (D1 implements it): `events.rs:76-77` ("copied and verified"), `VerifyDone`/`FileDone.hash` dead, `blake3` unused, "hash-on-copy" doc false (`config.rs:159`).
- **F8** ±2 s FAT mtime tolerance applied unconditionally (`engine.rs:89-103`; `mirror.rs:52`): same-size edit 1 s after last run compares unchanged even NTFS→NTFS. Fix: gate tolerance on a FAT-family volume being involved (`volume::probe` already knows); NTFS→NTFS requires exact mtime; tolerance-window matches get hash-compared per D1.
- **F9** Containment guard fails open for not-yet-created destinations behind junction/8.3/`\\?\` prefix (`guards.rs:144-150`; same hole in restore guard `restore.rs:83`). Fix: canonicalize longest existing prefix + re-append tail; treat unresolvable existing ancestor as overlap (fail closed).
- **F10** Repo guard checks only `dest/.backstar` (`mirror.rs:134-143`): mirror into a repo subfolder, or onto a folder containing a repo, purges history. Fix: ancestor walk for `.backstar`; hard-exclude any `.backstar` dir in the dest walk.
- **F11** `prune_empty_dirs` follows junctions (deletes outside root), ignores exclusions, junction-to-ancestor → stack overflow (`mirror.rs:79-100`). Fix: `symlink_metadata`, skip reparse points, thread `CompiledExcludes` through.
- **F12** Junction at mirror dest subfolder/root is followed into its target (`mirror.rs:58,63`; `walk.rs:52` only checks children). Fix: `symlink_metadata` on `dest_root` and on `root` in `walk`; refuse reparse roots.
- **F13** Mirror preview/execute drift: run re-walks and deletes whatever is absent then (`mirror.rs:117-216`). Fix: pass preview delete-set into the run; abort back to preview if the fresh delete-set grows beyond it.

### Shell
- **F14** Heavy sync commands on UI thread (`lib.rs:370-375` preview_mirror walks both trees; `lib.rs:63-67` get_status volume probes; `lib.rs:111-128` schtasks wait; `restore_cmd.rs` list_snapshots parses all manifests). Fix: `#[tauri::command(async)]` / `spawn_blocking`.
- **F15** Schedule failures invisible: `apply_schedule` error → warn only, UI shows schedule set (`lib.rs:111-115`); `DailySchedule` never validated (`25:61`); no reconciliation. Fix: validate hour/minute in `save_job`, return `scheduleApplied`+error in response, surface in UI.
- **F16** "Protected" computed from the single best job (`status.rs:175-213`): newest `last_run` wins; never-run requires ALL jobs never-run. Fix: per-job freshness; worst enabled job drives the headline.
- **F17** Failed runs never reach history (`lib.rs:255-302` only `Ok` path appends); cancelled run writes history with `snapshot_id` of a deleted dir and counts as protection (`engine.rs:322-323`; `status.rs:93-95`). Fix: append `Failed` entries on start-failure; `Cancelled` gets `snapshot_id: None`; status ignores Cancelled/Failed when picking `last_run`.
- **F18** Panic on run thread aborts process in release (`panic = "abort"`, workspace `Cargo.toml`) — mid-mirror death; debug wedges the slot forever (`runner.rs:96-106`). Fix: `catch_unwind` around `work`, clear slot + emit `RunDone::Failed` from panic path; drop `panic = "abort"`.
- **F19** Corrupt repo presented as "no backups yet" (`restore_cmd.rs:29-33`); corrupt manifests vanish silently (`repo.rs:294-296`); corrupt `config.json` not quarantined despite doc (`config.rs:11,200-213`). Fix: distinguish NotARepo from other errors; manifests() surfaces per-snapshot errors; config parse failure renames to `config.json.corrupt-<ts>` and continues from defaults with a loud note.
- **F20** UNC destination permanently misreported "Backup drive unavailable" (`status.rs:163-173`); `volume_root` assumes drive letter — NTFS folder-mount volumes get C:'s identity so the mounted-check passes with the drive removed and writes land on the system drive (`volume.rs:67-74,167-183`). Fix: `GetVolumePathNameW` for the true root; status distinguishes UNC (probe share reachability, no AtRisk verdict) from unresolvable letter.
- **F21** `snapshot_id`/`snapshot_rel` joined unchecked from IPC and CLI (`restore_cmd.rs:53,123`; `src-restore/lib.rs:222,248,269`; `repo.rs:185-187,199-227`). Fix: validate id shape `^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}Z(-\d{3})?$`; reject rel with absolute/parent/prefix components.
- **F22** Restore overwrites newer files unconditionally (`restore.rs:59-104`; app `restore_cmd.rs`; `SnapshotsView.tsx:199-233`). Fix per D5: probe dest mtimes, count newer collisions, require explicit overwrite confirmation.
- **F23** CLI dest semantics contradict help and app (`src-restore/lib.rs:47,250-258`; `restore.rs:101,137-147`): file→existing dir fails or creates file named `Recovered`; dir restore scatters contents. Fix: share `compute_destination` logic in core; file→dir joins file name; dir→dest creates named subfolder.
- **F24** CLI restore requires manifest even with explicit dest (`src-restore/lib.rs:247`). Fix: read manifest lazily only for `--original`.

### UI
- **F25** RunPanel calls `start()` without awaiting listener registration — fast-failing run's `RunDone` lost, panel wedges forever (`RunPanel.tsx:97-146`). Fix: `await listen()` (with disposed guard) before invoking start.
- **F26** Home status never refreshes after job CRUD (`App.tsx:240-242`). Fix: refresh when view becomes home + on window focus.
- **F27** Lock-prone browser presets unwarned (`presets.rs:108-126` machinery exists, mandates UI warning; `JobEditor.tsx:261-288` has none). Fix: Tauri command reporting which lock-prone apps are running; inline warning in JobEditor.
- **F28** Dead knobs (D2 resolves): `git_gc`, `keep_snapshots`, `Engine::Robocopy`.

## P2 — Medium

- **F29** exFAT/FAT32 dest: hardlink failure silently falls back to full copies every run — "versioned" backup loses versioning with no warning (`parallel.rs:118-124`; `volume.rs:50` degraded_reason only in a DTO). Fix: run-level warning event on first link failure, keyed off probe.
- **F30** Mid-loop `diff_source` error in `run_mirror` discards accumulated stats, no history (`mirror.rs:185`). Fix: emit RunDone with accumulated stats + record history on the error path.
- **F31** File symlinks silently never backed up (`walk.rs:86-91`). Fix per D7: surface count+paths in run stats/UI; do not follow.
- **F32** Exclusion globs case-sensitive — `*.TMP`, `THUMBS.DB` never match (`exclude.rs:113`; robocopy `/XF` is case-insensitive). Fix: `GlobBuilder::case_insensitive(true)`.
- **F33** Guard/write path mismatch: `%VAR%` dest created literally; source `is_dir` check runs before canonicalization (`engine.rs:137,150,207`; `mirror.rs:166`). Fix: canonicalize dest once at run start, use everywhere; canonicalize sources before the is_dir check.
- **F34** Per-file progress events all dead; `OverallProgress` only every 64 files → few-large-file jobs sit at "0%" looking hung (`parallel.rs:130,152`; `RunPanel.tsx:148-157`). Fix: wire throttled byte-level progress through the existing copy callback; bytes-based bar when filesTotal small.
- **F35** Crash-orphaned partial snapshots never cleaned, browsable as if complete, counted as real (`engine.rs:213,322-327`; `status.rs:54-60`). Fix: next run prunes manifest-less dirs older than the run start; listings distinguish incomplete; count_snapshots requires a manifest.
- **F36** `write_readme` failure fails a perfect run; `write_manifest` failure reports "Failed" for a complete restorable snapshot (`engine.rs:322-327`). Fix: readme → logged warning; manifest failure gets its own honest outcome text.
- **F37** Empty dirs preserved nowhere (walk records files only; mirror prunes them even when source has them). Fix per D8.
- **F38** Temp-name prefix pushes >234-char filenames over the 255 component limit (`copy.rs:83-88`). Fix: short capped temp name (`.bstmp-<pid>-<ctr>`).
- **F39** No sweep of crash-orphaned `.backstar-tmp-*` (`copy.rs:87,146-149`). Fix: sweep at run start.
- **F40** History keyed by mutable job name — rename orphans history, duplicate names share it (`status.rs:93-95`; `config.rs:242-256`). Fix: `job_id` on HistoryEntry (tolerate old lines), match on id.
- **F41** Missed scheduled runs never caught up — `StartWhenAvailable` not expressible via schtasks args (`schedule.rs:31-46`). Fix: create task via XML definition with StartWhenAvailable, and surface "last fire missed" in status.
- **F42** Tray Exit kills process mid-run, no confirmation (`tray.rs:47`). Fix per D6: confirm/refuse while Runner busy.
- **F43** Partially-failed scheduled run exits 0; headless has no logging (`lib.rs:343-363,389-394`). Fix: distinct exit code for Partial; file logging under state dir in headless.
- **F44** Status goes stale in tray-resident app; `backstar://run-finished` emitted but never listened (`App.tsx:235-242`; `lib.rs:332-334`). Fix: refresh on window focus/show; wire or delete the event.
- **F45** `save_job` validates nothing; job id interpolated unquoted into task command (`lib.rs:101-117`; `schedule.rs:23-25`). Fix: validate name/sources/dest/schedule/id-shape; quote the id.
- **F46** Legacy import dead in production (D4 removes it): `lib.rs:54-60`; `config.rs:365`; `App.tsx:311-319`.
- **F47** `count_snapshots` counts manifest-less partials and any folder named `snapshots` (`status.rs:54-60`). Fix: count manifest-backed only.
- **F48** Runner slot leaks if OS thread spawn fails (`runner.rs:88-107`).
- **F49** CLI `list` hides corrupt-manifest snapshots (`src-restore/lib.rs:214`). Fix: explicit "(manifest unreadable)" row; still browsable/restorable.
- **F50** CLI common mistakes give raw OS errors with internal paths (`src-restore/lib.rs:223,247`). Fix: friendly mapped messages with next-step hints.
- **F51** CLI conflicting (`dest` + `--original`) and surplus args silently ignored (`src-restore/lib.rs:104-131`). Fix: hard errors.
- **F52** CLI `--repo --json` eats the flag; `--repo=D:\x` rejected (`src-restore/lib.rs:83-93`). Fix: support `=` form; reject values starting with `--`.
- **F53** CLI no `--version`; exit codes undocumented.
- **F54** `filesDeleted` never reaches UI results or history (`RunPanel.tsx:31-39`; `config.rs:243-256`). Fix: TS field + done-panel render + HistoryEntry field.
- **F55** JobCard hides last-run outcome (40 failed files looks like a perfect run); snapshot framing applied to mirror jobs (`App.tsx:127-179`). Fix: render outcome with tint; branch framing on kind.
- **F56** JobsView shows no missing-source warning (`JobsView.tsx:92-168`). Fix: drive rows from `get_status`, surface `missingSources`, confirm before running such jobs.
- **F57** JobEditor validates almost nothing (`JobEditor.tsx:159`): dest ==/inside/containing source, duplicate leaf names, duplicate job names. Fix: client-side validation + `save_job` server-side (F45).
- **F58** Vanished-but-selected preset renders checked+disabled, can never be removed (`JobEditor.tsx:276-281`). Fix: `disabled={!p.found && !selectedPresets.has(p.key)}`.
- **F59** Saving a job rebuilds excludes from two toggles, discarding custom sets (`JobEditor.tsx:123-128,174`). Fix: round-trip unknown excludes unchanged; indeterminate toggle state.
- **F60** `resolve_restore_original` errors misreported as "not recorded" (`SnapshotsView.tsx:102-107`). Fix: render error distinctly from null.
- **F61** Activity/Settings nav dead; no run history anywhere in UI (`App.tsx:216-225,355-357`). Fix: implement Activity view from history.jsonl (incl. failures after F17); hide Settings until built; expose keepSnapshots there once D2 lands.
- **F62** Disabled job's snapshots unreachable (`status.rs:84`; `SnapshotsView.tsx:245,292`). Fix: include disabled jobs (marked) in the snapshot browser.
- **F63** Snapshots view "Loading…" forever with zero jobs (`SnapshotsView.tsx:245,304-308`). Fix: explicit empty state.
- **F64** Directory restores show blank panel during scan (`RunPanel.tsx:107-109`; `restore.rs:125-131`). Fix: scanning phase on scanProgress; "Starting…" pre-RunStarted.
- **F65** Mirror delete-before-copy (D3 staged trash resolves): `mirror.rs:200-234`.

## P3 — Low

- **L1** `JobDone` carries run-cumulative stats (`engine.rs:262-280`; `mirror.rs:243`); `RunStarted.jobs` misused on directory restores (`restore.rs:89,129`).
- **L2** Duplicated RFC-3339 formatting with different silent fallbacks (`engine.rs:302-307` vs `repo.rs:125-128`).
- **L3** `trim_trailing_sep` mangles `\\?\C:\` (`guards.rs:116-118`).
- **L4** `Error::Io2` dead variant strips path context (`error.rs:14-15`).
- **L5** `write_atomic` no directory fsync (`config.rs:234-238`) — document residual window.
- **L6** Golden test never asserts `outcome == Ok`; tree-assert compares files only, not dirs/attrs/timestamps (`tests/golden_restore.rs`).
- **L7** Elapsed clock/ETA freeze between events — add 1 s ticker (`RunPanel.tsx:150,168`).
- **L8** Stale error banners never cleared on success (`App.tsx:235-238`; `JobsView.tsx:181-187`; `SnapshotsView.tsx:259-275`; `JobEditor.tsx:134-138` presets failure leaves "Loading…").
- **L9** StrictMode dev double-start invokes start_run twice (`RunPanel.tsx:133-136`).
- **L10** `window.confirm` for job deletion — use registered dialog plugin `ask()` (`JobsView.tsx:190`).
- **L11** `relativeTime` duplicated verbatim (`App.tsx:105-116`; `SnapshotsView.tsx:55-66`); exclude constants duplicated UI/Rust with no parity check (`JobEditor.tsx:13-43`; `exclude.rs:34-71`).
- **L12** No React error boundary (`main.tsx`).
- **L13** A11y: radio groups missing `name` (`JobEditor.tsx:214,223`; `SnapshotsView.tsx:146,174`); progress bar no `role="progressbar"`/`aria-valuenow` (`RunPanel.tsx:205-213`); status changes not in `aria-live`.
- **L14** Dead light theme (`index.html:2`; `styles.css:51-72` unreachable).
- **L15** Plan line uses snapshot language during mirror/restore runs (`RunPanel.tsx:261-266`).
- **L16** Cancel gives no "Cancelling…" feedback (`RunPanel.tsx:268-276`).
- **L17** Restore loses your place after Done (SnapshotsView remounts).
- **L18** `useEffect(refresh, [])` passes a promise (`JobsView.tsx:187`).
- **L19** `remove_schedule` matches English schtasks stderr (`schedule.rs:69`) — match exit codes/query-first.
- **L20** Stale capability description comment (`capabilities/default.json:4`); README tray claim inverse of code (`tray.rs:79-90`) — fix per D6; copy.rs `catch_unwind` dead under panic=abort (resolved by F18).

## Missing test coverage

- **MT1** Empty-directory fidelity (after D8: preserved in snapshot+restore, kept by mirror).
- **MT2** Guard comparison where a non-existent destination has a junction/8.3 intermediate ancestor (F9).
- **MT3** Mirror destination nested inside a repo refused (F10).
- **MT4** `snapshot_id`/rel traversal rejected (F21).
- **MT5** Case-variant exclusions end-to-end (F32).
- **MT6** Locked-file → `Partial` outcome end-to-end.
- **MT7** Progress events during a golden run (after F34).
- **MT8** Unicode case-fold containment (`Äpfel` vs `äpfel`).
- **MT9** Unreadable source → Partial, and mirror refuses to delete (F1).
- **MT10** Hardlink parent source-mismatch falls back to copy (F6); cancelled run leaves no history-counted protection (F17).

## Fix log

| Wave | Findings | Status |
|---|---|---|
| A1a engine truthfulness | F1, F4, F5, F31, F32, F33 | done (2026-09-22) |
| A1b engine guards | F6, F8, F9, F10, F11, F12, F13 | done (2026-09-22) |
| A1c engine concurrency | F3 (engine side), F38, F39 | done (2026-09-22) |
| A2 engine robustness | F19 (engine side), F20, F29, F30, F35, F36, F37, L1–L6 | done (2026-09-22) |
| A3a verification + progress | F7/D1, F34 | done (2026-09-22) |
| A3b engine features | F28/D2, F65/D3, F22 (core) | done (2026-09-22) |
| B1 shell core | F2b, F3b, F13-shell, F14, F18, F19b, F21-shell, F22-app, F45, F46, F48 | done (2026-09-22) |
| B2 shell status/schedule/tray | F15, F16, F17, F20-shell, F40–F44, F47, F54 (history field), F62 (backend), L19, L20 | done (2026-09-22) |
| C restore CLI | F21 (CLI), F22 (CLI), F23, F24, F49–F53 | done (2026-09-22) |
| D1 UI core | F2 (UI), F25, F26, F44, F54 (UI), F55, F64, L7–L9, L12–L18 | done (2026-09-22) |
| D2 UI views | F27, F56–F63, F22 (UI), L10, L11 | done (2026-09-22) |
| E integration + docs | L11 (exclude-constant dedup), restore-dialog file dates, README, build/test verification, final report | done (2026-09-22) |
