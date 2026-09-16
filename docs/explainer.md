# The Explainer — the Hebrew ⌘⇧E panel

A right-hand column, **closed by default**, that opens on ⌘⇧E and answers one question: what is the
focused claude saying *right now*? A headless Sonnet call produces a very short, very simple
**Hebrew** explanation of the turn on screen (English identifiers kept verbatim), and the panel
shows that one explanation and nothing else. Built 2026-08-30 as an after-every-turn feed; made
on-demand 2026-09-16. Every claim below marked *measured* was driven on a real session or
transcript.

## The pipeline

```
⌘⇧E
  → App.svelte::toggleExplainer → commands::explain_now(handle, id)
  → explainer::transcript_path(project_dir, session_uuid)   ← the file, found on disk
  → stat it: unchanged since this instance's answer? → Verdict::Cached, stop here
  → explainer.rs worker queue (2 threads, latest-wins per instance)
      read_turn: a waiting AskUserQuestion / ExitPlanMode wins, else the turn's text
      → claude -p --model sonnet
  → in-memory store, ONE entry per (handle,id) + the TranscriptKey it was made from
  → `explain-update {handle, id, entry}` event → stores.ts::applyExplainFor → ExplainerPanel
```

**Nothing runs by itself.** There is no hook, no request file and no poll handshake behind this —
the `Stop`/`askq`/`plan` hooks no longer write anything for the Explainer, the `explainreq/`,
`explainq/` and `explainplan/` scratch dirs are gone, and `hub.rs`'s poll loop does no Explainer
work per tick (only `forget` when a row is reaped). A Sonnet call happens when, and only when, the
user asks for one.

## Finding the transcript without a hook

`transcript_path(project_dir, session_uuid)`. Claude Code stores a session as
`<claude home>/projects/<slug of cwd>/<session uuid>.jsonl`, and **keeps appending to that same file
across `--resume`** — measured 2026-09-16 on a live 6787-entry transcript spanning two days, whose
uuid is the one sitting in Mulpex's own session store. Mulpex generated that uuid and handed it to
`claude`, so it can simply open the file.

- The **slug is Claude Code's rule, not ours** (every non-alphanumeric byte → `-`, measured over 112
  real project dirs), so it is a fast path only: a uuid that is not where the slug says is looked up
  across the project dirs, since a uuid is unique by construction. A rule change on their side then
  costs one directory scan, on a keypress, rather than the feature. *Measured 2026-09-16: of every
  live session in the store, each one that had a transcript at all was exactly where the slug said.*
- `CLAUDE_CONFIG_DIR` is honoured, `~/.claude` otherwise.
- This is why the Stop-hook bookmark could be deleted outright rather than kept "just in case": the
  authoritative source and the derived one agree, and one of them costs a file write per turn.

## One entry, cached, and the one thing that clears it

`MAX_ENTRIES` is **1**. The panel is about the turn on screen, so there is no feed, no timestamps,
no scrollback and no dimming-of-history — the claude transcript two inches away is the history.

**Closing the panel does not throw the answer away.** It used to, which made open/close/open cost
three Sonnet calls for three identical answers. Now the answer is kept and `explain_now` decides on
each press whether it still applies, returning a `Verdict`:

| Verdict | When | The panel |
| --- | --- | --- |
| `cached` | the instance's transcript has not moved since its answer was made | shows it; nothing runs |
| `running` | it moved, or there is no answer | drops what it has and waits for `explain-update` |
| `unavailable` | a terminal, a gone instance, or no transcript yet | says so, rather than sitting on a busy dot |

The key is a **`stat`** — `TranscriptKey { path, len, mtime }` — not a hash of the parsed input. The
check runs on the keypress with the user waiting and transcripts here reach 60 MB, and a stat errs
in the safe direction: any byte appended counts as a change and re-runs. The key is captured by the
*worker*, from the file it actually read, not by the keypress — `read_turn_settled` can spend a
second waiting out the flush race, and an entry stamped with the earlier file would let the next
press reuse an answer for content it never saw.

So there are three ways the panel goes away and only one that discards an answer:

| Trigger | Panel | Answer |
| --- | --- | --- |
| ⌘⇧E again, or ✕ | closes | **kept** — re-opening is free |
| Focus moves to another row or project | closes | **kept**, per row. Watched as `(activeProjectHandle, activeId)` in one effect, which covers the sidebar, ⌘[ / ⌘], a project switch and ⌘W at once. |
| The focused claude goes back to `working` | closes | **discarded** (`clear_explain`, which also cancels a job in flight). The user sent the next prompt, or answered the question that was on screen; the answer describes the turn before it. Only the transition **into** `working` counts — `working` → `working` is the same turn's next tool call. |

