//! Headless coordination core shared by the Mulpex desktop app and the
//! `mulpex-helper` binary.
//!
//! This crate carries everything that is UI-independent and was ported verbatim
//! from the original terminal-UI mulpex: the file-locking hook coordinator
//! (`hook`), the inner MCP coordination-hub server (`mcp`), per-project session
//! persistence (`persist`), the shell-terminal transcript format the app writes
//! and the helper reads (`termlog`), the workspace registry + address grammar
//! that let an instance message one in another open project (`registry`), and the
//! static `--settings` / `--mcp-config` templates (`config`). It links no
//! GUI/terminal dependencies so the helper binary the child `claude` processes
//! exec stays tiny and fast.

pub mod config;
pub mod hook;
pub mod listen;
pub mod mcp;
pub mod persist;
pub mod registry;
pub mod remote;
pub mod rules;
pub mod state_dir;
pub mod termlog;

/// Mulpex's persistent home: recents/open-project lists and the per-project
/// session stores. A **debug build uses `~/.mulpex-dev`** so `tauri dev` never
/// sees (or rewrites) the live app's projects and sessions — the two builds run
/// side by side without contaminating each other. `MULPEX_HOME` overrides both
/// ways (point a dev build at the real data, or a release build at a sandbox).
/// Release-script assets (`~/.mulpex/signing`, `updater.key`) are not read by
/// the app and deliberately stay on the literal path.
pub fn mulpex_home() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("MULPEX_HOME") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(if cfg!(debug_assertions) { ".mulpex-dev" } else { ".mulpex" })
}

/// Prefix marker on any prompt Mulpex itself injects into a `claude` session's
/// stdin (currently the hub-listener bootstrap). The `UserPromptSubmit` hook keys
/// off it to avoid overwriting the sidebar task with our plumbing text. The
/// injected prompt in `src-tauri`'s `pty.rs` must begin with this exact string.
pub const MULPEX_SENTINEL: &str = "[mulpex:hub]";

/// Prefix on the **doorbell**: the one line Mulpex types into an idle instance's
/// input box to tell it peer mail has arrived. It replaces the hub-listener
/// Monitor, which since Claude Code v2.1.271 expires after 30 minutes at most and
/// so woke every instance twice an hour purely to re-arm itself.
///
/// It is a doorbell and not a delivery: the message body stays in
/// `inbox/<id>/*.json` and is read with `hub_inbox`, exactly as when a listener
/// printed the wake. Only the trigger is typed, and it has to stay this short —
/// `claude` reads a fast burst as a paste and truncates it at one tty read
/// (measured: 1022 characters, silently). See `rules.rs`'s INCOMING MESSAGES,
/// which tells the instance what this line means; the two must not drift.
///
/// **Not to be confused with `remote.rs`'s `<<<MPX …>>>` signal marker.** That one
/// is matched in a shell terminal's *transcript*, this one at the head of a
/// *prompt*, so they cannot collide in code — but the near-identical spelling is
/// the kind of thing this repo has drifted on before, and
/// `doorbell_is_not_a_remote_signal` pins them apart.
pub const DOORBELL_PREFIX: &str = "<<<MPX>>>";

/// The doorbell line for `n` waiting messages. One spelling, in `mulpex-core`,
/// because two processes have to agree on it: `src-tauri`'s poll loop types it and
/// the `UserPromptSubmit` hook recognises it.
pub fn doorbell_line(n: usize) -> String {
    format!("{DOORBELL_PREFIX} {n} new hub message(s)")
}

/// Where an instance asks Mulpex to name its own sidebar row (`hub_set_name`):
/// one file per instance under `<state_dir>/namereq/`, holding the label. Written
/// by the helper, consumed by the app's poll loop (`Core::process_name_requests`).
pub fn name_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(NAMEREQ_DIR).join(id.to_string())
}

