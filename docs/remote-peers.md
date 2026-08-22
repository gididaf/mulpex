# Remote claude peers (`hub_remote_open`)

Starting a `claude` on another machine over ssh inside an ordinary Mulpex terminal, handing it a
task, and being woken when it has something to say. Builds entirely on
[shell-terminals.md](shell-terminals.md); the code is `crates/mulpex-core/src/remote.rs` plus the
watcher in `src-tauri/src/state.rs`.

Back to [CLAUDE.md](../CLAUDE.md).

A local instance can start a `claude` **on another machine** over ssh, inside an ordinary Mulpex
terminal, hand it a task, and be *woken* when it has something to say. The remote knows nothing
about Mulpex, has no instance id, no inbox and no hub tools — it is a plain `claude` on a plain
terminal — and that asymmetry is the whole design problem.

Driving it needed nothing new: `hub_terminal_send`/`_read` already type into a terminal and read it
back, and a remote claude experiences that as a human typing. What did not exist was the **other
direction**. The remote can only print; nothing told the driver to go and look; and there was no way
to distinguish "still thinking" from "finished and waiting". So the feature is exactly one thing —
a convention the remote follows and the poll loop watches for, which turns a line of its output into
a message in the **opener's inbox**. That inbox is the directory the driver's hub-listener Monitor
already polls, so **no new wake path was built**: a remote claude wakes an idle local instance
through the same machinery a peer's `hub_send` does.

- **The launch is the only moment the rules can be attached.** They ride in on
  `--append-system-prompt`, which is re-sent with every request and therefore survives both a long
  conversation and compaction. There is deliberately **no way to adopt a remote claude started by
  hand** (⌘⇧T, ssh, type `claude`): rules delivered as a typed message drift out of context, which
  is the failure this design exists to avoid. A hand-started remote is just a terminal, as before.
- **It stays a terminal.** No hub identity, no sidebar treatment, nothing in `hub_instances`' instance
  list — the only trace is `terminals/remote/<id>.json` holding its token, target and opener. This
  keeps the standing invariant that a terminal is never a hub peer, which the badge counts, the
  updater's busy guard and `attention.ts` all lean on. The wake message says so twice over, because
  a *hub message* invites a `hub_send` reply and that would be addressed to a shell.

## Three ways to start one

`hub_remote_open` opens its own terminal by default, and takes an optional `terminal_id` to use one
that already exists. What matters in every case is that the rules are attached **at launch**, on the
command line — where the terminal came from is irrelevant to the mechanism:

- **No `terminal_id`** — Mulpex opens a terminal and runs the whole `ssh … claude …` in it.
- **`terminal_id` + `ssh_target`** — the same launch, but in a terminal that already exists (e.g. one
  the user opened and left idle).
- **`terminal_id`, no `ssh_target`** — the terminal is *already logged in to the far machine*, so only
  the `claude` half is launched, on the far side. This is the one that makes a password login, a
  jump host or a VPN workable: the human does the connecting, the instance does the rest. The wake
  message then has no target to name, so `wake_body` drops that clause rather than printing a gap.

**No task named means start it idle, and do not ask.** "Start a claude on that server for our next
task" contains no task, so an instance had nothing to hand the remote and stopped to ask which is
a round trip for a question with an obvious answer: `hub_remote_open` accepts no `task` at all and
the remote simply waits at its prompt. `HUB_RULES` says to do that and report it ready. Deliberately
narrow — it removes an unnecessary question, it does **not** license guessing. A remote runs
unattended with permissions skipped on someone's server, so "pick something plausible" is the wrong
instinct there, and the standing zero-assumptions rule still governs anything genuinely ambiguous.

**Adopting an already-running remote claude is still not supported, and that is a different thing.**
Rules typed in as a message drift out of context; rules on the command line do not. Only the launch
is being moved, never the delivery mechanism.

`launch_into_existing` refuses a terminal it cannot safely type into, because a launch command sent
to a running program is *input to that program*, not a command line — the same class of mistake as
appending `; printf …` to a heredoc terminator. It refuses on three distinct grounds, each with its
own message: the shell exited, a Claude TUI is already on screen, or it is not free.

**"Free" is deliberately two-sided, and the first version was wrong.** It required output to have
stopped *and* the last line to look like a prompt — and prompt themes are endless. Run live, the box
answered with `➜  ~`, oh-my-zsh's default, which ends with the **path** rather than a sigil: the tool
would have permanently refused a perfectly idle terminal on the most common zsh theme there is. So
`at_shell_prompt` now matches a leading sigil as well as a trailing one, and — more importantly —
an unrecognised prompt is no longer fatal: after `UNRECOGNISED_GRACE_MS` of silence the terminal is
treated as free regardless of how its prompt looks.

## The marker, and why it looks like that

Every one of these was measured against a real remote over ssh (fixtures
`src-tauri/tests/fixtures/remote-claude-*.bin`, pinned by `vtgrid::remote_claude_replays`):

- **`<<<MPX <token> <kind> <summary>>>>`, because the delimiters cannot be markdown.** The first
  design used `__MPX_TO_LOCAL__`. Claude Code renders its output as markdown, `__x__` is *bold*, and
  the underscores were eaten by the renderer before the bytes reached the terminal — what arrived was
  a bare `MPX_TO_LOCAL`, and a grep for the marker found **zero** occurrences. Designed by reasoning,
  the wake path would have been dead on arrival and looked like "the remote ignores instructions".
  Angle-bracket runs survive verbatim, confirmed twice through the real recorder.
