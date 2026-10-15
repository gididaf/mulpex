# Remote Hosts (File ▸ Connect to Remote Mulpex…)

One Mulpex driving another. The user's case is a Mac driving a Linux VM, which is otherwise only
reachable over RDP (laggy, and copy/paste breaks). The VM's projects show up on the Mac as tabs
behind a **host switcher**. Each claude there is a **real terminal mirror** of the VM's pane: you
type into it, and copy/paste uses the Mac's own clipboard. Every feature works on a remote tab:
saves, explain, secrets, pins, compact, open/close projects and drag-and-drop.

Built on Remote Control (`remote-control.md`), and uses the same relay, pairing and encryption.
A remote desktop pairs exactly like a phone. Then it says `hello` as a desktop and gets the host
itself, not a phone's view.

## The pieces

```
Mac:  frontend ─ ipc.ts call() ─► host_invoke ─► remote/client.rs ─wss─► relay ─► VM: remote/mod.rs
                                                     ▲ events, lifted                 + desktop.rs (streams, lease, uploads)
                                                     └ Channel per pane ◄──────────── + rpc.rs (allowlisted commands)
```

- **`remote/client.rs`**: the Mac side. It holds one connection thread per paired host
  (`<home>/remote/hosts.json`, 0600), the pending commands, a channel per mirrored pane, and the
  handle translation.
- **`remote/desktop.rs`**: the host side, inside the Remote Control connection loop. It covers:
  - event forwarding;
  - terminal streams and snapshots;
  - keystrokes;
  - the geometry lease;
  - uploads.
- **`remote/rpc.rs`**: what a remote desktop may run. Every command in `lib.rs`'s
  `generate_handler!` must be in `ALLOWED` or `DENIED`, and a unit test parses `lib.rs` to enforce
  it. **A new command must be classified there**, or it silently does nothing on a remote tab.
- **Frontend**:
  - `hosts.ts`: the vocabulary.
  - `remoteHosts.ts`: the lifecycle.
  - `HostSwitcher.svelte`, `ConnectHostDialog.svelte`, `RemoteProjectPicker.svelte`.
  - `ipc.ts::call`: the one router.
  - `terminals.ts`: mirrors, `serialize`, the geometry lock.

## Handles

A remote project is `slot · 2^32 + its handle there`:
- `slot` numbers the paired hosts from 1;
- 0 is this Mulpex;
- the sum is exact in JS.

So `projects`, `terminals.ts`'s keys, pins, To Do lists (`todos-changed` is forwarded like
`pins-changed`), explain, attention and save states all work on remote
projects unchanged.

`client.rs` is the only place a handle crosses the boundary:
- `lower` on a command's arguments (`projectHandle`, `handles`);
- `lift` on answers and events (every `handle` field, a workspace's `active`, and
  `import-update`'s bare number).

Events arrive under their **own names**: `hub-update`, `sessions-changed` and the rest. So the
existing listeners serve both kinds of project. The one exception is `projects-changed`, which
arrives as `host-projects-changed {slot, ws}`.

## Terminal mirrors: exact, in order

Each session's `OutputSink` keeps a ring for **every** kind:
- 2 MB for a claude;
- 256 KB for a shell;
- `MULPEX_DEBUG_TAIL_BYTES` overrides both in a debug build.

The ring records **resizes** as markers at absolute stream offsets. `desktop_tap()` returns the
ring plus a live receiver of bytes and resizes.

When a mirror starts (`sub`), `desktop.rs::compose` picks one of three openings:

1. **The ring still holds everything since spawn:** replay it from the PTY's spawn size. This is
   exact and needs no frontend.
2. **Otherwise, ask this window's xterm for its screen** (`host-serialize`, answered through
   `remote_snapshot_reply`):
   - `terminals.serialize` counts the bytes handed to `term.write` (`received`) and queues an
     empty write.
   - Its callback runs once everything before it is parsed, so the serialized screen, plus 2,000
     lines of scrollback (`@xterm/addon-serialize`), is exactly stream offset `q`.
   - The host then sends the screen, then `ring[q..]`, then live output. If the xterm got ahead
     of the tap, the live stream skips what the screen already shows.
3. **No answer within 3 s** (a throttled webview): send the ring alone, marked `partial`. The host
   then nudges that PTY one row smaller and back, so claude repaints its whole screen.

Everything for one remote desktop travels on **one sealed channel**, so this order holds end to
end.

Frames:
- `snap`: the screen, deflated;
- `o`: bytes, deflated when they are over 2 KB;
- `r`: a resize;
- `gone`.

Mirrors acknowledge received output (`ack` every 256 KB). The host stops streaming 4 MB ahead,
and re-snapshots a session whose unsent backlog passes 8 MB.

A mirror's size changes **only** by its stream, never by this window's refit. A resize keeps a
pane that was at the bottom at the bottom.

On the Mac, `client.rs` re-subscribes every open mirror on each `welcome`, so a reconnect
restarts each pane from a fresh snapshot. The frontend does nothing for that.

When ⌘⇧R replaces a session's process, the old tap's receiver disconnects and the host
re-subscribes the new one (a replay from spawn). Separately, `session-restarted` (now emitted by
`restart_session`) is what makes **every** window rebind its own pane.

## The geometry lease

Every PTY has one geometry (root CLAUDE.md), so two windows cannot both size them:
- `Workspace::geometry_owner` names the remote desktop driving it;
- while it does, this window's `resize_terminals` is ignored.

