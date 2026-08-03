//! System / power menu logic: tray icon + Shutdown / Restart / Sleep / Wake.
//!
//! "Sleep" hides the main window to the tray and pauses the aurora canvas
//! (the app's dominant idle CPU/GPU cost) while keeping the model, memory, and
//! schema engines warm in RAM/VRAM: the "barely noticeable" state. The render
//! loop is what makes sleep cheap, so we emit `canvas-pause` / `canvas-resume`
//! to the frontend which gates `requestAnimationFrame`.
//!
//! All four actions are exposed as `#[tauri::command]`s invoked from the paw
//! dropdown in `index.html`. The tray icon's double-click and its menu "Wake"
//! / "Quit" items route through the same power_wake / power_shutdown paths.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, Runtime,
};

/// Menu item IDs (compared as `&str` against `event.id().as_ref()`).
pub const TRAY_WAKE: &str = "wupi_wake";
pub const TRAY_QUIT: &str = "wupi_quit";

const MAIN_WINDOW: &str = "main";
const EVT_CANVAS_PAUSE: &str = "canvas-pause";
const EVT_CANVAS_RESUME: &str = "canvas-resume";

/// Build + install the system-tray icon. Called once from `setup()`.
///
/// The icon reuses the paw asset bundled into the binary via
/// `tauri::generate_context!`. The menu offers "Wake" (restore the window)
/// and "Quit" (full shutdown); a double-click on the icon itself also wakes.
pub fn build_tray<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let wake = MenuItem::with_id(app, TRAY_WAKE, "Wake", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_QUIT, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&wake, &quit])?;

    // The icon: prefer the bundled paw PNG (32x32) shipped as a Tauri icon.
    // Fall back to no explicit icon if it can't be resolved: the tray still
    // works, just with the platform default.
    let icon = app
        .default_window_icon()
        .cloned();

    let mut builder = TrayIconBuilder::with_id("wupi-tray")
        .tooltip("WUPI")
        .menu(&menu)
        .on_tray_icon_event(move |tray, event| {
            // Double-click (left button) wakes the app from sleep / brings it
            // forward. Single-click is left to the OS default (show menu).
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                power_wake(tray.app_handle());
            }
        });
    if let Some(ic) = icon {
        builder = builder.icon(ic);
    }
    builder.build(app)?;
    Ok(())
}


/// Best-effort teardown of the system-tray icon. Sends `NIM_DELETE` to the
/// Windows shell (via the platform `Drop`) so the tray paw is removed before
/// the process exits. Idempotent + non-fatal.
///
/// `remove_tray_by_id` is the correct Tauri 2 API: it takes the icon out of
/// Tauri's internal state AND calls `icon.close()` (the platform-level
/// teardown — `Shell_NotifyIcon(NIM_DELETE)` on Windows, run synchronously
/// inside the tray-icon `Drop`). The returned icon drops at the end of this
/// fn, finalizing the cleanup. A missing icon (None return) is the normal
/// case on a second call, just swallowed silently.
///
/// Called explicitly from `power_shutdown` BEFORE `app.exit(0)` so the
/// `NIM_DELETE` is sent while the event loop is still pumping — the shell's
/// UI thread (in explorer.exe) reconciles the deletion asynchronously, and
/// the graceful exit gives it the message-pump time the old hard-kill
/// (`std::process::exit`) starved. See `power_shutdown` for the full history.
pub fn destroy_tray<R: Runtime>(app: &AppHandle<R>) {
    // Returns Option<TrayIcon>; dropping it finalizes the platform cleanup.
    // No error path — `remove_tray_by_id` returns None if the icon doesn't
    // exist (already destroyed, never built), which is fine for cleanup.
    drop(app.remove_tray_by_id("wupi-tray"));
}

