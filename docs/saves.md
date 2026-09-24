# Save / Load sessions

Unfinished work used to be kept by asking a claude for "a runbook". Those files piled up in the repo,
and only a claude could read them. Save/Load makes this a Mulpex feature:

- **⌘S** saves one claude's work as a handoff doc in the repo, at `mulpex/saves/<slug>.md`. The doc
  gets a short, simple Hebrew title and description.
- **⌘L** lists the saves and starts a claude from one.

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

## Load (Phase 2)

- **⌘L / the palette** opens `LoadDialog.svelte`, which lists `saves.rs::list`: every `*.md` in the
  repo's `mulpex/saves/`, newest `updated` first.
  - A file with no readable header still shows up, titled by its file name. It is in the folder, so
    hiding it would be the list lying.
  - The list is hard `dir="rtl"`, never `auto`: that is the Explainer's rule, because a title that
    starts with an English term would flip `auto` to LTR.
- **Controls:** typing filters, ↑↓ chooses, Enter loads, 🗑 deletes after a native confirm.
- **Enter** runs `load_save`, which spawns a claude through `Core::spawn_instance_with_prompt`:
  - The prompt goes on **argv** (`SpawnSpec::Claude::plain_prompt`) exactly as written. There is no
    `[mulpex:hub]` wrapper, no whitespace collapsing, and no delivery watchdog: it is an ordinary
    first turn, which `UserPromptSubmit` captures like a typed one.
  - The prompt says: read the doc (by absolute path, since the cwd may be a subfolder), check the
    repo, report where things stand and the next step, and **wait for the user's go**.
  - The row is named after the save's title as a **user-owned** name (`manual_names`), so the
    instance's own `hub_set_name` can't replace it.