That last row is why the status rule survives the cache rather than being made redundant by it: the
two catch different things. The status rule covers the *focused* row the instant a new turn starts,
including the window where the transcript has not been flushed yet; the key covers everything the
frontend never saw — most obviously a background row that finished two more turns while the user was
elsewhere, whose working transition no effect ever observed.

**There is deliberately no refresh button.** If nothing changed, a second call could only produce the
same three lines; the user chose the cache over a ↻. A failed entry is the exception and already has
its own `נסה שוב`.

`explain_now` drops the stale entry *before* enqueueing but leaves the **queue** alone: `enqueue`'s
latest-wins replaces a not-yet-running job in place, which is what keeps the busy count honest when
someone leans on the key. The frontend holds the panel's content for the one frame the verdict takes
(`explainDeciding`) rather than flashing an explanation it is about to discard.

## A turn with nothing in it yet

⌘⇧E mid-turn, before the claude has written any prose, produces the entry `עדיין אין מה להסביר` —
no Sonnet call, because a call on nothing invents something. It is an entry and not a silent skip on
purpose: a silent skip here is exactly what the feature's most expensive bug looked like from the
outside (see the flush race below). Chosen by the user over summarizing tool activity, and over
silently falling back to the previous turn.

## Questions and plans are read out of the transcript

`read_turn` checks for a waiting dialog **first**, and it wins over the prose that came before it in
the same turn: a claude blocking on the user is the case the panel is most worth opening for.

- A `tool_use` named `AskUserQuestion` or `ExitPlanMode` with **no `tool_result` for its id** is the
  current turn. An answered one is ordinary history — the question the user already dismissed is not
  what ⌘⇧E is asking about.
- Both payloads are already in the transcript while the dialog is on screen. *Measured 2026-09-16:
  the assistant entry carrying the `questions` array is written before the tool runs.* This is what
  lets one keypress cover all three cases from one file, and what let the `askq`/`plan` hooks stop
  writing anything for the Explainer. Those hooks still write the **`needs` status word**, which is
  the one job they must never lose (`a_waiting_dialog_still_writes_the_needs_status_word`).

### A pending question is the same three parts (2026-09-16)

The question does not get a shape of its own: it *is* what `NEED` holds, with the options as rows
under it, so the panel never changes shape. What changed is what the summarizer is given.

**The bug this fixed:** a pending question made `read_turn` return the question payload **and throw
the turn's prose away**. The panel therefore showed a claude who had done nothing and immediately
started asking — reported from a real screenshot, and visible in the code as `question_text(&json)`
returned on its own. The turn's text now rides in front of the question, and `QUESTION_PROMPT` names
`DID` twice: do it, and what to write when there is genuinely nothing (`עוד לא עשיתי כלום בסבב הזה`).
What he found before stopping is usually the reason the question exists at all. Guarded by
`a_waiting_dialog_outranks_the_prose_before_it`, which asserts both that the work is there and that
it reads *before* the question.

`NEED` is the one multi-line part: a short line saying what to decide, then one row per option —
label, dash, a few plain words, capped at 12. `(Recommended)` is rendered as `(מומלץ)`; the rule has
to say so, because "only mark what is explicitly marked" left the English marker sitting in a Hebrew
line (measured). With several questions, each gets its own header row and its own options.

That multi-line shape is why `parse_sections` distinguishes a **wrapped sentence** from a **new row**:
only a plain line directly under another plain line is a wrap. A list item, the line *after* a list
item, and anything after a blank line each start a row. Measured on real three-question output —
without the "after a list item" rule, question 2's header was glued onto the tail of question 1's
last option. `.text` is `white-space: pre-wrap`, so the newline is what puts the rows on screen.

*Measured end to end on two real questions from this project's own transcript, through the real
summarizer with the real flags:* three questions with nine options came back as one tight block,
first person, `DID` describing the actual work of the turn, `(מומלץ)` on exactly the options the
input marked.

### The plan case is the same three parts too (2026-09-16)

A plan gets the identical shape: `NEED` is "I'm waiting for your go-ahead" plus **at most four rows**
of what he intends to do, in the layout a question's options already use. The panel therefore reads
the same way whatever it is showing, and `process` parses sections for all three kinds.

