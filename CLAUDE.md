# Mulpex (desktop app) — design notes

Native macOS Tauri app hosting multiple coordinated Claude Code sessions. Successor to the
terminal-UI mulpex (`../mulpex-deprecated`). This doc covers what's specific to the desktop
rewrite; the coordination-hub semantics are unchanged from the deprecated project's `CLAUDE.md`
(the hook/mcp/persist modules were copied verbatim).

## The key idea

**xterm.js is the terminal emulator.** The old TUI needed `vt100` + `tui-term` only to composite
Claude's ANSI output into a ratatui sub-rect; in the browser xterm.js does that. So the backend
is a **raw byte pipe** (PTY bytes → `term.write`; xterm `onData` → PTY writer). That deleted
`ratatui`/`crossterm`/`tui-term`/`vt100`, all key-encoding (`keymap.rs`), mouse-selection,
manual scrollback, and the Kitty/Ctrl keybinding workarounds. Everything that makes mulpex
*mulpex* — the hook coordinator, MCP hub, persistence, file IPC — is UI-independent and was
reused unchanged.

## Layout (Cargo workspace + Svelte frontend)

```
crates/mulpex-core/   headless lib: hook, mcp, persist, config  (copied verbatim from the TUI)
crates/mulpex-helper/ bin: `hook <event>` / `mcp` dispatch → mulpex-core
src-tauri/            the Tauri app (Rust backend)
  src/pty.rs          Session = one claude on a PTY, streaming to a frontend Channel
  src/state.rs        Core (open project + sessions + scratch dir); reap/persist/hub-read
  src/commands.rs     #[tauri::command] surface
  src/hub.rs          200ms poll loop → emits hub-update / session-exited / sessions-changed
  src/menu.rs         native ⌘ menu; ids forwarded to the frontend as a `menu` event
  src/project.rs      recent-projects list (~/.mulpex/recents.txt)
  src/snapshot.rs     serde types shared with the frontend
src/                  Svelte/Vite frontend
  lib/terminals.ts    TerminalManager: one xterm per session, alive-while-hidden, central resize
  lib/ipc.ts          typed command/event/channel wrappers
  lib/stores.ts       reactive sidebar/hub state (PTY bytes bypass this)
  lib/components/*     TopBar, InstanceList, HubPanel, TerminalPane/View, MessageReader, Rename…
```

## The helper (why it's a separate binary)

Child `claude` processes invoke `<helper> hook <event>` (from `settings.json`) and `<helper> mcp`
(from `mcp.json`) **by absolute path** — `claude`, not Tauri, spawns them. A `PreToolUse` hook
forks on every Read/Write/Edit/Bash and the MCP server is long-lived per instance, so the helper
must be tiny and fast to exec. It links only `mulpex-core` (~1.8 MB vs the ~29 MB app).

**Path resolution** (`lib.rs::resolve_helper_path`): `current_exe().parent().join("mulpex-helper")`
— works in `tauri dev` (`target/<profile>/`) and in the bundled `.app` (`Contents/MacOS/`). The
absolute path is substituted for `__MULPEX_BIN__` in the config templates when a project opens
(`state.rs::Core::open`).

### Bundling the helper (do before shipping a `.app`)

The dev flow needs nothing (helper sits beside the app in `target/`). For `tauri build`, add
`mulpex-helper` as a **signed sidecar** so it lands in `Contents/MacOS/` *and is signed with the
bundle* — otherwise Gatekeeper SIGKILLs it and **every hook fails-open silently** (no
coordination, no error). Set `bundle.externalBin` in `tauri.conf.json` and provide the
triple-suffixed binary (`mulpex-helper-aarch64-apple-darwin`) via a pre-build step, or bundle it
under `Contents/Resources/` and adjust `resolve_helper_path`. **Not yet wired** — `beforeBuildCommand`
currently just builds the helper into `target/release/`.

## Data flow

- **PTY → frontend:** per-session `tauri::ipc::Channel<String>` carrying **base64** chunks
  (`pty.rs::OutputSink`, decoded in `terminals.ts`). Before the frontend attaches (restored
  sessions paint immediately on `--resume`), output is **buffered** and flushed on
  `attach_session` — race-free under the sink mutex.
- **frontend → PTY:** `send_bytes(id, data)` from xterm `onData` (UTF-8 encoded). The one manual
  key carve-out is **Shift+Enter → `\n`** (`attachCustomKeyEventHandler`); `macOptionIsMeta`
  covers Option word-motions.
- **hub state:** the 200 ms poll reads the scratch-dir files into a `HubSnapshot` and emits it on
  change; the sidebar/hub panel are a reactive projection. Reaping (with `bounce_dead_inbox`,
  peer-list rewrite, persistence) is **single-sourced in the poll loop** — an explicit ⌘W close
  just `kill()`s and lets the next reap emit `session-exited`, identical to a self-exit.

## Keyboard

Native macOS menu accelerators (⌘T/⌘W/⌘R/⌘M/⌘1–9/⌘[ ⌘]/⌘O/⌘Q) are intercepted by the menu
before xterm; Claude never uses ⌘, so there's zero collision. Everything else (arrows, Ctrl+C,
Esc, Shift+Enter) flows straight to the focused terminal. Copy/paste/select-all are predefined
Edit-menu items macOS routes to the focused xterm textarea.

## Terminals kept alive while hidden

