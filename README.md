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
 ⌘T new · ⌘⇧T terminal · ⌘W close · ⌘R rename · ⌘[ ⌘] switch · ⌘M mute  ← status bar
```

## What it does

- **Multiple projects at once.** Open several projects side by side — a persistent tab bar and a
  **⌘P** fuzzy quick-switcher. Each project keeps its own coordination hub and its sessions
  running in the background; Mulpex reopens everything you had open on the next launch. Jump
  straight to a tab with ⌘1–9, close a project with ⌘⇧W, cycle with ⌘⇧[ ⌘⇧].
- **Multiple Claude sessions per project.** Each runs as a real `claude` process on its own
  PTY, rendered by xterm.js. Add (⌘T), switch (⌘[ ⌘]), rename (⌘R), close (⌘W), and drag rows in
  the sidebar to reorder them — that order is what ⌘[ ⌘] cycle through, and it survives a restart.
  **Rows label themselves**: a session names its own row after the work it's doing, so a sidebar
  of five Claudes reads as five tasks rather than five numbers. A name you type with ⌘R always
  wins. Shift+Enter (or Option+Enter) inserts a newline instead of submitting — no
  `/terminal-setup` needed.
- **Terminal sessions (⌘⇧T).** A session can also be a plain interactive shell, in the same
  sidebar and the same pane. Use it for the long-running and the interactive — a dev server, a
  watcher, `tail -f`, a REPL — anything Claude's request/response Bash tool cannot hold open. They
  are **shared**: your instances can open their own, and any of them can read or drive any
  terminal, including one you started, so a Claude can watch your dev server's output and tell
  when a command finished and with what exit code. A terminal that exits stays in the list with
  its output still readable until you close it.
- **Mute a session (⌘M)** when you don't want to hear from it. It keeps running and keeps
  coordinating — it just stops competing for your attention: dimmed, sunk to the bottom of the
  sidebar, and dropped from every badge, including its project tab's. Persists across restarts.
- **Right-to-left text renders correctly.** Hebrew and Arabic read in the right order, mixed with
  English on the same line — no setting to turn on. Most terminals (and anything drawing the grid
  on a GPU, VS Code's included) show RTL mirrored, because a cell grid puts character *n* in
  column *n*; Mulpex renders rows as real text so the browser's BiDi engine reorders them.
- **Coordination hub.** Parallel Claudes in the same directory are kept consistent by a
  file-locking coordinator (a `PreToolUse` hook: edits/reads to a file another instance is
  writing simply *wait*, then proceed — never a hard deny, zero model tokens) plus an inner MCP
  server (`mcp__mulpex__*` tools: see peers, publish your task, message another instance).
- **They can message across projects.** An instance in one project can see and message the
  instances in every *other* project you have open — address them `central-one#3`, discovered
  through `hub_instances`. That's how two repos get built against each other: the claude in your
  API repo tells the one in your frontend what the endpoint now returns, and it acts on it. Only
  messaging crosses the boundary; file locks, spawning and terminals stay inside their own
  project, and each instance is told the other side is a *different* checkout it cannot see, so
  what it sends has to stand on its own.
- **Idle wake.** An instance notices and acts on a hub message *even while it's sitting idle*
  between your prompts — from a peer in its own project or from one in another. On its first turn
  it arms a persistent background listener on its inbox (via the `Monitor` tool) and wakes itself
  when mail arrives, tagging the self-triggered turn (`⟳ hub message from cloud#1 →`) so you can
  tell it wasn't your prompt.
- **Shared-tree safety.** Every instance shares one working tree (not a git worktree each), so
  each is told via its system prompt to treat tree-wide/destructive git ops (`reset --hard`,
  `checkout .`, `clean`, `stash`, branch switch, `rebase`) as dangerous — check the hub and
  coordinate or ask before running them. Instances are also held to a zero-assumptions planning
  discipline (verify assumptions via `AskUserQuestion` before implementing).
- **Session persistence.** The sessions you worked on are remembered per project and
  auto-resume (`claude --resume`) when you reopen that project.
