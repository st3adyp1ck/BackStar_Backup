import { Banner } from "./Banner";
import type { UpdatePhase } from "./updater";

function formatBytes(n: number): string {
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * The update offer/progress card. Only rendered when there is something to say -- an
 * available update (unless dismissed), a download in flight, or an install about to
 * restart the app.
 */
export function UpdateBanner(props: {
  phase: UpdatePhase;
  dismissed: boolean;
  installError: string | null;
  /** A backup/sync/restore run is active -- installing now would kill it mid-run. */
  busy: boolean;
  onInstall: () => void;
  onDismiss: () => void;
}) {
  const { phase, dismissed } = props;
  const visible =
    (phase.kind === "available" && !dismissed) ||
    phase.kind === "downloading" ||
    phase.kind === "installing";
  if (!visible) return null;
  return (
    <div className="mb-5">
      <UpdateBannerCard {...props} />
    </div>
  );
}

function UpdateBannerCard({
  phase,
  dismissed,
  installError,
  busy,
  onInstall,
  onDismiss,
}: {
  phase: UpdatePhase;
  dismissed: boolean;
  installError: string | null;
  /** A backup/sync/restore run is active -- installing now would kill it mid-run. */
  busy: boolean;
  onInstall: () => void;
  onDismiss: () => void;
}) {
  if (phase.kind === "available" && !dismissed) {
    const { update } = phase;
    return (
      <Banner tone="info" title={`BackStar ${update.version} is available`} onDismiss={onDismiss}>
        {update.body && (
          <p className="selectable mb-2 whitespace-pre-wrap">
            {update.body.length > 300 ? `${update.body.slice(0, 300)}…` : update.body}
          </p>
        )}
        <p>BackStar closes and restarts on the new version. Your jobs and history are untouched.</p>
        {installError && (
          <p className="mt-1" style={{ color: "var(--c-danger)" }}>
            The update failed to install: {installError}
          </p>
        )}
        <button
          onClick={onInstall}
          disabled={busy}
          className="mt-3 rounded-md px-4 py-1.5 text-[13px] font-medium disabled:cursor-not-allowed disabled:opacity-40"
          style={{ background: "var(--c-accent)", color: "var(--c-on-accent)" }}
          title={busy ? "Wait for the current run to finish" : undefined}
        >
          Update now
        </button>
        {busy && (
          <span className="ml-2 text-[12px]" style={{ color: "var(--c-text-faint)" }}>
            available once the current run finishes
          </span>
        )}
      </Banner>
    );
  }

  if (phase.kind === "downloading") {
    const pct =
      phase.total && phase.total > 0 ? Math.min(100, (phase.downloaded / phase.total) * 100) : null;
    return (
      <Banner tone="info" title={`Downloading BackStar ${phase.update.version}…`}>
        <div
          className="mt-2 h-1.5 w-full overflow-hidden rounded-full"
          style={{ background: "color-mix(in srgb, var(--c-accent) 18%, transparent)" }}
          role="progressbar"
          aria-valuenow={pct ?? undefined}
          aria-valuemin={0}
          aria-valuemax={100}
        >
          <div
            className="h-full rounded-full transition-[width]"
            style={{ width: `${pct ?? 100}%`, background: "var(--c-accent)" }}
          />
        </div>
        <p className="mt-1.5 text-[12px] tabular-nums" style={{ color: "var(--c-text-faint)" }}>
          {pct != null
            ? `${formatBytes(phase.downloaded)} of ${formatBytes(phase.total!)} (${Math.floor(pct)}%)`
            : `${formatBytes(phase.downloaded)} downloaded`}
        </p>
      </Banner>
    );
  }

  if (phase.kind === "installing") {
    return (
      <Banner tone="info" title={`Installing BackStar ${phase.update.version}`}>
        The app closes and restarts on the new version.
      </Banner>
    );
  }

  return null;
}
