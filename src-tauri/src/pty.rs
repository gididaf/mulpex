//! One session on its own pseudo-terminal — either a Claude Code instance or a
//! plain interactive shell (a "terminal", see `SessionKind`).
//!
//! Unlike the old TUI, we do **not** emulate the terminal here — xterm.js in the
//! frontend is the emulator. The backend is a raw byte pipe: the PTY reader
//! thread streams bytes to the session's frontend `Channel`, and keystrokes come
//! back via `send`. We keep the parts that are genuinely terminal-agnostic: the
//! process invocation (flags/env) and the process-group teardown.
//!
//! Shell sessions carry one extra thing: a `vtgrid::Recorder` that maintains a
//! plain-text transcript on disk, because a claude instance lives in a different
//! process and can only read a terminal's output through a file.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tauri::ipc::Channel;

use crate::claude_bin;
use crate::vtgrid::Recorder;

/// The spawn-time text and the `hub_spawn` child's first prompt now live in
/// `mulpex_core::rules`, shared with `mpx`. `HUB_RULES` is a two-process contract
/// with `hook.rs` (the arming `touch`) and with `registry.rs` (the address
/// grammar), so it has exactly one copy. Re-exported here because `state.rs`
/// builds a `SpawnTask`.
pub use mulpex_core::rules::SpawnTask;
use mulpex_core::rules::{append_system_prompt, spawn_prompt};

/// What is running on a session's PTY. The two kinds share every mechanism that
/// keys off `(project, id)` — attach, input, resize, close, process-group
/// teardown — and differ only in what gets launched and what the hub knows about
/// it (a terminal is a shell, not an agent: it is never a messageable peer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Claude,
    Shell,
}

/// Everything kind-specific about a spawn, so `Session::spawn` takes one
/// argument instead of a growing tail of claude-only ones.
pub enum SpawnSpec<'a> {
    Claude {
        settings_path: &'a Path,
        state_dir: &'a Path,
        /// Absolute path of `mulpex-helper`. The hooks and MCP server reach it
        /// through the config files; the **listener** reaches it through
        /// `--append-system-prompt`, which is why it has to be here too.
        helper_path: &'a Path,
        session_id: &'a str,
        /// Reopen an existing session id rather than creating it.
        resume: bool,
        initial_task: Option<SpawnTask>,
    },
    Shell {
        state_dir: &'a Path,
        /// A command line to type in once the shell's prompt appears.
        seed: Option<String>,
    },
}

impl SpawnSpec<'_> {
    fn kind(&self) -> SessionKind {
        match self {
            SpawnSpec::Claude { .. } => SessionKind::Claude,
            SpawnSpec::Shell { .. } => SessionKind::Shell,
        }
    }
}

/// Where a shell terminal's plain-text transcript lives, inside the project's
/// scratch dir. Both names contain a `.`, so they can never be mistaken for the
/// bare-integer status files the hub scans for.
pub fn terminal_log_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("terminals").join(format!("{id}.log"))
}

pub fn terminal_screen_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("terminals").join(format!("{id}.screen"))
}

/// Timed snapshots of a full-screen program's screen. See `vtgrid::Recorder`
/// for why a repainting TUI needs its own file: nothing it draws ever scrolls,
/// so the transcript can hold none of it.
pub fn terminal_frames_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("terminals").join(format!("{id}.frames"))
}

/// How long to wait for a shell to paint its prompt before typing a seeded
/// command in anyway. Nothing like `claude`'s cold start — a shell is up in
/// milliseconds — so this is a backstop, not the expected path.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the recorder publishes an unchanged-but-unpublished screen. The
/// PTY reader thread only runs when there is output, so without this the final
/// chunk of a burst (typically the shell prompt itself) would sit unpublished.
const RECORDER_SETTLE: Duration = Duration::from_millis(200);


/// How long to wait for a spawned child to reach its first turn before declaring
/// the task undelivered. The task is on `claude`'s command line from the start, so
/// this only has to cover a cold start plus MCP server load — but that cold start
/// is genuinely slow when several children boot at once, so it stays generous. A
/// `claude` that has not begun a turn after this long is broken, not slow.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a spawned child's task-delivery state is published, so the rest of the
/// system can tell "its task has not landed YET" from "its task never landed"
/// from "it started on the WRONG text". The path shape is a two-process contract
/// (the child's own hook writes the verdict), so it lives in `mulpex_core`.
///
/// Until this existed nothing anywhere recorded how delivery went: a child that
/// had not started yet was indistinguishable from one whose task was lost — both
/// show `status: waiting` (`mcp::status_of`'s default for a missing status file)
/// and an empty task (the prompt is sentinel-prefixed, so
/// `hook::userpromptsubmit` deliberately skips capturing it).
pub fn spawn_delivery_path(state_dir: &Path, id: usize) -> PathBuf {
    mulpex_core::spawn_delivery_path(state_dir, id)
}

/// Mark a spawned child's task as not-yet-delivered. Written synchronously by the
/// spawn path, before `hub_spawn` can answer, so there is no window in which the
/// child looks like an ordinary idle instance.
pub fn mark_delivery_pending(state_dir: &Path, id: usize) {
    let p = spawn_delivery_path(state_dir, id);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, "pending");
}

