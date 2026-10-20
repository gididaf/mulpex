//! One session on its own pseudo-terminal — either a Claude Code instance or a
//! plain interactive shell (a "terminal", see `SessionKind`).
//!
//! Unlike the old TUI, we do **not** emulate the terminal here — xterm.js in the
//! frontend is the emulator. The backend is a raw byte pipe: PTY bytes stream to
//! the session's frontend `Channel`, and keystrokes come back via `send`.
//!
//! **The PTY itself is not ours.** Since the daemon (`mulpex-ptyd`, see
//! `ptyd.rs` and docs/ptyd.md) every PTY, child process and shell transcript
//! recorder lives there, so a session outlives an app restart. What stays here
//! is everything Mulpex-specific about a spawn — the argv, the whole
//! environment, the hub files — and a mirror of the output ring.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mulpex_ptyd::client::{Event, Handler};
use mulpex_ptyd::procs;
use tauri::ipc::Channel;

use crate::claude_bin;
use crate::ptyd;

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
        /// A prompt handed over exactly as written — no `[mulpex:hub]` wrapper,
        /// no whitespace collapsing — as if the user had typed it (⌘L Load).
        /// Still argv, never typed into the TUI. Ignored when `initial_task` is set.
        plain_prompt: Option<String>,
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
///
/// The bytes come from the daemon (`mulpex-ptyd`), which owns the PTY; this is
/// the app's mirror of its ring, at the same absolute offsets — which is what
/// lets a session adopted after an app restart (`Session::adopt`) be repainted
/// from a snapshot at an exact offset plus the ring after it.
pub struct OutputSink {
    state: Mutex<SinkState>,
    /// The last stretch of output and whoever is watching it live: Remote
    /// Control phones (shell terminals only) and remote desktops (every kind —
    /// `remote/desktop.rs`). A watcher gets the tail to paint from, then every
    /// byte after it — the same bytes, at the same one geometry, the desktop
    /// xterm got. Taken BEFORE `state` in `push`, so a tap can never see a byte
    /// the channel hasn't been handed, or miss one it has.
    tail: Mutex<Tail>,
    /// How an adopted session's pane is rebuilt on its first attach (instead of
    /// flushing a buffer it never had). `None` for a session spawned this run.
    replay: Mutex<Option<Replay>>,
}

/// How much of a terminal's recent output a phone repaints from.
const TAIL_BYTES: usize = 256 * 1024;
/// A claude's tail: what a remote desktop catches up from between the screen
/// snapshot it was sent and the live stream (`remote/desktop.rs`), and what a
/// pane is rebuilt from after an app restart.
const CLAUDE_TAIL_BYTES: usize = 2 * 1024 * 1024;

fn tail_cap(shell: bool) -> usize {
    // A debug build can shrink the tail, so a remote desktop's snapshot path
    // (`remote/desktop.rs::compose`, the serialized-screen branch) is reached
    // without first printing megabytes.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("MULPEX_DEBUG_TAIL_BYTES").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    if shell {
        TAIL_BYTES
    } else {
        CLAUDE_TAIL_BYTES
    }
}

/// One event in a session's output, for a remote desktop mirroring it: bytes,
/// or the PTY changing size at that point in the stream (its xterm must resize
/// right there, between the bytes drawn for the old size and the new).
#[derive(Debug, PartialEq)]
pub enum TapMsg {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

/// What a remote desktop starts mirroring a session from: the ring as it is
/// now, at absolute stream offsets, then every event after `total` on `rx`.
pub struct DesktopTap {
    /// Absolute offset of `ring[0]` in the session's output since spawn.
    pub start: u64,
    pub ring: Vec<u8>,
    /// Resizes inside the ring: (absolute offset, cols, rows).
    pub resizes: Vec<(u64, u16, u16)>,
    /// The PTY's size at spawn — meaningful when `start == 0`, i.e. the ring
    /// still holds everything since, so replaying it from this size is exact.
    pub init: (u16, u16),
    /// `start + ring.len()`: where `rx` picks up.
    pub total: u64,
    pub rx: std::sync::mpsc::Receiver<TapMsg>,
}

struct Tail {
    cap: usize,
    /// Phones may watch shells only — what reaches a claude from a phone goes
    /// through the desktop's prompt checks instead.
    shell: bool,
    ring: std::collections::VecDeque<u8>,
    start: u64,
    resizes: std::collections::VecDeque<(u64, u16, u16)>,
    init: (u16, u16),
    /// The PTY's size now, (cols, rows).
    size: (u16, u16),
    taps: Vec<std::sync::mpsc::Sender<Vec<u8>>>,
    dtaps: Vec<std::sync::mpsc::Sender<TapMsg>>,
}

impl Tail {
    fn total(&self) -> u64 {
        self.start + self.ring.len() as u64
    }
}

enum SinkState {
    Buffering(Vec<u8>),
    Attached(Channel<String>),
}

/// An xterm's screen as the frontend serialized it just before the previous
/// app restarted (`ptyd::save_snapshots`): everything up to `offset`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub data: String,
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
}

