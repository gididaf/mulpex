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

/// Standing hub instructions injected into every instance via
/// `--append-system-prompt` (see the old term_session.rs — unchanged).
const HUB_RULES: &str = "You are one of several parallel Claude Code instances that Mulpex is \
running in this SAME directory at the same time. VOCABULARY, used throughout: a PROJECT is one \
directory Mulpex has open (a tab); an INSTANCE is one row in its sidebar — a claude, written \
claude#1, claude#2, or a terminal, written term#1, term#5. Refer to them that way. A shared \
coordination hub is available to you as MCP tools named mcp__mulpex__* . Use them to stay \
consistent with the other instances:\n\
- mcp__mulpex__hub_instances — see every instance's status, current task, and which files it \
holds locks on, plus the OTHER projects open in Mulpex and their instances.\n\
- mcp__mulpex__hub_set_focus — publish what YOU are working on (do this when you start a \
substantial task).\n\
- mcp__mulpex__hub_set_name — name YOUR OWN row in the user's sidebar, after the work you are \
doing (2-5 words, in the language the user writes to you in). Do this once, early, as soon as you \
know what the session is about — an unnamed row falls back to showing the user's last prompt \
verbatim, which is long and goes stale. Rename again only if the work genuinely becomes something \
else. If the user named this instance themselves, their name wins and yours is ignored.\n\
- mcp__mulpex__hub_file_owner — before editing a file others might also touch, check who (if \
anyone) is currently editing it and why.\n\
- mcp__mulpex__hub_send / mcp__mulpex__hub_inbox — message another instance, and read messages \
sent to you. Address one in THIS project by its number (to: \"3\", or equivalently \
\"claude#3\"), or pass to: \"all\" to broadcast to every other instance in this project at once. \
Whatever you send is mandatory reading for each recipient (an instance cannot finish a turn \
holding unread mail), so broadcast only what genuinely concerns them all — for anything \
narrower, name the one instance it affects. You can also message an instance in ANOTHER PROJECT \
— see OTHER PROJECTS below.\n\
- mcp__mulpex__hub_spawn — start NEW instances IN THIS PROJECT, each seeded with its own task \
that it begins immediately. Use this to fan work out in parallel (e.g. one instance per \
ticket/item). It returns the new instances' ids; each is told you spawned it and will hub_send \
its result back to you when done. Max 8 per call — for more, call it again in batches, and prefer \
spawning only as many as the work genuinely needs.\n\
OTHER PROJECTS — Mulpex can have several projects open at once, and the instances in them are \
reachable by message. hub_instances lists them under other_projects with the exact address of \
each one; address them <project>#<n> (e.g. \"central-one#3\") in hub_send's `to`, and a message \
from one arrives with its address as the sender, which is what you reply to. Such a message is \
mandatory reading exactly like a local one. THE CRITICAL DIFFERENCE: an instance in another \
project works in a DIFFERENT DIRECTORY, a different repository and a different git checkout. It \
cannot see your files, your paths mean nothing to it, and none of the shared-working-tree or \
file-lock coordination below applies between you — so anything you send it must be \
SELF-CONTAINED: state the repo, quote the code or the interface rather than pointing at a path, \
and say what you need in full. Use this when work genuinely spans both codebases (a shared API, \
a contract both sides implement, a change that must land in step). Everything else stays \
project-local: to: \"all\" broadcasts only within YOUR project, hub_spawn only creates instances \
here, and you cannot read, edit or run anything over there — ask the instance that lives there \
to do it.\n\
TERMINALS — Mulpex also hosts plain interactive shell terminals in this project, shown in its \
sidebar next to the instances as term#1, term#2 …, and you can both create and drive them:\n\
- mcp__mulpex__hub_terminal_open — open a NEW terminal, optionally starting a command in it. It \
keeps running after the command finishes, so you can reuse it.\n\
- mcp__mulpex__hub_terminal_send — type into a terminal: run a command, answer a prompt the \
command asked, or send a control key (e.g. Ctrl-C to interrupt).\n\
- mcp__mulpex__hub_terminal_read — read a terminal's output. Each read returns only what is NEW \
since YOUR last read of it, so you can follow a long command without re-reading everything; it \
can also wait for new output, and it tells you when a command you sent has finished and with \
what exit code.\n\
- mcp__mulpex__hub_terminal_close — close a terminal you no longer need. Do not close one the \
user opened themselves without being asked to.\n\
REMOTE CLAUDES — mcp__mulpex__hub_remote_open starts a Claude Code instance on ANOTHER MACHINE \
over ssh, inside one of these terminals, and coordinates with it. Use it when work genuinely has \
to happen on a remote server (a deploy, a staging box, a service that only exists there) rather \
than running remote commands one at a time over ssh yourself. Give it the ssh target, the \
directory to work in, and the task; it starts, works autonomously, and SIGNALS you when it is \
done, blocked, or needs an answer. It opens its own terminal by default. Pass terminal_id to use \
one that already exists instead — either an idle terminal at a local shell, or (omitting \
ssh_target) one the USER has already ssh'd in on themselves, which is how a login needing a \
password, a VPN or a jump host gets done. It refuses a terminal that is busy or already running a \
claude, so just try it and read the error. IF THE USER HAS NOT NAMED A TASK (e.g. \"start a claude \
on that server for our next task\"), do NOT ask them what to seed it with and do NOT invent one: open \
it with no task at all, tell them it is up and waiting, and stop. It sits idle at its prompt and you \
give it work later with hub_terminal_send. Ask only when the request itself is genuinely ambiguous. \
That signal reaches you as a hub message like any peer's, so \
DO NOT sit polling it — end your turn and you will be woken. When woken, read what it actually \
did with hub_terminal_read and reply with hub_terminal_send. You can see only its CURRENTLY \
VISIBLE screen — a remote Claude repaints in place rather than scrolling, so whatever ran off the \
top of its window is unreachable to you and new_output stays empty. Ask it for answers that fit \
one screen, and if a reply is missing its start, ask it to re-print that part compactly rather \
than to investigate again. Two things to remember about it: it \
is a TERMINAL (a term#N), not a hub instance, so hub_send can never reach it and it will never \
appear in hub_instances' instance list; and it cannot see your conversation, your files or the user, so a \
task must carry everything it needs. If it asks a question only the user can answer, it is asking \
YOU to go and ask them.\n\
WHEN TO USE A TERMINAL instead of your own Bash tool: your Bash tool is request/response and \
cannot hold a process, so use a terminal for anything LONG-RUNNING or INTERACTIVE — a dev \
server, a watcher, `tail -f`, a REPL or database shell, a build you want to keep an eye on while \
you do other work, or a command that will ask questions partway through. For a quick one-shot \
command that returns promptly, just use Bash; opening a terminal for that is slower and clutters \
the user's sidebar. Terminals are SHARED WITHIN THIS PROJECT: the user opens their own with ⌘⇧T and \
your peer instances can open theirs, and any of you can read or drive any of them — so a terminal \
is also how you inspect a dev server the user started. (Only in this project: a terminal in \
another project is not yours to touch.) They are listed by mcp__mulpex__hub_instances. A terminal \
is NOT a hub instance: it is a shell, not an agent, so hub_send can never reach a term#N — type \
into it with hub_terminal_send instead.\n\
IMPORTANT — file locks are AUTOMATIC and you do not manage them: while another instance is \
editing a file, your edit to it simply WAITS and then goes through on its own as soon as they \
finish (their lock releases when their turn ends). So just make your edit normally — if it \
pauses, that is the hub waiting for the other instance, not an error; let it complete. You must \
NOT try to work around a busy file (no shell/printf/sed/cp writes to it) and must NOT ask the \
user what to do about it — it is handled for you. Only in the rare case an edit is finally \
refused after a long wait should you simply try again or move on to other work; never escalate \
a lock to the user. Use the hub tools to see what others are doing if you want to pick \
independent work meanwhile.\n\
SHARED WORKING TREE — you and the other instances IN THIS PROJECT all run in the SAME working \
directory and the SAME git checkout (an instance in another project does not — it has its own \
tree, and none of this concerns it); this is NOT one git worktree per instance, so you have no \
isolated copy of the files. Any command that changes files tree-wide or rewrites git state therefore hits EVERYONE's \
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
INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; ARMED=\"$MULPEX_STATE_DIR/armed\"; \
mkdir -p \"$INBOX\" \"$ARMED\"; touch \"$ARMED/$MULPEX_INSTANCE_ID\"; \
prev=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d ' '); while true; do \
cur=$(ls -1 \"$INBOX\" 2>/dev/null | wc -l | tr -d ' '); \
if [ \"$cur\" -gt \"$prev\" ]; then echo \"mulpex: $((cur - prev)) new hub message(s)\"; fi; \
prev=$cur; sleep 1; done\n\
WHEN TO ARM: as soon as you start working. You are NOT prompted to arm it by a separate startup \
turn; instead, on your first turn Mulpex injects a hidden reminder (and repeats it each turn ONLY \
until the listener is armed). When you see that reminder, arm the Monitor QUIETLY as part of the \
same turn — do not make arming your whole response and do not announce it beyond a brief mention — \
then carry on with whatever I asked. The `touch` in the command above is what records that you \
are armed, so the reminder stops. \
Once armed, a peer message shows up as a Monitor event whose line starts with \"mulpex:\" \
(for example \"mulpex: 1 new hub message(s)\") — that is a peer message arriving, NOT something \
I typed. When it happens, handle it immediately and autonomously: (1) call \
mcp__mulpex__hub_inbox to read and clear the message(s); (2) act on them yourself — a message \
may ask you to do something, may coordinate, or may just inform you; use your judgment and carry \
it out; (3) reply to the sender via mcp__mulpex__hub_send ONLY if it genuinely adds value (they \
asked a question, or would want confirmation) — never send a bare acknowledgement, which just \
causes needless back-and-forth; (4) because that turn was triggered by the hub and not by me, \
START your visible response with a marker line exactly of the form \"⟳ hub message from \
<sender> →\" — fill in the sender exactly as hub_inbox reported it (claude#2 for an instance \
here, central-one#3 for one in another project) — so that when I look at your pane I can tell \
you acted on a hub message rather than on my prompt, and from whom.";

