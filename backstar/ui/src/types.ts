/** Mirrors the serde representations in `backstar-core` and the `src-tauri` shell. */

export type JobKind = "snapshot" | "mirror";

/**
 * Mirrors `backstar_core::exclude::ExcludeRules`. This one struct is deliberately NOT
 * renamed by serde -- the fields cross the wire as snake_case, unlike everything else.
 */
export interface ExcludeRules {
  dir_names: string[];
  dir_paths: string[];
  file_patterns: string[];
}

/**
 * The `default_excludes` response (L11): the engine's canonical exclusion sets, grouped
 * exactly as the job editor's two toggles combine them. The UI never hardcodes these
 * lists -- a copied list could drift from the engine's and silently rebuild a job's
 * rules from sets the engine no longer applies.
 */
export interface DefaultExcludes {
  /** "Skip build/cache folders": project dir names and source-relative paths. */
  project: ExcludeRules;
  /** "Skip browser caches and temp files": system dir names and file globs. */
  system: ExcludeRules;
}

export interface PortableDest {
  volumeGuidPath: string;
  relative: string;
}

/** A once-a-day schedule, at a fixed local time. Mirrors `backstar_core::config::DailySchedule`. */
export interface DailySchedule {
  /** 0-23, local time. */
  hour: number;
  /** 0-59. */
  minute: number;
}

export interface Job {
  id: string;
  name: string;
  sources: string[];
  dest: string;
  /**
   * Opaque to the UI: never edited directly, only round-tripped. A job editor that
   * reconstructs a `Job` from scratch (rather than spreading the one `get_config` returned)
   * would silently wipe this out on every save, and with it the "survives a drive-letter
   * change" property -- the next backfill would have to start over from the literal `dest`.
   */
  destPortable: PortableDest | null;
  kind: JobKind;
  excludes: ExcludeRules;
  presets: string[];
  gitGc: boolean;
  enabled: boolean;
  /** When to run this job unattended, if at all. `null` means it only ever runs by hand. */
  schedule: DailySchedule | null;
}

export interface Config {
  schemaVersion: number;
  jobs: Job[];
  /** Keep-last-N retention per job; `null` keeps everything. */
  keepSnapshots: number | null;
}

export interface VolumeCapability {
  root: string;
  filesystem: string;
  hardLinks: boolean;
  serial: number;
  guidPath: string | null;
}

/**
 * Mirrors `backstar_core::volume::ProbeOutcome`: internally tagged by `kind`
 * (`{"kind":"local", ...}`). A network destination is not an error, and only
 * `unavailable` justifies a "drive unavailable" verdict.
 */
export type ProbeOutcome =
  | ({ kind: "local" } & VolumeCapability)
  | { kind: "network"; path: string }
  | { kind: "unavailable"; path: string; reason: string };

export interface Preset {
  key: string;
  label: string;
  path: string | null;
  found: boolean;
}

/** One completed (or attempted) run, from history.jsonl. */
export interface HistoryEntry {
  timestamp: string;
  job: string;
  /** Empty for entries written before history was keyed by job id -- those match by name. */
  jobId: string;
  /** `null` for mirror runs and cancelled runs. */
  snapshotId: string | null;
  durationMs: number;
  filesCopied: number;
  filesLinked: number;
  filesFailed: number;
  /** Mirror deletions; always 0 for snapshot runs. */
  filesDeleted: number;
  bytesCopied: number;
  dest: string;
  /** A display string written by the shell: "OK", "OK (N files failed)", "Failed: …", "Cancelled". */
  outcome: string;
}

/** Per-job protection state, computed in Rust so the UI never has to guess. */
export interface JobStatus {
  jobId: string;
  jobName: string;
  /** Snapshot or mirror -- Home must not label a mirror "Back up now" (F2). */
  kind: JobKind;
  /** Disabled jobs are listed for browsing but never enter the protection math. */
  enabled: boolean;
  dest: string;
  /** Sources that do not currently exist. A job with these is not protecting what you think. */
  missingSources: string[];
  sourceCount: number;
  /** The latest run that counts as protection (never a Cancelled or Failed entry). */
  lastRun: HistoryEntry | null;
  /** The latest attempt of any outcome: when newer than `lastRun`, the last run failed. */
  lastAttempt: HistoryEntry | null;
  /** Manifest-backed snapshots only. */
  snapshotCount: number;
  /** An interrupted run's manifest-less leftovers (pruned by the next run). */
  incompleteSnapshots: number;
  /** Snapshots whose manifest will not parse -- the tree may be intact. */
  unreadableSnapshots: number;
  volume: ProbeOutcome;
  /** Set when source and destination share a physical volume. */
  sameVolumeAsSource: boolean;
  /** A scheduled job whose last protection is older than the interval plus grace. */
  scheduledMissed: boolean;
}

export type Protection = "protected" | "stale" | "at-risk" | "never-run" | "no-jobs";

export interface AppStatus {
  protection: Protection;
  headline: string;
  detail: string;
  jobs: JobStatus[];
  version: string;
}

export interface SnapshotSource {
  name: string;
  original: string;
  files: number;
  bytes: number;
}

/**
 * Mirrors `backstar_core::events::RunOutcome`. Externally-tagged JSON: unit variants
 * serialise as bare strings ("ok", "cancelled"), struct variants as
 * `{ variantName: { fields } }`. This is also what `backstar-core::repo::SnapshotManifest`
 * stores on disk -- it was briefly a `Debug`-formatted string instead, which looked
 * plausible in a terminal but was not machine-parseable; see repo.rs's regression test.
 */
export type RunOutcome =
  | "ok"
  | { partial: { failed: number } }
  | { failed: { reason: string } }
  | "cancelled";

