//! Tauri app wiring: managed state, command surface, native menu + event
//! forwarding, the poll-loop startup, and deterministic teardown on exit.

mod broadcast;
mod checkpoint;
mod remote_update;
mod claude_bin;
mod commands;
mod hub;
mod live_token;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod menu;
mod project;
mod pins;
mod todos;
mod pty;
mod ptyd;
mod remote;
mod docs_import;
mod explain;
mod fda;
mod saves;
mod secrets;
mod snapshot;
mod state;
pub(crate) use mulpex_ptyd::vtgrid;
// Read by `build.rs`; the app itself only needs `env!("MULPEX_WIRE")`.
#[cfg(test)]
mod wire;

use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(target_os = "macos")]
use tauri::Emitter;
use tauri::{Manager, RunEvent, WindowEvent};

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
#[cfg(target_os = "macos")]
fn is_forwarded(id: &str) -> bool {
    matches!(
        id,
        "open_project"
            | "close_project"
            | "next_project"
            | "prev_project"
            | "move_project_left"
            | "move_project_right"
            | "new_session"
            | "new_terminal"
            | "close_session"
            | "rename"
            | "restart"
            | "save_session"
            | "load_session"
            | "import_docs"
            | "mute"
            | "messages"
            | "explain"
            | "secrets"
            | "live_token"
            | "pin_selection"
            | "toggle_panel"
            | "request_compact"
            | "ask_all"
            | "minimize"
            | "next"
            | "prev"
            | "move_instance_up"
            | "move_instance_down"
            | "check_updates"
            | "remote_control"
            | "full_disk_access"
            | "checkpoint_quit"
            | "connect_host"
            | "next_host"
            | "prev_host"
    ) || id.starts_with("project_")
}

