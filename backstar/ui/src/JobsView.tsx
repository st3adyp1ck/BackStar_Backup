import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import type { AppStatus, Config, Job, JobStatus } from "./types";
import { JobEditor } from "./JobEditor";
import { MirrorConfirm } from "./MirrorConfirm";
import { relativeTime } from "./App";

function JobRow({
  job,
  status,
  onEdit,
  onDelete,
  onRun,
}: {
  job: Job;
  /** The backend-computed truth about this job (F56); undefined while status loads. */
  status: JobStatus | undefined;
  onEdit: () => void;
  onDelete: () => void;
  onRun: () => void;
}) {
  const problems: { tone: "warn" | "danger"; text: string }[] = [];

  if (status && status.missingSources.length > 0) {
    const shown = status.missingSources.slice(0, 3).join(", ");
    const more =
      status.missingSources.length > 3 ? ` and ${status.missingSources.length - 3} more` : "";
    problems.push({
      tone: "danger",
      text: `${status.missingSources.length} source${status.missingSources.length === 1 ? "" : "s"} missing: ${shown}${more} — ${status.missingSources.length === 1 ? "it" : "they"} will be skipped until fixed`,
    });
  }
  if (status?.scheduledMissed) {
    problems.push({
      tone: "warn",
      text: "A scheduled run was missed — the computer was probably off at the scheduled time",
    });
  }

  // Same honesty rule as Home's JobCard: the latest ATTEMPT is reported separately from
  // the latest run that actually protected something, so a failure is never hidden
  // behind an older success.
  const attempt = status?.lastAttempt;
  const attemptFailed = attempt != null && !attempt.outcome.startsWith("OK");
  const attemptIsNewest =
    attemptFailed &&
    (!status?.lastRun || Date.parse(attempt.timestamp) >= Date.parse(status.lastRun.timestamp));

  return (
    <article
      className="flex items-center justify-between gap-3 rounded-[var(--radius-card)] border p-4"
      style={{ background: "var(--c-surface)" }}
    >
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h3 className="truncate text-[14px] font-semibold">{job.name}</h3>
          <span
            className="rounded px-1.5 py-0.5 text-[10.5px] font-semibold uppercase tracking-[0.06em]"
            style={{
              background:
                job.kind === "mirror" ? "color-mix(in srgb, var(--c-danger) 16%, transparent)" : "color-mix(in srgb, var(--c-accent) 16%, transparent)",
              color: job.kind === "mirror" ? "var(--c-danger)" : "var(--c-accent)",
            }}
          >
            {job.kind}
          </span>
          {!job.enabled && (
            <span className="text-[11px]" style={{ color: "var(--c-text-faint)" }}>
              disabled
            </span>
          )}
        </div>
        <p className="selectable truncate font-mono text-[12px]" style={{ color: "var(--c-text-faint)" }}>
          {job.sources.length} folder{job.sources.length === 1 ? "" : "s"}
          {job.presets.length > 0 && `, ${job.presets.length} preset${job.presets.length === 1 ? "" : "s"}`}
          {" → "}
          {job.dest}
        </p>
        {job.schedule && (
          <p className="text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
            Runs daily at {String(job.schedule.hour).padStart(2, "0")}:
            {String(job.schedule.minute).padStart(2, "0")}
          </p>
        )}
        {attemptIsNewest && attempt ? (
          <p className="text-[11.5px]" style={{ color: "var(--c-warn)" }}>
            {attempt.outcome.startsWith("Cancelled") ? "Last run cancelled" : "Last run failed"}
            {" · "}
            {relativeTime(attempt.timestamp)}
          </p>
        ) : status?.lastRun ? (
          <p className="text-[11.5px]" style={{ color: "var(--c-text-faint)" }}>
            Last {job.kind === "mirror" ? "synced" : "backed up"} {relativeTime(status.lastRun.timestamp)}
          </p>
        ) : null}
        {problems.length > 0 && (
          <ul className="mt-1.5 space-y-1">
            {problems.map((p, i) => (
              <li
                key={i}
                className="text-[12px] leading-snug"
                style={{ color: p.tone === "danger" ? "var(--c-danger)" : "var(--c-warn)" }}
              >
                {p.text}
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="flex shrink-0 gap-2">
        <button
          onClick={onEdit}
          className="rounded-md px-2.5 py-1.5 text-[12.5px] font-medium"
          style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
        >
          Edit
        </button>
        <button
          onClick={onDelete}
          className="rounded-md px-2.5 py-1.5 text-[12.5px] font-medium"
          style={{ background: "var(--c-surface-2)", color: "var(--c-danger)" }}
        >
          Delete
        </button>
        <button
          onClick={onRun}
          disabled={!job.enabled}
          title={!job.enabled ? "This job is disabled" : undefined}
          className="rounded-md px-3 py-1.5 text-[12.5px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{
            background: job.kind === "mirror" ? "var(--c-danger)" : "var(--c-accent)",
            color: job.kind === "mirror" ? "var(--c-on-danger)" : "var(--c-on-accent)",
          }}
        >
          {job.kind === "mirror" ? "Sync now…" : "Back up now"}
        </button>
      </div>
    </article>
  );
}

export function JobsView({
  onRunStarted,
}: {
  onRunStarted: (title: string, start: () => Promise<unknown>) => void;
}) {
  const [jobs, setJobs] = useState<Job[] | null>(null);
  const [statusById, setStatusById] = useState<Map<string, JobStatus> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Job | null | "new">(null);
  const [confirmingMirror, setConfirmingMirror] = useState<Job | null>(null);

  // F56: rows are driven by the config (what is editable) MERGED with get_status (what is
  // true -- missing sources, last attempt, missed schedules), keyed by job id.
  function refresh() {
    setError(null);
    Promise.all([invoke<Config>("get_config"), invoke<AppStatus>("get_status")])
      .then(([cfg, status]) => {
        setJobs(cfg.jobs);
        setStatusById(new Map(status.jobs.map((j) => [j.jobId, j])));
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }

  // L18: wrap so the effect returns nothing, not a promise.
  useEffect(() => {
    refresh();
  }, []);

  async function deleteJob(job: Job) {
    // L10: the registered dialog plugin, not window.confirm.
    const yes = await ask(`Delete "${job.name}"? This does not touch any backups already made.`, {
      title: "Delete job",
      kind: "warning",
    });
    if (!yes) return;
    try {
      await invoke("delete_job", { jobId: job.id });
      refresh();
    } catch (e) {
      setError(String(e));
    }
  }

  function runJob(job: Job) {
    if (job.kind === "mirror") {
      // A mirror deletes at the destination; the preview-confirm dialog is the only path
      // in, and the backend refuses the run without a fresh preview + confirmMirror.
      setConfirmingMirror(job);
      return;
    }
    onRunStarted(job.name, () => invoke("start_run", { jobId: job.id, confirmMirror: false }));
  }

  if (editing !== null) {
    return (
      <JobEditor
        initial={editing === "new" ? null : editing}
        existingJobs={(jobs ?? []).map((j) => ({ id: j.id, name: j.name }))}
        onCancel={() => setEditing(null)}
        onSaved={() => {
          setEditing(null);
          refresh();
        }}
      />
    );
  }

  if (confirmingMirror) {
    return (
      <div className="mx-auto max-w-[640px]">
        <MirrorConfirm
          jobId={confirmingMirror.id}
          jobName={confirmingMirror.name}
          onCancel={() => setConfirmingMirror(null)}
          onConfirm={() => {
            const job = confirmingMirror;
            setConfirmingMirror(null);
            onRunStarted(job.name, () =>
              invoke("start_run", { jobId: job.id, confirmMirror: true }),
            );
          }}
        />
      </div>
    );
  }

  return (
    <div className="mx-auto flex max-w-[860px] flex-col gap-4">
      <div className="flex items-center justify-between">
        <h1 className="text-[18px] font-semibold tracking-[-0.01em]">Jobs</h1>
        <button
          onClick={() => setEditing("new")}
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
          style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
        >
          + New job
        </button>
      </div>

      {error && (
        <p className="text-[13px]" style={{ color: "var(--c-danger)" }} role="alert">
          {error}
        </p>
      )}

      {jobs === null && !error && (
        <p style={{ color: "var(--c-text-faint)" }}>Loading…</p>
      )}

      {jobs?.length === 0 && (
        <div
          className="rounded-[var(--radius-card)] border p-6 text-center"
          style={{ background: "var(--c-surface)", color: "var(--c-text-faint)" }}
        >
          No jobs yet. Add one to choose what gets backed up and where it goes.
        </div>
      )}

      <div className="flex flex-col gap-3">
        {jobs?.map((j) => (
          <JobRow
            key={j.id}
            job={j}
            status={statusById?.get(j.id)}
            onEdit={() => setEditing(j)}
            onDelete={() => deleteJob(j)}
            onRun={() => runJob(j)}
          />
        ))}
      </div>
    </div>
  );
}
