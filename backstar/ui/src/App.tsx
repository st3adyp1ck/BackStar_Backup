import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AppStatus, JobStatus, Protection } from "./types";
import { RunPanel, type RunKind } from "./RunPanel";
import { SnapshotsView } from "./SnapshotsView";
import { JobsView } from "./JobsView";
import { MirrorConfirm } from "./MirrorConfirm";
import { ActivityView } from "./ActivityView";
import { Banner } from "./Banner";
import { UpdateBanner } from "./UpdateBanner";
import { useAppUpdater } from "./updater";

type View = "home" | "jobs" | "snapshots" | "activity";

const NAV: { id: View; label: string; icon: string }[] = [
  { id: "home", label: "Home", icon: "M3 10.5 12 3l9 7.5M5 9.5V21h14V9.5" },
  { id: "jobs", label: "Jobs", icon: "M3 7h18M3 12h18M3 17h10" },
  { id: "snapshots", label: "Snapshots", icon: "M12 8v4l3 2M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z" },
  { id: "activity", label: "Activity", icon: "M3 12h4l3 8 4-16 3 8h4" },
];

function Icon({ d, className = "" }: { d: string; className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden="true"
    >
      <path d={d} />
    </svg>
  );
}

const PROTECTION_STYLE: Record<Protection, { tint: string; label: string }> = {
  protected: { tint: "var(--c-ok)", label: "Protected" },
  stale: { tint: "var(--c-warn)", label: "Out of date" },
  "at-risk": { tint: "var(--c-danger)", label: "At risk" },
  "never-run": { tint: "var(--c-text-faint)", label: "Never run" },
  "no-jobs": { tint: "var(--c-text-faint)", label: "Not set up" },
};

/** The one number that matters, stated plainly. */
function StatusHero({ status }: { status: AppStatus }) {
  const s = PROTECTION_STYLE[status.protection];
  return (
    <section
      className="rounded-[var(--radius-card)] border p-7"
      style={{
        background: `linear-gradient(180deg, color-mix(in srgb, ${s.tint} 9%, var(--c-surface)), var(--c-surface))`,
        borderColor: `color-mix(in srgb, ${s.tint} 28%, var(--c-border))`,
      }}
    >
      <div className="flex items-center gap-2.5">
        <span
          className="inline-block size-2.5 rounded-full"
          style={{ background: s.tint, boxShadow: `0 0 0 4px color-mix(in srgb, ${s.tint} 22%, transparent)` }}
        />
        <span
          className="text-[11px] font-semibold uppercase tracking-[0.13em]"
          style={{ color: s.tint }}
        >
          {s.label}
        </span>
      </div>
      <h1 className="mt-3 text-[26px] font-semibold leading-tight tracking-[-0.02em]">
        {status.headline}
      </h1>
      <p className="mt-2 max-w-[62ch] text-[14px]" style={{ color: "var(--c-text-dim)" }}>
        {status.detail}
      </p>
    </section>
  );
}

