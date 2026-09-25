export function Banner({
  tone,
  title,
  onDismiss,
  children,
}: {
  tone: "warn" | "danger" | "info";
  title: string;
  onDismiss?: () => void;
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
      role={tone === "danger" ? "alert" : "status"}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="text-[13px] font-semibold" style={{ color: tint }}>
          {title}
        </div>
        {onDismiss && (
          <button
            onClick={onDismiss}
            aria-label="Dismiss"
            className="text-[12px] font-medium"
            style={{ color: tint }}
          >
            Dismiss
          </button>
        )}
      </div>
      <div className="mt-1 text-[13px]" style={{ color: "var(--c-text-dim)" }}>
        {children}
      </div>
    </div>
  );
}
