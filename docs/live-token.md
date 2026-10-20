# Live Token — switch accounts without restarting claudes

Mulpex ▸ Live Token… (`src-tauri/src/live_token.rs`, `LiveTokenDialog.svelte`).

## The problem

A `claude` reads `CLAUDE_CODE_OAUTH_TOKEN` once, when it starts. Someone who switches Claude accounts
by rewriting a token file (the user's token manager at tokens.dreamvps.com writes
`~/.claude-token-manager/token`, and `.zshrc` exports it) had to ⌘⇧R every claude afterwards. The fix
must not depend on Claude Code internals, because Claude Code updates constantly.

## How it works

- **Off by default.** `<mulpex home>/live-token.json` holds `token_file` and `refresh_command`. An
  empty `token_file` means off. When it is off, no proxy starts and no spawn changes at all.
- **The proxy runs in the PTY daemon** (`crates/mulpex-ptyd/src/daemon/live_token.rs`), not in
  the app, since 2026-10-19: claudes now outlive an app restart (docs/ptyd.md), and the port they
  call has to be served while the app is down. The app keeps the settings and the dialog and pushes
  the effective config (`live_token::push`: token file, refresh command, the upstream base URL and
  the refresh command's environment — the two things only the app can read from the login
  environment) on every connect and every change; the reply says where the proxy listens.
- **When it is on**, Mulpex runs an HTTP proxy on `127.0.0.1:<random port>`. Every claude it spawns
  gets two env vars:
  - `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>`;
  - `ENABLE_TOOL_SEARCH=true`, unless the user's own env already sets it.

  Both are documented Claude Code env vars. This covers interactive claudes (`pty.rs`) and the
  hidden `claude -p` runs (`saves.rs` — also used by Import Docs — `explain.rs` and `broadcast.rs`).
- **On each request**, the proxy reads the token file again, but only when its mtime or size has
  changed. It replaces `Authorization: Bearer …` with that token and forwards the request to the real
  API: the user's own `ANTHROPIC_BASE_URL` if they set one, else `api.anthropic.com`. The response
  streams back unchanged. The proxy never decompresses, so `content-length` and `content-encoding`
  stay correct.
  - A request with no `Authorization` header (an API-key user) passes through untouched.
  - A file that is missing, empty or not `sk-ant-…` keeps the last good token. A refresher's `mv`
    can briefly leave no file.
- **Refresh.** `refresh_command` runs via `/bin/sh -c` with the login env:
  - every 60 s;
  - at once when the upstream returns 401 or 429. If the token then differs, the request is retried
    **once** with the new one, so the claude never sees the error.

  Concurrent failures share one run (5 s debounce, a mutex held while it runs). Each run has a 15 s
  timeout.
- **The `<secret>` path prefix.** Requests without it get a 403. This keeps any other local process
  from using the proxy to borrow the account. Claude Code keeps a base URL's path on every request
  (measured).
- **The proxy binds before any project restores** (`lib.rs` `setup` → `live_token::init`), so
  restored claudes already point at it. Once started it stays up for the life of the app, even if
  the feature is turned off, so claudes already pointed at it keep working.
- **Remote Hosts.** `live_token_get` and `live_token_set` are host-scoped. They are in
  `remote/rpc.rs` `ALLOWED`, and `ipc.ts` passes `host`. The dialog opened on a remote host's tab
  edits **that** machine's settings, and the paths in it are that machine's paths. Each Mulpex runs
  its own proxy for its own claudes.

## Named tokens and per-claude pins (2026-10-19)

- **The default** is the token file + refresh command, exactly as above. **Named tokens** are pasted
  `sk-ant-…` tokens with a name the user types, kept in `<mulpex home>/live-tokens.json` (0600,
  `secrets::write_private`). The frontend only ever sees the name, the last 4 characters and the org id.
- **Adding one** runs a free `POST /v1/messages/count_tokens` with it: a 401/403 refuses it, and the
  reply's `anthropic-organization-id` is stored (so two tokens of one account look alike). No
  account name is available: these tokens lack the `user:profile` scope (measured).
