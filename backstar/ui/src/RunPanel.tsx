import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { RunOutcome } from "./types";

/** Mirrors `backstar_core::events::Event`, which serialises with a `type` tag. */
type BackstarEvent =
  | { type: "runStarted"; runId: string; jobs: number }
  | { type: "jobStarted"; job: string; source: string; dest: string }
  | { type: "scanProgress"; filesSeen: number; bytesSeen: number }
  | {
      type: "planReady";
      toCopy: number;
      toLink: number;
      bytesToCopy: number;
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
  | { type: "fileFailed"; path: string; message: string }
  | { type: "jobDone"; job: string; stats: JobStats }
  | { type: "verifyDone"; examined: number; mismatches: string[] }
  | { type: "runDone"; runId: string; outcome: RunOutcome; stats: JobStats };

interface JobStats {
  filesCopied: number;
  filesLinked: number;
  filesSkipped: number;
  filesFailed: number;
  bytesCopied: number;
  bytesLinked: number;
  durationMs: number;
}

type Phase = "idle" | "scanning" | "copying" | "done";

export function human(bytes: number): string {
  const GB = 1 << 30;
  const MB = 1 << 20;
  const KB = 1 << 10;
  if (bytes >= GB) return `${(bytes / GB).toFixed(2)} GB`;
  if (bytes >= MB) return `${(bytes / MB).toFixed(1)} MB`;
  if (bytes >= KB) return `${(bytes / KB).toFixed(0)} KB`;
  return `${bytes} B`;
}

function duration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${String(s % 60).padStart(2, "0")}s`;
}

export function outcomeLabel(o: RunOutcome): { text: string; tint: string } {
  if (o === "ok") return { text: "Completed", tint: "var(--c-ok)" };
  if (o === "cancelled") return { text: "Cancelled", tint: "var(--c-warn)" };
  if ("partial" in o)
    return {
      text: `Completed with ${o.partial.failed} file${o.partial.failed === 1 ? "" : "s"} failed`,
      tint: "var(--c-warn)",
    };
  return { text: `Failed: ${o.failed.reason}`, tint: "var(--c-danger)" };
}

/**
 * Drives one backup run or restore to completion, listening on the shared event channel.
 *
 * `start` is called once, on mount, to kick the operation off (`invoke("start_run", ...)`
 * or `invoke("start_restore", ...)`); everything after that is identical between the two,
 * because both emit the same `backstar_core::events::Event` sequence over the same channel.
 */
export function RunPanel({
  title,
  start,
  onFinished,
}: {
  title: string;
  start: () => Promise<unknown>;
  onFinished: () => void;
}) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [scan, setScan] = useState({ files: 0, bytes: 0 });
  const [plan, setPlan] = useState<{ toCopy: number; toLink: number; bytes: number } | null>(null);
  const [progress, setProgress] = useState({ files: 0, filesTotal: 0, bytes: 0, bytesTotal: 0 });
  const [current, setCurrent] = useState<string | null>(null);
  const [failures, setFailures] = useState<{ path: string; message: string }[]>([]);
  const [result, setResult] = useState<{ outcome: RunOutcome; stats: JobStats } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const startedAt = useRef<number>(Date.now());

  useEffect(() => {
    let disposed = false;
    const unlisten = listen<BackstarEvent>("backstar://event", (e) => {
      if (disposed) return;
      const ev = e.payload;
      switch (ev.type) {
        case "runStarted":
          setPhase("scanning");
          startedAt.current = Date.now();
          break;
        case "scanProgress":
          setScan({ files: ev.filesSeen, bytes: ev.bytesSeen });
          break;
        case "planReady":
          setPlan({ toCopy: ev.toCopy, toLink: ev.toLink, bytes: ev.bytesToCopy });
          setPhase("copying");
          break;
        case "overallProgress":
          setProgress({
            files: ev.filesDone,
            filesTotal: ev.filesTotal,
            bytes: ev.bytesDone,
            bytesTotal: ev.bytesTotal,
          });
          setCurrent(ev.current);
          break;
        case "fileFailed":
          setFailures((f) => [...f, { path: ev.path, message: ev.message }]);
          break;
        case "runDone":
          setResult({ outcome: ev.outcome, stats: ev.stats });
          setPhase("done");
          break;
      }
    });

    start().catch((e) => {
      setError(String(e));
      setPhase("done");
    });

    return () => {
      disposed = true;
      unlisten.then((f) => f());
    };
    // Intentionally empty: `start` is expected to be a stable closure captured once at
    // mount time (this component is remounted fresh whenever a new operation begins), not
    // re-invoked on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const pct =
    progress.filesTotal > 0 ? Math.round((progress.files / progress.filesTotal) * 100) : 0;
  const elapsed = Date.now() - startedAt.current;

  // An ETA is only offered once there is a real denominator and enough samples for the
  // rate to mean anything. A number that swings wildly is worse than no number.
  const eta =
    phase === "copying" && progress.files > 50 && progress.filesTotal > progress.files
      ? ((elapsed / progress.files) * (progress.filesTotal - progress.files)) / 1000
      : null;

  return (
    <div className="mx-auto flex max-w-[860px] flex-col gap-5">
      <section
        className="rounded-[var(--radius-card)] border p-7"
        style={{ background: "var(--c-surface)" }}
      >
        <div className="flex items-baseline justify-between">
          <h1 className="text-[22px] font-semibold tracking-[-0.02em]">{title}</h1>
          <span className="text-[13px] tabular-nums" style={{ color: "var(--c-text-dim)" }}>
            {duration(elapsed)}
          </span>
        </div>

        {phase === "scanning" && (
          <>
            <p className="mt-4 text-[14px]" style={{ color: "var(--c-text-dim)" }}>
              Scanning — {scan.files.toLocaleString()} files, {human(scan.bytes)}
            </p>
            {/* Indeterminate on purpose: until the walk finishes there is no denominator,
                and a percentage here would be invented. */}
            <div
              className="mt-3 h-1.5 overflow-hidden rounded-full"
              style={{ background: "var(--c-surface-2)" }}
            >
              <div
                className="h-full w-1/3 rounded-full"
                style={{
                  background: "var(--c-accent)",
                  animation: "backstar-indeterminate 1.4s ease-in-out infinite",
                }}
              />
            </div>
          </>
        )}

        {phase === "copying" && (
          <>
            <div className="mt-4 flex items-baseline justify-between">
              <span className="text-[28px] font-semibold tabular-nums tracking-[-0.02em]">
                {pct}%
              </span>
              <span className="text-[13px] tabular-nums" style={{ color: "var(--c-text-dim)" }}>
                {progress.files.toLocaleString()} / {progress.filesTotal.toLocaleString()} files
                {eta !== null && ` · ~${duration(eta * 1000)} left`}
              </span>
            </div>
            <div
              className="mt-3 h-1.5 overflow-hidden rounded-full"
              style={{ background: "var(--c-surface-2)" }}
            >
              <div
                className="h-full rounded-full transition-[width] duration-300"
                style={{ width: `${pct}%`, background: "var(--c-accent)" }}
              />
            </div>
            {current && (
              <p
                className="selectable mt-3 truncate font-mono text-[12px]"
                style={{ color: "var(--c-text-faint)" }}
                title={current}
              >
                {current}
              </p>
            )}
          </>
        )}

        {phase === "done" && result && (
          <>
            <p
              className="mt-4 text-[15px] font-semibold"
              style={{ color: outcomeLabel(result.outcome).tint }}
            >
              {outcomeLabel(result.outcome).text}
            </p>
            <dl className="mt-4 grid grid-cols-2 gap-x-8 gap-y-2 text-[13px] sm:grid-cols-4">
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>Copied</dt>
                <dd className="tabular-nums">{result.stats.filesCopied.toLocaleString()}</dd>
              </div>
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>Linked</dt>
                <dd className="tabular-nums">{result.stats.filesLinked.toLocaleString()}</dd>
              </div>
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>Written</dt>
                <dd className="tabular-nums">{human(result.stats.bytesCopied)}</dd>
              </div>
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>Saved by linking</dt>
                <dd className="tabular-nums">{human(result.stats.bytesLinked)}</dd>
              </div>
            </dl>
          </>
        )}

        {error && (
          <p className="mt-4 text-[13px]" style={{ color: "var(--c-danger)" }}>
            {error}
          </p>
        )}

        {plan && phase !== "done" && (
          <p className="mt-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            {plan.toCopy.toLocaleString()} to copy ({human(plan.bytes)}) ·{" "}
            {plan.toLink.toLocaleString()} unchanged, linked from the previous snapshot
          </p>
        )}

        <div className="mt-6 flex gap-2">
          {phase !== "done" ? (
            <button
              onClick={() => invoke("cancel_run")}
              className="rounded-md px-4 py-2 text-[13px] font-medium"
              style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
            >
              Cancel
            </button>
          ) : (
            <button
              onClick={onFinished}
              className="rounded-md px-4 py-2 text-[13px] font-medium"
              style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
            >
              Done
            </button>
          )}
        </div>
      </section>

      {failures.length > 0 && (
        <section
          className="rounded-[var(--radius-card)] border p-5"
          style={{ background: "var(--c-surface)" }}
        >
          <h2 className="text-[14px] font-semibold" style={{ color: "var(--c-warn)" }}>
            {failures.length} file{failures.length === 1 ? "" : "s"} could not be copied
          </h2>
          <ul className="mt-3 space-y-1.5">
            {failures.slice(0, 20).map((f, i) => (
              <li key={i} className="selectable font-mono text-[12px]">
                <span style={{ color: "var(--c-text)" }}>{f.path}</span>
                <span style={{ color: "var(--c-text-faint)" }}> — {f.message}</span>
              </li>
            ))}
          </ul>
          {failures.length > 20 && (
            <p className="mt-2 text-[12px]" style={{ color: "var(--c-text-faint)" }}>
              …and {failures.length - 20} more
            </p>
          )}
        </section>
      )}
    </div>
  );
}
