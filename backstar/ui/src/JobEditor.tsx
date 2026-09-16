import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { DailySchedule, ExcludeRules, Job, JobKind, Preset } from "./types";

/**
 * Mirrors `backstar_core::exclude::{PROJECT_EXCLUDE_DIRS, PROJECT_EXCLUDE_PATHS,
 * SYSTEM_EXCLUDE_DIRS, SYSTEM_EXCLUDE_FILES}`. Kept as two independent, mergeable toggles
 * rather than the original app's one-exclude-set-per-tab: a job made of both custom source
 * folders and system presets can reasonably want both sets applied at once, which a single
 * Project-vs-System choice could not express.
 */
const PROJECT_EXCLUDE_DIRS = [
  "node_modules",
  "dist",
  "build",
  ".next",
  "__pycache__",
  ".venv",
  "venv",
  "target",
  ".gradle",
  ".dart_tool",
  ".expo",
];
const PROJECT_EXCLUDE_PATHS = [
  "apps\\mobile\\ios\\App\\App\\public",
  "apps\\mobile\\android\\app\\src\\main\\assets\\public",
];
const SYSTEM_EXCLUDE_DIRS = [
  "Cache",
  "Code Cache",
  "GPUCache",
  "cache2",
  "Service Worker",
  "CacheStorage",
  "blob_storage",
  "Crashpad",
  "IndexedDB",
  "$RECYCLE.BIN",
  "System Volume Information",
];
const SYSTEM_EXCLUDE_FILES = ["Thumbs.db", "desktop.ini", "*.tmp"];

/** `DailySchedule` <-> the string an `<input type="time">` wants, in both directions. */
function formatScheduleTime(schedule: DailySchedule | null): string {
  const s = schedule ?? { hour: 2, minute: 0 };
  return `${String(s.hour).padStart(2, "0")}:${String(s.minute).padStart(2, "0")}`;
}

function parseScheduleTime(time: string): DailySchedule {
  const [hour, minute] = time.split(":").map((n) => Number.parseInt(n, 10));
  return { hour: Number.isFinite(hour) ? hour : 2, minute: Number.isFinite(minute) ? minute : 0 };
}

function buildExcludes(skipBuild: boolean, skipCaches: boolean): ExcludeRules {
  return {
    dir_names: [...(skipBuild ? PROJECT_EXCLUDE_DIRS : []), ...(skipCaches ? SYSTEM_EXCLUDE_DIRS : [])],
    dir_paths: skipBuild ? PROJECT_EXCLUDE_PATHS : [],
    file_patterns: skipCaches ? SYSTEM_EXCLUDE_FILES : [],
  };
}

function fieldLabel(children: React.ReactNode) {
  return (
    <label
      className="text-[12px] font-semibold uppercase tracking-[0.08em]"
      style={{ color: "var(--c-text-faint)" }}
    >
      {children}
    </label>
  );
}

function TextButton({
  onClick,
  children,
  danger,
}: {
  onClick: () => void;
  children: React.ReactNode;
  danger?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      className="rounded-md px-2.5 py-1 text-[12px] font-medium"
      style={{
        background: "var(--c-surface-2)",
        color: danger ? "var(--c-danger)" : "var(--c-text)",
      }}
    >
      {children}
    </button>
  );
}