export function relativeTime(iso: string): string {
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

function JobCard({ job, onRun }: { job: JobStatus; onRun: (j: JobStatus) => void }) {
  const isMirror = job.kind === "mirror";
  const vol = job.volume;
  const problems: { tone: "warn" | "danger"; text: string }[] = [];

  if (job.missingSources.length > 0) {
    problems.push({
      tone: "danger",
      text: `${job.missingSources.length} source${job.missingSources.length === 1 ? "" : "s"} missing — this job is not backing up what you think it is`,
    });
  }
  if (vol.kind === "unavailable") {
    problems.push({ tone: "warn", text: "Destination drive is not available right now" });
  }
  // Hardlink economics only apply to versioned snapshots; a mirror recopies or skips
  // files but never links, so an exFAT destination is a non-issue for it.
  if (!isMirror && vol.kind === "local" && !vol.hardLinks) {
    problems.push({
      tone: "warn",
      text: `Destination is ${vol.filesystem}, which has no hard links — snapshots cannot share storage and each backup costs its full size`,
    });
  }
  if (!isMirror && vol.kind === "network") {
    problems.push({
      tone: "warn",
      text: "Destination is a network location, which cannot hardlink — every snapshot costs its full size",
    });
  }
  if (job.sameVolumeAsSource) {
    problems.push({
      tone: "warn",
      text: "Source and destination are on the same physical drive — this protects against deletion, not drive failure",
    });
  }
  if (job.scheduledMissed) {
    problems.push({
      tone: "warn",
      text: "A scheduled run was missed — the computer was probably off at the scheduled time",
    });
  }
  if (!isMirror && job.unreadableSnapshots > 0) {
    problems.push({
      tone: "warn",
      text: `${job.unreadableSnapshots} snapshot record${job.unreadableSnapshots === 1 ? "" : "s"} could not be read — the files may still be intact; see Snapshots`,
    });
  }

  // F55: the latest ATTEMPT answers "did the last run go wrong?", separately from the
  // latest run that actually protected something. A failure newer than the last success
  // must not hide behind a cheerful relative time.
  const attempt = job.lastAttempt;
  const attemptFailed = attempt != null && !attempt.outcome.startsWith("OK");
  const attemptIsNewest =
    attemptFailed &&
    (!job.lastRun || Date.parse(attempt.timestamp) >= Date.parse(job.lastRun.timestamp));

  const unavailable = vol.kind === "unavailable";
  const disabled = job.missingSources.length > 0 || unavailable || !job.enabled;

  return (
    <article
      className="rounded-[var(--radius-card)] border p-5"
      style={{ background: "var(--c-surface)" }}
    >
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="flex min-w-0 items-center gap-2 text-[15px] font-semibold">
          <span className="truncate">{job.jobName}</span>
          {isMirror && (
            <span
              className="shrink-0 rounded px-1.5 py-0.5 text-[10.5px] font-semibold uppercase tracking-[0.06em]"
              style={{
                background: "color-mix(in srgb, var(--c-danger) 16%, transparent)",
                color: "var(--c-danger)",
              }}
            >
              mirror
            </span>
          )}
          {!job.enabled && (
            <span className="shrink-0 text-[11px] font-normal" style={{ color: "var(--c-text-faint)" }}>
              disabled
            </span>
          )}
        </h3>
        {/* Snapshot framing belongs to snapshot jobs: a mirror has no history to count. */}
        {!isMirror && (
          <span className="text-[12px] tabular-nums" style={{ color: "var(--c-text-faint)" }}>
            {job.snapshotCount > 0
              ? `${job.snapshotCount} snapshot${job.snapshotCount === 1 ? "" : "s"}`
              : "no snapshots"}
          </span>
        )}
      </div>

      <dl className="mt-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-[13px]">
        <dt style={{ color: "var(--c-text-faint)" }}>Sources</dt>
        <dd>
          {job.sourceCount} folder{job.sourceCount === 1 ? "" : "s"}
        </dd>
        <dt style={{ color: "var(--c-text-faint)" }}>Destination</dt>
        <dd className="selectable truncate font-mono text-[12px]" title={job.dest}>
          {job.dest}
        </dd>
        <dt style={{ color: "var(--c-text-faint)" }}>Last run</dt>
        <dd>
          {attemptIsNewest && attempt ? (
            <span style={{ color: "var(--c-warn)" }}>
              {attempt.outcome.startsWith("Cancelled") ? "Last run cancelled" : "Last run failed"}
              {" · "}
              {relativeTime(attempt.timestamp)}
            </span>
          ) : job.lastRun ? (
            <>
              {relativeTime(job.lastRun.timestamp)}
              <span style={{ color: "var(--c-text-faint)" }}>
                {" · "}
                {job.lastRun.filesCopied.toLocaleString()} copied
                {isMirror && job.lastRun.filesDeleted > 0 &&
                  ` · ${job.lastRun.filesDeleted.toLocaleString()} deleted`}
              </span>
            </>
          ) : (
            <span style={{ color: "var(--c-text-faint)" }}>never</span>
          )}
        </dd>
      </dl>

      {problems.length > 0 && (
        <ul className="mt-4 space-y-2">
          {problems.map((p, i) => (
            <li
              key={i}
              className="flex gap-2 text-[12.5px] leading-snug"
              style={{ color: p.tone === "danger" ? "var(--c-danger)" : "var(--c-warn)" }}
            >
              <span aria-hidden="true">•</span>
              <span>{p.text}</span>
            </li>
          ))}
        </ul>
      )}

      <button
        onClick={() => onRun(job)}
        disabled={disabled}
        className="mt-5 w-full rounded-md px-4 py-2 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
        style={
          isMirror
            ? { background: "var(--c-danger)", color: "var(--c-on-danger)" }
            : { background: "var(--c-accent)", color: "var(--c-on-accent)" }
        }
        title={
          job.missingSources.length > 0
            ? "Fix the missing source folders first"
            : unavailable
              ? "The destination drive is not available"
              : !job.enabled
                ? "This job is disabled — enable it in Jobs"
                : undefined
        }
      >
        {/* A mirror deletes at the destination, so it must never read as a plain backup,
            and it always previews first (the "…" is the promise of a dialog). */}
        {isMirror ? "Sync now…" : "Back up now"}
      </button>
    </article>
  );
}

interface RunningState {
  /** Distinct per run, used as the panel's React key so every run mounts a fresh panel. */
  key: number;
  title: string;
  kind: RunKind;
  start: () => Promise<unknown>;
}

export default function App() {
  const [view, setView] = useState<View>("home");
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [configNotice, setConfigNotice] = useState<string | null>(null);
  const [scheduleNotices, setScheduleNotices] = useState<string[] | null>(null);
  const [confirmingMirror, setConfirmingMirror] = useState<JobStatus | null>(null);
  const [running, setRunning] = useState<RunningState | null>(null);
  const runSeq = useRef(0);
  const updater = useAppUpdater();

  // L8: a stale error banner must not survive a refresh that is about to succeed.
  const refresh = useCallback(() => {
    setError(null);
    invoke<AppStatus>("get_status")
      .then((s) => {
        setStatus(s);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    refresh();
    invoke<string | null>("config_notice")
      .then(setConfigNotice)
      .catch(() => {});
    invoke<string[]>("schedule_notices")
      .then((n) => setScheduleNotices(n.length > 0 ? n : null))
      .catch(() => {});
  }, [refresh]);

  // F26: Home reflects facts on disk (snapshots, history), which other views change --
  // coming back to Home re-reads them.
  useEffect(() => {
    if (view === "home") refresh();
  }, [view, refresh]);

  // F26/F44: the app lives in the tray while runs proceed headlessly; regaining focus is
  // the moment the user expects current truth.
  useEffect(() => {
    const onFocus = () => refresh();
    const onVisible = () => {
      if (document.visibilityState === "visible") refresh();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [refresh]);

  // F44: the backend emits this on every terminal run path -- including scheduled runs
  // and runs driven by another window's click.
  useEffect(() => {
    const unlisten = listen<unknown>("backstar://run-finished", () => refresh());
    return () => {
      unlisten.then((f) => f());
    };
  }, [refresh]);

  function begin(title: string, kind: RunKind, start: () => Promise<unknown>) {
    runSeq.current += 1;
    setRunning({ key: runSeq.current, title, kind, start });
  }

  function runJob(job: JobStatus) {
    if (job.kind === "mirror") {
      // F2: a mirror run is destructive; the preview-confirm dialog is the only path in.
      setConfirmingMirror(job);
      return;
    }
    begin(job.jobName, "snapshot", () =>
      invoke("start_run", { jobId: job.jobId, confirmMirror: false }),
    );
  }

  return (
    <div className="flex h-full">
      <nav
        className="flex w-[196px] shrink-0 flex-col border-r px-3 py-4"
        style={{ background: "var(--c-surface)" }}
      >
        <div className="flex items-center gap-2 px-2 pb-5">
          <div
            className="grid size-7 place-items-center rounded-md text-[13px] font-bold"
            style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
          >
            B
          </div>
          <span className="text-[15px] font-semibold tracking-[-0.01em]">BackStar</span>
        </div>

        {NAV.map((n) => {
          const active = view === n.id;
          return (
            <button
              key={n.id}
              onClick={() => setView(n.id)}
              aria-current={active ? "page" : undefined}
              className="flex items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-[13.5px] transition-colors"
              style={{
                background: active ? "var(--c-surface-2)" : "transparent",
                color: active ? "var(--c-text)" : "var(--c-text-dim)",
                fontWeight: active ? 600 : 400,
              }}
            >
              <Icon d={n.icon} className="size-4 shrink-0" />
              {n.label}
            </button>
          );
        })}

        <div className="mt-auto px-2 text-[11px]" style={{ color: "var(--c-text-faint)" }}>
          <div>{status ? `v${status.version}` : ""}</div>
          <button
            onClick={() => updater.checkNow(true)}
            disabled={updater.checking}
            className="mt-1 underline decoration-dotted underline-offset-2 disabled:opacity-40"
            style={{ color: "var(--c-text-dim)" }}
          >
            {updater.checking ? "Checking…" : "Check for updates"}
          </button>
          {updater.manualFeedback && (
            <div className="mt-1">
              {updater.manualFeedback === "up-to-date"
                ? "You're on the latest version"
                : "The update check failed — no connection?"}
            </div>
          )}
        </div>
      </nav>

      <main className="flex-1 overflow-y-auto px-8 py-7">
        {/* The update offer stays visible during a run (views hide); installing is what
            waits for the run to finish, since install closes the app. */}
        <UpdateBanner
          phase={updater.phase}
          dismissed={updater.dismissed}
          installError={updater.installError}
          busy={running != null}
          onInstall={updater.install}
          onDismiss={updater.dismiss}
        />

        {running && (
          <RunPanel
            key={running.key}
            title={running.title}
            kind={running.kind}
            start={running.start}
            onFinished={() => {
              setRunning(null);
              refresh();
            }}
          />
        )}

        {/* L17: views are HIDDEN during a run, not unmounted, so the snapshot browser's
            place (selected job, snapshot, folder) survives a restore end to end. */}
        <div style={running ? { display: "none" } : undefined}>
          {error && (
            <Banner tone="danger" title="Could not load status">
              <span className="selectable font-mono text-[12px]">{error}</span>
            </Banner>
          )}

          {configNotice && (
            <Banner
              tone="warn"
              title="Configuration problem recovered"
              onDismiss={() => setConfigNotice(null)}
            >
              {configNotice}
            </Banner>
          )}

          {scheduleNotices && (
            <Banner
              tone="warn"
              title="Some scheduled tasks could not be recreated"
              onDismiss={() => setScheduleNotices(null)}
            >
              <ul className="space-y-1">
                {scheduleNotices.map((n, i) => (
                  <li key={i}>{n} Re-save the job to try again.</li>
                ))}
              </ul>
            </Banner>
          )}

          {!status && !error && <p style={{ color: "var(--c-text-faint)" }}>Loading…</p>}

          {status && view === "home" && (
            <div className="mx-auto flex max-w-[860px] flex-col gap-5">
              <StatusHero status={status} />

              {confirmingMirror ? (
                <MirrorConfirm
                  jobId={confirmingMirror.jobId}
                  jobName={confirmingMirror.jobName}
                  onCancel={() => setConfirmingMirror(null)}
                  onConfirm={() => {
                    const job = confirmingMirror;
                    setConfirmingMirror(null);
                    begin(job.jobName, "mirror", () =>
                      invoke("start_run", { jobId: job.jobId, confirmMirror: true }),
                    );
                  }}
                />
              ) : status.jobs.length === 0 ? (
                <Banner tone="info" title="No backup jobs yet">
                  Add a job to choose what gets backed up and where it goes.
                </Banner>
              ) : (
                <div className="grid gap-4 md:grid-cols-2">
                  {status.jobs.map((j) => (
                    <JobCard key={j.jobId} job={j} onRun={runJob} />
                  ))}
                </div>
              )}
            </div>
          )}

          {status && view === "jobs" && (
            <JobsView
              onRunStarted={(title, start) => {
                // JobsView passes only the name; the kind is looked up so the run panel's
                // framing (and the mirror confirm gate, once D2 wires it) stays truthful.
                const kind = status.jobs.find((j) => j.jobName === title)?.kind ?? "snapshot";
                begin(title, kind, start);
              }}
            />
          )}

          {status && view === "snapshots" && (
            <SnapshotsView
              jobs={status.jobs}
              onRestoreStarted={(title, start) => begin(title, "restore", start)}
            />
          )}

          {status && view === "activity" && <ActivityView />}
        </div>
      </main>
    </div>
  );
}
