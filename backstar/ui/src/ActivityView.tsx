import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { HistoryEntry } from "./types";
import { duration, human } from "./RunPanel";
import { relativeTime } from "./App";

/** The history outcome strings are written by the shell (`outcome_summary` in lib.rs). */
function outcomeStyle(outcome: string): { tint: string } {
  if (outcome === "OK") return { tint: "var(--c-ok)" };
  if (outcome.startsWith("OK")) return { tint: "var(--c-warn)" }; // "OK (N files failed)"
  if (outcome.startsWith("Failed")) return { tint: "var(--c-danger)" };
  if (outcome === "Cancelled") return { tint: "var(--c-warn)" };
  return { tint: "var(--c-text-dim)" };
}

function EntryRow({ entry }: { entry: HistoryEntry }) {
  const counts = [
    `${entry.filesCopied.toLocaleString()} copied`,
    entry.filesLinked > 0 ? `${entry.filesLinked.toLocaleString()} linked` : null,
    entry.filesDeleted > 0 ? `${entry.filesDeleted.toLocaleString()} deleted` : null,
    entry.filesFailed > 0 ? `${entry.filesFailed.toLocaleString()} failed` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <li
      className="rounded-[var(--radius-card)] border px-4 py-3"
      style={{ background: "var(--c-surface)" }}
    >
      <div className="flex items-baseline justify-between gap-3">
        <span className="text-[13.5px] font-medium">{entry.job}</span>
        <span
          className="text-[12px] tabular-nums"
          style={{ color: "var(--c-text-faint)" }}
          title={entry.timestamp}
        >
          {relativeTime(entry.timestamp)}
        </span>
      </div>
      <div className="mt-1 flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-[12.5px]">
        <span className="font-semibold" style={{ color: outcomeStyle(entry.outcome).tint }}>
          {entry.outcome}
        </span>
        <span style={{ color: "var(--c-text-dim)" }}>{counts}</span>
        <span className="tabular-nums" style={{ color: "var(--c-text-faint)" }}>
          {duration(entry.durationMs)} · {human(entry.bytesCopied)}
        </span>
      </div>
      {entry.snapshotId && (
        <div
          className="selectable mt-1 font-mono text-[11.5px]"
          style={{ color: "var(--c-text-faint)" }}
        >
          snapshot {entry.snapshotId}
        </div>
      )}
    </li>
  );
}

/**
 * Every run the app has recorded, newest first (F61) -- including failures and
 * cancellations, which reach history just like successes. This view stays mounted while a
 * run is in flight (App hides rather than unmounts it), so it re-reads on every
 * `backstar://run-finished` rather than only on mount.
 */
export function ActivityView() {
  const [entries, setEntries] = useState<HistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let disposed = false;
    const refresh = () => {
      invoke<HistoryEntry[]>("get_history")
        .then((h) => {
          if (!disposed) {
            setError(null);
            setEntries(h);
          }
        })
        .catch((e) => {
          if (!disposed) setError(String(e));
        });
    };
    refresh();
    const unlisten = listen<unknown>("backstar://run-finished", refresh);
    return () => {
      disposed = true;
      unlisten.then((f) => f());
    };
  }, []);

  return (
    <div className="mx-auto flex max-w-[860px] flex-col gap-4">
      <h1 className="text-[18px] font-semibold tracking-[-0.01em]">Activity</h1>

      {error && (
        <p className="text-[13px]" style={{ color: "var(--c-danger)" }} role="alert">
          Could not load run history: {error}
        </p>
      )}

      {!error && entries === null && (
        <p style={{ color: "var(--c-text-faint)" }}>Loading…</p>
      )}

      {!error && entries?.length === 0 && (
        <div
          className="rounded-[var(--radius-card)] border p-6 text-center"
          style={{ background: "var(--c-surface)", color: "var(--c-text-faint)" }}
        >
          No runs yet. Backups, syncs, and restores will appear here once they have run.
        </div>
      )}

      {entries && entries.length > 0 && (
        <ul className="flex flex-col gap-2">
          {[...entries].reverse().map((e, i) => (
            <EntryRow key={`${e.timestamp}-${i}`} entry={e} />
          ))}
        </ul>
      )}
    </div>
  );
}
