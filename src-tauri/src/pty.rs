//! One embedded Claude Code session on its own pseudo-terminal.
//!
//! Unlike the old TUI, we do **not** emulate the terminal here — xterm.js in the
//! frontend is the emulator. The backend is a raw byte pipe: the PTY reader
//! thread streams bytes to the session's frontend `Channel`, and keystrokes come
//! back via `send`. We keep the parts that are genuinely terminal-agnostic: the
//! `claude` invocation (flags/env) and the process-group teardown.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tauri::ipc::Channel;

/// Standing hub instructions injected into every instance via
/// `--append-system-prompt` (see the old term_session.rs — unchanged).
const HUB_RULES: &str = "You are one of several parallel Claude Code instances that Mulpex is \
running in this SAME directory at the same time. A shared coordination hub is available to you \
as MCP tools named mcp__mulpex__* . Use them to stay consistent with the other instances:\n\
- mcp__mulpex__hub_instances — see every instance's status, current task, and which files it \
holds locks on.\n\
- mcp__mulpex__hub_set_focus — publish what YOU are working on (do this when you start a \
substantial task).\n\
- mcp__mulpex__hub_file_owner — before editing a file others might also touch, check who (if \
anyone) is currently editing it and why.\n\
- mcp__mulpex__hub_send / mcp__mulpex__hub_inbox — message another instance, and read messages \
sent to you.\n\
IMPORTANT — file locks are AUTOMATIC and you do not manage them: while another instance is \
editing a file, your edit to it simply WAITS and then goes through on its own as soon as they \
finish (their lock releases when their turn ends). So just make your edit normally — if it \
pauses, that is the hub waiting for the other instance, not an error; let it complete. You must \
NOT try to work around a busy file (no shell/printf/sed/cp writes to it) and must NOT ask the \
user what to do about it — it is handled for you. Only in the rare case an edit is finally \
refused after a long wait should you simply try again or move on to other work; never escalate \
a lock to the user. Use the hub tools to see what others are doing if you want to pick \
independent work meanwhile.\n\
SHARED WORKING TREE — you and the other instances all run in the SAME working directory and the \
SAME git checkout; this is NOT one git worktree per instance, so you have no isolated copy of the \
files. Any command that changes files tree-wide or rewrites git state therefore hits EVERYONE's \
in-progress, uncommitted work at once. Treat the following as DANGEROUS and never run them \
unilaterally: git reset --hard, git checkout . / git restore ., git clean, git stash, switching \
or checking out a different branch, git rebase, git revert, and likewise any non-git \
bulk-destructive command (rm -rf, or a mass find/sed/overwrite across files). Before ANY such \
operation: (1) call mcp__mulpex__hub_instances; (2) if any other instance is live, do NOT run it \
on your own — use mcp__mulpex__hub_send to coordinate with them first, or ask the user what to \
do; (3) even if you are currently the only instance, still ask the user before a \
tree-wide-destructive op, because they may have their own uncommitted work. Prefer \
narrowly-scoped, single-file changes over anything that touches the whole tree.\n\
STALE READS — a parallel instance may change a shared file between when you read it and when \
you edit it. If much happened since your last read of a hot shared file (e.g. main.rs / lib.rs / \
mod.rs or any file you know others also touch) — you dispatched a subagent, ran a long build, or \
many steps passed — RE-READ it right before editing. Editing against a stale read fails with \
\"File has been modified since read\" and costs you a re-read+retry anyway; reading first avoids \
the round-trip and silently picking up the peer's changes.\n\
INCOMING MESSAGES (hub listener) — To be woken when another instance messages you, even while \
you are idle between my prompts, you run a persistent background listener on your inbox. TO ARM \
IT: call the Monitor tool (if it is a deferred tool, load it first via ToolSearch with query \
select:Monitor) with persistent set to true and this EXACT command: \
INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; mkdir -p \"$INBOX\"; \
prev=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d ' '); while true; do \
cur=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d ' '); \
if [ \"$cur\" -gt \"$prev\" ]; then echo \"mulpex: $((cur - prev)) new hub message(s)\"; fi; \
prev=$cur; sleep 1; done\n\
Mulpex will ask you to arm this at the start of a session — do it right away, then just wait. \
Once armed, a peer message shows up as a Monitor event whose line starts with \"mulpex:\" \
(for example \"mulpex: 1 new hub message(s)\") — that is a peer message arriving, NOT something \
I typed. When it happens, handle it immediately and autonomously: (1) call \
mcp__mulpex__hub_inbox to read and clear the message(s); (2) act on them yourself — a message \
may ask you to do something, may coordinate, or may just inform you; use your judgment and carry \
it out; (3) reply to the sender via mcp__mulpex__hub_send ONLY if it genuinely adds value (they \
asked a question, or would want confirmation) — never send a bare acknowledgement, which just \
causes needless back-and-forth; (4) because that turn was triggered by the hub and not by me, \
START your visible response with a marker line exactly of the form \"⟳ hub message from \
#<sender> →\" (fill in the sender's instance number) so that when I look at your pane I can tell \
you acted on a peer message rather than on my prompt. If Mulpex asks you at the start of a \
session to start your hub listener, do so right away — that is what arms this wake mechanism.";