/// Publish the exact prompt this child was launched with, for its own
/// `UserPromptSubmit` hook to check against what it actually received. See
/// `mulpex_core::spawn_expected_path` for why that check exists.
fn publish_expected_prompt(state_dir: &Path, id: usize, prompt: &str) {
    let p = mulpex_core::spawn_expected_path(state_dir, id);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, prompt);
}

/// Drop both delivery files for an instance — the verdict and the expected
/// prompt. Used when a spawn fails outright and when a session is reaped, so a
/// recycled id never inherits a stale verdict.
pub fn clear_delivery(state_dir: &Path, id: usize) {
    let _ = std::fs::remove_file(spawn_delivery_path(state_dir, id));
    let _ = std::fs::remove_file(mulpex_core::spawn_expected_path(state_dir, id));
}

fn mark_delivery(state_dir: &Path, id: usize, state: &str) {
    if state.is_empty() {
        clear_delivery(state_dir, id);
    } else {
        let _ = std::fs::write(spawn_delivery_path(state_dir, id), state);
    }
}

/// Whether the child's hook has already published a verdict on its own delivery.
/// The hook is the authority — it is the only thing that sees the prompt `claude`
/// actually received — so the watchdog must not overwrite a verdict it reached.
fn delivery_settled(state_dir: &Path, id: usize) -> bool {
    std::fs::read_to_string(spawn_delivery_path(state_dir, id))
        .map(|s| s.trim() != "pending")
        .unwrap_or(true)
}

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

/// A live `claude` or shell process on a PTY, streaming to one frontend terminal.
pub struct Session {
    pub id: usize,
    pub session_id: String,
    pub kind: SessionKind,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    sink: Arc<OutputSink>,
    /// Plain-text transcript on disk. Shell sessions only — it exists so a
    /// claude in another process can read this terminal's output.
    recorder: Option<Arc<Mutex<Recorder>>>,
    /// The child's pid, captured at spawn so teardown still has it once the
    /// process has gone.
    child_pid: Option<libc::pid_t>,
    /// Device number of this session's controlling terminal, latched once the
    /// child is definitely up. `killpg` alone is not enough to tear a session
    /// down — see `kill`.
    tty_dev: Arc<AtomicU32>,
    rows: u16,
    cols: u16,
}

impl Session {
    /// Spawn `claude` or a shell in `dir` on a PTY of `rows`x`cols`. The reader
    /// thread streams raw bytes to an `OutputSink` (and, for a shell, into a
    /// `Recorder`); it never emulates the terminal — xterm.js does that.
    pub fn spawn(
        id: usize,
        dir: &Path,
        rows: u16,
        cols: u16,
        spec: SpawnSpec,
    ) -> anyhow::Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let kind = spec.kind();

        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // One match resolves everything kind-specific: the command to run, the
        // scratch dir, and the optional first thing to type in. Everything after
        // this point is kind-agnostic.
        let (mut cmd, state_dir, session_id, initial_task, seed) = match spec {
            SpawnSpec::Claude {
                settings_path,
                state_dir,
                helper_path,
                session_id,
                resume,
                initial_task,
            } => {
                let mut cmd = claude_command()?;
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
                cmd.arg(append_system_prompt(helper_path));
                // A spawned child's task is handed over as `claude`'s POSITIONAL
                // prompt argument. It used to be TYPED into the child's TUI once
                // that TUI looked ready, and that silently truncated every task
                // over ~1 KB — see `spawn_prompt` for the measurement. argv has
                // no such limit, needs no readiness detection and cannot race the
                // TUI: the prompt is submitted before the process finishes
                // starting, and the `UserPromptSubmit` hook fires with the whole
                // text (verified against a real `claude` at 12,000 characters).
                if let Some(prompt) = spawn_prompt(initial_task.as_ref()) {
                    publish_expected_prompt(state_dir, id, &prompt);
                    cmd.arg(prompt);
                }
                // Each mulpex-spawned `claude` is a genuine TOP-LEVEL session
                // (Mulpex owns its `--session-id`), not a sub-session. If Mulpex
                // itself was launched from inside another Claude Code session,
                // that parent's `CLAUDE_CODE_CHILD_SESSION` marker would be
                // inherited and Claude would disable transcript saving — which
                // silently breaks our `--resume` persistence. Strip the inherited
                // child markers so persistence always works.
                cmd.env_remove("CLAUDE_CODE_CHILD_SESSION");
                cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
                cmd.env("IS_SANDBOX", "1");
                cmd.env("MULPEX_INSTANCE_ID", id.to_string());
                cmd.env("MULPEX_STATE_DIR", state_dir);
                cmd.env(
                    "MULPEX_PROJECT_DIR",
                    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
                );
                (
                    cmd,
                    state_dir.to_path_buf(),
                    session_id.to_string(),
                    initial_task,
                    None,
                )
            }
            SpawnSpec::Shell { state_dir, seed } => (
                shell_command()?,
                state_dir.to_path_buf(),
                // A terminal has no Claude session id, and nothing resumes it.
                String::new(),
                None,
                seed.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            ),
        };
        cmd.cwd(dir);

