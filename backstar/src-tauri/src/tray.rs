//! System tray: icon, menu, and the minimize/close-to-tray behavior.
//!
//! Two behaviors, matching (and slightly refining) the PowerShell app's own
//! `BackStar.Tray.ps1`:
//!
//! - Minimizing hides the window to the tray rather than leaving a taskbar-minimized
//!   window sitting there -- BackStar is meant to be resident, not to clutter the taskbar
//!   between runs.
//! - Closing (the title bar X) does the same, UNLESS nothing is running, in which case it
//!   exits normally. This is the one place this differs from the original, which showed a
//!   blocking "a backup is still running -- stop it and exit?" dialog on every close
//!   attempt while busy. Reproducing that exact modal from a `WindowEvent` handler is
//!   awkward in Tauri's event model; hiding to the tray instead is strictly safer (an
//!   in-flight copy is never interrupted by an accidental X click), and the tray's own
//!   "Exit" item is always available for a real, deliberate quit.
//!
//! A deliberate quit (`Exit` from the tray menu) is refused while a run is in flight
//! (D6/F42): a tray menu cannot host a modal confirm, so the honest behavior is to bring
//! up the window -- which shows the running panel with its Cancel button -- and stay
//! alive. When nothing is running, Exit exits. This replaces the pre-B2 behavior of
//! letting Exit kill the process mid-run, which was survivable for data (every write is
//! temp-file-then-rename) but the exact opposite of what a user means by tidying up.

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use crate::runner::Runner;

const OPEN_ID: &str = "open";
const EXIT_ID: &str = "exit";
const MAIN_WINDOW: &str = "main";

/// What the tray's Exit item should do, given whether a run is in flight. Split out pure
/// so the decision is unit-testable (F42).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitDecision {
    /// Nothing running: exit.
    Exit,
    /// A run is in flight: refuse to exit and show the window (which displays the running
    /// panel and its Cancel button) instead.
    ShowWindow,
}

fn exit_decision(busy: bool) -> ExitDecision {
    if busy {
        ExitDecision::ShowWindow
    } else {
        ExitDecision::Exit
    }
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let open_item = MenuItemBuilder::new("Open BackStar").id(OPEN_ID).build(app)?;
    let exit_item = MenuItemBuilder::new("Exit").id(EXIT_ID).build(app)?;
    let menu = MenuBuilder::new(app).item(&open_item).separator().item(&exit_item).build()?;

    let mut builder = TrayIconBuilder::new()
        .tooltip("BackStar")
        .menu(&menu)
        // The menu already opens on right-click by default; a left click instead restores
        // the window, matching NotifyIcon's double-click-to-restore behaviour closely
        // enough without requiring an actual double-click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().0.as_str() {
            OPEN_ID => show_main_window(app),
            EXIT_ID => {
                let busy = app
                    .try_state::<Runner>()
                    .map(|r| r.running_label().is_some())
                    .unwrap_or(false);
                match exit_decision(busy) {
                    ExitDecision::Exit => app.exit(0),
                    ExitDecision::ShowWindow => {
                        tracing::warn!(
                            "exit requested from the tray while a run is in progress -- \
                             showing the window instead of exiting"
                        );
                        show_main_window(app);
                    }
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // Reuse the window/bundle icon rather than shipping a second image asset for the tray.
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;

    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let handle = app.clone();
        window.on_window_event(move |event| match event {
            WindowEvent::Resized(_) => {
                // There is no direct "minimized" event in Tauri's WindowEvent -- minimizing
                // fires as a resize, so the state has to be read back explicitly.
                if let Some(w) = handle.get_webview_window(MAIN_WINDOW) {
                    if w.is_minimized().unwrap_or(false) {
                        let _ = w.hide();
                    }
                }
            }
            WindowEvent::CloseRequested { api, .. } => {
                let busy = handle
                    .try_state::<Runner>()
                    .map(|r| r.running_label().is_some())
                    .unwrap_or(false);
                if busy {
                    api.prevent_close();
                    if let Some(w) = handle.get_webview_window(MAIN_WINDOW) {
                        let _ = w.hide();
                    }
                }
            }
            _ => {}
        });
    }

    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F42/D6: tray Exit refuses while a run is in flight (showing the window instead)
    /// and exits when idle.
    #[test]
    fn tray_exit_refuses_while_busy_and_exits_when_idle() {
        assert_eq!(exit_decision(true), ExitDecision::ShowWindow);
        assert_eq!(exit_decision(false), ExitDecision::Exit);
    }
}
