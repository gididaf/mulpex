# Mulpex

**Run multiple live Claude Code sessions side by side in one terminal — all working in the
same project directory, coordinating with each other so they don't clobber your files.**

Mulpex is a terminal UI that hosts several fully-interactive `claude` sessions at once. They
share one working directory (deliberately *not* git worktrees), and a built-in coordination
hub keeps them in sync: per-file locking, live awareness of what the others are doing, and a
cross-instance message channel — so you can fan a project out across several parallel Claudes
without them stepping on each other.

```
 project · /path/to/project                                         ← top bar
────────────────────────────────────────────────────────────────────
┌──────────────┬────────────────────────────┬──────────────┐
│ instances    │   Claude Code              │ info / hub   │
│ sidebar      │   (behaves exactly         │ (locks,      │
│ (+ each one's│    like `claude`)          │  waiting,    │
│  task)       │                            │  edits, msgs)│
└──────────────┴────────────────────────────┴──────────────┘
   left sidebar          center pane            right sidebar
 Ctrl+T new · Ctrl+] next · … · Ctrl+Q×2 quit          [kitty]  ← bottom bar
```

## Why

Running several Claude Code sessions on the same codebase at once is powerful — but in a plain
terminal they can't see each other. Two sessions edit the same file, one reads a stale copy,
and you get *"file modified since read"* errors, lost edits, and constant coordination
questions back to you.

Mulpex solves that. Each instance is a real, native `claude` session, but they share a
coordination hub that:

- **Locks files per-edit** so two instances never write the same file at once. A blocked edit
  **waits transparently** for the holder to finish, then proceeds — no tokens burned, no
  questions asked.
- **Gates reads** of files another instance is actively editing, so you never edit a stale
  snapshot.
- **Gives each instance live awareness** of the others — their current task, which files they
  hold — injected into every turn.
- **Lets instances message each other** (and guarantees the mail is read) through an inner MCP
  server.

The result, in real multi-instance runs on overlapping files: **zero staleness errors, zero
coordination questions to you, and output that compiles clean.**

## Install

Requires [Rust](https://rustup.rs/) and the [Claude Code](https://claude.com/claude-code) CLI
(`claude`) on your `PATH`.

```sh
git clone https://github.com/gididaf/mulpex.git
cd mulpex
cargo install --path . --locked
```

> **Always install with `--locked`.** `cargo install` otherwise re-resolves dependencies and
> can pull a `ratatui` version incompatible with `tui-term` 0.3, producing a confusing
> `PseudoTerminal doesn't implement Widget` build error. `--locked` pins the known-good
> versions.

## Use

```sh
cd /path/to/your/project
mulpex
```

Mulpex opens with one Claude session in your project directory. Add more with **Ctrl+T**,
switch between them, and give each its own task — they'll coordinate automatically.

When you quit and reopen Mulpex in the same project, the sessions you actually worked in
**auto-resume live**, with their prior conversations intact (powered by Claude Code's own
`--session-id` / `--resume`, not by copying any conversation content).

### The three panes

- **Left — instances.** Every running session (`claude #N`) with a **status dot**
  (🟢 ready · 🟡 working · 🔴 needs you) and its **current task** beneath it.
- **Center — Claude.** The focused session's live Claude, behaving exactly like `claude` in a
  normal terminal.
- **Right — the hub.** Live coordination state: **Locks** (file → holder), **Waiting** (who's
  ⏳ blocked on whose file), and **Messages** (the cross-instance conversation, newest first,
  with an unread count).

### Keybindings

| Key | Action |
| --- | --- |
| **Ctrl+T** | New Claude instance (in the project dir) |
| **Ctrl+]** | Focus next instance (wraps) |
| **Ctrl+[** | Focus previous instance (wraps) — *Kitty keyboard protocol only* |
| **Ctrl+M** | Open/close the full-screen cross-instance message reader — *Kitty only* |
| **Ctrl+Q ×2** | Quit (press twice within 3s) |
| **mouse wheel** | Scroll Mulpex's scrollback of the focused Claude |
| **click-drag** | Select text (double-click = word); copies to clipboard on release |
| *everything else* | Forwarded to the focused Claude (incl. Ctrl+C, Esc) |

> **Why these keys?** macOS reserves Ctrl+arrows (Mission Control) and mangles Option+letter
> (dead keys), so navigation lives on Ctrl+letters, which terminals leave alone. A couple of
> bindings (`Ctrl+[`, `Ctrl+M`) need the **Kitty keyboard protocol** to be distinguishable
> from Esc / Enter; Mulpex enables it automatically when your terminal supports it (e.g. recent
> iTerm2) and the info pane shows whether it's active. `Ctrl+]` (next) always works regardless.

## How it works

Each instance is a real `claude` binary running on its own PTY, sized to the center pane and
rendered through a `vt100` terminal emulator (the same job tmux does internally) so it fits
between the sidebars.

Coordination is built on lightweight, file-based IPC in a per-run scratch directory, wired into
Claude Code through its official extension points — **no patching of your project**:

- **File locking** via a `PreToolUse` **hook**: edit tools acquire an atomic per-file lock
  before writing; blocked edits/reads wait for the holder's turn to end, then proceed.
- **An inner MCP server** (`--mcp-config`) exposing `hub_*` tools so instances can query each
  other's status, publish their task, look up who owns a file, and send/read messages.
- **Status & awareness** via lifecycle hooks (`UserPromptSubmit` / `PostToolUse` / `Stop` /
  notifications) that drive the status dots and inject a live snapshot of the other instances
  into each turn.

None of this touches your project's `.claude/settings.json` — Mulpex passes everything through
`--settings`, `--mcp-config`, and `--append-system-prompt` on its own.

## Built with

Rust + [ratatui](https://ratatui.rs/) · [tui-term](https://crates.io/crates/tui-term) +
[vt100](https://crates.io/crates/vt100) for the embedded terminal ·
[portable-pty](https://crates.io/crates/portable-pty) to host `claude` ·
[crossterm](https://crates.io/crates/crossterm) for input/rendering.

## License

[MIT](LICENSE)
