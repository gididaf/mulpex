#!/usr/bin/env bash
# Remote Hosts, end to end and headless (docs/remote-hosts.md): two real Mulpex
# apps with their windows hidden — A the host, B driving it — and a local relay.
# Pre-paired, so nothing needs a click. Each scenario prints IDENTICAL when B's
# mirror of a pane is A's pane, line for line (both dump their buffers to disk
# with MULPEX_DEBUG_DUMP).
#
#   bash scripts/test-remote-hosts.sh            # build, run, clean up
#   KEEP=1 bash scripts/test-remote-hosts.sh     # leave everything running
#
# Needs: python3 with `cryptography`; with CLAUDE=1 also a logged-in `claude`
# (token in ~/.claude-token-manager/token) for the real-TUI scenario.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/mulpex-rh-XXXXXX")"
PORT="${PORT:-18799}"
RELAY_URL="http://127.0.0.1:$PORT"
BIN="$ROOT/target/headless/debug/mulpex"

# A run left behind (KEEP=1, a crash) still holds the port, and every check
# would then fail for a reason that has nothing to do with the code.
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "port $PORT is in use — a previous run still going? (PORT=… to pick another)" >&2
  exit 1
fi

echo "› building (frontend, relay, hidden-window app)"
npm run build >/dev/null
cargo build -q -p mulpex-relay
bash scripts/bundle-helper.sh >/dev/null
# The window config is baked in at build time: hidden, and never throttled.
(cd src-tauri && CARGO_TARGET_DIR=../target/headless \
  TAURI_CONFIG='{"app":{"windows":[{"title":"Mulpex","width":1280,"height":820,"visible":false,"backgroundThrottling":"disabled"}]}}' \
  cargo build -q --features tauri/custom-protocol)

mkdir -p "$WORK"/{A/remote,B/remote,proj,relay}
WORK="$WORK" RELAY_URL="$RELAY_URL" python3 - <<'EOF'
import json, base64, os, time
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives import serialization as s
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
def pair():
    k = X25519PrivateKey.generate()
    return (b64(k.private_bytes(s.Encoding.Raw, s.PrivateFormat.Raw, s.NoEncryption())),
            b64(k.public_key().public_bytes(s.Encoding.Raw, s.PublicFormat.Raw)))
w, relay = os.environ["WORK"], os.environ["RELAY_URL"]
hs, hp = pair(); ds, dp = pair(); now = int(time.time())
json.dump({"host_id": "rh-test", "relay_url": relay, "host_secret": hs, "relay_token": b64(os.urandom(32)),
           "vapid_secret": "", "enabled": True}, open(f"{w}/A/remote/config.json", "w"))
json.dump([{"id": "macB", "name": "TestMac", "public": dp, "added": now, "push": None, "kind": "desktop"}],
          open(f"{w}/A/remote/devices.json", "w"))
json.dump([{"slot": 1, "name": "vm", "renamed": True, "relay_url": relay, "host_id": "rh-test", "host_pub": hp,
            "device": "macB", "device_secret": ds, "added": now, "enabled": True}], open(f"{w}/B/remote/hosts.json", "w"))
open(f"{w}/A/open.txt", "w").write(f"{w}/proj\n")
EOF

