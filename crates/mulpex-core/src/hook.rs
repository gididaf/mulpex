//! The `mulpex hook` subcommand — the enforcement half of the file-locking
//! coordinator. Mulpex spawns each `claude` with `--settings` hooks that invoke
//! this same binary as `mulpex hook <event>`. The hook reads the tool-call JSON
//! on stdin and the instance identity from the environment, then implements a
//! per-file **semaphore** so two parallel instances never edit the same file at
//! once.
//!
//! - `pretooluse` fires *before* an edit runs. For Write/Edit/MultiEdit/
//!   NotebookEdit it ATOMICALLY acquires the lock for the target file (an
//!   `O_EXCL` create, the single-syscall test-and-set) before the edit happens:
//!   free or already self-held → allow; held by another instance → deny, naming
//!   the holder. For Bash (whose target file we can't know) it best-effort denies
//!   only when the command names a path another instance currently holds. On an
//!   allowed edit of a file a *different* instance changed earlier this session
//!   it injects an awareness note so the new editor reads the current state.
//! - `stop` fires when an instance finishes its turn: if it still has unread hub
//!   mail it BLOCKS the stop (the model continues and reads its inbox), so no turn
//!   ends with unhandled coordination messages; otherwise it releases every lock
//!   that instance holds (per-turn lifetime) and writes its `waiting` status word.
//! - `posttooluse` keeps the `working` status word and nudges the instance mid-turn
//!   (each deduped) to read newly-arrived hub mail, to drop coordination with a peer
//!   that has closed, and to name its own sidebar row if it still hasn't.
//!
//! Identity/coordination come from env vars set at spawn (`MULPEX_INSTANCE_ID`,
//! `MULPEX_STATE_DIR`, `MULPEX_PROJECT_DIR`), inherited by the hook process. The
//! lock table lives under `$MULPEX_STATE_DIR/locks/` (one `O_EXCL` file per
//! locked path) and the edit ledger under `history/`, keyed by an FNV-1a hash of
//! the canonical absolute path. Every decision **fails open** (allow) on any
//! error, so a coordinator bug can never wedge a Claude session.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::persist::fnv1a;

/// Hard ceiling on how long a blocked edit waits for a *continuously-hot* holder
/// before proceeding contended (allow-with-awareness, never a deny). In practice
/// the idle-lease (`LOCK_IDLE`) frees a file long before this — a waiter only
/// nears this ceiling when the holder is genuinely editing the *same* file over
/// and over for minutes, where blocking is correct. The model burns no tokens
/// while a hook blocks (it's idle awaiting the tool result), so the wait is
/// near-free. Kept well under Claude Code's PreToolUse hook timeout (a timeout
/// would *allow* the edit) — see the matcher's `timeout` in app.rs.
const LOCK_WAIT: Duration = Duration::from_secs(240);

/// Idle-lease window: a lock is held for the holder's whole turn, but its `ts` is
/// **heartbeated** every time the holder actually touches that file. A waiter
/// reclaims a lock whose `ts` is older than this — i.e. the holder acquired it
/// but has moved on to other files this turn, so there's no reason to block for
/// the rest of their turn. This makes block time track *real file activity*, not
/// turn length. If the holder later re-edits the reclaimed file from a stale
/// buffer, Claude Code's own "file modified since read" check + the HUB_RULES
/// re-read nudge self-heal it in one cycle (see term_session.rs).
const LOCK_IDLE: Duration = Duration::from_secs(30);

/// How often the waiting edit re-checks whether the lock has been released (the
/// holder's `Stop` hook deletes it) or gone idle. A small local poll, only while
/// blocked.
const LOCK_POLL: Duration = Duration::from_millis(400);

/// Entry point for `mulpex hook <event>`. Decisions are emitted to stdout; this
/// always returns `Ok` (the process then exits) — failing open on any problem.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let Some(ctx) = Ctx::from_env() else {
        return Ok(()); // no coordination context → allow silently
    };
    match args.first().map(String::as_str).unwrap_or("") {
        "pretooluse" => pretooluse(&ctx),
        "posttooluse" => posttooluse(&ctx),
        "askq" => askq(&ctx),
        "plan" => plan(&ctx),
        "stop" => stop(&ctx),
        "notification" => notification(&ctx),
        "precompact" => precompact(&ctx),
        "sessionstart" => sessionstart(&ctx),
        "userpromptsubmit" => userpromptsubmit(&ctx),
        _ => Ok(()),
    }
}

/// Per-invocation context derived from the environment. Shared by the hook
/// (`hook.rs`) and the hub MCP server (`mcp.rs`), since both key off the same
/// instance identity and on-disk state laid out under `state_dir`.
pub(crate) struct Ctx {
    pub(crate) instance: usize,
    pub(crate) state_dir: PathBuf,
    /// Canonicalized project dir; only paths inside it are coordinated.
    pub(crate) project_dir: PathBuf,
    pub(crate) locks_dir: PathBuf,
    pub(crate) history_dir: PathBuf,
    /// One line per instance: its current task (auto from prompt + hub_set_focus).
    pub(crate) tasks_dir: PathBuf,
    /// `inbox/<id>/<uuid>` message files, one dir per recipient instance.
    pub(crate) inbox_dir: PathBuf,
    /// `waiting/<id>` = "<basename>\t<holder>" while this instance is blocked
    /// waiting for a locked file (for the UI's ⏳ indicator).
    pub(crate) waiting_dir: PathBuf,
    /// `bg/<id>` exists while this instance ended a turn with background work
    /// still running — a background agent or a `run_in_background` shell. It is
    /// the only way the idle notification can tell "waiting for the user" from
    /// "waiting for its own agent"; see `notification`.
    pub(crate) bg_dir: PathBuf,
    /// `compacting/<id>` holds the compaction `trigger` ("manual"/"auto") between
    /// `PreCompact` and the `SessionStart` that ends it. Same job as `bg_dir`:
    /// compaction can run for minutes with no hook in between, so without it the
    /// 60 s idle notification lands mid-compaction and reports "needs you".
    pub(crate) compacting_dir: PathBuf,
}

impl Ctx {
    pub(crate) fn from_env() -> Option<Self> {
        let instance: usize = std::env::var("MULPEX_INSTANCE_ID").ok()?.parse().ok()?;
        let state_dir = PathBuf::from(std::env::var_os("MULPEX_STATE_DIR")?);
        let project_raw = std::env::var_os("MULPEX_PROJECT_DIR")?;
        let project_dir =
            std::fs::canonicalize(&project_raw).unwrap_or_else(|_| PathBuf::from(project_raw));
        let locks_dir = state_dir.join("locks");
        let history_dir = state_dir.join("history");
        let tasks_dir = state_dir.join("tasks");
        let inbox_dir = state_dir.join("inbox");
        let waiting_dir = state_dir.join("waiting");
        let bg_dir = state_dir.join("bg");
        let compacting_dir = state_dir.join("compacting");
        let _ = std::fs::create_dir_all(&locks_dir);
        let _ = std::fs::create_dir_all(&history_dir);
        let _ = std::fs::create_dir_all(&tasks_dir);
        let _ = std::fs::create_dir_all(&inbox_dir);
        let _ = std::fs::create_dir_all(&waiting_dir);
        let _ = std::fs::create_dir_all(&bg_dir);
        let _ = std::fs::create_dir_all(&compacting_dir);
        Some(Ctx {
            instance,
            state_dir,
            project_dir,
            locks_dir,
            history_dir,
            tasks_dir,
            inbox_dir,
            waiting_dir,
            bg_dir,
            compacting_dir,
        })
    }

    pub(crate) fn id_str(&self) -> String {
        self.instance.to_string()
    }
}

/// Handle a PreToolUse event: dispatch on the tool name.
fn pretooluse(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(());
    };
    let tool = json.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let tool_input = json.get("tool_input");

    match tool {
        "Bash" => {
            let cmd = tool_input
                .and_then(|t| t.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            bash_guard(ctx, cmd);
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            if let Some(fp) = tool_input
                .and_then(|t| t.get("file_path"))
                .and_then(|v| v.as_str())
            {
                edit_guard(ctx, fp);
            }
        }
        // Reading a file another instance is actively editing would give a STALE
        // snapshot — and Claude Code then rejects the follow-up edit with "file
        // modified since read", pre-empting our lock and causing a churn. So we
        // make the read WAIT for the holder's turn to end, then read the final
        // content, so the subsequent edit applies cleanly in one shot.
        "Read" => {
            if let Some(fp) = tool_input
                .and_then(|t| t.get("file_path"))
                .and_then(|v| v.as_str())
            {
                read_guard(ctx, fp);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Gate a Read of a file another instance is actively editing: WAIT until their
/// turn ends (lock released) so the read returns the final content, then allow.
/// A read never denies — past the budget (or if the holder is blocked on the
/// user) it simply allows, falling back to a possibly-stale read.
fn read_guard(ctx: &Ctx, file_path: &str) {
    let Some(path) = canonical_target(ctx, file_path) else {
        return;
    };
    if !path.starts_with(&ctx.project_dir) {
        return; // outside the project → uncoordinated, allow
    }
    let key = format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()));
    let lock_file = ctx.locks_dir.join(&key);
    wait_until_free(ctx, &lock_file, &path);
    // No output → allow.
}

/// Block until `lock_file` is free (or held by us), the wait budget elapses, or
/// the holder is itself blocked on the user. Used to gate reads (which never
/// acquire). Marks/clears the ⏳ waiting indicator while blocked.
fn wait_until_free(ctx: &Ctx, lock_file: &Path, path: &Path) {
    let deadline = Instant::now() + LOCK_WAIT;
    let mut marked = false;
    loop {
        match read_field(lock_file, "instance") {
            None => break,                                     // free (or gone)
            Some(owner) if owner == ctx.id_str() => break,     // ours → fine
            Some(owner) => {
                // Stop waiting once the holder is idle on this file (LOCK_IDLE),
                // stuck on the user, or the budget elapsed — then allow the read.
                if Instant::now() >= deadline
                    || holder_blocked_on_user(ctx, &owner)
                    || lock_is_stale(lock_file)
                {
                    break;
                }
                if !marked {
                    mark_waiting(ctx, path, &owner);
                    marked = true;
                }
                std::thread::sleep(LOCK_POLL);
            }
        }
    }
    if marked {
        clear_waiting(ctx);
    }
}

/// Semaphore acquire for an edit tool: allow (acquiring the lock) when the file
/// is free or already ours; deny when another instance holds it.
fn edit_guard(ctx: &Ctx, file_path: &str) {
    let Some(path) = canonical_target(ctx, file_path) else {
        return; // can't resolve → allow silently
    };
    if !path.starts_with(&ctx.project_dir) {
        return; // outside the project → uncoordinated, allow
    }

    let key = format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()));
    let lock_file = ctx.locks_dir.join(&key);
    let hist_file = ctx.history_dir.join(&key);

    // Awareness: did a *different* instance edit this earlier this session?
    let note = match read_field(&hist_file, "instance") {
        Some(prev) if prev != ctx.id_str() => Some(format!(
            "claude#{prev} modified this file earlier this session — read its current state before editing."
        )),
        _ => None,
    };

    // Acquire the lock — WAITING for the file to free rather than denying. A
    // blocked PreToolUse hook costs no model tokens (the model is idle awaiting
    // the tool result), so a same-file collision resolves itself with zero user
    // involvement: the edit proceeds once the file frees OR the holder goes idle
    // on it (`LOCK_IDLE`, reclaimed). We never deny — a holder that's stuck on the
    // user, or genuinely hot for the full budget, falls back to proceeding
    // *contended* with a stale-read awareness note instead of blocking forever.
    match acquire_or_wait(ctx, &lock_file, &path) {
        AcquireOutcome::Contended(owner) => {
            allow_contended(ctx, &path, &owner);
            return;
        }
        AcquireOutcome::Acquired => {
            // Record this edit so a later, different instance gets the note above.
            let _ = std::fs::write(
                &hist_file,
                format!("instance={}\nts={}\npath={}\n", ctx.instance, now(), path.display()),
            );
            if let Some(note) = note {
                emit("allow", None, Some(&note));
            }
            // No note → exit silently, which Claude treats as "allow".
        }
    }
}

