# The Explainer — the Hebrew panel

A right-hand column, **open by default**, that answers one question about the focused claude after
every turn: what did he just say? A headless Sonnet call produces a very short, very simple
**Hebrew** explanation (English identifiers kept verbatim) in three fixed parts — what we are
working on, what I did this turn, what I need from you — and the panel keeps a short feed of them,
newest at the bottom, history dimmed. Built 2026-08-30 as an after-every-turn feed; made on-demand
(⌘⇧E only, one entry) on the morning of 2026-09-16; **made automatic again that evening**, keeping
the on-demand version's content rules and dropping its trigger. Every claim below marked
*measured* was driven on a real session or transcript.

## The pipeline

```
claude turn ends            → Stop hook   → explainreq/<id> = <transcript_path>
claude stops to ask/plan    → askq/plan   → explainreq/<id> = <transcript_path>\ndialog
  → 200ms poll: Core::take_explain_requests (consume-and-delete, live claudes only)
  → explainer::submit → worker queue (2 threads, latest-wins per instance)
      read_turn_settled: wait for prose (or, with `dialog`, for the dialog entry)
      read_turn: a waiting AskUserQuestion / ExitPlanMode wins, else the turn's text
      → claude -p --model sonnet   → parse_sections (WORK / DID / NEED)
  → in-memory store, newest first, MAX_ENTRIES = 10 per (handle, id)
  → `explain-update {handle, id, entry}` → stores.ts::applyExplainFor → ExplainerPanel
```

**The hook is the trigger, the transcript is the input.** The hook payload carries
`transcript_path` on every event (*measured 2026-08-30 on `Stop`, 2026-09-16 on `PreToolUse`*), so
the hook writes that path and nothing else. What kind of turn it is — prose, a pending question, a
pending plan — is decided by `read_turn` from the transcript itself, which is why one request dir
covers all three (the original feed had `explainq/` and `explainplan/` carrying the tool payloads;
those are gone for good).

**`Stop` writes after the unread-mail block**, so a blocked stop (a continuing turn) writes nothing
and a turn is explained exactly once. Overwriting `explainreq/<id>` between polls is the latest-wins
coalescing; `enqueue` repeats it for a job already queued but not yet running.

`take_explain_requests` drops ids that are not a live claude: a shell never writes one (shells run no
hooks), so such a file is leftover from a closed instance whose id a terminal now holds. The file is
deleted either way, so a refused request cannot sit on disk retrying every tick.

`mpx` (the tmux host) runs the same hook and drains nothing, so it accumulates one `explainreq/<id>`
per instance — overwritten per turn, gone with its state dir. Bounded; deliberately unhandled.

## The feed: ten entries, dropped completely

