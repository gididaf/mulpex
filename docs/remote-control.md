# Remote Control (⌘⇧O)

Drive the whole of Mulpex from a phone: every project, every claude's conversation, typing,
answering questions and plans, terminals, starting and closing instances, and push notifications
when a claude needs you. Built 2026-10-07/08 in nine QA'd phases.

## The pieces

```
Mac (src-tauri/src/remote/)  ──wss out──►  relay (crates/mulpex-relay)  ◄──wss──  phone (remote/, a PWA)
                                           mulpex.dreamvps.com, Caddy TLS
```

- **`src-tauri/src/remote/`** — the Mac side. `mod.rs`: state, the connection thread, the
  protocol. `crypto.rs`: pairing + per-connection handshake + sealed frames. `chat.rs`: transcript
  → chat items. `dialog.rs`: AskUserQuestion / plan → buttons → keystrokes. `push.rs`: Web Push.
- **`crates/mulpex-relay`** — a dumb router (axum). One room per Mac (`host_id`); it also serves
  the phone app. It never looks inside `data`, and must not start to.
- **`remote/`** — the phone app (Svelte, built with `vite build --config remote/vite.config.ts`
  into `remote/dist`, served by the relay). Shares the repo's `node_modules`; its own `tsconfig.json`
  and `svelte.config.js` so `svelte-check --tsconfig ./remote/tsconfig.json` covers it.
- **Desktop UI** — `RemoteDialog.svelte` (on/off, QR, relay address, paired phones + Remove), the
  `Remote · N phones` chip in `TopBar.svelte`, and the `remote-*` event handlers in `App.svelte`.

## Security model

Claudes run with `--dangerously-skip-permissions`, so this is full control of the Mac. Therefore:

- **End-to-end encrypted; the relay reads nothing.** The QR carries the Mac's X25519 public key and
  a one-time pairing secret **in the URL fragment** (never sent to a server). The phone makes its
  own key and proves the QR with `HMAC(secret, device id ‖ device key)`. Every connection then runs a
  Noise-KK-style handshake (three DHs, HKDF-SHA256) and every frame is ChaCha20-Poly1305 with a
  counter nonce. The phone side is `@noble` (WebCrypto only exists on https pages, and the app is also
  served over LAN http while developing); `crypto.rs::tests::matches_the_phone` pins the two
  implementations together with vectors **produced by the phone's code**.
- **A QR works once, for 10 minutes.** Paired phones live in `<mulpex home>/remote/devices.json`
  (0600, public keys only) and can be removed from the dialog; a removed phone's live session is cut
  on the next pass.
- **The relay authenticates the Mac**: the first token to claim a `host_id` owns it; the relay
  keeps only SHA-256 hashes (`/var/lib/mulpex-relay/hosts.json`).
- **On/off survives a restart** (`config.json` `enabled`) — the user asked for it. The top-bar chip
  is what keeps an open door visible.
- Keys: `<mulpex home>/remote/config.json` (host id, relay URL, host key, relay token, VAPID key,
  on/off), all 0600.

## What crosses, and how

- **View** — every project's sidebar rows with status, task, ctx %, and the open dialog. Built in
  `hub.rs`'s loop (only while on), sent when it changes.
- **Chat** — from the transcript `.jsonl` (`saves.rs::transcript_path`, re-resolved every 2 s since
  an in-TUI `/resume` moves it). Shown: human prompts (`origin.kind == "human"`), assistant text,
  tool calls collapsed with their results joined by id; task-notification wakes and interrupts as
  system lines. Hidden: thinking, `isMeta`, sidechain, all bookkeeping entry types. First load reads
  the last 4 MB / 400 items.
- **Queued messages** — a message typed while the claude is busy goes into Claude Code's queue,
  which the transcript records as `queue-operation` entries: `enqueue` (+content), `dequeue` (oldest
  taken at turn end → later a `user` entry), `remove` (+content; `absorbed_mid_turn` → it exists
  only as a `queued_command` attachment, which is now shown as a user bubble), `popAll` (pulled back
  to edit). `chat.rs::Queue` replays them — notifications too, since a `dequeue` names nothing — and
  sends the human ones as a `queue` item whenever they change; the phone shows them as faded
  "queued" bubbles, hidden while the claude is `waiting` (a killed turn can leave a stale one).
  Replayed against 4,788 real transcripts (41,148 operations): 2 messages left queued.
- **Typing into a claude goes through the desktop**, never straight to the PTY: only the frontend
  can read the input box (`promptbox.ts`), so it refuses onto a draft, into an open dialog, or with no
  prompt on screen, and the reason goes back to the phone. One bracketed paste + `\r` in one write
  (measured exact on claude 2.1.292, multi-line Hebrew included). **Over 900 bytes the message is
  written to `<mulpex home>/remote/inbox/<uuid>.md` and only a one-line pointer is typed** — the root
  rule about >1 KB through a TUI. Control characters are stripped so a message can't end the paste.