/// Full shutdown: terminate the process gracefully. We call `app.exit(0)`:
/// Tauri's graceful exit flow, which keeps the event loop pumping during
/// teardown so the OS-level tray teardown (the `NIM_DELETE` sent by
/// `destroy_tray` above) is fully serviced by the Windows shell BEFORE the
/// process goes away.
///
/// This replaced `std::process::exit(0)` (the immediate OS-level kill). The
/// hard kill was originally chosen because `app.exit`'s window/webview
/// teardown could stall when a secondary window was wedged — but WUPI is
/// single-window, so that stall path no longer exists, and the hard kill had
/// a real cost: `process::exit` skips the live message pump, so this process
/// frequently died before `explorer.exe` (a SEPARATE process hosting the
/// tray) reconciled the `NIM_DELETE` on its UI thread. Result: a "ghost" paw
/// icon cached in the hidden-icons overflow popover until the user hovered it
/// (the well-known Windows shell caching quirk). The 200ms sleep that used to
/// live here was a losing race against that; graceful exit fixes it properly
/// because the loop pumps messages until teardown completes.
///
/// `destroy_tray` is still called explicitly first so the `NIM_DELETE` is
/// sent deterministically while we know the loop is alive (not relying on
/// Tauri's internal tray Drop ordering). The belt-and-suspenders
/// `RunEvent::ExitRequested → destroy_tray` in lib.rs is idempotent (a second
/// `remove_tray_by_id` returns None) so there's no double-free risk.
pub fn power_shutdown<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(EVT_CANVAS_PAUSE, ());
    destroy_tray(app);
    app.exit(0);
}

/// Restart: spawn a fresh copy of this executable, then shut down.
///
/// `current_exe()` is the canonical way to re-launch; detached spawn so the
/// new process survives this one's exit.
pub fn power_restart<R: Runtime>(app: &AppHandle<R>) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "restart: could not resolve current_exe");
            return;
        }
    };
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => tracing::info!("restart: spawned new instance, shutting down"),
        Err(e) => {
            tracing::error!(error = %e, exe = %exe.display(), "restart: spawn failed");
            // If we can't relaunch, do NOT shut down: that would leave the
            // user with nothing. Surface the failure and stay alive.
            return;
        }
    }
    power_shutdown(app);
}

/// Sleep: hide the main window to the tray and pause the canvas. Engines
/// stay warm. The window leaves the taskbar entirely (hidden, not minimized).
pub fn power_sleep<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(EVT_CANVAS_PAUSE, ());
    if let Some(win) = app.get_webview_window(MAIN_WINDOW) {
        let _ = win.hide();
    }
}

/// Wake: restore + focus the main window and resume the canvas.
pub fn power_wake<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window(MAIN_WINDOW) {
        let _ = win.show();
        let _ = win.set_focus();
    }
    let _ = app.emit(EVT_CANVAS_RESUME, ());
}


#[tauri::command]
pub fn power_shutdown_cmd<R: Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    // Defer the actual shutdown so the IPC call returns to the frontend first;
    // otherwise the window can tear down before the reply is ack'd, which on
    // some WebView2 builds logs a harmless-but-ugly disconnect warning.
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        power_shutdown(&app2);
    });
    Ok(())
}

#[tauri::command]
pub fn power_restart_cmd<R: Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    power_restart(&app);
    Ok(())
}

#[tauri::command]
pub fn power_sleep_cmd<R: Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    power_sleep(&app);
    Ok(())
}

/// Toggle the main window's always-on-top state at runtime.
///
/// As of 2026-07-23 the window boots with `alwaysOnTop: false`
/// (tauri.conf.json): WUPI is a normal OS window the user can alt-tab away
/// from freely — the app-lifecycle onPause/onResume framework freezes Fable's
/// GPU/audio on focus loss so there's no cost to leaving WUPI unfocused, and
/// forcing on-top would block that alt-tab workflow. The command + IPC are
/// RETAINED as a runtime hook for a future "kiosk mode" (or any other use
/// case that needs to flip on-top after boot). Today NOTHING calls it: the
/// old first-run download overlay callers in script.js were stale (from when
/// the config default was `true`) and trapped the window on-top for the whole
/// session after the first download — removed 2026-07-31.
///
/// Custom `#[tauri::command]` (not the Tauri built-in window plugin command):
/// `core:default` in capabilities/default.json auto-allows custom commands,
/// so no capability change needed. Mirrors `power_sleep_cmd` above.
#[tauri::command]
pub fn set_always_on_top<R: Runtime>(app: tauri::AppHandle<R>, on: bool) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(MAIN_WINDOW) {
        win.set_always_on_top(on).map_err(|e| e.to_string())?;
    }
    Ok(())
}
