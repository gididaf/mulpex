//! Tauri app wiring: managed state, command surface, native menu + event
//! forwarding, the poll-loop startup, and deterministic teardown on exit.

mod claude_bin;
mod commands;
mod hub;
mod menu;
mod project;
mod pty;
mod snapshot;
mod state;
mod vtgrid;

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::{Emitter, Manager, RunEvent, WindowEvent};

use state::{AppState, Workspace};

/// Resolve the absolute path of the `mulpex-helper` binary, which sits beside the
/// app executable in both `tauri dev` (`target/<profile>/`) and the bundled
/// `.app` (`Contents/MacOS/`, placed there as a signed sidecar via externalBin).
fn resolve_helper_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mulpex-helper")))
        .unwrap_or_else(|| PathBuf::from("mulpex-helper"))
}

/// Forward a custom menu item id to the frontend (predefined items handle
/// themselves and aren't matched here).
fn is_forwarded(id: &str) -> bool {
    matches!(
        id,
        "open_project"
            | "close_project"
            | "next_project"
            | "prev_project"
            | "new_session"
            | "new_terminal"
            | "close_session"
            | "rename"
            | "mute"
            | "messages"
            | "minimize"
            | "next"
            | "prev"
            | "check_updates"
    ) || id.starts_with("project_")
}

/// Kill every project's session process groups and remove the scratch root.
/// Idempotent, so running it on both window-close and exit is safe.
fn teardown(app: &tauri::AppHandle) {
    app.state::<AppState>().ws.lock().unwrap().teardown_all();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            ws: Mutex::new(Workspace::new()),
            helper_path: resolve_helper_path(),
        })
        .menu(menu::build)
        .on_menu_event(|app, event| {
            let id = event.id().0.as_str();
            if is_forwarded(id) {
                let _ = app.emit("menu", id.to_string());
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::list_recent_projects,
            commands::claude_status,
            commands::open_project,
            commands::close_project,
            commands::switch_project,
            commands::reorder_projects,
            commands::attach_session,
            commands::create_session,
            commands::create_terminal,
            commands::close_session,
            commands::reorder_sessions,
            commands::rename_session,
            commands::set_session_muted,
            commands::set_mute_menu_checked,
            commands::send_bytes,
            commands::resize_session,
            commands::focus_session,
            commands::get_hub_snapshot,
            commands::restart_app,
        ])
        .setup(|app| {
            // Warm the `claude` lookup off-thread: it shells out to the user's
            // login shell (up to a 5s timeout) to reconstruct the PATH a
            // Finder-launched bundle doesn't inherit. Caching it here means the
            // first project open — and the `claude_status` probe — hit a
            // populated OnceLock instead of paying the shell spawn inline.
            std::thread::spawn(|| {
                let _ = claude_bin::resolve_claude();
            });

            // Restore every project that was open when Mulpex last quit, each
            // spawning its sessions (--resume). Output buffers pre-attach, so the
            // frontend's `bootstrap` sees them ready and they paint on first frame.
            {
                let state = app.state::<AppState>();
                let helper = state.helper_path.clone();
                let mut ws = state.ws.lock().unwrap();
                // Before creating ours, collect the scratch roots of Mulpex
                // processes that died without running teardown.
                ws.sweep_stale_state_roots();
                for dir in project::list_open() {
                    // open_or_focus dedups + skips non-dirs; a failed open is skipped.
                    let _ = ws.open_or_focus(&dir, &helper);
                }
                ws.active = ws.projects.first().map(|c| c.handle);
                // Drop any dirs that no longer exist / failed to open from the set.
                ws.persist_open();
            }
            hub::start(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error building Mulpex");

    app.run(|app, event| match event {
        // A single-window utility: closing the window quits the app (which fires
        // ExitRequested → teardown).
        RunEvent::WindowEvent {
            event: WindowEvent::CloseRequested { .. },
            ..
        } => {
            app.exit(0);
        }
        // BOTH arms are required, and neither subsumes the other. `ExitRequested`
        // is only reachable through `app.exit()` (the window-close arm above, and
        // `AppHandle::restart()`); ⌘Q and an Apple-Event `quit` go straight to
        // Cocoa's `NSApplication terminate:`, which tao turns into
        // `applicationWillTerminate:` → `Event::LoopDestroyed` → `RunEvent::Exit`.
        // Matching only `ExitRequested` meant teardown never ran on either of
        // those, leaking the whole scratch root (measured: ⌘Q and AppleScript quit
        // both left `temp/mulpex-<pid>` intact; window-close cleaned up). The
        // `claude`s still died there — but from the PTY hangup, not from our
        // `killpg`, so anything they'd backgrounded outside the foreground process
        // group was orphaned. `teardown` is idempotent; the window-close path fires
        // both events and runs it twice.
        RunEvent::ExitRequested { .. } | RunEvent::Exit => {
            teardown(app);
        }
        _ => {}
    });
}