/// Outcome of trying to acquire a file's lock (possibly after waiting).
enum AcquireOutcome {
    /// We hold the lock (freshly acquired, already ours, or a stale/stray we
    /// reclaimed). Edit proceeds cleanly with the lock held.
    Acquired,
    /// Still actively held by `<instance id>` after the full wait budget, or the
    /// holder is blocked on the user (waiting is pointless). The edit proceeds
    /// *contended* — allowed with a stale-read awareness note, never denied.
    Contended(String),
}

/// Acquire `lock_file` for this instance, **waiting** up to `LOCK_WAIT` for a
/// conflicting holder's turn to end (their `Stop` hook deletes the lock). The
/// `O_EXCL` create is the atomic test-and-set; on conflict we re-check every
/// `LOCK_POLL`. Gives up early if the holder is itself blocked on the user.
fn acquire_or_wait(ctx: &Ctx, lock_file: &Path, path: &Path) -> AcquireOutcome {
    let deadline = Instant::now() + LOCK_WAIT;
    let mut marked = false;
    let result = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_file)
        {
            Ok(mut f) => {
                let _ = write!(f, "{}", lock_token(ctx.instance, path));
                break AcquireOutcome::Acquired;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match read_field(lock_file, "instance") {
                    Some(owner) if owner == ctx.id_str() => {
                        // Already ours — heartbeat the lease so it stays "hot"
                        // while we're actively touching this file.
                        let _ = std::fs::write(lock_file, lock_token(ctx.instance, path));
                        break AcquireOutcome::Acquired;
                    }
                    Some(owner) => {
                        // Holder stuck on the user, or hot for the full budget:
                        // proceed contended rather than block forever.
                        if Instant::now() >= deadline || holder_blocked_on_user(ctx, &owner) {
                            break AcquireOutcome::Contended(owner);
                        }
                        // Idle-lease reclaim: the holder acquired this file but
                        // hasn't touched it within LOCK_IDLE — they've moved on.
                        // Drop their stale token so the next iteration's O_EXCL
                        // create claims it atomically (two racing waiters can't
                        // both win). `release_my_locks` only deletes locks still
                        // owned by `self`, so the old holder won't clobber ours.
                        if lock_is_stale(lock_file) {
                            let _ = std::fs::remove_file(lock_file);
                            continue;
                        }
                        if !marked {
                            mark_waiting(ctx, path, &owner);
                            marked = true;
                        }
                        std::thread::sleep(LOCK_POLL);
                    }
                    // Stray lock (meta unreadable, a hook died mid-acquire): take
                    // it; mulpex's reaper reclaims the entry anyway.
                    None => break AcquireOutcome::Acquired,
                }
            }
            Err(_) => break AcquireOutcome::Acquired, // fail open
        }
    };
    if marked {
        clear_waiting(ctx);
    }
    result
}

/// Whether instance `owner` is currently blocked on the user (status `needs`),
/// in which case waiting for the lock it holds would be pointless.
fn holder_blocked_on_user(ctx: &Ctx, owner: &str) -> bool {
    read_field_or_line(&ctx.state_dir.join(owner)).as_deref() == Some("needs")
}

/// Record (for the UI's ⏳ indicator) that this instance is blocked waiting on
/// `path`, held by `holder`. Body: "<basename>\t<holder>".
fn mark_waiting(ctx: &Ctx, path: &Path, holder: &str) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::fs::write(ctx.waiting_dir.join(ctx.id_str()), format!("{name}\t{holder}"));
}

fn clear_waiting(ctx: &Ctx) {
    let _ = std::fs::remove_file(ctx.waiting_dir.join(ctx.id_str()));
}

/// Best-effort Bash guard: deny only when the command text names a path that a
/// *different* instance currently holds. We can't know which file arbitrary
/// shell will touch, so builds / `npm install` / etc. pass through.
fn bash_guard(ctx: &Ctx, command: &str) {
    if command.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&ctx.locks_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.path();
        let Some(owner) = read_field(&file, "instance") else {
            continue;
        };
        if owner == ctx.id_str() {
            continue; // our own locks never block us
        }
        let Some(locked) = read_field(&file, "path") else {
            continue;
        };
        let locked_path = PathBuf::from(&locked);
        let rel = locked_path
            .strip_prefix(&ctx.project_dir)
            .ok()
            .map(|r| r.to_string_lossy().into_owned());
        let hit = command.contains(&locked)
            || rel
                .as_deref()
                .is_some_and(|r| !r.is_empty() && command.contains(r));
        if hit {
            deny_edit(ctx, &locked_path, &owner);
            return;
        }
    }
}

/// Path of the per-instance "last unread count we nudged about" marker. Used to
/// nudge once per *new* message (not on every tool call). Lives beside the inbox
/// dirs but is named `<id>.notified` (not a bare integer), so neither
/// `unread_for` (reads `inbox/<id>/`) nor `App`'s inbox scan (integer names only)
/// ever counts it.
fn notified_marker(ctx: &Ctx) -> PathBuf {
    ctx.inbox_dir.join(format!("{}.notified", ctx.instance))
}

/// Handle a Stop event: an instance must not finish its turn holding unread hub
/// mail (a peer may be coordinating a change that affects its work). If there is
/// unread mail, **block** the stop with a reason telling it to read the inbox —
/// the model then continues, calls `hub_inbox`, and clears it. Otherwise this is
/// a normal stop: release the instance's locks (per-turn) and mark it `waiting`.
fn stop(ctx: &Ctx) -> anyhow::Result<()> {
    // `stop_hook_active` is set when this Stop already fired as a result of a
    // prior Stop-block — never block twice in a row, so a model that ignores the
    // nudge can still finish (no wedge).
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload = serde_json::from_str::<serde_json::Value>(&input).ok();
    let already_continued = payload
        .as_ref()
        .and_then(|j| j.get("stop_hook_active").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    // The turn is ending, but the instance may not be: a background agent or a
    // `run_in_background` shell keeps running and will wake it with a
    // `<task-notification>` turn of its own. Recorded here because this is the
    // ONLY place the fact is available — `Stop`'s payload carries
    // `background_tasks`, and the idle notification's does not (measured; see
    // `notification`).
    let busy = background_work_running(ctx, payload.as_ref());
    set_background_flag(ctx, busy);
    // Same payload, same reason it can only be read here: this is the only hook
    // that is told which listeners are running.
    note_listener_needs_replacing(ctx, payload.as_ref());
    // A turn boundary is proof we are not mid-compaction.
    clear_compacting(ctx);

    // Per-turn locks release at *every* turn boundary — including when we block to
    // deliver mail. The continuation re-acquires (via `edit_guard`) anything it
    // actually edits, so holding them across the block would only add contention:
    // another instance could time out waiting on a lock we're no longer using.
    release_my_locks(ctx);

    let unread = crate::mcp::unread_for(ctx, ctx.instance);
    if unread > 0 && !already_continued {
        let reason = format!(
            "You have {unread} unread hub message(s) from other Mulpex instances. Call \
             mcp__mulpex__hub_inbox to read them before finishing — a peer may be \
             coordinating a change that affects your work, so handle it now."
        );
        println!("{}", serde_json::json!({ "decision": "block", "reason": reason }));
        // The turn continues, so keep the `working` status (locks already freed).
        let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "working");
        return Ok(());
    }

    // The turn is really ending; reset the nudge high-water mark to the current
    // (now-read, usually 0) count so the next message re-nudges cleanly.
    let _ = std::fs::write(notified_marker(ctx), unread.to_string());
    // Hand the finished turn to the Explainer. Placed after the mail block above
    // on purpose: a blocked stop is a continuing turn, and writing here twice
    // would explain the same turn twice. The summarizer itself must never run in
    // this hook — a Stop hook blocks the claude's turn end.
    write_explain_request(ctx, payload.as_ref(), false);
    // Preserve the sidebar status the old `printf waiting` Stop hook produced —
    // unless work this instance started is still running, in which case the turn
    // ended but the instance did not, and `waiting` (a green "ready" dot, and 60 s
    // later a red "needs you") would be a lie.
    let status = if busy { "working" } else { "waiting" };
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), status);
    Ok(())
}

/// `PreToolUse[AskUserQuestion]`: the instance stopped to ask the user
/// something. Two jobs: the `needs` status word (this handler replaced the
/// inline `printf needs` matcher and must keep doing that), and handing the
/// turn to the Explainer so the question is explained *while it sits on
/// screen* — `Stop` does not fire while a dialog waits, so without this the
/// question would only be explained after it was answered.
///
/// The questions payload itself is not forwarded: it is already in the
/// transcript by the time the dialog is on screen, and `explainer::read_turn`
/// reads it from there. What is forwarded is the transcript path plus the
/// `dialog` marker, which tells the reader to wait for that entry to land.
fn askq(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload = serde_json::from_str::<serde_json::Value>(&input).ok();
    write_needs(ctx);
    write_explain_request(ctx, payload.as_ref(), true);
    Ok(())
}

/// `PreToolUse[ExitPlanMode]`: the instance finished a plan and is about to ask
/// the user whether to execute it. Writes the `needs` status word and hands the
/// turn to the Explainer, exactly as `askq` does.
///
/// The status write is deliberately redundant: measured 2026-09-01, the
/// approval dialog also fires `Notification{permission_prompt}` ~6 s later,
/// which `notification` already turns into `needs`. Writing it here makes the
/// sidebar dot immediate and keeps it correct if that notification type ever
/// changes — the same belt-and-braces `askq` uses.
fn plan(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload = serde_json::from_str::<serde_json::Value>(&input).ok();
    write_needs(ctx);
    write_explain_request(ctx, payload.as_ref(), true);
    Ok(())
}

/// `needs` — the sidebar's "this instance is holding something up for YOU". The
/// one job the two `PreToolUse` matchers above must never lose.
///
/// Split out from them so it can be tested without stdin: the handlers read
/// stdin to EOF first, which never comes when the test runner's stdin is an
/// open pipe (measured — a test calling `askq` directly hung forever under a
/// background shell and passed from a terminal, the worst shape a test can
/// have).
fn write_needs(ctx: &Ctx) {
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "needs");
}