`MAX_ENTRIES` is **10** per instance, on both sides: `push_entry` prepends and truncates (pruning the
retry stash of anything it cut), and `stores.ts::applyExplainFor` re-slices to
`MAX_EXPLAIN_ENTRIES` after every prepend. The user asked for the oldest to *disappear*, not merely
scroll away — memory per instance is bounded whatever the session's length. Keep the two constants
equal: a reload paints from `get_explains` (the backend's copy), and a mismatch would make the
history change size on reload.

Nothing in the frontend discards an entry except a row exiting (`session-exited` →
`removeExplainsFor`), and the backend forgets a feed on reap (`explainer::forget`) and on project
close (`forget_project`). Closing the panel, switching rows or projects, and sending the next prompt
all leave every feed alone — the on-demand version's three auto-clears are gone with its cache.

## A turn with nothing in it

A `Stop` whose turn contains no prose — an interrupted turn, or one whose final entry never flushed
in the ~1 s the reader waits — produces the entry `עדיין אין מה להסביר`, no Sonnet call. It is an
entry and not a silent skip on purpose: a silent skip here is exactly what the feature's most
expensive bug looked like from the outside (see the flush race below). Chosen by the user over the
original feed's log-only skip.

## Questions and plans are read out of the transcript

`read_turn` checks for a waiting dialog **first**, and it wins over the prose that came before it in
the same turn: a claude blocking on the user is the entry the panel is most worth reading.

- A `tool_use` named `AskUserQuestion` or `ExitPlanMode` with **no `tool_result` for its id** is the
  current turn. An answered one is ordinary history.
- Both payloads are in the transcript while the dialog is on screen. *Measured 2026-09-16: the
  assistant entry carrying the `questions` array is written before the tool runs.* The `askq`/`plan`
  hooks therefore forward only the transcript path; they still write the **`needs` status word**,
  which is the one job they must never lose (`a_waiting_dialog_still_writes_the_needs_status_word`).
- **Why they trigger a run at all:** `Stop` does not fire while a dialog waits (*measured
  2026-09-01*), so without a hook there the question would only be explained after it was answered.

### The `dialog` marker (2026-09-16, evening)

The `askq`/`plan` request carries a second line, `dialog`, and `read_turn_settled` treats a request
so marked as *unsettled* while the transcript still reads as a plain turn. The measurement above
orders the JSONL append against the tool's *run*, not against the hook's *exec*; a request drained in
that window would find the turn's prose and no `tool_use`, summarize it as a finished turn, and never
get a second chance — the wrong explanation with no correction. With the marker the reader keeps
re-reading (the same 250 ms × 4) until the dialog lands, and if it never does, uses the prose and
says so on stderr. Guarded by `a_dialog_request_waits_for_the_dialog_entry`, which appends the
question entry from another thread 400 ms in. Not measured live — the guard exists so it does not
have to be.

### A pending question is the same three parts

The question does not get a shape of its own: it *is* what `NEED` holds, with the options as rows
under it, so the panel never changes shape. What changed (the morning of 2026-09-16) is what the
summarizer is given.

**The bug this fixed:** a pending question made `read_turn` return the question payload **and throw
the turn's prose away**. The panel therefore showed a claude who had done nothing and immediately
started asking — reported from a real screenshot. The turn's text now rides in front of the question,
and `QUESTION_PROMPT` names `DID` twice: do it, and what to write when there is genuinely nothing
(`עוד לא עשיתי כלום בסבב הזה`). Guarded by `a_waiting_dialog_outranks_the_prose_before_it`, which
asserts both that the work is there and that it reads *before* the question.

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

### The plan case is the same three parts too

A plan gets the identical shape: `NEED` is "I'm waiting for your go-ahead" plus **at most four rows**
of what he intends to do, in the layout a question's options already use. It is still **not a copy
of the plan** — a plan is long, structured and technical by construction and reproducing that here
would defeat the panel. The rows carry the big moves in plain Hebrew, no file names, no commands.
`DID` carries the prose before the plan, for the same reason as the question case: the research is
what justifies the plan.

*Measured on two real plans from this machine's transcripts (a 26 KB monorepo plan and a warweb
CLAUDE.md-shrinking plan), through the real summarizer:* four rows each, plain Hebrew, first person,
`DID` naming what was actually checked.

Measured on a real `claude` v2.1.252 on a PTY, 2026-09-01 (`scratchpad/probe`):

- **`tool_input` is `{plan, planFilePath}`** — `plan` is the plan as markdown; `planFilePath` is
  ignored (one source, and no file read on this path).
- **The plan is head-capped at 12 KB** (`MAX_PLAN_BYTES`), the opposite end from a turn's tail-cap.
- **`Stop` does not fire while the plan waits.** So the transcript sits still while you read, and
  no dedup between a plan entry and a turn entry is needed.

### Plan mode is not reachable the way you would guess

`--dangerously-skip-permissions` (which every Mulpex claude runs with) **silently overrides**
`--permission-mode plan`: every hook payload still reads `permission_mode: bypassPermissions`,
no `ExitPlanMode` is ever called, and Claude just writes a plan as prose. Plan mode is reached
**only by shift+tab**, four presses from bypass — measured cycle: bypass → auto → manual →
accept edits → **plan**. Two probe runs found nothing before this was understood.

