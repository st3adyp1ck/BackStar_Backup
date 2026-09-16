/** Mirrors the serde representations in `backstar-core`. */

export type JobKind = "snapshot" | "mirror";

export interface ExcludeRules {
  dir_names: string[];
  dir_paths: string[];
  file_patterns: string[];
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
  engine: "native" | "robocopy";
  keepSnapshots: number | null;
  importNotes: string[];
}

export interface VolumeCapability {
  root: string;
  filesystem: string;
  hardLinks: boolean;
  serial: number;
  guidPath: string | null;
}

export interface Preset {
  key: string;
  label: string;
  path: string | null;
  found: boolean;
}

export interface HistoryEntry {
  timestamp: string;
  job: string;
  snapshotId: string | null;
  durationMs: number;
  filesCopied: number;
  filesLinked: number;
  filesFailed: number;
  bytesCopied: number;
  dest: string;
  outcome: string;
}

/** Per-job protection state, computed in Rust so the UI never has to guess. */
export interface JobStatus {
  jobId: string;
  jobName: string;
  dest: string;
  /** Sources that do not currently exist. A job with these is not protecting what you think. */
  missingSources: string[];
  sourceCount: number;
  lastRun: HistoryEntry | null;
  snapshotCount: number;
  /** null when the destination volume could not be probed (unplugged drive, for example). */
  volume: VolumeCapability | null;
  /** Set when source and destination share a physical volume. */
  sameVolumeAsSource: boolean;
}

export type Protection = "protected" | "stale" | "at-risk" | "never-run" | "no-jobs";

export interface AppStatus {
  protection: Protection;
  headline: string;
  detail: string;
  jobs: JobStatus[];
  importNotes: string[];
  engine: string;
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
}

export interface BrowseEntry {
  name: string;
  isDir: boolean;
  size: number | null;
}

export type RestoreTarget =
  | { mode: "original" }
  | { mode: "custom"; folder: string };

export interface MirrorPreview {
  toAddOrUpdate: number;
  bytesToWrite: number;
  toDelete: number;
}
