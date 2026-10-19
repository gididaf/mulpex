#!/usr/bin/env bash
# The PTY daemon end to end and headless (docs/ptyd.md): a real Mulpex with its
# window hidden, on a scratch home, restarted three ways —
#
#   1. a detached restart (what an update does): same processes, panes
#      repainted from the snapshots the old app saved — IDENTICAL dumps;
#   2. a crash (kill -9): same processes, panes replayed from the daemon's ring;
#   3. a real quit: every process gone, the daemon gone, the scratch root gone.
#
#   bash scripts/test-ptyd.sh            # build, run, clean up
#   CLAUDE=1 bash scripts/test-ptyd.sh   # also a real claude instance (⌘T)
#   KEEP=1 …                             # leave the work dir
#   VISIBLE=1 …                          # a visible window — for Linux under Xvfb,
#                                        # where a hidden one measures 2x1 cells
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/mulpexptyd-XXXXXX")"
WORK="$(cd "$WORK" && pwd -P)"
BIN="$ROOT/target/headless/debug/mulpex"
H="$WORK/home"

if [ -z "${NOBUILD:-}" ]; then
  echo "› building (frontend, sidecars, hidden-window app)"
  npm run build >/dev/null
  bash scripts/bundle-helper.sh >/dev/null
  (cd src-tauri && CARGO_TARGET_DIR=../target/headless \
    TAURI_CONFIG="{\"app\":{\"windows\":[{\"title\":\"Mulpex\",\"width\":1280,\"height\":820,\"visible\":$( [ -n "${VISIBLE:-}" ] && echo true || echo false ),\"backgroundThrottling\":\"disabled\"}]}}" \
    cargo build -q --features tauri/custom-protocol)
fi
cp target/release/mulpex-ptyd target/release/mulpex-helper "$(dirname "$BIN")/"

mkdir -p "$H" "$WORK/proj"
echo "$WORK/proj" >"$H/open.txt"
KEY="$(printf %s "$WORK/proj" | sha256sum 2>/dev/null | cut -c1-16)"
[ -n "$KEY" ] || KEY="$(printf %s "$WORK/proj" | shasum -a 256 | cut -c1-16)"

