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
crates/mulpex-core/   headless lib: hook, mcp, persist, config (copied verbatim from the TUI)
                      + termlog (the terminal-transcript header, written by the app and
                        parsed by the helper — the one format both processes must agree on)
crates/mulpex-helper/ bin: `hook <event>` / `mcp` dispatch → mulpex-core
src-tauri/            the Tauri app (Rust backend)
  src/pty.rs          Session = one claude OR one shell on a PTY (SessionKind), streaming
                      to a frontend Channel
  src/vtgrid.rs       shell terminals only: a small VT grid → plain-text transcript on
                      disk, so a claude in another process can read a terminal's output
  src/state.rs        Workspace = N open projects (Vec<Core> + active handle); each
                      Core = one project + sessions + its OWN scratch dir; reap/persist/hub-read
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
                      TerminalPane/View, MessageReader, Rename, UpdateBanner…
scripts/release.sh    signed build → latest.json → gh release (see Auto-update)
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

### Bundling the helper (wired)

The dev flow needs nothing (helper sits beside the app in `target/`). For `tauri build`,
`mulpex-helper` ships as a **signed sidecar** so it lands in `Contents/MacOS/` *and is signed with
the bundle* — otherwise Gatekeeper SIGKILLs it and **every hook fails-open silently** (no
coordination, no error). This is **wired**: `bundle.externalBin` is `["binaries/mulpex-helper"]`
in `tauri.conf.json`, and `beforeBuildCommand` runs `scripts/bundle-helper.sh`, which builds the
helper in release and copies it to `src-tauri/binaries/mulpex-helper-<target-triple>` (the
suffix Tauri expects). Tauri strips the suffix, places it at `Contents/MacOS/mulpex-helper`, and
signs it with the app. Bundle `targets` are `["app", "dmg"]`, so `tauri build` produces both
`Mulpex.app` and `Mulpex_<version>_aarch64.dmg`. Verified: the built `.app` has the signed helper
beside the main binary.

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
  **Dropped paths** below). `TerminalManager` keys xterms by `(handle,id)` and keeps
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

## Dropped paths

Dropping a file or folder on the window puts its absolute path **at the focused session's prompt**
(`App.svelte::dropPaths`) — space-separated for a multi-drop, nothing submitted. It no longer opens
the drop as a project; that's ⌘O / the `+` tab / ⌘P. The one fallback is "nowhere to type": with no
active session (the picker, or a project at zero sessions) a drop still goes to
`openOrFocusProject`, and the backend rejects non-directories.

**The paths go over as one bracketed paste (`ESC[200~ … ESC[201~`), not as typed keystrokes** —
this is load-bearing, not incidental framing. Paste is the channel Claude Code inspects for
attachments: a **pasted** image path becomes an `[Image #N]` attachment the instance can actually
*see*, while the identical path *typed* stays inert text. That one byte-level difference is the
whole reason dropping a screenshot used to yield nothing but a string. One paste holding every
dropped path is the correct shape — Claude extracts each image and leaves non-images as text.

**Files and folders behave identically, deliberately** — this matches **Claude Code's own
drag-and-drop**, where dragging either into the terminal adds its path. The v0.4.0
drag-a-folder-to-open-a-project gesture was a Mulpex-only invention that shadowed the standard
behavior; a directory is a normal argument to hand Claude, and project-opening already has three
dedicated entry points. Don't "restore" the old gesture without asking — matching stock
Claude Code is the intent, not an oversight.

Two non-obvious constraints, both of which this had to be built around:

- **Tauri owns the drop.** `dragDropEnabled` defaults to **true**, so the webview converts the
  native OS drop into an `onDragDropEvent` and the DOM never fires `drop` — xterm cannot see it,
  so that handler is the only place a drop can be honoured. (xterm.js has no built-in
  path-insertion either; dragging a file to get its path is *emulator* behavior from
  Terminal.app/iTerm, so it had to be written by hand regardless of which layer got the event.)
  The event is also **window-wide**, not per-element: a drop on the sidebar or tab bar is
  indistinguishable from one on the terminal.
