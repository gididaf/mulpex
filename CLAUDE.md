# Mulpex

A CLI tool, opened from inside a project directory, that wraps **multiple live, parallel
Claude Code sessions** — working in the **same directory** (deliberately *not* git
worktrees) — in a coordinated terminal shell:

```
 project · /path/to/project                                         ← top bar
────────────────────────────────────────────────────────────────────
┌──────────────┬───────────────────────────────────────────┐
│ instances    │                                            │
│ (+ each one's│         Claude Code                         │
│  task)       │         (behaves exactly like `claude`)     │
├──────────────┤                                            │
│ info / hub   │                                            │
│ (locks,      │                                            │
│  waiting,    │                                            │
│  msgs)       │                                            │
└──────────────┴───────────────────────────────────────────┘
   left column                  center pane
 (instances over the hub)
 Ctrl+T new · Ctrl+] next · … · Ctrl+Q×2 quit              ← bottom bar
```

Run it from a project directory:

```sh
cd /path/to/project
mulpex
```

## Status (done)

Two-column layout (with full-width **top bar** = project, **bottom bar** = key legend)
hosting **multiple live, fully-interactive Claude Code sessions** for the current project
that **coordinate with each other** through a shared hub (see *The coordination hub*). You
can add instances, switch between them, and exited instances are removed automatically. The
**sessions you worked on are remembered**: quit Mulpex and reopen it in the same project and
it auto-resumes them with their prior conversations (see *Session persistence*).

- **Left column** is split vertically: the **instances list** on top, the **info/hub view**
  below it. The instances list shows all running instances (`claude #N`); the focused one is
  highlighted. Each carries a **status dot** (green `ready`, yellow `working`, red `needs
  you`; see *Status indicators*) and, beneath it, that instance's **current task** (from the
  hub, word-wrapped across up to 3 lines).
- **Center pane** shows the focused instance's live Claude, or a clean "No active Claude"
  hint when none are running. It takes all the width left over by the single left column
  (there is no separate right sidebar anymore).