/// Record where this turn's transcript lives, for the Explainer. Every hook
/// payload carries `transcript_path` (measured on `Stop`, 2026-08-30; a common
/// field of every event per Claude Code's hook contract). The app's poll loop
/// consumes `explainreq/<id>` and summarizes the turn off-process. A payload
/// without the field writes nothing — that turn simply gets no explanation,
/// which is better than guessing at the transcript's location.
///
/// `dialog` is set by `askq`/`plan`: the transcript may not yet hold the
/// `tool_use` entry the hook is firing for, and a reader that summarized it as
/// a plain turn would never get a second chance (`Stop` does not fire while the
/// dialog waits). The marker is what tells `explainer::read_turn_settled` to
/// wait for the dialog rather than for prose.
fn write_explain_request(ctx: &Ctx, payload: Option<&serde_json::Value>, dialog: bool) {
    let Some(path) = payload
        .and_then(|j| j.get("transcript_path"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let file = crate::explain_request_path(&ctx.state_dir, ctx.instance);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = if dialog { format!("{path}\ndialog") } else { path.to_string() };
    let _ = std::fs::write(file, body);
}

/// Does this `Stop` payload say work the instance started is still running?
///
/// `background_tasks` covers both a background **agent** and a
/// `run_in_background` **shell** — measured shapes:
///   `{"id":…,"type":"subagent","status":"running","description":…,"agent_type":…}`
///   `{"id":…,"type":"shell","status":"running","description":…,"command":…}`
/// and the array is `[]` once everything has finished. Entries with any other
/// status are ignored; a task with no status at all counts as running, because
/// the failure that matters is claiming the instance is idle when it isn't.
///
/// `session_crons` is deliberately NOT counted. A scheduled future run is not
/// work in flight — between firings the instance genuinely is idle and a prompt
/// really is what it is waiting for.
///
/// Neither is **Mulpex's own hub listener**, for the same reason and with much
/// worse consequences: Mulpex itself tells every instance to arm one on its
/// inbox (`HUB_RULES` "INCOMING MESSAGES"), and it runs for as long as the tool
/// lets it. It arrives here as
/// `{"id":…,"type":"shell","status":"running","description":"Mulpex hub inbox","command":…}`
/// — in shape, indistinguishable from a real `run_in_background` shell. Left
/// counted, every instance in every project is `working` forever, which used to
/// suppress `needs` forever too: no red dot, no tab badge, no dock badge, no
/// banner, for anyone.
///
/// **It is recognised by its `command`, which is Mulpex's own text.** `HUB_RULES`
/// dictates that command byte-for-byte and `hook.rs` already gates the arm nudge
/// on the `touch` inside it, so matching it here is the same contract read from
/// the other end — and measured (real `claude`, `scratchpad/monprobe`,
/// 2026-09-16) the whole command reaches `Stop` verbatim, however long it is.
///
/// This replaced a two-hook handshake that recorded each **persistent** Monitor's
/// task id from `PostToolUse` and subtracted it here. That broke completely when
/// Claude Code removed persistent Monitors: `persistent` moved from `tool_input`
/// to `tool_response` **and became permanently `false`**, so nothing was ever
/// recorded and every instance stuck on yellow. Three properties make the
/// command match the better contract, not merely the working one:
///
/// - **One hook.** No `monitors/<id>` file, no ordering between `PostToolUse` and
///   `Stop`, nothing to clear at `SessionStart`, and `posttooluse` no longer
///   parses a payload on every tool call.
/// - **It is retroactive.** An instance that armed its listener before this
///   shipped goes green at its very next turn end. The id-recording version could
///   not see a Monitor armed before it, so every instance had to re-arm first.
/// - **It depends only on a string Mulpex authors.** The previous version
///   depended on an optional parameter of someone else's tool, which vanished.
fn background_work_running(_ctx: &Ctx, payload: Option<&serde_json::Value>) -> bool {
    payload
        .and_then(|j| j.get("background_tasks"))
        .and_then(|v| v.as_array())
        .is_some_and(|tasks| {
            tasks.iter().any(|t| {
                let running = t
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "running")
                    .unwrap_or(true);
                running && !is_hub_listener(t)
            })
        })
}

/// The markers that identify a hub listener inside its own command line, each
/// short enough that a hand-rolled variant still carries it and specific enough
/// that nothing else plausibly does.
///
/// There are two because there have been two listeners. The **inbox path** is
/// what every shell one-liner watches, spelled exactly as `HUB_RULES` used to
/// spell it; sessions that were already running when this shipped keep re-arming
/// that form out of their own conversation history, so it has to stay
/// recognisable indefinitely rather than until the next release. The **helper
/// subcommand** is the current form.
const LEGACY_LISTENER_MARKER: &str = "$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID";
const LISTENER_BIN: &str = "mulpex-helper";
const LISTENER_SUBCOMMAND: &str = "listen";

/// Does this command line start a hub listener?
///
/// The current form is matched on the binary **and** the subcommand rather than
/// on one literal, because the path in front of it varies per install and is
/// quoted (`"/Applications/…/mulpex-helper" listen`), while `mulpex-helper`
/// alone would also match the hook and MCP invocations.
pub fn command_is_hub_listener(command: &str) -> bool {
    command.contains(LEGACY_LISTENER_MARKER)
        || (command.contains(LISTENER_BIN) && command.contains(LISTENER_SUBCOMMAND))
}

/// Is this `background_tasks` entry the instance's own hub listener?
fn is_hub_listener(task: &serde_json::Value) -> bool {
    task.get("command")
        .and_then(|v| v.as_str())
        .is_some_and(command_is_hub_listener)
}

/// The ids of every hub listener this payload reports as running.
fn running_listener_ids(payload: Option<&serde_json::Value>) -> Vec<String> {
    payload
        .and_then(|j| j.get("background_tasks"))
        .and_then(|v| v.as_array())
        .map(|tasks| {
            tasks
                .iter()
                .filter(|t| {
                    let running = t
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "running")
                        .unwrap_or(true);
                    running && is_hub_listener(t)
                })
                .map(|t| {
                    t.get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn relisten_path(ctx: &Ctx) -> PathBuf {
    ctx.state_dir
        .join(crate::RELISTEN_DIR)
        .join(ctx.id_str())
}

/// Record, at the one hook that can see it, a listener that needs replacing.
///
/// **Only `Stop` carries `background_tasks`** — measured against a real `claude`
/// (v2.1.273, 2026-09-16): `UserPromptSubmit` does not carry it even on a prompt
/// taken 24 s after a Monitor was armed, and neither does `PostToolUse` or the
/// idle notification. `listener_armed` is asked from `UserPromptSubmit`, so it
/// can never look at the task list itself; this writes down what `Stop` saw and
/// lets the arm nudge read it back.
///
/// Two states are worth repairing, and both are invisible from every other angle:
///
/// - **A listener that runs but never refreshes `armed/<id>`.** Before the
///   in-loop `touch` existed the command only touched the flag once at startup,
///   and an instance re-arming from its own conversation history keeps emitting
///   that version long after `HUB_RULES` changed — measured on a live instance
///   that armed the superseded command 71 times over two days and only stopped
///   when `/compact` dropped it from context. The heartbeat then reads dead
///   forever, so the arm nudge fires every single turn and a *fresh* Monitor
///   stacks on each one.
/// - **More than one listener.** That is the same failure one step later, and
///   what the user actually sees: one hub message, N wake-ups.
///
/// Deliberately not a `decision: block`. The arm nudge is already firing every
/// turn in this state, so the instruction rides along on a turn that was going to
/// happen anyway; and that channel belongs to unread mail, which must not have to
/// queue behind housekeeping.
fn note_listener_needs_replacing(ctx: &Ctx, payload: Option<&serde_json::Value>) {
    let ids = running_listener_ids(payload);
    let stale_heartbeat = !ids.is_empty() && !listener_armed(ctx);
    if stale_heartbeat || ids.len() > 1 {
        let _ = std::fs::create_dir_all(ctx.state_dir.join(crate::RELISTEN_DIR));
        let _ = std::fs::write(relisten_path(ctx), ids.join(" "));
    } else {
        let _ = std::fs::remove_file(relisten_path(ctx));
    }
}

fn background_flag(ctx: &Ctx) -> PathBuf {
    ctx.bg_dir.join(ctx.id_str())
}

fn set_background_flag(ctx: &Ctx, busy: bool) {
    if busy {
        let _ = std::fs::write(background_flag(ctx), "");
    } else {
        let _ = std::fs::remove_file(background_flag(ctx));
    }
}

/// Handle a `Notification` event (matcher `permission_prompt|idle_prompt`).
///
/// **This hook can no longer produce `needs`.** Red means one thing now — the
/// instance is holding an `AskUserQuestion` or an `ExitPlanMode` plan up for an
/// answer — and those are written by `askq` and `plan` from their own
/// `PreToolUse` matchers. Everything else a notification reports is either
/// idleness (`waiting`) or work in flight (`working`), and calling either of
/// those "needs YOU" is what made the dot, the tab badge, the dock badge and the
/// desktop banner unreadable: `idle_prompt` fires 60 s after *every* turn end
/// (measured: `Stop` at 11:58:56, `Notification` at 11:59:56, to the second), so
/// the common state of a healthy instance was red.
///
/// What it still does, and why it is not deleted:
///
/// - **It never clears `needs` either.** The plan dialog fires its own
///   `permission_prompt` ~6 s after `PreToolUse[ExitPlanMode]` (measured
///   2026-09-01), so a notification that wrote `waiting` unconditionally would
///   flip the plan's red back to green while the dialog is still on screen. A
///   status already reading `needs` is left exactly as it is.
/// - **It downgrades a stale `working`.** A turn that ended without a `Stop`
///   (an interrupt) leaves `working` behind; the idle notification is the only
///   event that then says otherwise.
/// - **`busy` still wins over idleness.** The notification's own payload cannot
///   see background work — it carries only `notification_type` and `message`
///   (measured) — so the answer comes from the flag the `Stop` hook left behind,
///   which is written from the one payload that does know. Same for the
///   `PreCompact` flag: compaction fires nothing between its endpoints.
///
/// `notification_type` is therefore no longer read: both kinds get the same
/// answer, and the matcher is what limits which ones arrive.
fn notification(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), notify_status(ctx));
    Ok(())
}

/// The whole decision `notification` makes, split out so a test can drive the
/// real thing rather than a copy of it (the hook itself reads stdin, which a test
/// cannot hand it).
fn notify_status(ctx: &Ctx) -> &'static str {
    if read_field_or_line(&ctx.state_dir.join(ctx.id_str())).as_deref() == Some("needs") {
        return "needs";
    }
    let busy = background_flag(ctx).exists() || compacting_flag(ctx).exists();
    if busy {
        "working"
    } else {
        "waiting"
    }
}

fn compacting_flag(ctx: &Ctx) -> PathBuf {
    ctx.compacting_dir.join(ctx.id_str())
}

/// Compaction has started (`/compact`, or an automatic one when the context
/// fills). It can run for minutes and fires **no other hook while it runs** —
/// measured on a real session: `PreCompact` 09:24:19, the `SessionStart` that
/// ends it 09:24:53, and nothing in between. `/compact` does not even fire
/// `UserPromptSubmit` (it is a local command, not a prompt), so without this the
/// status file still says whatever the last turn left — and the 60 s idle
/// notification then overwrites it with `needs`, mid-compaction. Measured:
/// `PreCompact` 09:18:10 → `Notification{idle_prompt}` 09:19:10, to the second.
///
/// The `trigger` is kept because it decides what the END of compaction means:
/// after a manual `/compact` the instance is idle at its prompt, but an
/// automatic one happens mid-turn and the turn carries on afterwards.
fn precompact(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let trigger = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|j| {
            j.get("trigger")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "manual".into());
    let _ = std::fs::write(compacting_flag(ctx), trigger);
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "working");
    Ok(())
}

/// A session began. Only `source == "compact"` is ours: it is the event that
/// ends a compaction (the other sources — startup, resume, clear — are ordinary
/// lifecycle and must not touch a status the restore path already set).
fn sessionstart(ctx: &Ctx) -> anyhow::Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let source = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|j| j.get("source").and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_default();
    if !is_compaction_end(&source) {
        // startup / resume / clear: whatever Monitors the last session had died
        // with it. Their ids must not keep excusing this session's tasks.
        return Ok(());
    }
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), compaction_end_status(ctx));
    clear_compacting(ctx);
    Ok(())
}

/// `SessionStart` also fires for `startup`, `resume` and `clear`; only the
/// compaction one ends a compaction, and the others must leave the status alone.
fn is_compaction_end(source: &str) -> bool {
    source == "compact"
}

/// What the instance is doing the moment a compaction finishes. A manual
/// `/compact` leaves it idle at the prompt; an automatic one interrupted a turn
/// that now resumes, and reporting a green "ready" dot in the middle of that
/// turn would be the same lie in the other direction.
fn compaction_end_status(ctx: &Ctx) -> &'static str {
    match std::fs::read_to_string(compacting_flag(ctx)).as_deref().map(str::trim) {
        Ok("auto") => "working",
        _ => "waiting",
    }
}

/// Bound how long a stale flag can last. Compaction normally ends with its own
/// `SessionStart`, but a REFUSED one does not: `PreCompact` fires and then Claude
/// Code answers "Not enough messages to compact" and nothing else happens
/// (measured). Any hook that proves the instance is doing something else clears
/// it, so the worst case is one status word until the next prompt or turn end.
fn clear_compacting(ctx: &Ctx) {
    let _ = std::fs::remove_file(compacting_flag(ctx));
}

/// Release every lock currently held by this instance (per-turn lifetime).
fn release_my_locks(ctx: &Ctx) {
    if let Ok(entries) = std::fs::read_dir(&ctx.locks_dir) {
        for entry in entries.flatten() {
            let file = entry.path();
            if read_field(&file, "instance") == Some(ctx.id_str()) {
                let _ = std::fs::remove_file(&file);
            }
        }
    }
}