/// The flag that records "this row has a name, stop asking" — read by the
/// `UserPromptSubmit` hook (`hook::instance_named`), written by whichever side
/// settled it: the instance naming itself, or the app when the *user* renames the
/// row (⌘R) or a restored session comes back already named.
pub fn named_flag_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(NAMED_DIR).join(id.to_string())
}

/// Where a hook hands a turn to the Explainer: one file per instance under
/// `<state_dir>/explainreq/`. Line 1 is the session's transcript path (from the
/// payload's `transcript_path` — measured present on `Stop`, 2026-08-30, and a
/// common field of every hook event per Claude Code's hook contract); an optional
/// line 2 is a marker: `dialog` says the writer was `askq`/`plan`, so the reader
/// should wait for the pending `AskUserQuestion`/`ExitPlanMode` entry to land in
/// that transcript before summarizing; `final` says the writer was `Stop` and
/// **everything after that line** is the payload's `last_assistant_message` — the
/// text the turn ended on, which the reader waits for in the transcript (it lands
/// a beat after `Stop` fires) and falls back to appending. Written by the helper
/// (`Stop`, `askq`, `plan`), consumed-and-deleted by the app's poll loop
/// (`Core::take_explain_requests`); overwriting between polls is the latest-wins
/// coalescing.
pub fn explain_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(EXPLAINREQ_DIR).join(id.to_string())
}

/// A spawned child's task-delivery verdict: `<state_dir>/spawning/<id>`, holding
/// `pending` (created, not started yet), `failed` (never began a turn) or
/// `partial` (began a turn, but on text that is not what Mulpex sent). Absent
/// means delivered and verified. Written by the app's watchdog and by the child's
/// own `UserPromptSubmit` hook; read by `mcp::delivery_of` to answer `hub_spawn`.
pub fn spawn_delivery_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(SPAWNING_DIR).join(id.to_string())
}

/// The exact prompt Mulpex put on a spawned child's command line, so the child's
/// own hook can check what it ACTUALLY received against it.
///
/// This exists because the previous delivery mechanism failed silently. The task
/// was typed into the child's TUI, `claude` capped the paste at one tty read
/// (measured: 1022 characters, whatever the input size), and every signal said
/// success — a turn had genuinely started, just on the first kilobyte of the
/// brief. The `UserPromptSubmit` hook is the ONLY place in the system that sees
/// what the child really got, so it is the only place that can tell delivery from
/// the appearance of delivery. Delivery is argv now and cannot truncate, but the
/// check stays: it is what turns a future silent corruption into a loud one.
pub fn spawn_expected_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(SPAWNING_DIR).join(format!("{id}.expected"))
}

/// The flag that says "this child was resumed in place (⌘⇧R), so the orphaned
/// background task it is about to be told about is *expected*". Written by the app
/// in `Core::restart_instance`, consumed-and-deleted by the `UserPromptSubmit`
/// hook (`hook::take_resumed_in_place`).
///
/// It exists because the two ways a `claude` gets `--resume`d need opposite
/// answers to the same notification. An app launch builds a **fresh** `state_dir`,
/// so the inbox is empty and the wake carries nothing to act on — it is pure
/// restart noise and is blocked. ⌘⇧R reuses the **same** `state_dir` and
/// deliberately keeps the inbox while clearing `armed/<id>`, so that same wake is
/// the only thing that re-arms the listener and drains any mail waiting for it.
/// Blocking it there would leave the restarted instance unarmed and its mail
/// undelivered, with nothing anywhere to say so.
pub fn resumed_in_place_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(RESUMED_DIR).join(id.to_string())
}