It is still **not a copy of the plan** — a plan is long, structured and technical by construction
(headings, absolute paths, code fences) and reproducing that here would defeat the panel. The rows
carry the big moves in plain Hebrew, no file names, no commands, small steps merged.

This replaces the original one-sentence `PLAN_PROMPT`. That call was made when the sentence *was*
the whole panel; with `WORK` and `DID` now carrying the goal and the research, the user chose the
rows.

`DID` carries the prose before the plan, the same fix and for the same reason as the question case:
the research is what justifies the plan, and without it a plan turn reads as pure intent.

*Measured on two real plans from this machine's transcripts (a 26 KB monorepo plan and a warweb
CLAUDE.md-shrinking plan), through the real summarizer:* four rows each, plain Hebrew, first person,
`DID` naming what was actually checked.

Measured on a real `claude` v2.1.252 on a PTY, 2026-09-01 (`scratchpad/probe`):

- **`tool_input` is `{plan, planFilePath}`** — `plan` is the plan as markdown. `planFilePath`
  points at the same text under `~/.claude/plans/`; it is deliberately ignored (one source, and
  no file read on this path).
- **The plan is head-capped at 12 KB** (`MAX_PLAN_BYTES`), the opposite end from a turn's
  tail-cap: a plan opens with its goal and descends into detail, and only the opening matters
  to a short summary.
- **`Stop` does not fire while the plan waits.** That used to matter for dedup; now it only means
  the transcript sits still while you read, which is exactly what ⌘⇧E wants.

### Plan mode is not reachable the way you would guess

`--dangerously-skip-permissions` (which every Mulpex claude runs with) **silently overrides**
`--permission-mode plan`: every hook payload still reads `permission_mode: bypassPermissions`,
no `ExitPlanMode` is ever called, and Claude just writes a plan as prose. Plan mode is reached
**only by shift+tab**, four presses from bypass — measured cycle: bypass → auto → manual →
accept edits → **plan**. Two probe runs found nothing before this was understood; if you are
driving a plan on a PTY, this is the trap.

## Why a dedicated event, not a HubSnapshot field

`HubSnapshot` is rebuilt and PartialEq-compared for every project every 200ms, and a Hebrew
paragraph in it would inflate every idle compare. Summaries also arrive asynchronously, seconds
after the keypress — the worker already knows exactly what changed, so it emits `explain-update`
itself. There is **no initial-paint command**: an explanation only exists between a ⌘⇧E and the next
prompt, so a fresh frontend has nothing to fetch, and `get_explains` was deleted with the rest of
the automatic path.

A second event, **`explain-pending {handle, id, active}`**, drives the panel's busy indicator
(pulsing amber dot + "מסביר…" line while a summary is being produced for the focused instance).
It fires on 0↔1+ transitions of a per-instance job **count** — a count, not a bool, because a
running job plus a freshly queued one must not go idle when only the first finishes. Every path
through a job (summary, failure entry, the `NOTHING_YET` entry) decrements it; `forget`/`clear`
clears it, and a late finish after that stays silent. Not persisted — after a dev hot-reload an
in-flight job just shows no dot, which is the acceptable direction of error.

## The turn-end race (the expensive lesson)

**Claude Code appends the turn's final assistant entry to the transcript a beat *after* the turn
visibly ends.** Measured: final text entry at 11:55:15.346, the turn's own end inside the same
second. So a ⌘⇧E pressed the instant the claude stops can read a transcript whose last word has not
arrived. The first implementation skipped an empty extraction *silently*, which presented as "the
whole feature doesn't work" and cost a debugging round — the classic "a real event with nowhere to
arrive". Two rules came out of it, and they outlived the Stop hook that caused them:

- `read_turn_settled` tries **immediately** — a keypress is waiting on it — and retries only while
  the turn reads as empty (250ms × 4, ~1s). Still empty after that is a turn with genuinely nothing
  said yet, and the panel **says so** (`NOTHING_YET`). No silent branch anywhere in `process()`.
- The retry is only for *empty* results. A partial read (mid-turn text present, final message
  still in flight) is possible in that same sub-second window; pressing ⌘⇧E again is the answer,
  and is cheap now that nothing runs unasked.

## Turn extraction (measured transcript shapes)

