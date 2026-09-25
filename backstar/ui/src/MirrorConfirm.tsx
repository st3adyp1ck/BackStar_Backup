import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { MirrorPreview } from "./types";
import { human } from "./RunPanel";

/**
 * The confirmation step for a mirror ("Live Sync") run, shared by Home (App.tsx) and the
 * Jobs view: real add/update/delete counts, fetched from `preview_mirror` (which walks
 * both trees for real -- nothing here is invented). The backend stashes the preview and
 * REFUSES a mirror run that was not freshly previewed, so this dialog is not optional
 * decoration; confirming here is what unlocks `start_run({ confirmMirror: true })`.
 *
 * Matches the destructive-confirm discipline the PowerShell app already got right and this
 * rebuild preserves deliberately: for a destructive action, "No" -- Cancel -- is both the
 * visually secondary choice AND the one a stray Enter/Space should land on, never
 * "Sync and delete".
 */
export function MirrorConfirm({
  jobId,
  jobName,
  onCancel,
  onConfirm,
}: {
  jobId: string;
  jobName: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [preview, setPreview] = useState<MirrorPreview | null | "error">(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<MirrorPreview>("preview_mirror", { jobId })
      .then((p) => {
        if (!cancelled) setPreview(p);
      })
      .catch((e) => {
        if (!cancelled) {
          setError(String(e));
          setPreview("error");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [jobId]);

  return (
    <div
      className="rounded-[var(--radius-card)] border p-5"
      style={{
        background: "color-mix(in srgb, var(--c-danger) 7%, var(--c-surface))",
        borderColor: "color-mix(in srgb, var(--c-danger) 35%, var(--c-border))",
      }}
    >
      <h3 className="text-[14px] font-semibold" style={{ color: "var(--c-danger)" }}>
        Confirm sync: {jobName}
      </h3>
      <p className="mt-1.5 text-[13px]" style={{ color: "var(--c-text-dim)" }}>
        This makes the destination match the source exactly. Files at the destination that
        no longer exist in the source will be <strong>permanently deleted</strong>.
      </p>

      <div className="mt-3 text-[13px]" aria-live="polite">
        {preview === null && (
          <span style={{ color: "var(--c-text-faint)" }}>Checking what would change…</span>
        )}
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
        {/* A failed or unfinished preview means the delete count is unknown -- never
            confirm a destructive run over an invented number. */}
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
