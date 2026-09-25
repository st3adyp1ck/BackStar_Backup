import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { BackstarEvent, JobStats, RunOutcome } from "./types";

/** What kind of operation this panel is driving -- result framing depends on it. */
export type RunKind = "snapshot" | "mirror" | "restore";

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

export function duration(ms: number): string {
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
  kind,
  start,
  onFinished,
}: {
  title: string;
  kind: RunKind;
  start: () => Promise<unknown>;
  onFinished: () => void;
}) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [scan, setScan] = useState({ files: 0, bytes: 0 });
  const [plan, setPlan] = useState<{
    toCopy: number;
    toLink: number;
    bytes: number;
    deletes: number | null;
  } | null>(null);
  const [progress, setProgress] = useState({ files: 0, filesTotal: 0, bytes: 0, bytesTotal: 0 });
  const [current, setCurrent] = useState<string | null>(null);
  const [failures, setFailures] = useState<{ path: string; message: string }[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [verify, setVerify] = useState<{ examined: number; mismatches: string[] }[]>([]);
  const [result, setResult] = useState<{ outcome: RunOutcome; stats: JobStats } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const startedAt = useRef<number>(Date.now());
  // L9: StrictMode runs this component's mount effect twice in dev. The listener may be
  // re-registered freely, but the run itself must be started exactly once per mounted
  // panel (a fresh run gets a fresh panel, keyed by App, hence a fresh ref).
  const startFired = useRef(false);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;

    const onEvent = (e: { payload: BackstarEvent }) => {
      if (disposed) return;
      const ev = e.payload;
      switch (ev.type) {
        case "runStarted":
          setPhase("scanning");
          startedAt.current = Date.now();
          break;
        case "scanProgress":
          setScan({ files: ev.filesSeen, bytes: ev.bytesSeen });
          // F64: a directory restore walks the snapshot BEFORE any runStarted, so scanning
          // can arrive while still idle -- it is the first sign of life the user gets.
          setPhase((p) => (p === "done" ? p : "scanning"));
          break;
        case "planReady":
          setPlan({ toCopy: ev.toCopy, toLink: ev.toLink, bytes: ev.bytesToCopy, deletes: ev.deletes });
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
        case "sourceSkipped":
          setWarnings((w) => [...w, `Skipped ${ev.source} — ${ev.reason}`]);
          break;
        case "sourceUnreadable":
          setWarnings((w) => [...w, `Could not read ${ev.path} — ${ev.message}`]);
          break;
        case "runWarning":
          setWarnings((w) => [...w, ev.message]);
          break;
        case "verifyDone":
          setVerify((v) => [...v, { examined: ev.examined, mismatches: ev.mismatches }]);
          break;
        case "runDone":
          setResult({ outcome: ev.outcome, stats: ev.stats });
          setPhase("done");
          break;
      }
    };

    (async () => {
      // F25: the listener must be fully registered BEFORE start() is invoked. A
      // fast-failing run's terminal runDone otherwise races the in-flight registration,
      // is dropped, and this panel wedges on "scanning" forever.
      try {
        const un = await listen<BackstarEvent>("backstar://event", onEvent);
        if (disposed) {
          un();
          return;
        }
        unlisten = un;
      } catch (e) {
        if (!disposed) {
          setError(`Could not listen for run events: ${String(e)}`);
          setPhase("done");
        }
        return;
      }
      if (startFired.current) return;
      startFired.current = true;
      try {
        await start();
      } catch (e) {
        if (!disposed) {
          setError(String(e));
          setPhase("done");
        }
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
    // Intentionally empty: `start` is expected to be a stable closure captured once at
    // mount time (this component is remounted fresh whenever a new operation begins), not
    // re-invoked on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // L7: the elapsed clock and ETA would otherwise only move when an event arrives.
  useEffect(() => {
    if (phase === "done") return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [phase]);

  // Bytes are the honest denominator once known: a three-file 20 GB job sits at "0%" by
  // file count for most of its run (F34), while its bytes move continuously.
  const pct =
    phase === "copying"
      ? progress.bytesTotal > 0
        ? Math.min(100, Math.round((progress.bytes / progress.bytesTotal) * 100))
        : progress.filesTotal > 0
          ? Math.round((progress.files / progress.filesTotal) * 100)
          : 0
      : 0;
  const elapsed = now - startedAt.current;

  // An ETA is only offered once there is a real denominator and enough samples for the
  // rate to mean anything. A number that swings wildly is worse than no number.
  const eta =
    phase === "copying" && progress.files > 50 && progress.filesTotal > progress.files
      ? ((elapsed / progress.files) * (progress.filesTotal - progress.files)) / 1000
      : null;

  const planLine = (() => {
    if (!plan || phase === "done") return null;
    if (kind === "snapshot") {
      return `${plan.toCopy.toLocaleString()} to copy (${human(plan.bytes)}) · ${plan.toLink.toLocaleString()} unchanged, linked from the previous snapshot`;
    }
    if (kind === "mirror") {
      const deletes =
        plan.deletes === null
          ? null // unknown must never read as "~0"
          : plan.deletes > 0
            ? ` · ${plan.deletes.toLocaleString()} to permanently delete`
            : null;
      return `${plan.toCopy.toLocaleString()} to copy or update (${human(plan.bytes)}) — unchanged files are skipped${deletes ?? ""}`;
    }
    return `${plan.toCopy.toLocaleString()} to restore (${human(plan.bytes)})`;
  })();

  return (
    <div className="mx-auto flex max-w-[860px] flex-col gap-5">
      <section
        className="rounded-[var(--radius-card)] border p-7"
        style={{ background: "var(--c-surface)" }}
      >
        <div className="flex items-baseline justify-between">
          <h1 className="text-[22px] font-semibold tracking-[-0.02em]">{title}</h1>
          <span
            className="text-[13px] tabular-nums"
            style={{ color: "var(--c-text-dim)" }}
            aria-hidden="true"
          >
            {duration(elapsed)}
          </span>
        </div>

        {phase === "idle" && (
          <>
            {/* F64: before runStarted (a directory restore's scan, or just IPC latency)
                there is genuinely nothing to report yet -- say so. */}
            <p className="mt-4 text-[14px]" style={{ color: "var(--c-text-dim)" }} role="status">
              Starting…
            </p>
            <div
              className="mt-3 h-1.5 overflow-hidden rounded-full"
              style={{ background: "var(--c-surface-2)" }}
              role="progressbar"
              aria-label="Starting"
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

        {phase === "scanning" && (
          <>
            <p className="mt-4 text-[14px]" style={{ color: "var(--c-text-dim)" }} role="status">
              Scanning — {scan.files.toLocaleString()} files, {human(scan.bytes)}
            </p>
            {/* Indeterminate on purpose: until the walk finishes there is no denominator,
                and a percentage here would be invented. */}
            <div
              className="mt-3 h-1.5 overflow-hidden rounded-full"
              style={{ background: "var(--c-surface-2)" }}
              role="progressbar"
              aria-label="Scanning files"
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
            <div className="mt-4 flex items-baseline justify-between" role="status">
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
              role="progressbar"
              aria-valuenow={pct}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-label="Run progress"
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
              role="status"
            >
              {outcomeLabel(result.outcome).text}
            </p>
            <dl className="mt-4 grid grid-cols-2 gap-x-8 gap-y-2 text-[13px] sm:grid-cols-4">
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>
                  {kind === "restore" ? "Restored" : "Copied"}
                </dt>
                <dd className="tabular-nums">{result.stats.filesCopied.toLocaleString()}</dd>
              </div>
              {kind === "snapshot" && (
                <div>
                  <dt style={{ color: "var(--c-text-faint)" }}>Linked</dt>
                  <dd className="tabular-nums">{result.stats.filesLinked.toLocaleString()}</dd>
                </div>
              )}
              <div>
                <dt style={{ color: "var(--c-text-faint)" }}>Written</dt>
                <dd className="tabular-nums">{human(result.stats.bytesCopied)}</dd>
              </div>
              {kind === "snapshot" && (
                <div>
                  <dt style={{ color: "var(--c-text-faint)" }}>Saved by linking</dt>
                  <dd className="tabular-nums">{human(result.stats.bytesLinked)}</dd>
                </div>
              )}
              {/* F54: a mirror's deletions are the number the user confirmed -- report them. */}
              {kind === "mirror" && result.stats.filesDeleted > 0 && (
                <div>
                  <dt style={{ color: "var(--c-danger)" }}>Permanently deleted</dt>
                  <dd className="tabular-nums" style={{ color: "var(--c-danger)" }}>
                    {result.stats.filesDeleted.toLocaleString()}
                  </dd>
                </div>
              )}
              {result.stats.filesSkipped > 0 && (
                <div>
                  <dt style={{ color: "var(--c-warn)" }}>Kept (destination newer)</dt>
                  <dd className="tabular-nums">{result.stats.filesSkipped.toLocaleString()}</dd>
                </div>
              )}
              {kind === "snapshot" && result.stats.snapshotsPruned > 0 && (
                <div>
                  <dt style={{ color: "var(--c-text-faint)" }}>Old snapshots pruned</dt>
                  <dd className="tabular-nums">{result.stats.snapshotsPruned.toLocaleString()}</dd>
                </div>
              )}
              {result.stats.reparseSkipped > 0 && (
                <div>
                  <dt style={{ color: "var(--c-text-faint)" }}>Junctions/symlinks skipped</dt>
                  <dd className="tabular-nums">{result.stats.reparseSkipped.toLocaleString()}</dd>
                </div>
              )}
            </dl>

            {/* The post-copy verification pass (D1): "checked nothing" and "all clean" are
                different statements, and only the second is reassuring. */}
            {verify.map((v, i) =>
              v.mismatches.length === 0 ? (
                <p
                  key={i}
                  className="mt-3 text-[13px]"
                  style={{ color: "var(--c-ok)" }}
                  role="status"
                >
                  Verified {v.examined.toLocaleString()} file{v.examined === 1 ? "" : "s"}, no
                  mismatches
                </p>
              ) : (
                <div key={i} role="alert">
                  <p className="mt-3 text-[13px] font-semibold" style={{ color: "var(--c-danger)" }}>
                    Verification failed for {v.mismatches.length} of{" "}
                    {v.examined.toLocaleString()} file{v.examined === 1 ? "" : "s"} — the bad
                    copies were removed and will be re-copied on the next run
                  </p>
                  <ul className="mt-1 space-y-1">
                    {v.mismatches.slice(0, 10).map((m, j) => (
                      <li key={j} className="selectable font-mono text-[12px]">
                        {m}
                      </li>
                    ))}
                  </ul>
                </div>
              ),
            )}
          </>
        )}

        {error && (
          <p className="mt-4 text-[13px]" style={{ color: "var(--c-danger)" }} role="alert">
            {error}
          </p>
        )}

        {planLine && (
          <p className="mt-4 text-[12.5px]" style={{ color: "var(--c-text-faint)" }}>
            {planLine}
          </p>
        )}

        <div className="mt-6 flex gap-2">
          {phase !== "done" ? (
            <button
              onClick={() => {
                // L16: the click must be acknowledged -- a run may take seconds to notice.
                setCancelling(true);
                invoke("cancel_run").catch(() => setCancelling(false));
              }}
              disabled={cancelling}
              className="rounded-md px-4 py-2 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-50"
              style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
            >
              {cancelling ? "Cancelling…" : "Cancel"}
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

      {warnings.length > 0 && (
        <section
          className="rounded-[var(--radius-card)] border p-5"
          style={{
            background: "var(--c-surface)",
            borderColor: "color-mix(in srgb, var(--c-warn) 35%, var(--c-border))",
          }}
        >
          <h2 className="text-[14px] font-semibold" style={{ color: "var(--c-warn)" }}>
            {warnings.length} warning{warnings.length === 1 ? "" : "s"} during this run
          </h2>
          <ul className="mt-3 space-y-1.5">
            {warnings.map((w, i) => (
              <li key={i} className="selectable text-[12.5px]" style={{ color: "var(--c-text-dim)" }}>
                {w}
              </li>
            ))}
          </ul>
        </section>
      )}

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