/// What an adopted pane is rebuilt from on its first attach.
#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub snapshot: Option<Snapshot>,
}

/// One message on a local pane's channel. Plain base64 is output (base64 never
/// starts with `{`); a JSON object is a control frame for `terminals.ts`:
/// `snap` resets the xterm to a size (optionally painting a serialized screen)
/// and says which stream offset it now shows; `r` resizes it at that point in
/// the stream.
fn snap_msg(cols: u16, rows: u16, offset: u64, screen: &str) -> String {
    serde_json::json!({"t": "snap", "cols": cols, "rows": rows, "offset": offset, "s": screen}).to_string()
}

fn resize_msg(cols: u16, rows: u16) -> String {
    serde_json::json!({"t": "r", "cols": cols, "rows": rows}).to_string()
}

/// How a rebuilt pane gets from nothing to the live stream, in channel
/// messages. Three cases, in order of preference (docs/ptyd.md):
///
/// 1. A snapshot at an offset the ring still holds: paint it, then the ring
///    from there — exact.
/// 2. The ring still starts at spawn: reset to the spawn size and replay all of
///    it, resizes included — exact.
/// 3. Neither (a crash, after more than a ring of output): replay what there is
///    at the current size, and report `true` so the caller makes the program
///    redraw (`Session::repaint`).
fn compose(t: &Tail, replay: &Replay) -> (Vec<String>, bool) {
    let total = t.total();
    if let Some(s) = replay.snapshot.as_ref().filter(|s| s.offset >= t.start && s.offset <= total) {
        let mut out = vec![snap_msg(s.cols, s.rows, s.offset, &s.data)];
        out.extend(ring_msgs(t, s.offset));
        return (out, false);
    }
    if t.start == 0 {
        let mut out = vec![snap_msg(t.init.0, t.init.1, 0, "")];
        out.extend(ring_msgs(t, 0));
        return (out, false);
    }
    // The size in force at `start` is unknown; the first marker after it (if
    // any) fixes it from there on.
    let mut out = vec![snap_msg(t.size.0, t.size.1, t.start, "")];
    out.extend(ring_msgs(t, t.start));
    (out, true)
}

/// The ring from absolute offset `from`, cut at each resize so the xterm
/// changes size exactly where the PTY did.
fn ring_msgs(t: &Tail, from: u64) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<u8> = t.ring.iter().copied().collect();
    let mut at = from;
    for &(off, cols, rows) in t.resizes.iter().filter(|r| r.0 >= from) {
        let (a, b) = ((at - t.start) as usize, (off - t.start) as usize);
        if b > a {
            out.push(b64encode(&bytes[a..b]));
        }
        out.push(resize_msg(cols, rows));
        at = off;
    }
    let a = (at - t.start) as usize;
    if a < bytes.len() {
        out.push(b64encode(&bytes[a..]));
    }
    out
}

impl OutputSink {
    pub(crate) fn new(shell: bool, init: (u16, u16)) -> Self {
        Self {
            state: Mutex::new(SinkState::Buffering(Vec::new())),
            tail: Mutex::new(Tail {
                cap: tail_cap(shell),
                shell,
                ring: Default::default(),
                start: 0,
                resizes: Default::default(),
                init,
                size: init,
                taps: Vec::new(),
                dtaps: Vec::new(),
            }),
            replay: Mutex::new(None),
        }
    }