/// Kill every project's session process groups and remove the scratch root —
/// or, on a restart that keeps them (`ptyd::restart_detached`), save what the
/// next app needs to adopt them and let go. Idempotent, so running it on both
/// window-close and exit is safe.
fn teardown(app: &tauri::AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut ws = state.ws.lock().unwrap();
        if ptyd::detaching() {
            ws.detach_all();
        } else {
            ws.teardown_all();
            // Whatever the daemon still holds ends with this quit, and so
            // does the daemon.
            ptyd::shutdown();
        }
    }
    explain::discard_all();
    broadcast::cancel_summary();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            ws: Mutex::new(Workspace::new()),
            // A stable copy, not the bundle's: claudes now outlive the app
            // (and its bundle, replaced by an update) — see `ptyd::stable_helper`.
            helper_path: ptyd::stable_helper(&resolve_helper_path()),
        });
    // The native menu is macOS-only. Linux has no global menu bar, and a GTK
    // strip in the window was declined (the webview draws its own, `MenuBar.svelte`): there the webview claims every
    // shortcut itself (`lib/linuxKeys.ts`) and feeds the same ids into the same
    // `handleMenu`, so no command has a second implementation.
    #[cfg(target_os = "macos")]
    let builder = builder.menu(menu::build).on_menu_event(|app, event| {
        let id = event.id().0.as_str();
        if is_forwarded(id) {
            let _ = app.emit("menu", id.to_string());
        }
    });
    // Linux copy/paste goes through the webview too (Super+C / Super+V), and
    // WebKitGTK's own clipboard API is not reliably readable from script.
    #[cfg(target_os = "linux")]
    let builder = builder.plugin(tauri_plugin_clipboard_manager::init());
    let app = builder
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
            commands::restart_session,
            commands::reorder_sessions,
            commands::reparent_session,
            commands::rename_session,
            commands::set_compact_phase,
            commands::set_session_muted,
            commands::get_pins,
            commands::set_pin,
            commands::todos_get,
            commands::todos_edit,
            commands::todos_update,
            commands::set_todos_enabled,
            commands::set_session_collapsed,
            commands::set_auto_compact,
            commands::set_mute_menu_checked,
            commands::send_bytes,
            commands::resize_terminals,
            commands::geometry_take_back,
            commands::browse_dir,
            commands::remote_snapshot_reply,
            commands::debug_dump_enabled,
            commands::debug_dump,
            commands::debug_drive_take,
            commands::focus_session,
            commands::get_hub_snapshot,
            commands::save_check,
            commands::save_session,
            commands::explain_start,
            commands::explain_followup,
            commands::explain_close,
            commands::list_saves,
            commands::delete_save,
            commands::load_save,
            commands::secrets_create_ephemeral,
            commands::secrets_list,
            commands::secrets_save,
            commands::secrets_attach,
            commands::secrets_get,
            commands::secrets_update,
            commands::secrets_delete,
            commands::live_token_get,
            commands::live_token_set,
            commands::live_token_add_token,
            commands::live_token_rename_token,
            commands::live_token_delete_token,
            commands::live_token_assign,
            commands::live_token_claudes,
            commands::list_guides,
            commands::delete_guide,
            commands::load_guide,
            commands::import_start,
            commands::import_state,
            commands::import_discard,
            commands::import_apply,
            commands::restart_app,
            commands::quit_app,
            commands::save_snapshots,
            commands::update_survives,
            commands::broadcast_targets,
            commands::broadcast_send,
            commands::broadcast_progress,
            commands::broadcast_summarize,
            commands::broadcast_cancel,
            commands::checkpoint_stop,
            commands::checkpoint_progress,
            commands::checkpoint_cancel,
            commands::checkpoint_commit,
            commands::checkpoint_pending,
            commands::checkpoint_wake,
            commands::checkpoint_dismiss,
            remote::remote_status,
            remote::remote_set_enabled,
            remote::remote_set_relay_url,
            remote::remote_revoke_device,
            remote::remote_reply,
            remote::client::host_list,
            remote::client::host_pair,
            remote::client::host_remove,
            remote::client::host_rename,
            remote::client::host_connect,
            remote::client::host_cache_layout,
            remote::client::host_invoke,
            remote::client::host_attach,
            remote::client::host_detach,
            remote::client::host_input,
            remote::client::host_geo,
            remote::client::host_frontend_action,
            remote::client::host_upload,
            remote::client::host_clipboard_image,
            remote::client::host_update,
            remote::client::host_update_cancel,
            remote::client::host_update_clear,
            fda::has_full_disk_access,
            fda::open_full_disk_access_settings,
        ])
        .setup(|app| {
            // Warm the `claude` lookup off-thread: it shells out to the user's
            // login shell (up to a 5s timeout) to reconstruct the environment a
            // Finder-launched bundle doesn't inherit — the PATH `claude` is found
            // on, and the rc-file exports (auth tokens above all) every session
            // needs. Caching it here means the first project open — and the
            // `claude_status` probe — hit a populated cache instead of paying the
            // shell spawn inline. The same thread then re-probes periodically, so
            // a rotating token stays live in a long-running Mulpex.
            claude_bin::warm_and_refresh();

            // Live Token (off unless configured): its proxy runs in the PTY
            // daemon (started here if it isn't running). Configured before any
            // project restores, so restored claudes are spawned pointing at it.
            live_token::init();

            // Put a commented `watchers.txt` in the home dir if there isn't one.
            // The watcher exemption works out of the box (the built-ins cover the
            // hub listener and agentalk), so this file exists purely so the
            // mechanism can be found and extended without reading the source.
            // Never overwrites — see `hook::seed_watchers_template`.
            mulpex_core::hook::seed_watchers_template();

            // Restore every project that was open when Mulpex last quit, each
            // spawning its sessions (--resume). Output buffers pre-attach, so the
            // frontend's `bootstrap` sees them ready and they paint on first frame.
            {
                let state = app.state::<AppState>();
                let helper = state.helper_path.clone();
                let mut ws = state.ws.lock().unwrap();
                // The daemon's scratch root, not one of our own: it outlives
                // this app, and every claude that survived the last restart
                // has a dir under it as its `MULPEX_STATE_DIR`.
                match ptyd::state_root() {
                    Some(root) => ws.state_root = root,
                    None => eprintln!("[ptyd] no PTY daemon — sessions cannot start"),
                }
                // Every PTY shares one size; take the one the surviving
                // sessions run at, before the frontend builds an xterm.
                ws.adopt_geometry();
                // Before creating ours, collect the scratch roots of Mulpex
                // processes that died without running teardown...
                ws.sweep_stale_state_roots();
                // ...and the hub listeners those processes left running. They are
                // not in any process group or terminal session this app can
                // signal, so nothing has ever killed them: six were found alive on
                // one machine at once, the oldest from the previous morning, each
                // still forking a `sleep` once a second. Safe before we open
                // anything, because an orphan is one whose parent is already gone.
                let reaped = crate::pty::reap_orphaned_listeners();
                if reaped > 0 {
                    eprintln!("mulpex: reaped {reaped} orphaned hub listener(s)");
                }
                for dir in project::list_open() {
                    // open_or_focus dedups + skips non-dirs; a failed open is skipped.
                    let _ = ws.open_or_focus(&dir, &helper);
                }
                ws.active = ws.projects.first().map(|c| c.handle);
                // Sessions of a project that is no longer open end now.
                ws.kill_unadopted();
                ptyd::sweep_snapshots();
                // Drop any dirs that no longer exist / failed to open from the set.
                ws.persist_open();
                // An update ordered from another Mulpex checkpointed these
                // claudes; wake them as their listeners come up, unasked.
                ws.checkpoint_auto_wake();
            }
            remote::init(app.handle().clone());
            remote::client::init(app.handle().clone());
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
