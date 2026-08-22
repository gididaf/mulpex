# Mulpex (desktop app) — design notes

Native macOS Tauri app hosting multiple coordinated Claude Code sessions. Successor to the
terminal-UI mulpex (`../mulpex-deprecated`). This doc covers what's specific to the desktop
rewrite; the coordination-hub semantics are unchanged from the deprecated project's `CLAUDE.md`
(the hook/mcp/persist modules were copied verbatim).

**This file is the always-loaded part: orientation, vocabulary, and the rules you can break from
anywhere.** Everything else — each subsystem's full story, and the evidence behind it — lives in
`docs/`, indexed below. The `docs/` files are not optional reading; they are *deferred* reading.
Open the one that covers what you are about to change, and note that `src/`, `src-tauri/` and
`crates/mulpex-core/` each carry a small `CLAUDE.md` router that points at the right ones.

**New notes go in the `docs/` page that owns the subsystem, not here.** This file earns its
length back only by staying short; it grew to 2000 lines once already. Something belongs in this
file only if it is orientation you need before you can navigate at all, or a rule you could
break from a file that doesn't mention it — and even then, as a short form linking to the full
reasoning in `docs/`.

## Where things are documented

| File | Covers | Read it before touching |
| --- | --- | --- |
| [docs/rendering.md](docs/rendering.md) | One geometry; RTL/BiDi; terminals alive while hidden | `terminals.ts`, `TerminalView/Pane.svelte`, `styles.css`, any spawn/resize path |
| [docs/frontend.md](docs/frontend.md) | Sidebar order (claudes above terminals), context menu, dropped paths, mute, drag-reorder, tab badges, attention/dock, hub panel | `src/lib/components/*`, `stores.ts`, `attention.ts`, `App.svelte` |
| [docs/sessions.md](docs/sessions.md) | Finding the `claude` binary + login env; status words (`needs`/`working`); failed starts; stable instance numbers; failed restores | `claude_bin.rs`, `pty.rs` spawn, `hook.rs` status writes, `persist.rs`, `reap_dead` |
| [docs/hub.md](docs/hub.md) | Idle-wake listener, `hub_set_name`, cross-project `<project>#<n>`, `hub_spawn` + task delivery | `mcp.rs`, `hook.rs`, `registry.rs`, `state.rs` poll-loop handshakes |
| [docs/shell-terminals.md](docs/shell-terminals.md) | ⌘⇧T shells, `vtgrid` transcript, `hub_terminal_*`, killing jobs | `vtgrid.rs`, `termlog.rs`, `SessionKind`, `Session::kill`, terminal MCP tools |
| [docs/remote-peers.md](docs/remote-peers.md) | `hub_remote_open`, the `<<<MPX …>>>` marker, screen-only reads | `remote.rs`, the remote watcher in `state.rs` |
| [docs/packaging.md](docs/packaging.md) | Helper sidecar bundling, TCC + signing identity, the DMG Finder race (`CI=true`), auto-update, teardown | `tauri.conf.json`, `scripts/release.sh`, `lib.rs` `RunEvent`, anything about shipping |
| [docs/verification-log.md](docs/verification-log.md) | What was actually measured/driven, and what was NOT | Before claiming something is verified, or re-testing something |

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
crates/mulpex-core/   headless lib: hook, mcp, persist, config (copied verbatim from the TUI)
                      + termlog (the terminal-transcript header, written by the app and
                        parsed by the helper — the one format both processes must agree on)
crates/mulpex-helper/ bin: `hook <event>` / `mcp` dispatch → mulpex-core
src-tauri/            the Tauri app (Rust backend)
  src/pty.rs          Session = one claude OR one shell on a PTY (SessionKind), streaming
                      to a frontend Channel
  src/vtgrid.rs       shell terminals only: a small VT grid → plain-text transcript on
                      disk, so a claude in another process can read a terminal's output
  src/state.rs        Workspace = N open projects (Vec<Core> + active handle + the ONE
                      geometry every PTY spawns at); each Core = one project + sessions
                      + its OWN scratch dir; reap/persist/hub-read
  src/commands.rs     #[tauri::command] surface (session cmds carry a projectHandle)
  src/hub.rs          200ms poll over ALL projects → emits handle-scoped hub-update /
                      session-exited / sessions-changed (+ projects-changed)
  src/menu.rs         native ⌘ menu; ids forwarded to the frontend as a `menu` event
  src/project.rs      recents + open-project set (~/.mulpex/recents.txt, open.txt)
  src/snapshot.rs     serde types shared w/ frontend (adds ProjectHandle, WorkspaceInfo)