## Why a dedicated event, not a HubSnapshot field

`HubSnapshot` is rebuilt and PartialEq-compared for every project every 200ms, and a feed of Hebrew
paragraphs in it would inflate every idle compare. Summaries also arrive asynchronously, seconds
after the turn — the worker already knows exactly what changed, so it emits `explain-update`
itself. `get_explains` is the one pull: the initial paint of a fresh webview (a dev hot-reload,
mostly), grouped by `entry.id` in `setExplainsFor`.

A second event, **`explain-pending {handle, id, active}`**, drives the panel's busy indicator
(pulsing amber dot + the `מסביר…` line at the bottom of the feed). It fires on 0↔1+ transitions of a
per-instance job **count** — a count, not a bool, because a running job plus a freshly queued one
must not go idle when only the first finishes. Every path through a job (summary, failure entry, the
`NOTHING_YET` entry) decrements it; `forget` clears it, and a late finish after that stays silent.
Not persisted — after a dev hot-reload an in-flight job just shows no dot.

## The turn-end race (the expensive lesson)

**Claude Code appends the turn's final assistant entry to the transcript a beat *after* the turn
visibly ends.** Measured: final text entry at 11:55:15.346, the turn's own end inside the same
second. So a request drained the instant the `Stop` hook wrote it can read a transcript whose last
word has not arrived. The first implementation skipped an empty extraction *silently*, which
presented as "the whole feature doesn't work" and cost a debugging round — the classic "a real event
with nowhere to arrive". Two rules came out of it:

- `read_turn_settled` tries **immediately** and retries only while the turn reads as *unsettled*
  (250 ms × 4, ~1 s): empty, or — with the `dialog` marker — a plain turn where a dialog was
  announced. Still empty after that is the `NOTHING_YET` entry. No silent branch anywhere in
  `process()`.
- The retry is only for those two states. A partial read (mid-turn text present, final message
  still in flight) is possible in that same sub-second window and is accepted; the next turn's entry
  is seconds away.

## Turn extraction (measured transcript shapes)

Boundary = the **last real human prompt**: a `type:"user"` entry, `isMeta`/`isSidechain` absent,
whose content is a string or a text/image list with **no `tool_result` block** (tool results
arrive as `type:"user"`!). Then take every `text` block of every non-sidechain `assistant` entry
after it; join; keep the ~24KB **tail** (conclusions live at the end), `[…truncated…]` marker
when cut.

- A `<command-name>` entry (`/clear`, `/sync-docs`…) is a plain string user entry, **not**
  `isMeta` — and it *is* a valid boundary: a slash command that runs a real turn starts it, and
  keeping it as one is what stops the previous turn's text leaking in. Pinned by test.
- `<task-notification>` turns arrive as plain string user entries and count as boundaries — a turn
  the runtime started is explained as its own turn. Deliberate.
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
- Measured latency 6–13s; three concurrent calls fine. That latency is why the panel shows a busy
  dot and dims the previous entry the moment a new summary starts.
- Failures become `ExplainEntry { ok: false, text: "ההסבר נכשל (exit N: <reason> / timeout / …)" }`
  — rendered dim, never silently dropped, and **retryable** (below). Say what you know.

## When the summarizer dies (the reason, the auto-retry, the button)

Added 2026-09-03, after a real failure entry read `ההסבר נכשל (exit 1)` and there was nothing in
it to act on and nothing in it to diagnose.

- **`claude -p` prints its own failure on stdout, not stderr.** *Measured 2026-09-03*: with a bad
  token it exits 1 with `Failed to authenticate. API Error: 401 OAuth access token is invalid.` on
  **stdout** and an **empty stderr**; an unknown `--model` likewise puts its user-facing sentence
  on stdout. Reading stderr alone is exactly how the reason got thrown away and only `exit 1`
  survived. `failure_reason()` prefers stderr (more specific when present) and falls back to
  stdout, capped to the first non-blank line and 200 chars. Guarded by
  `a_dead_summarizer_reports_the_reason_not_just_the_code` over the measured strings.
