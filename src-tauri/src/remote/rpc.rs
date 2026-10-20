//! Remote Hosts: the commands a paired remote desktop may run on this Mulpex.
//!
//! A remote desktop (another Mulpex that paired with this one — see
//! `desktop.rs`) drives this one through the very same command surface its own
//! frontend uses: the frontend's `ipc.ts` sends a remote project's call here
//! instead of to its own backend, and this table runs the same
//! `crate::commands::*` function the local webview would. Arguments arrive in
//! the frontend's camelCase, exactly as `invoke` would have passed them.
//!
//! **Every command in `lib.rs`'s `generate_handler!` is either in `ALLOWED` or
//! in `DENIED`** — `tests::every_command_is_classified` fails otherwise. That is
//! the point: a new command must be thought about once, here, or it silently
//! does nothing from a remote desktop (this codebase's allowlist trap, see the
//! root CLAUDE.md, "A real event with nowhere to arrive").

use serde::de::DeserializeOwned;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::commands as c;
use crate::state::AppState;

/// What a remote desktop may run. Everything that acts on one project carries
/// its handle (`projectHandle`) like the local call does.
pub const ALLOWED: &[&str] = &[
    "bootstrap",
    "list_recent_projects",
    "claude_status",
    "open_project",
    "close_project",
    "reorder_projects",
    "create_session",
    "create_terminal",
    "close_session",
    "restart_session",
    "reorder_sessions",
    "reparent_session",
    "rename_session",
    "set_session_muted",
    "get_pins",
    "set_pin",
    "board_get",
    "board_edit",
    "board_set_on",
    "board_update",
    "board_image_put",
    "board_image_import",
    "board_image_get",
    "set_session_collapsed",
    "set_auto_compact",
    "get_hub_snapshot",
    "save_check",
    "save_session",
    "explain_start",
    "explain_followup",
    "explain_close",
    "list_saves",
    "delete_save",
    "load_save",
    "secrets_create_ephemeral",
    "secrets_list",
    "secrets_save",
    "secrets_attach",
    "secrets_get",
    "secrets_update",
    "secrets_delete",
    "live_token_get",
    "live_token_set",
    "live_token_add_token",
    "live_token_rename_token",
    "live_token_delete_token",
    "live_token_assign",
    "live_token_claudes",
    "list_guides",
    "delete_guide",
    "load_guide",
    "import_start",
    "import_state",
    "import_discard",
    "import_apply",
    "browse_dir",
    "checkpoint_stop",
    "checkpoint_progress",
    "checkpoint_cancel",
    "checkpoint_commit",
    "checkpoint_pending",
    "checkpoint_wake",
    "checkpoint_dismiss",
    "broadcast_targets",
    "broadcast_send",
    "broadcast_progress",
    "broadcast_summarize",
    "broadcast_cancel",
    "restart_app",
    "remote_status",
    "remote_revoke_device",
];

/// Never from a remote desktop, and why:
/// - `switch_project`, `focus_session`: the remote desktop picks what IT shows;
///   this Mulpex's own window is never moved around from afar.
/// - `attach_session`, `send_bytes`, `resize_terminals`: have their own paths
///   (`sub`, `in`, the geometry lease in `desktop.rs`).
/// - `set_compact_phase`: ⌘⇧K's state machine runs in THIS frontend
///   (`compact.ts`), reached through `fa`; writing the phase from afar would
///   race it.
/// - `set_mute_menu_checked`: this Mulpex's own menu.
/// - `quit_app`: nobody could start this Mulpex again from afar. A remote
///   ⌘⇧Q ends in `restart_app` instead.
/// - `remote_set_enabled`, `remote_set_relay_url`: either one cuts the very
///   link the remote desktop is driving through. (`remote_revoke_device` is
///   allowed, but refuses the caller's own device for the same reason.)
/// - `remote_reply`, `remote_snapshot_reply`, `host_*`, `fda_*`,
///   `geometry_take_back`: this machine's own permissions, and the
///   remote-control plumbing itself.
#[cfg_attr(not(test), allow(dead_code))]
pub const DENIED: &[&str] = &[
    "switch_project",
    "focus_session",
    "attach_session",
    "send_bytes",
    "resize_terminals",
    "set_compact_phase",
    "set_mute_menu_checked",
    "quit_app",
    // A detached restart's pane snapshots come from THIS window's xterms; the
    // survival check is for this machine's own updater.
    "save_snapshots",
    "update_survives",
    "geometry_take_back",
    "remote_snapshot_reply",
    "debug_dump_enabled",
    "debug_dump",
    "debug_drive_take",
    "remote_set_enabled",
    "remote_set_relay_url",
    "remote_reply",
    "host_list",
    "host_update",
    "host_update_cancel",
    "host_update_clear",
    "host_pair",
    "host_remove",
    "host_rename",
    "host_connect",
    "host_invoke",
    "host_attach",
    "host_detach",
    "host_input",
    "host_geo",
    "host_upload",
    "host_clipboard_image",
    "host_frontend_action",
    "host_cache_layout",
    "has_full_disk_access",
    "open_full_disk_access_settings",
];

fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("bad argument `{key}`: {e}"))
}

fn out<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// How long a remote `restart_app` waits before restarting, so its answer
/// reaches the remote desktop first (it would otherwise read a dropped link
/// as a failure).
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(800);

/// Run one allowed command, as the local frontend would have. Blocking: the
/// caller runs it off the connection thread. `device` is the remote desktop
/// asking — the one thing it must never do is cut itself off.
pub fn dispatch(app: &AppHandle, cmd: &str, args: &Value, device: &str) -> Result<Value, String> {
    if !ALLOWED.contains(&cmd) {
        return Err(format!("`{cmd}` can't be run from a remote Mulpex"));
    }
    let st = || app.state::<AppState>();
    let h = || arg::<u64>(args, "projectHandle");
    let id = || arg::<usize>(args, "id");
    match cmd {
        "bootstrap" => out(c::bootstrap(st())),
        "list_recent_projects" => out(c::list_recent_projects()),
        "claude_status" => out(c::claude_status()),
        "open_project" => out(c::open_project(st(), app.clone(), arg(args, "path")?)?),
        "close_project" => out(c::close_project(st(), app.clone(), h()?)),
        "reorder_projects" => out(c::reorder_projects(st(), app.clone(), arg(args, "handles")?)),
        "create_session" => out(c::create_session(st(), h()?)?),
        "create_terminal" => out(c::create_terminal(st(), h()?)?),
        "close_session" => out(c::close_session(st(), h()?, id()?)?),
        "restart_session" => out(c::restart_session(st(), app.clone(), h()?, id()?)?),
        "reorder_sessions" => out(c::reorder_sessions(st(), h()?, arg(args, "ids")?)),
        "reparent_session" => {
            out(c::reparent_session(st(), h()?, id()?, arg(args, "parent")?, arg(args, "ids")?))
        }
        "rename_session" => out(c::rename_session(st(), h()?, id()?, arg(args, "name")?)),
        "set_session_muted" => out(c::set_session_muted(st(), h()?, id()?, arg(args, "muted")?)),
        "get_pins" => out(c::get_pins(st(), h()?)),
        "set_pin" => out(c::set_pin(st(), app.clone(), h()?, id()?, arg(args, "pin")?)),
        "board_get" => out(c::board_get(st(), h()?)),
        "board_edit" => out(c::board_edit(st(), h()?, arg(args, "edit")?)?),
        "board_set_on" => out(c::board_set_on(st(), h()?, arg(args, "on")?)),
        "board_update" => out(c::board_update(st(), h()?, arg(args, "ids")?)),
        "board_image_put" => out(c::board_image_put(
            st(),
            h()?,
            arg(args, "data")?,
            arg(args, "upload")?,
            arg(args, "done")?,
        )?),
        "board_image_import" => out(c::board_image_import(st(), h()?, arg(args, "paths")?)?),
        "board_image_get" => out(c::board_image_get(st(), h()?, arg(args, "id")?, arg(args, "offset")?)?),
        "set_session_collapsed" => {
            out(c::set_session_collapsed(st(), h()?, id()?, arg(args, "collapsed")?))
        }
        "set_auto_compact" => out(c::set_auto_compact(st(), h()?, id()?, arg(args, "pct")?)),
        "get_hub_snapshot" => out(c::get_hub_snapshot(st(), h()?)),
        "save_check" => out(c::save_check(st(), h()?, id()?)),
        "save_session" => out(c::save_session(app.clone(), st(), h()?, id()?)?),
        "explain_start" => out(c::explain_start(
            app.clone(),
            st(),
            h()?,
            id()?,
            arg(args, "reqId")?,
            arg(args, "selection")?,
        )?),
        "explain_followup" => out(c::explain_followup(
            app.clone(),
            h()?,
            id()?,
            arg(args, "reqId")?,
            arg(args, "question")?,
        )?),
        "explain_close" => out(c::explain_close(h()?, id()?)),
        "list_saves" => out(c::list_saves(st(), h()?)?),
        "delete_save" => out(c::delete_save(st(), h()?, arg(args, "file")?)?),
        "load_save" => out(c::load_save(st(), h()?, arg(args, "file")?, arg(args, "mode")?)?),
        "secrets_create_ephemeral" => {
            out(c::secrets_create_ephemeral(st(), h()?, id()?, arg(args, "rows")?)?)
        }
        "secrets_list" => out(c::secrets_list(st(), h()?)),
        "secrets_save" => out(c::secrets_save(
            st(),
            h()?,
            id()?,
            arg(args, "name")?,
            arg(args, "projectOnly")?,
            arg(args, "rows")?,
            arg(args, "send")?,
        )?),
        "secrets_attach" => out(c::secrets_attach(st(), h()?, id()?, arg(args, "name")?)?),
        "secrets_get" => out(c::secrets_get(arg(args, "name")?)?),
        "secrets_update" => out(c::secrets_update(
            st(),
            h()?,
            arg(args, "name")?,
            arg(args, "projectOnly")?,
            arg(args, "rows")?,
        )?),
        "secrets_delete" => out(c::secrets_delete(arg(args, "name")?)?),
        "live_token_get" => out(c::live_token_get()),
        "live_token_set" => {
            out(c::live_token_set(arg(args, "tokenFile")?, arg(args, "refreshCommand")?)?)
        }
        "live_token_add_token" => out(c::live_token_add_token(arg(args, "name")?, arg(args, "token")?)?),
        "live_token_rename_token" => {
            out(c::live_token_rename_token(arg(args, "tokenId")?, arg(args, "name")?)?)
        }
        "live_token_delete_token" => out(c::live_token_delete_token(arg(args, "tokenId")?)?),
        "live_token_assign" => out(c::live_token_assign(st(), h()?, id()?, arg(args, "tokenId")?)?),
        "live_token_claudes" => out(c::live_token_claudes(st())),
        "list_guides" => out(c::list_guides(st(), h()?)?),
        "delete_guide" => out(c::delete_guide(st(), h()?, arg(args, "file")?)?),
        "load_guide" => out(c::load_guide(st(), h()?, arg(args, "file")?)?),
        "import_start" => out(c::import_start(app.clone(), st(), h()?)?),
        "import_state" => out(c::import_state(h()?)),
        "import_discard" => out(c::import_discard(h()?)),
        "import_apply" => out(c::import_apply(st(), h()?, arg(args, "decisions")?)?),
        "browse_dir" => out(c::browse_dir(arg(args, "path")?)?),
        // ⌘⇧Q on a remote tab: the remote desktop runs the panel, this host
        // stops its own claudes. It always ends in a restart nobody here will
        // answer a "Wake all" for, so the checkpoint wakes by itself.
        "checkpoint_stop" => out(c::checkpoint_stop(st(), app.clone())),
        "checkpoint_progress" => out(c::checkpoint_progress(st())),
        "checkpoint_cancel" => out(c::checkpoint_cancel(st(), app.clone())),
        "checkpoint_commit" => {
            st().ws.lock().unwrap().checkpoint_commit(true).map_err(|e| e.to_string())?;
            out(())
        }
        "checkpoint_pending" => out(c::checkpoint_pending(st())),
        "checkpoint_wake" => out(c::checkpoint_wake(st(), app.clone())),
        "checkpoint_dismiss" => out(c::checkpoint_dismiss(app.clone())),
        // ⌘⇧B on a remote tab asks this host's claudes. The host keeps one
        // round, so this replaces one started from its own window (and the
        // reverse); the summary runs here, its events forwarded.
        "broadcast_targets" => out(c::broadcast_targets(st())),
        "broadcast_send" => out(c::broadcast_send(st(), arg(args, "message")?, arg(args, "targets")?)?),
        "broadcast_progress" => out(c::broadcast_progress(st())),
        "broadcast_summarize" => out(c::broadcast_summarize(app.clone(), st(), arg(args, "reqId")?)?),
        "broadcast_cancel" => out(c::broadcast_cancel(st())),
        "restart_app" => {
            let app = app.clone();
            let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);
            std::thread::spawn(move || {
                std::thread::sleep(RESTART_DELAY);
                if detach {
                    crate::ptyd::restart_detached(app);
                } else {
                    app.request_restart();
                }
            });
            out(())
        }
        "remote_status" => {
            let mut v = out(super::remote_status())?;
            v["you"] = Value::String(device.to_string());
            Ok(v)
        }
        "remote_revoke_device" => {
            let target: String = arg(args, "id")?;
            if target == device {
                return Err("That is this computer's own pairing — removing it would cut the link you are using.".into());
            }
            out(super::remote_revoke_device(target))
        }
        // In `ALLOWED` but not matched above: a bug in this file, said plainly.
        _ => Err(format!("`{cmd}` is allowed but has no dispatcher")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command names `lib.rs` registers, read from its source.
    fn registered() -> Vec<String> {
        let src = include_str!("../lib.rs");
        let start = src.find("generate_handler![").expect("generate_handler! in lib.rs");
        let body = &src[start..];
        let body = &body[..body.find("])").expect("end of generate_handler!")];
        body.lines()
            .filter_map(|l| {
                let l = l.trim().trim_end_matches(',');
                let name = l.rsplit("::").next()?;
                (!l.is_empty() && !l.starts_with("//") && l.contains("::")).then(|| name.to_string())
            })
            .collect()
    }

    #[test]
    fn every_command_is_classified() {
        let names = registered();
        assert!(names.len() > 40, "parsed the handler list: {names:?}");
        for n in &names {
            assert!(
                ALLOWED.contains(&n.as_str()) || DENIED.contains(&n.as_str()),
                "`{n}` is registered in lib.rs but neither allowed nor denied in remote/rpc.rs"
            );
        }
        for a in ALLOWED {
            assert!(!DENIED.contains(a), "`{a}` is both allowed and denied");
            assert!(names.iter().any(|n| n == a), "`{a}` is allowed but not registered");
        }
    }

    #[test]
    fn a_denied_command_is_refused_by_name() {
        for d in DENIED {
            assert!(!ALLOWED.contains(d));
        }
    }
}