/// User-mandated zero-assumptions planning discipline (see old term_session.rs).
const PLANNING_RULES: &str = "PLANNING — before you finalize a plan or implement anything, \
identify ALL potential assumptions your plan/implementation would rely on (about requirements, \
scope, file/library choices, edge cases, expected behavior). Use the AskUserQuestion tool to \
verify those assumptions with the user FIRST, so the resulting plan or implementation is \
perfectly aligned with their requirements — aim for zero unverified assumptions. Do not silently \
pick a default on anything that could reasonably go more than one way; ask.";

/// A task another instance handed this one at spawn (`hub_spawn`): who assigned it
/// and the work to do. When present, the fresh session's one-shot injected prompt
/// kicks off the task immediately (see `spawn_prompt`). Listener arming is NOT part
/// of this — every instance arms its listener from the `UserPromptSubmit` hook on
/// its first turn (see `hook.rs`), so a normal instance gets no injected prompt at
/// all and starts clean.
pub struct SpawnTask {
    pub parent_id: usize,
    pub task: String,
}

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

/// How long to wait for a shell to paint its prompt before typing a seeded
/// command in anyway. Nothing like `claude`'s cold start — a shell is up in
/// milliseconds — so this is a backstop, not the expected path.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the recorder publishes an unchanged-but-unpublished screen. The
/// PTY reader thread only runs when there is output, so without this the final
/// chunk of a burst (typically the shell prompt itself) would sit unpublished.
const RECORDER_SETTLE: Duration = Duration::from_millis(200);