/// Handle a PostToolUse event: keep the sidebar status `working` (preserving the
/// old `printf` hook), and inject a mid-turn nudge (once) when (a) new hub mail
/// has arrived, (b) a peer this instance knew about has closed — so it stops
/// messaging / waiting on / deferring to an instance that's gone — or (c) this
/// instance still hasn't named its sidebar row. All three are deduped: mail via
/// the `<id>.notified` high-water mark, departures via the `peers/<id>` baseline
/// (see `departed_peers`), naming via the per-turn `namenudge/<id>` count. At most
/// one nudge is emitted per tool call (a hook can print only one decision), so the
/// notes are combined.
fn posttooluse(ctx: &Ctx) -> anyhow::Result<()> {
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "working");

    // Drained but never parsed. This hook forks on EVERY tool call and the
    // payload carries the whole `tool_response` (a Read of a big file, a long
    // Bash output), so the cheapest thing it can do with it is nothing. It used
    // to scan for `Monitor` and record persistent Monitors' task ids here; `Stop`
    // now recognises the hub listener from its own command line instead, which is
    // why there is nothing left to learn from this payload — see
    // `background_work_running`.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    drop(input);

    let mut notes: Vec<String> = Vec::new();

    // (a) New hub mail arrived mid-turn?
    let unread = crate::mcp::unread_for(ctx, ctx.instance);
    let marker = notified_marker(ctx);
    let last: usize = read_field_or_line(&marker)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if unread > last {
        notes.push(format!(
            "You have {unread} unread message(s) from other instances — call \
             mcp__mulpex__hub_inbox to read them (a peer may be coordinating a change that \
             affects your work)."
        ));
    }
    // Track the high-water mark (whether it rose or fell, e.g. after a hub_inbox
    // read cleared it) so each new message nudges exactly once.
    let _ = std::fs::write(&marker, unread.to_string());

    // (b) Did a peer this instance knew about close mid-turn?
    let departed = departed_peers(ctx);
    if !departed.is_empty() {
        notes.push(departed_nudge(&departed));
    }

    // (c) Still unnamed a few tool calls into the turn?
    if name_nudge_due(ctx) {
        notes.push(NAME_NUDGE_MIDTURN.to_string());
    }

    if !notes.is_empty() {
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": format!("[Mulpex hub] {}", notes.join("\n\n")),
            }
        });
        println!("{out}");
    }
    Ok(())
}

/// A one-line nudge naming the instances that have just closed, told to the
/// surviving instance so it drops any coordination that involved them.
fn departed_nudge(ids: &[usize]) -> String {
    let list = ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ");
    let verb = if ids.len() == 1 { "has" } else { "have" };
    format!(
        "Instance(s) claude {list} {verb} closed and are no longer running. Disregard any \
         earlier coordination, waiting, or plans that involve them — they can't reply or act, \
         and their file locks are released. Call mcp__mulpex__hub_instances if you need the \
         current instance list."
    )
}

/// This instance's "known live peers" baseline, kept in a `peers/` subdir so the
/// App's integer-named state scans (status files, `live_ids` fallback) never pick
/// it up. Diffing it against the current live peers detects a peer closing.
fn seen_peers_file(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("peers").join(ctx.id_str())
}

/// Reset this instance's known-peers baseline to the current live peers. Called
/// at prompt submit (right after the model receives a fresh peer snapshot), so a
/// mid-turn departure is measured against exactly what the model was told.
fn seed_seen_peers(ctx: &Ctx) {
    write_seen_peers(ctx, &crate::mcp::peer_ids(ctx));
}

