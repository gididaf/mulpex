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
- **Center pane** shows the focused instance's live Claude, or a clean "No active Claude"
  hint when none are running.
- **Right sidebar** shows project path, instance count, center-pane size, and the key legend.

Real "general info" content for the right sidebar is a future milestone.

### Keybindings

- **Ctrl+T** — new Claude instance (in the project dir)
- **Ctrl+]** — focus next instance (wraps)
- **Ctrl+[** — focus previous instance (wraps) — *Kitty protocol only, see below*
- **Ctrl+Q ×2** — quit Mulpex (press twice within 3s; the first press shows a red
  "press Ctrl+Q again to quit" banner on the center pane, which clears after the window lapses)
- **everything else (incl. Ctrl+C, Esc)** — forwarded to the focused Claude

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
  hook that restores the terminal) and `ratatui::restore()`. Enables bracketed paste.
- `app.rs` — `App` state + event loop. Holds `instances: Vec<TermSession>` and an `active`
  index. Polls events (~15ms), redraws when input is handled or the PTY reader signals new
  output via a shared `dirty` flag, and reaps exited sessions each iteration. Routes the
  reserved combos (Ctrl+Q/N/↑/↓); all other keys forward to the focused session.
- `term_session.rs` — `TermSession`: spawns `claude` on a PTY, a reader thread feeds the
  `vt100::Parser`, `resize()` updates both the parser and the PTY master, and `Drop`
  tears down the child (see teardown note). All instances share one `dirty` flag.
- `keymap.rs` — `key_to_bytes`: translate crossterm `KeyEvent`s into the byte sequences a
  terminal program expects (control bytes, ESC-prefixed alt, CSI arrows/keys with xterm
  modifier encoding, function keys). This is what makes the embedded session feel native.
- `ui.rs` — the 3-pane `Layout` (`Length(30) | Min(20) | Length(34)`), focus border
  styling, and compositing the `PseudoTerminal` into the center pane. `center_inner_size`
  is the single source of truth for the PTY size (pane minus its border).
- `pane.rs` — placeholder sidebar renderers (instances list, info panel).

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
cargo build              # or: cargo build --release
cargo run                # runs in the current directory's project
cargo install --path .   # installs `mulpex` on PATH
```

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
