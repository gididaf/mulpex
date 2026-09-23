# Save / Load sessions

Unfinished work used to be kept by asking a claude for "a runbook". Those files piled up in the repo,
and only a claude could read them. Save/Load makes this a Mulpex feature:

- **⌘S** saves one claude's work as a handoff doc in the repo, at `mulpex/saves/<slug>.md`. The doc
  gets a short, simple Hebrew title and description.
- **⌘L** lists the saves and starts a claude from one. Not built yet; Phase 2.

The files are ordinary repo files, and Mulpex never commits them. You commit them like any other
work, and that is how coworkers get them.

Code: `src-tauri/src/saves.rs`, prompts in `src-tauri/src/save_prompts/`.

## The doc must stand alone

Claude Code deletes conversations after 30 days (`cleanupPeriodDays`), and a coworker never had the
conversation at all. So nothing may assume the transcript survives: **Load starts a fresh claude from
the doc**. Resuming the original conversation (Phase 3) is only a local bonus.

## How a save is written

A save runs three headless `claude -p` steps, all on Opus, all read-only:

1. **write**: a hidden **fork of the instance's own conversation** (`--resume <uuid>
   --fork-session`) writes the doc. It has the whole conversation in context, so the doc is as good
   as asking the instance itself, and nothing is typed into its TUI (see the argv rule in CLAUDE.md).
2. **check**: a claude with **no memory** gets only the doc plus the repo, and lists what it could
   not continue without.
3. **fix**: a second fork answers those gaps. It is skipped when the checker says `NONE`.

The steps may use `Read`, `Grep` and `Glob`, plus `git log/status/diff/show/branch`, and nothing
else. Mulpex writes the file itself. Other details:

- **Where they run:** every step runs with cwd = the project dir, because `--resume` finds a
  conversation by its cwd. The file goes to the **git top-level**, so a tab opened on a subfolder
  still uses its repo's single `mulpex/saves/`.
- **Environment:** scrubbed exactly like the Explainer's child (`claude_bin::forwarded_env`), and
  like it, no `--bare`.
- **`--no-session-persistence`:** so no fork leaves a transcript behind.
- **`--output-format json`:** the answer is read from `result`, and `is_error: true` counts as a
  failure even on exit 0.

The file's header holds the `title` and `description` (Hebrew), `author` (git `user.name`), and
`created` / `updated`. Header values are JSON strings, which are valid YAML and safe for any text. A
taken slug gets `-2`, `-3`…; a save never overwrites. The body is English and written for a claude,
with these sections: Goal / Status / Context / Done / Left / Decisions / Traps / How to verify /
Open questions. It says "the user", never a name, because coworkers read it too.

The Hebrew follows the Explainer's rules: simple everyday Hebrew, a title of at most 6 words and a
description of one sentence of at most 15 words. An English term stays in English exactly as
written, never transliterated.

## UI (Phase 1)

- **⌘S / right-click ▸ Save… / the palette:** a native confirm. It is allowed mid-turn, but the
  confirm then warns that the save may miss the step in progress.
- **The instance stays usable.** Its sidebar row shows `saving… writing / checking / fixing`, then
  `saved ✓`, which fades after 10 s.
- **A failure** shows the reason on the row, with **Retry** (no second confirm) and **✕**.
- **A second ⌘S on the same instance** is refused while a save is running.

## Measured (2026-09-23)

The full chain was run on real conversations with a Python probe, then through `saves.rs` itself
(`live_save`, an `#[ignore]` test).

- **Safe on a live instance.** Saving the very session that was running the probe left the old
  bytes of its `.jsonl` intact while the instance kept appending. No new transcript appeared.
- **Cost and time:** about 2.5 min and $2–4 per save. The CLI's `total_cost_usd` on a resumed fork
  **includes the original conversation's cost**, so read `modelUsage` for the real number.
- **The check step earns its cost.** On the first real save it found a crash path the draft never
  mentioned, and the fix step made it item #1 of "Left".