export function JobEditor({
  initial,
  onSaved,
  onCancel,
}: {
  /** `null` means creating a new job. */
  initial: Job | null;
  onSaved: (job: Job) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [kind, setKind] = useState<JobKind>(initial?.kind ?? "snapshot");
  const [sources, setSources] = useState<string[]>(initial?.sources ?? []);
  const [dest, setDest] = useState(initial?.dest ?? "");
  const [gitGc, setGitGc] = useState(initial?.gitGc ?? false);
  const [enabled, setEnabled] = useState(initial?.enabled ?? true);
  const [scheduled, setScheduled] = useState(initial?.schedule != null);
  const [scheduleTime, setScheduleTime] = useState(formatScheduleTime(initial?.schedule ?? null));
  const [selectedPresets, setSelectedPresets] = useState<Set<string>>(
    new Set(initial?.presets ?? []),
  );
  // Heuristic seed from an existing job's excludes: "was the project set applied" is well
  // approximated by "does it contain node_modules", since that is the one entry unique to
  // it. Good enough for round-tripping a job this editor itself created; a job with
  // hand-edited excludes just starts both toggles off, which is a safe, inspectable default.
  const [skipBuild, setSkipBuild] = useState(
    initial?.excludes.dir_names.includes("node_modules") ?? false,
  );
  const [skipCaches, setSkipCaches] = useState(
    initial?.excludes.dir_names.includes("Cache") ?? false,
  );

  const [presets, setPresets] = useState<Preset[] | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<Preset[]>("list_presets")
      .then(setPresets)
      .catch((e) => setError(String(e)));
  }, []);

  async function addSource() {
    const picked = await open({ directory: true, multiple: false, title: "Add source folder" });
    if (typeof picked === "string" && !sources.includes(picked)) {
      setSources([...sources, picked]);
    }
  }

  async function pickDest() {
    const picked = await open({ directory: true, multiple: false, title: "Choose destination" });
    if (typeof picked === "string") setDest(picked);
  }

  function togglePreset(key: string) {
    const next = new Set(selectedPresets);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    setSelectedPresets(next);
  }

  const canSave = name.trim().length > 0 && dest.trim().length > 0 && (sources.length > 0 || selectedPresets.size > 0);

  async function save() {
    if (!canSave) return;
    setSaving(true);
    setError(null);
    const job: Job = {
      id: initial?.id ?? "",
      name: name.trim(),
      sources,
      dest,
      // Round-tripped, never reconstructed: editing a job must not wipe out a portable
      // destination record a previous run already backfilled.
      destPortable: initial?.destPortable ?? null,
      kind,
      excludes: buildExcludes(skipBuild, skipCaches),
      presets: Array.from(selectedPresets),
      gitGc,
      enabled,
      schedule: scheduled ? parseScheduleTime(scheduleTime) : null,
    };
    try {
      const saved = await invoke<Job>("save_job", { job });
      onSaved(saved);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      className="mx-auto flex max-w-[640px] flex-col gap-5 rounded-[var(--radius-card)] border p-6"
      style={{ background: "var(--c-surface)" }}
    >
      <h1 className="text-[18px] font-semibold tracking-[-0.01em]">
        {initial ? "Edit job" : "New job"}
      </h1>

      <div className="flex flex-col gap-1.5">
        {fieldLabel("Name")}
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Projects"
          className="rounded-md border px-2.5 py-1.5 text-[13px]"
          style={{ background: "var(--c-surface-2)", borderColor: "var(--c-border)" }}
        />
      </div>

      <div className="flex flex-col gap-1.5">
        {fieldLabel("Type")}
        <div className="flex gap-4 text-[13px]">
          <label className="flex cursor-pointer items-center gap-2">
            <input type="radio" checked={kind === "snapshot"} onChange={() => setKind("snapshot")} />
            <span>
              Snapshot
              <span className="block text-[12px]" style={{ color: "var(--c-text-faint)" }}>
                Versioned, never deletes
              </span>
            </span>
          </label>
          <label className="flex cursor-pointer items-center gap-2">
            <input type="radio" checked={kind === "mirror"} onChange={() => setKind("mirror")} />
            <span>
              Mirror
              <span className="block text-[12px]" style={{ color: "var(--c-danger)" }}>
                Live sync — deletes at destination
              </span>
            </span>
          </label>
        </div>
      </div>

      <div className="flex flex-col gap-1.5">
        <div className="flex items-center justify-between">
          {fieldLabel("Source folders")}
          <TextButton onClick={addSource}>+ Add folder</TextButton>
        </div>
        {sources.length === 0 ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            No custom folders — add one, or pick a system preset below.
          </p>
        ) : (
          <ul className="flex flex-col gap-1">
            {sources.map((s) => (
              <li
                key={s}
                className="flex items-center justify-between gap-2 rounded-md px-2.5 py-1.5"
                style={{ background: "var(--c-surface-2)" }}
              >
                <span className="selectable truncate font-mono text-[12px]">{s}</span>
                <TextButton danger onClick={() => setSources(sources.filter((x) => x !== s))}>
                  Remove
                </TextButton>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        {fieldLabel("System presets")}
        {presets === null ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            Loading…
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-1.5">
            {presets.map((p) => (
              <label
                key={p.key}
                className="flex items-center gap-2 text-[13px]"
                style={{ color: p.found ? "var(--c-text)" : "var(--c-text-faint)" }}
                title={p.found ? p.path ?? undefined : "Not found on this machine"}
              >
                <input
                  type="checkbox"
                  disabled={!p.found}
                  checked={selectedPresets.has(p.key)}
                  onChange={() => togglePreset(p.key)}
                />
                {p.label}
                {!p.found && <span className="text-[11px]"> (missing)</span>}
              </label>
            ))}
          </div>
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        <div className="flex items-center justify-between">
          {fieldLabel("Destination")}
          <TextButton onClick={pickDest}>Choose…</TextButton>
        </div>
        <p className="selectable font-mono text-[12px]" style={{ color: dest ? "var(--c-text)" : "var(--c-text-faint)" }}>
          {dest || "Not set"}
        </p>
      </div>

      <div className="flex flex-col gap-2 text-[13px]">
        <label className="flex cursor-pointer items-center gap-2">
          <input type="checkbox" checked={skipBuild} onChange={(e) => setSkipBuild(e.target.checked)} />
          Skip build/cache folders (node_modules, dist, .venv, target, …)
        </label>
        <label className="flex cursor-pointer items-center gap-2">
          <input type="checkbox" checked={skipCaches} onChange={(e) => setSkipCaches(e.target.checked)} />
          Skip browser caches and temp files (recommended with presets)
        </label>
        {kind === "snapshot" && (
          <label className="flex cursor-pointer items-center gap-2">
            <input type="checkbox" checked={gitGc} onChange={(e) => setGitGc(e.target.checked)} />
            Run <code>git gc --auto</code> on git repositories before backing them up
          </label>
        )}
        <label className="flex cursor-pointer items-center gap-2">
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          Enabled
        </label>
      </div>

      <div className="flex flex-col gap-1.5">
        {fieldLabel("Schedule")}
        <label className="flex cursor-pointer items-center gap-2 text-[13px]">
          <input
            type="checkbox"
            checked={scheduled}
            onChange={(e) => setScheduled(e.target.checked)}
          />
          Run automatically every day at
          <input
            type="time"
            value={scheduleTime}
            disabled={!scheduled}
            onChange={(e) => setScheduleTime(e.target.value)}
            className="rounded-md border px-2 py-1 text-[13px] disabled:opacity-40"
            style={{ background: "var(--c-surface-2)", borderColor: "var(--c-border)" }}
          />
        </label>
        <p className="text-[12px]" style={{ color: "var(--c-text-faint)" }}>
          Runs via Windows Task Scheduler, even while BackStar is closed. The computer must be on
          and awake at that time.
        </p>
      </div>

      {error && (
        <p className="text-[13px]" style={{ color: "var(--c-danger)" }}>
          {error}
        </p>
      )}

      <div className="flex gap-2">
        <button
          onClick={onCancel}
          className="rounded-md px-4 py-2 text-[13px] font-medium"
          style={{ background: "var(--c-surface-2)", color: "var(--c-text-dim)" }}
        >
          Cancel
        </button>
        <button
          onClick={save}
          disabled={!canSave || saving}
          className="rounded-md px-4 py-2 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </div>
    </div>
  );
}
