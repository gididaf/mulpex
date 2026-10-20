# Remote Hosts (File ▸ Connect to Remote Mulpex…)

One Mulpex driving another. The user's case is a Mac driving a Linux VM, which is otherwise only
reachable over RDP (laggy, and copy/paste breaks). The VM's projects show up on the Mac as tabs
behind a **host switcher**. Each claude there is a **real terminal mirror** of the VM's pane: you
type into it, and copy/paste uses the Mac's own clipboard. Every feature works on a remote tab:
saves, explain, secrets, pins, compact, open/close projects, drag-and-drop, ⌘⇧Q (which restarts
the host) and the host's own ⌘⇧O.

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

So `projects`, `terminals.ts`'s keys, pins, the Board (`board-changed` is forwarded like
`pins-changed`; images come over `host_invoke` in chunks), explain, attention and save states all work on remote
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
- **Never sent to the host:** `switch_project`, `focus_session`, `quit_app` (nobody could start it
  again), and anything that would cut the link itself — `remote_set_enabled`,
  `remote_set_relay_url` (`rpc.rs::DENIED`, with reasons). Driving the host never moves its own
  window around.
- **Files:** a drop, and Ctrl+V with an image in a remote claude, upload over the channel. They
  land in `<home>/remote/uploads/<id>/` on the host (0600, pruned after 7 days, 100 MB cap,
  folders kept whole), and the host paths are pasted. Ctrl+V with no image goes through as
  `\x16`.

## Checkpoint, restart and the wake offer on a host

Before 2026-10-16 ⌘⇧Q on a remote tab stopped the **Mac's** claudes and quit the Mac; the host was
untouched, because every `checkpoint_*`, `restart_app` and `remote_*` command was `DENIED`. Now:

- **⌘⇧Q acts on the machine whose tab is in front.** On a host's tab, `checkpoint.ts` runs the
  same panel with `host` set, every `checkpoint_*` call goes through `ipc.ts::call`, and the
  rows read the host's lifted statuses like any remote project. One round at a time across all
  machines: a second ⌘⇧Q elsewhere is refused with a toast naming where one is running.
- **Its final button restarts the host** ("Restart vm"), never quits it — nobody could start it
  again. `rpc.rs` maps `checkpoint_commit` to `checkpoint_commit(true)` (auto-wake: nobody is at
  that screen to click "Wake all"), then `restart_app` answers first and restarts 800 ms later
  (`RESTART_DELAY`), so the Mac reads success, not a dropped link. The tabs grey, reconnect and
  rebuild on the new `boot`; the claudes get "Mulpex is back" as their listeners come up.
- **The link dropping mid-round** greys the final button and says the claudes stay stopped;
  Cancel once it is back un-stops them.
- **Plain restart:** the host switcher's right-click ▸ **Restart Mulpex…**. It names any claude
  still `working` there (cut off mid-turn, resumed after) and points at ⌘⇧Q; Restart anyway.
