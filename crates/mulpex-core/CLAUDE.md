# mulpex-core (the headless lib the helper links)

Root rules: [../../CLAUDE.md](../../CLAUDE.md). This crate is what a child `claude` process actually
executes — `hook <event>` on every tool call and turn boundary, and a long-lived `mcp` server. It
runs in a **different process from Tauri** and shares state with it only through files under
`$MULPEX_STATE_DIR`, which is why nearly everything here is a file handshake.

| Module | Read first |
| --- | --- |
| `hook.rs`, `config.rs` | [../../docs/sessions.md](../../docs/sessions.md) — what each status word means and why `needs` must mean "needs YOU"; [../../docs/hub.md](../../docs/hub.md) — listener arming and naming nudges |
| `mcp.rs` (`hub_send`/`hub_spawn`/`hub_set_name`) | [../../docs/hub.md](../../docs/hub.md) |
| `mcp.rs` (`hub_terminal_*`), `termlog.rs` | [../../docs/shell-terminals.md](../../docs/shell-terminals.md) |
| `registry.rs` | [../../docs/hub.md](../../docs/hub.md) — the `<project>#<n>` grammar and its ordered parser |
| `remote.rs` | [../../docs/remote-peers.md](../../docs/remote-peers.md) |
| `persist.rs` | [../../docs/sessions.md](../../docs/sessions.md) — the store's positional columns |

Traps that live in this crate specifically:

- **A bare integer filename at the state-dir root is scanned as an instance status file**
  (`mcp::live_ids`). Any new per-instance flag goes in a subdir — `bg/`, `compacting/`, `armed/`,
  `named/`, `namenudge/`, `spawning/`, like `peers/` already does.
- **`persist.rs`'s store columns are positional** (`<uuid>[\t<name>[\tmuted[\t<id>]]]`). Only
  *trailing* empties may be dropped, or the id is read back as the name.
- **Don't report a default as a fact.** `status_of` returns `waiting` for a *missing* file, and that
  ambiguity once made a 91 s spawn stall indistinguishable from a lost task.
  → [../../docs/hub.md](../../docs/hub.md)
- **`HUB_RULES`/`config.rs` templates are `--append-system-prompt` text**, re-sent every turn, so
  they survive compaction — that is why contracts with instances live there and not in an injected
  prompt. Anything whose grammar is also parsed in code (the remote `<<<MPX …>>>` marker) has a test
  asserting the rules' own example parses, so the two halves cannot drift.