Boundary = the **last real human prompt**: a `type:"user"` entry, `isMeta`/`isSidechain` absent,
whose content is a string or a text/image list with **no `tool_result` block** (tool results
arrive as `type:"user"`!). Then take every `text` block of every non-sidechain `assistant` entry
after it; join; keep the ~24KB **tail** (conclusions live at the end), `[…truncated…]` marker
when cut.

- A `<command-name>` entry (`/clear`, `/sync-docs`…) is a plain string user entry, **not**
  `isMeta` — and it *is* a valid boundary: a slash command that runs a real turn starts it, and
  keeping it as one is what stops the previous turn's text leaking in. Pinned by test.
- `<task-notification>` turns arrive as plain string user entries and count as boundaries — press
  ⌘⇧E during one and you get *that* turn, not the one you prompted. Deliberate.
- Transcript noise (`attachment`, `ai-title`, `mode`, `cost-state`…) falls through the filters.

## The summarizer child

```
<resolve_claude()> -p --setting-sources "" --model sonnet \
    --no-session-persistence --tools "" --strict-mcp-config \
    --system-prompt "<HEBREW_PROMPT>"          # the turn/question/plan text via stdin
```

- **NOT `--bare`**: measured, `--bare -p` cannot see the subscription OAuth token and dies with
  "Not logged in". `--setting-sources ""` is the working equivalent (skips user/project
  settings, hooks, plugins); `--tools ""` + `--strict-mcp-config` = no tools, no MCP;
  `--no-session-persistence` leaves nothing in `~/.claude/projects` (verified).
- `env_clear()` + `claude_bin::forwarded_env()` + `PATH = merged_path()`: the deny-list strips
  `MULPEX_*` / `CLAUDE_CODE_CHILD_SESSION` / `CLAUDE_CODE_ENTRYPOINT`, so the child has no hub
  identity — and under `--setting-sources ""` it runs no hooks anyway. cwd = the project's
  scratch dir (neutral; no CLAUDE.md pickup).
- Both pipes are drained on their own threads **before** the timeout wait — a child blocked on
  a full pipe while the parent only `try_wait`s reads as a timeout (the classic self-inflicted
  deadlock). Timeout 90s → kill + `wait()` (reap) + failure entry.
- Measured latency 6–13s; three concurrent calls fine. That latency is why ⌘⇧E lights a busy
  dot rather than pretending to be instant.
- Failures become `ExplainEntry { ok: false, text: "ההסבר נכשל (exit N: <reason> / timeout / …)" }`
  — rendered dim, never silently dropped, and **retryable** (below). Say what you know.

## When the summarizer dies (the reason, the auto-retry, the button)

Added 2026-09-03, after a real failure entry read `ההסבר נכשל (exit 1)` and there was nothing in
it to act on and nothing in it to diagnose.

- **`claude -p` prints its own failure on stdout, not stderr.** *Measured 2026-09-03*: with a bad
  token it exits 1 with `Failed to authenticate. API Error: 401 OAuth access token is invalid.` on
  **stdout** and an **empty stderr**; an unknown `--model` likewise puts its user-facing sentence
  on stdout. Reading stderr alone is exactly how the reason got thrown away and only `exit 1`
  survived — a default reporting ignorance in the same words it reports a diagnosis.
  `failure_reason()` now prefers stderr (more specific when present) and falls back to stdout,
  capped to the first non-blank line and 200 chars. Guarded by
  `a_dead_summarizer_reports_the_reason_not_just_the_code` over the measured strings.
- **One automatic second attempt** (`run_summarizer_twice`, `AUTO_RETRY_DELAY` = 2 s): most of what
  kills the child is transient — a 401/429, the CLI self-updating out from under it — and only the
  second failure ever reaches the panel. The first is logged (two failures for *different* reasons
  is a different bug from two for the same one, and the entry can only show one).
- **A failed row carries a `נסה שוב` button** that re-runs *the same summarizer input*, never the
  transcript. `process` stashes `(kind, prompt, text, cwd)` in `Inner::retries` keyed by the
  entry's `seq`; the button calls `retry_explain(handle, id, seq)` → `explainer::retry`, which
  enqueues an `Input::Retry`. Going back to the transcript instead would be a real bug: by the
  time the user clicks, that claude may have moved on, and `read_turn` reads the **current** turn —
  the entry would quietly explain a different one.