- **`file` comes from the webview**, so `saves::resolve` accepts only a plain `.md` name inside the
  saves dir: no `/`, no `\`, no leading `.`.

## Links, Continue, and re-save (Phase 3)

`<mulpex home>/save-links.tsv` stores the link between a save file and a conversation. It is
**never in the repo**, because a conversation id means nothing on a coworker's machine, and a debug
build keeps its own copy in `~/.mulpex-dev`.

- **The file:** append-only, one line per event: `kind \t uuid \t project dir \t save path`,
  with canonical paths.
  - `saved`: that conversation wrote the save.
  - `loaded`: a fresh claude was started on it, keyed by the uuid Mulpex minted.
- **⌘S on a linked conversation** updates that same file, found through its latest link of either
  kind whose file still exists.
  - The write step also gets the old doc (`UPDATE_NOTE`) and writes **one** current doc that keeps
    whatever is still true.
  - `created` is kept, `updated` is bumped, and `author` becomes the latest saver.
- **⌘L marks a save ↺** when the conversation that last **saved** it still has its `.jsonl` here, in
  this project. `resumable` ignores `loaded` links, because a loader may never have saved, so the
  doc can be newer than anything that conversation holds.
  - Enter then shows **Continue conversation / Start fresh from doc** in place (←→, Enter, Esc).
  - Continue spawns a new row with `--resume <uuid>` (`Core::spawn_instance_resuming`, marked
    `worked` + `restored` like a startup restore).
  - If that conversation is **already open**, the button reads "Go to claude #N" and focuses that
    row instead. Two claudes on one transcript would corrupt it.
- **Where the transcript is:** `transcript_path` builds Claude Code's own path:
  `$CLAUDE_CONFIG_DIR` (or `~/.claude`), then `/projects/`, then the canonical dir with every
  non-alphanumeric character turned into `-`, then `/<uuid>.jsonl`.
- **Known gap:** a loaded instance whose transcript later diverges from its minted uuid (see
  `reconcile_session_ids`) loses its link, and its next ⌘S writes a new file.
- **Saves from before Phase 3** have no links. They get one on their next ⌘S.

## Guides (Phase 4a)

These were called **playbooks** until 2026-09-24. They were renamed because the word needed
explaining, and the menu item became **Import Docs…** because it scans every `.md`, not only
runbooks.

Most old "runbooks" are not unfinished work. They are **recurring-incident guides** ("attach this
when a client reports X"): used again and again, never finished, and linked from code comments and
CLAUDE.md files. So a guide **never moves**:

- **The pointer:** Mulpex knows a guide through a committed pointer,
  `mulpex/guides/<slug>.md`. It is only a header: Hebrew `title` / `description`, and `source`,
  the runbook's path relative to the repo root. `source` is text in a committed file, so
  `source_path` accepts only normal components: nothing absolute and no `..`.
- **The ⌘L window** has two tabs, **Saves | Guides**. Tab switches between them.
  - A missing runbook still lists, marked "missing", and can't be loaded.
- **Loading one:** Enter runs `load_guide`, which spawns a claude on `guide_prompt`:
  - It reads the runbook, says in a line or two what it is for, and **asks what the user needs this
    time**. There is deliberately no "what happened?" box in the dialog: one was built and dropped
    as an extra step nobody used.
  - It then works read-only first, and asks before changing anything.
  - At the end it **suggests** an edit to the guide, or **suggests deleting** it (runbook +
    pointer) if it describes something that no longer exists. It never writes or deletes without
    asking.
  - The row starts named after the guide, **not** user-owned, so the instance may rename it after
    the specific incident.
- **🗑 on a guide retires it for good:** it deletes the runbook **and** the pointer. Git keeps the
  history, and Mulpex does not commit.

## Import Docs (Phase 4b)

**File ▸ Import Docs…** (also in the palette; no accelerator) converts a repo's existing
markdown into what ⌘L shows. The code is `src-tauri/src/docs_import.rs` and
`save_prompts/sort.md`. It runs in four steps:

1. **Scan:** every committed `.md` (`git ls-files -z`), minus:
   - README / CHANGELOG / CLAUDE / AGENTS / LICENSE / CONTRIBUTING;
   - any dot-folder (`.claude/`, `.github/`) and `node_modules/`;
   - Mulpex's own `mulpex/`;
   - runbooks that already have a guide pointer;
   - whatever the committed `mulpex/import-skip.txt` lists.
2. **Sort:** a headless **Sonnet**, 6 at a time and read-only, puts each file into one of four
   kinds: `save` (unfinished work), `guide` (a reusable guide), `stale` (describes something
   gone) or `skip`. It also writes the Hebrew title and description. It may check the repo, and
   that is what lets it call a runbook stale with evidence.
3. **Review:** the job lives in the backend per project, so the window can be closed mid-sort and
   reopened (`import-update {handle}` → `import_state`). Each row has a tick, a kind picker and
   editable Hebrew. **Apply** is enabled once sorting ends.
4. **Apply:** only ticked rows are applied. Each kind does something different:
   - **save:** the original body plus a Hebrew header goes to `mulpex/saves/`, and the original is
     deleted. Its dates and author come from git: first commit, last commit, last committer.
   - **guide:** a pointer is written, and the runbook stays where code links to it.
   - **stale:** the file is deleted.
   - **skip:** the path is appended to `mulpex/import-skip.txt`, so no later import asks again,
     yours or a coworker's.

**Apply makes one commit** in the target repo, `docs: import into Mulpex (N saves, N guides, N
deleted)`, whose body lists each file and what happened to it. That way the whole conversion can be
reviewed or reverted as one commit. It is the one place in the saves feature where Mulpex commits
(⌘S never does), on the user's request (2026-09-24):

- **Only the import's files:** new files are `git add`ed, then `git commit --only -- <paths>`. So
  anything else staged in that tree, the user's or another instance's, stays staged and out of the
  commit. The test proves this with a real temp repo.
- **Hooks run** like any commit. If one fails, the files are still changed, the commit isn't made,
  and the notice says why.

**Apply keeps links whole** (`fix_references`, added after the first real import left two dead
links in `cloud`). After the moves and deletes, every tracked file that names one of those docs is
checked, and the edits go into the same commit:

- **A link to a moved save** is re-pointed relative to the linking file, and a plain repo-relative
  mention of it (a code comment, say) is rewritten.
- **A link to a deleted stale doc:** if it sits in a table row or list item, that line is dropped,
  since it was an index entry for a doc that no longer exists. Inside a sentence the link becomes
  plain text.
- **Any other mention of a deleted doc** is left alone and listed in the notice (`leftovers`).
  Guessing in code or prose does more harm than a note.

This was replayed on `cloud`'s `production-debugging.md` as it stood right after that import. It
produced the same result as the hand fix: the row re-pointed, the row dropped, and nothing reported.

The sorter is also told never to open a title with a document-kind word (`מדריך`, `תיעוד`…). 18 of
`cloud`'s first 20 guide titles did, which only repeated the tab's name.

`apply_one` refuses any `file` that isn't a plain repo-relative path.

Measured on 6 real `cloud` files (2026-09-23), in about 2.5 min in parallel:

| File | Sorted as | Why |
| --- | --- | --- |
| `sophos-xg-auto-provisioning` | guide | the feature shipped |
| `disable-mfa-for-user` | **stale** | it checked the code: the bypass list the runbook relies on was replaced by an `mfa_exempt` field |
| `TODO.md` | save | it checked that both items are still open |
| `ai-chat-audit-rubric` | skip | |
| `pricing-api` | skip | |
| `rdns-coverage` | guide | |

## Measured (2026-09-23)

The full chain was run on real conversations with a Python probe, then through `saves.rs` itself
(`live_save`, an `#[ignore]` test).

- **Safe on a live instance.** Saving the very session that was running the probe left the old
  bytes of its `.jsonl` intact while the instance kept appending. No new transcript appeared.
- **Cost and time:** about 2.5 min and $2–4 per save. The CLI's `total_cost_usd` on a resumed fork
  **includes the original conversation's cost**, so read `modelUsage` for the real number.
- **The check step earns its cost.** On the first real save it found a crash path the draft never
  mentioned, and the fix step made it item #1 of "Left".