- **Drag a file in to reference it.** Drop a file or folder anywhere on the window and its
  absolute path lands at the prompt — escaped if it needs it, nothing submitted — so you can hand
  Claude a path without typing it out. Multiple files land space-separated. **Drop an image and
  Claude sees the image**, not just its path (it becomes an `[Image #N]` attachment), exactly as in
  Claude Code's own terminal.
- **Open a project** via the picker / recent-projects list, the `+` tab, or ⌘P — no terminal
  needed.

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
4. The first time you open a project, macOS asks for access to the folder it lives in (Documents,
   Desktop or Downloads). **Click Allow.** Mulpex runs each `claude` *inside* your project
   directory, so without it the session cannot start.

> **Already clicked "Don't Allow"?** macOS records that and never asks again, so sessions fail to
> start. Re-enable Mulpex under **System Settings ▸ Privacy & Security ▸ Files and Folders**, or
> reset the prompt with `tccutil reset SystemPolicyDocumentsFolder com.mulpex.app` and reopen the
> app. From v0.6.0 the answer sticks across updates; earlier builds were re-asked every time.

> **"Mulpex is damaged and can't be opened. You should move it to the Trash."**
> The download is fine — this is Gatekeeper, not corruption. The build is **ad-hoc signed and not
> notarized**, and macOS reports any downloaded app it can't verify with that (misleading) wording.
> Run the `xattr` command above and it opens. Right-click → Open does *not* clear this on recent
> macOS versions; the `xattr` step is the reliable one. A notarized build would install with no
> warning — that needs a paid Apple Developer account and isn't set up.

**You only do that once.** From then on Mulpex updates itself: it checks at launch and every six
hours (or on demand via **Mulpex ▸ Check for Updates…**), and a new version raises a card with an
**Update & Restart** button that downloads, verifies, installs and relaunches. No second `xattr`
— that flag is set by the *browser* that downloads a file, and the in-app updater doesn't go
through one. If sessions are mid-turn it says so and asks before restarting; sessions come back
via `--resume`.

## Develop

Requires Rust and Node. Mulpex launches whatever `claude` you have installed (resolved via
`PATH`) — no binary patching.

```sh
npm install
npm run tauri dev      # builds mulpex-helper, starts Vite, launches the app
```

## Build

```sh
npm run tauri:build    # Mulpex.app + .dmg + the signed updater artifacts
```

> **Use `tauri:build`, not `tauri build`.** With `createUpdaterArtifacts` on, the bundler needs the
> minisign key in `TAURI_SIGNING_PRIVATE_KEY` or it compiles the whole release and *then* fails at
> the signing step. The `tauri:build` script reads it from `~/.mulpex/updater.key`.

## Release

```sh
npm run release              # or: npm run release -- --dry-run
```

Bump `version` in `src-tauri/tauri.conf.json` **and** `src-tauri/Cargo.toml` (the script refuses a
mismatch), commit, then run it: builds and signs, writes `latest.json`, and publishes the DMG,
`Mulpex.app.tar.gz`, its `.sig` and `latest.json` to a GitHub release. All four must be on the same
release — the in-app updater reads `latest.json` from `/releases/latest/download/`.

> **Back up `~/.mulpex/updater.key`.** It signs every update, and installs only accept updates
> signed by the key matching the pubkey compiled into them. Lose it and the only path back is a new
> keypair plus a manual DMG reinstall by every user.

> **Helper signing (already wired).** For the packaged `.app`, `mulpex-helper` must ship
> **inside the signed bundle** (`Contents/MacOS/`) or the child `claude` hooks fail-open
> silently. This is handled: `scripts/bundle-helper.sh` stages it as a triple-suffixed
> `bundle.externalBin` sidecar (run from `beforeBuildCommand`), so Tauri places and signs it in
> the bundle. In `tauri dev` it's resolved beside the app binary in `target/debug/`, so dev
> needs no extra step. Bundle targets are `app` and `dmg` — `tauri build` produces both
> `Mulpex.app` and `Mulpex_<version>_aarch64.dmg` (see `tauri.conf.json`).