- **`ExplainEntry::seq`** is a process-wide monotonic id and the whole addressing story: it is the
  retry address, and it is what makes a retry *rewrite the entry in place* — the backend
  `replace_entry`s at the same `seq` and `applyExplainFor` upserts by `seq` rather than prepending.
  The retry's entry carries the **retry's** `ts`, which is how the panel notices its answer came
  back and drops the in-place `מסביר…`.
- **A retry cannot be coalesced.** `enqueue`'s latest-wins would collapse two retries naming two
  different entries, leaving one of them spinning forever. The stash is *kept*, not taken: a retry
  that fails again writes itself back under the same `seq`, so the button survives. It is dropped
  when the retry succeeds, when the entry is evicted by a newer one (`push_entry` prunes what it
  truncates), and on `clear` / `forget` / `forget_project` — so it can only ever hold input for an
  entry currently on screen.
- `retry_explain` returns **false** when nothing is stashed under that seq (the entry was cleared,
  the instance is gone). The panel puts the button back rather than spinning on an update that is never
  coming — and `retry_explain` is registered in `lib.rs`'s `invoke_handler` list, which is an
  allowlist like every other dispatcher here.

## Decisions (confirmed with the user)

| Decision | Choice | Why |
| --- | --- | --- |
| When it runs | **⌘⇧E only** — never after a turn, never on a question or a plan | it is a thing you reach for, not a thing that talks at you; and nothing costs a Sonnet call unasked |
| Panel visibility | closed by default; ⌘⇧E opens it **and asks** in one keystroke | with no standing content, a visible panel would be an empty third of the window |
| Second ⌘⇧E | closes (plain toggle) — which is also how you refresh | one key, one meaning; the close clears, so the next press asks again |
| Switching rows / projects | closes and clears | the explanation belongs to one row; a stale one under a new row is worse than none |
| The next prompt | closes and clears, on the focused claude's transition into `working` | reuses state Mulpex already has; also covers answering a question or approving a plan, which the user wanted |
| History | none. one entry, no timestamp, no scrollback | the claude transcript beside it *is* the history |
| Re-opening the panel | reuses the answer if the transcript has not moved (a `stat`) | open/close/open used to cost three Sonnet calls for three identical answers |
| Forcing a re-run | no refresh button | if nothing changed the answer cannot change; a failed entry has `נסה שוב` |
| Re-opening after a new prompt | fresh, and usually `עדיין אין מה להסביר` | the old answer is about the previous turn, and that state costs no call |
| Nothing said yet | the entry `עדיין אין מה להסביר`, no Sonnet call | a call on nothing invents something; chosen over summarizing tool activity and over silently showing the previous turn |
| Where the transcript comes from | found on disk from the session uuid | it is a file, and Mulpex issued the uuid — a hook writing a bookmark per turn was work for something already known |
| Muted claudes | explained like any other | mute is presentational, and you asked for this one explicitly |
| Terminals | never | shells run no turns; `explain_now` refuses them, and the panel says so |
| Persistence | in-memory, cleared constantly | see History |
| Instance exits / project closes | dropped (`forget` / `forget_project`) | a dead row is unreachable in the UI |
| Panel | real third grid column, not an overlay | toggling refits every PTY workspace-wide — same class as a window resize (one geometry) |
| A pending question's shape | the same three parts; the question and its options go in `NEED` | the panel never changes shape, and the question IS what he needs from you |
| A question's `DID` | the turn's prose, carried in alongside the question payload | it was being dropped, so the panel showed a claude who asks having done nothing |
| Options | one row each: label, dash, a few plain words (max 12) | enough to answer from the panel without reading the terminal |
| Several questions at once | all of them, each very short | it should match what is actually on screen waiting |
| `(Recommended)` | rendered as `(מומלץ)`, and only where the input marks it | left to itself the model either kept the English or invented a recommendation |
| A turn's shape | three fixed parts: on what we're working / what I did / what I need from you | the panel is for deciding what to do next, and those are the three things that decide it |
| Who writes the headings | the app, from `ExplainSections` | model-written headings drift in wording and spacing every turn |
| A part that won't parse | raw text, no headings, logged | better an unstructured sentence than a heading over nothing |
| `NEED` with nothing needed | says so: `כלום, אפשר להמשיך` | the panel keeps one shape; you never wonder if a part was dropped |
| Length | 15 words a line, one idea, no semicolons | "short" alone measurably produced three clauses joined by semicolons |
| English identifiers | verbatim, never translated or transliterated | they are what you grep for; plain words go *around* them |
| Context sent per press | the turn + a capped window of your recent prompts | the goal lives in your prompts; the whole conversation measured up to ~137k tokens for no better answer |
| A plan's shape | the same three parts; `NEED` is the approval plus up to four rows of intent | supersedes the original one-sentence form, which was chosen when the sentence was the whole panel |
| A plan's rows | big moves only, plain Hebrew, no file names or commands | the terminal beside it holds the real plan; this is for deciding whether to say yes |
| A plan's `DID` | the prose before the plan | the research is what justifies the plan; without it a plan turn reads as pure intent |
| Plan attention | `needs` written by the `plan` hook | immediate dot, independent of the notification type — the one Explainer-era job those hooks kept |
| A failed explanation | one silent auto-retry, then a `נסה שוב` button | most failures are transient; the rest are one click, not a dead panel |
| A retry's result | rewrites the entry in place, with the retry's timestamp | the panel shows an answer, not a log of attempts |
| A failure's text | shows the reason `claude` printed, not just the exit code | "exit 1" is a code with the diagnosis thrown away |