- **Which claude is calling.** A claude's base URL is `…/<secret>/c/<route>`, with
  `route = <project_key>-<instance id>` (`live_token::route`) — stable across app restarts,
  adoption and `--resume`. The daemon strips it (`split_route`) before forwarding.
- **Pins** live in `<mulpex home>/live-token-claudes.json` (`assign`: route → token id) and are
  pushed to the daemon with the config. The daemon reads them on every request, so a pin changes the
  claude's **next request**, no restart. A pin to a deleted token falls back to the default.
  `Core::close` (⌘W, `hub_close`) forgets the route, so a reused number starts on the default.
- **A pinned token's 401/429 goes back to the claude unchanged.** Only the default is refreshed
  and retried.
- **Hidden runs** carry their claude's route (⌘S forks, ⌘⇧E); Ask All and Import Docs use the default.
- **Stats.** The proxy records, per route, the last request's token and status, and per token the
  org id, its last status and every `anthropic-ratelimit-unified-*` header (`5h-utilization`,
  `7d-utilization` are 0–1 fractions; `*-reset` unix seconds). The `live_token_stats` daemon op
  returns them; `live_token_claudes` joins them with each project's live claudes for the dialog,
  which polls every 2 s.
- **`routed`.** `live-token-claudes.json` also lists the routes a claude was spawned with. A claude
  started before (or against an old daemon) is shown as "restart once (⌘⇧R) to control".
- **Old daemon.** The `configure` reply carries `routes: true`. A daemon from before has none, and
  then claudes get the plain `/<secret>` URL (it would forward `/c/…` upstream as a path). The
  wire change is additive, so `PROTO` was not bumped: per-claude pins start on a user's machine after
  their next real quit, when the new daemon starts. The dialog says so.
- **UI.** The dialog: Default (file + refresh), Tokens (rename by clicking the name, ✕ asks and
  lists who moves to Default, Add), Claudes (a picker per claude). The sidebar row's right-click has
  a **Token ›** submenu while Live Token is on (`App.svelte::tokenMenu`, which gives up after 400 ms
  so a slow remote host never delays the menu).

## Why it's built this way (measured on claude 2.1.301, 2026-10-16)

- **`ANTHROPIC_UNIX_SOCKET` was tried first and fails.** It is the path `claude ssh` uses, with an
  `ssh-placeholder` token. Over that socket, `/v1/messages` is **TLS to `api.anthropic.com`** — raw
  ClientHello bytes were captured. So a proxy on the socket can only tunnel; it can never change a
  header. Only `policy_limits` went over it in plain HTTP.
- **`ANTHROPIC_BASE_URL` works.** `/v1/messages?beta=true` arrives as plain HTTP, with
  `Authorization: Bearer <env token>` and `anthropic-beta: oauth-2025-04-20,…`. With the token swapped
  in the file between two turns of one interactive session, the next turn used the new token: a bad
  token gave a 401 on that turn, and the restored good token answered on the following one.
- **The env token stays the real one, not a placeholder.** `/api/oauth/*`, bootstrap and telemetry
  ignore the base URL and always go straight to `api.anthropic.com` with the env token. A placeholder
  401s them, and the claude logs "OAuth 401: keeping the user-supplied CLAUDE_CODE_OAUTH_TOKEN". With
  the real spawn-time token those calls behave exactly as before. The catch: anything they report
  (such as account details) describes the spawn-time account until a restart. **Inference, and the
  rate-limit footer, which comes from inference response headers, follow the live token.**
- **ToolSearch.** A base URL that isn't first-party switches its optimistic mode off: "is not a
  first-party Anthropic host. Set ENABLE_TOOL_SEARCH=true". Hence the env var.
- **`/usage` was the same** with the proxy as without it.

## Limits

- A claude started while the feature was off has no proxy env. Restart it once with ⌘⇧R. The dialog
  says so.
- 401 and 429 trigger the retry; any other status is passed through untouched.
- Only one account was available to test with. Swaps were measured as good → invalid → good, not
  account A → account B; pins were measured with the same token pasted as a second one.