        // A shell's transcript. Built before the child so the file exists the
        // moment the terminal does — a read that races the first output should
        // return "nothing yet", not a missing-file error.
        let recorder = match kind {
            SessionKind::Claude => None,
            SessionKind::Shell => Some(Arc::new(Mutex::new(Recorder::new(
                terminal_log_path(&state_dir, id),
                terminal_screen_path(&state_dir, id),
                terminal_frames_path(&state_dir, id),
                rows,
                cols,
            )?))),
        };

        let child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id().map(|p| p as libc::pid_t);
        // Publish the child's pid so its hub listener can tell when it dies.
        //
        // Nothing else can tell it. `Session::kill` `killpg`s this pid's process
        // group and sweeps its controlling terminal, and the listener is in
        // neither: Claude Code runs each background command in its own process
        // group with no controlling tty, so it survives ⌘W, a crash and app
        // teardown alike and is reparented to launchd still spinning (measured —
        // six such orphans found alive at once, the oldest over a day old). The
        // listener therefore has to notice on its own, and a pid it can probe is
        // the smallest thing that lets it. Claude sessions only: a shell terminal
        // is never a hub peer and has no listener.
        if let (SessionKind::Claude, Some(pid)) = (kind, child_pid) {
            let dir = state_dir.join(mulpex_core::PIDS_DIR);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(id.to_string()), pid.to_string());
        }
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let master = pair.master;

        let sink = Arc::new(OutputSink::new());
        let alive = Arc::new(AtomicBool::new(true));
        // Latched by the reader thread on first output. It can't be read here:
        // `spawn_command` has returned from the fork, but the child sets its
        // controlling terminal in the forked half and may not have got there
        // yet. First output is proof it has.
        let tty_dev = Arc::new(AtomicU32::new(0));
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
            let recorder = recorder.clone();
            let tty_dev = Arc::clone(&tty_dev);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            sink.push(&buf[..n]);
                            if tty_dev.load(Ordering::Relaxed) == 0 {
                                // The child has painted, so it definitely owns
                                // the tty by now. Latched here so teardown still
                                // knows the device after the child is gone.
                                if let Some(dev) = child_pid.and_then(tty_dev_of) {
                                    tty_dev.store(dev, Ordering::Relaxed);
                                }
                            }
                            saw_output.store(true, Ordering::Relaxed);
                            if let Ok(mut t) = last_activity.lock() {
                                *t = Instant::now();
                            }
                            if let Some(rec) = &recorder {
                                if let Ok(mut r) = rec.lock() {
                                    r.push(&buf[..n]);
                                }
                            }
                        }
                    }
                }
                // EOF on the master is the process's death certificate. Note we
                // deliberately do NOT `wait()` the child here: leaving it a zombie
                // keeps its pid unrecyclable, which is what makes the `killpg` in
                // `Drop`/`teardown_all` safe for a terminal that may sit in the
                // list for a long time after exiting.
                if let Some(rec) = &recorder {
                    if let Ok(mut r) = rec.lock() {
                        r.finish();
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        // Publish the recorder's pending state on a timer. The reader thread only
        // runs when there is output, so the last chunk of a burst — usually the
        // shell prompt itself — would otherwise stay unpublished indefinitely.
        if let Some(rec) = &recorder {
            let rec = Arc::clone(rec);
            let alive = Arc::clone(&alive);
            thread::spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    thread::sleep(RECORDER_SETTLE);
                    if let Ok(mut r) = rec.lock() {
                        r.settle();
                    }
                }
            });
        }

        // A seeded terminal (`hub_terminal_open` with a command): type the command
        // once the shell has printed its prompt. Unlike `claude`, a shell has no
        // TUI to be "not listening yet" — waiting for first output plus a short
        // settle is enough, and the tty's own input buffer covers the rest.
        if let Some(seed) = seed {
            let writer = Arc::clone(&writer);
            let alive = Arc::clone(&alive);
            let saw_output = Arc::clone(&saw_output);
            let last_activity = Arc::clone(&last_activity);
            thread::spawn(move || {
                let start = Instant::now();
                loop {
                    thread::sleep(Duration::from_millis(80));
                    if !alive.load(Ordering::Relaxed) {
                        return;
                    }
                    let quiet = last_activity.lock().map(|t| t.elapsed()).unwrap_or_default();
                    if saw_output.load(Ordering::Relaxed) && quiet >= Duration::from_millis(150) {
                        break;
                    }
                    if start.elapsed() >= SHELL_READY_TIMEOUT {
                        break;
                    }
                }
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(seed.as_bytes());
                    let _ = w.write_all(b"\r");
                    let _ = w.flush();
                }
            });
        }

        // Delivery watchdog (`hub_spawn` children only). The task is already on
        // `claude`'s command line by the time we get here, so nothing has to be
        // typed and there is no readiness to detect.
        //
        // The child's own `UserPromptSubmit` hook publishes the verdict, because it
        // is the only thing in the system that sees the prompt `claude` ACTUALLY
        // received — this thread cannot tell a correct turn from a turn started on
        // mangled text, and the old code's habit of calling the first one success
        // is exactly how a truncated brief was reported as delivered. So the
        // watchdog covers only the case the hook cannot: a child that never reaches
        // its first turn at all, because it died starting up or simply never got
        // there. Silence is not success — an unresolved wait ends in `failed`.
        //
        // A NORMAL instance gets `None` here and no watchdog: it starts clean, and
        // its hub listener is armed from the `UserPromptSubmit` hook on the user's
        // own first turn.
        if initial_task.is_some() {
            let alive = Arc::clone(&alive);
            let state_dir: PathBuf = state_dir.to_path_buf();
            thread::spawn(move || {
                let deadline = Instant::now() + DELIVERY_TIMEOUT;
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(150));
                    if delivery_settled(&state_dir, id) {
                        return; // the hook has ruled; it outranks us
                    }
                    if !alive.load(Ordering::Relaxed) {
                        // Died before its first turn. One last look: the hook may
                        // have ruled in the instant before the exit.
                        if !delivery_settled(&state_dir, id) {
                            mark_delivery(&state_dir, id, "failed");
                        }
                        return;
                    }
                }
                if !delivery_settled(&state_dir, id) {
                    mark_delivery(&state_dir, id, "failed");
                }
            });
        }

        Ok(Self {
            id,
            session_id,
            kind,
            writer,
            master,
            child,
            alive,
            sink,
            recorder,
            child_pid,
            tty_dev,
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

    /// Resize the PTY so the child re-lays-out. No-op if unchanged.
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
        // The recorder's grid has to track the child's idea of the screen, or
        // cursor-addressed redraws land on the wrong rows.
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resize(rows, cols);
            }
        }
    }

    /// This session's PTY geometry, `(cols, rows)`. The frontend's xterm for it
    /// must match, or the pane is corrupted permanently — see `terminals.ts`.
    #[cfg(test)]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn is_shell(&self) -> bool {
        self.kind == SessionKind::Shell
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// What this terminal is doing: is a foreground command running, and where
    /// is the shell sitting. `None` for a claude, for a dead session, and on any
    /// platform without the probe — a reader must be able to tell "not known"
    /// from "no", so nothing is invented here.
    pub fn shell_state(&self) -> Option<ShellState> {
        if !self.is_shell() || !self.is_alive() {
            return None;
        }
        shell_state_of(self.child_pid?)
    }

    /// Write a line of Mulpex's own text into this session's pane.
    ///
    /// The pane is an xterm fed only by the PTY, so text the *app* wants to say
    /// about a session has nowhere else to appear — and the one moment it has
    /// something worth saying is when the child died before it was ever usable.
    /// Routing it through the same sink means it lands after whatever the child
    /// printed on its way out (an error of its own, usually) rather than
    /// replacing it, and it works whether the frontend has attached yet or not:
    /// a session that dies during startup restore does so long before its xterm
    /// exists, and `Buffering` holds the notice until `attach_session` flushes.
    pub fn notice(&self, text: &str) {
        // CRLF, not LF: the PTY is in raw mode, so a bare newline moves down a
        // row without returning to column 0 and the next line starts staircased
        // under the end of this one.
        self.sink.push(format!("\r\n{text}\r\n").as_bytes());
    }
}