- **`escapePath` backslash-escapes; it does not quote.** Anything a shell would act on gets a
  `\`, which is what Terminal.app/iTerm insert on a drag — so the prompt reads
  `/Users/me/My\ File.csv`, matching stock Claude Code, rather than a quote-wrapped path.
  Characters ≥ U+0080 stay **bare**: real terminals don't escape unicode. The one exception is
  ANSI-C `$'…'` for control characters, which is a *PTY* concern rather than a shell one — a
  literal `\n` in a filename (legal on macOS) would submit the message halfway through the path,
  and `$'…'` keeps the wire bytes CR/LF-free while round-tripping to the real name.
- **The trailing space belongs INSIDE the paste markers.** It looks misplaced and isn't: a space
  written *after* `ESC[201~` **wipes the prompt** for any non-image path — the path renders and
  Claude then erases the line, presumably the keystroke racing its async paste handling. Images
  are unaffected by it, so testing only an image drop passes this bug straight through. Measured;
  don't "tidy" the space back outside.

The old behavior was a **silent** failure worth remembering as a pattern: the handler passed every
dropped path to `openOrFocusProject`, a file hit `state.rs`'s `bail!("not a directory")`, and that
message landed in `openError` — which renders *only inside the picker*, off-screen whenever a
project is open. Same shape as the Finder-launch `claude`-not-found bug: a real error with nowhere
to appear.

## Muted sessions (⌘M)

A muted instance **keeps running and keeps coordinating** — same PTY, same inbox, same peer list,
same `hub_instances` entry. Mute is purely a statement about how loudly the *sidebar* may talk
about it, and it's deliberately not a hub concept: nothing in `mulpex-core` knows the flag exists.
Concretely it: dims the row, **sinks it below the unmuted ones**, drops its status dot, its status
word and its ⏳, and removes it from **every attention count** — the tab's red `needs` badge, the
amber unread badge, and the hub-panel/status-strip unread readouts.

- **Ordering is one function**, `stores.ts::displayOrder` — a stable sort on `Number(muted)`, so
  each group keeps its base order (creation order, or whatever a drag arranged — see **Sessions
  drag to reorder**) and unmuting drops a session straight back where it came from. It feeds both
  the sidebar and ⌘[ / ⌘], so what you see is what you cycle. `TerminalPane` is unaffected
  (absolute stacking; order is meaningless there).
- **The unread badge needed a backend change.** `pending_messages` is one project-wide total, and
  "how much of this is mail for a muted instance" isn't answerable from a total — so the poll loop
  now also emits a per-recipient `pending: Vec<PendingEntry>` breakdown, and `unreadCount`
  subtracts the muted share. The **message log itself is untouched**: mute silences the count that
  pulls your eye, not the record of what happened.
- **Persisted per project**, alongside the custom name, as a third tab-separated field in
  `~/.mulpex/sessions/<key>.txt` (`<uuid>[\t<name>[\tmuted]]`). Both older formats still load — a
  bare uuid and a `<uuid>\t<name>` line — and a muted-but-unnamed instance writes the name column
  empty so the flag stays in field three. Covered by three `persist.rs` tests.
- **Muting never moves focus**, and the muted terminal stays visible and typeable. Mute means "stop
  shouting at me", not "I'm done with this one".
- **The 🔇 is not decoration.** A dimmed, dot-less, status-less row would otherwise read as *dead*
  rather than *silenced* — same failure the empty hub-panel sections had, an ambiguous readout that
  teaches the eye wrong. It's also the click target for muting a session **without focusing it**
  (unmuted rows show a 🔊 only on hover, so it stays reachable without adding noise).

**A new menu item is not wired until `lib.rs::is_forwarded` lists its id.** That function is an
*allowlist* — `on_menu_event` drops anything not in it — so a new item builds, appears in the menu,
shows its accelerator, and (for a `CheckMenuItem`) even ticks itself on click, while the frontend
never hears a thing. Nothing fails: no error, no log line, no compiler complaint. Both `mute` and
`minimize` shipped in that state and were caught only by driving the real app. This is the same
shape as the other silent failures in this file — a real event with nowhere to arrive.

## Sessions drag to reorder

Sidebar rows drag vertically exactly as project tabs drag horizontally — same mechanism
(**pointer events, not HTML5 drag-and-drop**, because Tauri's webview drag-drop is enabled for
dropped paths and intercepts DOM drags; pointer capture also gives the 4 px threshold that keeps a
click from registering as a drag), same `suppressClick` so a drag never also selects the row, same
visuals (dragged row fades, target slot gets an accent edge — on the *top* edge here, since the
list runs vertically). Terminals drag like instances: one list, one behavior.

- **Manual order and muted-sinking are composed, not alternatives.** `p.sessions` is the *base*
  order a drag rewrites; the sidebar renders `displayOrder(p.sessions)` on top of it, so muted rows
  still sink. That means a drop **across the mute boundary could never stick** — the row would
  visibly snap back on release. So drops are **clamped to the dragged row's own group**
  (`stores.ts::clampToGroup`): dragging a muted row to the top lands it at the top of the *muted*
  block. Clamping keeps the drop indicator honest and keeps the emitted order already-grouped,
  which is the invariant that makes `displayOrder` of it the identity — i.e. the frontend's
  optimistic repaint is exactly what the backend echoes back. The math lives in `stores.ts` next to
  `displayOrder` (`clampToGroup` / `dragOrder`) rather than in the component, because it is a
  consequence of that sort and the two rules have to stay in one file.
- **Persisted, via the backend.** `reorder_sessions(handle, ids)` → `Core::reorder_sessions`
  rearranges the `sessions` vec, which *is* the persisted order (`persist_sessions` walks it), so a
  drag survives relaunch like a tab drag does. Terminals have no position after a restart because
  terminals themselves aren't persisted.
- **`Core.active` is an index into that vec**, so it must be re-derived from the focused session's
  *id* after a reorder — carrying the index across would silently focus whichever session slid into
  that slot. Guarded by `reordering_sessions_keeps_focus_and_never_drops_one`, which also pins the
  never-drop contract (ids the caller omitted are appended, unknown ids ignored) shared with
  `Workspace::reorder_projects`.
- Sidebar order is what ⌘[ / ⌘] cycle, so a drag remaps those too — the same "what you see is what
  you cycle" rule the muted sort already follows.

## What a project tab shows

Name + **session count** (always, `0` included — "nothing running here" is information) + two
count badges, each for a different ask, so a colored pill is never ambiguous: **red =
sessions in `needs`** (a claude stopped to ask *you* something) and **amber = unread hub
messages**. Both hide at zero, and **both exclude muted sessions** (see above) — the plain session
count does not, because it says what's *here*, not what wants you. By the same rule the plain count
includes **terminals**, while the badges exclude them for free (a terminal has no status entry and
no inbox). The needs count is the gap this closes —
a background project blocked on a question used to look identical to an idle one, findable only by
switching tabs, even though `ProjectState.statuses` had the answer all along. ⌘1–9 selects a tab
(see **Keyboard**).

**Tabs drag to reorder.** `ProjectTabBar` uses **pointer events, not HTML5 drag-and-drop** —
Tauri's webview drag-drop is enabled (App.svelte needs it to drop folders onto the window) and
intercepts drags before the DOM sees them; pointer capture also gives us the 4 px threshold that
keeps a click from registering as a drag. Dropping calls `reorderProjects` in `stores.ts` (rebuilds
the `Map`, since insertion order *is* tab order) and the `reorder_projects` command, which reorders
`Workspace::projects` and re-runs `persist_open()` — so the arrangement survives relaunch. Tab
order is also what ⌘1–9 index into, so a drag remaps them by design. Handles missing from the
submitted order are appended rather than dropped, so a stale caller can't make a project vanish.

## Attention: dock badge + notifications

`attention.ts` surfaces blocked claudes when you're *not* looking at Mulpex, both keyed off
`needs` (the status the `AskUserQuestion` / idle-prompt hooks write — see `config.rs`):

- **Dock badge** — `blockedTotal` (`stores.ts`) sums `needsCount` across *all* open projects and
  drives `setBadgeCount`. Zero must be passed as `undefined`, or the dock shows a literal "0".
- **Notification** — one silent banner per claude at the moment it becomes blocked, only when the
  window is unfocused. Clicking one raises the window and routes through the same select path as
  clicking a sidebar row, landing you on the pane with the question (the project handle + session
  id ride along in the notification's `extra`).

Three deliberate choices. It tracks `needs` and **not** `waiting`: `waiting` only means a turn
ended, which happens constantly and asks nothing of you — badging it would leave the dock lit
permanently and stop meaning "there is something to do". Muted sessions are excluded, matching the
tab badges. And the first sweep only *records* state (`primed`), because restored sessions can
already be in `needs` at launch and a burst of stale banners would bury the live one.

> `needs` fires less often than you'd guess: sessions run with `--dangerously-skip-permissions`, so
> the `permission_prompt` matcher is effectively dead and `needs` means AskUserQuestion or idle.

Banners come from **`tauri-plugin-notification`**, which needs *two* registrations to work — the
plugin in `lib.rs` **and** `notification:default` in `src-tauri/capabilities/default.json`. Miss
the capability and `sendNotification` is simply denied at runtime; the badge (a core window API)
keeps working, so the failure looks like "notifications are flaky", not "notifications are off".
Same allowlist shape as `lib.rs::is_forwarded` for menu ids.

## Hub panel is Messages only

`HubPanel.svelte` renders **Messages** and nothing else. It used to show **Waiting** and **Locks**
above it, anomaly-only (rendered only when non-empty — no header, no `none` placeholder), and both
were removed at the user's request: neither is something you steer with. Locks release at *turn*
boundaries (`hook.rs::release_my_locks` on Stop), not per tool call, so with a single session in a
project the lock list was always that session's own files — which is why it was already suppressed
at `sessions.length <= 1`, and why it was never worth the panel row. Don't "restore" either
section without asking.

Contention is still visible, in the place you're already looking: the ⏳ on the blocked session's
row in `InstanceList`, which reads `$hub.waiting` directly. So the **backend snapshot still carries
`locks` and `waiting`** (`ipc.ts::HubSnapshot`, `snapshot.rs`) — this was a UI-only removal, and
stripping the fields would break that ⏳.

The sidebar splits **72% session list / 28% hub panel** (was 45/55 — messages are reference
material, the session list is what you steer with). Both rows scroll independently, which needs
`min-height: 0` on the grid items (`.sidebar > :global(*)` in `App.svelte`): without it a grid
item's *auto* minimum lets it grow past its track and the children's `overflow-y: auto` never
engages.

## Terminals kept alive while hidden

`TerminalPane` stacks the xterms of **every session across every open project** absolutely (keyed
`(handle,id)`); exactly one — the active project's active session — is `visibility: visible`, all
the rest `visibility: hidden` (**never** `display:none`, which would zero their size and break
`fit()`). Hidden terminals (including whole background *projects*) keep receiving `term.write()`,
so background Claudes keep rendering. Geometry is central: a `ResizeObserver` on the pane fits the
visible terminal, then applies the same `cols/rows` to every session + backend PTY (all PTYs share
one size, as the TUI did) — `refit` issues one `resize_session(handle,…)` per open project so
background projects aren't left at spawn size.

## RTL (Hebrew/Arabic) — two separate fixes, both load-bearing

Terminals use xterm's **DOM renderer**. The WebGL addon was removed for this fix and **must not
come back for speed** — it draws one glyph quad per cell, so column *n* always gets character *n* and
RTL text renders mirrored (Hebrew read backwards). The DOM renderer emits each styled run as a
`<span>` of real text and the **browser's own BiDi engine** reorders it for free. Measured, same
frame through the app's xterm 5.5.0 in headless Chrome: DOM → `שלום זאת בדיקה`, WebGL →
`הקידב תאז םולש` (the reported bug, reproduced). xterm has **no BiDi of its own** (`grep -c
"bidi\|rtl"` on `lib/xterm.js` is 0), so the browser is the only implementation available; a
`unicode-bidi: plaintext` CSS override on the rows changes nothing (already the default behavior),
and `direction: rtl` flips the box-drawing borders and is unusable. Dropping the addon also
deleted the GL-context juggling (attach-on-focus / dispose-on-blur, since browsers cap live
contexts) and cut ~100 kB from the JS bundle.

**That alone only fixed the letters.** Words still ran left-to-right, because xterm's DOM renderer
injects `.xterm-rows span { display: inline-block }` — and an inline-block is an **atomic** inline
box, which the BiDi algorithm treats as one opaque object. It can reorder text *inside* a span but
never *across* spans, and Claude Code colors words individually (`<span>Opus</span><span>
</span><span>5</span>…`), so a Hebrew sentence became one span per word: letters right, words
mirrored-wrong. `src/styles.css` overrides it with `display: inline !important` (`!important` is
required — the injected rule is more specific and lands in `<head>` later). Measured on a 7-span
row: `inline-block` → first word leftmost; `inline` → whole run mirrored, correct. `inline` rather
than `display: contents` on purpose — the span keeps its box, so background colors, the block
cursor and wide-char widths still paint (CJK/emoji x-positions verified byte-identical).

How this was found, for the next RTL bug: a Python `pty.fork()` harness drove a real `claude`,
typed the sentence keystroke-by-keystroke and captured the raw bytes; those bytes were replayed
into the app's own xterm in headless Chrome, and **each character's x-position was measured** with
`Range.getBoundingClientRect` rather than eyeballed — reading Hebrew out of a screenshot is
useless here, because transcribing it silently re-applies BiDi and hides which end is which.

Residual limit: the caret is still column-based, so it can sit visually off inside an RTL run.

## Lifecycle

- **Startup:** `lib.rs::setup()` reopens **every project in `open.txt`** (each builds its `Core` —
  scratch dir, config files, restore sessions — before the window paints; output buffers
  pre-attach). The frontend then calls `bootstrap()` → `WorkspaceInfo`, builds one xterm per
  session of each project, `attach_session`es each, and activates `active`. With no open projects
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

## claude binary

Mulpex launches the **user's stock `claude`** (`pty.rs::claude_command`) — no byte-patching, no
re-signing. (The deprecated TUI shipped a `patch-claude-maxq.py` hack that raised the
`AskUserQuestion` caps to 10/10; that was intentionally dropped so instances behave exactly like a
plain `claude`. The matching "you may ask up to 10 questions/options" NOTE was removed from
`PLANNING_RULES` too — only the zero-assumptions planning discipline remains.)

### Finding it from a GUI launch (`claude_bin.rs`)

A bundle launched from **Finder inherits LaunchServices' environment, not a login shell's** —
`PATH` is the bare `/usr/bin:/bin:/usr/sbin:/sbin` and there is no `TERM`/`LANG`. `tauri dev`
hides this completely by inheriting the terminal's env, so both consequences only ever appeared
in the shipped `.app`:

- **`claude` was unfindable** (the installer puts it in `~/.local/bin`), so a bare
  `CommandBuilder::new("claude")` failed to spawn → `Core::open` errored → `open_project`
  returned `Err` → the frontend swallowed it in a `console.error`. Clicking a project did
  *nothing*, silently. `claude_bin::merged_path()` now rebuilds the real `PATH` — a
  `$SHELL -lic` probe (5 s timeout, killed on overrun; the only way to see nvm/asdf/volta) then a
  fallback list of known install dirs — resolves `claude` to an **absolute path**, and passes the
  same `PATH` to the child so *its* Bash tool finds `node`/`git`/Homebrew. Cached in a `OnceLock`,
  warmed on a background thread in `setup()` so nothing pays the probe inline.
- **Output was monochrome** — neither `portable_pty` nor `pty.rs` set `TERM`. The child talks to
  **xterm.js**, so we declare that emulator rather than inherit whatever started Mulpex:
  `TERM=xterm-256color` + `COLORTERM=truecolor`, plus `LANG` only when absent.

Failure is now visible rather than swallowed: a `claude_status` command backs a **picker banner**
when the CLI is missing, and `open_project` errors render inline in the picker.

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
- **Arming (hook-driven, no injected prompt):** only the agent can arm its *own* Monitor, and a
  `--resume` restart kills the previous one — but instead of typing a visible bootstrap prompt
  into the PTY (which looked ugly/confusing on every spawn and resume), a normal instance now
  **starts completely clean** and arms its listener from the **`UserPromptSubmit` hook** on the
  user's first turn. The hook (`hook.rs::userpromptsubmit`) injects `ARM_LISTENER_NUDGE` as hidden
  `additionalContext` — a low-key "arm your listener quietly as part of this turn" reminder — but
  only while `listener_armed(ctx)` is false, i.e. while `state_dir/armed/<id>` is absent. The
  Monitor command in `HUB_RULES` **`touch`es that flag as its first action**, so the flag tracks
  the *real* Monitor: once armed the reminder stops; if arming was missed it re-injects next turn
  (self-healing). Because `state_dir` is fresh per Mulpex launch (hence per `--resume`), the flag
  is absent at startup, so restored instances re-arm on their first prompt. The full arming
  procedure + exact Monitor command still live in `HUB_RULES` (append-system-prompt, so the wake→act
  contract survives compaction). *(`hub_spawn` children are the one case that still gets an injected
  PTY prompt — their assigned task — via `pty.rs::spawn_prompt`; they arm the listener from the
  same hook on that first turn.)*
- **On wake (auto-act):** the instance calls `mcp__mulpex__hub_inbox`, acts on the message(s)
  autonomously, replies to the sender only when it adds value (no bare acks), and prefixes the
  self-triggered turn with a `⟳ hub message from #<sender> →` marker so the human can tell it
  wasn't their prompt. This coexists with the `userpromptsubmit` hook's unread-count nudge, which
  still covers the "notice on your next prompt" path.

## Spawning instances (`hub_spawn`)

An instance can create new task-seeded siblings — e.g. fetch a list of tickets and spawn one
instance per ticket. The MCP helper runs in a **separate process** from Tauri and can't create
sessions itself, so `hub_spawn` (`mcp.rs`) is a **file handshake** through the poll loop:

- **Request:** `hub_spawn({tasks: [...]})` writes `state_dir/spawn/<token>.json`
  (`{from, tasks, ts}`), capped at `MAX_SPAWN_PER_CALL` (8) so a 50-item list can't fork 50
  `claude`s at once, then **polls** for `<token>.done` (~6 s) to return the assigned ids. That
  window and `SPAWN_STAGGER` below are coupled: a full 8-task batch spends ~3.5 s in stagger
  alone, so raising the stagger (or the cap) without raising the poll window turns a big batch
  into the "spawn requested, call `hub_instances` in a moment" fallback reply — correct, but the
  ids no longer come back in-line.
- **Fulfilment:** the 200 ms poll loop calls `Core::process_spawn_requests()` (`state.rs`), which
  consumes the request file into a `pending_spawns` queue and then **drip-feeds** it — at most one
  child per tick, and no closer together than `SPAWN_STAGGER` (500 ms) — spawning each via
  `spawn_instance_with_task(parent_id, task)`. `<token>.done` is written only once the batch's
  last child is up, so the caller still gets every id in one reply. If anything spawned it emits
  `sessions-changed` so the frontend builds the new xterms (the existing reap path already
  republishes on removal; added sessions ride the same event via `TerminalPane`'s keyed
  `{#each}`). The drip-feed **never sleeps** — this runs on the shared poll loop, so blocking
  would stall every project's UI. Staggering exists because N simultaneous `claude` cold starts
  contend hard enough to blow any injection deadline (see below).
- **Seeding + link:** the child's one-shot PTY prompt (`pty.rs::spawn_prompt`) is just the task:
  start it, then `hub_send` a summary back to the spawner when done (listener arming is *not* in
  this prompt — it comes from the `UserPromptSubmit` hook like every instance). Still
  `[mulpex:hub]`-sentinel-prefixed (skips the sidebar task-capture) and a single line (task
  whitespace collapsed). The child is **auto-named** `name_from_task(task)` so the sidebar labels
  it, and is **not** focused — the user stays on their pane while children appear. Recursion is
  inherent (children also have `hub_spawn`); only the per-call cap bounds a single call.
- **Injection is verified, not fire-and-forget** (`pty.rs`). Typing the task in requires the
  child's input box to actually exist, and nothing about the PTY stream says so directly. The
  injector therefore: (1) waits for a *drawn input box* — `input_box_ready` looks for the rounded
  box chrome in a rolling `TAIL_CAP` tail of output — rather than for "painted then quiet", which
  a mid-startup lull while MCP servers load imitates perfectly; (2) types the prompt, then sends
  `\r` separately (a `\r` at the tail of a fast burst is treated as paste content, not a submit);
  (3) **verifies** via `turn_started`, which reads the child's own `state_dir/<id>` status file —
  the `UserPromptSubmit` hook writes `working` there the moment a prompt submits, so this is
  positive proof the text landed; (4) retries up to `INJECT_ATTEMPTS`, clearing the box with
  Ctrl-U first so partial attempts can't concatenate.

  **Why it's built this way:** the original injector waited on a quiet-window heuristic with an
  8 s hard cap, then typed regardless. That survived the one-child case but failed *every* child
  of a six-way spawn — six concurrent cold starts all exceeded 8 s, so each typed into a TUI with
  no input box and the bytes were dropped, leaving six idle instances with no task and no way to
  recover (their hub listeners arm from the first turn, which never came, so even `hub_send`
  couldn't reach them). Readiness detection alone would not be enough — it can always be wrong on
  a future `claude` whose chrome differs — which is why verification plus retry is the part that
  actually makes this robust, and why the readiness check can afford to key off TUI chrome.

## Terminal sessions (⌘⇧T + `hub_terminal_*`)

A session is now either a `claude` or a **plain interactive shell** (`pty.rs::SessionKind`).
Terminals live in the *same* `Core.sessions` vec, the same per-project id space and the same
sidebar list (labelled `term #N`), so everything keyed by `(project, id)` — attach, `send_bytes`,
resize, close, the `OutputSink`, `TerminalManager`, `killpg` teardown — stayed kind-agnostic and
was not touched. What differs is what gets launched, what the hub knows about it (nothing), and
that its output is **recorded to disk**.

The user opens one with ⌘⇧T; an instance opens one with `hub_terminal_open` and can then drive it
(`_send`), read it (`_read`) and close it (`_close`). Any instance may drive **any** terminal in
its project, including one the user opened — inspecting the dev server the user started is a large
part of the point. `hub_instances` carries a `terminals` array so that costs no extra call.

- **Why not just the Bash tool:** Bash is request/response and cannot hold a process. A terminal
  is for the long-running and the interactive — dev server, watcher, `tail -f`, REPL, a build you
  want to glance at later, a command that asks questions partway through. `HUB_RULES` says exactly
  this, so instances don't open a terminal for `ls`.
- **`$SHELL -l -i`, not `-l`.** `-l` alone is login-but-*not*-interactive: zsh skips `.zshrc`,
  prints no prompt, and treats the PTY as a script. Both flags are required.
- **A terminal must not inherit a hub identity.** `portable_pty` passes the parent env through, so
  `shell_command()` `env_remove`s all three `MULPEX_*` vars: if Mulpex was launched from inside a
  Mulpex claude, a `claude` the user then typed into a terminal would write status files under a
  *terminal's* id and corrupt the hub. Same class of defence as the `CLAUDE_CODE_CHILD_SESSION`
  removal above. Terminals also don't go through `claude_command()`, which *errors* when `claude`
  is missing — a shell must not fail to open for that reason.
- **Hub isolation is by omission.** Terminals are excluded from `write_live_instances` (so
  `hub_send` never offers a shell as a peer), from `hub_snapshot`'s `statuses`, and from
  `persist_sessions`. Excluding them from `statuses` is what keeps `attention.ts`, the updater's
  busy guard and the tab badges correct *for free* — all three key off a session having a status
  entry. A synthetic `waiting` would have put every terminal into that math and painted a green
  "ready" dot on a shell forever.
- **Not persisted across restart**, deliberately: a restored terminal would be a fresh shell with
  no scrollback, and re-running a seeded command could be destructive. `persist_sessions` filters
  on `kind`, *not* on an empty `session_id` — and no column was added to the store file, because
  `persist.rs` parses it with `splitn(3, '\t')`.

### An exited terminal is kept; a dead instance is not

A shell that exits stays in the list marked `exited`, with its output still readable, until
someone closes it (⌘W / `hub_terminal_close`). Otherwise a command's output would vanish at the
moment the command finished — the exact thing "read it anytime" exists for.

That makes "dead" no longer mean "remove", which two things depend on:

- **`Core.closing`** marks a session as asked-to-go-away. `reap_dead` removes a dead session only
  if it's a Claude *or* it's in `closing`, so an explicit close and a self-exit still follow one
  single-sourced path.
- **`reap_dead`'s early-return tests removability, not liveness.** The old
  `if sessions.iter().all(is_alive) { return }` would be false *forever* once one terminal exited,
  and the whole body — two disk writes, including re-truncating the `instances` file the helper
  reads — would run on every 200 ms tick for the life of the app. Guarded by
  `a_kept_exited_terminal_does_not_make_every_poll_do_work`.
- **`hub.rs` diffs the session list** instead of emitting only on add/remove, which is how an
  alive→exited transition reaches the sidebar at all. That also closed a standing gap: a rename or
  a mute changed the backend and emitted nothing.
- **Never `wait()` the child to learn it exited.** Liveness is reader-thread EOF; leaving the
  child a zombie keeps its pid unrecyclable, which is what makes the `killpg` in `Drop` /
  `teardown_all` safe for a terminal that may sit in the list for a long time after dying.

### Killing a session: `killpg` is not enough once a shell is involved

`Session::kill` now sweeps **by controlling terminal** before the `killpg`, and that extra step
exists entirely because of terminals:

- A `claude` is a node process and does **no job control**, so everything its Bash tool spawns
  inherits its process group. One `killpg` reaches the lot — measured: descendant `pgid ==` session
  `pgid`, gone after close, no zombie (`child.wait()` reaps the direct child).
- An **interactive shell does** job control and puts every job in its own process group.
  A *foreground* job still dies, but from the hangup rather than the `killpg`: dropping the master
  SIGHUPs the tty's foreground group. A **backgrounded `cmd &` is in neither** — measured, it
  survived both ⌘W and app quit, which is a hole in the no-orphans guarantee.
- Giving the shell a grace period after SIGHUP so it could hup its own jobs **does not fix it**
  (measured at 150 ms and 400 ms; the job was still alive both times). Don't reach for that.

So the sweep SIGKILLs every process whose `e_tdev` matches this session's tty
(`proc_listpids` + `proc_pidinfo`, macOS-only, with a no-op fallback elsewhere). That set is
exactly this session's descendants: the device is ours for as long as the master is open, which is
why the sweep must run **inside `kill()` while `self.master` is still alive** — after the device is
released the kernel can hand the same number to an unrelated pty. `tty_dev` is latched by the
reader thread on first output (proof the child has claimed the tty; it cannot be read right after
`spawn_command`, which has only returned from the fork), so teardown still knows it once the child
is gone. Guarded by `closing_a_terminal_also_kills_its_background_jobs`, which was confirmed to
fail with the sweep disabled.

### The transcript (`vtgrid.rs` + `mulpex_core::termlog`)

The MCP helper is a *separate process*, so it cannot reach PTY bytes in memory — it reads a file.
Creating/driving a terminal is a `termreq/` file handshake through the poll loop (same shape as
`hub_spawn`); **reading is not**, which is what makes "read anytime" cheap enough to poll.

**Why a grid emulator and not a line-based ANSI stripper.** `cargo`, `vite`, `docker compose` and
every spinner repaint by moving the cursor and erasing. A filter that merely drops escape
sequences emits *every intermediate frame*, stacked. Measured on real captured PTY bytes
(`src-tauri/tests/fixtures/cargo-build.bin`, recorded through a real pty at 32×120): cargo's
progress bar repaints one line **17 times**; replayed through `vtgrid` the log is 8 lines — the 7
`Compiling` lines and `Finished`, exactly what a human read on screen. Frozen as
`vtgrid::replays`. So `Screen` is a bounded rows×cols grid the escape sequences actually act on,
and a row reaches the log only when it can no longer change.

- **A row is logged when it scrolls off the top** — or on a full clear (`ED 2/3`), which is what a
  real terminal's scrollback does too. Consequence: **what is still on screen is not in the log**,
  so a dev server sitting at a steady screen produces no history at all. That is why there is a
  second file (`<id>.screen`, temp+rename) and why `hub_terminal_read` returns both `new_output`
  (history since your cursor) and `current_screen` (re-sent every read, labelled as live).
- **Alt-screen (`?1049h`) is suppressed**, replaced by one `[full-screen program — output
  omitted]` line. A stray `vim`/`htop` would otherwise evict the whole 1 MB budget.
- **Partial UTF-8 *and* partial escape sequences carry across chunks.** The reader thread delivers
  arbitrary 8 KB slices; per-chunk `from_utf8_lossy` sprays U+FFFD at every boundary and a
  straddling CSI leaks `[0m` into "stripped" text. Both have tests.
- **A settle thread publishes the screen on a timer.** The reader thread only runs when there is
  output, so without it the last chunk of a burst — usually the prompt itself — would sit
  unpublished indefinitely.
- **The log trims in place on the one writer fd, header last.** A rename-based trim would unlink
  the inode the writer points at and every later byte would vanish into a deleted file, silently,
  forever. Writing the header last means a reader that sees `base` change across its own read
  knows the data moved and retries — without that check a trim landing mid-read returns *the wrong
  window of text*, which is a wrong answer, not an error. Both covered by tests.
- `terminals/index` (`<id>\t<running|exited>\t<label>`) is the manifest `hub_instances` reads; it
  is refreshed from the poll loop **on change**, because a shell can exit with nothing else
  happening and an index that only updated on add/remove would advertise a dead shell as running.

### Reading incrementally, and knowing when a command finished

- **Per-instance cursors** (`terminals/cursors/<id>.<instance>`, logical offsets) so a read returns
  only what's new to *that* reader — polling a build doesn't re-deliver everything.
- **The opener's cursor is seeded to 0**, so its first read is the terminal's whole life. A claude
  reading someone *else's* terminal for the first time gets the **tail** (200 lines) with
  `first_read: true` — never a 1 MB dump, never a confusing empty result.
- **`wait_ms` (≤30 s) blocks** until new output, completion, or exit. This is why
  `mcp.rs`'s transport became **concurrent** (thread per `tools/call`, only the stdout write
  serialized): Claude Code batches independent tool calls routinely, and a serial loop would park
  the whole batch behind one waiting read. Verified: a `hub_instances` issued while a 30 s read is
  parked comes back in well under a second. A timed-out wait with nothing new does **not** advance
  the cursor.
- **Completion is a sentinel, not a guess.** `hub_terminal_send` submits
  `<cmd>; printf '\n__MPX_DONE_<token>_%s__\n' "$?"` and records the token; a read reports
  `command_finished` + `exit_code` and strips both the marker line and the `; printf …` tail off
  the echoed command. `idle_ms` alone is a weak signal — a linking build is silent for a minute
  mid-run.
- **The sentinel is only appended when the terminal is at a prompt.** If a tracked command hasn't
  produced its marker yet, the text being sent is an *answer to that running command*, not a new
  command line, and `; printf …` would feed the program nonsense. That rule is automatic (no
  parameter) and is what makes `submit: true` safe for both cases. The rule lives in one pure
  function, `mcp.rs::mark_action` → `Track | Keep | Clear`, which is where the two ways it used to
  misfire were fixed (see **The six driving-session gaps** below).
- **Stripping the echoed tail is newline-tolerant, and has to be.** The shell echoes the command
  *including* the ~47-character `; printf …` tail, so any command longer than about half the
  terminal width wraps — and the grid turns that wrap into a real newline, landing anywhere inside
  the tail, `__MPX_DONE_` included. A per-line `find` misses it entirely on long commands and
  leaves half of it behind on medium ones. `strip_echoed_tails` therefore matches across newlines
  and swallows them with the span, which also rejoins the echoed command into the one line it was
  before wrapping. Guarded by a test that wraps at *every* position inside the tail. (Completion
  detection was never affected — the printed marker is ~25 chars and never wraps.)

### The six driving-session gaps — found 2026-08-03, all fixed 2026-08-04

The first time a claude actually drove a terminal for a whole task (instance #2, ~35 calls against
an interactive `ssh` session on a remote box) surfaced six gaps. All six are now fixed; each is kept
here because the *reason* is the part that's expensive to rediscover, and three of them are shapes
that will recur. Every one was **reproduced against the real `mulpex-helper`** before and after:
reverting `mark_action` to the pre-fix rule fails 9 of the harness's checks — including the exact
live symptoms, `PY; printf …` on the heredoc terminator and `exit_code: 0` reported while
`cd … && ls -la && cat config.yaml` was still running.

- **A stale `command_finished` / `exit_code` was reported for the wrong command.** `mark_path` was
  written only when `track` was true and **never cleared when it was false**, so after an untracked
  send the mark still named the *previous* command's token — which by then had completed — and the
  next read answered `command_finished: true, exit_code: 0` about a command still in flight. The 0
  belonged to a `docker ps` two sends earlier. The dangerous one: a *wrong answer*, not an error,
  and a model branches on it. Now `mark_action` returns `Clear` for any untracked send made while
  the terminal is at a prompt. A mark for a command that is still **running** is deliberately
  `Keep`t — that input is an answer to it, and its completion is still worth reporting.
  **Interrupts clear too**, which is not an extra: Ctrl-C kills the `; printf …` along with the
  command, so the marker never prints, and a dangling mark would leave the terminal reading as
  permanently not-idle — nothing sent afterwards could ever be tracked again. That wedge is its own
  harness check.
- **The sentinel broke any multi-line input; a heredoc hung the shell.** The marker is appended to
  the end of the *whole string*, so on multi-line input it lands on the last line — and for a
  heredoc that line is the terminator: `PY` became `PY; printf …`, which no longer terminates it,
  leaving the shell in `>` continuation until someone sent Ctrl-C. It was **nondeterministic**: the
  same claude's first heredoc went through untouched, purely because tracking happened to be off.
  Fixed as chosen (Gidi, 2026-08-03) by not tracking multi-line input at all — deterministic, and
  it cannot hang the shell. The cost is no exit code for heredocs, and the tool description says so.
  The two alternatives stay rejected: `{ … } ; printf …` hangs on unbalanced quotes/braces instead,
  and a marker on its own typed-ahead line is swallowed by anything that reads stdin (`cat`, a REPL).
  The same guard is on the **seeded** command in `hub_terminal_open` (`seed_and_mark`).
- **Long lines were hard-wrapped, so verbatim data came back mangled.** `vtgrid` had no soft-wrap
  tracking: a row overflowing `cols` became a real `\n` in both `screen_text` and the log, so real
  reads were full of `--no-lege\nnd` and JSON split mid-value — a token, URL or id straddling the
  boundary was silently corrupted, and grepping the log for it could never match. `Screen` now
  carries a per-row `wrapped` flag, set **only** by `print`'s auto-wrap, cleared whenever the row is
  re-written (CR), erased through the right margin, or replaced, and moved in lockstep with `cells`
  by every scroll / IL / DL. A wrapped row is serialized **untrimmed and unterminated**, so its
  continuation joins it — including across the log, where the continuation scrolls off in a later
  tick. Consequence by design: a log line is no longer bounded by the terminal width.
  Two non-obvious bits: an *exactly full* row followed by LF is **not** joined (deferred wrap — the
  flag is set when a further char actually arrives, which is also why the cargo/vite fixtures still
  replay byte-identically), and CR clearing the flag is the conservative choice — a line that is
  still long simply re-wraps as it reprints. The old test asserting `"abcd\nefgh"` was the bug,
  pinned; it now asserts `"abcdefgh"`.
  *(The old note here said this "touches the renderer the UI shares" — it does not. `vtgrid` is used
  only by `pty.rs`'s `Recorder`; the UI is xterm.js and never reads it. Blast radius is the
  transcript and `<id>.screen`.)*
- **`current_screen` was never marker-stripped.** `new_output` went through `strip_markers` while
  `<id>.screen` was read straight off disk and inserted raw, so every screen read carried the
  visible `; printf '\n__MPX_DONE_…' "$?"` plumbing. It mattered more than it looked: a claude
  driving a shell tends to prefix `clear;` (itself a workaround for `new_output` lagging the
  screen), which makes the screen the primary channel. Both now go through `strip_markers`.
- **`hub_terminal_open` mangled its seed command.** `.split_whitespace().join(" ")` collapsed all
  whitespace, silently rewriting a script before it ever ran. The command is now passed **verbatim**;
  only the *label* is flattened (`flatten_label`, capped at `LABEL_MAX_CHARS` with an ellipsis).
- **`wait_ms` was silently ignored when `full: true`**, and when tracking was off it quietly
  degraded from "wait for the command to finish" to "wait for any byte" — which is why nearly every
  wait in that session either timed out or came back early. Now: with a command in flight the wait
  is **for its completion**; with nothing tracked it is for output; `full` no longer disables it
  (what you read and how long you block are unrelated questions); and the reply carries
  **`waited_for: "completion" | "output"`** so a caller never has to guess which it got.
  Writing the test exposed a third bug in the same lines: the loop compared against the total *at
  call time*, so a read with unread output already in hand still blocked the whole window waiting
  for **more**. The condition is now "is there anything this reader hasn't seen", which is what the
  schema always claimed. A timed-out wait with nothing new still does not advance the cursor.

## macOS file access (TCC) — the failure with no symptom, and why signing is load-bearing

**Symptom, as the user experiences it: "Claude refuses to open."** A session appears in the sidebar
for about 100 ms, an error flashes in the pane, and both are gone. Every project is affected at
once, so the whole app looks broken. Nothing is logged anywhere.

**Cause.** macOS TCC protects `~/Documents`, `~/Desktop` and `~/Downloads` — where people keep
code. Mulpex spawns `claude` with `cwd` set to the project, and if the app has not been allowed
into that folder the child cannot even resolve its own directory:

```
job-working-directory: error retrieving current directory: getcwd: cannot access parent
directories: Operation not permitted
```

`claude` exits **1 in the same second**; the poll loop reaps the session; the row vanishes. The
denial is recorded **per bundle id and never asked about again** (`auth_value=0` in
`~/Library/Application Support/com.apple.TCC/TCC.db`), so it is permanent until reset.

Diagnosed 2026-08-05 by measurement, and the sequence is worth repeating because every cheaper
step was misleading: the same `claude` invocation ran fine from a terminal (that inherits the
terminal's *own* TCC grant); reproducing it needed the **launchd** environment a Finder launch
gets (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`, no `TERM`, no `LANG`). Ground truth came from a
transparent shim on `PATH` logging the real argv/env/exit code — all 8 spawns `rc=1`. The control
that proved it was the *folder* and not the app: the same build opened a project in `/private/tmp`
and `claude` stayed alive.

**Three defences, and the third is the one that matters most for other users:**

- **Preflight** (`pty::dir_access_error`, called from `Core::spawn_with`): ⌘T reads the project dir
  first and refuses with the folder name plus the Settings path. Deliberately **not** applied to
  restores — refusing there would reopen the project to an empty sidebar, which is the original
  bug. A restore instead spawns, fails, and becomes a visible failed row.
- **A session that dies within `EARLY_DEATH_GRACE` (10 s) is kept**, not reaped — see below.
- **Usage strings** (`src-tauri/Info.plist`, auto-merged by the bundler because it sits next to
  `tauri.conf.json`). Without `NSDocumentsFolderUsageDescription` the prompt appears with no reason
  attached, which makes "Don't Allow" the natural click.

**Signing is what makes an Allow stick, and every release before v0.6.0 got this wrong.**
`tauri build` does *not* sign the bundle: the output has no `_CodeSignature`, only the
linker-signed ad-hoc signature Rust puts on every arm64 binary (`flags=0x20002(adhoc,
linker-signed)`, `Sealed Resources=none`), and a **random per-build identifier**
(`mulpex-55de470ef4e5b764`). Verified against the *published* v0.5.0 tarball, not just a local
build. macOS will not persist a TCC grant for a bundle whose signature does not validate, and a
changing identifier makes every update look like a brand-new app — so users were re-asked on every
release until someone clicked Don't Allow, at which point they hit the permanent failure above.
Fixed with `bundle.macOS.signingIdentity: "-"`, which ad-hoc signs the `.app` **before** the
`.tar.gz` and `.dmg` are built from it; the identifier then falls back to the stable
`com.mulpex.app` and `codesign --verify` passes. Don't remove it, and don't assume a green build
means a signed one — `release.sh` only checks the artifacts *exist*.

## A session that failed to start is kept, not reaped

The general form of the bug above: any instance that dies before it was ever usable used to
disappear silently. `reap_dead` now keeps a dead instance that died within `EARLY_DEATH_GRACE`
(10 s), marks it in `Core.failed` (id → reason), and writes the reason into its own pane via
`Session::notice` — under whatever the child managed to print, so `claude`'s own last words survive
above it. The row reads `⚠ … failed to start` and stays until ⌘W, exactly like an exited terminal.

- **The reason is re-derived at death**, so the TCC case reports the actionable folder message
  rather than a bare exit; otherwise it says only what is known.
- **A failed instance leaves the hub**: its status file is deleted and it is dropped from
  `live_instances()`, so `hub_send` can never offer a corpse as a peer. That also keeps it out of
  `statuses`, which is what excludes it from the dock badge, the tab badges and the updater's busy
  guard **for free** — the same exclusion a terminal gets.
- **`reap_dead`'s early-return had to change too.** It tests *removability*, and a kept-failed
  instance is not removable — so testing that alone returned before anything was marked, and the
  row then sat dead and unexplained until the grace lapsed and it was silently reaped. The original
  bug, delayed by ten seconds. The guard now tests "is there anything to do"
  (`is_removable || needs_failure_mark`), and the mark **latches** so both go false again
  afterwards; without that latch every 200 ms tick redoes the body's two disk writes. Guarded by
  `a_kept_failed_instance_does_not_make_every_poll_do_work`, confirmed to fail (on exactly that
  mtime assertion) with the latch removed.
- The ordinary case is untouched: an instance that ran and then exited is still removed
  (`an_instance_that_dies_after_the_grace_is_still_reaped`).

## A failed restore must not erase the session record

`reap_dead` used to rewrite the store without any session it removed. That is right for a session
the user closed, and **catastrophic** for one that died because its restore failed: `claude
--resume <id>` prints `No conversation found with session ID: …` and exits in about **1.6 s**
(measured), the poll loop reaps it, the store is rewritten without the id — and now there is
nothing left to retry with and nothing to recover by hand. One bad restore turned into permanent
loss of the session.

So `Core` tracks `restored` (id → when it started) and, when a restored session dies inside
`RESTORE_GRACE` (120 s) without having been explicitly closed, keeps its `SavedSession` in
`sticky`, which `persist_sessions` merges back in. A restore that fails once may well succeed next
launch; if it never does, the user still has the id. Guarded by
`a_failed_restore_is_kept_visible_and_never_erases_the_record`, confirmed to fail with the `sticky` push
disabled.

**The failure mode this protects against, and how to recognise it:** a `claude` that inherits
`CLAUDE_CODE_CHILD_SESSION` runs with **transcript saving off**. It behaves completely normally —
it just writes no transcript — so the breakage only shows up at the *next* launch, as an instance
that appears for a second, prints the red `No conversation found` line, and vanishes. That is why
`pty.rs` strips the marker (and `CLAUDE_CODE_ENTRYPOINT`), and why
`the_child_session_marker_is_stripped_from_spawned_sessions` pins it: if `env_remove` ever
silently stopped reaching the child, *every* session would become unrestorable with nothing in any
log to say why.

Verified along the way, all against the real CLI: `--session-id` is honoured in interactive mode
(not just `-p`); `--resume` appends to the same transcript rather than forking a new id; a 165 MB
transcript resumes fine, through Mulpex's full invocation; and quitting preserves the store.

**Note the tests take `env_guard()`** — `HOME` is process-global and the session store path is
derived from it, so the tests that repoint it must take turns or they race.

## Remote claude peers (`hub_remote_open`)

A local instance can start a `claude` **on another machine** over ssh, inside an ordinary Mulpex
terminal, hand it a task, and be *woken* when it has something to say. The remote knows nothing
about Mulpex, has no instance id, no inbox and no hub tools — it is a plain `claude` on a plain
terminal — and that asymmetry is the whole design problem.

Driving it needed nothing new: `hub_terminal_send`/`_read` already type into a terminal and read it
back, and a remote claude experiences that as a human typing. What did not exist was the **other
direction**. The remote can only print; nothing told the driver to go and look; and there was no way
to distinguish "still thinking" from "finished and waiting". So the feature is exactly one thing —
a convention the remote follows and the poll loop watches for, which turns a line of its output into
a message in the **opener's inbox**. That inbox is the directory the driver's hub-listener Monitor
already polls, so **no new wake path was built**: a remote claude wakes an idle local instance
through the same machinery a peer's `hub_send` does.

- **The launch is the only moment the rules can be attached.** They ride in on
  `--append-system-prompt`, which is re-sent with every request and therefore survives both a long
  conversation and compaction. There is deliberately **no way to adopt a remote claude started by
  hand** (⌘⇧T, ssh, type `claude`): rules delivered as a typed message drift out of context, which
  is the failure this design exists to avoid. A hand-started remote is just a terminal, as before.
- **It stays a terminal.** No hub identity, no sidebar treatment, nothing in `hub_instances`' instance
  list — the only trace is `terminals/remote/<id>.json` holding its token, target and opener. This
  keeps the standing invariant that a terminal is never a hub peer, which the badge counts, the
  updater's busy guard and `attention.ts` all lean on. The wake message says so twice over, because
  a *hub message* invites a `hub_send` reply and that would be addressed to a shell.

### Three ways to start one

`hub_remote_open` opens its own terminal by default, and takes an optional `terminal_id` to use one
that already exists. What matters in every case is that the rules are attached **at launch**, on the
command line — where the terminal came from is irrelevant to the mechanism:

- **No `terminal_id`** — Mulpex opens a terminal and runs the whole `ssh … claude …` in it.
- **`terminal_id` + `ssh_target`** — the same launch, but in a terminal that already exists (e.g. one
  the user opened and left idle).
- **`terminal_id`, no `ssh_target`** — the terminal is *already logged in to the far machine*, so only
  the `claude` half is launched, on the far side. This is the one that makes a password login, a
  jump host or a VPN workable: the human does the connecting, the instance does the rest. The wake
  message then has no target to name, so `wake_body` drops that clause rather than printing a gap.

**Adopting an already-running remote claude is still not supported, and that is a different thing.**
Rules typed in as a message drift out of context; rules on the command line do not. Only the launch
is being moved, never the delivery mechanism.

`launch_into_existing` refuses a terminal it cannot safely type into, because a launch command sent
to a running program is *input to that program*, not a command line — the same class of mistake as
appending `; printf …` to a heredoc terminator. It refuses on three distinct grounds, each with its
own message: the shell exited, a Claude TUI is already on screen, or it is not free.

**"Free" is deliberately two-sided, and the first version was wrong.** It required output to have
stopped *and* the last line to look like a prompt — and prompt themes are endless. Run live, the box
answered with `➜  ~`, oh-my-zsh's default, which ends with the **path** rather than a sigil: the tool
would have permanently refused a perfectly idle terminal on the most common zsh theme there is. So
`at_shell_prompt` now matches a leading sigil as well as a trailing one, and — more importantly —
an unrecognised prompt is no longer fatal: after `UNRECOGNISED_GRACE_MS` of silence the terminal is
treated as free regardless of how its prompt looks.

### The marker, and why it looks like that

Every one of these was measured against a real remote over ssh (fixtures
`src-tauri/tests/fixtures/remote-claude-*.bin`, pinned by `vtgrid::remote_claude_replays`):

- **`<<<MPX <token> <kind> <summary>>>>`, because the delimiters cannot be markdown.** The first
  design used `__MPX_TO_LOCAL__`. Claude Code renders its output as markdown, `__x__` is *bold*, and
  the underscores were eaten by the renderer before the bytes reached the terminal — what arrived was
  a bare `MPX_TO_LOCAL`, and a grep for the marker found **zero** occurrences. Designed by reasoning,
  the wake path would have been dead on arrival and looked like "the remote ignores instructions".
  Angle-bracket runs survive verbatim, confirmed twice through the real recorder.
- **The token is per-terminal and secret**, because the transcript contains the *driver's own typed
  input*, echoed back by the remote TUI. Without it, a local instance that merely quoted the marker
  would wake itself. It never appears in plaintext on the command line either — the rules go over
  base64-encoded, which is also what keeps two levels of shell quoting from corrupting them.
- **Parsing is newline-tolerant, and has to be.** The TUI hard-wraps at the terminal width and the
  grid can turn that into a real newline anywhere, including mid-token. A wrap is genuinely ambiguous
  (the newline may replace a trimmed space, or may cut a word), so `parse_body` tries **both**
  readings and takes whichever yields a valid signal. Guarded by a test that wraps at every position.
- **Detection runs on the rendered grid, never on raw bytes.** The TUI writes words with cursor jumps
  between them, so `bypass permissions` is plainly visible on screen while a byte search for it
  returns 0 hits.
- **Both the log and the screen are scanned.** A row reaches the log only when it scrolls off the
  top, so a remote that answers briefly and sits there has its marker on screen and *nowhere else*.

### A remote claude is SCREEN-ONLY, and that is not fixable here

Newer Claude Code (v2.1.226 on a real box; v2.1.223 did not) draws on the **alternate screen**, and
it repaints by **absolute cursor positioning** — measured on a real capture
(`remote-claude-altscreen.bin`): `?1049h`, 22 CUP sequences, 11 erase-lines and **zero newlines** in
a 3 KB startup. Two consequences follow, both load-bearing:

- **The recorder must keep emulating while suppressed.** `Screen::suppressed` suppresses *logging*,
  never emulation. It used to drop every byte, which was fine for a stray `vim` and fatal here: a
  remote claude's terminal went completely dark, `<id>.screen` was **0 bytes**, and the driving
  instance could read *nothing at all*. Guarded by `a_real_alt_screen_remote_claude_stays_readable`,
  confirmed to fail ("the driver would be blind") with the old early-return restored.
- **Its history cannot be recovered by any amount of logging.** Nothing ever scrolls, so no row ever
  passes through `scroll_up` — the text above the viewport lives in *claude's own* buffer and is
  redrawn only when someone scrolls it. `new_output` is therefore empty by design and
  `current_screen` is the whole channel. Don't "fix" this by logging during alt screen: there is
  nothing there to log, and a repainting TUI would evict the retained history.

So the constraint is *reported* rather than papered over. `hub_terminal_read` sets **`screen_only`**
on a remote and explains it, the remote's rules cap replies at about a screen and tell it to
re-print (not re-investigate) on request, and the driver's rules say to ask for screen-sized chunks.
This came from the field: a remote answered in six sections, the driver received 4–6, and 1–3 had
scrolled into a buffer it could never reach. Nothing errored; the text simply was not there.

Worth noting how long the trigger stayed hidden: `?1049h` did **not** reproduce on a local claude of
the same version, nor with a `statusLine` configured, nor in a fresh directory — that last one only
because the probe never got past the trust prompt. It appears at 1.7 s in an already-trusted project
over ssh. Three hypotheses were falsified before the reproduction; treat "it renders inline" as a
fact about a specific recording, never as a property of Claude Code.

### Two triggers, because a model can forget

The marker is an instruction to an LLM, and instructions get skipped. `--append-system-prompt` means
it is re-sent every turn rather than remembered — it cannot decay — but re-sending is not obeying,
and the failure mode is the bad kind: the driver waits forever and nothing looks broken. So there is
a second, mechanical trigger:

- **The signal** carries *why* (`done` / `blocked` / `question`) plus a one-line summary.
- **Silence** — no output for `IDLE_TURN_END_MS` (1.5 s) — synthesises `Kind::Ended`. This is
  reliable because a working `claude` **animates its spinner continuously**, so output genuinely
  stops only between turns. Keyed on silence rather than on the spinner *word*: the vocabulary is
  randomised (`Lollygagging`, `Cooked`, `Brewed` all appeared in one short capture) and matching it
  would rot on the next Claude Code release.

**`Core.remote_awaiting` is what makes the backstop meaningful, and it is not an optimisation.** A
remote sitting at a fresh prompt, never asked anything, is *also* silent — so a backstop keyed on
silence alone fires the moment the TUI finishes drawing. Measured: the first live run woke the driver
**5.7 s after launch, before the task had even been typed**, and the test passed anyway because it
only asserted that *a* wake arrived. An id is armed when input is sent to it and disarmed when a wake
is delivered, so silence counts only while an answer is owed. Guarded by
`silence_is_only_a_wake_when_an_answer_is_owed`, confirmed to fail with the guard removed.

### Injection: the `\r` must be its own write

`mcp::inject_task` types the task, pauses, then sends `\r` **separately**, verifies a turn actually
started (the spinner, or an already-emitted signal), and retries up to `INJECT_ATTEMPTS` with a
Ctrl-U clear first. This is the same rule `pty.rs` documents for locally spawned instances, and it
was re-discovered here the hard way: sending `task + "\r"` in one write left the task **fully typed
in the input box and never submitted**, so the driver waited on a remote that had never read it. The
symptom is invisible unless you look at the screen — the bytes all arrived.

### Root, and what it costs

Claude Code refuses `--dangerously-skip-permissions` outright when running as root ("cannot be used
with root/sudo privileges for security reasons"), and remote boxes are commonly entered as root. The
launch therefore exports **`IS_SANDBOX=1`**, which is a deliberate bypass of a check Claude Code put
there on purpose. The justification is that a remote peer runs unattended and answers to another
model, so it must not stop at a permission prompt no human will ever see — but the consequence is
real and worth stating plainly: **a remote claude runs unsupervised, with permissions skipped, doing
whatever the driving instance asks of it.**

## Shared-tree guardrail

All instances **of a given project** share one working directory and one git checkout (not a
worktree per instance; separate projects are separate trees), so
a tree-wide git op by one (`reset --hard`, `checkout .`/`restore .`, `clean`, `stash`, branch
switch, `rebase`/`revert`) or any bulk-destructive command wipes every other instance's
uncommitted work. `HUB_RULES` (the append-system-prompt) tells each instance to treat these as
dangerous: check `hub_instances` first, coordinate via `hub_send` or ask the user if any peer is
live, and ask the user even when alone. This is **prompt-level guidance only** — no PreToolUse
hard block (that was considered; command-detection is heuristic and shell-bypassable).

## Verified so far

- Both crates + the Tauri app compile clean; `svelte-check` + `vite build` clean.
- The whole coordination hub works **end-to-end through `mulpex-helper`**: MCP `initialize` /
  `tools/list` (all 6 `hub_*` tools), `hub_send`→`hub_inbox` delivery + `messages.log`,
  `userpromptsubmit` task capture + peer snapshot, and `pretooluse` `O_EXCL` lock acquisition
  with a canonical-path + heartbeat token.
- **Run as a GUI and bundled.** `npm run tauri dev` and `tauri build` (→ `Mulpex.app` with the
  `mulpex-helper` sidecar inside `Contents/MacOS/`) both work; multi-instance spawn,
  focus-switch, resize, and session `--resume` verified in the window. *(This entry used to say
  "signed `Mulpex.app`". It was not: through v0.5.0 the bundler emitted no `_CodeSignature` at
  all — confirmed against the published tarball, not just a local build — and that unchecked
  claim is part of why the TCC breakage above went unexplained for so long. Signing is only real
  from v0.6.0, via `signingIdentity`.)*
- **Idle-wake hub listener verified live.** A `hub_send` from one instance woke the idle peer via
  its Monitor event (no injected prompt line), which read its inbox and replied — round-trip
  confirmed both directions, with the `⟳` marker and a clean sidebar (the sentinel +
  `<task-notification>` task-capture skips work).
- **Clean-start arming verified live.** A normal instance opened to an empty prompt (no bootstrap
  turn) and armed its Monitor invisibly from the `UserPromptSubmit` hook on the user's first
  message; the `armed/<id>` flag stopped the reminder on later turns.
- **`hub_spawn` verified live.** An instance spawned a task-seeded sibling that appeared in the
  sidebar auto-named, armed its own listener on its injected first turn, ran its task, and
  `hub_send`'d its result back to the spawner.
- **Multi-project (v0.4.0).** `cargo build`/`clippy` + `svelte-check` + `vite build` clean; the
  app boots to the picker with no open projects (fresh `open.txt`) and no panic, with the
  drag-drop listener registered. Released as `v0.4.0` with a signed `Mulpex_0.4.0_aarch64.dmg`.
  The interactive flows (tab/⌘P switching, per-project hub isolation, restore-all, dedup,
  no-orphans) are covered by the plan's verification checklist and want a live GUI to exercise.
- **Finder-launch fix (v0.4.1).** The bundled `.app` was exercised under a Finder-equivalent
  environment (`env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`): `claude` resolves out of
  `~/.local/bin` via `claude_bin::merged_path()`, renders in color (`TERM=xterm-256color`), and
  the picker shows the not-found banner when the CLI is genuinely unreachable. Released as
  `v0.4.1`.
- **Shift+Enter (v0.4.3).** *v0.4.2 changed the byte and did not fix it* — the byte was never the
  problem. Measured, not guessed, two ways: (1) driving the real `claude` on a Python PTY with
  `pyte` rendering the prompt box shows `\n`, `\x1b\r`, `\x1b[13;2u` and `\x1b\n` **all** insert a
  newline, so the original `\n` was fine; (2) driving the real `@xterm/xterm` build under jsdom
  and replaying the browser's keydown→keypress contract reproduces the bug exactly — the old
  handler emits `[27,13]` *and then* a stray `[13]`, the new one emits only `[27,13]`. The stray
  `\r` was the submit. Also worth knowing: Claude requests the **kitty keyboard protocol** and
  **modifyOtherKeys** at startup (visible in the PTY capture); xterm.js answers neither, so it
  can never disambiguate Shift+Enter on its own — the manual carve-out is mandatory.
- **Dropped paths (v0.4.5).** `shellQuote` was verified by round-tripping 17 adversarial paths
  (spaces, quotes, backticks, `$HOME`, `;rm -rf`, globs, unicode, backslashes, tab/CR/LF/ESC/BEL/
  DEL, and a mixed quote+newline+backslash case) through **real bash**: each yields exactly one
  argument equal to the original, a 17-path multi-drop splits back into 17, and no emitted text
  contains CR or LF (the invariant that stops a filename from submitting the prompt). Then
  confirmed live in the reinstalled `.app` by the user. `svelte-check` + `vite build` clean.
- **Bracketed-paste drops (v0.4.6).** The `[Image #N]` mechanism was found by measurement, not
  docs: driving a real `claude` over a PTY (`pyte`, typing only, never submitting) shows a *typed*
  image path stays plain text while the *same* path inside `ESC[200~ … ESC[201~` becomes
  `[Image #1]`. The app's exact `dropPaths` bytes were then replayed against a fresh `claude` per
  case — image, image-with-space, two images, csv-with-spaces, image+csv — all correct. The
  trailing-space-outside-the-markers bug was caught by this matrix and *only* by its non-image
  rows. 19 adversarial paths re-checked through real bash (one argument each, unicode bare, no
  CR/LF). `svelte-check` + `vite build` clean.
- **RTL / Hebrew.** Both halves measured rather than eyeballed (see **RTL** above), then
  confirmed **live in the installed `.app`**: a session started with ⌘T and Hebrew typed into it
  renders words *and* letters in reading order, including a mixed Hebrew/English line. The
  intermediate state is the instructive one — after only the WebGL removal the letters were right
  and the words still ran left-to-right, and *that* was invisible to screenshot-reading; it took
  per-character `getBoundingClientRect` to see which end was which.
- **⌘1–9 → projects.** Verified live by driving the installed app with System Events: ⌘3 selects
  the third tab (`dream-email`), matching `ProjectTabBar` order. Menu accelerators only exist in
  the built app, so this cannot be checked from `vite build` alone.
- **Tab counts / hub panel / sidebar split.** Verified by screenshotting the installed app: tabs
  show the session count (`0` with no sessions, no badges), the hub panel shows only `MESSAGES`
  when quiet, and the sidebar divider sits at 72%. The red `needs` and amber unread badges are
  **build-verified only** — producing them needs a background project actually asking a question.
- **Quit paths / scratch-root leak.** All four measured before and after against a real bundle on
  an isolated `HOME` (`scratchpad/measure-quit.sh`); see the teardown section for the table. The
  startup sweep collected the 12 dirs the bug had already accumulated.
- **Auto-update, end to end, against a local endpoint.** Two real bundles (0.4.6 and 0.4.7) built
  with the endpoint repointed at `127.0.0.1:8787`, the 0.4.7 update artifacts served by
  `python3 -m http.server`, and the 0.4.6 app run against them: banner appeared with the right
  version and notes, the click downloaded + signature-verified + swapped the bundle
  (**0.4.6 → 0.4.7 on disk**), the old process exited, a new one came up, and **the old scratch
  root was removed** — teardown ran on the update restart. No GitHub release was involved. Run
  **twice**: once on `AppHandle::restart` (which passed, showing the command body does not land on
  the main thread today) and again on `request_restart`, which is what ships.
  Three harness notes for next time: the `dangerousInsecureTransportProtocol` flag is required for
  an `http://` test endpoint (test builds only); **the buttons are unreachable from System
  Events** — AX can't enumerate into the WKWebView and `click at` fails with -25204, so the click
  has to be a real `CGEvent` post (`scratchpad/click.swift`) at coordinates read off a screenshot;
  and **gate that click on the test app actually being frontmost, by pid**. A CGEvent goes to
  whatever is under the point, so an un-gated click lands in whatever app is in front (here, the
  user's terminal) — and `tell application "Mulpex" to activate` resolves *by name*, so it can
  raise or launch the installed `/Applications` bundle instead of the one under test.
- **The release pipeline** was proven by `scripts/release.sh --dry-run`: signed `.tar.gz` + `.sig`
  and a well-formed `latest.json`, without publishing. That dry run is what caught the
  `TAURI_SIGNING_PRIVATE_KEY` naming trap.
- **Auto-update confirmed in production, by the user**, on the real GitHub endpoint: an installed
  v0.4.7 offered v0.4.8, applied it, and relaunched — no Gatekeeper prompt and no second `xattr`,
  which is the claim that mattered. Each published release is additionally checked by re-fetching
  the tarball GitHub actually serves and comparing its SHA-256 to the signed local one, so a
  mismatched or truncated asset can't sit there verifying against nothing.
- **Mute (⌘M).** Exercised against a real running app on an isolated `HOME` with three restored
  sessions. Menu shape read out of the **accessibility API** rather than a screenshot (`Mute
  Session` key=M mods=0, `Messages` key=M mods=1, `Minimize` key=missing) — AX reaches the menu bar
  even though it can't enter the WKWebView. Then: ⌘M dims/sinks/marks and ⌘M again restores the row
  to its original slot with dot and status back and the tick cleared; ⌘] from #3 lands on the muted
  #2 at the bottom rather than wrapping to #1, which is the case where visible order and creation
  order actually differ; the store file writes the awkward `<uuid>\t\tmuted` row and the mute comes
  back after a restart. **Badge exclusion was measured by forging the hook state files** — writing
  `needs` into `state_dir/<id>` and dropping files into `inbox/<id>/` is exactly what the hook and
  a peer's `hub_send` do, so the UI sees genuine input: with two sessions in `needs` and 3 unread,
  the tab read red **1** / amber **1** and the strip `1 unread`. Plus 13 assertions on
  `displayOrder`/`needsCount`/`unreadCount` driven through the real `stores.ts`, and 3 `persist.rs`
  tests including both pre-mute file formats. **Not verified:** clicking the row's 🔇 without
  selecting it first — the keyboard path proves the handler, only the hit target is untested.
- **The launch-time check** was verified separately, because "no banner at startup" had two very
  different possible causes (silent failure vs. still in flight). Running the *published* 0.4.7
  against the real endpoint under an isolated `HOME` with no projects, screenshotting at 6/15/30/50 s
  and never touching the menu: banner present at 6 s. That ruled out the check and pointed at the
  `ready` gate above.
- **Attention / tab drag / spawn injection (unreleased, post-v0.5.0).** `svelte-check` (120 files,
  0 errors) and `cargo check` are clean at `6a6b653`. Whatever live exercise these got happened
  before this doc entry existed — anything not recorded here should be treated as unconfirmed and
  re-driven in the real app: the dock badge needs a claude actually in `needs`, a banner needs the
  window unfocused *and* macOS notification permission granted for the bundle, and the injection
  fix is only meaningfully tested by a multi-child `hub_spawn` (the one-child case passed under
  the old code too — that's exactly why the bug shipped).
- **Terminal sessions (shipped in v0.6.0).** Everything below the GUI is measured; the headline GUI
  flows were driven too — see the two entries at the end.
  - **The grid, on real captured PTY bytes.** `cargo build` and `vite build` were recorded through
    a real pty at 32×120 (`TERM=xterm-256color`, the same shape a Mulpex terminal gives its child)
    and committed as fixtures. Replayed through `Screen`, cargo's **17** progress-bar repaints
    collapse to nothing and the log is the 8 lines a human read; vite's column-aligned size table
    survives byte-for-byte. This is the case a line-based stripper fails, which is why it is the
    one that got measured. Plus 16 unit cases: CR rewrite, `CUU`+`EL` repaint, `ED 2` preserving
    history, OSC 0 titles dropped, OSC 8 link *text* kept, alt-screen suppressed, UTF-8 and CSI
    split across chunk boundaries, wrap/scroll, IL/DL, resize.
  - **The log file:** header round-trip (shared `termlog`, so both sides parse the same bytes),
    and a 2 MB write proving `base` advances, the cut lands on a line boundary, the oldest lines
    go, and **content written after a trim is still there** — the assertion that would catch a
    rename-based trim orphaning the writer's fd.
  - **Real shell → recorder → file**, with nothing simulated: a `$SHELL` on a real PTY running
    `seq 1 200` puts the scrolled-off lines in the log, the last screenful in `<id>.screen`, and
    no escape byte in either.
  - **Lifecycle**, against real shells: a self-exited terminal is kept and marked, is absent from
    `instances` and from the store, shows `exited` in the manifest, and only an explicit close
    removes it (and its log + cursors); an idle exited terminal does not make `reap_dead` rewrite
    the session store on every tick; the `termreq` handshake opens, delivers real keystrokes to
    the shell, refuses a bad id, and closes.
  - **Teardown leaves nothing**, measured two ways — a PTY harness replicating the spawn shape,
    and two tests through the real code. A same-process-group descendant (the `claude` shape) dies
    with the `killpg` and leaves no zombie; a shell's **backgrounded** job, which `killpg` cannot
    reach, dies with the tty sweep. That second test was confirmed to **fail** with the sweep
    disabled, so it is a real regression test and not a tautology.
  - **The MCP surface driven end to end against the real `mulpex-helper` binary** over stdio, with
    a stand-in poll loop: 10 tools listed; the opener sees its terminal's whole life while a
    different instance's first read is a flagged, capped tail; second reads return only what's new;
    the `printf` plumbing never reaches the model; completion + exit code reported; a send while a
    command is still running is *not* wrapped; Ctrl-C accepted and an unknown control key is a tool
    error rather than silence; a timed-out wait loses nothing. And the reason the transport went
    concurrent: **a `hub_instances` issued while a 30 s blocking read is parked comes back in
    < 1 s.** (`scratchpad/drive_mcp.py`.)
  - **The six gap fixes, through that same real binary** (2026-08-04, 58 checks, all pass): the
    stale mark retired so no completion is claimed for an untracked send; a heredoc reaching the
    shell with its terminator intact and untracked; Ctrl-C clearing the mark so the *next* command
    can be tracked again; a screen read with no `printf`/marker in it; a seeded
    `awk '{ print $1,   $3 }'` arriving with its spacing intact and a multi-line seed keeping its
    layout; and `wait_ms` sitting through partial output until the marker lands, reporting
    `waited_for`, and running for `full` reads too. **Confirmed non-tautological**: reverting
    `mark_action` to the pre-fix rule fails 9 of them, reproducing the original live symptoms
    verbatim.
  - `cargo test` (55: 33 app + 22 core) and `cargo clippy` clean — the only two clippy warnings are
    pre-existing (`hook.rs` needless-return, `persist.rs` items-after-test-module). No frontend
    change, so `svelte-check`/`vite build` were not re-run for the gap fixes.
  - **Driven in the real window (2026-08-05, v0.6.0 build).** ⌘⇧T opens a shell: the row renders
    `$ term #N` with the `running` readout, the pane shows a real interactive prompt (zsh rc files
    loaded, git branch in the prompt — proof of `-l -i`), and a typed `echo … && exit` leaves the
    row **`exited` and still readable**, which is the whole point of keeping it. The app was
    rebuilt and installed, so the gap fixes now run live too rather than only through the helper
    binary.
  - **Still not verified:** an instance-opened terminal appearing without stealing focus, stdin
    going dead on an exited row, and RTL/colour inside a shell pane.
- **Remote claude peers (`hub_remote_open`).** Proven end to end **against a real VM**
  (`state::remote_peer_live`, `#[ignore]`d, run with `MULPEX_TEST_SSH=…`): a local instance's
  terminal ssh'd in, a remote `claude` started under the base64'd peer rules, ran the task it was
  given, emitted `<<<MPX <token> done …>>>`, and a hub message landed in the driver's inbox reading
  "has FINISHED the work you gave it. It says: cwd is /tmp/mpx-probe" — with the token stripped from
  everything a model can read. That test **failed twice before it passed**, and both failures were
  real bugs, not harness noise: the backstop firing before the task was typed, and the `\r` being
  swallowed as paste content so the task sat unsubmitted in the input box.
  The **already-connected** flow is proven live too
  (`a_claude_launched_into_an_already_connected_terminal_signals_home`): a terminal ssh'd in by hand,
  `claude` launched into it on the far side with no `ssh_target` at all, task delivered, and the wake
  read "has FINISHED the work you gave it. It says: echo attached ran; output: attached". That test
  also failed first — on `➜  ~` — which is how the prompt-detection defect above was found.
  Offline: 16 `remote.rs` unit tests (marker grammar, wrap-at-every-position, foreign/missing token,
  strip, base64 vectors, and one asserting the rules' own example parses — the two halves of the
  contract cannot drift); 2 watcher tests against real shells standing in for a remote, one of them
  confirmed to fail with the `remote_awaiting` guard removed; 2 `vtgrid` replays of the real captures
  pinning "no alternate screen" and the markdown-eats-underscores measurement.
  plus 4 `mcp.rs` tests for the refusals and the read integration.
  `cargo test` 88 (45 app + 43 core) green, `clippy` clean but for the two pre-existing warnings. No
  frontend change, so `svelte-check`/`vite build` were not re-run. **Not verified:** the flow inside
  the real GUI — an instance calling the tool itself and being woken while idle. The wake rides the
  existing Monitor path, which is separately proven, but that specific end-to-end has not been driven.
- **Session drag-to-reorder (shipped in v0.6.0).** The order math was driven through the **real
  `stores.ts`** (transpiled, not re-implemented) — 27 assertions on `clampToGroup` / `dragOrder` /
  `displayOrder` / the `reorderSessions` mutator, including the invariant that every emitted order
  survives re-sorting by `displayOrder` unchanged, both clamp directions, and the never-drop
  contract. Backend: `reordering_sessions_keeps_focus_and_never_drops_one` against real sessions.
  `cargo test` (36: 34 pass, 2 pre-existing ignored) + `clippy` clean, `svelte-check` 120 files
  0 errors, `vite build` clean. **Not verified:** the gesture itself — the pointer/threshold/
  indicator behavior and the clamp *as felt*. The v0.6.0 build is installed, so this can now simply
  be driven; it just hasn't been.
- **TCC / failed-start visibility (v0.6.0).** The diagnosis is in **macOS file access** above, with
  the shim-captured `rc=1` and the `/private/tmp` control. The fixes were then driven in the real
  window: a failed restore renders `⚠ claude #1 — failed to start` with `claude`'s own
  `No conversation found with session ID: …` still on screen *above* Mulpex's explanation, and the
  row is **kept** (the tab counts it) instead of vanishing; ⌘T on a `chmod 000` project refuses
  before spawning, with the folder name and the Settings path in the notice. Backend: three tests,
  two of them confirmed non-tautological by breaking the code — un-latching the failure mark fails
  `a_kept_failed_instance_does_not_make_every_poll_do_work` on exactly its mtime assertion, which
  is the "does work every 200 ms tick" regression this file already warns about. `dir_access_error`
  is tested against a real `chmod 000` directory (only root is excused, so it cannot pass
  vacuously).
- **The v0.6.0 release artifact itself.** Signing was verified in the *published* tarball, not just
  locally: re-fetched from GitHub, its SHA-256 matches the signed local build byte-for-byte, and
  the `.app` inside reports `Identifier=com.mulpex.app`, `Sealed Resources version=2`, and passes
  `codesign --verify --deep --strict` — where the published v0.5.0 fails all three. The same
  artifact was installed and launched before publishing: it runs under the hardened runtime the
  bundler adds (`flags=0x10002(adhoc,runtime)`), spawns `claude`, restores projects, and **kept its
  TCC grant** across the swap.

## Auto-update

`tauri-plugin-updater` against the repo's GitHub releases. Checked at launch and every **6 h**
(`updater.ts::CHECK_INTERVAL_MS`), plus **Mulpex ▸ Check for Updates…** on demand; an available
version raises a fixed card (`UpdateBanner.svelte`) with **Update & Restart**.

- **The `.dmg` is not the update channel.** The updater consumes `Mulpex.app.tar.gz` + `.sig`
  (emitted by `bundle.createUpdaterArtifacts`), verifies the minisign signature against the
  `plugins.updater.pubkey` compiled into the app, and swaps the bundle in place. The DMG stays the
  first-install channel only. All four artifacts must land on the **same** GitHub release —
  `latest.json` is fetched from `/releases/latest/download/`, which resolves to the newest
  *published, non-prerelease* release, so a draft ships an update nobody can see.
- **`xattr -dr com.apple.quarantine` does not come back.** `com.apple.quarantine` is written by
  the *downloading* app (a browser, via LaunchServices); the updater fetches over the app's own
  HTTP client, so nothing sets the xattr and the extracted bundle inherits none. Gatekeeper's
  first-launch assessment only fires on quarantined bundles. One manual `xattr` on the first DMG
  install, never again. Ad-hoc signing (`Signature=adhoc`, `TeamIdentifier=not set`) stays fine
  for *this*: there is no cert continuity to break. It is **not** fine for TCC, which is a separate
  concern this bullet used to obscure — see **macOS file access** above. Through v0.5.0 `tauri
  build` produced no bundle signature at all, and the resulting invalid signature plus random
  per-build identifier is why folder permissions never persisted across updates.
- **Restart goes through `AppHandle::request_restart`** (`commands::restart_app`), not
  `plugin-process`'s `relaunch` and **not `AppHandle::restart`**. The restart has to fire
  `ExitRequested`/`Exit` on the way out, because that is what runs teardown — otherwise every
  update orphans process groups and leaks a scratch dir. `relaunch()` doesn't fire them at all,
  and `restart()` only *sometimes* does: its own docs say that called **on the main thread** it
  "cannot guarantee the delivery of those events, so we skip them" and re-execs immediately.
  Whether a command body runs on the main thread is Tauri's scheduling choice — measured today it
  is *not*, and `restart()` did fire the events and did tear down correctly, so this is a bug that
  would have stayed invisible until a runtime upgrade moved the thread. `request_restart` always
  routes through `request_exit(RESTART_EXIT_CODE)`. The leak fix below is a prerequisite for all
  of this, not a nicety.
- **Busy guard.** `updater.ts::busySessionCount()` counts `working` (mid-turn) and `needs`
  (stopped on a question) across **every open project**, not just the visible one; non-zero parks
  the banner in a `confirming` state naming the count. `waiting` sessions don't count — `--resume`
  restores those intact.
- **Automatic checks are silent on failure; manual ones aren't.** A laptop on flaky wifi must not
  accumulate error banners nobody asked for, but a menu-item check that silently did nothing would
  read as broken. Same function, `manual` flag.
- **The banner is NOT gated on `ready`** — don't "tidy" it back inside that block. `ready` flips
  only after `bootstrap()` has walked every open project and built + attached an xterm for every
  session, serially, so with several projects restoring, the launch check finishes early and the
  card would sit invisible for as long as bootstrap took. That is exactly what the first user
  report of "the banner only appears if I click Check for Updates" was: the check had worked
  fine. Measured against the real endpoint, with zero projects the banner is up ~6 s after
  launch. The card is fixed-position and owns nothing bootstrap provides.
- **Shipped in v0.4.7**, which by construction had to be installed by hand — it is the release
  that *adds* the updater, so nothing older could deliver it. v0.4.8 was a deliberate no-op
  release published to exercise the real path end to end.
- **Releasing:** `npm run release` (`scripts/release.sh`) — preflights the key, the
  tauri.conf.json/Cargo.toml version agreement, a clean tree and an unused tag; builds; writes
  `latest.json`; `gh release create`s all four artifacts. `--dry-run` builds and writes the JSON
  without publishing — and because it skips the clean-tree and unused-tag checks, it is also how
  you inspect a release build *before* committing. Use it: `release.sh` checks only that the
  artifacts **exist**, never that the `.app` inside them is signed, which is exactly how the
  unsigned bundles above shipped for five releases. Worth checking after publishing too — re-fetch
  the served tarball, compare its SHA-256 to the local one, and run `codesign --verify --deep
  --strict` on the `.app` inside it.
- **The signing-key gotcha, which costs a full release compile to rediscover:** `tauri signer
  generate` prints `TAURI_SIGNING_PRIVATE_KEY_PATH`, but the v2 bundler reads **only**
  `TAURI_SIGNING_PRIVATE_KEY` (contents or path). With just the `_PATH` form set, the build runs to
  completion, emits the `.tar.gz`, and *then* dies with "A public key has been found, but no
  private key". Both `release.sh` and the `tauri:build` npm script export the key contents. The key
  lives at `~/.mulpex/updater.key` (0600, no password) and is **not** in the repo — lose it and no
  existing install can ever accept another update; the only recovery is a new keypair plus a manual
  DMG reinstall by every user.

## Teardown fires on TWO RunEvents (the fixed scratch-root leak)

`lib.rs` matches **`RunEvent::ExitRequested | RunEvent::Exit`**, and dropping either arm
re-opens a measured bug. `ExitRequested` is only reachable through `app.exit()` — i.e. the
window-close arm and `AppHandle::restart()`. **⌘Q and an Apple-Event `quit` never touch it:**
they go to Cocoa's `NSApplication terminate:`, which tao turns into `applicationWillTerminate:` →
`AppState::exit()` → `Event::LoopDestroyed`, and tauri-runtime-wry maps *that* to
`RunEvent::Exit`. Matching only `ExitRequested` meant teardown never ran on the two quit paths a
human actually uses.

The old diagnosis in this file inverted the evidence: it read "every `claude` was dead" as proof
the `killpg` half ran and only `remove_dir_all` failed. **Neither half ran.** The children died
from the PTY hangup when the process exited — which kills the foreground process group only, so
anything an instance had backgrounded was orphaned, exactly the case `killpg` exists for.

Measured before/after on an isolated `HOME` with one empty project (`scratchpad/measure-quit.sh`):
AppleScript quit and ⌘Q both **leaked** the whole `temp/mulpex-<pid>/` tree and both are now
**clean**; window-close was clean throughout; the Quit *menu item* is clean too. `teardown` is
idempotent, so window-close firing both events and running it twice is fine.

**Second layer: `Workspace::sweep_stale_state_roots()`**, run in `setup()` before the new root is
created. No shutdown hook can cover Force Quit, `kill -9`, a crash or a power loss, so each launch
also collects `temp/mulpex-<pid>` dirs whose pid is no longer alive (`libc::kill(pid, 0)`, with
`EPERM` counting as alive). It errs toward *keeping*: a recycled pid just defers that dir to a
later launch, whereas deleting a live Mulpex's root would break its running hub. This is what
finally collected the 12 dirs the old bug had accumulated on this machine.

## Last Synced Commit

`45442123527b9fbe55d0d153daa6d4aeba058069` — 2026-08-05