- **One automatic second attempt** (`run_summarizer_twice`, `AUTO_RETRY_DELAY` = 2 s): most of what
  kills the child is transient — a 401/429, the CLI self-updating out from under it — and only the
  second failure ever reaches the panel. The first is logged.
- **A failed row carries a `נסה שוב` button** that re-runs *the same summarizer input*, never the
  transcript. `process` stashes `(kind, prompt, text, cwd)` in `Inner::retries` keyed by the
  entry's `seq`; the button calls `retry_explain(handle, id, seq)` → `explainer::retry`, which
  enqueues an `Input::Retry`. Going back to the transcript instead would be a real bug: by the
  time the user clicks, that claude may have moved on, and `read_turn` reads the **current** turn.
- **`ExplainEntry::seq`** is a process-wide monotonic id and the whole addressing story: it is the
  retry address, and it is what makes a retry *rewrite the entry in place* — the backend
  `replace_entry`s at the same `seq` and `applyExplainFor` upserts by `seq` rather than prepending.
  The retry's entry carries the **retry's** `ts`, which is how the panel notices its answer came
  back and drops that row's in-place `מסביר…`.
- **A retry cannot be coalesced.** `enqueue`'s latest-wins would collapse two retries naming two
  different entries, leaving one of them spinning forever. The stash is *kept*, not taken: a retry
  that fails again writes itself back under the same `seq`, so the button survives. It is dropped
  when the retry succeeds, when the entry falls off the end of the feed (`push_entry` prunes what it
  truncates), and on `forget` / `forget_project` — so it can only ever hold input for an entry
  currently in a feed.
- `retry_explain` returns **false** when nothing is stashed under that seq (the row aged out, the
  instance is gone). The panel puts the button back rather than spinning on an update that is never
  coming — and `retry_explain` is registered in `lib.rs`'s `invoke_handler` list, which is an
  allowlist like every other dispatcher here.

## Decisions (confirmed with the user)

| Decision | Choice | Why |
| --- | --- | --- |
| When it runs | **automatically** — after every turn, and the moment a question or plan dialog appears | the morning-of-2026-09-16 on-demand version was reverted the same evening: the user prefers a panel that is always current |
| Trigger | the `Stop`/`askq`/`plan` hooks write `explainreq/<id>`; the poll drains it | the exact turn-end event, proven; a status-transition trigger would also fire on compaction end and idle notifications |
| Dialog race | the `askq`/`plan` request carries `dialog`; the reader waits for the entry | a question explained as a plain turn gets no second chance (`Stop` does not fire while it waits) |
| ⌘⇧E | plain show/hide; panel **open by default** | the feed is always current, so a keypress has nothing to ask for |
| Switching rows / projects, the next prompt | nothing happens to any feed | the on-demand auto-clears existed for a cache that no longer exists |
| History | **10 per instance**, oldest dropped completely on both sides | the user's cap, with an explicit worry about memory |
| Empty turn | the entry `עדיין אין מה להסביר`, no Sonnet call | says what it knows; a silent skip once made the feature look dead |
| Layout | oldest first, newest at the bottom, sticky scroll, bottom-pinned when short | next to the claude's own latest words |
| Old entries | `opacity: 0.18`, hover reveals all | the newest is the one being read; history is there when wanted |
| Every entry's shape | timestamp + tag + the same three headed parts | uniform, at the cost of height — chosen over headings on the newest only |
| Where the transcript comes from | the hook payload's `transcript_path` | the hook exists anyway for the trigger, and every event carries it (measured on `PreToolUse` 2026-09-16) |
| Muted claudes | explained like any other | mute is presentational |
| Terminals | never | shells run no hooks; the panel says so |
| Persistence | in-memory; a webview reload refetches via `get_explains` | the transcript is the archive |
| Instance exits / project closes | dropped (`forget` / `forget_project`) | a dead row is unreachable in the UI |
| Panel | real third grid column, not an overlay | toggling refits every PTY workspace-wide — same class as a window resize (one geometry) |
| A pending question's shape | the same three parts; the question and its options go in `NEED` | the panel never changes shape |
| A question's `DID` | the turn's prose, carried in alongside the question payload | it was being dropped, so the panel showed a claude who asks having done nothing |
| Options | one row each: label, dash, a few plain words (max 12) | enough to answer from the panel without reading the terminal |
| `(Recommended)` | rendered as `(מומלץ)`, and only where the input marks it | left to itself the model either kept the English or invented a recommendation |
| A turn's shape | three fixed parts: on what we're working / what I did / what I need from you | the panel is for deciding what to do next |
| Who writes the headings | the app, from `ExplainSections` | model-written headings drift in wording and spacing every turn |
| A part that won't parse | raw text, no headings, logged | better an unstructured sentence than a heading over nothing |
| `NEED` with nothing needed | says so: `כלום, אפשר להמשיך` | the panel keeps one shape |
| Length | 15 words a line, one idea, no semicolons | "short" alone measurably produced three clauses joined by semicolons |
| English identifiers | verbatim, never translated or transliterated | they are what you grep for |
| Context sent per turn | the turn + a capped window of your recent prompts | the goal lives in your prompts; the whole conversation measured up to ~137k tokens for no better answer |
| A plan's shape | the same three parts; `NEED` is the approval plus up to four rows of intent | supersedes the original one-sentence form |
| A failed explanation | one silent auto-retry, then a `נסה שוב` button | most failures are transient; the rest are one click |
| A failure's text | shows the reason `claude` printed, not just the exit code | "exit 1" is a code with the diagnosis thrown away |