/// Why `dir` cannot be used as a working directory, in words for the user — or
/// `None` if it is usable.
///
/// This exists because of a failure with no other symptom. macOS TCC protects
/// `~/Documents`, `~/Desktop` and `~/Downloads`, and a bundle only gets in once
/// the user allows it; a denial is recorded per bundle id and **never asked
/// about again**. Mulpex still spawns `claude` with `cwd` set to the project,
/// and the child then cannot even resolve its own directory:
///
/// ```text
/// getcwd: cannot access parent directories: Operation not permitted
/// ```
///
/// `claude` exits 1 within the same second, so the pane shows an error for
/// roughly 100 ms and the session is gone — indistinguishable from "Claude
/// refuses to start". Every project under `~/Documents` fails at once, which is
/// most people's entire project list. Reading the directory is the same
/// permission the child needs, so asking here answers the question before the
/// spawn instead of after it.
///
/// Deliberately *not* called on the restore path: a blocked restore should still
/// produce a session row that says why (see `Core::reap_dead`), whereas ⌘T can
/// refuse up front and say so immediately.
pub fn dir_access_error(dir: &Path) -> Option<String> {
    match std::fs::read_dir(dir) {
        Ok(_) => None,
        Err(e) => Some(match e.kind() {
            std::io::ErrorKind::PermissionDenied => format!(
                "macOS is blocking access to {}.\n\
                 Open System Settings ▸ Privacy & Security ▸ Files and Folders, \
                 find Mulpex, and turn on the folder this project lives in \
                 (or grant Full Disk Access), then try again.",
                dir.display()
            ),
            std::io::ErrorKind::NotFound => {
                format!("{} no longer exists.", dir.display())
            }
            _ => format!("{} cannot be opened: {e}", dir.display()),
        }),
    }
}

impl Session {