- **Dialogs** — the `askq`/`plan` hooks save the tool input to `dialog/<id>.json`
  (`mulpex_core::DIALOG_DIR`); the view carries it while the status is `needs`. The answer is turned
  into keys **on the Mac against the file as it is now** (`dialog.rs`, every sequence measured on a
  real claude — see its module doc), and the desktop plays them 250 ms apart after re-checking `needs`.
  **Esc on a dialog fires no hook at all** (measured on claude 2.1.296, both dialogs), so nothing
  cleared `needs`. The phone kept the card up and refused every message ("Answer the open question
  first"), a deadlock, and the desktop row stayed red. `Core::clear_escaped_dialogs` (in the poll
  loop) now reads the transcript of a `needs` claude that has a dialog on file. An `is_error` result
  for the dialog call followed by `[Request interrupted…` sets `waiting`. The same result followed
  by more assistant output (a plan rejected with feedback) sets `working`.
- **Terminals** — shells only keep a 256 KB output tail plus taps (`pty.rs::OutputSink::tap`); the
  phone gets the tail, then every byte. The phone feeds them to a **hidden xterm at the desktop's one
  geometry** (the emulator — never resized from the phone) and shows its buffer as **readable 13 px
  text**: wrapped rows joined, re-wrapped to the phone, colors kept, a BiDi paragraph per line.
  Full-screen programs don't survive the re-wrap; that was the user's choice over a tiny exact screen.
  Phone input reaches shells directly (`write_terminal`) — never a claude.
- **Tabs and rows** — the phone shows the desktop's two tab badges (green idle, red needs you,
  muted excluded) and its sidebar order (`ordered()` mirrors `stores.ts::displayOrder`). Long-press
  (0.4 s) + drag reorders (`remote/src/order.ts::dragSort`, touch events — a non-passive
  `touchmove` is what stops the page scrolling under the drag); rows only among siblings, the
  desktop's rule. A drop is sent as `reorder` → `remote-reorder` → `applyProjectOrder` /
  `applySessionOrder`, i.e. committed like a desktop drag, and comes back in the next view. There is
  no phone-local order (a phone-only one shipped for a day; its `mulpex.order` key is deleted).
- **Start / close** — forwarded to the desktop (`remote-action`), the same calls as ⌘T / ⌘⇧T / ⌘W,
  aimed at the phone's project, never moving the Mac's focus. The phone confirms a close. (The message
  is `close-instance`; plain `close` means "leave the chat view".)
- **Push** — the **Mac** sends Web Push itself (VAPID + RFC 8291 `aes128gcm`, `push.rs`), so neither
  the relay nor the push service can read it; payload is only the title + project + task. Two
  events (`notes_for`): **needs you** (a claude enters `needs`) and **done** (`working` → `waiting`,
  i.e. it replied — the first thing the user tested, and not in the first build). Only claudes the
  user started (no `parent`), never muted ones; nothing for what was already so when turned on.
  Skipped for a phone that has that claude's chat open **with the app in front** — the phone reports
  `visible` on every change, because a minimized Android app stays connected for a while and would
  otherwise swallow every notification. A 404/410 drops the subscription. Encryption was checked against `http_ece` (the library
  `web-push` uses). iOS needs the app on the Home Screen.
- **Keep awake** — `caffeinate -i -w <mulpex pid>` while on: the Mac doesn't idle-sleep, the
  display still does, and a closed lid still sleeps.

## The relay VM

`mulpex.dreamvps.com`: Caddy (auto TLS) → `mulpex-relay` on `127.0.0.1:8787`, systemd unit
`mulpex-relay`, its own user, `ProtectSystem=strict`. Deploy with
`MULPEX_RELAY_SSH=root@185.145.254.21 scripts/deploy-relay.sh` (key
`~/.ssh/mulpex_relay_ed25519`; by IP because only the IP is in `known_hosts`); it builds the phone app here and the relay **on the VM** with this
repo's `Cargo.lock`. `--setup` (`scripts/relay-setup.sh`) only for a fresh box. A deploy that touches only `remote/`
needs no Mulpex release — the Mac reconnects to the restarted relay on its own.

**Caching.** The relay sends `Cache-Control: no-cache` on everything but the content-hashed
`/assets/` (immutable). Before that it sent none, the browser guessed a lifetime from
`Last-Modified`, and a phone kept opening a two-day-old app for hours after a deploy. `sw.js` also
fetches navigations with `cache: "no-store"`, and on activation reloads open windows once — a
changed `sw.js` is the one file a browser re-fetches past its HTTP cache, which is what un-stuck
phones that had cached the page before the header existed (measured in headless Chrome against a
server caching everything for an hour). Locally:
`target/debug/mulpex-relay --listen 0.0.0.0:8787 --static remote/dist --data <dir>` and set the
dialog's relay address to `http://<lan ip>:8787`.

## Testing it

- `cargo test -p mulpex --lib remote` (protocol, crypto vectors, transcript parser, dialog keys,
  push), `cargo test -p mulpex-relay`.
- **`live_host`** (ignored): a real Mac-side connection serving a fake workspace, transcript
  (`MULPEX_TEST_TRANSCRIPT`), terminal and desktop, against any relay
  (`MULPEX_TEST_RELAY=https://mulpex.dreamvps.com`). Drive the real phone page against it in headless
  Chrome over CDP — that is how every phase was checked before QA, and how two real bugs were found
  that no unit test saw (a constructor-time callback before `conn` existed; a 0×0 geometry).
- Dialog key sequences and typing were measured on a real `claude` on a PTY with an isolated
  `CLAUDE_CONFIG_DIR` and the token resolved through zsh — see
  [verification-log.md](verification-log.md) habits; read answers from the transcript's
  `tool_result`, never the screen.
