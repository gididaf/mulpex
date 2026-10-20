# Tauri backend

Root rules: [../CLAUDE.md](../CLAUDE.md).

| You are editing | Read first |
| --- | --- |
| `pty.rs` (spawn, geometry, task delivery, `kill`), `claude_bin.rs` | [../docs/sessions.md](../docs/sessions.md) — binary resolution, login env forwarding, status words; [../docs/rendering.md](../docs/rendering.md) — one geometry; [../docs/shell-terminals.md](../docs/shell-terminals.md) — the tty sweep behind a terminal's `running`/`cwd` (now in `crates/mulpex-ptyd/src/procs.rs`) |
| `state.rs` (`Core`, `Workspace`, `reap_dead`, poll-loop handshakes) | [../docs/sessions.md](../docs/sessions.md) — kept-failed instances, stable ids, `sticky` restores; [../docs/hub.md](../docs/hub.md) — spawn/name/term request fulfilment |
| `ptyd.rs`, `pty.rs` `Session`/`OutputSink`/`compose`, `Core::open` adoption, `lib.rs` `teardown`, `restart_app`, `crates/mulpex-ptyd/*` | [../docs/ptyd.md](../docs/ptyd.md) — the daemon owns every PTY; quit vs detach; why a `Session` drop must not kill during a detach; daemon changes ship late |
| `SessionKind`; `crates/mulpex-ptyd/src/vtgrid.rs` (the `Recorder`, now run by the daemon) | [../docs/shell-terminals.md](../docs/shell-terminals.md) |
| the remote-peer watcher | [../docs/remote-peers.md](../docs/remote-peers.md) |
| `saves.rs`, `docs_import.rs`, `save_prompts/*` (⌘S: fork → memoryless check → fork fix, read-only tools, writes `mulpex/saves/` at the git root; guides; the links file; Import Docs' sort / apply / one commit / link fixing) | [../docs/saves.md](../docs/saves.md) — why the doc must stand alone, why the forks run in the project dir, why `total_cost_usd` over-reports |
| `remote/*` (Remote Control: relay connection, pairing/E2E crypto, transcript chat, dialog keys, Web Push), `pty.rs` `OutputSink::tap` | [../docs/remote-control.md](../docs/remote-control.md) — the security model, why typing goes through the desktop, the measured dialog keys |
| `remote/client.rs`, `remote/desktop.rs`, `remote/rpc.rs`, `remote_update.rs`, `wire.rs` (Remote Hosts: driving another Mulpex / being driven), `pty.rs` `OutputSink::desktop_tap`, `Workspace::geometry_owner` | [../docs/remote-hosts.md](../docs/remote-hosts.md) — handles, mirror snapshots at an exact stream offset, the geometry lease |
| `explain.rs`, `explain_prompt.md` (⌘⇧E: a Sonnet `--fork-session` explains a selection; follow-ups `--resume` that fork; Mulpex deletes it) | [../docs/explain.md](../docs/explain.md) — why follow-ups resume instead of re-fork (measured cache cost), why it is gated on the transcript file |
| `secrets.rs` (⌘K) | [../docs/secrets.md](../docs/secrets.md) |
| `live_token.rs` (Mulpex ▸ Live Token…: settings + named tokens + pins for the daemon's token-swapping proxy), its env in `pty.rs`/`saves.rs`/`explain.rs`/`broadcast.rs` | [../docs/live-token.md](../docs/live-token.md) — why `ANTHROPIC_BASE_URL` and not the unix socket, why the env token stays real, per-claude routes/pins |
| `broadcast.rs`, `broadcast_prompt.md` (⌘⇧B Ask All Claudes) | [../docs/hub.md](../docs/hub.md) |
| `pins.rs` (⌘⇧P) | [../docs/frontend.md](../docs/frontend.md) |
| `todos.rs` (To Do: `hub_todo`, drafts, UPDATE) | [../docs/hub.md](../docs/hub.md) — single writer, delivery gate, message body; [../docs/frontend.md](../docs/frontend.md) — drafts and conflicts. No field may be named `handle` (`client.rs::lift`) |
| `menu.rs`, `lib.rs` menu dispatch | **Keyboard** in [../CLAUDE.md](../CLAUDE.md); a new shortcut also goes in `src/lib/linuxKeys.ts`, a new item in `src/lib/components/MenuBar.svelte` → [../docs/linux.md](../docs/linux.md) |
| `crates/mulpex-ptyd/src/procs.rs` process sweeps (`/proc` on Linux), `tauri.linux.conf.json`, `capabilities/linux.json`, `scripts/release-linux.sh` | [../docs/linux.md](../docs/linux.md) |
| `lib.rs` `RunEvent`, `tauri.conf.json`, `Info.plist` | [../docs/packaging.md](../docs/packaging.md) |

Traps that live in this directory specifically:

- **A new `#[tauri::command]` must be classified in `remote/rpc.rs`** — `ALLOWED` (and dispatched
  there) or `DENIED` — or `every_command_is_classified` fails; that test is the only thing standing
  between a new feature and a dead button on every remote host's tab.

- **A code edit in a wire file forces every remote host to update** with the next release
  (`wire.rs::FILES`: `remote/rpc.rs`, `remote/desktop.rs`, `remote/client.rs`, `snapshot.rs`,
  `remote_update.rs`, `commands.rs`; comment-only edits don't count). Changing a serialized shape
  a remote desktop reads that lives *outside* those files is not seen — touch a wire file in the
  same release. → [../docs/remote-hosts.md](../docs/remote-hosts.md)
- **`lib.rs::is_forwarded` is an allowlist.** A menu item not listed there builds, appears, shows its
  accelerator and even ticks itself, while the frontend never hears a thing. Same for
  `capabilities/default.json` and the notification plugin.
- **Teardown matches BOTH `RunEvent::ExitRequested` and `RunEvent::Exit`.** ⌘Q and an Apple-Event
  quit only reach the second; dropping that arm re-opens a measured scratch-root leak.
  → [../docs/packaging.md](../docs/packaging.md)
- **`reap_dead`'s early return tests *removability*, not liveness**, and the failure mark must latch
  — otherwise the body's two disk writes run on every 200 ms tick forever.
  → [../docs/sessions.md](../docs/sessions.md)
- **Don't run `cargo fmt` here.** This tree is not rustfmt-default-clean and there is no
  `rustfmt.toml`; one run (2026-09-03) reformatted five files nobody had touched — `state.rs`,
  `pty.rs`, `vtgrid.rs`, `claude_bin.rs`, `menu.rs` — and re-wrapped hand-written single-line
  struct literals, burying a small change in ~700 lines of noise. Match the surrounding style by
  hand instead. → [../docs/verification-log.md](../docs/verification-log.md)
- **A child process's failure reason may be on stdout.** `claude -p` exits 1 and prints
  `API Error: 401 …` on **stdout** with an empty stderr (measured). Reading stderr alone is how the
  (since removed) Explainer shipped a failure entry that said only `exit 1`. `saves.rs::failure_reason`
  is the one reader now.
- **Never `wait()` a terminal's child to learn it exited.** Liveness is reader-thread EOF; a zombie
  keeps the pid unrecyclable, which is what makes the `killpg` in teardown safe.
- **A `claude`'s background commands are reachable only while the `claude` is alive.**
  - **Why:** Claude Code runs each one as a direct child in its own process group, with no
    controlling terminal (measured: `PGID == pid`, `SESS 0`, `Ss`). So the `killpg` and
    `kill_tty_session` both miss it.
  - **The fix:** since 2026-10-07, a kill (now the daemon's `DSession::kill`) first runs `kill_child_groups` and kills the
    group of every direct child of the `claude`. That covers ⌘W, ⌘S, ⌘⇧R and quit.
  - **What it can't catch:** a `claude` that dies on its own (a crash, `/exit`). Its children are
    reparented to launchd and can no longer be told apart. The hub listener therefore still has to
    notice its owner die by itself (`pids/<id>` → `listen.rs`), and
    `pty::reap_orphaned_listeners` (launch + teardown) stays as the reaper for orphans.
  → [../docs/hub.md](../docs/hub.md)
- **A task is an argv argument, not keystrokes** — for a spawned child here and for a remote peer
  in `mcp.rs`. Typing it into the TUI capped it at 1022 characters with no error anywhere. If you
  need to hand an ALREADY-RUNNING instance text, `hub_send` it — that path is a file the
  recipient's MCP server reads, and has never truncated.
  → [../docs/hub.md](../docs/hub.md), [../docs/remote-peers.md](../docs/remote-peers.md)
- **A spawned child must not inherit `MULPEX_*`, `CLAUDE_CODE_CHILD_SESSION` or
  `CLAUDE_CODE_ENTRYPOINT`.** The first corrupts the hub, the second silently disables transcript
  saving and only shows up at the *next* launch.
- **Anything written into `state_dir` *once* has a three-day fuse.** The scratch root is in
  `$TMPDIR`, which macOS purges of untouched files every night, and `settings.json` / `mcp.json`
  being the only write-once files in the tree is exactly how a Mulpex left open over a long weekend
  lost the two files `claude --settings` needs — while every running instance carried on, so
  nothing looked wrong until the next ⌘T. New scratch state goes in `mulpex-core`'s
  `state_dir.rs::write_state_dir` — it lives there so the helper reads back the same layout the
  app writes — which every spawn re-runs. → [../docs/sessions.md](../docs/sessions.md)