## The panel's own UI rules

`ExplainerPanel.svelte` is a merge of the original feed's mechanics and the on-demand version's
content rendering. The feed rules only make sense together; don't remove one piecemeal.

- **Render-time reversal.** The store is newest-first (a prepend-and-truncate); the panel renders
  `[...$activeExplains].reverse()` so the newest is at the bottom, growing downward like the
  transcript beside it.
- **Chat-sticky scroll.** `stuck` (a plain `let`, deliberately not `$state`) is true while the user
  is within 24 px of the bottom; the scroll-to-bottom effect keys on `ordered.length` and the busy
  flag and only runs while stuck. Switching rows resets it, so a new row opens at its newest end.
- **The content is pinned to the BOTTOM of the column** when the feed is shorter than the box, so
  a fresh row's single entry sits next to the claude's latest words. Two rules, both load-bearing
  — *measured in a headless Chromium against the shipped stylesheet, 2026-09-16*:
  - `.body { flex: 1 }`. Without it the body is only as tall as its content, so there is no free
    space for the auto margin to eat and the pin does **nothing** (`gapAboveFirst: 8` where it
    should have been ~330).
  - `.content { margin-top: auto }`, and **never** `justify-content: flex-end`. Measured with tall
    content: flex-end put the content **1570px above the container's top edge**, unreachable by any
    scroll. The auto margin resolves to 0 once content exceeds the box, so a long feed scrolls
    normally from the top.
- **History recedes.** `article { opacity: 0.18 }`, `article:last-of-type { opacity: 1 }` —
  `:last-of-type`, not `:last-child`, because the `מסביר…` line is the last child while a summary is
  in flight. `.body.explaining article:last-of-type { opacity: 0.18 }` dims the previous entry the
  moment a new summary starts (it describes the turn before the one in progress), and
  `.explainer:hover .body article { opacity: 1 }` reveals everything while the pointer is in the
  panel. The last two have equal specificity (0,3,1); source order is what lets hover win.