## The panel's own UI rules

There is one entry, so the feed rules this section used to hold — render-time reversal,
chat-sticky scroll, `opacity: 0.18` history with a hover reveal, `article:last-of-type` — are all
**gone**, along with the bugs they existed to manage. Don't reintroduce them piecemeal; they only
made sense together, and only for a feed.

What is left:

- **The content is pinned to the BOTTOM of the column.** The claude's own latest words are at the
  bottom of the terminal beside it, so an explanation at the top means looking up for one and down
  for the other. Two rules do it, and both are load-bearing — *measured in a headless Chromium
  against the shipped stylesheet, 2026-09-16*:
  - `.body { flex: 1 }`. Without it the body is only as tall as its content (a flex item defaults to
    `flex: 0 1 auto`), so there is no free space for an auto margin to eat and the pin does
    **nothing**. The first version of this change was exactly that no-op, and only the measurement
    said so — `gapAboveFirst: 8` where it should have been ~330.
  - `.content { margin-top: auto }`, and **never** `justify-content: flex-end`. Measured with a tall
    entry: flex-end put the content **1570px above the container's top edge**, reported the
    container as *not overflowing*, and left those lines unreachable by any scroll. The auto margin
    resolves to 0 the moment content exceeds the box, so short content sits at the bottom and tall
    content scrolls normally from the top (measured both: `gapAboveFirst` 327 short, 8 tall, first
    line reachable in both).
- **Three parts, headings drawn here.** `PARTS` in `ExplainerPanel.svelte` holds the Hebrew wording
  and the order — goal, then turn, then the ask, widest scope first with the thing that might need
  you last, where the eye lands. An entry with `sections: null` (a parse that failed, or a question
  or plan) renders its raw text instead; don't paper over that with empty headings.
- **`מסביר…` replaces the entry**, it does not sit under it — while a summary is in flight there is
  nothing else in the panel to look at. The header keeps its pulsing amber busy dot.
- **A retry shows `מסביר…` in place of its own entry**, tracked as `{seq, ts}`: the backend rewrites
  a retried entry in place and stamps it with the retry's time, so a changed `ts` under the same
  `seq` is exactly "your retry came back", success or fresh failure alike. An entry that is merely
  *absent* must not clear the marker — that means the panel cleared or another row is focused, and
  clearing on it would put the button back under a retry that is still running.
- **`explain_now` returning false is a real answer**, not a missing one: a terminal, a gone
  instance, or a claude with no transcript yet. The panel says so rather than sitting on a busy dot
  forever.

## RTL in the panel

The entry text is **hard `dir="rtl"`, not `dir="auto"`** — auto resolves from the first strong
character, and the prompt's own rules keep English identifiers verbatim, so entries often *begin*
with one; auto then lays the whole Hebrew paragraph out LTR (measured: scrambled line order,
period on the wrong side). Inline English sits correctly inside an RTL paragraph. None of this
touches the xterm CSS — the `.xterm-rows span` rule stays sacred (docs/rendering.md).

## The Hebrew prompt

Lives as `HEBREW_PROMPT` in `explainer.rs` (turns), `QUESTION_PROMPT` (pending questions) and
`PLAN_PROMPT` (pending plans). **All three now answer the same three questions** — see the sections
above for what `NEED` holds in each. They are dead-simple Hebrew, English
terms verbatim, no preamble/headers/bullets, failures stated directly, nothing added that is not in
the input.