src/                  Svelte/Vite frontend
  lib/terminals.ts    TerminalManager: one xterm per session keyed by (project,id),
                      alive-while-hidden across ALL projects, central resize
  lib/ipc.ts          typed command/event/channel wrappers
  lib/stores.ts       per-project state map + derived active-project projections (PTY bytes bypass)
  lib/updater.ts      update check/download/apply + the cross-project busy-session count
  lib/components/*     ProjectTabBar, CommandPalette, TopBar, InstanceList, HubPanel,
                      TerminalPane/View, MessageReader, Rename, ContextMenu, UpdateBanner…
scripts/release.sh    signed build → latest.json → gh release (docs/packaging.md)
docs/                 the deferred half of these notes — see the table above
```

`src/`, `src-tauri/` and `crates/mulpex-core/` each hold a short `CLAUDE.md` router listing the
`docs/` pages and the directory-local traps that apply when you're editing there.

## The helper (why it's a separate binary)

Child `claude` processes invoke `<helper> hook <event>` (from `settings.json`) and `<helper> mcp`
(from `mcp.json`) **by absolute path** — `claude`, not Tauri, spawns them. A `PreToolUse` hook
forks on every Read/Write/Edit/Bash and the MCP server is long-lived per instance, so the helper
must be tiny and fast to exec. It links only `mulpex-core` (~1.8 MB vs the ~29 MB app).

**Path resolution** (`lib.rs::resolve_helper_path`): `current_exe().parent().join("mulpex-helper")`
— works in `tauri dev` (`target/<profile>/`) and in the bundled `.app` (`Contents/MacOS/`). The
absolute path is substituted for `__MULPEX_BIN__` in the config templates when a project opens
(`state.rs::Core::open`).

Bundling it as a **signed sidecar** is what keeps hooks working in the shipped `.app` — an unsigned
helper is SIGKILLed by Gatekeeper and **every hook then fails open silently**. Details in
[docs/packaging.md](docs/packaging.md).

## Data flow

- **PTY → frontend:** per-session `tauri::ipc::Channel<String>` carrying **base64** chunks
  (`pty.rs::OutputSink`, decoded in `terminals.ts`). Before the frontend attaches (restored
  sessions paint immediately on `--resume`), output is **buffered** and flushed on
  `attach_session` — race-free under the sink mutex.
- **frontend → PTY:** `send_bytes(id, data)` from xterm `onData` (UTF-8 encoded). The one manual
  key carve-out is **Shift+Enter / Option+Enter → `\x1b\r`** (`attachCustomKeyEventHandler`) —
  meta-Return, the sequence `/terminal-setup` installs for VS Code (`sendSequence "\x1b\r"`) and
  Terminal.app ("Use Option as Meta"). Claude accepts a bare `\n` too; ESC+CR is preferred only
  because it's the documented one. Both bytes must reach the PTY in **one write** (`send_bytes`
  → `write_all`): split across reads, Claude consumes the lone ESC and the CR then submits.
  `macOptionIsMeta` covers Option word-motions.

  **The handler must call `e.preventDefault()`** — this is the whole fix, not the byte choice.
  Returning `false` makes xterm bail out of `_keyDown` *before* setting `_keyDownHandled`, and it
  does not cancel the event on that path. The browser therefore still fires `keypress`, and
  xterm's `_keyPress` (which short-circuits only on `_keyDownHandled`) turns charCode 13 into a
  `\r`. So the PTY got our newline **and then a submit** — the newline was inserted and the
  message sent in the same keystroke, which is what "Shift+Enter sends the message" was. The
  `keypress` arm of the handler is a second line of defence. Option+Enter accidentally worked
  throughout, because `_keyPress` already ignores keys with `altKey` set.
- **hub state:** the 200 ms poll walks **every open project**, reads each one's scratch-dir files
  into a `HubSnapshot`, and emits a **handle-scoped** `hub-update {handle, snapshot}` on change;
  the frontend keys it into that project's slice, and the sidebar/hub panel are a reactive
  projection of the *active* project. Reaping (with `bounce_dead_inbox`, peer-list rewrite,
  persistence) is **single-sourced in the poll loop** — an explicit ⌘W close just `kill()`s and
  lets the next reap emit `session-exited {handle, id}`, identical to a self-exit.

## Multiple projects (Workspace)

Mulpex hosts **several projects at once in one window** and switches between them instantly. The
single `Core` is now one of N owned by a `Workspace` (`state.rs`: `Mutex<Workspace>` replacing
`Mutex<Option<Core>>`), each with a **monotonic `ProjectHandle`** (never reused after close, so a
stale reference resolves to a no-op) and its **own scratch dir** `temp/mulpex-<pid>/<handle>/`.

- **Hub isolation is free:** the hub is scoped entirely by `MULPEX_STATE_DIR`, passed per session
  in `pty.rs::Session::spawn`, so a per-project `state_dir` isolates each project's hub with no
  other change — instances only coordinate with peers in the *same* project. Instance ids stay
  per-project (each numbers 1,2,3…), so every session command carries a `projectHandle` and every
  hub event is handle-scoped; `(handle, id)` disambiguates.
- **Commands:** `bootstrap()` (replaces `current_project`) returns a `WorkspaceInfo { projects,
  active }`; `open_project` dedups canonically (re-activates if already open) and emits
  `projects-changed`; `close_project` tears the project down + re-picks the active neighbor;
  `switch_project` sets the active handle. Session cmds (`attach/create/close/rename/send_bytes/
  resize/focus_session`, `get_hub_snapshot`) all take `projectHandle`.
- **Frontend:** `stores.ts` holds a `Map<handle, ProjectState>` + `activeProjectHandle`; the
  classic `sessions/statuses/tasks/hub/activeId/project` stores are **derived read-only
  projections of the active project**, so the sidebar/hub components are unchanged. Navigation is a
  persistent **`ProjectTabBar`** (click / `+` open / ✕ close) plus a **`⌘P`
  `CommandPalette`** fuzzy switcher (drag-and-drop no longer opens projects — see
  [docs/frontend.md](docs/frontend.md)). `TerminalManager` keys xterms by `(handle,id)` and keeps
  **every project's** terminals alive while hidden; exactly one is visible globally.
- **Persistence:** the open-project set is saved to `~/.mulpex/open.txt` (distinct from
  `recents.txt`) on open/close; on launch every project reopens (`--resume`).

## Keyboard

Native macOS menu accelerators (⌘T/**⌘⇧T**/⌘W/⌘R/⌘M/⌘⇧M/⌘[ ⌘]/⌘O/⌘Q, plus **⌘⇧W** close project and
**⌘⇧] / ⌘⇧[** next/prev project) are intercepted by the menu before xterm; Claude never uses ⌘,
so there's zero collision. **⌘P** (the project quick-switcher) is *not* a menu accelerator — it's
handled in the webview (`svelte:window` keydown, `preventDefault` stops the print dialog).