- **The "Wake all" offer** of a host relaunched at a checkpoint (someone ⌘⇧Q'd *on* it and started
  it again) shows on the Mac too: `checkpoint_pending` is read through the link on every connect.
  Wake/dismiss on either side emits `checkpoint-changed`, forwarded to the Mac as
  `host-checkpoint-changed {slot}` (it names no project, so it can't be lifted like the rest);
  both re-read, so the card closes on both.
- **⌘⇧O on a host's tab** is that host's Remote Control: its QR / pairing link, its devices,
  removing a phone. What would cut this link is not offered — no on/off, the relay address is
  read-only, and the Mac's own row says "this computer". `rpc.rs` enforces the last one too:
  `dispatch` gets the asking `device` and refuses to revoke it, and `remote_status` answers with
  `you` set to it. No `remote-changed` reaches the Mac from there, so the dialog polls (3 s).

Measured headless with two real apps and a real claude on the host (2026-10-16): the round, the
restart and the auto-wake; the link dropping mid-round; the wake offer answered on the Mac and,
separately, on the host; host compacts held during the round; the refusals.

## Lifecycle (`remoteHosts.ts`)

- **On connect:**
  - the host's workspace is fetched (`bootstrap`, through the host) and built like local projects;
  - each project's hub snapshot and pins are reloaded;
  - the geometry is claimed;
  - the layout (names only) is cached in `hosts.json`, so a launch with the host down still draws
    its tabs.
- **On disconnect:** the tabs stay, greyed, with every status cleared. A claude on an unreachable
  host is not "waiting" for anyone, and must not light a badge.
- **At launch** `initHosts` listens for `host-changed` *before* reading `host_list`, then reads
  it once more. The connections start in the backend's setup, so a host can reach `connected`
  before the frontend listens, and that event would otherwise be lost: "connecting" forever.
- **Host restart:** a new `boot` id means every handle may now name something else, so that
  host's tabs are dropped and rebuilt.
- **Attention:** remote claudes count in the dock badge and notifications, which name the host.
  A (re)connect is a priming pass for that host (`attention.ts::quietHost`), so whatever already
  needed you there doesn't fire a burst of banners.
- **Versions:** `hello` carries the app version and the **wire fingerprint** (`wire.rs`). The host
  connects on the fingerprint, not the version; a mismatch (or a `hello` with none — a Mac from
  before v0.48) is refused with the host's version and wire. A host from before Remote Hosts
  answers with a phone view, which the Mac reports as "too old". See **Updates** below.

## The wire fingerprint (`wire.rs`, `build.rs`)

Until v0.48 both sides had to run the same app version, so every release — half of which never
touched the link — stopped every claude on every host. Now `build.rs` hashes the files that define
what the two sides say to each other and bakes it in as `MULPEX_WIRE`:
`remote/rpc.rs`, `remote/desktop.rs`, `remote/client.rs`, `snapshot.rs`, `remote_update.rs`,
`commands.rs` (most command signatures and return types). Each line is trimmed and blank lines and
whole `//` comment lines are dropped first, so a doc-only edit is not a new wire.

- **Same fingerprint = compatible, whatever the versions.** It is deliberately coarse: any code
  edit in those files is a new wire, even one that would have been harmless. No judgment is asked
  of anyone, so it cannot be forgotten.
- **What it does not see:** a serialized type defined outside those files (e.g. in `board.rs`,
  `pins.rs`) that the Mac's frontend reads from a host. If you change one of those shapes, also
  touch a wire file in the same release (or add the file to `wire::FILES`).
- **latest.json carries it** (`"wire"`, written by `release.sh` from `mulpex --wire-hash` on the
  built binary; `release-linux.sh` refuses a Linux build whose hash differs). A Mac reads it
  before updating (`updater.ts::wireOf`, the plugin's `rawJson`). Missing → treated as a new wire.
- **A host behind on the same wire** keeps running. Its host button shows `↑` with the versions in
  the tooltip, and right-click › **Update to v…** sends the same `update` order, after a confirm
  (`App.svelte::updateHostByHand`). Its progress is a card of its own in `UpdateBanner.svelte`.

## Updates: the hosts go first (`remote_update.rs`, `updater.ts`)

Both sides must have the same wire fingerprint, and after the Mac restarts into an update on a new
wire its `hello` is refused. So an update order has to reach each host on another wire *before*
the Mac restarts — or, for a host that was offline then, through the refusal itself. A host
already on the new release's wire is not touched.

- **The message.** `{"t":"update","to":v}` / `{"t":"update-cancel"}`, Mac → host, sealed like
  everything else. It is handled in `on_sealed` **before** the desk check, so a host that just
  refused our version still accepts it. It is not an RPC and is not in `rpc.rs`'s allowlist.
  The host answers with `update-progress` (`step`: `checking` → `stopping` (with `pending`, the
  claudes still working) → `downloading` (`percent`) → `installing` → `restarting`, or `error` /
  `cancelled`). `pump_update` sends the latest one to every session of the device that ordered it,
  so a reconnect mid-run still hears it.
- **The host runs it headless** (`remote_update.rs`), since nobody is at its screen:
  1. `updater().check()`; the version offered must equal `to`, or it is an error. That catches
     a latest.json the Linux build is missing from.
  2. `checkpoint_stop` (the ⌘⇧Q messages).
  3. Wait until `checkpoint_not_stopped` is empty for 1.5 s. That is `checkpoint.ts`'s "done" rule
     (read it and `waiting`) plus two cases a person at the screen would wave through:
     - **idle with a dead listener:** it can never read the message, and nothing is in progress;
     - **read it, turn over, only background work left** (`only_background_work`): the restart
       ends that work anyway.

     Both held the first real update forever (2026-10-15, docs/verification-log.md). A `needs` or
     a claude still mid-turn still holds it. There is no timeout: the Mac's banner lists who is
     still working, and its Cancel un-stops them (`checkpoint_cancel`).
  4. `download`, then `install`.
  5. `checkpoint_commit(auto_wake = true)`.
  6. `request_restart`.

  Any failure after step 2 un-stops the claudes. On the next launch, `lib.rs` setup calls
  `checkpoint_auto_wake`, which queues the wake with no "Wake all" dialog (docs/sessions.md).
- **The Mac, one click** (`updater.ts::applyUpdate`): before its own busy check and install, it
  calls `host_update` on every *connected* host whose `wire` differs from latest.json's (all of
  them when latest.json has none) and waits on `HostStatus.update` until each says
  `done`. Each host is `done` when its reconnect is refused with `app == to` (the Mac is still on
  the old version, so that refusal *is* the success). Any host error or Cancel calls the rest off,
  and the Mac does not update. Its hosts still run the old version, so the link keeps working.
  The banner shows one line per host (`UpdateBanner.svelte`).
- **A late host** (offline during the update, so now older than the Mac): `client.rs::on_refused`
  (reached only on a wire mismatch now) sees a host strictly older by number (`version_of`, never
  a string compare) and orders it to the Mac's version on the spot. The host's state reads `updating`, and a `restarting` ends the link
  with `Retry`, so backoff reconnects to the updated host. A host newer than the Mac keeps the old
  "update both" message, unless the Mac ordered that version, in which case it is `done`.
- **A host from before this feature** ignores the unknown message. After 30 s with no answer
  (`UPDATE_ANSWER`), the Mac says to update it by hand once.
- **Linux hosts must run the AppImage.** The `.deb`'s updater runs `dpkg -i` as root through
  pkexec or zenity, which is a password dialog on a screen nobody is watching. The AppImage
  replaces its own file.
- **Releases are drafts until Linux is in.** `release.sh` creates the release as a draft, runs
  `release-linux.sh`, and only then publishes (docs/packaging.md). A public latest.json without
  `linux-x86_64` would fail every remote update at step 1.

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
- **Remote updates, end to end:** `bash scripts/test-remote-update.sh`. It builds three real
  `.app` bundles — BASE, a made-up NEXT with a code edit in `commands.rs` (another wire) and
  COMPAT (same wire as BASE) — pointed at a local `latest.json`
  (`dangerousInsecureTransportProtocol`, so nothing touches GitHub). It runs four scenarios:
  - **late host:** A is on BASE and B on NEXT, and A must update itself;
  - **one click:** both are on BASE, and a driven click on B's "Update & Restart" must update A,
    then B;
  - **compatible:** A on BASE, B on COMPAT connect and A is left alone (same pid); then a driven
    `host_update` (what the menu item sends) brings A to COMPAT;
  - **one click, compatible:** COMPAT offered; only B updates, A keeps its pid, B reconnects;
  - **rollout:** A built from the last pre-fingerprint release (`LEGACY_REF`, default `e7e452d`
    = v0.47.1), B on NEXT; A must update itself as in "late host".

  Its first run caught a race: a host sends its phone view (`state`) to every session that is not
  a desktop, which included a Mac it had just refused and that was waiting on an update — the Mac
  read it as "too old" and gave up. Fixed on both ends: the host marks a refused desktop
  (`Session::refused_desk`) and sends it no view, and the Mac ignores `state` while an update it
  ordered is still running (needed for hosts still on ≤ v0.47.1).

  `CLAUDE=1` adds a busy real claude in A, which must get both the stop and the wake.
