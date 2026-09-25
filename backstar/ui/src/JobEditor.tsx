import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  DailySchedule,
  DefaultExcludes,
  ExcludeRules,
  Job,
  JobKind,
  Preset,
  SaveJobResponse,
} from "./types";

/**
 * The canonical exclude sets are fetched from the engine via `default_excludes` (L11) --
 * this file used to hardcode a copy of `backstar_core::exclude`'s constants, and the two
 * sides could drift: a stale UI copy would rebuild a job's rules from lists the engine no
 * longer applies, or misclassify a real profile job as custom. The engine is the single
 * source of truth; this component never names a single list entry itself.
 *
 * The sets stay two independent, mergeable toggles rather than the original app's
 * one-exclude-set-per-tab: a job made of both custom source folders and system presets
 * can reasonably want both sets applied at once, which a single Project-vs-System choice
 * could not express.
 */

/** `DailySchedule` <-> the string an `<input type="time">` wants, in both directions. */
function formatScheduleTime(schedule: DailySchedule | null): string {
  const s = schedule ?? { hour: 2, minute: 0 };
  return `${String(s.hour).padStart(2, "0")}:${String(s.minute).padStart(2, "0")}`;
}

function parseScheduleTime(time: string): DailySchedule | null {
  const [hour, minute] = time.split(":").map((n) => Number.parseInt(n, 10));
  // F57: validate before submit even though the picker constrains input -- a hand-typed
  // or programmatically-set value must not reach the backend.
  if (!Number.isFinite(hour) || !Number.isFinite(minute)) return null;
  if (hour < 0 || hour > 23 || minute < 0 || minute > 59) return null;
  return { hour, minute };
}

function buildExcludes(
  defaults: DefaultExcludes,
  skipBuild: boolean,
  skipCaches: boolean,
): ExcludeRules {
  return {
    dir_names: [
      ...(skipBuild ? defaults.project.dir_names : []),
      ...(skipCaches ? defaults.system.dir_names : []),
    ],
    dir_paths: skipBuild ? defaults.project.dir_paths : [],
    file_patterns: skipCaches ? defaults.system.file_patterns : [],
  };
}

/** Set-equality on string lists, so profile matching is order-insensitive. */
function sameStrings(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  const sa = [...a].sort();
  const sb = [...b].sort();
  return sa.every((v, i) => v === sb[i]);
}

/** Every on/off combination of the two toggles, for profile matching (F59). */
const PROFILE_COMBOS: [boolean, boolean][] = [
  [false, false],
  [true, false],
  [false, true],
  [true, true],
];

function matchesExcludeProfile(
  defaults: DefaultExcludes,
  excludes: ExcludeRules,
  skipBuild: boolean,
  skipCaches: boolean,
): boolean {
  const built = buildExcludes(defaults, skipBuild, skipCaches);
  return (
    sameStrings(excludes.dir_names, built.dir_names) &&
    sameStrings(excludes.dir_paths, built.dir_paths) &&
    sameStrings(excludes.file_patterns, built.file_patterns)
  );
}