/// Diff the stored known-peers baseline against the current live peers: return
/// the ids that have since vanished (closed), and reset the baseline to the
/// current set so each departure is nudged exactly once. New peers (spawned this
/// turn) are folded into the baseline silently — only closures are reported.
fn departed_peers(ctx: &Ctx) -> Vec<usize> {
    let prev: Vec<usize> = read_field_or_line(&seen_peers_file(ctx))
        .map(|s| s.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default();
    let current = crate::mcp::peer_ids(ctx);
    let departed: Vec<usize> = prev.into_iter().filter(|id| !current.contains(id)).collect();
    write_seen_peers(ctx, &current);
    departed
}

fn write_seen_peers(ctx: &Ctx, ids: &[usize]) {
    let file = seen_peers_file(ctx);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = ids.iter().map(usize::to_string).collect::<Vec<_>>().join(" ");
    let _ = std::fs::write(&file, body);
}

/// Rule on whether this instance's spawn task actually arrived, by comparing the
/// prompt `claude` really received against what Mulpex put on its command line.
///
/// This runs in the child, and it is the ONLY vantage point in the system with
/// that comparison available: the app knows what it sent, `claude` knows what it
/// got, and nothing else sees both. That gap is what let a truncated brief be
/// reported as delivered — the task used to be typed into the TUI, `claude` capped
/// the paste at one tty read (measured: 1022 characters regardless of input size),
/// and a turn started, so every signal the app could check said success.
///
/// Absent expectation file = not a spawned child (or already ruled on): stay out
/// of the way. Delivery is argv now and cannot truncate; this is the detector that
/// keeps the next mechanism honest.
fn verify_spawn_delivery(ctx: &Ctx, received: &str) {
    let expected_path = crate::spawn_expected_path(&ctx.state_dir, ctx.instance);
    let Ok(expected) = std::fs::read_to_string(&expected_path) else {
        return;
    };
    let verdict_path = crate::spawn_delivery_path(&ctx.state_dir, ctx.instance);
    if received.trim() == expected.trim() {
        let _ = std::fs::remove_file(&verdict_path); // delivered, and verified so
    } else {
        // Started, but not on what was sent. Distinct from `failed` (never
        // started): the instance IS working, which is precisely what makes this
        // the dangerous case — left unsaid it looks like a healthy child, and the
        // spawner would never think to check.
        let _ = std::fs::write(&verdict_path, "partial");
    }
    let _ = std::fs::remove_file(&expected_path);
}

/// Handle a UserPromptSubmit event: (a) mark this instance `working` (preserving
/// the old `printf` status hook), (b) capture the submitted prompt as this
/// instance's baseline task for the hub, and (c) inject a compact snapshot of the
/// other instances into this turn via `additionalContext`.
fn userpromptsubmit(ctx: &Ctx) -> anyhow::Result<()> {
    let status_path = ctx.state_dir.join(ctx.id_str());
    // Read before overwriting: a turn we block never runs, so no `Stop` hook will
    // fire to correct this, and the row would sit on `working` forever.
    let prior_status = std::fs::read_to_string(&status_path).ok();
    let _ = std::fs::write(&status_path, "working");
    clear_compacting(ctx);

    let mut input = String::new();
    // Whether this turn is the runtime injecting an event rather than the user
    // talking. Kept out of the parse block because the nudges below need it.
    let mut system_turn = false;
    // ...and whether it is the one system turn we *asked* for. ⌘⇧R lets the wake
    // through precisely so the nudges can ride on it, so this has to re-open the
    // gate `system_turn` closes — without it the exemption buys a turn that still
    // never re-arms, which is the whole thing it exists to prevent.
    let mut sanctioned_resume = false;
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&input) {
            // Claude Code's UserPromptSubmit payload carries the text under
            // `prompt` (NOT `userPrompt`) — see code.claude.com/docs hooks.
            if let Some(prompt) = json.get("prompt").and_then(|v| v.as_str()) {
                // The sidebar task should reflect the USER's work, so skip prompts
                // that aren't the user talking: (a) prompts Mulpex injects itself
                // (the hub-listener bootstrap, tagged with MULPEX_SENTINEL), and
                // (b) runtime-injected event turns — a Monitor wake or a background
                // job completion arrives as a synthetic `<task-notification>…`
                // prompt. Neither should overwrite the task.
                let p = prompt.trim_start();
                system_turn = p.starts_with("<task-notification");
                if p.starts_with(crate::MULPEX_SENTINEL) {
                    verify_spawn_delivery(ctx, prompt);
                } else if !system_turn {
                    let task = crate::mcp::summarize(prompt);
                    if !task.is_empty() {
                        let _ = std::fs::write(ctx.tasks_dir.join(ctx.id_str()), &task);
                    }
                    // The user is talking to this instance, so a ⌘M on it has
                    // been overtaken: the host's poll loop reads this mark and
                    // unmutes the row. It is written in this branch and nowhere
                    // else because this is the only place in the system that
                    // knows the turn is the *user's* — a `<task-notification>`
                    // (a hub wake, a finished background job) fires
                    // `UserPromptSubmit` identically, and unmuting on one would
                    // undo a ⌘M the moment a peer sent mail. Unconditional: the
                    // reader is the only side that knows whether the row is
                    // muted, and the mark is consumed either way.
                    let _ = std::fs::write(crate::user_prompt_path(&ctx.state_dir, ctx.instance), "");
                }
                // The restart wake. Swallow it unless ⌘⇧R asked for it — see
                // `orphaned_task_wake` for why this is the whole bug.
                if orphaned_task_wake(p) {
                    sanctioned_resume = take_resumed_in_place(ctx);
                    if !sanctioned_resume {
                        match prior_status {
                            Some(s) => {
                                let _ = std::fs::write(&status_path, s);
                            }
                            // Absent reads as `waiting` (`mcp::status_of`'s
                            // default), the truth about a claude that just booted.
                            None => {
                                let _ = std::fs::remove_file(&status_path);
                            }
                        }
                        println!(
                            "{}",
                            serde_json::json!({
                                "decision": "block",
                                "reason": ORPHAN_WAKE_BLOCK_REASON,
                            })
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    // Assemble this turn's injected context: a one-time reminder to arm the hub
    // listener (only until it's armed), then the peer snapshot. This replaces the
    // old visible PTY bootstrap prompt — a normal instance now starts clean and
    // arms its listener from here, invisibly, on the user's first turn.
    //
    // The two nudges are asked of the USER's turns only. Injected on a
    // `<task-notification>` they are worse than useless: the arm nudge makes the
    // instance start a fresh persistent Monitor, which is precisely the orphan
    // that produces the next restart's wake — the reminder manufacturing the
    // artifact that causes the next reminder. (Measured; see `docs/hub.md`.) The
    // peer snapshot below is NOT gated — a hub wake *is* a task-notification, and
    // it is the turn that most needs to know about unread mail.
    let welcome = nudges_welcome(system_turn, sanctioned_resume);
    let mut parts: Vec<String> = Vec::new();
    if welcome && !listener_armed(ctx) {
        parts.push(arm_listener_nudge(ctx));
    }
    if welcome && !instance_named(ctx) {
        parts.push(AUTO_NAME_NUDGE.to_string());
    }
    if let Some(context) = crate::mcp::peers_context(ctx) {
        parts.push(context);
    }
    if !parts.is_empty() {
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": parts.join("\n\n"),
            }
        });
        println!("{out}");
    }

    // Baseline the peers this turn starts knowing about, so the PostToolUse hook
    // can nudge if any of them close before this turn ends.
    seed_seen_peers(ctx);
    // Restart the tool-call count behind the mid-turn naming reminder, so it gets
    // one shot per turn rather than one per session.
    let _ = std::fs::remove_file(name_nudge_marker(ctx));
    Ok(())
}

/// Hidden reminder injected on each turn while this instance's hub listener is
/// not running (see `HUB_RULES` "INCOMING MESSAGES" for the exact Monitor
/// command). The Monitor `touch`es `armed/<id>` when it starts and again every
/// second it lives, `listener_armed` reads that heartbeat, and the reminder
/// stops. Kept low-key so arming happens in the background of the user's actual
/// first request, not as a separate visible turn.
///
/// It says "not running" rather than "not armed yet" because since monitors
/// started expiring this fires for a listener that *was* armed and has since
/// died, which is the case the heartbeat exists to catch.
const ARM_LISTENER_NUDGE: &str = "[Mulpex hub] Your background hub listener is not running right \
now (never armed, or it expired). As part of THIS turn — quietly, in the background — arm it: call \
Monitor with timeout_ms at the maximum the tool allows and exactly this command (do NOT copy a \
Monitor call from earlier in this conversation — an earlier one may be superseded):\n";

/// Tail of both nudges: the command itself, spelled the one way `HUB_RULES`
/// spells it. `rules::listener_command` is the single source, and the helper can
/// name its own path because the hook *is* the helper — the same binary the
/// instance is being told to run.
fn nudge_listener_command() -> String {
    std::env::current_exe()
        .map(|p| crate::rules::listener_command(&p))
        .unwrap_or_else(|_| "\"<mulpex-helper>\" listen".to_string())
}

/// Starts with a newline so the command sits on a line of its own: run into the
/// next sentence it reads as one long string, and the thing being copied has to
/// be unambiguous.
const ARM_LISTENER_TAIL: &str = "\nDo not make arming it your whole response and do not narrate it \
beyond a brief mention; just arm it and then continue with what I actually asked. (You'll see this \
reminder only while the listener is down.)";

/// Added to the nudge when `Stop` left a `relisten/<id>` note: the instance has a
/// listener that is *running* yet dead as far as the hub is concerned, or several
/// of them. Without this the plain nudge would have it arm an Nth Monitor beside
/// the ones already there — which is the pile-up the user sees as one hub message
/// producing several wake-ups.
///
/// The "do not copy an earlier Monitor call" clause is the whole point, and it is
/// stated twice on purpose: the measured failure was an instance re-emitting a
/// superseded command out of its own history 71 times across two days and an app
/// update, healing only when `/compact` finally dropped that call from its
/// context.
const RELISTEN_NUDGE: &str = "[Mulpex hub] Your current hub listener is a SUPERSEDED one: it is \
running, but it cannot report that it is alive, so it is not usable and this reminder cannot stop \
— and every turn you are asked again, another one piles up beside it. Fix it as part of THIS turn, \
quietly: call TaskStop on the task id(s) listed at the end of this message, then arm ONE new \
Monitor, with timeout_ms at the maximum the tool allows, using exactly this command (do NOT copy \
the Monitor call from earlier in this conversation — that is the superseded one):\n";

/// The arm nudge for this turn: the plain reminder, plus the repair instruction
/// when `Stop` recorded a listener that needs replacing. The note is consumed
/// here, so a listener that is genuinely fine after the repair does not keep
/// being told about itself.
///
/// Both forms **spell the command out**. The old nudge pointed at "INCOMING
/// MESSAGES in your instructions", and a pointer is what let a superseded copy
/// win: the nearest, most concrete text wins, and for an instance that had armed
/// the same command seventy times that text was its own history.
fn arm_listener_nudge(ctx: &Ctx) -> String {
    let cmd = nudge_listener_command();
    match std::fs::read_to_string(relisten_path(ctx)) {
        Ok(ids) if !ids.trim().is_empty() => {
            let _ = std::fs::remove_file(relisten_path(ctx));
            format!(
                "{RELISTEN_NUDGE}{cmd}{ARM_LISTENER_TAIL} Stale listener task id(s) to stop: {}",
                ids.trim()
            )
        }
        _ => format!("{ARM_LISTENER_NUDGE}{cmd}{ARM_LISTENER_TAIL}"),
    }
}

/// Whether this turn may carry the arm/name nudges.
///
/// A function rather than an inline `||` because getting it wrong is silent both
/// ways, and it was in fact wrong once: gating on `system_turn` alone suppressed
/// the nudges on ⌘⇧R's *sanctioned* wake too, so the exemption bought a turn that
/// still never re-armed — the precise failure it exists to prevent, and invisible
/// except by reading the hook's stdout.
fn nudges_welcome(system_turn: bool, sanctioned_resume: bool) -> bool {
    !system_turn || sanctioned_resume
}

/// Shown in the pane in place of the swallowed wake. Claude Code renders a blocked
/// `UserPromptSubmit` as a warning that also dumps the original prompt, and neither
/// `suppressOutput` nor an empty reason removes it (measured, 2026-09-03) — so the
/// reason is written to explain the notice the user is going to see anyway.
const ORPHAN_WAKE_BLOCK_REASON: &str =
    "[Mulpex hub] Ignored: this is the previous Mulpex run's hub listener being \
reported dead, not a message and not work. Nothing to do.";

/// The restart wake, and the reason idle instances "open by themselves" after every
/// Mulpex update.
///
/// Mulpex kills each `claude`'s whole process group at teardown, so the persistent
/// Monitor backing the hub listener dies with no completion record. On the next
/// launch the resumed session is handed a synthetic
/// `<task-notification><status>stopped</status>… No completion record was found …`
/// prompt, which is a **real turn**: the model wakes, the `UserPromptSubmit` hook
/// fires, its arm nudge lands, and the instance dutifully starts a fresh persistent
/// Monitor — the orphan that does this again next launch. Measured end-to-end in a
/// live transcript (`bvpgm5lxl` → `bjwjmo0kw`, 2026-09-03).
///
/// The match is deliberately narrow. Only the `stopped`-with-no-completion-record
/// shape is swallowed, because that one is *provably* stale: it describes a task
/// belonging to a process this app already killed, so it can never be actionable.
/// Everything else a `<task-notification>` carries goes through untouched — a hub
/// wake (`<event>mulpex: N new hub message(s)</event>`), a background job that
/// genuinely completed or failed, a Monitor the user stopped themselves.
fn orphaned_task_wake(prompt: &str) -> bool {
    prompt.starts_with("<task-notification")
        && prompt.contains("<status>stopped</status>")
        && prompt.contains("No completion record was found")
}

/// Consume the ⌘⇧R flag, reporting whether it was there. Deleted on read: it
/// sanctions exactly one wake, so a later app restart of the same instance is back
/// to being ordinary restart noise.
fn take_resumed_in_place(ctx: &Ctx) -> bool {
    let path = crate::resumed_in_place_path(&ctx.state_dir, ctx.instance);
    path.exists() && std::fs::remove_file(&path).is_ok()
}

/// How long `armed/<id>` may go untouched before the listener counts as dead.
/// The loop touches it once a second, so anything past a few seconds is a
/// stopped listener; the margin is for a machine that slept or a very loaded box.
const LISTENER_HEARTBEAT_GRACE: Duration = Duration::from_secs(30);

/// Whether this instance's hub listener is **alive**, not merely whether it was
/// ever armed.
///
/// `armed/<id>` is a heartbeat: the listener `touch`es it when it starts and then
/// again on every pass of its one-second loop (`HUB_RULES` "INCOMING MESSAGES"),
/// so the file's *mtime* is the liveness signal and its mere existence is not.
///
/// The distinction became load-bearing when Claude Code removed persistent
/// Monitors. Every monitor now expires (30 min at most), so a listener stops on
/// its own while the flag it wrote sits there forever — and the arm nudge, which
/// is the only thing that ever gets one re-armed, is gated on exactly this
/// function. Testing existence meant a listener died silently at the 30-minute
/// mark and the instance was never woken by peer mail again, with nothing
/// anywhere saying so. Testing freshness makes the nudge come back by itself.
///
/// A fresh `state_dir` per Mulpex launch (and thus per `--resume`, which kills
/// the old Monitor) still means the flag is absent at startup, so restored
/// instances re-arm as before.
fn listener_armed(ctx: &Ctx) -> bool {
    let path = ctx.state_dir.join("armed").join(ctx.id_str());
    let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
        return false;
    };
    // A clock that moved backwards leaves `elapsed()` erroring; treat that as
    // alive rather than nudging an instance whose listener is probably fine.
    modified
        .elapsed()
        .map(|age| age < LISTENER_HEARTBEAT_GRACE)
        .unwrap_or(true)
}

/// Hidden reminder injected each turn until this instance has named its own
/// sidebar row (`mcp__mulpex__hub_set_name`, which writes the `named/<id>` flag
/// `instance_named` reads). Same self-healing shape as `ARM_LISTENER_NUDGE`.
///
/// Without a name a row falls back to showing the captured prompt, which is the
/// user's *last request* verbatim — long, and wrong the moment the session moves
/// on. Deliberately permission to *defer*: a first turn of "hi" or "what does
/// this crate do?" has nothing to name a session after, and being re-asked next
/// turn is cheaper than a row labelled after a throwaway question.
const AUTO_NAME_NUDGE: &str = "[Mulpex hub] This instance has no sidebar name yet. As part of \
THIS turn — quietly, in the background — call mcp__mulpex__hub_set_name with a short label for \
the work you're starting: 2-5 words, in the same language I write to you in, naming the TASK (not \
you, not the tool). Do not narrate it beyond a brief mention, and do not make it your whole \
response; just name it and continue with what I actually asked. If this turn doesn't yet make \
clear what the session is about, skip it — you'll be reminded next turn. (You'll see this \
reminder only until the instance is named.)";

/// Whether this instance has a sidebar name, i.e. `named/<id>` exists. The flag
/// is written by `hub_set_name` (this instance naming itself), and by Mulpex when
/// the *user* renames the row (⌘R) or when a restored session comes back with a
/// name — so a name the user chose is never nudged over. A fresh `state_dir` per
/// launch is why the restore case has to seed it explicitly, unlike `armed`.
fn instance_named(ctx: &Ctx) -> bool {
    crate::named_flag_path(&ctx.state_dir, ctx.instance).exists()
}

/// How many tool calls into a turn the mid-turn naming reminder fires.
///
/// `AUTO_NAME_NUDGE` arrives with the user's prompt and is easy to *acknowledge*
/// and then lose: measured on a live instance, claude#6 opened its turn with "I'll
/// start by arming the hub listener and naming this instance", armed the Monitor
/// (so `armed/<id>` was written — proof the nudge landed), and then spent three
/// minutes on the actual task and never called `hub_set_name`. `named/<id>` was
/// absent and `namereq/` empty afterwards, so nothing was refused; the reminder was
/// simply dropped, and the next one would not come until the user's *next* prompt.
///
/// So naming gets the second chance hub mail already has (`posttooluse`). A few
/// calls in is the useful moment: late enough that the model knows what the session
/// is about, early enough that the row is labelled while the work is still running.
const NAME_NUDGE_AFTER_TOOLS: usize = 3;

/// The mid-turn form of `AUTO_NAME_NUDGE`. Shorter and more direct than the
/// prompt-time one — this instance has already been asked once this turn — but it
/// keeps the same permission to defer, for the same reason.
const NAME_NUDGE_MIDTURN: &str = "You still have no sidebar name, so your row shows my raw prompt \
instead. Call mcp__mulpex__hub_set_name now with a short label (2-5 words, my language, naming \
the TASK) and then carry straight on with what you were doing — one call, no narration, don't \
restate your plan. Skip it only if it's still genuinely unclear what this session is about.";

/// Whether *this* tool call is the one that carries the mid-turn naming reminder:
/// the instance is still unnamed, and this is the Nth call of the turn. The count
/// only advances while unnamed, so a named instance costs nothing.
fn name_nudge_due(ctx: &Ctx) -> bool {
    if instance_named(ctx) {
        return false;
    }
    bump_name_nudge(ctx) == NAME_NUDGE_AFTER_TOOLS
}

/// Count this tool call against `NAME_NUDGE_AFTER_TOOLS` and return the new total.
///
/// Kept in its own `namenudge/` subdir rather than beside the status files: the
/// App's state scans pick up bare-integer filenames at the root, and `mcp::live_ids`
/// falls back to exactly that — the same reason `peers/` is a subdir. The counter is
/// cleared at each `UserPromptSubmit`, so the nudge fires at most once per turn.
fn bump_name_nudge(ctx: &Ctx) -> usize {
    let path = name_nudge_marker(ctx);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let count = read_field_or_line(&path)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        + 1;
    let _ = std::fs::write(&path, count.to_string());
    count
}

fn name_nudge_marker(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("namenudge").join(ctx.id_str())
}

/// Emit a PreToolUse deny naming the holder (and what they're working on, when
/// known), for both edit and Bash conflicts. The wording frames the lock as
/// normal coordination so the blocked instance switches work instead of trying
/// to bypass it or asking the user — reinforcing the injected hub rules.
fn deny_edit(ctx: &Ctx, path: &Path, owner: &str) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    // The holder's current task, if they've published one (auto from their
    // prompt or via hub_set_focus).
    let doing = read_field_or_line(&ctx.tasks_dir.join(owner))
        .filter(|t| !t.is_empty())
        .map(|t| format!(", who is working on: \"{t}\""))
        .unwrap_or_default();
    let reason = format!(
        "{name} is locked by claude#{owner}{doing} (editing it now). This is normal \
         multi-instance coordination, not an error — do NOT try to bypass it (no shell \
         workarounds) and do NOT ask the user about it. Work on a different file/task, or \
         stop and let that instance finish; the lock releases when its turn ends. You can \
         call mcp__mulpex__hub_file_owner to check a file, or hub_instances to see everyone."
    );
    emit("deny", Some(&reason), None);
}

