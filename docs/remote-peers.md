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

## The task goes on the remote's command line

`hub_remote_open` passes the task as `claude`'s positional prompt argument, base64'd through the
same wrapper the rules already use:

```
cd <dir> && export IS_SANDBOX=1 && exec claude --dangerously-skip-permissions \
  --append-system-prompt "$(printf %s <RULES_B64> | base64 -d)" \
  "$(printf %s '<TASK_B64>' | base64 -d)"
```

So there is **nothing to type, no input box to wait for, and no submit key**. The base64 is what
makes an arbitrary task safe to put in a shell command line typed into a terminal — it was already
the answer for the rules blob, and the task's needs are identical.

**What this replaced, and why.** The task used to be typed into the remote claude's TUI once
`looks_like_claude_tui` saw it come up: one write of the text, 400 ms, then a separate `\r`, with
up to three attempts and a Ctrl-U clear between them. That machinery existed because a `\r` at the
tail of a fast burst is read as *paste content* rather than Enter, so the task would sit fully
typed and unsubmitted — a real failure, correctly diagnosed at the time. What nobody looked for was
what happened to the text that *did* submit: **`claude` caps a burst at one tty read — measured at
1022 characters, at every input size from 1200 to 6000** (see [hub.md](hub.md)). A remote brief
over about a kilobyte therefore arrived cut mid-word, the spinner started, and the driver reported
`task_delivered: true`. Locally spawned children had the same defect and were fixed first; this
path was the last one left typing.

`inject_task`, `INJECT_ATTEMPTS` and the pre-typing readiness wait are deleted. What remains is a
start confirmation: wait up to `REMOTE_READY_TIMEOUT_MS` for the TUI, a spinner, or an
already-emitted signal, and report **`task_started`** — renamed from `task_delivered`, deliberately.

**The reply cannot claim more than that, and says so.** A locally spawned child verifies its own
delivery: `hook::verify_spawn_delivery` compares the prompt `claude` received against the one
Mulpex sent. A remote has no Mulpex hook, so nothing on this side can read back what the far side
got. `started` is the honest word for what was observed. The compensation is that argv, unlike
typing, has no failure mode to detect.

### The one length limit in the system

`remote::MAX_REMOTE_TASK_CHARS` = **32,000**, and `hub_remote_open` **refuses** above it rather
than sending part of a brief — clipping to fit would be the original bug with a different number.
The error names the way through: open the remote with a short task and `hub_terminal_send` the
full text.

The local terminal the command is typed into is **not** what constrains this. A real shell driven
on a PTY took a **128,148-character** command line with nothing lost, so the tty is nowhere near
being the limit. The real ceilings are on the far side: `MAX_ARG_STRLEN` (128 KiB for any single
argument on a Linux remote — and the decoded task is one argument) and `ARG_MAX` (1 MiB). Base64
inflates by 4/3, so 32,000 characters puts the whole command line near 43 KB: a third of the
tightest hard limit. It is set to make an absurd task fail in a defined way, not because a real
brief comes close.

**Measured end-to-end, including the ssh hop.** Two runs, both typing the command the code really
generates into a real `zsh -l -i` on a PTY, exactly as a Mulpex terminal does:

- *Locally, real claude.* `remote_launch_command` for a 6,000-character task (8,352 bytes) — the
  `claude` it launched received the task **byte-exact**, read from that child's own transcript
  `.jsonl`.
- *Over ssh to a real Ubuntu x86_64 box.* `ssh_command` with a stub `claude` on the remote `PATH`
  recording its argv. The remote process received **argc 4** — the two flags, the decoded rules,
  and the task as the **last** argument — with `cwd` and `IS_SANDBOX=1` correct. At 6,000
  characters the task came back byte-identical; **at the full 32,000-character cap** (a 42,921-byte
  ssh command line) it came back with a matching FNV-1a hash on both sides. Nothing about the ssh
  hop, the remote login shell, or the `printf | base64 -d` round trip alters a byte.

Pinned by `remote::a_remote_task_travels_on_the_command_line_not_through_the_tui` (the task is the
last argument, the rules keep their slot, a task-less launch grows no empty argument, and it
survives the ssh wrapper's quoting) and
`mcp::an_oversized_remote_task_is_refused_rather_than_trimmed`.

## Root, and what it costs

Claude Code refuses `--dangerously-skip-permissions` outright when running as root ("cannot be used
with root/sudo privileges for security reasons"), and remote boxes are commonly entered as root. The
launch therefore exports **`IS_SANDBOX=1`**, which is a deliberate bypass of a check Claude Code put
there on purpose. The justification is that a remote peer runs unattended and answers to another
model, so it must not stop at a permission prompt no human will ever see — but the consequence is
real and worth stating plainly: **a remote claude runs unsupervised, with permissions skipped, doing
whatever the driving instance asks of it.**

