//! Tauri app wiring: managed state, command surface, native menu + event
//! forwarding, the poll-loop startup, and deterministic teardown on exit.

mod commands;
mod hub;
mod menu;
mod project;
mod pty;
mod snapshot;
mod state;

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
            | "close_session"
            | "rename"
            | "messages"
            | "next"
            | "prev"
    ) || id.starts_with("focus_")
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
            commands::open_project,
            commands::close_project,
            commands::switch_project,
            commands::attach_session,
            commands::create_session,
            commands::close_session,
            commands::rename_session,
            commands::send_bytes,
            commands::resize_session,
            commands::focus_session,
            commands::get_hub_snapshot,
        ])
        .setup(|app| {
            // Restore every project that was open when Mulpex last quit, each
            // spawning its sessions (--resume). Output buffers pre-attach, so the
            // frontend's `bootstrap` sees them ready and they paint on first frame.
            {
                let state = app.state::<AppState>();
                let helper = state.helper_path.clone();
                let mut ws = state.ws.lock().unwrap();
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
        RunEvent::ExitRequested { .. } => {
            teardown(app);
        }
        _ => {}
    });
}
