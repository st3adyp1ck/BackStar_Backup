import { useCallback, useEffect, useRef, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";

/** Where the update flow stands. `total` is null when the server sends no length. */
export type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; update: Update; downloaded: number; total: number | null }
  | { kind: "installing"; update: Update };

/** Feedback for a manual "check for updates" click; auto-checks stay silent. */
export type ManualFeedback = "up-to-date" | "failed" | null;

/**
 * The self-update flow: check GitHub Releases for a newer version, offer it, download and
 * install on demand. An automatic check runs once at startup (production builds only) and
 * fails silently -- the app must never nag or break because the network or the release
 * feed is unreachable. On Windows the installer restarts BackStar itself, so a finished
 * download means the app is about to exit.
 */
export function useAppUpdater() {
  const [phase, setPhase] = useState<UpdatePhase>({ kind: "idle" });
  const [dismissed, setDismissed] = useState(false);
  const [manualFeedback, setManualFeedback] = useState<ManualFeedback>(null);
  const [installError, setInstallError] = useState<string | null>(null);
  /** The update on offer, kept in a ref so `install` reads it without stale closures. */
  const updateRef = useRef<Update | null>(null);
  const busy = useRef(false);
  const feedbackTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const showManualFeedback = useCallback((value: Exclude<ManualFeedback, null>) => {
    setManualFeedback(value);
    if (feedbackTimer.current) clearTimeout(feedbackTimer.current);
    feedbackTimer.current = setTimeout(() => setManualFeedback(null), 5000);
  }, []);

  const checkNow = useCallback(
    async (manual: boolean) => {
      if (busy.current) return;
      busy.current = true;
      setPhase({ kind: "checking" });
      try {
        const update = await check();
        if (update) {
          updateRef.current = update;
          setDismissed(false);
          setInstallError(null);
          setPhase({ kind: "available", update });
        } else {
          updateRef.current = null;
          setPhase({ kind: "idle" });
          if (manual) showManualFeedback("up-to-date");
        }
      } catch {
        setPhase({ kind: "idle" });
        if (manual) showManualFeedback("failed");
      } finally {
        busy.current = false;
      }
    },
    [showManualFeedback],
  );

  const install = useCallback(async () => {
    const update = updateRef.current;
    if (!update || busy.current) return;
    busy.current = true;
    setInstallError(null);
    let downloaded = 0;
    let total: number | null = null;
    try {
      await update.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          total = ev.data.contentLength ?? null;
          setPhase({ kind: "downloading", update, downloaded: 0, total });
        } else if (ev.event === "Progress") {
          downloaded += ev.data.chunkLength;
          setPhase({ kind: "downloading", update, downloaded, total });
        } else {
          // Finished: the installer takes over and BackStar exits to let it.
          setPhase({ kind: "installing", update });
        }
      });
    } catch (e) {
      setInstallError(String(e));
      setPhase({ kind: "available", update });
    } finally {
      busy.current = false;
    }
  }, []);

  const dismiss = useCallback(() => setDismissed(true), []);

  useEffect(() => {
    if (!import.meta.env.DEV) void checkNow(false);
    return () => {
      if (feedbackTimer.current) clearTimeout(feedbackTimer.current);
    };
  }, [checkNow]);

  return {
    phase,
    dismissed,
    manualFeedback,
    installError,
    checking: phase.kind === "checking",
    checkNow,
    install,
    dismiss,
  };
}