/// User-mandated zero-assumptions planning discipline (see old term_session.rs).
const PLANNING_RULES: &str = "PLANNING — before you finalize a plan or implement anything, \
identify ALL potential assumptions your plan/implementation would rely on (about requirements, \
scope, file/library choices, edge cases, expected behavior). Use the AskUserQuestion tool to \
verify those assumptions with the user FIRST, so the resulting plan or implementation is \
perfectly aligned with their requirements — aim for zero unverified assumptions. Do not silently \
pick a default on anything that could reasonably go more than one way; ask.";

/// One-shot prompt Mulpex injects into a fresh session's PTY (once `claude` is up)
/// to trigger arming the hub listener. The full procedure + exact Monitor command
/// live in `HUB_RULES` (append-system-prompt), so this stays a short one-liner —
/// keeping the visible startup turn compact. MUST begin with `MULPEX_SENTINEL`
/// (`mulpex_core::MULPEX_SENTINEL`, "[mulpex:hub]") so the `UserPromptSubmit` hook
/// skips it for the sidebar task, and MUST stay a single line so it submits as one
/// prompt.
const HUB_LISTENER_BOOTSTRAP: &str = "[mulpex:hub] Arm your hub listener now, exactly as \
described under \"INCOMING MESSAGES\" in your instructions (the persistent Monitor on your \
inbox), then reply \"hub listener armed\" and wait for my next message.";

/// Where a session's PTY output goes. Before the frontend has created its xterm
/// and attached a `Channel`, output is buffered so a restored (`--resume`d)
/// session's initial repaint isn't lost; on attach the buffer is flushed and we
/// switch to live streaming. Bytes are base64-encoded over the channel (a plain
/// `Serialize` payload that survives Tauri IPC without ArrayBuffer plumbing).
pub struct OutputSink {
    state: Mutex<SinkState>,
}

enum SinkState {
    Buffering(Vec<u8>),
    Attached(Channel<String>),
}

impl OutputSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(SinkState::Buffering(Vec::new())),
        }
    }

    /// Called from the reader thread for every chunk of PTY output.
    fn push(&self, bytes: &[u8]) {
        let mut st = self.state.lock().unwrap();
        match &mut *st {
            SinkState::Attached(ch) => {
                let _ = ch.send(b64encode(bytes));
            }
            SinkState::Buffering(buf) => buf.extend_from_slice(bytes),
        }
    }

    /// Bind the frontend channel: flush anything buffered, then stream live.
    /// Holding the lock across the swap means the reader thread can't interleave
    /// a push between flush and attach (it blocks on `push`'s lock).
    pub fn attach(&self, ch: Channel<String>) {
        let mut st = self.state.lock().unwrap();
        if let SinkState::Buffering(buf) = &*st {
            if !buf.is_empty() {
                let _ = ch.send(b64encode(buf));
            }
        }
        *st = SinkState::Attached(ch);
    }
}

/// A live `claude` process on a PTY, streaming to one frontend terminal.
pub struct Session {
    pub id: usize,
    pub session_id: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    sink: Arc<OutputSink>,
    rows: u16,
    cols: u16,
}

