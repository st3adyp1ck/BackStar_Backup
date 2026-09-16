import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Config, Job, MirrorPreview } from "./types";
import { human } from "./RunPanel";
import { JobEditor } from "./JobEditor";

/**
 * The confirmation step for a mirror ("Live Sync") run: real add/update/delete counts,
 * fetched from `preview_mirror` (which walks both trees for real -- nothing here is
 * invented). Matches the destructive-confirm discipline the PowerShell app already got
 * right and this rebuild preserves deliberately: for a destructive action, "No" -- Cancel
 * -- is both the visually secondary choice AND the one a stray Enter/Space should land on,
 * never "Sync anyway".
 */
function MirrorConfirm({
  job,
  onCancel,
  onConfirm,
}: {
  job: Job;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [preview, setPreview] = useState<MirrorPreview | null | "error">(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<MirrorPreview>("preview_mirror", { jobId: job.id })
      .then(setPreview)
      .catch((e) => {
        setError(String(e));
        setPreview("error");
      });
  }, [job.id]);

  return (
    <div
      className="rounded-[var(--radius-card)] border p-5"
      style={{
        background: "color-mix(in srgb, var(--c-danger) 7%, var(--c-surface))",
        borderColor: "color-mix(in srgb, var(--c-danger) 35%, var(--c-border))",
      }}
    >
      <h3 className="text-[14px] font-semibold" style={{ color: "var(--c-danger)" }}>
        Confirm sync: {job.name}
      </h3>
      <p className="mt-1.5 text-[13px]" style={{ color: "var(--c-text-dim)" }}>
        This makes the destination match the source exactly. Files at the destination that
        no longer exist in the source will be <strong>permanently deleted</strong>.
      </p>

      <div className="mt-3 text-[13px]">
        {preview === null && <span style={{ color: "var(--c-text-faint)" }}>Checking what would change…</span>}
        {preview === "error" && (
          <span style={{ color: "var(--c-danger)" }}>Could not preview this sync: {error}</span>
        )}
        {preview && preview !== "error" && (
          <ul className="flex flex-col gap-0.5">
            <li>
              <strong>{preview.toAddOrUpdate.toLocaleString()}</strong> file(s) added or updated
              ({human(preview.bytesToWrite)})
            </li>
            <li style={{ color: preview.toDelete > 0 ? "var(--c-danger)" : undefined }}>
              <strong>{preview.toDelete.toLocaleString()}</strong> file(s) permanently deleted
            </li>
          </ul>
        )}
      </div>

      <div className="mt-4 flex gap-2">
        <button
          onClick={onCancel}
          autoFocus
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium"
          style={{ background: "var(--c-surface-2)", color: "var(--c-text)" }}
        >
          Cancel
        </button>
        <button
          onClick={onConfirm}
          disabled={preview === null || preview === "error"}
          className="rounded-md px-3.5 py-1.5 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{ background: "var(--c-danger)", color: "var(--c-on-danger)" }}
        >
          Sync and delete
        </button>
      </div>
    </div>
  );
}

function JobRow({
  job,
  onEdit,
  onDelete,
  onRun,
}: {
  job: Job;
  onEdit: () => void;
  onDelete: () => void;
  onRun: () => void;
}) {
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
          className="rounded-md px-3 py-1.5 text-[12.5px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{
            background: job.kind === "mirror" ? "var(--c-danger)" : "var(--c-accent)",
            color: job.kind === "mirror" ? "var(--c-on-danger)" : "var(--c-on-accent)",
          }}
        >
          {job.kind === "mirror" ? "Sync now" : "Back up now"}
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
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Job | null | "new">(null);
  const [confirmingMirror, setConfirmingMirror] = useState<Job | null>(null);

  function refresh() {
    invoke<Config>("get_config")
      .then((cfg) => setJobs(cfg.jobs))
      .catch((e) => setError(String(e)));
  }

  useEffect(refresh, []);

  async function deleteJob(job: Job) {
    if (!confirm(`Delete "${job.name}"? This does not touch any backups already made.`)) return;
    try {
      await invoke("delete_job", { jobId: job.id });
      refresh();
    } catch (e) {
      setError(String(e));
    }
  }

  function runJob(job: Job) {
    if (job.kind === "mirror") {
      setConfirmingMirror(job);
      return;
    }
    onRunStarted(job.name, () => invoke("start_run", { jobId: job.id }));
  }

  if (editing !== null) {
    return (
      <JobEditor
        initial={editing === "new" ? null : editing}
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
          job={confirmingMirror}
          onCancel={() => setConfirmingMirror(null)}
          onConfirm={() => {
            const job = confirmingMirror;
            setConfirmingMirror(null);
            onRunStarted(job.name, () => invoke("start_run", { jobId: job.id }));
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
        <p className="text-[13px]" style={{ color: "var(--c-danger)" }}>
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
            onEdit={() => setEditing(j)}
            onDelete={() => deleteJob(j)}
            onRun={() => runJob(j)}
          />
        ))}
      </div>
    </div>
  );
}