APP=""
cleanup() {
  [ -n "$APP" ] && kill -9 "$APP" 2>/dev/null || true
  # The daemon outlives the app by design; this run's is the one on $H.
  local d; d="$(cat "$H/ptyd.pid" 2>/dev/null || true)"
  [ -n "$d" ] && kill -9 "$d" 2>/dev/null || true
  [ -n "${KEEP:-}" ] && { echo "kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

start() {
  MULPEX_DEBUG_DUMP=1 MULPEX_HOME="$H" "$BIN" >>"$WORK/app.log" 2>&1 &
  APP=$!
  sleep "${1:-6}"
}
# The relaunched app is a new process (a restart re-execs); find it.
find_app() { APP="$(pgrep -nf "^$BIN\$" || true)"; }
daemon_pid() { cat "$H/ptyd/ptyd.pid" 2>/dev/null; }
TMP="${TMPDIR:-/tmp}"
state_dir() { ls -d "${TMP%/}"/mulpex-*/"$KEY" 2>/dev/null | head -1; }
N=0
req() { N=$((N + 1)); local sd; sd="$(state_dir)"; mkdir -p "$sd/termreq"; printf '%s' "$1" >"$sd/termreq/$(printf %016d $N)-t.json"; }
drive() { mkdir -p "$H/drive"; printf '%s' "$1" >"$H/drive/$(date +%s%N).json"; sleep 2; }
snap_dumps() { rm -rf "$WORK/$1"; mkdir -p "$WORK/$1"; cp "$H"/dump/1-*.txt "$WORK/$1/"; }
FAIL=0
ok() { echo "  ✓ $*"; }
bad() { echo "  ✗ $*"; FAIL=1; }
same_dumps() { # $1 before-dir, $2 label
  local f n
  for f in "$WORK/$1"/1-*.txt; do
    n="$(basename "$f")"
    if diff <(tail -n +2 "$f") <(tail -n +2 "$H/dump/$n") >/dev/null 2>&1; then ok "$2: pane $n IDENTICAL"
    else bad "$2: pane $n DIFFERS"; diff <(tail -n +2 "$f") <(tail -n +2 "$H/dump/$n") | head -8; fi
  done
}
pid_in() { grep -aoE "$1=[0-9]+" "$H/dump/$2.txt" | head -1 | cut -d= -f2; }

echo "› first launch"
start
D="$(daemon_pid)"; echo "$D" >"$H/ptyd.pid"
[ -n "$D" ] && ok "the app started a daemon (pid $D)" || bad "no daemon"
SD="$(state_dir)"
[ -n "$SD" ] && ok "project scratch dir under the daemon's root: $SD" || bad "no scratch dir for the project"

req '{"op":"open","label":"plain","seed":"echo SHELLPID=$$; seq 1 40; printf \"\\033[1;33mYELLOW\\033[0m שלום עולם\\n\""}'
req '{"op":"open","label":"tui","seed":"seq 1 400 | less"}'
if [ -n "${CLAUDE:-}" ]; then
  drive '{"menu":"new_session"}'; sleep 10
  # A fresh folder: answer Claude Code's trust question ("Yes, I trust").
  drive '{"termInput":"\u001b[B"}'; drive '{"termInput":"\r"}'; sleep 6
fi
sleep 5
snap_dumps before
P1="$(pid_in SHELLPID 1-1)"; [ -n "$P1" ] && ok "term#1's shell is pid $P1" || bad "no pid in term#1"
CLAUDE_ID=""; CLAUDE_PID=""
if [ -n "${CLAUDE:-}" ]; then
  CLAUDE_ID="$(ls "$SD/pids" 2>/dev/null | head -1)"
  CLAUDE_PID="$(cat "$SD/pids/$CLAUDE_ID" 2>/dev/null || true)"
  [ -n "$CLAUDE_PID" ] && ok "claude#$CLAUDE_ID is pid $CLAUDE_PID" || bad "no claude started"
  L0="$(ps -A -o pid=,ppid=,command= | awk -v p="$CLAUDE_PID" '$2==p && /listen/ {print $1}' | head -1)"
fi

echo "› 1. detached restart (an update)"
OLD="$APP"
drive '{"invoke":"restart_app","args":{"detach":true}}'
sleep 9
find_app
[ -n "$APP" ] && [ "$APP" != "$OLD" ] && ok "the app restarted (pid $OLD → $APP)" || bad "no new app process"
[ "$(daemon_pid)" = "$D" ] && ok "same daemon" || bad "the daemon changed"
kill -0 "$P1" 2>/dev/null && ok "term#1's shell survived" || bad "term#1's shell died"
[ -n "$CLAUDE_PID" ] && { kill -0 "$CLAUDE_PID" 2>/dev/null && ok "claude#$CLAUDE_ID survived" || bad "claude#$CLAUDE_ID died"; }
same_dumps before "after a detached restart"
req '{"op":"send","id":1,"data":"echo AFTER1=$$\r"}'
sleep 3
[ "$(pid_in AFTER1 1-1)" = "$P1" ] && ok "term#1 takes input, same shell" || bad "term#1 input after restart: $(pid_in AFTER1 1-1)"
ls "$H/ptyd/snapshots/$KEY" 2>/dev/null | grep -q . && bad "snapshots were left behind" || ok "snapshots were used up"
if [ -n "$CLAUDE_PID" ]; then
  # The focused row is focused again, so typing reaches the claude.
  grep -q "\"activeSession\": $CLAUDE_ID" "$H/dump/ui.txt" && ok "claude#$CLAUDE_ID still has focus" || bad "focus moved: $(grep activeSession "$H/dump/ui.txt")"
  # Its hub listener (a child of the claude, armed at session start) is the
  # same process as before: nothing had to re-arm.
  lst() { ps -A -o pid=,ppid=,command= | awk -v p="$CLAUDE_PID" '$2==p && /listen/ {print $1}' | head -1; }
  L="$(lst)"; [ -n "$L" ] && [ "$L" = "$L0" ] && ok "claude#$CLAUDE_ID's hub listener is the same process (pid $L)" || bad "hub listener under claude#$CLAUDE_ID: before $L0, after $L"
  drive '{"termInput":"Reply with exactly one word: PONG"}'; drive '{"termInput":"\r"}'
  for _ in $(seq 1 30); do grep -q "PONG" "$H/dump/1-$CLAUDE_ID.txt" 2>/dev/null && grep -c PONG "$H/dump/1-$CLAUDE_ID.txt" | grep -qv '^1$' && break; sleep 2; done
  [ "$(grep -c PONG "$H/dump/1-$CLAUDE_ID.txt")" -ge 2 ] && ok "claude#$CLAUDE_ID answers after the restart" || bad "claude#$CLAUDE_ID did not answer"
fi

echo "› 2. crash (kill -9)"
snap_dumps before2
kill -9 "$APP"; sleep 1
kill -0 "$P1" 2>/dev/null && ok "term#1 survived the crash" || bad "term#1 died with the app"
start 8
same_dumps before2 "after a crash"
req '{"op":"send","id":1,"data":"echo AFTER2=$$\r"}'
sleep 3
[ "$(pid_in AFTER2 1-1)" = "$P1" ] && ok "term#1 takes input after the crash" || bad "term#1 input after crash"

echo "› 3. quit"
ROOTDIR="$(dirname "$SD")"
drive '{"invoke":"quit_app"}'
sleep 4
kill -0 "$APP" 2>/dev/null && bad "the app is still running" || ok "the app quit"
kill -0 "$D" 2>/dev/null && bad "the daemon is still running" || ok "the daemon exited"
kill -0 "$P1" 2>/dev/null && bad "term#1's shell outlived the quit" || ok "term#1's shell is gone"
[ -n "$CLAUDE_PID" ] && { kill -0 "$CLAUDE_PID" 2>/dev/null && bad "claude outlived the quit" || ok "claude is gone"; }
[ -d "$ROOTDIR" ] && bad "the scratch root is still there" || ok "the scratch root is gone"

[ "$FAIL" = 0 ] && echo "all passed" || { echo "FAILED — app log: $WORK/app.log (KEEP=1 keeps it)"; exit 1; }