**⌘M is Mute Session; the message reader moved to ⌘⇧M.** ⌘M has a *third* claimant nobody
declares: muda hard-binds `PredefinedMenuItem::minimize` to ⌘M and exposes no accelerator setter.
Two items on one key means the earlier menu silently wins (the app already ships that collision —
File ▸ Close Session and Window ▸ Close Window both claim ⌘W), which would leave the Window menu
advertising a ⌘M that mutes. So **Minimize is a custom item with no accelerator**, handled in
`App.svelte` via `getCurrentWindow().minimize()`. Don't "restore" the predefined one.

Mute is a **`CheckMenuItem`**, and muda flips a check item's own state *before* dispatching the
event — so the tick has to be pushed back from the frontend (`set_mute_menu_checked` →
`menu::set_mute_checked`, which walks one level into the Session submenu because `Menu::get` only
searches direct children). `App.svelte::syncMuteMenu` dedups that IPC call, since its trigger
re-runs on every 200 ms hub poll; the one `force` case is a ⌘M with no session focused, where muda
has ticked an item we never wrote and the dedup would otherwise never clear it.

**⌘1–⌘9 select the Nth open *project*, not the Nth session** (menu ids `project_<n>`, File menu).
Projects are the top-level thing you switch between — sessions have ⌘[ / ⌘] and the sidebar — and
⌘N-selects-a-tab is the browser/terminal convention the tab bar already implies. The index is the
`projects` Map's insertion order, which is exactly what `ProjectTabBar` renders, so ⌘3 is always
the third visible tab.

