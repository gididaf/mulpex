# Mulpex

**A native macOS desktop app that runs multiple live, parallel Claude Code sessions side by
side. Open several projects at once and switch between them instantly; within each project the
sessions share one directory and coordinate through a hub so they never clobber each other's
files.**

[![Download for macOS](https://img.shields.io/badge/Download-Mulpex.dmg-0a84ff?style=for-the-badge&logo=apple&logoColor=white)](https://github.com/gididaf/mulpex/releases/latest)
&nbsp;
[![Latest release](https://img.shields.io/github/v/release/gididaf/mulpex?style=for-the-badge&label=version)](https://github.com/gididaf/mulpex/releases/latest)

macOS 11+ · Apple Silicon · requires the `claude` CLI · unsigned, so one `xattr` command after download. See [**Install**](#install).

This is the desktop successor to the original terminal-UI mulpex (the ratatui/crossterm terminal
version, preserved in this repo's git history). It sheds every terminal-only workaround — no
iTerm2, no Ctrl-key minefield, no Kitty-protocol hacks — and uses a real window with native ⌘
menu shortcuts.

```
[ Cloud ▾ ][ API ][ Docs ]  +                               ← project tabs (⌘P switcher)
 project · /path/to/project                                  ← top bar
┌──────────────┬───────────────────────────────────────────┐
│ instances    │                                            │
│ (status +    │         Claude Code                        │
│  task/name)  │         (real xterm.js terminal)           │
├──────────────┤                                            │
│ hub          │                                            │
│ (locks,      │                                            │
│  waiting,    │                                            │
│  messages)   │                                            │
└──────────────┴───────────────────────────────────────────┘
 ⌘T new · ⌘W close · ⌘R rename · ⌘[ ⌘] switch · ⌘M messages  ← status bar
```

## What it does

- **Multiple projects at once.** Open several projects side by side — a persistent tab bar, a
  **⌘P** fuzzy quick-switcher, or **drag a folder** onto the window. Each project keeps its own
  isolated coordination hub and its sessions running in the background; Mulpex reopens everything
  you had open on the next launch. Close a project with ⌘⇧W, cycle with ⌘⇧[ ⌘⇧].
- **Multiple Claude sessions per project.** Each runs as a real `claude` process on its own
  PTY, rendered by xterm.js. Add (⌘T), switch (⌘1–9 / ⌘[ ⌘]), rename (⌘R), close (⌘W).
  Shift+Enter (or Option+Enter) inserts a newline instead of submitting — no `/terminal-setup`
  needed.
- **Coordination hub.** Parallel Claudes in the same directory are kept consistent by a
  file-locking coordinator (a `PreToolUse` hook: edits/reads to a file another instance is
  writing simply *wait*, then proceed — never a hard deny, zero model tokens) plus an inner MCP
  server (`mcp__mulpex__*` tools: see peers, publish your task, message another instance).
- **Idle wake.** An instance notices and acts on a hub message from a peer *even while it's
  sitting idle* between your prompts: on startup it arms a persistent background listener on its
  inbox (via the `Monitor` tool) and wakes itself when mail arrives, tagging the self-triggered
  turn (`⟳ hub message from #N →`) so you can tell it wasn't your prompt.
- **Shared-tree safety.** Every instance shares one working tree (not a git worktree each), so
  each is told via its system prompt to treat tree-wide/destructive git ops (`reset --hard`,
  `checkout .`, `clean`, `stash`, branch switch, `rebase`) as dangerous — check the hub and
  coordinate or ask before running them. Instances are also held to a zero-assumptions planning
  discipline (verify assumptions via `AskUserQuestion` before implementing).
- **Session persistence.** The sessions you worked on are remembered per project and
  auto-resume (`claude --resume`) when you reopen that project.
- **Open a project** via the picker / recent-projects list, the `+` tab, ⌘P, or drag-and-drop —
  no terminal needed.

## Architecture

A Cargo workspace + a Svelte/Vite frontend:

- **`crates/mulpex-core`** — the headless, UI-independent coordination core, ported verbatim
  from the old TUI: the file-locking hook (`hook`), the MCP hub server (`mcp`), session
  persistence (`persist`), and the `--settings` / `--mcp-config` templates (`config`).
- **`crates/mulpex-helper`** — a tiny binary (`hook <event>` / `mcp`) that each child `claude`
  invokes by absolute path. Kept separate from the GUI so it stays small and fast to exec (a
  hook forks on every tool call).
- **`src-tauri`** — the Tauri app: PTY hosting (`pty.rs`), the `Workspace` of open projects with
  per-project reap/persist/hub-read (`state.rs`), commands (`commands.rs`), the 200 ms poll loop
  that emits handle-scoped hub updates over every project (`hub.rs`), the native menu (`menu.rs`),
  and project selection + open-set persistence (`project.rs`).
- **`src/`** — the Svelte frontend. xterm.js **is** the terminal emulator (one `Terminal` per
  session, kept alive while hidden — across all open projects); the backend is a raw byte pipe.
  The tab bar / ⌘P palette switch projects; sidebar + hub panel render from the active project's
  `hub-update` snapshot.

See `CLAUDE.md` for the detailed design.

## Install

**Requirements:** macOS 11+, and the [Claude Code CLI](https://code.claude.com) installed and
logged in (`claude` must be on your `PATH`) — Mulpex launches your own `claude`.

1. Download `Mulpex_<version>_aarch64.dmg` from the [latest release](https://github.com/gididaf/mulpex/releases/latest).
2. Open the DMG and drag **Mulpex** into **Applications**.
3. Clear the download quarantine flag (one time), then open Mulpex normally:
   ```sh
   xattr -dr com.apple.quarantine /Applications/Mulpex.app
   ```

> **"Mulpex is damaged and can't be opened. You should move it to the Trash."**
> The download is fine — this is Gatekeeper, not corruption. The build is **ad-hoc signed and not
> notarized**, and macOS reports any downloaded app it can't verify with that (misleading) wording.
> Run the `xattr` command above and it opens. Right-click → Open does *not* clear this on recent
> macOS versions; the `xattr` step is the reliable one. A notarized build would install with no
> warning — that needs a paid Apple Developer account and isn't set up.

## Develop

Requires Rust and Node. Mulpex launches whatever `claude` you have installed (resolved via
`PATH`) — no binary patching.

```sh
npm install
npm run tauri dev      # builds mulpex-helper, starts Vite, launches the app
```

## Build

```sh
npm run tauri build    # produces Mulpex.app under target/release/bundle/macos/
```

> **Helper signing (already wired).** For the packaged `.app`, `mulpex-helper` must ship
> **inside the signed bundle** (`Contents/MacOS/`) or the child `claude` hooks fail-open
> silently. This is handled: `scripts/bundle-helper.sh` stages it as a triple-suffixed
> `bundle.externalBin` sidecar (run from `beforeBuildCommand`), so Tauri places and signs it in
> the bundle. In `tauri dev` it's resolved beside the app binary in `target/debug/`, so dev
> needs no extra step. Bundle targets are `app` and `dmg` — `tauri build` produces both
> `Mulpex.app` and `Mulpex_<version>_aarch64.dmg` (see `tauri.conf.json`).