All sessions' xterms are stacked absolutely in `TerminalPane`; the focused one is
`visibility: visible`, the rest `visibility: hidden` (**never** `display:none`, which would
zero their size and break `fit()`). Hidden terminals keep receiving `term.write()`, so
background Claudes keep rendering. Geometry is central: a `ResizeObserver` on the pane fits the
visible terminal, then applies the same `cols/rows` to every session + the backend PTYs (all
PTYs share one size, as the TUI did). **WebGL** is attached only to the focused terminal (browser
caps live GL contexts) and disposed on blur.

## Lifecycle

- **Startup:** frontend calls `current_project()`; if none, shows the picker (recents +
  `@tauri-apps/plugin-dialog` folder picker) → `open_project(path)` builds `Core` (scratch dir,
  config files, restore/spawn sessions), returns `BootstrapInfo`; the frontend builds one xterm
  per session and `attach_session`es each.
- **Teardown:** handled in `lib.rs` via `RunEvent` (managed-state `Drop` isn't guaranteed on
  exit). Window close → `app.exit(0)` → `ExitRequested` → `Core::teardown()` kills every process
  group (`killpg` SIGHUP→SIGKILL→wait) **then** removes the scratch dir. This is the "no orphaned
  claude" guarantee.

## claude binary

Mulpex launches the **user's stock `claude`**, resolved via `PATH` (`pty.rs::claude_command`) —
no byte-patching, no re-signing. (The deprecated TUI shipped a `patch-claude-maxq.py` hack that
raised the `AskUserQuestion` caps to 10/10; that was intentionally dropped so instances behave
exactly like a plain `claude`. The matching "you may ask up to 10 questions/options" NOTE was
removed from `PLANNING_RULES` too — only the zero-assumptions planning discipline remains.)

## Hub listener — idle wake (A2)

By default a `claude` instance only reads its inbox when it takes a turn, so a message from a
peer sits unread while the instance is idle between the user's prompts. To make an idle instance
react on its own **without host-side stdin polling**, each instance runs the agentalk pattern
against the *local* hub:

- **Watcher:** the instance arms a **persistent `Monitor`** on a ~1 s poll of its own inbox dir
  (`$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID/`), emitting a `mulpex: N new hub message(s)`
  line only when new message files appear (seeded to the current count, so only post-arm
  arrivals fire). Each such line is a wake event the Claude Code runtime injects as a new turn —
  even while the instance is idle waiting for the user.
- **Bootstrap (`pty.rs`, "A2"):** only the agent can arm its *own* Monitor, and a `--resume`
  restart kills the previous one — so Mulpex injects a one-shot prompt (`HUB_LISTENER_BOOTSTRAP`)
  into the PTY on **every** spawn, once `claude`'s initial paint has settled (a reader-thread
  readiness gate: first output seen, then ≥600 ms quiet, hard cap 8 s; the submitting Enter is
  sent as a **separate** write ~400 ms later, or `claude` treats the burst as a paste and the
  trailing `\r` becomes a literal newline instead of submitting). To keep the visible startup
  turn compact, the *trigger* is one short line; the full arming procedure + exact Monitor
  command live in `HUB_RULES` (append-system-prompt, which also carries the wake→act contract so
  it survives compaction). The trigger begins with `mulpex_core::MULPEX_SENTINEL` (`[mulpex:hub]`)
  so the `UserPromptSubmit` hook skips it for the sidebar task (else the plumbing text would show
  up as the instance's "task").
- **On wake (auto-act):** the instance calls `mcp__mulpex__hub_inbox`, acts on the message(s)
  autonomously, replies to the sender only when it adds value (no bare acks), and prefixes the
  self-triggered turn with a `⟳ hub message from #<sender> →` marker so the human can tell it
  wasn't their prompt. This coexists with the `userpromptsubmit` hook's unread-count nudge, which
  still covers the "notice on your next prompt" path.

## Shared-tree guardrail

All instances share one working directory and one git checkout (not a worktree per instance), so
a tree-wide git op by one (`reset --hard`, `checkout .`/`restore .`, `clean`, `stash`, branch
switch, `rebase`/`revert`) or any bulk-destructive command wipes every other instance's
uncommitted work. `HUB_RULES` (the append-system-prompt) tells each instance to treat these as
dangerous: check `hub_instances` first, coordinate via `hub_send` or ask the user if any peer is
live, and ask the user even when alone. This is **prompt-level guidance only** — no PreToolUse
hard block (that was considered; command-detection is heuristic and shell-bypassable).

## Verified so far

- Both crates + the Tauri app compile clean; `svelte-check` + `vite build` clean.
- The whole coordination hub works **end-to-end through `mulpex-helper`**: MCP `initialize` /
  `tools/list` (all 5 `hub_*` tools), `hub_send`→`hub_inbox` delivery + `messages.log`,
  `userpromptsubmit` task capture + peer snapshot, and `pretooluse` `O_EXCL` lock acquisition
  with a canonical-path + heartbeat token.
- **Run as a GUI and bundled.** `npm run tauri dev` and `tauri build` (→ signed `Mulpex.app`
  with the `mulpex-helper` sidecar inside `Contents/MacOS/`) both work; multi-instance spawn,
  focus-switch, resize, and session `--resume` verified in the window.
- **Idle-wake hub listener verified live.** Two instances armed their listeners at startup; a
  `hub_send` from one woke the idle peer via its Monitor event (no injected prompt line), which
  read its inbox and replied — round-trip confirmed both directions, with the `⟳` marker and a
  clean sidebar (the sentinel + `<task-notification>` task-capture skips work).

## Last Synced Commit

`1fadc63e9d9cc1ea5f756ebe684a05b43485d897` — 2026-07-23