/// Edit fallback when a file stays *actively* held after the full wait budget (or
/// the holder is stuck on the user): proceed with a stale-read awareness note
/// rather than deny. Leans on Claude's intelligence + Claude Code's own "file
/// modified since read" check — exactly the "be aware, don't block forever"
/// tradeoff. Only reached in the rare hot/contended case; the idle-lease frees
/// most files long before this.
fn allow_contended(ctx: &Ctx, path: &Path, owner: &str) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let doing = read_field_or_line(&ctx.tasks_dir.join(owner))
        .filter(|t| !t.is_empty())
        .map(|t| format!(", who is working on: \"{t}\""))
        .unwrap_or_default();
    let note = format!(
        "{name} is being edited concurrently by claude#{owner}{doing}. Proceeding anyway: \
         re-read {name} RIGHT NOW immediately before you write it, so your edit applies to \
         its current contents. If Claude Code reports \"File has been modified since read\", \
         that's expected coordination between parallel instances — just re-read and retry, \
         do NOT ask the user and do NOT use shell workarounds."
    );
    emit("allow", None, Some(&note));
}

/// The `key=value` body of a lock token: who holds the file, its path, and the
/// heartbeat timestamp (`ts`) that `lock_is_stale` compares against `LOCK_IDLE`.
fn lock_token(instance: usize, path: &Path) -> String {
    format!("instance={}\npath={}\nts={}\n", instance, path.display(), now())
}

/// A lock is *stale* (reclaimable by a waiter) when its holder hasn't heartbeated
/// it within `LOCK_IDLE` — i.e. acquired the file but moved on to others this
/// turn. A missing/garbled `ts` also reads stale (a waiter shouldn't block on an
/// un-dateable token).
fn lock_is_stale(lock_file: &Path) -> bool {
    match read_field(lock_file, "ts").and_then(|s| s.parse::<u64>().ok()) {
        Some(ts) => now().saturating_sub(ts) >= LOCK_IDLE.as_secs(),
        None => true,
    }
}

/// Read a small single-value file (the task files are a bare line, not `k=v`).
fn read_field_or_line(file: &Path) -> Option<String> {
    std::fs::read_to_string(file)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Print a PreToolUse hook decision as JSON on stdout.
fn emit(decision: &str, reason: Option<&str>, context: Option<&str>) {
    let mut hso = serde_json::json!({
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
    });
    if let Some(r) = reason {
        hso["permissionDecisionReason"] = serde_json::Value::String(r.to_string());
    }
    if let Some(c) = context {
        hso["additionalContext"] = serde_json::Value::String(c.to_string());
    }
    println!("{}", serde_json::json!({ "hookSpecificOutput": hso }));
}

/// Read a `key=value` line's value from a small meta file.
pub(crate) fn read_field(file: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(file).ok()?;
    let prefix = format!("{key}=");
    content
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(|v| v.trim().to_string()))
}