    /// Output at absolute offset `at`. Anything this sink already has is
    /// dropped (an adopted session's first live bytes can overlap the ring it
    /// was seeded with); a gap is taken as it comes.
    pub(crate) fn push_at(&self, at: u64, bytes: &[u8]) {
        let total = self.tail.lock().unwrap().total();
        let skip = total.saturating_sub(at) as usize;
        if skip < bytes.len() {
            self.push(&bytes[skip..]);
        }
    }

    /// Called for every chunk of PTY output.
    pub(crate) fn push(&self, bytes: &[u8]) {
        let mut t = self.tail.lock().unwrap();
        t.ring.extend(bytes);
        let over = t.ring.len().saturating_sub(t.cap);
        t.ring.drain(..over);
        t.start += over as u64;
        let start = t.start;
        while t.resizes.front().is_some_and(|r| r.0 < start) {
            t.resizes.pop_front();
        }
        // A watcher that went away dropped its receiver; forget it.
        t.taps.retain(|tx| tx.send(bytes.to_vec()).is_ok());
        t.dtaps.retain(|tx| tx.send(TapMsg::Bytes(bytes.to_vec())).is_ok());
        let mut st = self.state.lock().unwrap();
        match &mut *st {
            SinkState::Attached(ch) => {
                let _ = ch.send(b64encode(bytes));
            }
            SinkState::Buffering(buf) => buf.extend_from_slice(bytes),
        }
    }

    /// Text for the pane only: not output, so not in the ring and not at any
    /// stream offset (an offset-keyed snapshot must never count it).
    fn push_local(&self, bytes: &[u8]) {
        let _t = self.tail.lock().unwrap();
        let mut st = self.state.lock().unwrap();
        match &mut *st {
            SinkState::Attached(ch) => {
                let _ = ch.send(b64encode(bytes));
            }
            SinkState::Buffering(buf) => buf.extend_from_slice(bytes),
        }
    }

    /// The PTY changed size here in the stream.
    pub(crate) fn resized(&self, cols: u16, rows: u16) {
        let mut t = self.tail.lock().unwrap();
        let at = t.total();
        t.resizes.push_back((at, cols, rows));
        t.size = (cols, rows);
        t.dtaps.retain(|tx| tx.send(TapMsg::Resize(cols, rows)).is_ok());
        let st = self.state.lock().unwrap();
        if let SinkState::Attached(ch) = &*st {
            let _ = ch.send(resize_msg(cols, rows));
        }
    }

    /// Start from the daemon's ring (an adopted session), to be rebuilt by
    /// `replay` on the first attach.
    fn seed(&self, start: u64, ring: Vec<u8>, resizes: Vec<(u64, u16, u16)>, init: (u16, u16), size: (u16, u16), replay: Replay) {
        let mut t = self.tail.lock().unwrap();
        t.start = start;
        t.ring = ring.into();
        t.resizes = resizes.into();
        t.init = init;
        t.size = size;
        *self.replay.lock().unwrap() = Some(replay);
    }

    /// Watch this terminal: its recent output now, every new byte after. Under
    /// the one lock, so nothing falls between the two. `None` for a claude.
    pub fn tap(&self) -> Option<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>)> {
        let mut t = self.tail.lock().unwrap();
        if !t.shell {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        t.taps.push(tx);
        Some((t.ring.iter().copied().collect(), rx))
    }

    /// Mirror this session on a remote desktop: the ring and its resizes now,
    /// and every byte and resize after, under the one lock.
    pub fn desktop_tap(&self) -> DesktopTap {
        let mut t = self.tail.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        t.dtaps.push(tx);
        DesktopTap {
            start: t.start,
            ring: t.ring.iter().copied().collect(),
            resizes: t.resizes.iter().copied().collect(),
            init: t.init,
            total: t.total(),
            rx,
        }
    }

    /// Bind the frontend channel: flush anything buffered (or, for an adopted
    /// session, rebuild the pane from its snapshot and the ring), then stream
    /// live. Holding both locks across the swap means no output can interleave
    /// between the catch-up and the live stream. Returns whether the pane
    /// could not be rebuilt exactly and wants a redraw.
    pub fn attach(&self, ch: Channel<String>) -> bool {
        let t = self.tail.lock().unwrap();
        let mut st = self.state.lock().unwrap();
        let mut redraw = false;
        if let Some(replay) = self.replay.lock().unwrap().take() {
            let (msgs, partial) = compose(&t, &replay);
            for m in msgs {
                let _ = ch.send(m);
            }
            // Pane-only text that arrived since (a failure notice).
            if let SinkState::Buffering(buf) = &*st {
                if !buf.is_empty() {
                    let _ = ch.send(b64encode(buf));
                }
            }
            redraw = partial;
        } else if let SinkState::Buffering(buf) = &*st {
            if !buf.is_empty() {
                let _ = ch.send(b64encode(buf));
            }
        }
        *st = SinkState::Attached(ch);
        redraw
    }
}