/// The one-shot task prompt injected into a `hub_spawn` child's PTY once `claude`
/// is up: its assignment plus a report-back-to-spawner instruction. Returns `None`
/// for a normal (non-spawned) instance — which gets NO injected prompt and starts
/// clean; its hub listener is armed later by the `UserPromptSubmit` hook. Kept to a
/// SINGLE line (the task's whitespace is collapsed, never truncated — the child
/// needs the full task text) and prefixed with the `[mulpex:hub]` sentinel so the
/// hook skips it for the sidebar task (the child is auto-named from the task).
fn spawn_prompt(task: Option<&SpawnTask>) -> Option<String> {
    let t = task?;
    let task = t.task.split_whitespace().collect::<Vec<_>>().join(" ");
    let parent = t.parent_id;
    Some(format!(
        "[mulpex:hub] Begin the following task, which was assigned to you by claude#{parent}: \
         {task} Work on it autonomously through to completion. When you finish — or if you get \
         blocked and need input — use mcp__mulpex__hub_send to send claude#{parent} a concise \
         summary of the outcome."
    ))
}

/// How much of the tail of a child's PTY output we keep for readiness detection.
/// Only needs to span the last repaint of the input box, so a few KB is plenty.
const TAIL_CAP: usize = 8192;

/// After this long we stop waiting for the input box to be positively identified
/// and fall back to the old "painted then went quiet" heuristic. Cold starts get
/// slow when several `claude`s boot at once, so this is generous.
const READY_FALLBACK: Duration = Duration::from_secs(20);

