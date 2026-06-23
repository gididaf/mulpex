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
//! - `posttooluse` keeps the `working` status word and nudges the instance to read
//!   newly-arrived hub mail mid-turn (deduped, so once per message).
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
        "stop" => stop(&ctx),
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
        let _ = std::fs::create_dir_all(&locks_dir);
        let _ = std::fs::create_dir_all(&history_dir);
        let _ = std::fs::create_dir_all(&tasks_dir);
        let _ = std::fs::create_dir_all(&inbox_dir);
        let _ = std::fs::create_dir_all(&waiting_dir);
        Some(Ctx {
            instance,
            state_dir,
            project_dir,
            locks_dir,
            history_dir,
            tasks_dir,
            inbox_dir,
            waiting_dir,
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
            "claude #{prev} modified this file earlier this session — read its current state before editing."
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
    let already_continued = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|j| j.get("stop_hook_active").and_then(|v| v.as_bool()))
        .unwrap_or(false);

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
    // Preserve the sidebar status the old `printf waiting` Stop hook produced.
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "waiting");
    Ok(())
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
/// old `printf` hook), and — the moment new hub mail has arrived *mid-turn* —
/// inject a one-line nudge so the instance reads it without waiting for its turn
/// to end. Deduped via the `<id>.notified` high-water mark so a single message
/// nudges once, not on every subsequent tool call.
fn posttooluse(ctx: &Ctx) -> anyhow::Result<()> {
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "working");

    let unread = crate::mcp::unread_for(ctx, ctx.instance);
    let marker = notified_marker(ctx);
    let last: usize = read_field_or_line(&marker)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if unread > last {
        let reason = format!(
            "[Mulpex hub] You have {unread} unread message(s) from other instances — call \
             mcp__mulpex__hub_inbox to read them (a peer may be coordinating a change that \
             affects your work)."
        );
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": reason,
            }
        });
        println!("{out}");
    }
    // Track the high-water mark (whether it rose or fell, e.g. after a hub_inbox
    // read cleared it) so each new message nudges exactly once.
    let _ = std::fs::write(&marker, unread.to_string());
    Ok(())
}

/// Handle a UserPromptSubmit event: (a) mark this instance `working` (preserving
/// the old `printf` status hook), (b) capture the submitted prompt as this
/// instance's baseline task for the hub, and (c) inject a compact snapshot of the
/// other instances into this turn via `additionalContext`.
fn userpromptsubmit(ctx: &Ctx) -> anyhow::Result<()> {
    let _ = std::fs::write(ctx.state_dir.join(ctx.id_str()), "working");

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&input) {
            // Claude Code's UserPromptSubmit payload carries the text under
            // `prompt` (NOT `userPrompt`) — see code.claude.com/docs hooks.
            if let Some(prompt) = json.get("prompt").and_then(|v| v.as_str()) {
                let task = crate::mcp::summarize(prompt);
                if !task.is_empty() {
                    let _ = std::fs::write(ctx.tasks_dir.join(ctx.id_str()), &task);
                }
            }
        }
    }

    if let Some(context) = crate::mcp::peers_context(ctx) {
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": context,
            }
        });
        println!("{out}");
    }
    Ok(())
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
        "{name} is locked by claude #{owner}{doing} (editing it now). This is normal \
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
        "{name} is being edited concurrently by claude #{owner}{doing}. Proceeding anyway: \
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
