import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

/**
 * L12: a render error anywhere in the tree must not leave a white, dead window -- a
 * desktop app's worst failure mode, because it looks like the app simply isn't running.
 * Show what happened and offer the one action that always works.
 */
class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { message: string | null }
> {
  state: { message: string | null } = { message: null };

  static getDerivedStateFromError(e: unknown) {
    return { message: e instanceof Error ? e.message : String(e) };
  }

  componentDidCatch(e: unknown, info: React.ErrorInfo) {
    console.error("BackStar render error:", e, info.componentStack);
  }

  render() {
    if (this.state.message !== null) {
      return (
        <div
          style={{
            display: "flex",
            height: "100%",
            alignItems: "center",
            justifyContent: "center",
            padding: 32,
          }}
        >
          <div
            style={{
              maxWidth: 560,
              borderRadius: "var(--radius-card)",
              border: "1px solid color-mix(in srgb, var(--c-danger) 35%, var(--c-border))",
              background: "var(--c-surface)",
              padding: 28,
            }}
            role="alert"
          >
            <h1 style={{ fontSize: 18, fontWeight: 600 }}>BackStar hit an unexpected error</h1>
            <p
              className="selectable"
              style={{
                marginTop: 8,
                fontFamily: "var(--font-mono)",
                fontSize: 12,
                color: "var(--c-text-dim)",
              }}
            >
              {this.state.message}
            </p>
            <p style={{ marginTop: 8, fontSize: 13, color: "var(--c-text-dim)" }}>
              Your backups on disk are unaffected. Reloading is safe, even mid-run: the run
              continues in the background.
            </p>
            <button
              onClick={() => window.location.reload()}
              style={{
                marginTop: 16,
                borderRadius: 6,
                padding: "8px 16px",
                fontSize: 13,
                fontWeight: 500,
                background: "var(--c-accent)",
                color: "var(--c-on-accent)",
              }}
            >
              Reload
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
