# Tauri backend

Root rules: [../CLAUDE.md](../CLAUDE.md).

| You are editing | Read first |
| --- | --- |
| `pty.rs` (spawn, geometry, injection, `kill`), `claude_bin.rs` | [../docs/sessions.md](../docs/sessions.md) — binary resolution, login env forwarding, status words; [../docs/rendering.md](../docs/rendering.md) — one geometry; [../docs/shell-terminals.md](../docs/shell-terminals.md) — the tty sweep behind a terminal's `running`/`cwd` |
| `state.rs` (`Core`, `Workspace`, `reap_dead`, poll-loop handshakes) | [../docs/sessions.md](../docs/sessions.md) — kept-failed instances, stable ids, `sticky` restores; [../docs/hub.md](../docs/hub.md) — spawn/name/term request fulfilment |
| `vtgrid.rs`, the `Recorder`, `SessionKind` | [../docs/shell-terminals.md](../docs/shell-terminals.md) |
| the remote-peer watcher | [../docs/remote-peers.md](../docs/remote-peers.md) |
| `explainer.rs` (turn extraction, pending questions + plans, the headless Sonnet child, the worker queue) | [../docs/explainer.md](../docs/explainer.md) — the transcript-flush race, why not `--bare`, one-sentence plans, no-silent-skip rule |
| `menu.rs`, `lib.rs` menu dispatch | **Keyboard** in [../CLAUDE.md](../CLAUDE.md) |
| `lib.rs` `RunEvent`, `tauri.conf.json`, `Info.plist` | [../docs/packaging.md](../docs/packaging.md) |

Traps that live in this directory specifically:

- **`lib.rs::is_forwarded` is an allowlist.** A menu item not listed there builds, appears, shows its
  accelerator and even ticks itself, while the frontend never hears a thing. Same for
  `capabilities/default.json` and the notification plugin.
- **Teardown matches BOTH `RunEvent::ExitRequested` and `RunEvent::Exit`.** ⌘Q and an Apple-Event
  quit only reach the second; dropping that arm re-opens a measured scratch-root leak.
  → [../docs/packaging.md](../docs/packaging.md)
- **`reap_dead`'s early return tests *removability*, not liveness**, and the failure mark must latch
  — otherwise the body's two disk writes run on every 200 ms tick forever.
  → [../docs/sessions.md](../docs/sessions.md)
- **Never `wait()` a terminal's child to learn it exited.** Liveness is reader-thread EOF; a zombie
  keeps the pid unrecyclable, which is what makes the `killpg` in teardown safe.
- **A spawned child must not inherit `MULPEX_*`, `CLAUDE_CODE_CHILD_SESSION` or
  `CLAUDE_CODE_ENTRYPOINT`.** The first corrupts the hub, the second silently disables transcript
  saving and only shows up at the *next* launch.
- **Anything written into `state_dir` *once* has a three-day fuse.** The scratch root is in
  `$TMPDIR`, which macOS purges of untouched files every night, and `settings.json` / `mcp.json`
  being the only write-once files in the tree is exactly how a Mulpex left open over a long weekend
  lost the two files `claude --settings` needs — while every running instance carried on, so
  nothing looked wrong until the next ⌘T. New scratch state goes in `state.rs::write_state_dir`,
  which every spawn re-runs. → [../docs/sessions.md](../docs/sessions.md)
