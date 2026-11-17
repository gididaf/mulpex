# System Text Replacements (`--replace OLD NEW`)

Mulpex ▸ System Text Replacements… (`src-tauri/src/system_replace.rs`,
`SystemReplaceDialog.svelte`) adds `--replace OLD NEW` pairs to every **interactive** `claude`
Mulpex starts.

## What `--replace` is

It is **not a Claude Code flag**. It comes from a launcher a user can put in front of `claude`.
The launcher strips the pairs from argv, preloads a Bun hook that rewrites the `system` text of
each model request at the `fetch` boundary, and then execs the real binary.

- A plain `claude` rejects the flag and fails to start.
- So the feature is **off by default**, and when off it adds **zero** arguments.
  `system_replace::tests::off_adds_nothing` guards that, because other people use Mulpex with a
  plain `claude`.

## Behaviour

- **Settings.** One global list, saved as `<mulpex home>/system-replace.json`:
  `{enabled, rules: [{old, new, enabled}]}`.
  - Rules run in order and are literal; an empty NEW deletes.
  - An empty OLD is refused on save.
  - The settings are host-scoped like Live Token: on a remote host's tab, the dialog reads and
    writes that host's file.
- **Which launches.** Only the Claude arm of `pty.rs::Session::spawn`, so: ⌘T, restore
  (`--resume`), `hub_spawn` children, ⌘⇧R and ⌘L.
  - The pairs go right after the binary.
  - Headless `-p` runs (save, explain, ⌘⇧B summary, handoff, Import Docs) never get them.
  - Codex never gets them.
- **When it takes effect.** On the next launch. A running claude keeps the rules it started with,
  and nothing marks it ↻ outdated.
- **Failure.** Mulpex does not probe at launch. If the launcher refuses, the row shows "failed to
  start" with the launcher's own message.
  - The launcher refuses, for example, when its pinned Claude Code hash no longer matches after
  an update.
  - The dialog's **Test** button runs `<claude> <the unsaved pairs> plugin list`. With no ticked
    pair it uses a dummy pair.
  - It is not `--version` or `--help`. A plain `claude` answers those before parsing options, so
    it would "accept" `--replace`. `plugin list` parses first, so a plain `claude` exits 1 with
    `unknown option '--replace'`. It is read-only and takes ~2 s through the launcher. (Measured
    on 2.1.326.)
  - The Test run has no tty, so on a hash mismatch the launcher prints its manual-recovery hint
    instead of offering an interactive repair.
- **argv.** The pairs are on argv, so they show in `ps` and every word is a `pkill -f` target.
  This is the same trade-off as spawn tasks (see the root CLAUDE.md).

## Measured: editing the built-in prompt can 400 (2026-11-17, Claude Code 2.1.326)

This was measured with the launcher and real claudes. Two things decide whether a rule works:
where OLD sits, and how claude reaches the API.

| OLD is in | Direct to api.anthropic.com | Through a base URL (`ANTHROPIC_BASE_URL`, e.g. Live Token) |
| --- | --- | --- |
| Mulpex's `--append-system-prompt` (HUB_RULES) | works (witness answered) | works |
| Claude Code's built-in identity line (`You are Claude Code, …`) | **API Error 400** `cache_control.scope: "global"` … | works |

The 400 is not caused by Mulpex. A plain `claude --replace …` in tmux, with no Mulpex flags,
returns the same 400 once `ANTHROPIC_BASE_URL` is unset.

- **Why direct differs.** Talking to the API directly, Claude Code sends the built-in prompt with
  the static part as a separate block marked `cache_control.scope: "global"`. The identity block
  in front of it carries no `cache_control` at all.
- **The guess (not measured).** The server appears to accept that only while the earlier blocks
  are the canonical text, so editing them is rejected.
- **Through a base URL.** No block is global-scoped, so the edit goes through.

This is the launcher's limitation. Mulpex passes the pairs exactly as configured
(`/proc/<pid>/cmdline` checked: order kept, unticked rules skipped, empty NEW kept, Hebrew intact).