The rest of the lease:
- **On the host's own window:** `geometry-changed` makes its frontend hold every xterm at the
  leased size (`terminals.setLock`) and show "<Mac> is driving this Mulpex · Take back".
- **On the Mac:** it claims the lease:
  - when it connects;
  - whenever its window gets focus;
  - when its pane resizes while it holds the lease;
  - never before its pane has been measured once (`terminals.geometry()` is null until then).
  When another window holds the lease, a remote pane shows "sized by … · Take over".
- The lease returns to the host when the last remote desktop leaves.

## Things that run on one side only

- **Typing:** the Mac's xterm encodes keys and `sendBytes` sends them as `in`, straight to the
  PTY. There is no prompt check: this is the user's own keyboard, as with a local pane.
- **⌘⇧K:** the compact state machine (`compact.ts`) runs in the **host's** window, reached
  through `fa` → `remote-action`. On the Mac, `setCompactPhase` and `autoCompactTick` are no-ops
  for remote handles, and `compactLines` shows the phase the host's hub reports. The auto-compact
  rules are enforced only by the host.
- **Selections** (⌘⇧E, ⌘⇧P, ⌘K's prompt read) are read from the Mac's mirror.
- **Never sent to the host:** `switch_project`, `focus_session`, and the host's lifecycle commands
  (`rpc.rs::DENIED`, with reasons). Driving the host never moves its own window around.
- **Files:** a drop, and Ctrl+V with an image in a remote claude, upload over the channel. They
  land in `<home>/remote/uploads/<id>/` on the host (0600, pruned after 7 days, 100 MB cap,
  folders kept whole), and the host paths are pasted. Ctrl+V with no image goes through as
  `\x16`.

## Lifecycle (`remoteHosts.ts`)

- **On connect:**
  - the host's workspace is fetched (`bootstrap`, through the host) and built like local projects;
  - each project's hub snapshot and pins are reloaded;
  - the geometry is claimed;
  - the layout (names only) is cached in `hosts.json`, so a launch with the host down still draws
    its tabs.
- **On disconnect:** the tabs stay, greyed, with every status cleared. A claude on an unreachable
  host is not "waiting" for anyone, and must not light a badge.
- **Host restart:** a new `boot` id means every handle may now name something else, so that
  host's tabs are dropped and rebuilt.
- **Attention:** remote claudes count in the dock badge and notifications, which name the host.
  A (re)connect is a priming pass for that host (`attention.ts::quietHost`), so whatever already
  needed you there doesn't fire a burst of banners.
- **Versions:** `hello` carries the app version, and a mismatch is refused with the other side's
  version. A host from before Remote Hosts answers with a phone view, which the Mac reports as
  "too old".

## UX

- **Host switcher** at the left of the tab bar, drawn only once a host is paired. Each button
  carries:
  - the tab badges summed over that host's projects, muted excluded;
  - a connection dot.
- **Clicking a host** lands on its first project with a claude that needs you, else the one last
  in front there.
- **Right-click a host:** Rename / Disconnect or Connect / Reconnect now / Remove.
- **The tab bar** shows the host in front's projects. ⌘1–9 and ⌘⇧[ / ⌘⇧] act within it.
- **Switching hosts:** ⌘⌥[ / ⌘⌥], declared in the menu and also claimed in `onGlobalKey` by
  `e.code`, because Option changes the bracket's character. On Linux it is `A-Bracket*` in
  `linuxKeys.ts`, whose `chordId` now includes Alt.
- **⌘P** lists every host's projects as "◆ vm ▸ name".
- **Pairing:** "Copy link" on the host's ⌘⇧O dialog, then File ▸ Connect to Remote Mulpex… on
  the Mac. Macs appear in the host's device list with 💻, and its top-bar chip counts phones and
  computers separately.

## Testing it

- **Unit tests:** `cargo test -p mulpex --lib remote` covers:
  - compose (all branches);
  - frames;
  - upload paths;
  - handle lifting;
  - link parsing;
  - folder walking;
  - the allowlist;
  - the tap ring.

  `a_desktop_pairs_mirrors_and_drives` plays a desktop against the real connection thread with a
  stand-in relay.
- **The real relay, in one process:** `live_through_a_real_relay` (ignored) runs the relay
  binary, `client.rs` and `desktop.rs` together:

  ```
  MULPEX_TEST_RELAY_BIN=target/debug/mulpex-relay cargo test -p mulpex --lib live_through_a_real_relay -- --ignored --nocapture
  ```

  It prints the keystroke echo time.
- **Headless, two real apps:** `bash scripts/test-remote-hosts.sh`. It builds a copy whose window
  is hidden, and pre-pairs two instances, so nothing needs a click. It then drives the host's
  terminals through `termreq`, and compares the panes line by line. Both apps dump every pane
  with `MULPEX_DEBUG_DUMP`. `CLAUDE=1` adds a real claude TUI.
  The same switch gives the frontend a tiny driver: one JSON action per 0.7 s from
  `<home>/drive/*.json` (`{"click": selector, "text": …}`, `{"menu": id}`, `{"termInput": …}` typed
  through xterm, `{"invoke": cmd, "args": …}`), and `ui.txt` / `drive-log.txt` dumps of what the
  window shows. That is how the "⌘O on a host opened the folder on the Mac" bug (the picker closed
  before the host was read) was replayed and its fix checked, with no window on screen. Write actions
  with `printf '%s'`, never `echo`: zsh's `echo` turns `\r` into a raw CR, the JSON stops parsing, and
  the action silently never runs (that cost an hour of chasing a typing bug that did not exist).