/// Feeds a session's sink from the daemon's stream. An adopted session's
/// events wait in `early` until the sink has been seeded with the ring they
/// follow.
struct SinkFeed {
    sink: Arc<OutputSink>,
    alive: Arc<AtomicBool>,
    /// The daemon itself went away, taking the session with it — not the
    /// child exiting. `reap_dead` keeps such a claude's record for the next
    /// launch's `--resume`.
    lost: Arc<AtomicBool>,
    activity: Arc<Activity>,
    early: Mutex<Option<Vec<Event>>>,
}

/// When a session last printed, for the seeded-command typer.
#[derive(Default)]
struct Activity {
    saw_output: AtomicBool,
    last: Mutex<Option<Instant>>,
}

impl SinkFeed {
    fn apply(&self, ev: Event) {
        match ev {
            Event::Output(at, bytes) => {
                self.sink.push_at(at, &bytes);
                self.activity.saw_output.store(true, Ordering::Relaxed);
                *self.activity.last.lock().unwrap() = Some(Instant::now());
            }
            Event::Resize(cols, rows) => self.sink.resized(cols, rows),
            Event::Exit => self.alive.store(false, Ordering::Relaxed),
            Event::Lost => {
                self.lost.store(true, Ordering::Relaxed);
                self.alive.store(false, Ordering::Relaxed);
            }
        }
    }

    /// The sink is seeded: apply what waited, then everything as it comes.
    fn release(&self) {
        // Applied under the lock: an event arriving meanwhile waits in
        // `event`, so it cannot overtake the ones queued before it.
        let mut early = self.early.lock().unwrap();
        for ev in early.take().unwrap_or_default() {
            self.apply(ev);
        }
    }
}

impl Handler for SinkFeed {
    fn event(&self, ev: Event) {
        {
            let mut early = self.early.lock().unwrap();
            if let Some(q) = early.as_mut() {
                q.push(ev);
                return;
            }
        }
        self.apply(ev);
    }
}

/// A `claude` or shell on a PTY the daemon owns, streaming to one frontend
/// terminal.
pub struct Session {
    pub id: usize,
    pub session_id: String,
    pub kind: SessionKind,
    /// The daemon's handle for this PTY.
    sid: u32,
    client: Arc<mulpex_ptyd::client::Client>,
    alive: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    sink: Arc<OutputSink>,
    /// The child's pid, captured at spawn so teardown still has it once the
    /// process has gone.
    child_pid: Option<libc::pid_t>,
    /// `kill` already ran: the daemon reaped the child, after which its pid
    /// can be recycled.
    killed: bool,
    rows: u16,
    cols: u16,
}

/// Where a spawn's child runs and with what — the app builds all of it, the
/// daemon only execs it (`env_clear` + exactly this env).
struct Cmd {
    argv: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
}

impl Cmd {
    /// Like `portable_pty::CommandBuilder`, start from OUR environment.
    fn new(prog: impl AsRef<std::ffi::OsStr>) -> Self {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Self { argv: vec![prog.as_ref().to_string_lossy().into_owned()], env }
    }
    fn arg(&mut self, a: impl AsRef<std::ffi::OsStr>) {
        self.argv.push(a.as_ref().to_string_lossy().into_owned());
    }
    fn env(&mut self, k: impl AsRef<std::ffi::OsStr>, v: impl AsRef<std::ffi::OsStr>) {
        self.env.insert(
            k.as_ref().to_string_lossy().into_owned(),
            v.as_ref().to_string_lossy().into_owned(),
        );
    }
    fn env_remove(&mut self, k: &str) {
        self.env.remove(k);
    }
}