PIDS=()
cleanup() {
  [ -n "${KEEP:-}" ] && { echo "kept running: ${PIDS[*]} (work dir $WORK)"; return; }
  kill "${PIDS[@]}" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

relay() { target/debug/mulpex-relay --listen "127.0.0.1:$PORT" --static remote/dist --data "$WORK/relay" >"$WORK/relay.log" 2>&1 & RELAY=$!; PIDS+=($RELAY); }
start_a() { MULPEX_DEBUG_DUMP=1 MULPEX_DEBUG_REMOTE=1 MULPEX_HOME="$WORK/A" MULPEX_DEBUG_TAIL_BYTES=8192 "$BIN" >"$WORK/A.log" 2>&1 & A=$!; PIDS+=($A); sleep 5; mkdir -p "$TMPDIR/mulpex-$A/1/termreq"; }
start_b() { MULPEX_DEBUG_DUMP=1 MULPEX_HOME="$WORK/B" "$BIN" >"$WORK/B.log" 2>&1 & B=$!; PIDS+=($B); }
N=0
req() { N=$((N + 1)); printf '%s' "$1" >"$TMPDIR/mulpex-$A/1/termreq/$(printf %016d $N)-t.json"; }
check() { # $1 scenario, then the terminal ids
  local name=$1; shift
  for id in "$@"; do
    if diff <(tail -n +2 "$WORK/A/dump/1-$id.txt") <(tail -n +2 "$WORK/B/dump/4294967297-$id.txt") >/dev/null 2>&1; then
      echo "  ✓ $name: term#$id IDENTICAL"
    else
      echo "  ✗ $name: term#$id DIFFERS"; FAIL=1
    fi
  done
}
FAIL=0

relay; start_a
req '{"op":"open","label":"big","seed":"seq 1 3000 | tr \"\\n\" \" \"; echo; printf \"\\033[1;33mYELLOW\\033[0m שלום עולם\\n\""}'
sleep 3; start_b; sleep 10
check "snapshot from the host's screen" 1

req '{"op":"send","id":1,"data":"for i in 1 2 3; do printf \"\\033[3${i}mline $i עברית\\033[0m\\n\"; done\r"}'
req '{"op":"open","label":"small","seed":"echo replayed from spawn"}'
sleep 4
check "live output, and replay from spawn" 1 2

if [ -n "${CLAUDE:-}" ]; then
  req '{"op":"open","label":"tui","seed":"clear; CLAUDE_CODE_OAUTH_TOKEN=$(cat $HOME/.claude-token-manager/token) claude"}'
  sleep 14
  check "a real claude TUI" 3
fi

# The Mac's own UI, driven like a user (MULPEX_DEBUG_DUMP's `drive/` queue):
# host button, ⌘O, Open this folder — the project must open ON THE HOST (it
# once opened on the Mac: the picker closed before the host was read).
drive() { printf '%s' "$1" >"$WORK/B/drive/$(date +%s%N).json"; sleep 2; }
mkdir -p "$WORK/B/drive"
drive '{"click":".hosts .host","text":"◆"}'
drive '{"menu":"open_project"}'
drive '{"click":"button.primary","text":"Open this folder"}'
sleep 3
if grep -qx "$HOME" "$WORK/A/open.txt" && ! grep -qx "$HOME" "$WORK/B/open.txt" 2>/dev/null; then
  echo "  ✓ ⌘O on the host opened the folder on the host"
else
  echo "  ✗ ⌘O on the host opened it somewhere else"; FAIL=1
fi
# Typing in that project's new terminal, through the Mac's xterm.
drive '{"menu":"new_terminal"}'; sleep 3
drive '{"termInput":"echo typed-on-the-mac שלום\r"}'; sleep 3
if grep -q "^typed-on-the-mac" "$WORK"/A/dump/2-*.txt; then echo "  ✓ typing reached the host"; else echo "  ✗ typing never arrived"; FAIL=1; fi

kill "$RELAY"; sleep 2
req '{"op":"send","id":1,"data":"echo while-the-relay-was-down\r"}'
sleep 1; relay; sleep 14
check "relay dropped and back" 1 2

kill "$B"; sleep 2; start_b; sleep 10
check "this Mac restarted" 1 2

kill "$A"; sleep 3; rm -rf "$WORK/A/dump" "$WORK/B/dump"; start_a
req '{"op":"open","label":"after","seed":"echo new run"}'
sleep 14
check "the host restarted" 1

[ "$FAIL" = 0 ] && echo "all IDENTICAL" || { echo "FAILED — logs in $WORK (KEEP=1 keeps them)"; exit 1; }
