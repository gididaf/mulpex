# Frontend behavior (Svelte UI)

Sidebar order, tabs, badges, drops, mute, drag-reorder, context menu, attention. Read this before changing anything in
`src/lib/components/`, `stores.ts`, `attention.ts`, or `App.svelte`.

Rendering invariants (geometry, RTL) live in [rendering.md](rendering.md).

Back to [CLAUDE.md](../CLAUDE.md).

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

## Sidebar order: claudes above terminals

The sidebar is **two blocks with a rule between them** — every claude, then every terminal — rather
than one list in creation order. Before this, a ⌘⇧T shell landed wherever it happened to be
created, so the two kinds interleaved and the list had to be *read* to find anything.

- **Kind is the outer grouping, mute the inner one.** `stores.ts::groupOf` gives three ranks:
  unmuted claude (0), muted claude (1), terminal (2), and `displayOrder` is a stable sort on that
  one key. So a muted claude still sits **above** the terminals — it is a claude that has been
  quietened, not a lesser kind of row — and each block keeps its base order (creation order, or
  whatever a drag arranged). Mute only ranks claudes: `groupOf` ignores a shell's own `muted` flag,
  because the backend refuses to record one and a shell that somehow carried it must not fall out
  of the terminal block.
- **The rule is drawn from the first terminal row** (`InstanceList.svelte`, `.split`), not emitted
  once after the loop — so it lands correctly whatever the list contains, and never appears when a
  project has only claudes or only terminals. The margin around it carries as much of the split as
  the 1 px line does.
- **Everything downstream of the order followed for free**, because the order is one function:
  drops clamp to the new blocks (see **Sessions drag to reorder**) and ⌘[ / ⌘] cycle the same
  visible list. Nothing about hub state, ids or persistence changed — this is a sort, and
  `p.sessions` (the persisted base order) is untouched by it.

## Muted sessions (⌘M)

A muted instance **keeps running and keeps coordinating** — same PTY, same inbox, same peer list,
same `hub_instances` entry. Mute is purely a statement about how loudly the *sidebar* may talk
about it, and it's deliberately not a hub concept: nothing in `mulpex-core` knows the flag exists.
Concretely it: dims the row, **sinks it below the unmuted claudes** (but still above the terminal
block — see **Sidebar order** above), drops its status dot, its status word and its ⏳, removes it
from **every attention count** — the tab's green `ready` and red `needs` badges, and the
hub-panel/status-strip unread readouts — and takes it out of the ⌘[ / ⌘] rotation.

- **Ordering is one function**, `stores.ts::displayOrder` — a stable sort on `groupOf`, so each
  block keeps its base order (creation order, or whatever a drag arranged — see **Sessions drag to
  reorder**) and unmuting drops a session straight back where it came from. It feeds both the
  sidebar and ⌘[ / ⌘], so what you see is what you cycle. `TerminalPane` is unaffected
  (absolute stacking; order is meaningless there).