/// The daemon's name for (project scratch dir, id): stable across app
/// restarts, so the next app finds its sessions.
pub fn session_key(state_dir: &Path, id: usize) -> String {
    format!("{}/{id}", state_dir.display())
}

impl Session {
    /// Spawn `claude` or a shell in `dir` on a PTY of `rows`x`cols`. The PTY is
    /// the daemon's; its output reaches an `OutputSink` here. Nothing here
    /// emulates the terminal — xterm.js does that.
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
        let client = ptyd::client().map_err(anyhow::Error::msg)?;

        // One match resolves everything kind-specific: the command to run, the
        // scratch dir, and the optional first thing to type in. Everything after
        // this point is kind-agnostic.
        let (cmd, state_dir, session_id, initial_task, seed) = match spec {
            SpawnSpec::Claude {
                settings_path,
                state_dir,
                helper_path,
                session_id,
                resume,
                initial_task,
                plain_prompt,
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
                // The generated one-monitor plugin, written beside those two by
                // `write_state_dir`. Claude Code arms its hub listener itself at
                // session start (and on `--resume`), which is what replaced the
                // model arming a `Monitor` and re-arming it every 30 minutes.
                // See `config::PLUGIN_MONITORS_JSON` for the measurement.
                cmd.arg("--plugin-dir");
                cmd.arg(state_dir.join(mulpex_core::state_dir::PLUGIN_DIR));
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
                } else if let Some(prompt) = plain_prompt {
                    // No delivery verdict or watchdog: this is an ordinary first
                    // turn, captured by `UserPromptSubmit` like a typed one.
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
                // Claude Code's "fullscreen" renderer (alt screen + mouse
                // tracking) rolls out server-side and via an in-app trial. It
                // takes every drag away from xterm, so ⌘⇧P (pins) never sees a
                // selection, and the readers built on the classic screen
                // (`promptbox.ts`) were never measured against it. The env var
                // wins over `/tui fullscreen` and the rollout gate (measured,
                // 2.1.292: `0` emits no ?1049h / ?100xh).
                cmd.env("CLAUDE_CODE_NO_FLICKER", "0");
                // Live Token: point the API at the daemon's proxy, which puts
                // the current token (or the one this claude is pinned to) on
                // every request (`live_token.rs`). Nothing when it is off.
                for (k, v) in crate::live_token::claude_spawn_env(&crate::live_token::route(dir, id))
                    .unwrap_or_default()
                {
                    cmd.env(k, v);
                }
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

        // A shell's transcript, written by the daemon (it has to keep recording
        // while the app restarts): a claude in another process can only read a
        // terminal's output through a file.
        let record = (kind == SessionKind::Shell).then(|| {
            serde_json::json!({
                "log": terminal_log_path(&state_dir, id),
                "screen": terminal_screen_path(&state_dir, id),
                "frames": terminal_frames_path(&state_dir, id),
            })
        });
        let env: Vec<serde_json::Value> = cmd.env.iter().map(|(k, v)| serde_json::json!([k, v])).collect();
        let reply = client
            .request(serde_json::json!({
                "op": "spawn",
                "key": session_key(&state_dir, id),
                "argv": cmd.argv,
                "env": env,
                "cwd": dir,
                "rows": rows,
                "cols": cols,
                "ring_cap": tail_cap(kind == SessionKind::Shell),
                "record": record,
                "meta": meta_of(kind, id, &session_id),
            }))
            .map_err(anyhow::Error::msg)?;
        let sid = reply["sid"].as_u64().unwrap_or(0) as u32;
        let child_pid = reply["pid"].as_i64().map(|p| p as libc::pid_t);

        let sink = Arc::new(OutputSink::new(kind == SessionKind::Shell, (cols, rows)));
        let alive = Arc::new(AtomicBool::new(true));
        let lost = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Activity::default());
        client.register(
            sid,
            Arc::new(SinkFeed {
                sink: Arc::clone(&sink),
                alive: Arc::clone(&alive),
                lost: Arc::clone(&lost),
                activity: Arc::clone(&activity),
                early: Mutex::new(None),
            }),
        );

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

        // A seeded terminal (`hub_terminal_open` with a command): type the command
        // once the shell has printed its prompt. Unlike `claude`, a shell has no
        // TUI to be "not listening yet" — waiting for first output plus a short
        // settle is enough, and the tty's own input buffer covers the rest.
        if let Some(seed) = seed {
            let client = Arc::clone(&client);
            let alive = Arc::clone(&alive);
            let activity = Arc::clone(&activity);
            thread::spawn(move || {
                let start = Instant::now();
                loop {
                    thread::sleep(Duration::from_millis(80));
                    if !alive.load(Ordering::Relaxed) {
                        return;
                    }
                    let quiet = activity.last.lock().unwrap().map(|t| t.elapsed()).unwrap_or_default();
                    if activity.saw_output.load(Ordering::Relaxed) && quiet >= Duration::from_millis(150) {
                        break;
                    }
                    if start.elapsed() >= SHELL_READY_TIMEOUT {
                        break;
                    }
                }
                let mut line = seed.into_bytes();
                line.push(b'\r');
                client.send_input(sid, &line);
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
            sid,
            client,
            alive,
            lost,
            sink,
            child_pid,
            killed: false,
            rows,
            cols,
        })
    }

    /// Take over a session the daemon kept running through an app restart
    /// (`info` is its entry in the daemon's `list`). Its pane is rebuilt on the
    /// first attach from `snapshot` (if the previous app left one) and the
    /// daemon's ring — see `compose`.
    pub fn adopt(id: usize, kind: SessionKind, session_id: &str, info: &serde_json::Value, snapshot: Option<Snapshot>) -> anyhow::Result<Self> {
        use base64::Engine as _;
        let client = ptyd::client().map_err(anyhow::Error::msg)?;
        let sid = info["sid"].as_u64().unwrap_or(0) as u32;
        let shell = kind == SessionKind::Shell;
        let sink = Arc::new(OutputSink::new(shell, (80, 24)));
        let alive = Arc::new(AtomicBool::new(true));
        let lost = Arc::new(AtomicBool::new(false));
        let feed = Arc::new(SinkFeed {
            sink: Arc::clone(&sink),
            alive: Arc::clone(&alive),
            lost: Arc::clone(&lost),
            activity: Arc::new(Activity::default()),
            early: Mutex::new(Some(Vec::new())),
        });
        client.register(sid, feed.clone());
        let a = match client.request(serde_json::json!({"op": "attach", "sid": sid})) {
            Ok(a) => a,
            Err(e) => {
                client.unregister(sid);
                anyhow::bail!(e);
            }
        };
        let pair = |v: &serde_json::Value| (v[0].as_u64().unwrap_or(80) as u16, v[1].as_u64().unwrap_or(24) as u16);
        let ring = base64::engine::general_purpose::STANDARD
            .decode(a["ring"].as_str().unwrap_or(""))
            .unwrap_or_default();
        let resizes: Vec<(u64, u16, u16)> = a["resizes"]
            .as_array()
            .map(|v| {
                v.iter()
                    .map(|r| (r[0].as_u64().unwrap_or(0), r[1].as_u64().unwrap_or(80) as u16, r[2].as_u64().unwrap_or(24) as u16))
                    .collect()
            })
            .unwrap_or_default();
        let cols = a["cols"].as_u64().unwrap_or(80) as u16;
        let rows = a["rows"].as_u64().unwrap_or(24) as u16;
        sink.seed(
            a["start"].as_u64().unwrap_or(0),
            ring,
            resizes,
            pair(&a["init"]),
            (cols, rows),
            Replay { snapshot },
        );
        if a["alive"] == false {
            alive.store(false, Ordering::Relaxed);
        }
        feed.release();
        Ok(Self {
            id,
            session_id: session_id.to_string(),
            kind,
            sid,
            client,
            alive,
            lost,
            sink,
            child_pid: info["pid"].as_i64().map(|p| p as libc::pid_t),
            killed: false,
            rows,
            cols,
        })
    }

    /// The daemon's handle for this PTY.
    pub fn sid(&self) -> u32 {
        self.sid
    }

    /// Tell the daemon who this session is now (it outlives the app, and the
    /// next app identifies it by this). Cheap; called when a claude's
    /// conversation uuid changes under it (`reconcile_session_ids`).
    pub fn publish_meta(&self) {
        let _ = self.client.request(serde_json::json!({
            "op": "set_meta", "sid": self.sid, "meta": meta_of(self.kind, self.id, &self.session_id),
        }));
    }

    /// Bind this session's frontend terminal channel (flushing pre-attach
    /// output, or rebuilding an adopted pane).
    pub fn attach(&self, ch: Channel<String>) {
        if self.sink.attach(ch) {
            self.repaint();
        }
    }

    /// Make the program redraw its whole screen: shrink the PTY by a row for a
    /// moment and give it back, which every full-screen program answers with a
    /// full repaint (SIGWINCH). Used when a pane could not be rebuilt exactly.
    fn repaint(&self) {
        let (sid, rows, cols, client) = (self.sid, self.rows, self.cols, Arc::clone(&self.client));
        if rows < 2 {
            return;
        }
        thread::spawn(move || {
            let _ = client.request(serde_json::json!({"op": "resize", "sid": sid, "rows": rows - 1, "cols": cols}));
            thread::sleep(Duration::from_millis(150));
            let _ = client.request(serde_json::json!({"op": "resize", "sid": sid, "rows": rows, "cols": cols}));
        });
    }

    /// Remote Control: watch this terminal's output (`OutputSink::tap`).
    pub fn tap(&self) -> Option<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>)> {
        self.sink.tap()
    }

