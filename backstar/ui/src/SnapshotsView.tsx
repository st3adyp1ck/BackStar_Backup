import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  BrowseEntry,
  JobStatus,
  OverwriteArg,
  RestorePlan,
  RestoreTarget,
  SnapshotListing,
} from "./types";
import { human, outcomeLabel } from "./RunPanel";
import { relativeTime } from "./App";

function FolderIcon({
  className = "",
  style,
}: {
  className?: string;
  style?: React.CSSProperties;
}) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      style={style}
      aria-hidden="true"
    >
      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" />
    </svg>
  );
}

function FileIcon({
  className = "",
  style,
}: {
  className?: string;
  style?: React.CSSProperties;
}) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      style={style}
      aria-hidden="true"
    >
      <path d="M14 3v5a1 1 0 0 0 1 1h5M6 21h12a1 1 0 0 0 1-1V8l-6-6H6a1 1 0 0 0-1 1v17a1 1 0 0 0 1 1Z" />
    </svg>
  );
}

/** RFC 3339 → a compact local date-time for the overwrite dialog; anything unparseable is shown verbatim. */
function fmtMtime(iso: string | null): string {
  if (iso === null) return "date unknown";
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? iso
    : d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/** The inline panel for restoring one item, opened from a row in the browser. */
function RestorePanel({
  jobId,
  snapshotId,
  rel,
  name,
  onCancel,
  onStart,
}: {
  jobId: string;
  snapshotId: string;
  rel: string;
  name: string;
  onCancel: () => void;
  onStart: (title: string, start: () => Promise<unknown>) => void;
}) {
  const [original, setOriginal] = useState<string | null | undefined>(undefined);
  // F60: a failed LOOKUP is not the same as "no original recorded" -- keep the error.
  const [originalError, setOriginalError] = useState<string | null>(null);
  const [customFolder, setCustomFolder] = useState<string | null>(null);
  const [mode, setMode] = useState<"original" | "custom">("original");
  const [error, setError] = useState<string | null>(null);
  // F22/D5: overwrite facts computed before anything is written. `null` means either
  // "not checked yet" or "checked, nothing would be overwritten" -- those are the same
  // to the user, since a zero-overwrite plan starts the restore without a dialog.
  const [plan, setPlan] = useState<RestorePlan | null>(null);
  const [planning, setPlanning] = useState(false);

  useEffect(() => {
    let cancelled = false;
    invoke<string | null>("resolve_restore_original", {
      jobId,
      snapshotId,
      snapshotRel: rel,
    })
      .then((p) => {
        if (!cancelled) {
          setOriginal(p);
          if (p === null) setMode("custom");
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setOriginalError(String(e));
          setOriginal(null);
          setMode("custom");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [jobId, snapshotId, rel]);

  async function pickFolder() {
    try {
      const picked = await open({ directory: true, multiple: false, title: "Restore to folder" });
      if (typeof picked === "string") {
        setCustomFolder(picked);
        setMode("custom");
      }
    } catch (e) {
      setError(String(e));
    }
  }

  const target: RestoreTarget | null =
    mode === "original"
      ? original
        ? { mode: "original" }
        : null
      : customFolder
        ? { mode: "custom", folder: customFolder }
        : null;

  function startRestore(t: RestoreTarget, overwrite?: OverwriteArg) {
    onStart(`Restore: ${name}`, () =>
      invoke("start_restore", { jobId, snapshotId, snapshotRel: rel, target: t, overwrite }),
    );
  }

  /** F22: never write over a destination file without the user seeing the count first. */
  async function beginRestore() {
    if (!target) return;
    setPlanning(true);
    setError(null);
    try {
      const p = await invoke<RestorePlan>("plan_restore_overwrites", {
        jobId,
        snapshotId,
        snapshotRel: rel,
        target,
      });
      if (p.wouldOverwrite === 0) {
        // Nothing at stake: the default "fail on any overwrite" policy is fine.
        startRestore(target);
      } else {
        setPlan(p);
      }
    } catch (e) {
      // A plan that cannot be computed (an unreadable snapshot subtree) must not degrade
      // into "just restore anyway" -- an understated confirmation is worse than none.
      setError(`Could not check what would be overwritten: ${String(e)}`);
    } finally {
      setPlanning(false);
    }
  }

  if (plan) {
    const newer = plan.entries.filter((e) => e.destNewer);
    return (
      <div
        className="rounded-[var(--radius-card)] border p-5"
        style={{
          background:
            plan.wouldOverwriteNewer > 0
              ? "color-mix(in srgb, var(--c-danger) 7%, var(--c-surface))"
              : "var(--c-surface-2)",
          borderColor:
            plan.wouldOverwriteNewer > 0
              ? "color-mix(in srgb, var(--c-danger) 35%, var(--c-border))"
              : "var(--c-border-strong)",
        }}
      >
        <h3 className="text-[14px] font-semibold">
          Restore &ldquo;{name}&rdquo; — confirm overwrite
        </h3>
        <p className="mt-1.5 text-[13px]" style={{ color: "var(--c-text-dim)" }}>
          {plan.wouldOverwrite.toLocaleString()} existing file
          {plan.wouldOverwrite === 1 ? "" : "s"} at the destination will be overwritten.
        </p>
        {plan.wouldOverwriteNewer > 0 && (
          <>
            <p className="mt-2 text-[13px] font-medium" style={{ color: "var(--c-danger)" }}>
              {plan.wouldOverwriteNewer.toLocaleString()} of{" "}
              {plan.wouldOverwriteNewer === 1 ? "them is" : "them are"} NEWER than the backup
              — overwriting destroys work that was never backed up:
            </p>
            <ul className="mt-2 space-y-2">
              {newer.slice(0, 10).map((e) => (
                <li key={e.rel} className="selectable" title={e.dest}>
                  <span className="block truncate font-mono text-[12px]">{e.dest}</span>
                  {/* Both dates, so "newer" is something the user can SEE, not trust:
                      the live file's mtime against the backed-up copy's. */}
                  <span className="block text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
                    yours {fmtMtime(e.destMtime)} · backup {fmtMtime(e.srcMtime)}
                  </span>
                </li>
              ))}
              {newer.length > 10 && (
                <li className="text-[12px]" style={{ color: "var(--c-text-faint)" }}>
                  …and {newer.length - 10} more
                </li>
              )}
            </ul>
          </>
        )}

        <div className="mt-4 flex flex-wrap gap-2">
          {/* Destructive-choice discipline: Cancel is secondary AND default-focused. */}
          <button
            onClick={onCancel}
            autoFocus
            className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
            style={{ background: "var(--c-surface)", color: "var(--c-text)" }}
          >
            Cancel
          </button>
          {plan.wouldOverwriteNewer > 0 && target && (
            <button
              onClick={() => startRestore(target, "ifOlder")}
              className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
              style={{ background: "var(--c-surface)", color: "var(--c-text)" }}
            >
              Keep newer files, overwrite the rest
            </button>
          )}
          {target && (
            <button
              onClick={() => startRestore(target, "always")}
              className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
              style={
                plan.wouldOverwriteNewer > 0
                  ? { background: "var(--c-danger)", color: "var(--c-on-danger)" }
                  : { background: "var(--c-accent)", color: "var(--c-on-accent)" }
              }
            >
              {plan.wouldOverwriteNewer > 0
                ? "Overwrite everything"
                : `Overwrite ${plan.wouldOverwrite.toLocaleString()} file${plan.wouldOverwrite === 1 ? "" : "s"}`}
            </button>
          )}
        </div>
      </div>
    );
  }

  return (
    <div
      className="rounded-[var(--radius-card)] border p-5"
      style={{ background: "var(--c-surface-2)", borderColor: "var(--c-border-strong)" }}
    >
      <h3 className="text-[14px] font-semibold">Restore &ldquo;{name}&rdquo;</h3>

      <div className="mt-3 flex flex-col gap-2">
        <label className="flex cursor-pointer items-start gap-2.5 text-[13px]">
          <input
            type="radio"
            name="restore-mode"
            className="mt-0.5"
            checked={mode === "original"}
            disabled={original === undefined || (original === null && originalError === null) || originalError !== null}
            onChange={() => setMode("original")}
          />
          <span>
            To its original location
            {original === undefined && (
              <span style={{ color: "var(--c-text-faint)" }}> — checking…</span>
            )}
            {originalError !== null && (
              <span style={{ color: "var(--c-warn)" }}>
                {" "}
                — couldn&rsquo;t check the original location: {originalError}
              </span>
            )}
            {original === null && originalError === null && (
              <span style={{ color: "var(--c-text-faint)" }}>
                {" "}
                — not recorded for this item; choose a folder instead
              </span>
            )}
            {original && (
              <span className="selectable block font-mono text-[12px]" style={{ color: "var(--c-text-dim)" }}>
                {original}
              </span>
            )}
          </span>
        </label>

        <label className="flex cursor-pointer items-start gap-2.5 text-[13px]">
          <input
            type="radio"
            name="restore-mode"
            className="mt-0.5"
            checked={mode === "custom"}
            onChange={() => setMode("custom")}
          />
          <span className="flex-1">
            To a folder you choose
            {customFolder && (
              <span className="selectable block font-mono text-[12px]" style={{ color: "var(--c-text-dim)" }}>
                {customFolder}\{name}
              </span>
            )}
          </span>
        </label>
        {mode === "custom" && (
          <button
            onClick={pickFolder}
            className="ml-6 self-start rounded-md px-3 py-1.5 text-[12.5px] font-medium"
            style={{ background: "var(--c-surface)", color: "var(--c-text)" }}
          >
            {customFolder ? "Choose a different folder…" : "Choose folder…"}
          </button>
        )}
      </div>

      {error && (
        <p className="mt-2 text-[12.5px]" style={{ color: "var(--c-danger)" }} role="alert">
          {error}
        </p>
      )}

      <div className="mt-4 flex gap-2">
        <button
          onClick={onCancel}
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
          style={{ background: "var(--c-surface)", color: "var(--c-text-dim)" }}
        >
          Cancel
        </button>
        <button
          disabled={!target || planning}
          onClick={beginRestore}
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
        >
          {planning ? "Checking…" : "Restore…"}
        </button>
      </div>
    </div>
  );
}

export function SnapshotsView({
  jobs,
  onRestoreStarted,
}: {
  jobs: JobStatus[];
  onRestoreStarted: (title: string, start: () => Promise<unknown>) => void;
}) {
  const [jobId, setJobId] = useState<string | null>(jobs[0]?.jobId ?? null);
  const [listing, setListing] = useState<SnapshotListing | null>(null);
  const [snapshotId, setSnapshotId] = useState<string | null>(null);
  const [path, setPath] = useState<string[]>([]);
  const [entries, setEntries] = useState<BrowseEntry[] | null>(null);
  const [restoring, setRestoring] = useState<{ rel: string; name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!jobId) return;
    setError(null);
    setListing(null);
    setSnapshotId(null);
    setPath([]);
    setEntries(null);
    invoke<SnapshotListing>("list_snapshots", { jobId })
      .then((l) => {
        setListing(l);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, [jobId]);

  useEffect(() => {
    if (!jobId || !snapshotId) return;
    setError(null);
    setEntries(null);
    setRestoring(null);
    invoke<BrowseEntry[]>("browse_snapshot", {
      jobId,
      snapshotId,
      snapshotRel: path.join("/"),
    })
      .then((e) => {
        setEntries(e);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, [jobId, snapshotId, path]);

  // F63: zero jobs is an explicit state, not an eternal "Loading…".
  if (jobs.length === 0) {
    return (
      <div
        className="mx-auto max-w-[640px] rounded-[var(--radius-card)] border p-6 text-center"
        style={{ background: "var(--c-surface)", color: "var(--c-text-faint)" }}
      >
        No backup jobs yet — create one in Jobs first. Snapshots appear here once a job has
        run.
      </div>
    );
  }

  const activeJob = jobs.find((j) => j.jobId === jobId);

  function selectSnapshot(id: string) {
    setSnapshotId(id);
    setPath([]);
  }

  const snapshotListEmpty =
    listing != null &&
    listing.manifests.length === 0 &&
    listing.unreadable.length === 0 &&
    listing.incomplete.length === 0;

  return (
    <div className="mx-auto grid max-w-[1080px] grid-cols-[280px_1fr] gap-5">
      <aside className="flex flex-col gap-4">
        <div>
          <label className="text-[12px] font-semibold uppercase tracking-[0.08em]" style={{ color: "var(--c-text-faint)" }}>
            Job
          </label>
          <select
            value={jobId ?? ""}
            onChange={(e) => setJobId(e.target.value)}
            className="mt-1.5 w-full rounded-md border px-2.5 py-1.5 text-[13px]"
            style={{ background: "var(--c-surface)", borderColor: "var(--c-border)" }}
          >
            {/* F62: disabled jobs are listed (marked) -- their backups stay reachable. */}
            {jobs.map((j) => (
              <option key={j.jobId} value={j.jobId}>
                {j.jobName}
                {!j.enabled ? " (disabled)" : ""}
              </option>
            ))}
          </select>
        </div>

        <div
          className="flex-1 overflow-y-auto rounded-[var(--radius-card)] border"
          style={{ background: "var(--c-surface)", maxHeight: "calc(100vh - 220px)" }}
        >
          {listing === null && (
            <p className="p-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
              Loading…
            </p>
          )}
          {snapshotListEmpty && (
            <p className="p-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
              {activeJob?.jobName ?? "This job"} has no snapshots yet.
            </p>
          )}

          {listing?.manifests.map((s) => {
            const active = s.id === snapshotId;
            const o = outcomeLabel(s.outcome);
            return (
              <button
                key={s.id}
                onClick={() => selectSnapshot(s.id)}
                className="block w-full border-b px-3.5 py-3 text-left last:border-b-0"
                style={{
                  borderColor: "var(--c-border)",
                  background: active ? "var(--c-surface-2)" : "transparent",
                }}
              >
                <div className="flex items-center justify-between">
                  <span className="text-[13px] font-medium">{relativeTime(s.finished)}</span>
                  <span className="text-[11px] font-semibold" style={{ color: o.tint }}>
                    {o.text.split(" ")[0]}
                  </span>
                </div>
                <div className="mt-0.5 text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
                  {s.filesCopied + s.filesLinked} files · {human(s.bytesCopied + s.bytesLinked)}
                </div>
              </button>
            );
          })}

          {/* F19: a snapshot whose MANIFEST is unreadable is not nothing -- the tree on
              disk may be fully intact, so it stays browsable, labelled honestly. */}
          {listing?.unreadable.map(([id, err]) => (
            <button
              key={id}
              onClick={() => selectSnapshot(id)}
              className="block w-full border-b px-3.5 py-3 text-left last:border-b-0"
              style={{
                borderColor: "var(--c-border)",
                background: id === snapshotId ? "var(--c-surface-2)" : "transparent",
              }}
            >
              <div className="flex items-center justify-between">
                <span className="selectable font-mono text-[12px]">{id}</span>
                <span className="text-[11px] font-semibold" style={{ color: "var(--c-warn)" }}>
                  Damaged record
                </span>
              </div>
              <div className="mt-0.5 text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
                The files may be intact; the manifest could not be read: {err}
              </div>
            </button>
          ))}

          {/* Manifest-less dirs are an interrupted run's leftovers (pruned by the next
              run). Browsable as plain folders, but never presented as finished backups. */}
          {listing?.incomplete.map((id) => (
            <button
              key={id}
              onClick={() => selectSnapshot(id)}
              className="block w-full border-b px-3.5 py-3 text-left last:border-b-0"
              style={{
                borderColor: "var(--c-border)",
                background: id === snapshotId ? "var(--c-surface-2)" : "transparent",
              }}
            >
              <div className="flex items-center justify-between">
                <span className="selectable font-mono text-[12px]">{id}</span>
                <span className="text-[11px] font-semibold" style={{ color: "var(--c-warn)" }}>
                  Incomplete
                </span>
              </div>
              <div className="mt-0.5 text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
                Leftover from a run that did not finish; the next run cleans it up.
              </div>
            </button>
          ))}
        </div>
      </aside>

      <section
        className="rounded-[var(--radius-card)] border p-5"
        style={{ background: "var(--c-surface)" }}
      >
        {error && (
          <p className="mb-3 text-[13px]" style={{ color: "var(--c-danger)" }} role="alert">
            {error}
          </p>
        )}

        {!snapshotId && (
          <p className="text-[13px]" style={{ color: "var(--c-text-faint)" }}>
            Pick a snapshot on the left to browse it.
          </p>
        )}

        {snapshotId && (
          <>
            <nav className="mb-3 flex flex-wrap items-center gap-1 text-[13px]">
              <button
                onClick={() => setPath([])}
                style={{ color: path.length === 0 ? "var(--c-text)" : "var(--c-accent)" }}
                className="font-medium"
              >
                {snapshotId}
              </button>
              {path.map((seg, i) => (
                <span key={i} className="flex items-center gap-1">
                  <span style={{ color: "var(--c-text-faint)" }}>/</span>
                  <button
                    onClick={() => setPath(path.slice(0, i + 1))}
                    style={{
                      color: i === path.length - 1 ? "var(--c-text)" : "var(--c-accent)",
                    }}
                    className="font-medium"
                  >
                    {seg}
                  </button>
                </span>
              ))}
            </nav>

            {entries === null && !error && (
              <p className="text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
                Loading…
              </p>
            )}

            <ul className="flex flex-col">
              {entries?.map((e) => {
                const rel = [...path, e.name].join("/");
                return (
                  <li
                    key={e.name}
                    className="flex items-center justify-between gap-3 border-b py-2 last:border-b-0"
                    style={{ borderColor: "var(--c-border)" }}
                  >
                    <button
                      disabled={!e.isDir}
                      onClick={() => e.isDir && setPath([...path, e.name])}
                      className="flex min-w-0 flex-1 items-center gap-2 text-left text-[13px] disabled:cursor-default"
                    >
                      {e.isDir ? (
                        <FolderIcon className="size-4 shrink-0" style={{ color: "var(--c-accent)" }} />
                      ) : (
                        <FileIcon className="size-4 shrink-0" style={{ color: "var(--c-text-faint)" }} />
                      )}
                      <span className="truncate">{e.name}</span>
                      {e.size !== null && (
                        <span className="shrink-0 text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
                          {human(e.size)}
                        </span>
                      )}
                    </button>
                    <button
                      onClick={() => setRestoring({ rel, name: e.name })}
                      className="shrink-0 rounded-md px-2.5 py-1 text-[12px] font-medium"
                      style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
                    >
                      Restore…
                    </button>
                  </li>
                );
              })}
              {entries?.length === 0 && (
                <li className="py-2 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
                  Empty.
                </li>
              )}
            </ul>

            {restoring && (
              <div className="mt-4">
                <RestorePanel
                  jobId={jobId!}
                  snapshotId={snapshotId}
                  rel={restoring.rel}
                  name={restoring.name}
                  onCancel={() => setRestoring(null)}
                  onStart={onRestoreStarted}
                />
              </div>
            )}
          </>
        )}
      </section>
    </div>
  );
}