/// The mark that says "the USER just submitted a real prompt to this instance":
/// `<state_dir>/userprompt/<id>`, written by the `UserPromptSubmit` hook and
/// consumed-and-deleted by the host's poll loop, which unmutes the row.
///
/// The hook is the only place that can tell a user prompt from the runtime
/// injecting a turn — a `<task-notification>` (a hub wake, a finished background
/// job) fires `UserPromptSubmit` identically, and unmuting on one would undo the
/// user's ⌘M the moment a peer messaged the instance, which is the opposite of
/// what mute is for. So the mark is written only where `system_turn` is false;
/// no reader has to re-derive that distinction.
pub fn user_prompt_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(USERPROMPT_DIR).join(id.to_string())
}

/// Why closing `claude#id` right now would interrupt work in progress, or `None`
/// if it is safely idle. The refusal `hub_close` reports when it was called
/// without `force`.
///
/// It lives here, beside the `hub_close` that asks the question, rather than in
/// the app that answers it: a policy about when it is safe to kill someone's work
/// should be stated once, where the refusal text and the rule cannot drift apart.
///
/// **The delivery mark is checked before the status, and that order is the whole
/// point.** A just-spawned instance whose task has not been typed in yet has
/// written no status file at all, and a *missing* status file reads as `waiting`
/// — the same word an idle instance gets. Going by status alone, the one moment a
/// worker is least safe to close (its brief still in flight) is indistinguishable
/// from the moment it is most safe. `spawning/<id>` is what separates them.
pub fn close_busy_reason(state_dir: &std::path::Path, id: usize) -> Option<String> {
    let delivery = std::fs::read_to_string(spawn_delivery_path(state_dir, id))
        .ok()
        .map(|s| s.trim().to_string());
    if delivery.as_deref() == Some("pending") {
        return Some(format!(
            "claude#{id} was spawned with a task that has not reached it yet — it is still \
             starting up, not idle, and closing it now would lose the brief. Wait for it to \
             begin, or pass force: true."
        ));
    }
    let status = std::fs::read_to_string(state_dir.join(id.to_string()))
        .ok()
        .map(|w| w.trim().to_string());
    if status.as_deref() == Some("working") {
        return Some(format!(
            "claude#{id} is mid-turn — closing it now would kill the task it is running. Wait \
             for it to finish (hub_instances shows when it goes idle), or pass force: true if \
             you mean to interrupt it."
        ));
    }
    None
}

/// These live here, next to `MULPEX_SENTINEL`, for the same reason: they are
/// a contract between two *processes*, so a copy in each would be a contract that
/// can silently drift out of agreement.
pub const NAMEREQ_DIR: &str = "namereq";
pub const RESUMED_DIR: &str = "resumed";
pub const SPAWNING_DIR: &str = "spawning";
pub const NAMED_DIR: &str = "named";
/// `userprompt/<id>` = the user (not the runtime) just submitted a prompt to that
/// instance. See `user_prompt_path`.
pub const USERPROMPT_DIR: &str = "userprompt";
/// `relisten/<id>` = the task ids of hub listeners the `Stop` hook found in a
/// state only it can see: running, but not refreshing `armed/<id>` (a listener
/// armed from a stale copy of the command), or simply more than one of them.
/// `UserPromptSubmit` turns it into a repair instruction and consumes it.
pub const RELISTEN_DIR: &str = "relisten";
/// `pids/<id>` = the pid of the `claude` serving that instance, written by the
/// spawner. The hub listener reads it to notice its owner died — it runs in its
/// own process group with no controlling terminal, so no signal the app sends can
/// reach it (`listen.rs`).
pub const PIDS_DIR: &str = "pids";
/// `listeners/<id>` = the pid of the hub listener currently serving that
/// instance, so a second one stands down instead of doubling every wake-up.
pub const LISTENERS_DIR: &str = "listeners";
/// `explainreq/<id>` = a turn (or a pending dialog) the hooks handed to the
/// Explainer. See `explain_request_path`.
pub const EXPLAINREQ_DIR: &str = "explainreq";
/// `watching/<id>` = that instance ended its turn holding a **watcher** — its
/// hub listener, an agentalk poll loop, anything in `watchers.txt`.
///
/// Separate from `bg/<id>` because the two facts have opposite consequences and
/// only the app can hold both: a watcher is not work in flight, so the status
/// word is `waiting` (a green, honestly-idle row), but a restart would still
/// kill the `claude` and drop whatever the watcher is attached to — a live
/// agentalk channel — so the updater's busy guard must keep counting it.
/// Written by `hook::stop`, read by `state::hub_snapshot`.
pub const WATCHING_DIR: &str = "watching";