    /// A remote desktop mirroring this session (`OutputSink::desktop_tap`).
    pub fn desktop_tap(&self) -> DesktopTap {
        self.sink.desktop_tap()
    }

    /// Forward raw bytes to the child's stdin (from xterm `onData`).
    pub fn send(&mut self, bytes: &[u8]) {
        self.client.send_input(self.sid, bytes);
    }

    /// Resize the PTY so the child re-lays-out. No-op if unchanged. The sink
    /// records the resize when the daemon reports it, at its exact place in
    /// the stream.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.client.request(serde_json::json!({"op": "resize", "sid": self.sid, "rows": rows, "cols": cols}));
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

    /// The session ended because the daemon did, not because its child exited.
    pub fn lost_with_daemon(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// What this terminal is doing: is a foreground command running, and where
    /// is the shell sitting. `None` for a claude, for a dead session, and on any
    /// platform without the probe — a reader must be able to tell "not known"
    /// from "no", so nothing is invented here.
    pub fn shell_state(&self) -> Option<ShellState> {
        if !self.is_shell() || !self.is_alive() {
            return None;
        }
        procs::shell_state_of(self.child_pid?)
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
        self.sink.push_local(format!("\r\n{text}\r\n").as_bytes());
    }
}

/// What the daemon keeps about a session for the next app to recognise it.
fn meta_of(kind: SessionKind, id: usize, session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": if kind == SessionKind::Shell { "shell" } else { "claude" },
        "id": id,
        "session_id": session_id,
    })
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
            #[cfg(target_os = "macos")]
            std::io::ErrorKind::PermissionDenied => format!(
                "macOS is blocking access to {}.\n\
                 Open System Settings ▸ Privacy & Security ▸ Files and Folders, \
                 find Mulpex, and turn on the folder this project lives in \
                 (or grant Full Disk Access), then try again.",
                dir.display()
            ),
            // No TCC off macOS: a denial here is plain Unix permissions.
            #[cfg(not(target_os = "macos"))]
            std::io::ErrorKind::PermissionDenied => format!(
                "Permission denied: you are not allowed to read {}.\n\
                 Check the folder's owner and mode (`ls -ld`), then try again.",
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

    /// Tear the session down. The daemon does the killing — everything on its
    /// terminal, a claude's background process groups, its own group, then the
    /// child, reaped (`mulpex_ptyd::daemon::session::DSession::kill`, with the
    /// reasoning for each step in `mulpex_ptyd::procs`). Waits until it is done.
    pub fn kill(&mut self) {
        if std::mem::replace(&mut self.killed, true) {
            return;
        }
        let _ = self.client.request(serde_json::json!({"op": "kill", "sid": self.sid}));
        // The daemon replies once the child is reaped; its exit event may
        // still be in flight, and nothing is listening for it any more.
        self.alive.store(false, Ordering::Relaxed);
        self.client.unregister(self.sid);
    }
}