/** F57: compare paths the way Windows sees them -- case-insensitive, slash-agnostic. */
function normalizePath(p: string): string {
  return p.trim().replace(/\//g, "\\").replace(/\\+$/, "").toLowerCase();
}

/** The final component of a path -- what becomes the subfolder name inside a backup. */
function leafName(p: string): string {
  const n = normalizePath(p);
  return n.split("\\").pop() ?? n;
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
  existingJobs,
  onSaved,
  onCancel,
}: {
  /** `null` means creating a new job. */
  initial: Job | null;
  /** Every configured job, for the duplicate-name check (F57). */
  existingJobs: { id: string; name: string }[];
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
  const [excludesTouched, setExcludesTouched] = useState(false);
  // Seeded from the fetched default sets once they arrive (see loadDefaultExcludes);
  // until then the toggles are not rendered at all.
  const [skipBuild, setSkipBuild] = useState(false);
  const [skipCaches, setSkipCaches] = useState(false);
  // The engine's canonical exclude sets (L11), fetched on mount. `null` while loading;
  // on failure the toggles section shows an error with a retry, and saving an EXISTING
  // job still works -- its stored rules round-trip unchanged (see `save`).
  const [defaultExcludes, setDefaultExcludes] = useState<DefaultExcludes | null>(null);
  const [excludesError, setExcludesError] = useState<string | null>(null);

  const [presets, setPresets] = useState<Preset[] | null>(null);
  const [presetsError, setPresetsError] = useState<string | null>(null);
  const [lockProneRunning, setLockProneRunning] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // F15 hand-off: the job saved but Task Scheduler said no. The user must be told plainly
  // and navigate back deliberately, not have the editor vanish as if all went well.
  const [savedWithScheduleError, setSavedWithScheduleError] = useState<{
    job: Job;
    scheduleError: string;
  } | null>(null);

  function loadPresets() {
    setPresetsError(null);
    invoke<Preset[]>("list_presets")
      .then((p) => {
        setPresets(p);
        setPresetsError(null);
      })
      // L8: a failed load must say so (with a way to retry), not leave "Loading…" forever.
      .catch((e) => setPresetsError(String(e)));
  }

  // F59: a job whose stored excludes match NO toggle profile (both on, both off, either
  // one alone) was customised outside this editor -- a hand edit, or a profile list that
  // has since changed. Rebuilding from the two toggles on save would silently DISCARD
  // those rules. The rule: custom excludes round-trip unchanged; excludes that match a
  // profile rebuild from the toggles as before; and the moment the user touches a
  // toggle, that explicit choice wins and the excludes rebuild from the toggles.
  // Computed rather than state: the canonical sets arrive asynchronously (L11), and a
  // useState initializer would have matched against a list that did not exist yet.
  const customExcludes = Boolean(
    initial &&
      defaultExcludes &&
      !PROFILE_COMBOS.some(([b, c]) => matchesExcludeProfile(defaultExcludes, initial.excludes, b, c)),
  );

  function loadDefaultExcludes() {
    setExcludesError(null);
    invoke<DefaultExcludes>("default_excludes")
      .then((d) => {
        setDefaultExcludes(d);
        setExcludesError(null);
        // Seed the toggles now that the real sets are known. An existing job matching a
        // profile gets exactly that profile's combination; for a custom job this is only
        // the toggles' starting point for IF the user decides to change them -- the
        // stored rules are preserved until then (F59), so the overlap heuristic ("shares
        // a dir name with the set") is enough.
        if (initial) {
          const combo = PROFILE_COMBOS.find(([b, c]) =>
            matchesExcludeProfile(d, initial.excludes, b, c),
          );
          if (combo) {
            setSkipBuild(combo[0]);
            setSkipCaches(combo[1]);
          } else {
            setSkipBuild(initial.excludes.dir_names.some((n) => d.project.dir_names.includes(n)));
            setSkipCaches(initial.excludes.dir_names.some((n) => d.system.dir_names.includes(n)));
          }
        }
      })
      .catch((e) => setExcludesError(String(e)));
  }

  useEffect(() => {
    loadPresets();
    loadDefaultExcludes();
  }, []);

  // F27: which lock-prone preset apps are running right now. Refetched when the selection
  // changes so ticking a browser preset while the browser is open warns immediately.
  // Failure of the command simply means no warning -- never a block on editing.
  const presetSelectionKey = Array.from(selectedPresets).sort().join(",");
  useEffect(() => {
    let cancelled = false;
    invoke<string[]>("list_lock_prone_running")
      .then((keys) => {
        if (!cancelled) setLockProneRunning(keys);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [presetSelectionKey]);

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

  // ---- F57: client-side validation, computed live so the reason save is blocked is
  // always on screen, not just a disabled button. The backend re-checks all of this
  // (F45); the point of doing it here is a friendlier message next to the field.
  const validationErrors: string[] = [];
  const trimmedName = name.trim();
  if (trimmedName && existingJobs.some(
    (j) => j.id !== initial?.id && j.name.trim().toLowerCase() === trimmedName.toLowerCase(),
  )) {
    validationErrors.push(`A job named "${trimmedName}" already exists.`);
  }
  const normDest = dest.trim() ? normalizePath(dest) : null;
  if (normDest) {
    for (const src of sources) {
      const s = normalizePath(src);
      if (normDest === s) {
        validationErrors.push(
          `The destination is the same folder as the source ${src} — a backup cannot be its own source.`,
        );
        break;
      }
      if (normDest.startsWith(s + "\\")) {
        validationErrors.push(
          `The destination is inside the source ${src} — the backup would try to contain itself.`,
        );
        break;
      }
      if (s.startsWith(normDest + "\\")) {
        validationErrors.push(
          `The source ${src} is inside the destination — the backup would try to contain itself.`,
        );
        break;
      }
    }
  }
  {
    const seen = new Map<string, string>();
    for (const src of sources) {
      const leaf = leafName(src);
      const first = seen.get(leaf);
      if (first !== undefined && normalizePath(first) !== normalizePath(src)) {
        validationErrors.push(
          `The sources "${first}" and "${src}" end in the same folder name ("${leaf}") — they would collide inside the backup. Rename one, or back them up in separate jobs.`,
        );
        break;
      }
      seen.set(leaf, src);
    }
  }
  const parsedSchedule = scheduled ? parseScheduleTime(scheduleTime) : null;
  if (scheduled && parsedSchedule === null) {
    validationErrors.push("The schedule must be a valid 24-hour time.");
  }
  // A NEW job cannot be saved without the canonical sets -- its excludes come from the
  // toggles, and inventing an empty set would silently back up build folders and browser
  // caches. An existing job always can: its stored rules round-trip unchanged (F59).
  if (defaultExcludes === null && initial === null) {
    validationErrors.push(
      excludesError
        ? "The exclusion rules could not be loaded — retry above, then save."
        : "Still loading the exclusion rules…",
    );
  }

  const canSave =
    trimmedName.length > 0 &&
    dest.trim().length > 0 &&
    (sources.length > 0 || selectedPresets.size > 0) &&
    validationErrors.length === 0;

  async function save() {
    if (!canSave) return;
    setSaving(true);
    setError(null);
    const job: Job = {
      id: initial?.id ?? "",
      name: trimmedName,
      sources,
      dest,
      // Round-tripped, never reconstructed: editing a job must not wipe out a portable
      // destination record a previous run already backfilled.
      destPortable: initial?.destPortable ?? null,
      kind,
      // Excludes round-trip unchanged when the stored rules are custom and untouched
      // (F59) -- or when the canonical sets failed to load, in which case rebuilding
      // from the toggles is impossible and passing the stored rules through unchanged
      // is the only honest option.
      excludes:
        initial && (defaultExcludes === null || (customExcludes && !excludesTouched))
          ? initial.excludes
          : buildExcludes(defaultExcludes!, skipBuild, skipCaches),
      presets: Array.from(selectedPresets),
      gitGc,
      enabled,
      schedule: scheduled ? parsedSchedule : null,
    };
    try {
      const resp = await invoke<SaveJobResponse>("save_job", { job });
      if (!resp.scheduleApplied && resp.scheduleError) {
        setSavedWithScheduleError({ job: resp.job, scheduleError: resp.scheduleError });
      } else {
        onSaved(resp.job);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  if (savedWithScheduleError) {
    return (
      <div
        className="mx-auto flex max-w-[640px] flex-col gap-4 rounded-[var(--radius-card)] border p-6"
        style={{
          background: "var(--c-surface)",
          borderColor: "color-mix(in srgb, var(--c-warn) 40%, var(--c-border))",
        }}
        role="alert"
      >
        <h1 className="text-[18px] font-semibold tracking-[-0.01em]" style={{ color: "var(--c-warn)" }}>
          Saved, but it will not run on schedule
        </h1>
        <p className="text-[13px]" style={{ color: "var(--c-text-dim)" }}>
          The job &ldquo;{savedWithScheduleError.job.name}&rdquo; was saved and can be run by
          hand, but Windows Task Scheduler rejected the scheduled task:
        </p>
        <p
          className="selectable rounded-md px-3 py-2 font-mono text-[12px]"
          style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
        >
          {savedWithScheduleError.scheduleError}
        </p>
        <p className="text-[13px]" style={{ color: "var(--c-text-dim)" }}>
          Edit the job and save it again to retry creating the task.
        </p>
        <div>
          <button
            onClick={() => onSaved(savedWithScheduleError.job)}
            className="rounded-md px-4 py-2 text-[13px] font-medium"
            style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
          >
            Back to jobs
          </button>
        </div>
      </div>
    );
  }

  const lockProneWarnings = lockProneRunning.filter((key) => selectedPresets.has(key));

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
            <input
              type="radio"
              name="job-kind"
              checked={kind === "snapshot"}
              onChange={() => setKind("snapshot")}
            />
            <span>
              Snapshot
              <span className="block text-[12px]" style={{ color: "var(--c-text-faint)" }}>
                Versioned, never deletes
              </span>
            </span>
          </label>
          <label className="flex cursor-pointer items-center gap-2">
            <input
              type="radio"
              name="job-kind"
              checked={kind === "mirror"}
              onChange={() => setKind("mirror")}
            />
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
        {presetsError ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-danger)" }} role="alert">
            Could not load presets: {presetsError}{" "}
            <button
              onClick={loadPresets}
              className="font-medium underline"
              style={{ color: "var(--c-accent)" }}
            >
              Retry
            </button>
          </p>
        ) : presets === null ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            Loading…
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-1.5">
            {presets.map((p) => {
              const selected = selectedPresets.has(p.key);
              return (
                <label
                  key={p.key}
                  className="flex items-center gap-2 text-[13px]"
                  style={{ color: p.found ? "var(--c-text)" : "var(--c-text-faint)" }}
                  title={p.found ? p.path ?? undefined : "Not found on this machine"}
                >
                  {/* F58: a vanished preset that IS selected must stay clickable -- it is
                      the only way to turn it off. Only unselected missing presets lock. */}
                  <input
                    type="checkbox"
                    disabled={!p.found && !selected}
                    checked={selected}
                    onChange={() => togglePreset(p.key)}
                  />
                  {p.label}
                  {!p.found && (
                    <span className="text-[11px]">
                      {selected ? " (folder not found — uncheck to remove)" : " (missing)"}
                    </span>
                  )}
                </label>
              );
            })}
          </div>
        )}
        {/* F27: browser profiles are SQLite databases held open while the app runs; backing
            one up live can capture a torn write that only shows up as a corrupt restore. */}
        {lockProneWarnings.map((key) => {
          const label = presets?.find((p) => p.key === key)?.label ?? key;
          return (
            <p key={key} className="text-[12.5px]" style={{ color: "var(--c-warn)" }} role="status">
              {label} is running — its database may be copied mid-write; close it first for a
              clean backup.
            </p>
          );
        })}
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
        {excludesError ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-danger)" }} role="alert">
            Could not load the exclusion rules: {excludesError}{" "}
            <button
              onClick={loadDefaultExcludes}
              className="font-medium underline"
              style={{ color: "var(--c-accent)" }}
            >
              Retry
            </button>
            {initial && (
              <span className="mt-1 block text-[12px]" style={{ color: "var(--c-text-faint)" }}>
                Saving keeps this job's stored rules unchanged.
              </span>
            )}
          </p>
        ) : defaultExcludes === null ? (
          <p className="text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            Loading exclusion rules…
          </p>
        ) : (
          <>
            <label className="flex cursor-pointer items-center gap-2">
              <input
                type="checkbox"
                checked={customExcludes && !excludesTouched ? false : skipBuild}
                ref={(el) => {
                  // Custom rules in effect: the toggles do not describe them, so show it.
                  if (el) el.indeterminate = customExcludes && !excludesTouched;
                }}
                onChange={(e) => {
                  setSkipBuild(e.target.checked);
                  setExcludesTouched(true);
                }}
              />
              {/* Examples come from the fetched set itself (L11), not a hardcoded list. */}
              Skip build/cache folders (
              {defaultExcludes.project.dir_names.slice(0, 4).join(", ")}, …)
            </label>
            <label className="flex cursor-pointer items-center gap-2">
              <input
                type="checkbox"
                checked={customExcludes && !excludesTouched ? false : skipCaches}
                ref={(el) => {
                  if (el) el.indeterminate = customExcludes && !excludesTouched;
                }}
                onChange={(e) => {
                  setSkipCaches(e.target.checked);
                  setExcludesTouched(true);
                }}
              />
              Skip browser caches and temp files (recommended with presets)
            </label>
            {customExcludes && !excludesTouched && (
              <p className="text-[12px]" style={{ color: "var(--c-warn)" }}>
                This job has custom exclusion rules that the toggles cannot represent — they are
                kept unchanged unless you change a toggle, which replaces them with the standard
                sets.
              </p>
            )}
          </>
        )}
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
          Runs via Windows Task Scheduler, even while BackStar is closed. A run missed because
          the computer was off or asleep catches up when it is next awake.
        </p>
      </div>

      {validationErrors.length > 0 && (
        <ul className="flex flex-col gap-1" role="alert">
          {validationErrors.map((v, i) => (
            <li key={i} className="text-[12.5px]" style={{ color: "var(--c-danger)" }}>
              {v}
            </li>
          ))}
        </ul>
      )}

      {error && (
        <p className="text-[13px]" style={{ color: "var(--c-danger)" }} role="alert">
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