### Three fixed questions, one shape for all three prompts (2026-09-16)

None of the prompts asks for a paragraph any more. Each demands the same three keys, and the user's
three questions are the contract:

```
WORK: <על מה אנחנו עובדים>
DID:  <מה עשיתי בסבב הזה>
NEED: <מה אני צריך מהמשתמש עכשיו>
```

- **The keys are ASCII and the answers are Hebrew.** A Hebrew key would have to survive the model's
  own bidirectional handling before `parse_sections` ever saw it, and there is nothing to win by
  betting on that.
- **The panel draws the Hebrew headings, not the model.** `ExplainSections` carries the three values
  and `ExplainerPanel`'s `PARTS` carries the wording, so the headings cannot drift turn to turn and
  the typography is the app's. Nothing the summarizer emits is ever shown as a label.
- **`NEED` is where the three kinds differ, and the only part that is ever multi-line.** A turn: one
  sentence. A question: the ask, then a row per option. A plan: the approval, then up to four rows of
  intent. Everything else about the shape is identical, which is the point — the panel reads the same
  way whatever it is showing.
- **All three keys or none.** `parse_sections` returns `None` on a partial answer and the panel
  falls back to the raw text — a heading with nothing under it is the "default that reads as an
  assertion" shape, and the user could not tell an empty part from a dropped one.
- **`NEED` is always answered**, explicitly `כלום, אפשר להמשיך` when nothing is wanted — chosen over
  dropping the part, so the panel keeps the same shape every time.
- **The `WORK:` line needs the user's own words**, so the input is no longer the turn alone: a
  window of recent prompts rides in front of it (see `with_recent_prompts`). A claude mid-task
  rarely restates the goal; the person who set it is the one who said it.

*Measured on three real turns from two projects (cloudraw, mulpex), through the real summarizer with
the real flags.* The first pass produced the right shape, the right person and correct
`כלום, אפשר להמשיך`, but `DID` came back as three clauses strung together with semicolons — the
model crams when a rule says "short" without saying how short. Four rules fixed it and were
re-measured on the same turns: a **15-word cap**, "say only the most important thing and drop the
rest", "better a line too thin than a line too full", and an outright ban on semicolons, parentheses
and joining clauses with וגם. English identifiers survived throughout (`ORCHESTRATOR_URL`,
`Infisical`, `endpoint PATCH`, `Monitor`, `warweb#65`, `staging`) — which is the point of keeping
them verbatim: they are what the user greps for.

**All three speak in the first person, as the claude himself** (2026-09-01): "בדקתי… ועכשיו אני
צריך ממך לאשר", never "הוא בדק". Third person had a second failure mode beyond the tone the user
disliked: with nobody pinned to "אני", the summarizer regularly handed the *user* the claude's own
work — "אתה ממתין לשני agents נוספים" — which is exactly backwards. The prompts therefore pin both
pronouns at once: **אני = the claude, אתה/ממך/שלך = the human, always**, plus "גוף ראשון יחיד" so
Sonnet doesn't drift into the editorial "נציג/נעשה".

The person change had to be a *minimal* diff. A first draft opened with "תסכם… כאילו Claude עצמו
מספר מה עשה", and on a real 1.3 KB turn Sonnet answered with five paragraphs where the old prompt
gave one — "מספר מה עשה" reads as retell, not summarize. The shipped version keeps the original two
opening lines verbatim, adds the person as a clause plus two rules, and re-states brevity as
"זה סיכום, לא שכתוב: משפט אחד עד שלושה, רק העיקר. פסקה אחת". Re-measured on three real
dreamvps turns + a real `AskUserQuestion` payload + a real plan: one paragraph each, first person
throughout, options still one-per-`- ` line, `(מומלץ)` still only where the input marked it.

Two hardenings, both measured on real multi-question output and re-verified after the fix:

- **"No markdown symbols" must be explicit.** The panel renders plain text; without the rule,
  Sonnet wrapped question headers in `**…**` and the user saw literal asterisks.
- **The Recommended rule must be one-directional.** "Point out the recommended option" alone
  made Sonnet *invent* a `(מומלץ)` on an unmarked option — the rule now says mark only an
  explicit `(Recommended)`, and never hint at a preference otherwise. Verified both ways: no
  marker → zero recommendation language; marked input → exactly that option flagged.