pub use procs::{reap_orphaned_listeners, ShellState};

impl Drop for Session {
    fn drop(&mut self) {
        // A restart that keeps every session (`ptyd::restart_detached`) drops
        // them all on the way out; only a real teardown may kill.
        if !ptyd::detaching() {
            self.kill();
        }
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
fn claude_command() -> anyhow::Result<Cmd> {
    let bin = claude_bin::resolve_claude().ok_or_else(|| {
        anyhow::anyhow!(
            "Claude Code CLI not found. Mulpex launches your own `claude`, but no `claude` \
             executable was found on your PATH (searched: {}). Install it from \
             https://code.claude.com, then reopen Mulpex.",
            claude_bin::merged_path()
        )
    })?;
    let mut cmd = Cmd::new(bin);
    base_env(&mut cmd);
    Ok(cmd)
}

/// The environment every PTY child needs, whichever program it is.
fn base_env(cmd: &mut Cmd) {
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
fn shell_command() -> anyhow::Result<Cmd> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        // A Finder-launched bundle has no SHELL either; same fallback the PATH
        // probe uses.
        .unwrap_or_else(|| claude_bin::DEFAULT_SHELL.to_string());
    let mut cmd = Cmd::new(&shell);
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



    /// A Remote Control tap gets the recent output, then every byte after it;
    /// the tail is bounded; a phone that went away is forgotten; and a claude
    /// keeps no tail at all.
    #[test]
    fn a_terminal_tap_gets_the_tail_then_the_stream() {
        let sink = OutputSink::new(true, (80, 24));
        sink.push(b"before ");
        let (tail, rx) = sink.tap().unwrap();
        assert_eq!(tail, b"before ");
        sink.push(b"after");
        assert_eq!(rx.recv().unwrap(), b"after");

        sink.push(&vec![b'x'; TAIL_BYTES + 10]);
        let (tail, _kept) = sink.tap().unwrap();
        assert_eq!(tail.len(), TAIL_BYTES);

        drop(rx);
        sink.push(b"!");
        assert_eq!(sink.tail.lock().unwrap().taps.len(), 1, "the dropped tap is gone");

        assert!(OutputSink::new(false, (80, 24)).tap().is_none());
    }

    /// A desktop tap: the ring at absolute offsets with its resizes, then bytes
    /// and resizes in stream order; a ring that rolled over says where it starts.
    #[test]
    fn a_desktop_tap_carries_offsets_and_resizes() {
        let sink = OutputSink::new(false, (100, 30));
        sink.push(b"abc");
        sink.resized(120, 40);
        sink.push(b"de");
        let t = sink.desktop_tap();
        assert_eq!((t.start, t.total, t.init), (0, 5, (100, 30)));
        assert_eq!(t.ring, b"abcde");
        assert_eq!(t.resizes, vec![(3, 120, 40)]);
        sink.push(b"f");
        sink.resized(90, 20);
        assert_eq!(t.rx.recv().unwrap(), TapMsg::Bytes(b"f".to_vec()));
        assert_eq!(t.rx.recv().unwrap(), TapMsg::Resize(90, 20));

        sink.push(&vec![b'y'; CLAUDE_TAIL_BYTES]);
        let t2 = sink.desktop_tap();
        assert_eq!(t2.start, 6, "the six bytes before rolled off");
        assert_eq!(t2.resizes, vec![(6, 90, 20)], "the resize inside them went too");
        assert_eq!(t2.total, 6 + CLAUDE_TAIL_BYTES as u64);
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
            #[cfg(not(target_os = "macos"))]
            assert!(
                reason.contains("Permission denied") && reason.contains("ls -ld"),
                "the reason gives the user nothing to do about it: {reason}"
            );
            #[cfg(target_os = "macos")]
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