Everything else (arrows, Ctrl+C, Esc, Shift+Enter) flows straight to the focused terminal.
Copy/paste/select-all are predefined Edit-menu items macOS routes to the focused xterm textarea.

## Vocabulary (fixed — use these words)

- **project** — one entry in the project tab bar, named by its folder (`cloud`, `central-one`).
- **instance** — one sidebar row: a claude **or** a terminal. Written **`claude#1`**, **`term#5`**,
  no space, in prose, tool descriptions, `HUB_RULES` and error strings. Not "session", not "peer
  #3".

The code used instance/session/peer/terminal-id interchangeably until this was settled; one
vocabulary is what lets an address like `central-one#3` be read without guessing. The sidebar
itself still renders `claude #1` with a space (`InstanceList.svelte`); only the written/address
form drops it.

## Lifecycle

- **Startup:** `lib.rs::setup()` reopens **every project in `open.txt`** (each builds its `Core` —
  scratch dir, config files, restore sessions — before the window paints; output buffers
  pre-attach). The frontend then calls `bootstrap()` → `WorkspaceInfo`, **adopts the geometry it
  reports** (`terminals.setGeometry`, before any `TerminalView` mounts — the buffered startup paint
  is about to be flushed and the xterm has to already be the size it was rendered for), builds one
  xterm per session of each project, `attach_session`es each, and activates `active`. Restored
  instances come back with the numbers they had, gaps included. With no open projects
  it shows the picker (recents + `@tauri-apps/plugin-dialog` folder picker); opening one goes
  through `open_project(path)`.
- **No project ever auto-starts a `claude`.** `Core::open` restores what was worked on last time
  and stops there — with nothing to restore the project opens with **zero sessions** and waits for
  ⌘T. Because startup restore and `open_project` share that one path, a freshly added project
  behaves the same as a restored one. Zero sessions was already a supported state (⌘W on the last
  session produces it): `active` stays `0` and indexes nothing, `bootstrap_info` gives the frontend
  `activeSessionId: null`, `selectProject` calls `terminals.focus(handle, -1)` so no terminal is
  visible, and `TerminalPane` shows its "press ⌘T" empty state. Guarded by
  `state.rs::tests::open_with_nothing_to_restore_spawns_no_session`.
- **Teardown:** handled in `lib.rs` via `RunEvent` (managed-state `Drop` isn't guaranteed on
  exit). Window close → `app.exit(0)` → `ExitRequested` → `Workspace::teardown_all()` kills
  **every project's** process groups (`killpg` SIGHUP→SIGKILL→wait) **then** removes the whole
  scratch root. This is the "no orphaned claude" guarantee, now across all projects. `open.txt` is
  **not** touched on teardown, so the set survives to next launch.

## Shared-tree guardrail

(Note this is per project — an instance in *another* project has its own tree, and none of this
applies between them. See [docs/hub.md](docs/hub.md).)

All instances **of a given project** share one working directory and one git checkout (not a
worktree per instance; separate projects are separate trees), so
a tree-wide git op by one (`reset --hard`, `checkout .`/`restore .`, `clean`, `stash`, branch
switch, `rebase`/`revert`) or any bulk-destructive command wipes every other instance's
uncommitted work. `HUB_RULES` (the append-system-prompt) tells each instance to treat these as
dangerous: check `hub_instances` first, coordinate via `hub_send` or ask the user if any peer is
live, and ask the user even when alone. This is **prompt-level guidance only** — no PreToolUse
hard block (that was considered; command-detection is heuristic and shell-bypassable).

## Invariants you can break from anywhere

Short forms of rules whose full reasoning lives in `docs/`. Each one has been broken at least once
and cost real time; each links to the measurement that settled it.