- **The token is per-terminal and secret**, because the transcript contains the *driver's own typed
  input*, echoed back by the remote TUI. Without it, a local instance that merely quoted the marker
  would wake itself. It never appears in plaintext on the command line either — the rules go over
  base64-encoded, which is also what keeps two levels of shell quoting from corrupting them.
- **Parsing is newline-tolerant, and has to be.** The TUI hard-wraps at the terminal width and the
  grid can turn that into a real newline anywhere, including mid-token. A wrap is genuinely ambiguous
  (the newline may replace a trimmed space, or may cut a word), so `parse_body` tries **both**
  readings and takes whichever yields a valid signal. Guarded by a test that wraps at every position.
- **Detection runs on the rendered grid, never on raw bytes.** The TUI writes words with cursor jumps
  between them, so `bypass permissions` is plainly visible on screen while a byte search for it
  returns 0 hits.
- **Both the log and the screen are scanned.** A row reaches the log only when it scrolls off the
  top, so a remote that answers briefly and sits there has its marker on screen and *nowhere else*.

## A remote claude is SCREEN-ONLY, and that is not fixable here

Newer Claude Code (v2.1.226 on a real box; v2.1.223 did not) draws on the **alternate screen**, and
it repaints by **absolute cursor positioning** — measured on a real capture
(`remote-claude-altscreen.bin`): `?1049h`, 22 CUP sequences, 11 erase-lines and **zero newlines** in
a 3 KB startup. Two consequences follow, both load-bearing:

- **The recorder must keep emulating while suppressed.** `Screen::suppressed` suppresses *logging*,
  never emulation. It used to drop every byte, which was fine for a stray `vim` and fatal here: a
  remote claude's terminal went completely dark, `<id>.screen` was **0 bytes**, and the driving
  instance could read *nothing at all*. Guarded by `a_real_alt_screen_remote_claude_stays_readable`,
  confirmed to fail ("the driver would be blind") with the old early-return restored.
- **Its history cannot be recovered by any amount of logging.** Nothing ever scrolls, so no row ever
  passes through `scroll_up` — the text above the viewport lives in *claude's own* buffer and is
  redrawn only when someone scrolls it. `new_output` is therefore empty by design and
  `current_screen` is the whole channel. Don't "fix" this by logging during alt screen: there is
  nothing there to log, and a repainting TUI would evict the retained history.

So the constraint is *reported* rather than papered over. `hub_terminal_read` sets **`screen_only`**
on a remote and explains it, the remote's rules cap replies at about a screen and tell it to
re-print (not re-investigate) on request, and the driver's rules say to ask for screen-sized chunks.
This came from the field: a remote answered in six sections, the driver received 4–6, and 1–3 had
scrolled into a buffer it could never reach. Nothing errored; the text simply was not there.

Worth noting how long the trigger stayed hidden: `?1049h` did **not** reproduce on a local claude of
the same version, nor with a `statusLine` configured, nor in a fresh directory — that last one only
because the probe never got past the trust prompt. It appears at 1.7 s in an already-trusted project
over ssh. Three hypotheses were falsified before the reproduction; treat "it renders inline" as a
fact about a specific recording, never as a property of Claude Code.

## Two triggers, because a model can forget

The marker is an instruction to an LLM, and instructions get skipped. `--append-system-prompt` means
it is re-sent every turn rather than remembered — it cannot decay — but re-sending is not obeying,
and the failure mode is the bad kind: the driver waits forever and nothing looks broken. So there is
a second, mechanical trigger:

- **The signal** carries *why* (`done` / `blocked` / `question`) plus a one-line summary.
- **Silence** — no output for `IDLE_TURN_END_MS` (1.5 s) — synthesises `Kind::Ended`. This is
  reliable because a working `claude` **animates its spinner continuously**, so output genuinely
  stops only between turns. Keyed on silence rather than on the spinner *word*: the vocabulary is
  randomised (`Lollygagging`, `Cooked`, `Brewed` all appeared in one short capture) and matching it
  would rot on the next Claude Code release.

**`Core.remote_awaiting` is what makes the backstop meaningful, and it is not an optimisation.** A
remote sitting at a fresh prompt, never asked anything, is *also* silent — so a backstop keyed on
silence alone fires the moment the TUI finishes drawing. Measured: the first live run woke the driver
**5.7 s after launch, before the task had even been typed**, and the test passed anyway because it
only asserted that *a* wake arrived. An id is armed when input is sent to it and disarmed when a wake
is delivered, so silence counts only while an answer is owed. Guarded by
`silence_is_only_a_wake_when_an_answer_is_owed`, confirmed to fail with the guard removed.

## Injection: the `\r` must be its own write

`mcp::inject_task` types the task, pauses, then sends `\r` **separately**, verifies a turn actually
started (the spinner, or an already-emitted signal), and retries up to `INJECT_ATTEMPTS` with a
Ctrl-U clear first. This is the same rule `pty.rs` documents for locally spawned instances, and it
was re-discovered here the hard way: sending `task + "\r"` in one write left the task **fully typed
in the input box and never submitted**, so the driver waited on a remote that had never read it. The
symptom is invisible unless you look at the screen — the bytes all arrived.

## Root, and what it costs

Claude Code refuses `--dangerously-skip-permissions` outright when running as root ("cannot be used
with root/sudo privileges for security reasons"), and remote boxes are commonly entered as root. The
launch therefore exports **`IS_SANDBOX=1`**, which is a deliberate bypass of a check Claude Code put
there on purpose. The justification is that a remote peer runs unattended and answers to another
model, so it must not stop at a permission prompt no human will ever see — but the consequence is
real and worth stating plainly: **a remote claude runs unsupervised, with permissions skipped, doing
whatever the driving instance asks of it.**