- **The info/hub view** (lower half of the left column) shows the live coordination state:
  **Locks** (file → holder), **Waiting** (who's ⏳ blocked on whose file), and **Messages** —
  the persistent cross-instance conversation (who→who + a snippet, newest first, with an
  unread count). Press **Ctrl+M** for the full-screen message reader. See *The coordination
  hub*.

### Keybindings

- **Ctrl+T** — new Claude instance (in the project dir)
- **Ctrl+]** — focus next instance (wraps)
- **Ctrl+[** — focus previous instance (wraps) — *Kitty protocol only, see below*
- **Ctrl+M** — open/close the full-screen cross-instance message reader (↑↓/PageUp-Down/wheel
  scroll, Esc/q/Ctrl+M to close) — *Kitty protocol only: `Ctrl+M` IS the Enter byte (`0x0D`),
  so like `Ctrl+[` we only match the disambiguated `Char('m')+CONTROL` the Kitty protocol
  emits; in legacy mode Ctrl+M stays Enter and forwards to Claude untouched.*
- **Ctrl+Q ×2** — quit Mulpex (press twice within 3s; the first press shows a red
  "press Ctrl+Q again to quit" banner on the center pane, which clears after the window lapses)
- **everything else (incl. Ctrl+C, Esc)** — forwarded to the focused Claude
- **mouse wheel** — scrolls Mulpex's scrollback view of the focused Claude
- **left click-drag** — selects text in the center pane; **double-click** selects a word and
  **double-click+drag** extends by whole words; copies to the clipboard on release, with a
  brief `✓ copied N chars` flash in the title (see *Mouse: scrollback + selection*)

**The macOS keybinding minefield (why the keys are what they are):**

- **No Ctrl+arrows.** macOS Mission Control reserves all four Ctrl+arrows system-wide
  (Mission Control / App Exposé / Spaces), so they never reach the app.
- **No Alt+letter.** On macOS, Option+letter produces dead keys / accents (Option+N = `˜`)
  or gets grabbed by the terminal / a hotkey tool (Option+N opened a new iTerm2 window for
  the user). (Option+arrows survive, but we don't use them — navigation lives on Ctrl.)
- **Ctrl+letters are safe** in a terminal (iTerm2's new-window/-tab are ⌘N/⌘T, not Ctrl),
  so the combos live there.
- **`Ctrl+[` *is* Esc.** They are the same byte (`0x1B`). crossterm decodes a legacy
  `Ctrl+[` as `KeyCode::Esc`, indistinguishable from a real Esc — and `Ctrl+]` as
  `Char('5')`, not `Char(']')`. So:
  - `Ctrl+]` (next) matches **both** `Char(']')` (Kitty) and `Char('5')` (legacy) → always works.
  - `Ctrl+[` (prev) matches **only** `Char('[')`, which a terminal emits **only when the
    Kitty keyboard protocol is active**. In legacy mode `Ctrl+[` stays `KeyCode::Esc` and is
    forwarded to Claude — we never hijack Esc. So `Ctrl+[` works only when the protocol is on.

`main.rs` enables the **Kitty keyboard protocol**
(`PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`) when
`supports_keyboard_enhancement()` reports it (e.g. recent iTerm2). When the protocol is
*not* active (legacy mode), `Ctrl+[` (prev) and `Ctrl+M` (message reader) are affected —
both rely on Kitty disambiguation — but `Ctrl+]` (next) still works, so you can cycle forward
through all instances. (There used to be a `[kitty]`/`[legacy]` indicator on the bottom bar
and the `App.keyboard_enhanced` flag behind it; both were removed — the bottom bar is now
just the key legend.)

### Instance lifecycle

- All instances run `claude` in the directory Mulpex was launched from.
- When a Claude exits (Ctrl+C/Ctrl+D out of it, or the process dies), `App::reap_dead`
  removes it from the list and moves focus to a surviving neighbour. When the last one
  exits, the center shows the empty-state hint. Sessions are reaped in the main loop, woken
  by the reader thread flipping the shared `dirty` flag on EOF.
- Each `TermSession` has a stable display `id` (`claude #N`); `App.next_id` only increments.

### Session persistence (restore on restart)

Mulpex remembers the Claude Code sessions you worked on per project, so reopening it in the
same directory **auto-resumes them live** (their prior conversation reappears). Sourced from
Claude Code's own session storage via the `--session-id` / `--resume` CLI flags — *not* by
saving any conversation content ourselves.

- **Assign at spawn.** Every `TermSession` gets a UUID (`persist::new_uuid`, dependency-free
  from `/dev/urandom`) and is launched with `--session-id <uuid>` (fresh) so Mulpex
  deterministically owns each session's id. On restore it's launched with `--resume <uuid>`
  instead, which reopens that exact conversation (`resume` flag on `TermSession::spawn`).
- **Only "worked on" instances are remembered.** An instance is recorded only once a
  lifecycle hook has fired for it — i.e. a prompt was submitted, so a real conversation
  exists on disk. A freshly spawned, never-used instance is *never* saved (this is the
  "instances without a session don't reappear" rule). `App.worked: HashSet<usize>` tracks
  these: `refresh_statuses` adds an id the moment its hook state file appears; restored
  instances start already in the set.
- **The store.** `persist::SessionStore` writes one file per project under
  `~/.mulpex/sessions/<key>.txt` — Mulpex's own dir, separate from Claude's. The `<key>` is a
  readable tail of the project path plus a stable **FNV-1a** hash of the full path (unique,
  bounded length, stable across rebuilds — deliberately *not* `DefaultHasher`, whose output
  isn't stable). The file's first line is `# <project dir>` for collision verification;
  remaining lines are session UUIDs in sidebar order. `App::persist_sessions` rewrites it
  whenever the worked set changes (new hook fired, or `reap_dead` drops a closed instance).
- **Closed = forgotten.** When an instance exits, `reap_dead` prunes it from `worked` and
  re-persists, so it won't come back next launch. Sessions that fail to `--resume` (e.g. the
  transcript was cleaned up after 30 days) simply don't restore and self-heal: they're pruned
  on the next persist. `App::new` reconciles the store on startup (persists what actually
  came back). If nothing is restorable, it starts one fresh instance as before.
- Verified end-to-end with the real `claude` binary (single + multi-instance resume show the
  prior conversation; a never-prompted instance is not remembered).

### Status indicators (WORKING / WAITING / NEEDS YOU)

Each sidebar instance shows what its Claude is doing, sourced from **Claude Code lifecycle
hooks** — *not* by scraping the screen (robust across CC versions).

- At spawn, `App` creates a per-run scratch dir `$TMPDIR/mulpex-<pid>/` and writes one
  static `settings.json` into it (`HOOK_SETTINGS_JSON` in `app.rs`), with `__MULPEX_BIN__`
  substituted to the running binary's absolute path so hooks can invoke `mulpex hook …`.
- Each `TermSession` is launched with `--settings <that file>` plus env
  `MULPEX_INSTANCE_ID=<id>`, `MULPEX_STATE_DIR=<dir>`, `MULPEX_PROJECT_DIR=<canonical>`, so
  **one static settings file serves every instance** (identity lives in the env, not the
  file). Using `--settings` means we never touch the user's project `.claude/settings.json`.
- State machine: `UserPromptSubmit` / `PostToolUse` → `working`; `Stop` → `waiting`;
  `PreToolUse[AskUserQuestion]` and the `permission_prompt` / `idle_prompt` **Notification**
  matchers → `needs`. A fresh instance with no file yet reads as `Waiting` (ready). The
  `AskUserQuestion` and `Notification` matchers are still one-word `printf`s; `UserPromptSubmit`,
  `PostToolUse`, and `Stop` all route through the `mulpex hook` helper (which also drives the
  coordination hub — see below) but keep writing the same status word. (`PostToolUse` and
  `Stop` moved off `printf` so they can also deliver hub mail — see *Message delivery*.)
- `App` polls the state files every ~200ms (`STATUS_POLL`) and on change requests a redraw.
  Most transitions coincide with PTY output (already a redraw trigger); the poll is the
  backstop for the idle notification, which produces no output.
- **Known gap:** `AskUserQuestion` does not reliably fire its own hook, and `Stop` does not
  fire while a question is pending — so a session blocked on a question can read `working`
  until the `idle_prompt` notification arrives (the backstop). `--dangerously-skip-permissions`
  (how we launch) suppresses the permission UI, so the live states are mostly working↔waiting
  plus the idle/question case. Verified: hooks fire with the real `claude` binary
  (`working` mid-turn → `waiting` on Stop).
- Cleanup: `impl Drop for App` clears `instances` (killing every process group, see teardown)
  **before** `remove_dir_all`ing the scratch dir, so no child recreates a state file after.

## The coordination hub (file locking + inner MCP)

The "main event": parallel instances in **one directory** must not clobber each other or
drift out of sync. Two layers handle this, both built on the same env-keyed, file-based IPC
under `$MULPEX_STATE_DIR` as the status dots. The enforcement lives in **`mulpex hook`** and
**`mulpex mcp`** (hidden subcommands of the same binary, dispatched in `main.rs` before
`ratatui::init()`); the TUI only ships+wires them and *observes* the state for the hub view.

### Phase 1 — file-locking coordinator (`hook.rs`)

A per-file **semaphore**, enforced by a `PreToolUse` hook (`mulpex hook pretooluse`). The
lock key is an FNV-1a hash of the canonical absolute path; the lock token is an `O_EXCL`
file under `state_dir/locks/<hash>` (atomic test-and-set) holding `instance=/path=/ts=`
(written via `lock_token`). The `ts` is a **heartbeat**: it's rewritten every time the holder
actually touches that file, so a waiter can tell a live edit from an abandoned one.

- **Edit tools** (`Write`/`Edit`/`MultiEdit`/`NotebookEdit`): acquire the lock before the
  edit. Free or already ours → acquire + allow (and re-stamp the heartbeat); held by another
  → **wait** (see below). Released per-turn by `mulpex hook stop`. An awareness note is
  injected when a *different* instance edited the file earlier this session
  (`state_dir/history/<hash>`).
- **Transparent auto-wait, then proceed — never a hard deny.** A blocked edit/read does **not**
  deny: it **waits** (polling every `LOCK_POLL`) and then proceeds. Two things end the wait
  early so block time tracks *real file activity*, not turn length:
  - **Idle-lease (`LOCK_IDLE` ≈ 30s).** If the holder's heartbeat goes stale (`lock_is_stale`)
    — they acquired the file but moved on to other files this turn — a waiter **reclaims** it:
    it deletes the stale token so the next `O_EXCL` create wins atomically (two racing waiters
    can't both win; `release_my_locks` only deletes locks still owned by `self`, so the old
    holder won't clobber the new one).
  - **Hot holder / stuck-on-user.** If the holder is *continuously* editing the same file for
    the full `LOCK_WAIT` (≈ 4 min) budget, or is itself blocked on the user (`needs`), the
    waiter stops blocking and proceeds **contended** (`allow_contended`) — allowed *with a
    stale-read awareness note* telling the model to re-read the file right now before writing,
    leaning on Claude Code's own "file modified since read" check. There is no `deny` path for
    edits anymore.
  A blocking hook costs **zero model tokens** (the model is idle awaiting the tool result), so
  the wait is near-free. The `--settings` matcher carries a `timeout` *above* `LOCK_WAIT`
  because **a PreToolUse hook that times out is treated as *allow*** — we must always return an
  explicit decision first.
- **Reads are gated too** (`read_guard`). Reading a file another instance is actively editing
  yields a stale snapshot, and Claude Code then rejects the follow-up edit with *"file
  modified since read"* — pre-empting our lock and causing churn. So a read of a held file
  **waits** for the holder to finish (or go idle), then returns the *final* content, so the
  edit applies cleanly in one shot. (Cost: every `Read` now forks the hook; reads of unlocked
  files return instantly.) The wait **early-exits** on the same conditions: holder idle
  (`LOCK_IDLE`), holder blocked on the user (`needs`), or the budget elapsed.
- **Bash**: best-effort — denies immediately if the command text names a currently-locked
  path (no wait); builds / `npm install` pass through.
- `App::refresh_locks` mirrors `locks/` to the UI and **reaps** locks held by dead instances.

### Phase 2 — inner MCP coordination hub (`mcp.rs`)

A hand-rolled **stdio JSON-RPC MCP server** (`mulpex mcp`), registered on every instance via
`--mcp-config <state_dir/mcp.json>`. One static config serves all instances because identity
arrives through the inherited `MULPEX_*` env (the server is a child of `claude`). Tools are
namespaced `mcp__mulpex__*` and callable with no prompts under
`--dangerously-skip-permissions`. Minimum protocol: `initialize` / `tools/list` /
`tools/call` (+ `ping`), newline-delimited; notifications (no `id`) are ignored; every
handler **fails soft**. Tools:

- `hub_instances` — every instance's id / status / task / held files (+ my unread count).
- `hub_set_focus` — publish *my* current task (refines the auto-captured prompt).
- `hub_file_owner` — who holds a path, and what they're working on.
- `hub_send` / `hub_inbox` — message another instance (or `all`), and read my mailbox.

**Awareness plumbing:** the `UserPromptSubmit` hook (`mulpex hook userpromptsubmit`)
auto-captures the prompt as the instance's baseline task (`state_dir/tasks/<id>`) and injects
a compact live snapshot of the *other* instances into each turn via `additionalContext`.
(The prompt text is read from the payload's `prompt` field — *not* `userPrompt`, which was a
bug that silently disabled task capture until fixed.)

**Message delivery (don't let mail rot).** A peer's `hub_send` is only useful if the recipient
reads it — but the awareness snapshot rides on `UserPromptSubmit`, which never re-fires for a
single-prompt/autonomous instance, so mail arriving after its last prompt used to go unseen
(the locks still prevented clobbering, but the *intent* of the message was lost). Two hooks
close the gap, both keyed on the unread count (`mcp::unread_for`, = files under `inbox/<id>/`):
- **`mulpex hook posttooluse`** (now routed through the binary, replacing the old `printf
  working`) keeps the `working` status word *and* injects a one-line "you have N unread"
  nudge the moment new mail arrives **mid-turn**. Deduped via a high-water mark in
  `inbox/<id>.notified`, so a message nudges **once**, not on every subsequent tool call.
- **`mulpex hook stop`** will not let a turn end with unread mail: if `unread > 0` it returns
  `{"decision":"block","reason":…}`, so the model continues and reads its inbox.
  `stop_hook_active` is honoured — it never blocks twice in a row, so a model that ignores the
  nudge still finishes (no wedge). On a normal stop it writes `waiting` as before.
  **Locks release at every turn boundary, including a blocked stop** (`release_my_locks` runs
  *before* the block decision): the continuation re-acquires (via `edit_guard`) anything it
  actually edits, so holding locks across the block would only add contention — a peer could
  time out waiting on a lock this instance is no longer using.
Standing **hub rules** are injected with `--append-system-prompt` — two consts in
`term_session.rs`, joined and passed as one prompt:
- `HUB_RULES` — teaches each instance it's one of several parallel Claudes, that locks are
  normal (never bypass or ask the user; the edit waits and proceeds), how to use the
  `mcp__mulpex__*` tools, and a **stale-read rule**: re-read a hot shared file (main.rs /
  lib.rs / mod.rs / any file others also touch) right before editing if much happened since
  the last read (a dispatched subagent, a long build, many steps). The per-turn lock
  serializes *writes* but can't refresh a read taken minutes earlier, so a read held across a
  long turn edits stale and Claude Code rejects it with "File has been modified since read";
  re-reading first avoids that round-trip.
- `PLANNING_RULES` — a user-mandated **zero-assumptions** discipline: before finalizing a plan
  or implementing, surface ALL assumptions and verify them with the user via `AskUserQuestion`
  first. It also tells the model that the `AskUserQuestion` tool here is **modified** to allow
  up to **10 questions per call and 10 options per question** (stock Claude Code hardcodes both
  to 4), so it shouldn't artificially trim to 4 — see *Raised AskUserQuestion caps* under
  *Embedded `claude` invocation*.

None of this touches the user's project files.

### Shared on-disk state (under `state_dir`)

```
<id>                 status word (working|waiting|needs)
instances            live instance ids, one per line (App writes; mcp/hook read for peers)
locks/<hash>         lock token: instance=/path=/ts=   (O_EXCL; ts heartbeated → idle-lease)
history/<hash>       last editor of a file (awareness notes)
tasks/<id>           one line: instance's current task
inbox/<id>/<uuid>    a message JSON for instance <id> (deleted when read via hub_inbox)
inbox/<id>.notified  high-water mark of the unread count we last nudged about (dedup)
messages.log         append-only TSV "ts\tfrom\tto\tbody" — the PERSISTENT conversation
                     feed (survives reads; powers the Messages pane + Ctrl+M reader)
waiting/<id>         "<basename>\t<holder>" while blocked waiting on a lock (⏳ indicator)
mcp.json             the --mcp-config registering `mulpex mcp`
settings.json        the --settings hooks
```

`refresh_locks` / `refresh_hub` (the same ~200ms poll as the status dots) mirror these into
`App` for the hub view and reap entries belonging to dead instances. `App::Drop`'s
`remove_dir_all` cleans the whole scratch dir on quit.

### Verified

End-to-end via the tmux self-test harness (`scripts/selftest_collision.sh`): two real
instances forced into a genuine same-file collision; the blocked instance's read waits
~60–80s, returns the final content, and edits cleanly **in one shot — zero staleness errors,
zero questions to the user, zero shell-bypass attempts** (vs. the messy interleave without
read-gating). MCP tools, task capture, live `⏳ Waiting` panel, and per-instance task lines
all confirmed on-screen.

**Real-world run (2026-06-17, first use on a Bevy game, 4 parallel instances):** a level editor,
a shotgun+weapon-wheel, an alien enemy, and enemy sounds — all editing overlapping files
(combat.rs ×3, main.rs ×3, enemy.rs ×3) concurrently. Result: **zero staleness errors, zero
denies, zero coordination questions to the user, zero shell-bypass attempts**, and the merged
output compiled clean (`cargo check --all-features`). The transparent read-wait engaged for real
(one instance's `Read combat.rs`/`Read main.rs` blocked ~145s, then edited cleanly). The one gap
found — a single-prompt instance never read 2 messages sent to it after its only prompt — is what
the *Message delivery* hooks (Stop-block + PostToolUse nudge) now fix. Those two hooks are
verified by driving `mulpex hook stop`/`posttooluse` directly (block on unread, no double-block
under `stop_hook_active`, nudge-once dedup) and `messages.log` persistence via the real `mulpex
mcp` server; `read_messages` parsing has unit tests (`cargo test`).

### Mouse: scrollback + selection

Two facts drive the design: (1) without mouse capture the outer terminal turns the wheel into
**arrow keys** in the alternate screen (so it moved the prompt cursor, not the conversation);
(2) Claude Code renders its conversation **inline** and relies on the *terminal's* scrollback —
it does **not** scroll on the wheel itself. So forwarding the wheel to Claude does nothing
useful. Instead Mulpex behaves like tmux: it keeps its own scrollback and the wheel scrolls
*our* view of it.

- The vt100 parser is created with a real scrollback (`SCROLLBACK_LEN = 10_000`, a lazily
  growing `VecDeque` — not preallocated). Previously it was `0`, so there was nothing to
  scroll back to.
- `main.rs` enables `EnableMouseCapture` (required just to *receive* the wheel — crossterm has
  no wheel-only mode). `App::on_mouse` handles `ScrollUp`/`ScrollDown` over the center pane by
  moving the focused `TermSession`'s scrollback offset (`scroll_up`/`scroll_down`, ±3 lines;
  vt100 clamps). Events over a sidebar, and non-wheel events, are ignored.
- Any input (`send_to_active`) calls `scroll_to_bottom()` first, so typing snaps back to live
  output like a normal terminal. The center-pane title shows `↑ scrollback −N · type to return`
  with a yellow border while scrolled up, so it's obvious you're not at live.
- Mouse events only redraw when the offset actually changed (no storm on `1003` moves);
  scrolling is a Mulpex-side view change, so it sets the redraw itself (not via the PTY
  `dirty` flag).
- **Text selection / copy (tmux copy-mode style).** Capture suppresses the outer terminal's
  drag-to-select, so Mulpex does selection itself rather than offloading it (which would force
  an Option-drag bypass). The protocol forces this: there is no "wheel-only" mouse mode — the
  wheel is reported through the same button modes as clicks/drags, so enabling the wheel
  necessarily enables click/drag reporting, which suppresses terminal-native selection.
  - `App::on_mouse` tracks a left **Down → Drag → Up** as a `Selection` of visible-screen
    cells `(row, col)`; `ui::highlight_selection` overlays a blue background on those cells
    *after* the `PseudoTerminal` renders (via `Frame::buffer_mut`).
  - **Double-click** (two Downs on the same cell within `DOUBLE_CLICK` = 400ms) selects a
    word: `TermSession::word_at` expands the cell over word chars (`is_word_char` =
    alphanumeric + `_-./~+@:`, so paths/URLs grab whole). It's stored as the `word_pivot`;
    dragging then unions the pivot word with the word under the cursor, so the selection
    grows whole-word-at-a-time.
  - On release, `TermSession::selection_text` reads the range with vt100's scrollback-aware
    `contents_between` (start inclusive, end made exclusive) and `copy_to_clipboard` pipes it
    to `pbcopy` (macOS; best-effort), returning the char count. A bare click (no drag, no
    word) selects nothing.
  - **Why no ⌘C / the "✓ copied" flash:** ⌘ combos are owned by the terminal/macOS menu and
    never reach Mulpex, so ⌘C can't be bound — and iTerm2's own ⌘C only copies *its* mouse
    selection, which our mouse reporting suppresses (hence iTerm2's "disable mouse reporting?"
    nag if you press ⌘C). So copy happens automatically on release; the center title flashes
    `✓ copied N chars` (green, `COPIED_FLASH` = 2s) so you trust it and don't reach for ⌘C.
    (⌘V still works normally — iTerm2 sends a bracketed paste, which we forward to Claude.)
  - The selection is in *view* coordinates, so it's cleared on scroll and on any key input;
    `ui::center_inner_rect` is the shared source of truth for pane geometry / coordinate
    mapping. Verified end-to-end (simulated drag, double-click, and word-drag → exact
    clipboard matches + blue highlight + flash).

## Stack

Rust + ratatui. Verified working dependency chain (see `Cargo.toml`):

- `ratatui` 0.30 + `crossterm` 0.29 — layout, rendering, raw mode, key/paste events.
- `tui-term` 0.3 — `PseudoTerminal` widget that renders a `vt100` screen into a pane.
  (NOTE: tui-term 0.3 targets the ratatui 0.30 `ratatui-core`/`ratatui-widgets` split,
  so it is **incompatible with ratatui 0.29** — that mismatch produces a confusing
  "`PseudoTerminal` doesn't implement `Widget`" error.)
- `portable-pty` 0.9 — spawn `claude` on a PTY; clone reader / take writer; `resize()`.
- `vt100` 0.15 — parse Claude Code's ANSI/VT output into a screen buffer.
- `libc` — process-group kill on teardown (see below).
- `serde_json` — parse hook tool-call JSON and implement the `mulpex mcp` JSON-RPC server.

## Why the center pane needs a terminal emulator

Claude Code is itself a full-screen TUI. Because it is flanked by sidebars it is not
full-width, so we cannot pass its output straight through. We run `claude` on a PTY sized
to the center rectangle, parse its output into a `vt100` screen buffer on a background
thread, and composite that buffer into the pane — the same job tmux/iTerm2 do internally.

## Architecture (`src/`)

- `main.rs` — entry point. **Dispatches the hidden subcommands first**: `mulpex hook <event>`
  → `hook::run` and `mulpex mcp` → `mcp::run`, both *before* `ratatui::init()` (they're stdio
  helpers, not the TUI). Otherwise sets up the terminal via `ratatui::init()` (+ panic hook +
  `ratatui::restore()`), bracketed paste, mouse capture, and the Kitty protocol.
- `app.rs` — `App` state + event loop. Holds `instances: Vec<TermSession>` and an `active`
  index. Polls events (~15ms), redraws when input is handled or the PTY reader signals new
  output via a shared `dirty` flag, and reaps exited sessions each iteration. Owns the status
  pipeline (the `Status` enum, `HOOK_SETTINGS_JSON` + the `MCP_CONFIG_JSON` it writes, the
  scratch dir, the ~200ms `refresh_statuses` poll, `impl Drop`), the session-persistence
  pipeline (`worked`, `persist_sessions`, restore in `App::new`), and the **hub mirror**:
  `locks`/`tasks`/`pending_messages`/`waiting`/`messages` maps, `refresh_locks`/`refresh_hub`/
  `refresh_messages` (`read_messages` tails `messages.log`), `write_live_instances` (the peer
  list), dead-instance reaping, and the `show_messages`/`msg_scroll` state for the Ctrl+M reader
  (see *The coordination hub*).
- `hook.rs` — the `mulpex hook` subcommand: the file-locking enforcement (`pretooluse` →
  `edit_guard`/`read_guard`/`bash_guard` with the `acquire_or_wait`/`wait_until_free` loops,
  the `lock_token`/`lock_is_stale` heartbeat helpers driving the `LOCK_IDLE` idle-lease, and
  `allow_contended` — the never-deny fallback that proceeds with a stale-read note),
  `posttooluse` (status `working` + mid-turn unread-mail nudge, deduped via `<id>.notified`),
  `stop` (block the stop while mail is unread, else release locks + write `waiting`), and
  `userpromptsubmit` (capture task from the payload's `prompt` field + inject peer snapshot).
  `Ctx::from_env` + the `read_field`/`canonical_target`/`now` helpers are shared with `mcp.rs`.
- `mcp.rs` — the `mulpex mcp` subcommand: the stdio JSON-RPC coordination-hub server and its
  five `hub_*` tools, plus `peers_context`/`unread_for` (used by the hooks). `hub_send` also
  appends to the persistent `messages.log`. Reads the shared `state_dir` files; no new crates
  beyond `serde_json`.
- `persist.rs` — session persistence: `new_uuid` (RFC-4122 v4 from `/dev/urandom`, reused for
  message ids) and `SessionStore` (per-project `~/.mulpex/sessions/<key>.txt`). `fnv1a`
  (pub(crate)) keys both the session store filename and the lock/history hashes.
- `term_session.rs` — `TermSession`: spawns `claude` on a PTY with `--settings`,
  `--mcp-config <state_dir/mcp.json>`, `--append-system-prompt <HUB_RULES + PLANNING_RULES>`,
  the `MULPEX_*` env (incl. `MULPEX_PROJECT_DIR`), and `--session-id`/`--resume`. The binary
  it execs is resolved by `claude_command`/`patched_claude_bin` — a byte-patched `claude` with
  raised AskUserQuestion caps when available, else stock `claude` (see *Embedded `claude`
  invocation*). A reader thread feeds the `vt100::Parser` (with `SCROLLBACK_LEN`);
  `resize`/`scroll_*` and `Drop` as before. The `HUB_RULES` and `PLANNING_RULES` consts (and
  the `MAX_Q`/`MAX_A` caps) live here.
- `keymap.rs` — `key_to_bytes`: translate crossterm `KeyEvent`s into terminal byte sequences,
  so the embedded session feels native. (Mouse is Mulpex-side, not forwarded.)
- `ui.rs` — `outer_layout` splits the window into `[top bar (2) | middle | bottom bar (1)]`;
  `layout` splits the middle into `[Length(LEFT_WIDTH=32) | Min(20)]` and then splits the left
  column vertically into `[instances 45% | info 55%]`, returning `[instances, center, info]`
  (center stays at index 1 so `center_inner_rect`/mouse mapping need no change). There is no
  longer a right sidebar / `RIGHT_WIDTH`. Focus borders + compositing the `PseudoTerminal`.
  `center_inner_size`/`center_inner_rect` (relative to the middle band) drive PTY size + mouse
  mapping.
- `pane.rs` — renderers: `render_top_bar` (project), `render_bottom_bar` (just the key legend
  now), `render_instances` (status dot + each instance's task line, word-wrapped across up to
  3 lines via `wrap_words` — unit-tested), `render_info` (the hub view: Locks / Waiting ⏳ /
  Messages feed with unread count + snippets), and `render_message_log` (the full-screen
  Ctrl+M reader: full bodies, word-wrapped, newest first).

## Keyboard model (decided)

- **Direct combos, no leader key.** Mulpex reserves a *minimal* set of combos; everything
  else forwards to Claude. Reserved: **Ctrl+Q** (quit), **Ctrl+T** (new), **Ctrl+]** (next),
  **Ctrl+[** (prev, Kitty-only), **Ctrl+M** (message reader, Kitty-only).
- Raw mode means Mulpex gets every Ctrl/Alt/Fn/arrow/letter key first; macOS ⌘ combos stay
  owned by the terminal emulator (iTerm2) and cannot be intercepted by any app.
- Future: optionally enable the Kitty keyboard protocol on the outer terminal for richer
  combo disambiguation, and add pane-switch combos (`Focus::Left/Right` already stubbed).

## Embedded `claude` invocation (important)

The user's `claude` is a **zsh function** running `command claude
--dangerously-skip-permissions` with `IS_SANDBOX=1`. `portable-pty` execs the binary
directly (the real one at `~/.local/bin/claude`, a compiled native binary), which
**bypasses the function**. To match the user's `claude`, `TermSession::spawn` replicates
it: argv `claude --dangerously-skip-permissions`, env `IS_SANDBOX=1`, cwd = launch dir.
(Make these overridable via config in a later milestone.) It also appends
`--session-id <uuid>` (new) or `--resume <uuid>` (restore) for session persistence,
`--settings <file>` for the status + locking hooks, `--mcp-config <file>` to register the
`mulpex mcp` coordination hub, and `--append-system-prompt <HUB_RULES + PLANNING_RULES>` to
teach the hub rules and the zero-assumptions planning discipline (see *The coordination hub*).

### Raised AskUserQuestion caps (the patched `claude` binary)

The cap on `AskUserQuestion` — max 4 questions per call, max 4 options per question — is a
hardcoded Zod schema bound **baked into Claude Code's compiled JS bundle**. There is no flag,
env var, `--settings` key, or MCP mechanism to change it; byte-patching the binary is the only
lever. The user's shell already does this: `MAX_Q`/`MAX_A` env vars make their `claude` zsh
function build (via `~/.local/bin/patch-claude-maxq.py`) and run a *patched* copy with the
bounds raised, cached under `~/.cache/claude-patched/claude-q<q>a<a>` and rebuilt when Claude
Code auto-updates.

`portable-pty` execs the binary directly and bypasses that zsh function, so `term_session.rs`
replicates it: `patched_claude_bin` resolves `~/.cache/claude-patched/claude-q10a10`, building
it on demand with the same patch script (rebuilding when the stock `~/.local/bin/claude` is
newer), and `claude_command` spawns it. The caps are fixed at `MAX_Q = 10` / `MAX_A = 10`. It
**fails soft**: if the patch script is missing (e.g. another machine), `python3` fails, or no
usable binary results, it falls back to plain `claude` with the stock caps — Mulpex still
works. The matching `PLANNING_RULES` system-prompt note tells the model the caps are raised so
it actually uses them. (This is tied to the user's machine setup; it is *not* portable to
other users without that patch script — a candidate for a Rust-native port if Mulpex is ever
shipped widely.)

## Teardown / no orphans (important)

`claude` `setsid`s into its own session and spawns helper subprocesses. Killing only the
direct pid leaves orphans. `TermSession::Drop` therefore kills the whole **process group**
(`libc::killpg(pid, SIGHUP)` then `SIGKILL`, since the child is the group leader), then
`wait`s. On quit, `App` (and its `Vec<TermSession>`) drops, so **every** instance's group is
torn down. Verified: after Ctrl+Q ×2 with multiple instances, no orphaned `claude` remains.

## Build / run

```sh
cargo build                       # or: cargo build --release
cargo run                         # runs in the current directory's project
cargo install --path . --locked   # installs `mulpex` on PATH
```

**Always install with `--locked`.** `cargo install` ignores `Cargo.lock` by default and
re-resolves dependencies, which pulls a newer `ratatui-core`/`ratatui-widgets` than
`tui-term` 0.3 targets — producing the confusing `PseudoTerminal doesn't implement Widget`
(E0277) error even though `cargo build` (which respects the lockfile) compiles fine.
`--locked` makes the install use the pinned, known-good versions.

## How to verify (no real terminal needed)

`script -q /dev/null` gives a **0×0** PTY, so ratatui draws nothing there — use **tmux**
for a real sized terminal:

```sh
tmux new-session -d -s mptest -x 140 -y 40
tmux send-keys -t mptest './target/debug/mulpex' Enter
sleep 6
tmux capture-pane -t mptest -p          # should show both columns + live Claude
tmux send-keys -t mptest C-q            # Ctrl+Q quits
tmux kill-session -t mptest
```

To check for orphaned children precisely (avoid broad `pkill -f claude`, which would hit
unrelated Claude processes): `MPID=$(pgrep -x mulpex); pgrep -P "$MPID"` to find Mulpex's
own `claude` child, then confirm that exact PID is gone after quit. Note `pgrep -x mulpex`
also matches the `mulpex mcp` server children — filter them out by args when finding the TUI.

**Coordination self-test:** `scripts/selftest_collision.sh` reinstalls, resets a scratch
project, launches two real instances in tmux, drives a lock-gated same-file collision, and
inspects both session transcripts (`~/.claude/projects/<proj>/*.jsonl`) for staleness
errors / user-facing asks / shell-bypass attempts and the read-gate wait. This is how the hub
is verified end-to-end without manual clicking.

## Last Synced Commit

`639139f1ad6263309fc48c7394242f6bf40d32e2` — 2026-06-23