export interface SnapshotManifest {
  id: string;
  job: string;
  started: string;
  finished: string;
  sources: SnapshotSource[];
  filesCopied: number;
  filesLinked: number;
  filesFailed: number;
  bytesCopied: number;
  bytesLinked: number;
  durationMs: number;
  outcome: RunOutcome;
  parent: string | null;
  /** Configured sources skipped because they no longer existed at run time. */
  skippedSources: string[];
  /** blake3 hex digests of the files copied in the run, keyed by `<source>/<rel-path>`. */
  copiedHashes: Record<string, string>;
}

/**
 * The `list_snapshots` response: every snapshot directory on disk, sorted into honesty
 * buckets (F19). `manifests` are newest first; `unreadable` pairs a snapshot id with the
 * error its manifest failed with; `incomplete` are run leftovers with no manifest at all.
 */
export interface SnapshotListing {
  manifests: SnapshotManifest[];
  unreadable: [string, string][];
  incomplete: string[];
}

export interface BrowseEntry {
  name: string;
  isDir: boolean;
  size: number | null;
}

export type RestoreTarget =
  | { mode: "original" }
  | { mode: "custom"; folder: string };

/** What a restore may do to an existing destination file (`OverwriteArg` in restore_cmd.rs). */
export type OverwriteArg = "fail" | "ifOlder" | "always";

/** Overwrite facts for one file a restore would write (D5). */
export interface OverwriteInfo {
  rel: string;
  dest: string;
  exists: boolean;
  /** The existing destination file is newer than the snapshot copy -- someone's work. */
  destNewer: boolean;
  /**
   * The snapshot copy's modification time (RFC 3339) -- the version a restore would put
   * back. Copies preserve mtimes, so this is when the source file was last modified
   * before the run. `null` only if the snapshot's own mtime is unreadable.
   */
  srcMtime: string | null;
  /**
   * The existing destination file's modification time (RFC 3339), so the confirmation
   * can show HOW new the at-risk work is. `null` when no file exists at the destination
   * or its mtime is unreadable.
   */
  destMtime: string | null;
}

/** What a restore would write and overwrite, computed before anything is touched (D5). */
export interface RestorePlan {
  entries: OverwriteInfo[];
  wouldOverwrite: number;
  wouldOverwriteNewer: number;
}

/** One file a mirror preview plans to delete, tied to its source subfolder. */
export interface MirrorDelete {
  source: string;
  rel: string;
}

/**
 * A read-only summary of what a mirror run would do. Counts only become real by walking
 * both trees, which is exactly what `preview_mirror` does; the shell stashes the result
 * and refuses a mirror run that was not freshly previewed (F2/F13).
 */
export interface MirrorPreview {
  toAddOrUpdate: number;
  bytesToWrite: number;
  toDelete: number;
  deleteSet: MirrorDelete[];
}

/** The `save_job` response (F15): the persisted job plus what Task Scheduler did. */
export interface SaveJobResponse {
  job: Job;
  /** The Task Scheduler state now matches the job's `schedule` field. */
  scheduleApplied: boolean;
  /** Why not, when it isn't. `null` when applied. */
  scheduleError: string | null;
}

/** What `start_run` / `start_restore` return. */
export interface RunHandle {
  label: string;
  started: boolean;
}

/** Totals for one completed job (or a whole run). Mirrors `backstar_core::events::JobStats`. */
export interface JobStats {
  filesCopied: number;
  filesLinked: number;
  /** Restore-only: files deliberately not written because the destination copy was newer (D5). */
  filesSkipped: number;
  filesFailed: number;
  /** Mirror-only: files deleted (staged through the trash) this run. */
  filesDeleted: number;
  bytesCopied: number;
  bytesLinked: number;
  durationMs: number;
  /** Reparse points (junctions/symlinks) the walk refused to follow (D7). */
  reparseSkipped: number;
  /** Configured sources skipped because they no longer existed. */
  sourcesSkipped: number;
  /** Snapshot-only: old snapshots pruned by the retention policy. */
  snapshotsPruned: number;
}

/**
 * Mirrors `backstar_core::events::Event`: internally tagged by `type`, all fields
 * camelCase. The shell emits these on the `backstar://event` channel for both backup
 * runs and restores.
 */
export type BackstarEvent =
  | { type: "runStarted"; runId: string; jobs: number }
  | { type: "jobStarted"; job: string; source: string; dest: string }
  | { type: "scanProgress"; filesSeen: number; bytesSeen: number }
  | {
      type: "planReady";
      toCopy: number;
      toLink: number;
      bytesToCopy: number;
      /** `null` when the count could not be established -- never render it as "~0". */
      deletes: number | null;
    }
  | {
      type: "overallProgress";
      filesDone: number;
      filesTotal: number;
      bytesDone: number;
      bytesTotal: number;
      current: string | null;
    }
  | { type: "fileStarted"; path: string; bytes: number }
  | { type: "fileProgress"; path: string; done: number; total: number }
  | { type: "fileDone"; path: string; bytes: number; hash: string | null }
  | { type: "fileFailed"; path: string; message: string }
  /** A configured source was skipped because it no longer exists (a warning, not a failure). */
  | { type: "sourceSkipped"; source: string; reason: string }
  /** The walk itself found a hole: content the run never even attempted. */
  | { type: "sourceUnreadable"; path: string; message: string }
  /** A run-level warning belonging to no single file, written for display (F29/F36). */
  | { type: "runWarning"; message: string }
  | { type: "jobDone"; job: string; stats: JobStats }
  /** The post-copy verification pass finished (D1). Absent on cancelled runs. */
  | { type: "verifyDone"; examined: number; mismatches: string[] }
  | { type: "runDone"; runId: string; outcome: RunOutcome; stats: JobStats };