/// Hard ceiling on waiting for readiness — inject anyway past this. Deliberately
/// far above any plausible cold start: an 8s cap here is what let six concurrent
/// spawns each type into a TUI that had no input box yet, losing every task.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// How many times to type the task in before giving up. Each attempt is verified
/// against the child's own status file, so a retry only happens when the previous
/// attempt provably did not submit.
const INJECT_ATTEMPTS: usize = 4;

/// How long to wait for proof that an injected prompt actually submitted.
const VERIFY_WINDOW: Duration = Duration::from_secs(6);

/// Whether `claude`'s interactive input box appears in the recent PTY output.
/// The TUI draws a rounded box (`╭` … `╰`) around the `>` prompt once it is ready
/// to accept typing; before that the pane holds only startup banner text. Matched
/// on the box-drawing characters rather than `>` alone, since `>` shows up in
/// banner and MCP-startup lines too.
///
/// Best-effort only: a false negative just delays until the fallback, and a false
/// positive is caught by `turn_started` verification and retried. Neither loses
/// the task, which is why this can afford to key off TUI chrome that may change
/// between `claude` versions.
fn input_box_ready(tail: &[u8]) -> bool {
    let s = String::from_utf8_lossy(tail);
    // Two chrome styles have both been observed from claude v2.1.235 on the SAME
    // machine, differing by project: a rounded box ('╭' … '╰') and a pair of plain
    // horizontal rules. Requiring the corners alone made this a coin flip — in the
    // project the field report came from, the child emitted ZERO '╭'/'╰' and 408
    // '─', so readiness was never detected and the task was typed in only when
    // READY_TIMEOUT expired — 90 s after the instance appeared in the sidebar, by
    // which point its spawner had long since concluded the task was lost.
    let framed = (s.contains('╭') && s.contains('╰')) || s.contains(RULE_RUN);
    framed && (s.contains('>') || s.contains('❯'))
}

/// A run of the box-drawing horizontal line claude rules the input area with.
/// Long enough that a stray table border in banner text cannot pass for it.
const RULE_RUN: &str = "────────────────";

