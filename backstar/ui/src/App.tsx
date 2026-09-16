import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AppStatus, JobStatus, Protection } from "./types";
import { RunPanel } from "./RunPanel";
import { SnapshotsView } from "./SnapshotsView";
import { JobsView } from "./JobsView";

type View = "home" | "jobs" | "snapshots" | "activity" | "settings";

const NAV: { id: View; label: string; icon: string }[] = [
  { id: "home", label: "Home", icon: "M3 10.5 12 3l9 7.5M5 9.5V21h14V9.5" },
  { id: "jobs", label: "Jobs", icon: "M3 7h18M3 12h18M3 17h10" },
  { id: "snapshots", label: "Snapshots", icon: "M12 8v4l3 2M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z" },
  { id: "activity", label: "Activity", icon: "M3 12h4l3 8 4-16 3 8h4" },
  { id: "settings", label: "Settings", icon: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6ZM19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.6 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.6a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z" },
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

function Banner({
  tone,
  title,
  children,
}: {
  tone: "warn" | "danger" | "info";
  title: string;
  children: React.ReactNode;
}) {
  const tint =
    tone === "danger" ? "var(--c-danger)" : tone === "warn" ? "var(--c-warn)" : "var(--c-accent)";
  return (
    <div
      className="rounded-[var(--radius-card)] border px-4 py-3"
      style={{
        borderColor: `color-mix(in srgb, ${tint} 35%, var(--c-border))`,
        background: `color-mix(in srgb, ${tint} 8%, var(--c-surface))`,
      }}
    >
      <div className="text-[13px] font-semibold" style={{ color: tint }}>
        {title}
      </div>
      <div className="mt-1 text-[13px]" style={{ color: "var(--c-text-dim)" }}>
        {children}
      </div>
    </div>
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

function JobCard({ job, onRun }: { job: JobStatus; onRun: (j: JobStatus) => void }) {
  const problems: { tone: "warn" | "danger"; text: string }[] = [];

  if (job.missingSources.length > 0) {
    problems.push({
      tone: "danger",
      text: `${job.missingSources.length} source${job.missingSources.length === 1 ? "" : "s"} missing — this job is not backing up what you think it is`,
    });
  }
  if (job.volume && !job.volume.hardLinks) {
    problems.push({
      tone: "warn",
      text: `Destination is ${job.volume.filesystem}, which has no hard links — snapshots are unavailable and each backup costs full size`,
    });
  }
  if (job.sameVolumeAsSource) {
    problems.push({
      tone: "warn",
      text: "Source and destination are on the same physical drive — this protects against deletion, not drive failure",
    });
  }
  if (!job.volume) {
    problems.push({ tone: "warn", text: "Destination drive is not available right now" });
  }

  return (
    <article
      className="rounded-[var(--radius-card)] border p-5"
      style={{ background: "var(--c-surface)" }}
    >
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="text-[15px] font-semibold">{job.jobName}</h3>
        <span className="text-[12px] tabular-nums" style={{ color: "var(--c-text-faint)" }}>
          {job.snapshotCount > 0
            ? `${job.snapshotCount} snapshot${job.snapshotCount === 1 ? "" : "s"}`
            : "no snapshots"}
        </span>
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
          {job.lastRun ? (
            <>
              {relativeTime(job.lastRun.timestamp)}
              <span style={{ color: "var(--c-text-faint)" }}>
                {" · "}
                {job.lastRun.filesCopied.toLocaleString()} copied
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
        disabled={job.missingSources.length > 0 || !job.volume}
        className="mt-5 w-full rounded-md px-4 py-2 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
        style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
        title={
          job.missingSources.length > 0
            ? "Fix the missing source folders first"
            : !job.volume
              ? "The destination drive is not available"
              : undefined
        }
      >
        Back up now
      </button>
    </article>
  );
}

function Placeholder({ title }: { title: string }) {
  return (
    <div
      className="flex h-full items-center justify-center rounded-[var(--radius-card)] border"
      style={{ background: "var(--c-surface)" }}
    >
      <p style={{ color: "var(--c-text-faint)" }}>{title} — coming in the next phase.</p>
    </div>
  );
}

export default function App() {
  const [view, setView] = useState<View>("home");
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState<{ title: string; start: () => Promise<unknown> } | null>(
    null,
  );

  const refresh = () =>
    invoke<AppStatus>("get_status")
      .then(setStatus)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    refresh();
  }, []);

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
          {status ? `v${status.version} · ${status.engine}` : ""}
        </div>
      </nav>

      <main className="flex-1 overflow-y-auto px-8 py-7">
        {running && (
          <RunPanel
            title={running.title}
            start={running.start}
            onFinished={() => {
              setRunning(null);
              refresh();
            }}
          />
        )}

        {!running && error && (
          <Banner tone="danger" title="Could not load status">
            <span className="selectable font-mono text-[12px]">{error}</span>
          </Banner>
        )}

        {!running && !status && !error && (
          <p style={{ color: "var(--c-text-faint)" }}>Loading…</p>
        )}

        {!running && status && view === "home" && (
          <div className="mx-auto flex max-w-[860px] flex-col gap-5">
            <StatusHero status={status} />

            {status.importNotes.length > 0 && (
              <Banner tone="warn" title="Imported from your PowerShell setup">
                <ul className="space-y-1">
                  {status.importNotes.map((n, i) => (
                    <li key={i}>{n}</li>
                  ))}
                </ul>
              </Banner>
            )}

            {status.jobs.length === 0 ? (
              <Banner tone="info" title="No backup jobs yet">
                Add a job to choose what gets backed up and where it goes.
              </Banner>
            ) : (
              <div className="grid gap-4 md:grid-cols-2">
                {status.jobs.map((j) => (
                  <JobCard
                    key={j.jobId}
                    job={j}
                    onRun={(job) =>
                      setRunning({
                        title: job.jobName,
                        start: () => invoke("start_run", { jobId: job.jobId }),
                      })
                    }
                  />
                ))}
              </div>
            )}
          </div>
        )}

        {!running && status && view === "jobs" && (
          <JobsView onRunStarted={(title, start) => setRunning({ title, start })} />
        )}

        {!running && status && view === "snapshots" && (
          <SnapshotsView
            jobs={status.jobs}
            onRestoreStarted={(title, start) => setRunning({ title, start })}
          />
        )}

        {!running && status && view !== "home" && view !== "jobs" && view !== "snapshots" && (
          <Placeholder title={NAV.find((n) => n.id === view)!.label} />
        )}
      </main>
    </div>
  );
}