- **Every entry has the same body.** A `.ts` line (`HH:MM`, plus the inline שאלה/תוכנית tag), then
  the three `<section><h2>` parts from `PARTS` when `sections` parsed, else the raw text. `PARTS`
  holds the Hebrew wording and the order — goal, then turn, then the ask, widest scope first with
  the thing that might need you last, where the eye lands. An entry with `sections: null` renders
  its raw text; don't paper over that with empty headings.
- **`מסביר…` at the bottom** while a summary is in flight for the focused row; **a retry shows
  `מסביר…` in place of its own row**, tracked as `seq → ts-at-press`: a changed `ts` under the same
  `seq` is exactly "your retry came back". A row that is merely *absent* must not clear the marker
  — the user switched rows mid-retry — or coming back would put the button back under a retry that
  is still running.

## RTL in the panel

The entry text is **hard `dir="rtl"`, not `dir="auto"`** — auto resolves from the first strong
character, and the prompt's own rules keep English identifiers verbatim, so entries often *begin*
with one; auto then lays the whole Hebrew paragraph out LTR (measured: scrambled line order,
period on the wrong side). Inline English sits correctly inside an RTL paragraph. None of this
touches the xterm CSS — the `.xterm-rows span` rule stays sacred (docs/rendering.md).

## The Hebrew prompt

Lives as `HEBREW_PROMPT` in `explainer.rs` (turns), `QUESTION_PROMPT` (pending questions) and
`PLAN_PROMPT` (pending plans). **All three answer the same three questions** — see the sections
above for what `NEED` holds in each. They are dead-simple Hebrew, English terms verbatim, no
preamble/headers/bullets, failures stated directly, nothing added that is not in the input.

### Three fixed questions, one shape for all three prompts (2026-09-16)

None of the prompts asks for a paragraph. Each demands the same three keys, and the user's three
questions are the contract:

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
  intent.
- **All three keys or none.** `parse_sections` returns `None` on a partial answer and the panel
  falls back to the raw text — a heading with nothing under it is the "default that reads as an
  assertion" shape.
- **`NEED` is always answered**, explicitly `כלום, אפשר להמשיך` when nothing is wanted.
- **The `WORK:` line needs the user's own words**, so the input is no longer the turn alone: a
  window of recent prompts rides in front of it (`with_recent_prompts`). A claude mid-task rarely
  restates the goal; the person who set it is the one who said it.

*Measured on three real turns from two projects (cloudraw, mulpex), through the real summarizer with
the real flags.* The first pass produced the right shape, the right person and correct
`כלום, אפשר להמשיך`, but `DID` came back as three clauses strung together with semicolons — the
model crams when a rule says "short" without saying how short. Four rules fixed it and were
re-measured on the same turns: a **15-word cap**, "say only the most important thing and drop the
rest", "better a line too thin than a line too full", and an outright ban on semicolons, parentheses
and joining clauses with וגם. English identifiers survived throughout (`ORCHESTRATOR_URL`,
`Infisical`, `endpoint PATCH`, `Monitor`, `warweb#65`, `staging`).

**All three speak in the first person, as the claude himself** (2026-09-01): "בדקתי… ועכשיו אני
צריך ממך לאשר", never "הוא בדק". Third person had a second failure mode beyond the tone the user
disliked: with nobody pinned to "אני", the summarizer regularly handed the *user* the claude's own
work — "אתה ממתין לשני agents נוספים" — which is exactly backwards. The prompts therefore pin both
pronouns at once: **אני = the claude, אתה/ממך/שלך = the human, always**, plus "גוף ראשון יחיד" so
Sonnet doesn't drift into the editorial "נציג/נעשה".

Two hardenings, both measured on real multi-question output and re-verified after the fix:

- **"No markdown symbols" must be explicit.** The panel renders plain text; without the rule,
  Sonnet wrapped question headers in `**…**` and the user saw literal asterisks.
- **The Recommended rule must be one-directional.** "Point out the recommended option" alone
  made Sonnet *invent* a `(מומלץ)` on an unmarked option — the rule now says mark only an
  explicit `(Recommended)`, and never hint at a preference otherwise. Verified both ways.
