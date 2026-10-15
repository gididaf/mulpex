#!/usr/bin/env bash
# `npm run tauri:dev` — `tauri dev` that can run in several git worktrees at once.
#
# Plain `npm run tauri dev` serves Vite on the fixed port 1420 and uses the one
# debug home `~/.mulpex-dev`, so a second worktree's dev app fails to start (port
# taken) or, worse, reopens and rewrites the first one's projects. This picks a
# free port per run and, inside a linked worktree, a home of its own that stays
# the same across runs: `~/.mulpex-dev-<worktree folder>`. The main checkout keeps
# `~/.mulpex-dev`. Either can be pinned: MULPEX_DEV_PORT=1500, MULPEX_HOME=/path.
# Extra arguments go to `tauri dev`.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -z "${MULPEX_DEV_PORT:-}" ]]; then
  # Ask the kernel for a free port. Vite runs with strictPort, so the tiny race
  # between this and Vite binding it fails loudly instead of drifting to another port.
  MULPEX_DEV_PORT="$(node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')"
fi
export MULPEX_DEV_PORT

# A linked worktree's git dir differs from the shared one; the main checkout's doesn't.
if [[ -z "${MULPEX_HOME:-}" ]] &&
  [[ "$(git rev-parse --path-format=absolute --git-dir)" != "$(git rev-parse --path-format=absolute --git-common-dir)" ]]; then
  export MULPEX_HOME="$HOME/.mulpex-dev-$(basename "$(git rev-parse --show-toplevel)")"
fi

echo "tauri:dev → port ${MULPEX_DEV_PORT}, home ${MULPEX_HOME:-$HOME/.mulpex-dev}"
exec npx tauri dev --config "{\"build\":{\"devUrl\":\"http://localhost:${MULPEX_DEV_PORT}\"}}" "$@"