- **⌘[ / ⌘] skip muted rows.** Muting says "stop putting this in front of me", and a row you have
  to tab past is still in front of you. A muted row stays clickable and keeps focus if it has it;
  `App.svelte::cycle` then walks outward from it to the **nearest unmuted row in the direction of
  travel** rather than jumping to an end of the list, so ⌘] out of a muted row lands where the eye
  expects. With nothing focused the walk starts *before* the head, so a first ⌘] selects the top
  row and a first ⌘[ the bottom one. Everything muted → the keystroke does nothing, rather than
  reselecting a silenced row.
- **The unread badge needed a backend change.** `pending_messages` is one project-wide total, and
  "how much of this is mail for a muted instance" isn't answerable from a total — so the poll loop
  now also emits a per-recipient `pending: Vec<PendingEntry>` breakdown, and `unreadCount`
  subtracts the muted share. The **message log itself is untouched**: mute silences the count that
  pulls your eye, not the record of what happened.
- **Persisted per project**, alongside the custom name and the instance number, as the third
  tab-separated field in `~/.mulpex/sessions/<key>.txt`
  (`<uuid>[\t<name>[\tmuted[\t<id>]]]` — see the store's positional columns in
  [sessions.md](sessions.md)).
  Every older format still loads — a bare uuid, a `<uuid>\t<name>` line, and a three-column
  pre-id line — and because the columns are positional a muted-but-unnamed instance writes the
  name column empty so the flag stays in field three. Covered by five `persist.rs` tests.
- **Muting never moves focus**, and the muted terminal stays visible and typeable. Mute means "stop
  shouting at me", not "I'm done with this one".
- **Sending the instance a prompt unmutes it.** Mute says "stop putting this in front of me";
  talking to the row *is* the user putting it in front of themselves, so leaving it dimmed and sunk
  would hide the one instance they are actively working with. There is no ⌘M to undo — it undoes
  itself.

  **Only a genuine user prompt counts, and the hook is the only side that can tell.** A
  `<task-notification>` — a hub wake, a finished background job — fires `UserPromptSubmit` exactly
  like a prompt (the general rule in the root file's invariants), so unmuting on one would undo a
  ⌘M the moment a peer sent mail, which is precisely what mute exists to prevent. So the signal is
  a mark rather than a keystroke: `hook.rs::userpromptsubmit` writes `userprompt/<id>` in the one
  branch that already knows the turn is the user's (`!system_turn`, not `MULPEX_SENTINEL`), and the
  200 ms poll consumes it in `Core::process_user_prompts`, unmutes and persists. Both hosts do it —
  Cost is one empty dir
  read per project per tick.

  The frontend needed no change: the poll loop's session diff republishes the row, and
  `App.svelte::syncMuteMenu` already re-runs on every hub update, so the Session ▸ Mute tick
  follows. The mark is written for *every* instance, not only muted ones — the hook cannot see the
  mute flag, which lives in `Core` — so the reader's job is mostly to delete a file and report no
  change. It is consumed even for an unknown id, or a stale one would be re-read every tick.

  Typing into the terminal is deliberately **not** the trigger, though it was the cheaper option:
  a keystroke unmutes on an Esc or an arrow key, and an Enter is as often a y/n confirm or an
  AskUserQuestion pick as it is a prompt. `userprompt/<id>` lands ~200 ms later and means the thing
  it says.
- **The 🔇 is not decoration.** A dimmed, dot-less, status-less row would otherwise read as *dead*
  rather than *silenced* — same failure the empty hub-panel sections had, an ambiguous readout that
  teaches the eye wrong. It's also the click target for muting a session **without focusing it**
  (unmuted rows show a 🔊 only on hover, so it stays reachable without adding noise).

**A new menu item is not wired until `lib.rs::is_forwarded` lists its id.** That function is an
*allowlist* — `on_menu_event` drops anything not in it — so a new item builds, appears in the menu,
shows its accelerator, and (for a `CheckMenuItem`) even ticks itself on click, while the frontend
never hears a thing. Nothing fails: no error, no log line, no compiler complaint. Both `mute` and
`minimize` shipped in that state and were caught only by driving the real app. This is the same
shape as the other silent failures in these notes — a real event with nowhere to arrive (see
**How this codebase fails** in [../CLAUDE.md](../CLAUDE.md)).

## The sidebar context menu (right-click)

Right-clicking a row opens a small in-app menu; right-clicking the gap below the rows opens a
two-item **New Session / New Terminal** menu. `ContextMenu.svelte` is deliberately not a general
widget — no submenus, no icons — and `App.svelte::openRowMenu` builds the item list per row.

- **In-app, not `Menu::popup`.** A native item is only wired once `lib.rs::is_forwarded` lists its
  id, and one that isn't listed still builds, still draws and still ticks itself while the frontend
  hears nothing (see **How this codebase fails** in [../CLAUDE.md](../CLAUDE.md)). A menu whose
  items change per row would pay that trap on every entry. Plain DOM means what it renders is what
  it runs.
- **It acts on the row you clicked, which is not the focused row.** Right-click does *not* select —
  renaming or closing a background instance must not yank the center pane away from what you were
  reading. That is also why **⌘R / ⌘⇧R / ⌘M / ⌘W hints are printed only when the clicked row is the
  focused one**: those keys act on the focused row, so printing them beside an item aimed at a
  different instance would advertise a key that does something else. An absent hint means "no hint
  here", never "no shortcut exists".
- **Items:** Rename… (opens the same `RenameDialog`, which already takes an explicit `(handle, id)`
  and works for terminals), Mute/Unmute, Copy address, a separator, Restart…, Close. **Mute and
  Restart are absent on a terminal row** rather than greyed out — the backend drops the mute flag
  for a shell, and a shell has no conversation to resume, so both would be dead weight. Close does
  not confirm, matching ⌘W; **Restart does**, because it kills a process that may be mid-turn and
  sits one shift away from Rename (see [sessions.md](sessions.md#restarting-an-instance-in-place-r)).
- **Copy address copies the hub's own written form** — `claude#3` / `term#5` — so it pastes straight
  into a `hub_send` or a prompt. There is no clipboard plugin (and adding one means another
  capability entry, the same allowlist shape as above), so it is `navigator.clipboard.writeText`
  with a hidden-textarea `execCommand` fallback.
- **It qualifies the address once another open project has a claude in it**
  (`stores.ts::claudeInAnotherProject`, `App.svelte::instanceAddress`): `mulpex#1` for a claude,
  `term#4 (in mulpex)` for a terminal. The bare `claude#1` is the *unsafe* string here, not the
  safe one — it is a valid **local** address in every project, so a claude reading it elsewhere
  sends to its own instance 1 and the message is silently delivered to the wrong instance.
  `<project>#<n>` cannot do that, and it is **not cross-project-only**: `mcp.rs::send_foreign`
  resolves a qualifier naming the sender's own project back to a local send, so the one string is
  exact from everywhere and is what `hub_instances` itself reports. Terminals invert the argument —
  they have no cross-project address at all and `registry::parse_address` refuses every `term#…`
  outright, so nothing there can mis-deliver and the project is free to be readable prose. Only
  claudes elsewhere arm the qualifier; a project holding nothing but terminals adds nobody you
  could address.
- **`focus()` is ignored on a `visibility: hidden` element.** The menu is hidden until it has been
  measured and flipped away from the window edge, and focusing it in that same effect run silently
  did nothing — no error, nothing visible, it just meant every keystroke went on reaching the
  terminal underneath. The fix is to focus after `ready` lands in the DOM (`tick().then(...)`).
  Measured in headless Chrome: `document.activeElement` stayed `BODY`. Keys are handled on the
  **window**, not the menu element, so Escape/arrows still work if focus is lost — but the element
  focus is the part that keeps keystrokes out of the terminal, so it is not optional.
- Row right-clicks `stopPropagation()`, which is what keeps the container's empty-space menu from
  firing as well.

## Sessions drag to reorder

Sidebar rows drag vertically exactly as project tabs drag horizontally — same mechanism
(**pointer events, not HTML5 drag-and-drop**, because Tauri's webview drag-drop is enabled for
dropped paths and intercepts DOM drags; pointer capture also gives the 4 px threshold that keeps a
click from registering as a drag), same `suppressClick` so a drag never also selects the row, same
visuals (dragged row fades, target slot gets an accent edge — on the *top* edge here, since the
list runs vertically). Terminals drag like instances: one list, one behavior.

- **Manual order and the display grouping are composed, not alternatives.** `p.sessions` is the
  *base* order a drag rewrites; the sidebar renders `displayOrder(p.sessions)` on top of it, so the
  blocks still form. That means a drop **across a block boundary could never stick** — the row
  would visibly snap back on release. So drops are **clamped to the dragged row's own block**
  (`stores.ts::clampToGroup`, which scans for that block's first and last index rather than
  assuming two groups): dragging a muted row to the top lands it at the top of the *muted* block,
  and a terminal can never be dropped above the rule. Clamping keeps the drop indicator honest and keeps the emitted order already-grouped,
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
  you cycle" rule the kind split and the muted sort already follow.
- **⌘⇧↑ / ⌘⇧↓ is the keyboard form of that drag** (`App.svelte::moveInstance`, Session menu): the
  *focused* row moves one slot and commits through `applySessionOrder`, so there is no second
  implementation of the reorder. It reuses `clampToGroup` + `dragOrder` for exactly the reason the
  drag does, and a clamped `to` that equals `from` is dropped rather than round-tripping an order
  that changes nothing — so the ends of a block are a silent no-op, matching ⌘⇧← / ⌘⇧→ on tabs.
  Like those, the keys are declared in the menu *and* claimed in `onGlobalKey`, because with the
  terminal focused ⌘⇧↑/↓ is AppKit's `moveUp/DownAndModifySelection:` on xterm's helper textarea
  and never reaches the menu (see **Keyboard** in `../CLAUDE.md`).

## What a project tab shows

Name + **two count badges in the sidebar's own dot colors**, so a tab reads the way a row does:
**green = claudes done and idle** (`readyCount`, status `waiting`) and **red = claudes stopped on
a question** (`needsCount`, status `needs`). An instance holding only a **watcher** — an agentalk
pairing, its hub listener — is counted green, deliberately: it is waiting for a peer, which is
idleness. (The updater's guard reads a separate flag for those; see
[sessions.md](sessions.md).) Both hide at zero, and **both exclude muted
sessions** (see above). `working` is deliberately unbadged — the tab says what is *finished* and
what is *blocked on you*, not what is busy — so a project with everything running shows no pill at
all. Terminals are excluded for free (a terminal has no status entry). The pills are dark text on
a saturated background; white on either of those colors fails contrast.

The needs count is the gap this closes — a background project blocked on a question used to look
identical to an idle one, findable only by switching tabs, even though `ProjectState.statuses` had
the answer all along. ⌘1–9 selects a tab (see **Keyboard** in [../CLAUDE.md](../CLAUDE.md)).

**Two things the tab used to show and no longer does.** The always-on **session count** said
what's *here* rather than what wants you, which is the opposite of a badge's job; and the **amber
unread-mail pill** duplicated readouts the hub panel and the bottom bar already carry, while
competing for the eye with the two statuses that actually stop work. `unreadCount` is still
exported and still subtracts the muted share — it simply has no tab pill any more.

**Tabs drag to reorder.** `ProjectTabBar` uses **pointer events, not HTML5 drag-and-drop** —
Tauri's webview drag-drop is enabled (App.svelte needs it to drop folders onto the window) and
intercepts drags before the DOM sees them; pointer capture also gives us the 4 px threshold that
keeps a click from registering as a drag. Dropping calls `reorderProjects` in `stores.ts` (rebuilds
the `Map`, since insertion order *is* tab order) and the `reorder_projects` command, which reorders
`Workspace::projects` and re-runs `persist_open()` — so the arrangement survives relaunch. Tab
order is also what ⌘1–9 index into, so a drag remaps them by design. Handles missing from the
submitted order are appended rather than dropped, so a stale caller can't make a project vanish.

**⌘⇧← / ⌘⇧→ are the keyboard route to the same thing** (File ▸ Move Project Left/Right, and the
palette). `App.svelte::moveProject` splices the active handle one slot and commits through
`applyProjectOrder` — the identical path a drop takes — so persistence and the ⌘1–9 remap come for
free. **Clamped, not wrapped:** the edges are a no-op, unlike ⌘⇧[ / ⌘⇧] cycling, because a tab
teleporting from one end of the strip to the other reads as a mistake. The keys are claimed in the
webview rather than left to the menu accelerator; see **Keyboard** in [../CLAUDE.md](../CLAUDE.md)
for why a menu accelerator loses to xterm's textarea.

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
> the `permission_prompt` matcher is effectively dead and `needs` means AskUserQuestion, or idle
> **with nothing of its own still running** — see **`needs` must mean "needs YOU"** in
> [sessions.md](sessions.md).

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