    /// Tear the session down: everything attached to its terminal, then its
    /// process group, then the direct child (reaped, so nothing is left
    /// `<defunct>`). Called explicitly for deterministic teardown on app quit
    /// before the scratch dir is removed, and again from `Drop`.
    ///
    /// **`killpg` alone is not enough, and the gap is only visible with a
    /// shell.** A `claude` is a node process and does no job control, so
    /// everything its Bash tool spawns inherits its process group and one
    /// `killpg` reaches the lot. An interactive shell *does* do job control and
    /// puts every job in its own process group, which `killpg(shell_pgid)`
    /// cannot reach. A foreground job still dies — dropping the master hangs up
    /// the tty's foreground group — but a backgrounded `cmd &` is in neither,
    /// and measured, it survived both the close and app quit. Giving the shell a
    /// grace period after SIGHUP so it could hup its own jobs does not fix it
    /// (measured at 150 ms and 400 ms).
    ///
    /// So the sweep is by **controlling terminal**: every process still attached
    /// to this session's tty, which is exactly its descendants and nothing else,
    /// since the device is ours for as long as the master is open. That is also
    /// what a terminal emulator does when you close a tab with jobs running.
    pub fn kill(&mut self) {
        // Ours only while `self.master` is still open — which it is, since Drop
        // runs this before dropping the fields. Once the device is released the
        // kernel can hand the same number to someone else's pty.
        let dev = self.tty_dev.load(Ordering::Relaxed);
        let dev = if dev != 0 {
            Some(dev)
        } else {
            self.child_pid.and_then(tty_dev_of)
        };
        if let Some(dev) = dev {
            kill_tty_session(dev);
        }

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

/// What a shell terminal is doing right now, as far as the kernel knows.
///
/// `running` on a terminal only ever meant "the shell process is alive", which
/// is not the question a reader usually has — a shell sitting at its prompt and
/// a shell three minutes into a build are both `running: true`, and from the
/// transcript alone the two are indistinguishable for a terminal the *user* is
/// driving (a command Mulpex submitted itself is tracked by its completion
/// marker, but nothing typed by hand is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellState {
    /// A foreground command is running in this terminal.
    pub command_running: bool,
    /// The shell's own working directory. Unaffected by a foreground job, which
    /// cannot change it.
    pub cwd: Option<String>,
}

/// Ask the kernel what `pid`'s shell is doing. macOS-only, `None` elsewhere —
/// the same shape as the tty sweep, and `None` means "not known", never "no".
#[cfg(target_os = "macos")]
fn shell_state_of(pid: libc::pid_t) -> Option<ShellState> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // `e_tpgid` is the process group the tty currently has in the FOREGROUND;
    // `pbi_pgid` is the shell's own. An interactive shell puts every job it runs
    // into a process group of its own and hands the terminal to it, so the two
    // differing IS a foreground command running — no heuristics, no parsing the
    // prompt. A shell with no controlling terminal reports 0, which is not a
    // disagreement worth reporting.
    let command_running = info.e_tpgid != 0 && info.e_tpgid != info.pbi_pgid;
    Some(ShellState {
        command_running,
        cwd: cwd_of(pid),
    })
}

#[cfg(not(target_os = "macos"))]
fn shell_state_of(_pid: libc::pid_t) -> Option<ShellState> {
    None
}

/// `pid`'s current working directory.
#[cfg(target_os = "macos")]
fn cwd_of(pid: libc::pid_t) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // `vip_path` is a flat NUL-terminated buffer that `libc` declares as nested
    // arrays to keep an old rustc happy; read it as the C string it is.
    let raw = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = raw.to_str().ok()?;
    (!path.is_empty()).then(|| path.to_owned())
}

/// The device number of `pid`'s controlling terminal, or `None` if it has none
/// (or is gone). Still readable while the process is a zombie.
#[cfg(target_os = "macos")]
fn tty_dev_of(pid: libc::pid_t) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    // NODEV is -1; 0 means "no controlling terminal".
    if n == size && info.e_tdev != u32::MAX && info.e_tdev != 0 {
        Some(info.e_tdev)
    } else {
        None
    }
}

