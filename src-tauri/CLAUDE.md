# Tauri backend

Root rules: [../CLAUDE.md](../CLAUDE.md).

| You are editing | Read first |
| --- | --- |
| `pty.rs` (spawn, geometry, task delivery, `kill`), `claude_bin.rs` | [../docs/sessions.md](../docs/sessions.md) — binary resolution, login env forwarding, status words; [../docs/rendering.md](../docs/rendering.md) — one geometry; [../docs/shell-terminals.md](../docs/shell-terminals.md) — the tty sweep behind a terminal's `running`/`cwd` |
| `state.rs` (`Core`, `Workspace`, `reap_dead`, poll-loop handshakes) | [../docs/sessions.md](../docs/sessions.md) — kept-failed instances, stable ids, `sticky` restores; [../docs/hub.md](../docs/hub.md) — spawn/name/term request fulfilment |
| `vtgrid.rs`, the `Recorder`, `SessionKind` | [../docs/shell-terminals.md](../docs/shell-terminals.md) |
| the remote-peer watcher | [../docs/remote-peers.md](../docs/remote-peers.md) |
| `explainer.rs` (the `explainreq/<id>` drain via `submit`, turn extraction, pending questions + plans, the headless Sonnet child, the worker queue, the 10-entry feed, failure reason + retry) | [../docs/explainer.md](../docs/explainer.md) — the transcript-flush race and the `dialog` marker, why not `--bare`, the three-part shape, no-silent-skip rule, and why a retry re-runs the *stashed* input rather than the transcript |
| `saves.rs`, `save_prompts/*` (⌘S: fork → memoryless check → fork fix, read-only tools, writes `mulpex/saves/` at the git root) | [../docs/saves.md](../docs/saves.md) — why the doc must stand alone, why the forks run in the project dir, why `total_cost_usd` over-reports |
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
- **Don't run `cargo fmt` here.** This tree is not rustfmt-default-clean and there is no
  `rustfmt.toml`; one run (2026-09-03) reformatted five files nobody had touched — `state.rs`,
  `pty.rs`, `vtgrid.rs`, `claude_bin.rs`, `menu.rs` — and re-wrapped hand-written single-line
  struct literals, burying a small change in ~700 lines of noise. Match the surrounding style by
  hand instead. → [../docs/verification-log.md](../docs/verification-log.md)
- **A child process's failure reason may be on stdout.** `claude -p` exits 1 and prints
  `API Error: 401 …` on **stdout** with an empty stderr (measured). Reading stderr alone is how the
  Explainer shipped a failure entry that said only `exit 1`. → [../docs/explainer.md](../docs/explainer.md)
- **Never `wait()` a terminal's child to learn it exited.** Liveness is reader-thread EOF; a zombie
  keeps the pid unrecyclable, which is what makes the `killpg` in teardown safe.
- **`Session::kill` cannot reach a `claude`'s background commands.** Claude Code runs each in its
  own process group with no controlling terminal (measured: `PGID == pid`, `SESS 0`, `Ss`), so both
  the `killpg` and `kill_tty_session` miss it. The hub listener is the one that matters — it spins
  forever — and it is handled from the other end: `pty.rs` publishes `pids/<id>` so `listen.rs` can
  see its owner die, and `pty::reap_orphaned_listeners` (launch + teardown) kills the ones that
  predate that. Anything else long-lived a child backgrounds has the same hole.
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
