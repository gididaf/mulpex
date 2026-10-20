# The PTY daemon (`mulpex-ptyd`): claudes that outlive the app

Updating Mulpex used to mean stopping every claude: the app owned every PTY
master, so quitting it hung them all up. Since this change a small daemon,
`mulpex-ptyd`, owns them, and the app is its client. An update, a restart or a
crash closes only the app; every claude and terminal keeps running, and the
next app adopts them — same process, same pane, same row.

A real quit (⌘Q, closing the window) still ends everything, exactly as before.

## Who owns what

| Daemon (`crates/mulpex-ptyd`) | App |
| --- | --- |
| The PTY masters, the child processes, their teardown (`DSession::kill`: tty sweep, a claude's background groups, killpg, reap) | Every argv and every environment variable (`pty.rs`: `claude_command`, `base_env`, `shell_command`) |
| Each session's output ring (2 MiB claude / 256 KiB shell) with resize markers at absolute offsets | A mirror of that ring (`OutputSink`), which the frontend channel, phones and remote desktops tap |
| A shell's transcript recorder (`vtgrid`, moved into the crate) | All hub logic, the store, the poll loop, the seed typer, the delivery watchdog |
| The Live Token proxy (`daemon/live_token.rs`) | The Live Token settings; it pushes the config (`live_token::push`) |

The daemon is deliberately dumb. It never builds a command line, never reads a
hub file, never decides anything about a session. That is what lets it stay
unchanged across many app releases (see *Versions*).

`procs.rs` (moved from `pty.rs`) is the process-table code both sides need:
the daemon kills with it, the app reads a shell's state and reaps orphaned
listeners with it.

## The process

- One daemon per Mulpex home (`~/.mulpex`, a debug build's `~/.mulpex-dev`), so
  a dev build never touches the installed app's sessions. `flock` on
  `<home>/ptyd.lock`; a second one exits at once.
- Socket `<home>/ptyd.sock` (0600, peer uid checked), log `<home>/ptyd/ptyd.log`
  (one `.old` kept past 5 MiB), pid `<home>/ptyd/ptyd.pid`.
- The app starts it on first use (`ptyd::client`) from a **copy** of the bundled
  sidecar at `<home>/ptyd/<PROTO>-<hash>/mulpex-ptyd`: an update replaces the
  bundle, and on Linux the AppImage's mount disappears the moment the app quits.
  Older copies are deleted when a new one is installed; a daemon still running
  from a deleted copy is unaffected.
- `setsid` before exec: no controlling terminal, not in the app's process group.
  Minimal environment (HOME, USER, PATH, TMPDIR, LANG…) — every child gets its
  whole environment from the app (`env_clear` + exactly what `spawn` carries).
- Exits by itself after 15 s with no sessions and no app connected.
- **The helper moved too:** `mulpex-helper` is copied to `<home>/bin/` on every
  launch (`ptyd::stable_helper`) and that path goes into `settings.json`,
  `mcp.json` and the listener command. A claude's hooks name it by absolute path
  for as long as the claude lives — which is now longer than the bundle's.

### macOS permissions (TCC) — measured

The daemon is spawned by the app, so its responsible process is Mulpex.app.
Measured 2026-10-19 with a probe bundle signed as `com.mulpex.app` (the same
designated requirement): a setsid'd child kept Full Disk Access and Documents
access for 12 s after its parent exited, every new process it exec'd too — and
again after the bundle on disk was **replaced** by a re-signed build while the
child ran (16/16 checks). No extra grant is needed.

## The scratch root is the daemon's

Every claude has `MULPEX_STATE_DIR` fixed at spawn, so the dir must survive the
app. The state root is now `temp/mulpex-<daemon pid>` (the daemon creates it,
reported in `hello`), and a project's dir under it is named by
`ptyd::project_key` (sha256 of the canonical path), not by its handle. The
stale-root sweep needed no change: it keeps `mulpex-<pid>` while that pid
lives, and the pid is the daemon's.

`shutdown` removes the root; so does an idle exit.

## Exit modes

`lib.rs::teardown` runs on every way out:

- **Quit** (default): `teardown_all` (kills every session through the daemon),
  the orphaned-listener reap, then `shutdown` — the daemon kills anything left
  and exits.
- **Detach** (`ptyd::restart_detached`, used by `restart_app {detach: true}`):
  1. emit `ptyd-snapshot`; the frontend serializes every local xterm at its exact
     stream offset (`terminals.snapshotAll`) and calls `save_snapshots`, written
     to `<home>/ptyd/snapshots/<project key>/<id>.json` (waits ≤ 3 s);
  2. set `DETACH`, `request_restart`;
  3. teardown → `detach_all`: persist the store, write each project's row
     manifest, drop the sessions (`Session`'s Drop does not kill while
     detaching), close the socket.
- **Crash / kill -9**: no teardown at all. The daemon sees EOF and keeps
  everything. The next app adopts without snapshots.

## Adoption (`Core::open`)

The app writes `ptyd-rows.json` into each project's scratch dir from the poll
loop (only on change): every row — id, uuid, worked, name and who set it, mute,
parent, collapsed, auto-compact, To Do, which row had focus. The store cannot
serve: it holds only worked claudes, and a restart must bring back an
unprompted claude or a terminal as well.

On open, every live daemon session whose key starts with this project's dir
(`pty::session_key`: `<state_dir>/<id>`) is adopted (`Session::adopt`) in
manifest order, with the manifest's row state; a session spawned too late to be
in it gets defaults. Store records whose uuid was adopted are skipped; the rest
`--resume` as before. After every project has opened, `kill_unadopted` ends
whatever no open project claimed. `adopt_geometry` takes the size the
surviving sessions run at before the frontend builds an xterm.

Adopted sessions are not put in `restored`/`started`: they are not starting,
and the early-death and failed-restore graces must not apply.

## Repainting an adopted pane

The pane's first attach is rebuilt, not flushed (`pty.rs::compose`), in the
order of preference:

1. **A snapshot** at an offset the ring still holds → `snap` (the serialized
   screen, at its size) + the ring from that offset. Exact.
2. **The ring starts at spawn** → `snap` at the spawn size + the whole ring.
   Exact.
3. **Neither** (a crash after more than a ring of output) → what the ring has,
   at the current size, then a redraw: the PTY shrinks by a row for 150 ms
   and comes back, which a full-screen program answers with a full repaint.

The channel stays `Channel<String>`: base64 is output, and a message starting
with `{` (which base64 never does) is a control frame — `snap {cols, rows,
offset, s}` or `r {cols, rows}` (`terminals.ts::localChannel`). Resizes are
replayed at their exact stream offsets, the same rule as a remote mirror
(docs/rendering.md). `received` stays the stream offset, so the next restart's
snapshot is cut at the right byte. Text the app writes into a pane
(`Session::notice`) is pane-only and never counted at an offset.

## Versions

- `proto::PROTO` — what the wire speaks. `PROTO_MIN..=PROTO` is what this app can
  drive. The frozen core (`hello`, `list`, `shutdown`) keeps its shape forever,
  so any app can still learn what an incompatible daemon holds and shut it down.
- An update **never replaces a running daemon**. The new app keeps using the
  old one as long as its protocol is in range; the new daemon starts after the
  next real quit. So a daemon-only fix (vtgrid, the kill sequence) reaches a
  user late — after their next ⌘Q. That is the accepted cost; keep the daemon
  small.
- `release.sh`/`release-linux.sh` put `ptyd` and `ptyd_min` in `latest.json`
  (from `mulpex --ptyd-proto`). Before installing, the updater
  (`updater.ts::applyUpdate`, and `remote_update.rs` for a remote-ordered one)
  asks `update_survives`: if the running daemon is in the new release's range,
  there is no busy-claudes warning and no checkpoint — the restart is detached.
  A release without the fields (every one before this) does not survive, and
  gets the old warning and the ⌘⇧Q-style checkpoint, as it always did.
- A new app that meets a daemon outside its range: logs it, sends `shutdown`
  (frozen), starts its own, and the store's `--resume` brings the claudes back.

## If the daemon dies under a running app

The client's reader sees EOF and every session gets `Event::Lost`: they die in
the UI, and `reap_dead` keeps a worked claude's record in the store
(`lost_with_daemon`) so the next launch resumes it. The next spawn starts a new
daemon.

## Protocol

`u32 BE length | u8 kind | payload`. Kind 0 JSON (requests carry `op` and an
`id` the reply echoes; events carry `ev`), 1 output (`u32 sid | u64 offset |
bytes`), 2 input (`u32 sid | bytes` — one frame is one `write_all`, which keeps
Shift+Enter's ESC+CR in one write).

Ops: `hello`, `list`, `shutdown` (frozen); `spawn`, `attach` (ring + resizes +
init, and streaming starts exactly after it — both under the ring lock),
`resize`, `kill` (replies once reaped), `set_meta`, `live_token` (its reply carries
`routes: true` since 2026-10-19), `live_token_stats`, `detach`. An additive op or reply field
that the app detects at runtime is not a `PROTO` bump (docs/live-token.md).
Events: `exit`, `resize` (at its place in the stream). A new connection's
`hello` replaces the old one; a session streams to an app only after that app
spawned or attached it.

## Tests

- `cargo test -p mulpex-ptyd` — framing, the frozen core, ring math, and
  `tests/reattach.rs` against the real binary: a session survives its app
  vanishing mid-stream with output contiguous across the two connections and the
  resize at the right offset; kill reaches a background group; exit is
  reported; shutdown, the lock and idle exit.
- `cargo test -p mulpex` runs the app's session tests against a daemon inside
  the test process (`ptyd::tests_daemon`, scratch home); `a_reopened_project_
  adopts_the_sessions_the_daemon_kept` is the adoption path.
- `scripts/test-ptyd.sh` (`CLAUDE=1` for a real claude) — a hidden-window app:
  detached restart, crash, quit, with pane dumps compared and pids checked.
- `scripts/test-remote-update.sh` — real signed bundles and a local update
  server: scenario 2 checks a claude survives an update, 2b (`CLAUDE=1`) the
  fallback for a release without the `ptyd` fields.

## Not done (yet)

- No `systemd-run --scope` on Linux: an XFCE/xrdp session does not put apps in
  their own scopes, and a setsid'd child outlives the AppImage there. A desktop
  that kills an app's whole scope on exit would take the daemon with it.