/// `watching/<id>` for one instance.
pub fn watching_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(WATCHING_DIR).join(id.to_string())
}

/// `sessionid/<id>` = the uuid of the transcript that instance's `claude` is
/// **actually writing to**, taken from the `transcript_path` every hook payload
/// carries.
///
/// Mulpex mints a uuid, spawns with `--session-id`, and restores with
/// `--resume <that uuid>` — so the store has only ever held the id Mulpex *asked
/// for*, never the one the conversation ended up under. The two can diverge:
/// warweb#75 spent Sep 14–19 2026 in
/// `c30f48b2-ac30-4fad-8d29-4cf92cd5a7b9.jsonl` while every record after
/// 2026-09-17T17:22 was stamped `session_id: 7c1591ba-…`, the id Mulpex had
/// spawned it with and dutifully saved. The next launch resumed `7c1591ba` — a
/// file Claude Code never created — and the row came back "failed to start"
/// with a 64 MB conversation sitting intact on disk under the other name.
///
/// The filename is the declared contract; our minted id is an assumption. So
/// the hook reports the filename and the app believes it.
pub const SESSIONID_DIR: &str = "sessionid";

/// `sessionid/<id>` for one instance.
pub fn session_id_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(SESSIONID_DIR).join(id.to_string())
}

/// The conversation uuid a `transcript_path` names — its file stem.
///
/// Returns `None` for anything that is not a plain `<uuid>.jsonl`, because the
/// value's only use is as a `--resume` argument: a stem we cannot recognise is
/// worse than none, since recording it would overwrite the id that at least
/// might still work.
pub fn uuid_from_transcript_path(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    let shape = [8usize, 4, 4, 4, 12];
    let mut parts = stem.split('-');
    for want in shape {
        let part = parts.next()?;
        if part.len() != want || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one case worth pinning: a spawned instance whose task is still in
    /// flight reads `waiting` exactly like an idle one — a missing status file
    /// IS `waiting` — so the delivery mark has to be what separates them. Drop
    /// that check and `hub_close` cheerfully kills the worker it just created,
    /// a moment before its brief arrives, and reports success.
    #[test]
    fn a_spawn_still_in_flight_is_busy_even_though_it_looks_idle() {
        let dir = std::env::temp_dir().join(format!(
            "mulpex-busy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(SPAWNING_DIR)).unwrap();

        // Nothing on disk at all: never spawned with a task, never took a turn.
        assert_eq!(close_busy_reason(&dir, 1), None);

        // Spawned, task not delivered yet — and still no status file.
        std::fs::write(spawn_delivery_path(&dir, 1), "pending").unwrap();
        assert!(close_busy_reason(&dir, 1).unwrap().contains("has not reached it yet"));

        // Delivered (the mark is removed) and idle.
        std::fs::remove_file(spawn_delivery_path(&dir, 1)).unwrap();
        std::fs::write(dir.join("1"), "waiting").unwrap();
        assert_eq!(close_busy_reason(&dir, 1), None);

        // Blocked on the user is NOT busy: it is going nowhere until someone
        // answers it, which is precisely when closing it is the right call.
        std::fs::write(dir.join("1"), "needs").unwrap();
        assert_eq!(close_busy_reason(&dir, 1), None);

        std::fs::write(dir.join("1"), "working").unwrap();
        assert!(close_busy_reason(&dir, 1).unwrap().contains("mid-turn"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