/// Canonical absolute path for a tool's `file_path`, so two spellings of the
/// same file (relative, symlinked, `..`) map to one lock key. For a not-yet-
/// existing file (a `Write` creating it) `canonicalize` fails, so we canonicalize
/// the existing parent dir and re-append the final component.
pub(crate) fn canonical_target(ctx: &Ctx, raw: &str) -> Option<PathBuf> {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.project_dir.join(p)
    };
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return Some(c);
    }
    let parent = abs.parent()?;
    let name = abs.file_name()?;
    Some(std::fs::canonicalize(parent).ok()?.join(name))
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delivery check must tell three things apart: the task arrived exactly,
    /// the task arrived MANGLED, and there is nothing to check.
    ///
    /// The middle one is the whole point. When the task was typed into the TUI,
    /// `claude` capped it at one tty read — 1022 characters, measured, whatever the
    /// input size — and the child then worked, confidently, on the first kilobyte
    /// of its brief. The app could not see that: it knows what it sent, the child
    /// knows what it got, and only this hook sees both.
    #[test]
    fn a_mangled_spawn_task_is_caught_by_the_child_itself() {
        let dir = std::env::temp_dir().join(format!("mulpex-deliv3-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join(crate::SPAWNING_DIR)).unwrap();
        let ctx = test_ctx(&dir, 4);
        let verdict = crate::spawn_delivery_path(&dir, 4);
        let expected_at = crate::spawn_expected_path(&dir, 4);

        let sent = format!(
            "{} Begin the following task, which was assigned to you by claude#2: {}",
            crate::MULPEX_SENTINEL,
            "word ".repeat(1200)
        );
        assert!(sent.len() > 1022, "the fixture must exceed the old paste cap");

        // (a) Nothing expected — not a spawned child. Must not invent a verdict.
        std::fs::write(&verdict, "pending").unwrap();
        verify_spawn_delivery(&ctx, &sent);
        assert_eq!(std::fs::read_to_string(&verdict).unwrap(), "pending");

        // (b) Truncated exactly the way the TUI used to truncate it.
        std::fs::write(&expected_at, &sent).unwrap();
        verify_spawn_delivery(&ctx, &sent[..1022]);
        assert_eq!(
            std::fs::read_to_string(&verdict).unwrap(),
            "partial",
            "a child working on the first 1022 characters of its brief must not read as delivered"
        );

        // (c) The whole thing. Both files go: delivered, and verified so.
        std::fs::write(&verdict, "pending").unwrap();
        std::fs::write(&expected_at, &sent).unwrap();
        verify_spawn_delivery(&ctx, &sent);
        assert!(!verdict.exists(), "a verified delivery leaves no verdict behind");
        assert!(!expected_at.exists(), "the expectation is consumed once ruled on");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_ctx(dir: &Path, instance: usize) -> Ctx {
        let state_dir = dir.to_path_buf();
        Ctx {
            instance,
            project_dir: state_dir.clone(),
            locks_dir: state_dir.join("locks"),
            history_dir: state_dir.join("history"),
            tasks_dir: state_dir.join("tasks"),
            inbox_dir: state_dir.join("inbox"),
            waiting_dir: state_dir.join("waiting"),
            bg_dir: state_dir.join("bg"),
            compacting_dir: state_dir.join("compacting"),
            state_dir,
        }
    }

    /// The exact payloads Claude Code v2.1.234 hands the `Stop` hook, captured
    /// from a real session driven on a PTY (`scratchpad/agentprobe.py`). The array
    /// is what tells an ended turn apart from an idle instance.
    const STOP_WITH_AGENT: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,
        "background_tasks":[{"id":"a02a60b9ffa020198","type":"subagent","status":"running",
        "description":"Sleep then reply","agent_type":"general-purpose"}],"session_crons":[]}"#;
    const STOP_WITH_SHELL: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,
        "background_tasks":[{"id":"bwg6gwcry","type":"shell","status":"running",
        "description":"Sleep 150 seconds in background","command":"sleep 150; echo done"}],
        "session_crons":[]}"#;
    const STOP_IDLE: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,
        "background_tasks":[],"session_crons":[]}"#;
    /// A session with a cron scheduled but nothing in flight is genuinely idle.
    const STOP_CRON_ONLY: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,
        "background_tasks":[],"session_crons":[{"id":"c1"}]}"#;

    fn payload(s: &str) -> Option<serde_json::Value> {
        serde_json::from_str(s).ok()
    }

    /// A turn that ends with a background agent — or a `run_in_background` shell —
    /// still running is NOT the instance waiting for the user, and must not be
    /// reported as such. Both kinds arrive in the same `background_tasks` array.
    #[test]
    fn a_turn_that_ends_with_background_work_is_not_idle() {
        let dir = std::env::temp_dir().join(format!("mulpex-bgwork-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 3);
        assert!(background_work_running(&ctx, payload(STOP_WITH_AGENT).as_ref()));
        assert!(background_work_running(&ctx, payload(STOP_WITH_SHELL).as_ref()));
        assert!(!background_work_running(&ctx, payload(STOP_IDLE).as_ref()));
        assert!(
            !background_work_running(&ctx, payload(STOP_CRON_ONLY).as_ref()),
            "a scheduled cron is not work in flight — between firings the instance really is idle"
        );
        // A payload from some future Claude Code that drops the field at all, and
        // a finished task still listed, both read as idle.
        assert!(!background_work_running(&ctx, payload(r#"{"hook_event_name":"Stop"}"#).as_ref()));
        assert!(!background_work_running(
            &ctx, payload(r#"{"background_tasks":[{"id":"x","status":"completed"}]}"#).as_ref()
        ));
        // ...but an entry with no status at all counts as running: the failure that
        // matters is calling a busy instance idle.
        assert!(background_work_running(
            &ctx, payload(r#"{"background_tasks":[{"id":"x"}]}"#).as_ref()
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one job the two `PreToolUse` matchers must never lose, whatever else
    /// they do for the Explainer: the `needs` status word — `needs` is the
    /// sidebar's "this instance is holding something up for YOU".
    #[test]
    fn a_waiting_dialog_still_writes_the_needs_status_word() {
        let dir = std::env::temp_dir().join(format!("mulpex-needs-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir, 4);
        let status = ctx.state_dir.join(ctx.id_str());

        // `write_needs`, not `askq`/`plan`: those read stdin to EOF first, which
        // never comes when the runner's stdin is a pipe nobody closes.
        write_needs(&ctx);
        assert_eq!(std::fs::read_to_string(&status).unwrap(), "needs");

        let _ = std::fs::write(&status, "working");
        write_needs(&ctx);
        assert_eq!(std::fs::read_to_string(&status).unwrap(), "needs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `Stop` payload as measured 2026-08-30 (probe0): `transcript_path` is
    /// present alongside the fields the mail/background logic already uses.
    const STOP_WITH_TRANSCRIPT: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,
        "session_id":"3562192c-a1ad-48c3-a94c-94b6e742499c",
        "transcript_path":"/Users/x/.claude/projects/-p/3562192c.jsonl",
        "background_tasks":[],"session_crons":[]}"#;

    /// The Explainer request must mirror exactly what the payload says — and say
    /// nothing when the payload doesn't. A missing or empty `transcript_path`
    /// writes no file (that turn gets no explanation; better than a guessed path).
    /// A finished turn carries no `dialog` line: the reader waits for prose.
    #[test]
    fn a_finished_turn_hands_its_transcript_to_the_explainer() {
        let dir = std::env::temp_dir().join(format!("mulpex-explain-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 3);

        write_explain_request(&ctx, payload(STOP_WITH_TRANSCRIPT).as_ref(), false);
        let req = crate::explain_request_path(&ctx.state_dir, 3);
        assert_eq!(
            std::fs::read_to_string(&req).unwrap(),
            "/Users/x/.claude/projects/-p/3562192c.jsonl"
        );

        // A second turn overwrites — latest-wins is the coalescing contract.
        let redone = STOP_WITH_TRANSCRIPT.replace("3562192c.jsonl", "later.jsonl");
        write_explain_request(&ctx, payload(&redone).as_ref(), false);
        assert!(std::fs::read_to_string(&req).unwrap().ends_with("later.jsonl"));

        let _ = std::fs::remove_file(&req);
        write_explain_request(&ctx, payload(STOP_IDLE).as_ref(), false);
        assert!(!req.exists(), "no transcript_path in the payload → no request file");
        write_explain_request(&ctx, payload(r#"{"transcript_path":""}"#).as_ref(), false);
        assert!(!req.exists(), "an empty transcript_path is not a path");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pending AskUserQuestion reaches the Explainer as the transcript path
    /// plus the `dialog` marker — not as the questions payload, which the app
    /// reads out of the transcript itself. The marker is what stops the reader
    /// summarizing the turn's prose before the question entry has landed.
    #[test]
    fn a_pending_question_reaches_the_explainer() {
        let dir = std::env::temp_dir().join(format!("mulpex-askq-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 4);
        let req = crate::explain_request_path(&ctx.state_dir, 4);

        let pretool = r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion",
            "transcript_path":"/Users/x/.claude/projects/-p/3562192c.jsonl",
            "tool_input":{"questions":[{"question":"Which way?","header":"Way",
            "options":[{"label":"A","description":"first"}],"multiSelect":false}]}}"#;
        write_explain_request(&ctx, payload(pretool).as_ref(), true);
        assert_eq!(
            std::fs::read_to_string(&req).unwrap(),
            "/Users/x/.claude/projects/-p/3562192c.jsonl\ndialog"
        );

        let _ = std::fs::remove_file(&req);
        write_explain_request(&ctx, payload(r#"{"tool_input":{"questions":[]}}"#).as_ref(), true);
        assert!(!req.exists(), "no transcript_path → no request file, dialog or not");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pending plan reaches the Explainer the same way. The payload is the
    /// shape measured on a real `claude` v2.1.252 driven on a PTY, 2026-09-01
    /// (`scratchpad/probe`): `PreToolUse[ExitPlanMode]` with `plan` +
    /// `planFilePath` — neither of which is forwarded.
    #[test]
    fn a_pending_plan_reaches_the_explainer() {
        let dir = std::env::temp_dir().join(format!("mulpex-plan-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 6);
        let req = crate::explain_request_path(&ctx.state_dir, 6);

        // `r##`: the plan is markdown, so the payload contains `"#` (a quote then a
        // heading), which would close a plain `r#""#` literal mid-string.
        let pretool = r##"{"hook_event_name":"PreToolUse","tool_name":"ExitPlanMode",
            "transcript_path":"/Users/x/.claude/projects/-p/3562192c.jsonl",
            "permission_mode":"plan","tool_input":{"plan":"# Add a comment\n\nInsert one line.",
            "planFilePath":"/Users/x/.claude/plans/plan-abc.md"}}"##;
        write_explain_request(&ctx, payload(pretool).as_ref(), true);
        let written = std::fs::read_to_string(&req).unwrap();
        assert!(written.starts_with("/Users/x/.claude/projects/-p/3562192c.jsonl\n"));
        assert!(written.ends_with("dialog"));
        assert!(!written.contains("Insert one line."), "the plan is read from the transcript, not forwarded");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real hub-listener command, exactly as `HUB_RULES` dictates it and
    /// exactly as it reaches the `Stop` hook — captured from a real `claude`
    /// (`scratchpad/monprobe`, 2026-09-16). The whole command survives verbatim
    /// however long it is, which is what makes it usable as the identifying mark.
    const LISTENER_CMD: &str = r#"INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; ARMED=\"$MULPEX_STATE_DIR/armed\"; mkdir -p \"$INBOX\" \"$ARMED\"; touch \"$ARMED/$MULPEX_INSTANCE_ID\"; prev=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d \" \"); while true; do cur=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d \" \"); if [ \"$cur\" -gt \"$prev\" ]; then echo \"mulpex: $((cur - prev)) new hub message(s)\"; fi; prev=$cur; touch \"$ARMED/$MULPEX_INSTANCE_ID\"; sleep 1; done"#;

    /// A turn that ended with nothing but the hub listener running.
    fn stop_with_listener() -> String {
        format!(
            r#"{{"hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[
            {{"id":"bnxvw92ez","type":"shell","status":"running",
             "description":"Mulpex hub inbox","command":"{LISTENER_CMD}"}}],"session_crons":[]}}"#
        )
    }

    /// The same turn, with a real background shell running alongside the listener.
    fn stop_listener_and_shell() -> String {
        format!(
            r#"{{"hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[
            {{"id":"bnxvw92ez","type":"shell","status":"running",
             "description":"Mulpex hub inbox","command":"{LISTENER_CMD}"}},
            {{"id":"bwg6gwcry","type":"shell","status":"running",
             "description":"Sleep 150 seconds","command":"sleep 150"}}],"session_crons":[]}}"#
        )
    }

    /// Mulpex tells **every** instance to arm a Monitor on its inbox (`HUB_RULES`
    /// "INCOMING MESSAGES"). Counting it as work in flight made every instance in
    /// every project report `working` forever — which, because `working` also
    /// suppresses the idle notification, silently killed the red dot, the tab
    /// badge, the dock badge and the banner for everyone. Reported from the field
    /// with a pane sitting idle at its prompt under
    /// `Baked for 4m 0s · 1 monitor still running`.
    ///
    /// It is recognised by its **command**, which `HUB_RULES` authors. The
    /// previous version recorded each persistent Monitor's task id from
    /// `PostToolUse` and subtracted it here; that died silently when Claude Code
    /// removed persistent Monitors (`persistent` moved to `tool_response` and
    /// became permanently `false`, measured 2026-09-16), and every instance stuck
    /// on yellow with no way back short of an app restart.
    #[test]
    fn the_hub_listener_is_a_watcher_not_work_in_flight() {
        let dir = std::env::temp_dir().join(format!("mulpex-listener-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join("bg")).unwrap();
        let ctx = test_ctx(&dir, 3);

        assert!(
            !background_work_running(&ctx, payload(&stop_with_listener()).as_ref()),
            "an instance idle at its prompt with only its hub listener running is NOT working"
        );

        // ...and the whole point: the turn ends green rather than pinning the row
        // yellow forever.
        set_background_flag(
            &ctx,
            background_work_running(&ctx, payload(&stop_with_listener()).as_ref()),
        );
        assert_eq!(notify_status(&ctx), "waiting");

        // Real work running alongside the listener still counts.
        assert!(
            background_work_running(&ctx, payload(&stop_listener_and_shell()).as_ref()),
            "excusing the listener must not excuse the background shell next to it"
        );

        // Any other Monitor is ordinary work — the exemption is the inbox command,
        // not the tool.
        assert!(background_work_running(
            &ctx,
            payload(
                r#"{"background_tasks":[{"id":"bq7one1x","type":"shell","status":"running",
                "description":"Wait for the build","command":"tail -f build.log"}]}"#
            )
            .as_ref()
        ));

        // A subagent has no `command` at all, and must not be mistaken for one.
        assert!(background_work_running(
            &ctx,
            payload(
                r#"{"background_tasks":[{"id":"ba1","type":"subagent","status":"running",
                "description":"Explore","agent_type":"Explore"}]}"#
            )
            .as_ref()
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `armed/<id>` is a heartbeat, not a one-off flag: the listener re-`touch`es
    /// it every second, so a monitor that has expired — which every monitor now
    /// does — goes stale and the arm nudge comes back on its own. Testing mere
    /// existence meant a dead listener was indistinguishable from a live one and
    /// the instance was never woken by peer mail again.
    #[test]
    fn a_dead_listener_goes_stale_so_the_nudge_comes_back() {
        let dir = std::env::temp_dir().join(format!("mulpex-armed-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 4);
        let armed = dir.join("armed");
        std::fs::create_dir_all(&armed).unwrap();
        let flag = armed.join("4");

        assert!(!listener_armed(&ctx), "never armed");

        std::fs::write(&flag, "").unwrap();
        assert!(listener_armed(&ctx), "just touched — the listener is alive");

        // Age the heartbeat past the grace: the monitor expired and stopped
        // touching it, while the file it wrote sits there forever.
        let stale = SystemTime::now() - LISTENER_HEARTBEAT_GRACE - Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&flag)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        assert!(
            !listener_armed(&ctx),
            "an expired listener must read as unarmed, or nothing ever re-arms it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pre-`003ad33` listener command: it `touch`es `armed/<id>` once at
    /// startup and never again. Sessions that were already running keep re-arming
    /// this out of their own conversation history, so the hook has to recognise
    /// it as a listener *and* as one that can never report itself alive.
    const SUPERSEDED_LISTENER_CMD: &str = r#"INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; ARMED=\"$MULPEX_STATE_DIR/armed\"; mkdir -p \"$INBOX\" \"$ARMED\"; touch \"$ARMED/$MULPEX_INSTANCE_ID\"; prev=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d \" \"); while true; do cur=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d \" \"); if [ \"$cur\" -gt \"$prev\" ]; then echo \"mulpex: $((cur - prev)) new hub message(s)\"; fi; prev=$cur; sleep 1; done"#;

    fn stop_with_tasks(tasks: &str) -> Option<serde_json::Value> {
        payload(&format!(
            r#"{{"hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{tasks}],"session_crons":[]}}"#
        ))
    }

    /// The state only `Stop` can see, and the whole reason `relisten/<id>` exists.
    ///
    /// A listener running the superseded command never refreshes the heartbeat, so
    /// `listener_armed` is false on every turn, so the arm nudge fires on every
    /// turn, so a fresh Monitor stacks on every turn — measured on a live instance
    /// that reached three simultaneously. `UserPromptSubmit` cannot see this
    /// (its payload carries no `background_tasks`, measured against a real
    /// `claude` v2.1.273), which is why the detection has to happen at `Stop` and
    /// be handed forward on disk.
    #[test]
    fn stop_records_a_listener_that_cannot_report_itself_alive() {
        let dir = std::env::temp_dir().join(format!("mulpex-relisten-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 4);
        std::fs::create_dir_all(dir.join("armed")).unwrap();
        std::fs::create_dir_all(dir.join(crate::RELISTEN_DIR)).unwrap();
        let note = relisten_path(&ctx);

        // A healthy listener: heartbeat fresh, exactly one of it. Nothing to say.
        std::fs::write(dir.join("armed").join("4"), "").unwrap();
        note_listener_needs_replacing(
            &ctx,
            stop_with_tasks(&format!(
                r#"{{"id":"bok","type":"shell","status":"running","command":"{LISTENER_CMD}"}}"#
            ))
            .as_ref(),
        );
        assert!(!note.exists(), "a listener that reports itself alive needs no repair");

        // The superseded command: running, but the heartbeat it wrote once has
        // gone stale and never will refresh.
        let stale = SystemTime::now() - LISTENER_HEARTBEAT_GRACE - Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(dir.join("armed").join("4"))
            .unwrap()
            .set_modified(stale)
            .unwrap();
        note_listener_needs_replacing(
            &ctx,
            stop_with_tasks(&format!(
                r#"{{"id":"bstale1","type":"shell","status":"running","command":"{SUPERSEDED_LISTENER_CMD}"}}"#
            ))
            .as_ref(),
        );
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "bstale1");

        // Two listeners is the same failure one step later, and is what the user
        // actually sees: one hub message, two wake-ups. Recorded even when the
        // heartbeat is fine, because the duplicate still doubles every event.
        std::fs::write(dir.join("armed").join("4"), "").unwrap();
        note_listener_needs_replacing(
            &ctx,
            stop_with_tasks(&format!(
                r#"{{"id":"b1","type":"shell","status":"running","command":"{LISTENER_CMD}"}},
                   {{"id":"b2","type":"shell","status":"running","command":"{LISTENER_CMD}"}}"#
            ))
            .as_ref(),
        );
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "b1 b2");

        // No listener at all is the ordinary unarmed case — the plain nudge's job,
        // not a repair. A leftover note must be cleared, or the instance is told
        // to stop a task that is already gone.
        note_listener_needs_replacing(&ctx, stop_with_tasks("").as_ref());
        assert!(!note.exists(), "nothing running means nothing to replace");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The nudge has to name the tasks to stop, or the instance dutifully arms an
    /// Nth Monitor beside the ones already piling up. And it is consumed on read:
    /// once the repair has been asked for, the next turn's nudge is the ordinary
    /// one again.
    #[test]
    fn the_repair_nudge_names_the_tasks_and_is_consumed() {
        let dir = std::env::temp_dir().join(format!("mulpex-relisten-{}", crate::persist::new_uuid()));
        let ctx = test_ctx(&dir, 4);
        std::fs::create_dir_all(dir.join(crate::RELISTEN_DIR)).unwrap();

        // Both forms spell the command out rather than pointing at the rules —
        // a pointer is what let a superseded copy in the conversation win.
        // The command is spelled from `rules::listener_command`, the same source
        // `HUB_RULES` prints — one spelling, so a model is never choosing between
        // two. (Under `cargo test` the running binary is the test harness, not
        // `mulpex-helper`; in production the hook *is* the helper, which is how it
        // can name its own path. `rules::hub_rules_carry_the_exact_arming_command`
        // is where the real path is checked against `is_hub_listener`.)
        let cmd = crate::rules::listener_command(&std::env::current_exe().unwrap());
        let plain = arm_listener_nudge(&ctx);
        assert!(plain.starts_with(ARM_LISTENER_NUDGE));
        assert!(
            plain.contains(&cmd),
            "the nudge must carry the command itself, not a reference to it"
        );

        std::fs::write(relisten_path(&ctx), "bstale1 bstale2").unwrap();
        let nudge = arm_listener_nudge(&ctx);
        assert!(nudge.starts_with(RELISTEN_NUDGE), "the repair instruction, not the plain one");
        assert!(nudge.ends_with("bstale1 bstale2"), "names the tasks to TaskStop");
        assert!(nudge.contains(&cmd), "and the command to arm instead");
        assert!(
            nudge.contains("do NOT copy the Monitor call from earlier in this conversation"),
            "copying its own superseded call out of history is the failure being repaired"
        );
        assert!(!relisten_path(&ctx).exists(), "consumed on read");
        assert!(arm_listener_nudge(&ctx).starts_with(ARM_LISTENER_NUDGE));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Red is reserved for a pending question or plan, and a `Notification` can
    /// neither light it nor put it out.
    ///
    /// Claude Code fires `idle_prompt` 60 s after a turn ends whether or not the
    /// instance launched something that is still running (measured to the second:
    /// `Stop` 11:58:56 → `Notification` 11:59:56, with the agent still live). The
    /// notification's payload carries only `notification_type` and `message` — no
    /// task list — so the only thing that can answer "is it actually busy?" is
    /// what `Stop` recorded on its way out.
    #[test]
    fn a_notification_never_lights_red_and_never_clears_it() {
        let dir = std::env::temp_dir().join(format!("mulpex-notify-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join("bg")).unwrap();
        let ctx = test_ctx(&dir, 3);
        let status = ctx.state_dir.join("3");
        // Turn ended with an agent still running.
        set_background_flag(&ctx, background_work_running(&ctx, payload(STOP_WITH_AGENT).as_ref()));
        assert!(background_flag(&ctx).exists());
        assert_eq!(
            notify_status(&ctx),
            "working",
            "an instance waiting on its own agent must not claim it needs the user"
        );

        // The agent finishes; the next turn boundary clears the flag and the
        // instance is genuinely idle at its prompt — green, not red.
        set_background_flag(&ctx, background_work_running(&ctx, payload(STOP_IDLE).as_ref()));
        assert!(!background_flag(&ctx).exists());
        assert_eq!(
            notify_status(&ctx),
            "waiting",
            "an idle prompt is idleness, not a question for the user"
        );

        // A pending plan/question wrote `needs` from its own PreToolUse matcher.
        // The plan dialog's own `permission_prompt` lands ~6 s later (measured
        // 2026-09-01) and must not flip the row green while it is on screen.
        std::fs::write(&status, "needs").unwrap();
        assert_eq!(notify_status(&ctx), "needs");
        set_background_flag(&ctx, true);
        assert_eq!(notify_status(&ctx), "needs", "not even background work outranks it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Compaction is work, and it is invisible to every other hook.
    ///
    /// Measured on a real session: `/compact` fires **no `UserPromptSubmit`** (it
    /// is a local command, not a prompt), so the status file keeps whatever the
    /// last turn left it — and 60 s later the idle notification overwrites that
    /// with `needs`, while the pane is still drawing "Compacting conversation…".
    /// `PreCompact` 09:18:10 → `Notification{idle_prompt}` 09:19:10, to the
    /// second. Between `PreCompact` and the `SessionStart` that ends it nothing
    /// else fires at all (09:24:19 → 09:24:53 on a real compaction).
    #[test]
    fn compaction_is_working_and_never_needs_you() {
        let dir = std::env::temp_dir().join(format!("mulpex-compact-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join("compacting")).unwrap();
        std::fs::create_dir_all(dir.join("bg")).unwrap();
        let ctx = test_ctx(&dir, 1);

        // The exact payload Claude Code hands PreCompact.
        std::fs::write(compacting_flag(&ctx), "manual").unwrap();
        assert_eq!(
            notify_status(&ctx),
            "working",
            "the 60 s idle notification landed mid-compaction and called the instance idle"
        );

        // A manual /compact leaves the instance idle at its prompt...
        assert_eq!(compaction_end_status(&ctx), "waiting");
        // ...but an automatic one interrupted a turn that now carries on, and a
        // green "ready" dot in the middle of that turn is the same lie inverted.
        std::fs::write(compacting_flag(&ctx), "auto").unwrap();
        assert_eq!(compaction_end_status(&ctx), "working");

        // Once it has ended, the ordinary idle behaviour comes straight back.
        clear_compacting(&ctx);
        assert!(!compacting_flag(&ctx).exists());
        assert_eq!(notify_status(&ctx), "waiting");

        // A missing/garbled flag must not strand the row as busy.
        assert_eq!(compaction_end_status(&ctx), "waiting");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The verbatim prompt a resumed session is handed for a background task its
    /// dead predecessor never finished — captured from a live transcript
    /// (`bplozny5z`, 2026-09-03), which is the only place its exact wording lives.
    const ORPHAN_WAKE: &str = "<task-notification>\n<task-id>bplozny5z</task-id>\n\
<tool-use-id>toolu_01HwAgQuhcWwmQQFyWHX8eAh</tool-use-id>\n<status>stopped</status>\n\
<summary>No completion record was found for this background shell command from the \
previous session. It may have been stopped (via the UI, Monitor timeout, or agent \
teardown — these leave no transcript marker), or it may have been running when the \
previous Claude Code process exited. Check the output file for partial results before \
assuming it completed.</summary>\n</task-notification>";

    /// A hub wake, which is *also* a `<task-notification>` and must survive every
    /// check the orphan wake is caught by. Captured from this project's own pane.
    const HUB_WAKE: &str = "<task-notification>\n<task-id>bbmig56dj</task-id>\n\
<summary>Monitor event: \"mulpex hub inbox\"</summary>\n\
<event>mulpex: 1 new hub message(s)</event>\n</task-notification>";

    /// The restart loop, pinned at its narrowest point.
    ///
    /// Every instance "opening by itself" after a Mulpex update was this: the
    /// orphaned-Monitor wake is a real turn, the arm nudge rides in on it, and the
    /// instance arms the Monitor that becomes the next update's orphan. Both halves
    /// have to hold — recognising the wake, and *not* recognising anything else.
    #[test]
    fn the_restart_wake_is_swallowed_but_every_other_notification_is_not() {
        assert!(
            orphaned_task_wake(ORPHAN_WAKE),
            "the wake that reopens every instance after an update went unrecognised"
        );

        // The plural case, which is where this class of bug survives: these are all
        // task-notifications too, and swallowing any of them loses real news.
        for (label, prompt) in [
            ("a hub wake — the whole point of the listener", HUB_WAKE),
            (
                "a background job that genuinely finished",
                "<task-notification>\n<status>completed</status>\n<summary>Build \
                 finished</summary>\n</task-notification>",
            ),
            (
                "a background job that failed",
                "<task-notification>\n<status>failed</status>\n<summary>No completion \
                 record was found</summary>\n</task-notification>",
            ),
            (
                "a Monitor the user stopped themselves",
                "<task-notification>\n<status>killed</status>\n<summary>Monitor \
                 \"watching CI\" stopped</summary>\n</task-notification>",
            ),
            ("the user simply talking", "fix the login bug"),
            (
                "the user quoting the notification at us",
                "why do I keep seeing No completion record was found?",
            ),
        ] {
            assert!(
                !orphaned_task_wake(prompt),
                "swallowed {label} — that is real news the user never hears"
            );
        }
    }

    /// ⌘⇧R and an app launch both `--resume`, and the same wake means opposite
    /// things: noise after an update, the only path back to an armed listener
    /// after a restart-in-place. The flag is one-shot, so the *next* update does
    /// not inherit the exemption.
    #[test]
    fn a_restart_in_place_is_allowed_the_wake_an_app_launch_is_not() {
        let dir =
            std::env::temp_dir().join(format!("mulpex-orphanwake-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join(crate::RESUMED_DIR)).unwrap();
        let ctx = test_ctx(&dir, 3);

        assert!(
            !take_resumed_in_place(&ctx),
            "an app launch claimed a restart-in-place it never performed"
        );

        std::fs::write(crate::resumed_in_place_path(&dir, 3), "").unwrap();
        assert!(take_resumed_in_place(&ctx), "⌘⇧R's own wake was swallowed");
        assert!(
            !take_resumed_in_place(&ctx),
            "the exemption outlived its one wake, so the loop comes back next update"
        );

        // The flag is per instance: claude#3's restart must not speak for claude#4.
        std::fs::write(crate::resumed_in_place_path(&dir, 3), "").unwrap();
        assert!(!take_resumed_in_place(&test_ctx(&dir, 4)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gate that decides whether a turn is allowed to be asked to arm.
    #[test]
    fn only_a_wake_we_asked_for_is_allowed_to_re_arm() {
        assert!(
            nudges_welcome(false, false),
            "the user talking stopped being nudged — instances never arm or name themselves"
        );
        assert!(
            !nudges_welcome(true, false),
            "restart noise is still asked to arm, which is the loop itself"
        );
        assert!(
            nudges_welcome(true, true),
            "\u{2318}\u{21e7}R's own wake was not allowed to re-arm, so the restarted \
             instance stays deaf to hub mail with nothing to say so"
        );
    }

    /// `SessionStart` fires for startup, resume and clear as well as compaction,
    /// and only the compaction one is ours: the others must not overwrite a
    /// status the restore path has already set.
    #[test]
    fn only_a_compaction_session_start_touches_the_status() {
        let dir = std::env::temp_dir().join(format!("mulpex-sstart-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(dir.join("compacting")).unwrap();
        let ctx = test_ctx(&dir, 1);
        let status = dir.join("1");

        for source in ["startup", "resume", "clear"] {
            std::fs::write(&status, "waiting").unwrap();
            std::fs::write(compacting_flag(&ctx), "manual").unwrap();
            assert!(
                !is_compaction_end(source),
                "SessionStart[source={source}] would have rewritten the status"
            );
        }
        assert!(is_compaction_end("compact"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The naming reminder has to arrive a second time *inside* the turn, because
    /// the prompt-time one is easy to acknowledge and then lose — measured on a
    /// live instance that armed its Monitor, announced it would name itself, and
    /// then worked for three minutes without ever calling `hub_set_name`.
    ///
    /// Once per turn, though: repeating it on every tool call would be noise in
    /// the middle of someone else's work.
    #[test]
    fn the_naming_nudge_comes_back_once_mid_turn_until_the_row_is_named() {
        let dir = std::env::temp_dir().join(format!("mulpex-namenudge-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir, 6);

        // Not on the first calls — the model needs to know what it's naming.
        for call in 1..NAME_NUDGE_AFTER_TOOLS {
            assert!(!name_nudge_due(&ctx), "nudged too early, on call {call}");
        }
        assert!(name_nudge_due(&ctx), "no reminder ever arrived");
        // …and not again for the rest of the turn, however long it runs.
        for _ in 0..10 {
            assert!(!name_nudge_due(&ctx), "the reminder repeated within one turn");
        }

        // A new turn (UserPromptSubmit clears the count) re-arms it.
        let _ = std::fs::remove_file(name_nudge_marker(&ctx));
        for _ in 1..NAME_NUDGE_AFTER_TOOLS {
            assert!(!name_nudge_due(&ctx));
        }
        assert!(name_nudge_due(&ctx), "the next turn was never reminded");

        // Naming the row ends it for good — including the ⌘R case, where the flag
        // is written by Mulpex rather than by the instance.
        let _ = std::fs::remove_file(name_nudge_marker(&ctx));
        let flag = crate::named_flag_path(&ctx.state_dir, ctx.instance);
        std::fs::create_dir_all(flag.parent().unwrap()).unwrap();
        std::fs::write(&flag, "").unwrap();
        for _ in 0..(NAME_NUDGE_AFTER_TOOLS + 5) {
            assert!(!name_nudge_due(&ctx), "a named instance was still nudged");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