/// SIGKILL every process whose controlling terminal is `dev`.
///
/// Deliberately excludes this process: Mulpex launched from a terminal has a
/// controlling tty of its own, and while that can never be one of our PTYs, the
/// check costs nothing and makes the blast radius obvious.
#[cfg(target_os = "macos")]
fn kill_tty_session(dev: u32) {
    let me = std::process::id() as libc::pid_t;
    for pid in all_pids() {
        if pid <= 0 || pid == me {
            continue;
        }
        if tty_dev_of(pid) == Some(dev) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// `PROC_ALL_PIDS` from `<sys/proc_info.h>`; the `libc` crate exposes
/// `proc_listpids` but not this constant.
#[cfg(target_os = "macos")]
const PROC_ALL_PIDS: u32 = 1;

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<libc::pid_t> {
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let slot = std::mem::size_of::<libc::pid_t>() as libc::c_int;
    // Headroom: processes can appear between sizing the buffer and filling it.
    let cap = (bytes / slot) as usize + 64;
    let mut pids = vec![0 as libc::pid_t; cap];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (cap * slot as usize) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    pids.truncate((bytes / slot) as usize);
    pids
}

/// `KERN_PROCARGS2` from `<sys/sysctl.h>`. Returns argc, then the executable
/// path, then the argv strings, then the environment — all NUL-separated in one
/// blob. We do not parse it: every string we look for is distinctive enough that
/// a substring search over the whole blob is both sufficient and immune to the
/// layout's padding rules.
#[cfg(target_os = "macos")]
const KERN_PROCARGS2: libc::c_int = 49;

/// A process's command line, as one lossy string with NUL turned into newline.
/// `None` when the process is gone or is not ours to inspect.
///
/// **Argv only, in practice.** `KERN_PROCARGS2` is documented to return the
/// environment after the arguments, and for our own children it does not: a
/// legacy listener's blob came back 756 bytes — exactly its `zsh -c` line, with
/// no `MULPEX_*` in it (measured against a live orphan). So nothing here may be
/// keyed on an environment variable; a legacy listener's argv names
/// `$MULPEX_STATE_DIR` unexpanded and cannot tell you *which* state dir it serves.
/// That is why `reap_orphaned_listeners` keys on the parent instead.
#[cfg(target_os = "macos")]
fn argv_of(pid: libc::pid_t) -> Option<String> {
    // The buffer has to be `KERN_ARGMAX`, not whatever a sizing call reports.
    // Asking `KERN_PROCARGS2` for its size with a null buffer answers 32 — enough
    // for `sleep 60` and nothing else (measured) — so a sized fetch silently
    // returns a *truncated* command line. That cost a sweep that matched nothing
    // at all while looking like it worked.
    let mut argmax: libc::c_int = 0;
    let mut argmax_len = std::mem::size_of::<libc::c_int>();
    let mut argmax_mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let rc = unsafe {
        libc::sysctl(
            argmax_mib.as_mut_ptr(),
            argmax_mib.len() as libc::c_uint,
            &mut argmax as *mut _ as *mut libc::c_void,
            &mut argmax_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }

    let mut mib = [libc::CTL_KERN, KERN_PROCARGS2, pid];
    let mut size = argmax as usize;
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    // NUL is the separator; turn the blob into one searchable line.
    for b in buf.iter_mut() {
        if *b == 0 {
            *b = b'\n';
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A process's parent pid, or `None` if it is gone.
#[cfg(target_os = "macos")]
fn ppid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid as libc::pid_t)
}

/// SIGKILL every **orphaned** hub listener on this machine, and report how many.
///
/// This is the one cleanup no other code path can do. A listener runs in its own
/// process group with no controlling terminal (measured: `PGID == pid`, `SESS 0`,
/// state `Ss`), so `Session::kill`'s `killpg` misses it and so does
/// `kill_tty_session`. It outlives its `claude`, outlives app teardown, and is
/// reparented to launchd still spinning `sleep 1` — six were found alive on one
/// machine at once, the oldest more than a day old, belonging to a Mulpex that had
/// exited the previous morning. `listen.rs` makes new listeners exit by
/// themselves; this reaches the ones that predate that and the legacy shell loops
/// that will never learn to.
///
/// **Orphanhood is the parent, not the path.** The obvious test — "does it name a
/// scratch root that is dead?" — cannot be written: a legacy listener's argv
/// spells its state dir as the literal `$MULPEX_STATE_DIR`, and the expanded value
/// lives only in its environment, which `KERN_PROCARGS2` does not hand back for
/// our own children (measured). `ppid == 1` is both available and exactly the
/// signature that was observed. It is also safe by construction: a listener
/// serving a live instance is a child of that `claude`, in this Mulpex or in
/// another one or under `mpx`, and is therefore never matched.
/// Split out from the kill so the selection can be tested without a test run
/// SIGKILLing whatever orphans happen to be on the developer's machine.
#[cfg(target_os = "macos")]
fn orphaned_listener_pids() -> Vec<libc::pid_t> {
    let me = std::process::id() as libc::pid_t;
    all_pids()
        .into_iter()
        .filter(|&pid| pid != me && pid > 1 && ppid_of(pid) == Some(1))
        .filter(|&pid| {
            // One matcher, shared with the hook that excuses a listener from the
            // "still working" count — two spellings of "this is a listener" is how
            // one of them silently stops recognising the other.
            argv_of(pid).is_some_and(|argv| mulpex_core::hook::command_is_hub_listener(&argv))
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn reap_orphaned_listeners() -> usize {
    let pids = orphaned_listener_pids();
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    pids.len()
}

#[cfg(not(target_os = "macos"))]
pub fn reap_orphaned_listeners() -> usize {
    0
}

#[cfg(not(target_os = "macos"))]
fn tty_dev_of(_pid: libc::pid_t) -> Option<u32> {
    None
}

#[cfg(not(target_os = "macos"))]
fn kill_tty_session(_dev: u32) {}

impl Drop for Session {
    fn drop(&mut self) {
        // `claude` (Node) setsids into its own group and spawns helpers; kill the
        // whole process group, not just the direct pid, so nothing is orphaned.
        self.kill();
    }
}

// ---- claude binary resolution ----

/// Launch whatever `claude` the user has installed, with no modifications — the
/// exact binary they'd get from a stock `claude` invocation.
///
/// Resolved to an **absolute path** rather than left as a bare name: a
/// Finder-launched bundle inherits only LaunchServices' default `PATH`, which
/// omits `~/.local/bin` where the installer puts `claude` (see `claude_bin`).
/// The same reconstructed `PATH` is handed to the child so tools it runs
/// (`node`, `git`, Homebrew) resolve as they do in the user's terminal.
fn claude_command() -> anyhow::Result<CommandBuilder> {
    let bin = claude_bin::resolve_claude().ok_or_else(|| {
        anyhow::anyhow!(
            "Claude Code CLI not found. Mulpex launches your own `claude`, but no `claude` \
             executable was found on your PATH (searched: {}). Install it from \
             https://code.claude.com, then reopen Mulpex.",
            claude_bin::merged_path()
        )
    })?;
    let mut cmd = CommandBuilder::new(bin);
    base_env(&mut cmd);
    Ok(cmd)
}

/// The environment every PTY child needs, whichever program it is.
fn base_env(cmd: &mut CommandBuilder) {
    // `portable_pty` passes OUR environment through — and a Finder-launched
    // bundle's environment is LaunchServices' bare one, which never saw a login
    // shell. So the rc files' exports have to be reconstructed and handed over
    // explicitly, or the child simply doesn't have them. The sharpest case is
    // authentication: with `CLAUDE_CODE_OAUTH_TOKEN` (or `ANTHROPIC_API_KEY`)
    // exported from `.zshrc` and no `~/.claude/.credentials.json` on disk, every
    // instance opens on "Not logged in · Please run /login" while the user's own
    // terminal is authenticated. A ⌘⇧T terminal never showed it — `$SHELL -l -i`
    // sources the rc files itself — and neither did `tauri dev`, which inherits
    // the launching terminal's environment.
    //
    // What is NOT forwarded (PATH, TERM, the hub identity, the Claude child
    // markers) is `claude_bin::DENY`, next to the reasons.
    let mut has_lang = std::env::var_os("LANG").is_some();
    for (k, v) in claude_bin::forwarded_env() {
        has_lang |= k == "LANG";
        cmd.env(k, v);
    }

    // A Finder-launched bundle inherits only LaunchServices' bare PATH, which
    // omits `~/.local/bin`, Homebrew and every version manager. The child's own
    // tools (`node`, `git`, whatever the user types in a shell) resolve through
    // this, so it has to be the reconstructed one — the login shell's PATH plus
    // the fallback install dirs, which is why it overrides the forwarded value.
    cmd.env("PATH", claude_bin::merged_path());

    // The child talks to **xterm.js**, not to whatever terminal (if any) started
    // Mulpex — so describe that emulator explicitly rather than inheriting.
    // `portable_pty` sets no TERM of its own, and a Finder-launched bundle has
    // none in its environment, which makes `claude` render monochrome. Under
    // `tauri dev` the terminal's own TERM leaked in and hid this.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    // Same story for the locale: terminals export LANG, LaunchServices doesn't,
    // and without it child tools can fall back to ASCII. Only filled in when
    // genuinely absent — from our environment *and* from the forwarded one, or
    // this fallback would overwrite the user's real locale on exactly the launch
    // path that needs it most.
    if !has_lang {
        cmd.env("LANG", "en_US.UTF-8");
    }
}

/// The user's login shell, for a terminal session.
///
/// Deliberately not routed through `claude_command()`: that resolves the Claude
/// CLI and *errors* when it's missing, and a plain terminal must not fail to
/// open because `claude` isn't installed.
fn shell_command() -> anyhow::Result<CommandBuilder> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        // A Finder-launched bundle has no SHELL either; same fallback the PATH
        // probe uses.
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    // `-l` alone is login-but-not-interactive: zsh would skip `.zshrc`, print no
    // prompt, and treat the PTY as a script. Both flags are required.
    cmd.arg("-l");
    cmd.arg("-i");
    base_env(&mut cmd);

    // A terminal must NOT inherit a hub identity. `portable_pty` passes the
    // parent environment through, so if Mulpex was itself launched from inside a
    // Mulpex claude (the same scenario the `CLAUDE_CODE_CHILD_SESSION` removal
    // above defends against), a `claude` the user then typed into this terminal
    // would write status files under a *terminal's* id and corrupt the hub.
    cmd.env_remove("MULPEX_INSTANCE_ID");
    cmd.env_remove("MULPEX_STATE_DIR");
    cmd.env_remove("MULPEX_PROJECT_DIR");
    cmd.env_remove("IS_SANDBOX");
    Ok(cmd)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The selection behind the one cleanup nothing else in this file can do —
    /// and the one that has to be *narrow*, because it reaches into the process
    /// table and SIGKILLs, with no undo.
    ///
    /// Both halves are load-bearing and each is the whole test on its own:
    /// **orphaned** without **listener** is every stray daemon on the machine;
    /// **listener** without **orphaned** is every listener still serving a live
    /// instance — in this Mulpex, in a second one, or under `mpx`.
    ///
    /// The obvious third condition, "names a dead scratch root", is deliberately
    /// absent and cannot be added: a legacy listener's argv spells its state dir
    /// as the literal `$MULPEX_STATE_DIR`, and `KERN_PROCARGS2` does not return
    /// the environment that would expand it (measured — see `argv_of`).
    #[cfg(target_os = "macos")]
    #[test]
    fn only_a_parentless_listener_is_reaped() {
        const MARK: &str = "\"/x/mulpex-helper\" listen";
        let pidfile = std::env::temp_dir().join(format!(
            "mulpex-orphan-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // A genuine orphan: the shell we spawn backgrounds a listener-shaped child
        // and exits, so that child is reparented to launchd — exactly how a real
        // one is made, by its `claude` dying underneath it.
        let mut maker = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "/bin/sh -c 'echo $$ > {pf}; while :; do sleep 1; done # {MARK}' & exit 0",
                pf = pidfile.display()
            ))
            .spawn()
            .unwrap();
        let _ = maker.wait();

        // A listener whose parent is alive (us). Reaping this is the failure that
        // would deafen a running instance.
        let mut attached = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("while :; do sleep 1; done # {MARK}"))
            .spawn()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(400));
        let orphan: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("the orphan should have written its pid")
            .trim()
            .parse()
            .unwrap();
        assert_eq!(ppid_of(orphan), Some(1), "the test's orphan must be parentless");

        let reapable = orphaned_listener_pids();
        assert!(reapable.contains(&orphan), "a parentless listener is reapable");
        assert!(
            !reapable.contains(&(attached.id() as libc::pid_t)),
            "a listener whose instance is still alive must never be reaped"
        );

        unsafe { libc::kill(orphan, libc::SIGKILL) };
        let _ = attached.kill();
        let _ = attached.wait();
        let _ = std::fs::remove_file(&pidfile);
    }

    /// The task a spawner writes must reach the child WHOLE. This is the
    /// regression that cost two instances their briefs: the prompt used to be
    /// typed into `claude`'s TUI, which capped it at one tty read — 1022
    /// characters, measured — and both the spawner and the child were told
    /// everything had gone fine. Nothing in this builder may ever cap, elide or
    /// summarise the task; delivery is argv, which has no such limit.
    #[test]
    fn a_long_task_reaches_the_child_whole() {
        // Comfortably past the 1022-character paste cap that used to eat it, and
        // past any plausible "round number" a future cap would pick.
        let task: String = (0..1000).map(|i| format!("{i:05}.")).collect();
        assert_eq!(task.len(), 6000);

        let prompt = spawn_prompt(Some(&SpawnTask {
            parent_id: 2,
            task: task.clone(),
        }))
        .expect("a spawned child gets a prompt");

        assert!(prompt.contains(&task), "the task was altered in transit");
        assert!(prompt.contains("00000."), "the head of the task was lost");
        assert!(prompt.contains("00999."), "the tail of the task was lost");
        assert!(
            prompt.starts_with("[mulpex:hub]"),
            "the sentinel the UserPromptSubmit hook keys off must lead"
        );
        assert!(
            prompt.contains("claude#2"),
            "the child must be told who to report back to"
        );
        // One line: a spawned child's pane stays readable, and `name_from_task`
        // has something sane to auto-name the row from.
        assert!(!prompt.contains('\n'), "the prompt must stay a single line");
    }

    /// A normal (⌘T) instance is NOT a spawned child and must start clean — no
    /// prompt, no auto-started turn. Its hub listener is armed later, by the
    /// `UserPromptSubmit` hook on the user's own first turn.
    #[test]
    fn an_unspawned_instance_gets_no_prompt() {
        assert!(spawn_prompt(None).is_none());
    }

    use std::os::unix::fs::PermissionsExt;

    /// The preflight must actually recognise an unreadable directory, and say
    /// something the user can act on.
    ///
    /// Driven against a real `chmod 000` directory rather than a mocked error,
    /// because the whole point is the errno the filesystem really returns. This
    /// is the same `PermissionDenied` macOS raises for a TCC-protected folder
    /// the app has not been allowed into — the case that made every `claude`
    /// exit 1 in under a second with nothing left on screen to explain it.
    #[test]
    fn an_unreadable_directory_is_reported_before_anything_is_spawned() {
        let dir = std::env::temp_dir().join(format!("mulpex-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Readable: nothing to report, so a spawn goes ahead as normal.
        assert!(
            dir_access_error(&dir).is_none(),
            "a perfectly good directory was reported as unusable"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let reason = dir_access_error(&dir);
        // Root ignores the mode bits, so the denial cannot be staged there. Only
        // that one case is excused — otherwise this must genuinely fire, or the
        // test would pass without ever exercising the path it exists for.
        // SAFETY: `geteuid` is a plain read of the calling process's euid.
        let is_root = unsafe { libc::geteuid() } == 0;
        if !is_root {
            let reason = reason.expect("an unreadable directory was reported as usable");
            assert!(
                reason.contains("blocking access") && reason.contains("Privacy & Security"),
                "the reason gives the user nothing to do about it: {reason}"
            );
            assert!(
                reason.contains(&dir.display().to_string()),
                "the reason does not say which folder: {reason}"
            );
        }

        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that is simply gone must not be reported as a permission
    /// problem — that would send the user into Settings to fix nothing.
    #[test]
    fn a_missing_directory_is_reported_as_missing() {
        let dir = std::env::temp_dir().join("mulpex-definitely-not-here-8d3f1a");
        let _ = std::fs::remove_dir_all(&dir);
        let reason = dir_access_error(&dir).expect("a missing directory is not usable");
        assert!(
            reason.contains("no longer exists"),
            "a missing directory was misreported: {reason}"
        );
    }
}