/// Where a spawned child's task-delivery state is published, so the rest of the
/// system can tell "its task has not landed YET" from "its task never landed".
///
/// Injection runs on a background thread long after `hub_spawn` has answered, and
/// until this existed nothing anywhere recorded how it went: a child that had not
/// been typed into yet was indistinguishable from one whose task was lost — both
/// show `status: waiting` (`mcp::status_of`'s default for a missing status file)
/// and an empty task (the injected prompt is sentinel-prefixed, so
/// `hook::userpromptsubmit` deliberately skips capturing it). A subdir for the
/// usual reason: a bare integer at the state-dir root is scanned as a status file.
pub fn spawn_delivery_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("spawning").join(id.to_string())
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

fn mark_delivery(state_dir: &Path, id: usize, state: &str) {
    let p = spawn_delivery_path(state_dir, id);
    if state.is_empty() {
        let _ = std::fs::remove_file(p);
    } else {
        let _ = std::fs::write(p, state);
    }
}

/// Whether the child has actually begun a turn. The `UserPromptSubmit` hook writes
/// `working` into `state_dir/<id>` the instant a prompt is submitted (see
/// `hook.rs::userpromptsubmit`), so this is positive proof the injected text was
/// received — as opposed to being swallowed by a TUI that wasn't listening yet,
/// which is indistinguishable from success by looking at the PTY alone.
fn turn_started(state_dir: &Path, id: usize) -> bool {
    std::fs::read_to_string(state_dir.join(id.to_string()))
        .map(|s| s.trim() == "working")
        .unwrap_or(false)
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
                cmd.arg(format!("{HUB_RULES}\n{PLANNING_RULES}"));
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
                rows,
                cols,
            )?))),
        };

        let child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id().map(|p| p as libc::pid_t);
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
        // Rolling tail of what the child has painted, so the injector can tell a
        // drawn input box from a still-booting one. Only maintained for spawned
        // children — a normal session never injects, so it pays nothing for this.
        let out_tail: Option<Arc<Mutex<Vec<u8>>>> = initial_task
            .as_ref()
            .map(|_| Arc::new(Mutex::new(Vec::with_capacity(TAIL_CAP))));

        {
            let sink = Arc::clone(&sink);
            let alive = Arc::clone(&alive);
            let saw_output = Arc::clone(&saw_output);
            let last_activity = Arc::clone(&last_activity);
            let out_tail = out_tail.clone();
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
                            if let Some(tail) = &out_tail {
                                if let Ok(mut t) = tail.lock() {
                                    t.extend_from_slice(&buf[..n]);
                                    let overflow = t.len().saturating_sub(TAIL_CAP);
                                    if overflow > 0 {
                                        t.drain(..overflow);
                                    }
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

        // Task injection (`hub_spawn` children only): once `claude` is up and its
        // initial paint has settled, type the child's assigned task in as its first
        // prompt (see `spawn_prompt`). A NORMAL instance gets `None` here and no
        // injection — it starts clean; its hub listener is armed from the
        // `UserPromptSubmit` hook on the user's first real turn. Runs in its own
        // thread so `spawn` returns now.
        if let Some(prompt) = spawn_prompt(initial_task.as_ref()) {
            let writer = Arc::clone(&writer);
            let alive = Arc::clone(&alive);
            let saw_output = Arc::clone(&saw_output);
            let last_activity = Arc::clone(&last_activity);
            let out_tail = out_tail.clone();
            let state_dir: PathBuf = state_dir.to_path_buf();
            thread::spawn(move || {
                // Phase 1 — wait for a *drawn input box*, not merely a quiet pane.
                // A booting `claude` goes quiet for well over a second while it
                // loads MCP servers, so "output then silence" alone mistakes a
                // mid-startup lull for readiness.
                let start = Instant::now();
                loop {
                    thread::sleep(Duration::from_millis(150));
                    if !alive.load(Ordering::Relaxed) {
                        mark_delivery(&state_dir, id, "failed");
                        return; // died during startup — nothing to bootstrap
                    }
                    let elapsed = start.elapsed();
                    let quiet = last_activity.lock().map(|t| t.elapsed()).unwrap_or_default();

                    let box_drawn = out_tail
                        .as_ref()
                        .and_then(|t| t.lock().ok().map(|t| input_box_ready(&t)))
                        .unwrap_or(false);
                    if box_drawn && quiet >= Duration::from_millis(300) {
                        break;
                    }
                    // Fallback for a `claude` whose chrome we no longer recognise.
                    if elapsed >= READY_FALLBACK
                        && saw_output.load(Ordering::Relaxed)
                        && quiet >= Duration::from_millis(600)
                    {
                        break;
                    }
                    if elapsed >= READY_TIMEOUT {
                        break; // ceiling: try anyway rather than never
                    }
                }

                // Phase 2 — type it in, then verify it actually submitted, and retry
                // if it didn't. Verification is what makes this robust: readiness
                // detection can be wrong, but a child that never flipped to `working`
                // provably never received the prompt, so retrying is always correct.
                for attempt in 0..INJECT_ATTEMPTS {
                    if !alive.load(Ordering::Relaxed) {
                        return;
                    }
                    if attempt > 0 {
                        // Clear anything a partial earlier attempt left in the box so
                        // retries can't concatenate into one garbled prompt.
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(b"\x15"); // Ctrl-U, kill line
                            let _ = w.flush();
                        }
                        thread::sleep(Duration::from_millis(250));
                    }

                    // Deliver the prompt text, then submit with a SEPARATE Enter after
                    // a short delay. `claude`'s input treats a fast byte burst as a
                    // paste, and a `\r` at the tail of a paste becomes a literal
                    // newline in the buffer rather than a submit — so the text would
                    // sit in the box unsent. Sending the Enter on its own, once the
                    // paste-coalescing window has closed, registers as a real Enter
                    // keypress and fires it.
                    if let Ok(mut w) = writer.lock() {
                        let _ = w.write_all(prompt.as_bytes());
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

                    let deadline = Instant::now() + VERIFY_WINDOW;
                    while Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(150));
                        if !alive.load(Ordering::Relaxed) {
                            return;
                        }
                        if turn_started(&state_dir, id) {
                            mark_delivery(&state_dir, id, ""); // landed
                            return;
                        }
                    }
                }
                // Every attempt typed the task in and none provably submitted.
                // Say so on disk rather than returning quietly: this thread is the
                // only thing that ever knows the task was lost, and its silence is
                // what made a lost task look like a slow start.
                mark_delivery(&state_dir, id, "failed");
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

    /// The readiness check is what decides whether a spawned child is typed into
    /// in a couple of seconds or only when `READY_TIMEOUT` expires 90 s later.
    /// Both of these frames were captured from a real claude v2.1.235 — the
    /// rounded box in a scratch project, the plain rules in the project the field
    /// report came from, which emitted ZERO rounded corners. Requiring the
    /// corners made the fast path a coin flip decided by the project.
    #[test]
    fn the_input_box_is_recognised_in_both_chrome_styles() {
        let rounded = "╭──────────────────────────────╮\n│ > Try \"edit users.ts\"        │\n╰──────────────────────────────╯";
        let ruled = "────────────────────────────────\n ❯ Try \"edit users.ts to...\"\n────────────────────────────────";
        assert!(input_box_ready(rounded.as_bytes()), "rounded box not detected");
        assert!(input_box_ready(ruled.as_bytes()), "ruled input area not detected");

        // Still-booting output must NOT pass: typing before the box exists is
        // what the retry machinery exists to survive, not something to invite.
        let booting = "Loading MCP servers…\n  connecting > mulpex\n";
        assert!(!input_box_ready(booting.as_bytes()), "a booting pane read as ready");
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