- **One geometry, or the pane is corrupted forever.** An xterm must never be fed bytes rendered for
  a different size — claude repaints *differentially* and never rewrites a row it believes is
  correct, so debris is permanent. `Workspace::geometry` is the single (cols, rows) every PTY in
  every project spawns at, and the frontend adopts it *before* any `TerminalView` mounts. Resize is
  workspace-wide, never per project. → [docs/rendering.md](docs/rendering.md)
- **The WebGL renderer must not come back.** It draws one glyph quad per cell, so RTL text renders
  mirrored. The DOM renderer plus a `display: inline !important` override on `.xterm-rows span` is
  what makes Hebrew read correctly, and the user works in Hebrew. Not negotiable for speed.
  → [docs/rendering.md](docs/rendering.md)
- **Never break the signing identity.** TCC folder grants are pinned to the bundle's designated
  requirement. The self-signed certificate at `~/.mulpex/signing/` (and `~/.mulpex/updater.key`) are
  as load-bearing as the source: lose either and every user's folder permissions reset, or no
  install can ever update again. → [docs/packaging.md](docs/packaging.md)
- **`--dangerously-skip-permissions` is on, and hub instances share one working tree.** See
  **Shared-tree guardrail** above; remote peers run unattended on someone else's box.
  → [docs/remote-peers.md](docs/remote-peers.md)
- **A child must not inherit a hub identity or `CLAUDE_CODE_CHILD_SESSION`.** The former corrupts
  the hub; the latter silently disables transcript saving, so the breakage only appears at the
  *next* launch as an unrestorable session. → [docs/sessions.md](docs/sessions.md)
- **A terminal is never a hub peer.** Excluding shells from `statuses` is what keeps the dock badge,
  the tab badges and the updater's busy guard correct for free.
  → [docs/shell-terminals.md](docs/shell-terminals.md)
- **`to: "all"` and locks stay project-local**, and `hub_spawn` only creates instances in its own
  project. Only `hub_send`/`hub_inbox`/`hub_instances` cross the boundary.
  → [docs/hub.md](docs/hub.md)

## How this codebase fails

Every expensive bug in this project has had the same shape, and the pattern is worth carrying into
new work more than any individual fix is.

- **A real event with nowhere to arrive.** `lib.rs::is_forwarded` is an *allowlist*: a new menu item
  builds, appears, shows its accelerator and even ticks itself, while the frontend never hears a
  thing — no error, no log line, no compiler complaint. `notification:default` in
  `src-tauri/capabilities/default.json` is the same shape (miss it and `sendNotification` is denied
  at runtime, so notifications look "flaky" rather than off). So is an error rendered only inside
  the project picker, which is off-screen whenever a project is open. **When you add a case to a
  dispatcher, find its allowlist.**
- **A default that reads as an assertion.** `status: waiting` is `mcp::status_of`'s default for a
  *missing* file — it reports ignorance in the same word it reports idleness. `ok: true` used to
  mean the process existed, not that its task arrived. **Say what you know; don't round it up.**
- **It reproduces only in the shipped `.app`.** `tauri dev` inherits your terminal's environment, so
  the whole `PATH` / `TERM` / login-token class is invisible there. Finder gives LaunchServices'
  bare environment; reproduce with `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`.
- **The one-item case passes.** Six concurrent `hub_spawn` children blew a timeout that one child
  never approached; a `hub_terminal` bug appeared only on the second command. **Test the plural.**
- **Measure; don't reason.** The list of things that were *wrong* by inspection and *right* by
  measurement is long: the Shift+Enter byte (never the problem — the stray `keypress` `\r` was), the
  `__MPX__` marker (markdown ate the underscores), task truncation (it was a 91 s readiness stall),
  and reading Hebrew off a screenshot (transcription re-applies BiDi and hides which end is which).
  Drive a real `claude` on a PTY, replay real bytes through the real xterm build, and read the live
  scratch dir — `armed/` present with `named/` absent distinguishes "never called" from "refused" in
  one listing. → [docs/verification-log.md](docs/verification-log.md)

## Last Synced Commit

`1bb57fa70d556bea4f266ca77af7a5385408a112` — 2026-08-22
