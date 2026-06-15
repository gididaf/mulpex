# Mulpex

A CLI tool, opened from inside a project directory, that wraps live **Claude Code**
sessions in a 3-pane terminal shell:

```
┌──────────────┬────────────────────────────┬──────────────┐
│ instances    │   Claude Code              │ info         │
│ sidebar      │   (behaves exactly         │ sidebar      │
│ (running CC  │    like `claude`)          │ (general     │
│  for project)│                            │  info)       │
└──────────────┴────────────────────────────┴──────────────┘
   left sidebar          center pane            right sidebar
```

Run it from a project directory:

```sh
cd /path/to/project
mulpex
```

## Status (done)

3-pane layout with **multiple live, fully-interactive Claude Code sessions** for the
current project. You can add instances, switch between them, and exited instances are
removed automatically.

- **Left sidebar** lists all running instances (`claude #N`); the focused one is highlighted.
  Each carries a **status dot**: green `ready` (idle / waiting for you), yellow `working`
  (mid-turn), red `needs you` (a question, permission, or idle wait). See *Status indicators*.
- **Center pane** shows the focused instance's live Claude, or a clean "No active Claude"
  hint when none are running.
- **Right sidebar** shows project path, instance count, center-pane size, a status legend,
  and the key legend.

Real "general info" content for the right sidebar is a future milestone.

### Keybindings

- **Ctrl+T** — new Claude instance (in the project dir)
- **Ctrl+]** — focus next instance (wraps)
- **Ctrl+[** — focus previous instance (wraps) — *Kitty protocol only, see below*
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
`supports_keyboard_enhancement()` reports it (e.g. recent iTerm2). The **info pane shows
`Keyboard: enhanced (kitty)` or `legacy (Ctrl+[ off)`** so you can tell whether `Ctrl+[`
will work. When it reads legacy, only `Ctrl+[` is affected — `Ctrl+]` (next) still works,
so you can cycle forward through all instances.

### Instance lifecycle

- All instances run `claude` in the directory Mulpex was launched from.
- When a Claude exits (Ctrl+C/Ctrl+D out of it, or the process dies), `App::reap_dead`
  removes it from the list and moves focus to a surviving neighbour. When the last one
  exits, the center shows the empty-state hint. Sessions are reaped in the main loop, woken
  by the reader thread flipping the shared `dirty` flag on EOF.
- Each `TermSession` has a stable display `id` (`claude #N`); `App.next_id` only increments.

### Status indicators (WORKING / WAITING / NEEDS YOU)

Each sidebar instance shows what its Claude is doing, sourced from **Claude Code lifecycle
hooks** — *not* by scraping the screen (robust across CC versions).

- At spawn, `App` creates a per-run scratch dir `$TMPDIR/mulpex-<pid>/` and writes one
  static `settings.json` into it (`HOOK_SETTINGS_JSON` in `app.rs`).
- Each `TermSession` is launched with `--settings <that file>` plus env
  `MULPEX_INSTANCE_ID=<id>` and `MULPEX_STATE_DIR=<dir>`. The hooks are one-liners that
  `printf` a single word into `$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID`, so **one static
  settings file serves every instance** (the id lives in the env, not the file). Using
  `--settings` means we never touch the user's project `.claude/settings.json`.
- State machine: `UserPromptSubmit` / `PostToolUse` → `working`; `Stop` → `waiting`;
  `PreToolUse[AskUserQuestion]` and the `permission_prompt` / `idle_prompt` **Notification**
  matchers → `needs`. A fresh instance with no file yet reads as `Waiting` (ready).
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

## Why the center pane needs a terminal emulator

Claude Code is itself a full-screen TUI. Because it is flanked by sidebars it is not
full-width, so we cannot pass its output straight through. We run `claude` on a PTY sized
to the center rectangle, parse its output into a `vt100` screen buffer on a background
thread, and composite that buffer into the pane — the same job tmux/iTerm2 do internally.

## Architecture (`src/`)

- `main.rs` — entry point. Uses `ratatui::init()` (raw mode + alternate screen + a panic
  hook that restores the terminal) and `ratatui::restore()`. Enables bracketed paste and
  mouse capture (and disables both on exit).
- `app.rs` — `App` state + event loop. Holds `instances: Vec<TermSession>` and an `active`
  index. Polls events (~15ms), redraws when input is handled or the PTY reader signals new
  output via a shared `dirty` flag, and reaps exited sessions each iteration. Routes the
  reserved combos (Ctrl+Q/N/↑/↓); all other keys forward to the focused session. Also owns
  the status pipeline: the `Status` enum, the `HOOK_SETTINGS_JSON` it writes, the scratch
  dir, the ~200ms `refresh_statuses` poll, and an `impl Drop` that tears down sessions then
  removes the scratch dir (see *Status indicators*).
- `term_session.rs` — `TermSession`: spawns `claude` on a PTY (with `--settings` + the
  `MULPEX_INSTANCE_ID`/`MULPEX_STATE_DIR` env for status hooks), a reader thread feeds the
  `vt100::Parser` (created with a `SCROLLBACK_LEN` buffer), `resize()` updates both the parser
  and the PTY master, `scroll_up`/`scroll_down`/`scroll_to_bottom`/`scrollback` drive the
  wheel-scroll view, and `Drop` tears down the child (see teardown note). All instances share
  one `dirty` flag.
- `keymap.rs` — `key_to_bytes`: translate crossterm `KeyEvent`s into the byte sequences a
  terminal program expects (control bytes, ESC-prefixed alt, CSI arrows/keys with xterm
  modifier encoding, function keys). This makes the embedded session feel native. (Mouse is
  handled separately as Mulpex-side scrollback, not forwarded — see *Mouse: scrollback +
  selection*.)
- `ui.rs` — the 3-pane `Layout` (`Length(30) | Min(20) | Length(34)`), focus border
  styling, and compositing the `PseudoTerminal` into the center pane. `center_inner_size`
  is the single source of truth for the PTY size (pane minus its border); `center_inner_rect`
  gives the same area with its position, used for mouse-coordinate translation.
- `pane.rs` — sidebar renderers: the instances list (each row a status-coloured dot + word
  via `Status::indicator`) and the info panel (project, counts, status legend, key legend).

## Keyboard model (decided)

- **Direct combos, no leader key.** Mulpex reserves a *minimal* set of combos; everything
  else forwards to Claude. Currently the only reserved combo is **Ctrl+Q → quit**.
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
(Make these overridable via config in a later milestone.)

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
tmux capture-pane -t mptest -p          # should show all 3 panes + live Claude
tmux send-keys -t mptest C-q            # Ctrl+Q quits
tmux kill-session -t mptest
```

To check for orphaned children precisely (avoid broad `pkill -f claude`, which would hit
unrelated Claude processes): `MPID=$(pgrep -x mulpex); pgrep -P "$MPID"` to find Mulpex's
own `claude` child, then confirm that exact PID is gone after quit.

## Last Wrapped Commit

`7fb6a27765bb3adc38f3d163032815f665937c52` — 2026-06-15