impl Session {
    /// Spawn `claude` in `dir` on a PTY of `rows`x`cols`. Mirrors the old
    /// `TermSession::spawn` invocation (flags/env), only the reader thread now
    /// streams raw bytes to an `OutputSink` instead of a vt100 parser. `resume`
    /// reopens an existing session id vs. creating it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: usize,
        dir: &Path,
        rows: u16,
        cols: u16,
        settings_path: &Path,
        state_dir: &Path,
        session_id: &str,
        resume: bool,
    ) -> anyhow::Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = claude_command();
        cmd.arg("--dangerously-skip-permissions");
        if resume {
            cmd.arg("--resume");
        } else {
            cmd.arg("--session-id");
        }
        cmd.arg(session_id);
        cmd.arg("--settings");
        cmd.arg(settings_path);
        cmd.arg("--mcp-config");
        cmd.arg(state_dir.join("mcp.json"));
        cmd.arg("--append-system-prompt");
        cmd.arg(format!("{HUB_RULES}\n{PLANNING_RULES}"));
        // Each mulpex-spawned `claude` is a genuine TOP-LEVEL session (Mulpex owns
        // its `--session-id`), not a sub-session. If Mulpex itself was launched
        // from inside another Claude Code session, that parent's
        // `CLAUDE_CODE_CHILD_SESSION` marker would be inherited and Claude would
        // disable transcript saving — which silently breaks our `--resume`
        // persistence. Strip the inherited child markers so persistence always works.
        cmd.env_remove("CLAUDE_CODE_CHILD_SESSION");
        cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
        cmd.env("IS_SANDBOX", "1");
        cmd.env("MULPEX_INSTANCE_ID", id.to_string());
        cmd.env("MULPEX_STATE_DIR", state_dir);
        cmd.env(
            "MULPEX_PROJECT_DIR",
            std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
        );
        cmd.cwd(dir);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let master = pair.master;

        let sink = Arc::new(OutputSink::new());
        let alive = Arc::new(AtomicBool::new(true));
        // Readiness signals for the one-shot hub-listener bootstrap: the reader
        // thread marks when `claude` first paints (`saw_output`) and when it last
        // emitted (`last_activity`), so the injector can wait until the initial UI
        // has painted and then settled before typing into it.
        let saw_output = Arc::new(AtomicBool::new(false));
        let last_activity = Arc::new(Mutex::new(Instant::now()));

        {
            let sink = Arc::clone(&sink);
            let alive = Arc::clone(&alive);
            let saw_output = Arc::clone(&saw_output);
            let last_activity = Arc::clone(&last_activity);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            sink.push(&buf[..n]);
                            saw_output.store(true, Ordering::Relaxed);
                            if let Ok(mut t) = last_activity.lock() {
                                *t = Instant::now();
                            }
                        }
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        // A2 bootstrap: once `claude` is up and its initial paint has settled, inject
        // a one-shot prompt telling it to arm its hub listener (see
        // `HUB_LISTENER_BOOTSTRAP`). Re-fired on every spawn/`--resume` (a restart
        // kills the previous Monitor), so an instance is always listening for peer
        // messages even while idle. Runs in its own thread so `spawn` returns now.
        {
            let writer = Arc::clone(&writer);
            let alive = Arc::clone(&alive);
            thread::spawn(move || {
                let start = Instant::now();
                loop {
                    thread::sleep(Duration::from_millis(150));
                    if !alive.load(Ordering::Relaxed) {
                        return; // died during startup — nothing to bootstrap
                    }
                    let elapsed = start.elapsed();
                    if elapsed >= Duration::from_secs(8) {
                        break; // cap: inject anyway rather than never
                    }
                    if saw_output.load(Ordering::Relaxed) && elapsed >= Duration::from_millis(700) {
                        let quiet = last_activity.lock().map(|t| t.elapsed()).unwrap_or_default();
                        if quiet >= Duration::from_millis(600) {
                            break; // UI painted then went quiet → ready for input
                        }
                    }
                }
                // Deliver the prompt text, then submit with a SEPARATE Enter after a
                // short delay. `claude`'s input treats a fast byte burst as a paste,
                // and a `\r` at the tail of a paste becomes a literal newline in the
                // buffer rather than a submit — so the text would sit in the box
                // unsent. Sending the Enter on its own, once the paste-coalescing
                // window has closed, registers as a real Enter keypress and fires it.
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(HUB_LISTENER_BOOTSTRAP.as_bytes());
                    let _ = w.flush();
                }
                thread::sleep(Duration::from_millis(400));
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(b"\r");
                    let _ = w.flush();
                }
            });
        }

        Ok(Self {
            id,
            session_id: session_id.to_string(),
            writer,
            master,
            child,
            alive,
            sink,
            rows,
            cols,
        })
    }

    /// Bind this session's frontend terminal channel (flushing pre-attach output).
    pub fn attach(&self, ch: Channel<String>) {
        self.sink.attach(ch);
    }

    /// Forward raw bytes to Claude's stdin (from xterm `onData`). Shares the PTY
    /// writer (behind a mutex) with the one-shot hub-listener bootstrap thread.
    pub fn send(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Resize the PTY so Claude re-lays-out. No-op if unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Kill the whole process group (see `Drop`) explicitly, for deterministic
    /// teardown on app quit before the scratch dir is removed.
    pub fn kill(&mut self) {
        if let Some(pid) = self.child.process_id() {
            let pgid = pid as libc::pid_t;
            unsafe {
                libc::killpg(pgid, libc::SIGHUP);
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // `claude` (Node) setsids into its own group and spawns helpers; kill the
        // whole process group, not just the direct pid, so nothing is orphaned.
        self.kill();
    }
}

// ---- claude binary resolution ----

/// Launch whatever `claude` the user has installed, resolved via `PATH`, with no
/// modifications — the exact binary they'd get from a stock `claude` invocation.
fn claude_command() -> CommandBuilder {
    CommandBuilder::new("claude")
}

/// Standard base64 (no line breaks) — dependency-free, for streaming PTY bytes
/// to the frontend over a `Channel<String>`.
fn b64encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
