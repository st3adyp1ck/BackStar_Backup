import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { BrowseEntry, JobStatus, RestoreTarget, SnapshotManifest } from "./types";
import { human, outcomeLabel } from "./RunPanel";

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

function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return iso;
  const mins = Math.floor((Date.now() - then) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} day${days === 1 ? "" : "s"} ago`;
  return new Date(then).toLocaleDateString();
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
  const [customFolder, setCustomFolder] = useState<string | null>(null);
  const [mode, setMode] = useState<"original" | "custom">("original");
  const [error, setError] = useState<string | null>(null);

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
      .catch(() => {
        if (!cancelled) {
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

  const destinationPreview =
    mode === "original" ? original : customFolder ? `${customFolder}\\${name}` : null;

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
            className="mt-0.5"
            checked={mode === "original"}
            disabled={original === undefined || original === null}
            onChange={() => setMode("original")}
          />
          <span>
            To its original location
            {original === undefined && (
              <span style={{ color: "var(--c-text-faint)" }}> — checking…</span>
            )}
            {original === null && (
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

      {destinationPreview && (
        <p className="mt-3 text-[12px]" style={{ color: "var(--c-text-faint)" }}>
          Existing files at this location with the same name will be overwritten. Nothing
          else there is touched or deleted.
        </p>
      )}

      {error && (
        <p className="mt-2 text-[12.5px]" style={{ color: "var(--c-danger)" }}>
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
          disabled={!target}
          onClick={() => {
            if (!target) return;
            onStart(`Restore: ${name}`, () =>
              invoke("start_restore", { jobId, snapshotId, snapshotRel: rel, target }),
            );
          }}
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
        >
          Restore
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
  const [snapshots, setSnapshots] = useState<SnapshotManifest[] | null>(null);
  const [snapshotId, setSnapshotId] = useState<string | null>(null);
  const [path, setPath] = useState<string[]>([]);
  const [entries, setEntries] = useState<BrowseEntry[] | null>(null);
  const [restoring, setRestoring] = useState<{ rel: string; name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!jobId) return;
    setSnapshots(null);
    setSnapshotId(null);
    setPath([]);
    setEntries(null);
    invoke<SnapshotManifest[]>("list_snapshots", { jobId })
      .then(setSnapshots)
      .catch((e) => setError(String(e)));
  }, [jobId]);

  useEffect(() => {
    if (!jobId || !snapshotId) return;
    setEntries(null);
    setRestoring(null);
    invoke<BrowseEntry[]>("browse_snapshot", {
      jobId,
      snapshotId,
      snapshotRel: path.join("/"),
    })
      .then(setEntries)
      .catch((e) => setError(String(e)));
  }, [jobId, snapshotId, path]);

  const activeJob = jobs.find((j) => j.jobId === jobId);

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
            {jobs.map((j) => (
              <option key={j.jobId} value={j.jobId}>
                {j.jobName}
              </option>
            ))}
          </select>
        </div>

        <div
          className="flex-1 overflow-y-auto rounded-[var(--radius-card)] border"
          style={{ background: "var(--c-surface)", maxHeight: "calc(100vh - 220px)" }}
        >
          {snapshots === null && (
            <p className="p-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
              Loading…
            </p>
          )}
          {snapshots?.length === 0 && (
            <p className="p-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
              {activeJob?.jobName ?? "This job"} has no snapshots yet.
            </p>
          )}
          {snapshots?.map((s) => {
            const active = s.id === snapshotId;
            const o = outcomeLabel(s.outcome);
            return (
              <button
                key={s.id}
                onClick={() => {
                  setSnapshotId(s.id);
                  setPath([]);
                }}
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
        </div>
      </aside>

      <section
        className="rounded-[var(--radius-card)] border p-5"
        style={{ background: "var(--c-surface)" }}
      >
        {error && (
          <p className="mb-3 text-[13px]" style={{ color: "var(--c-danger)" }}>
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

            {entries === null && (
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
