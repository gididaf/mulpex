#!/usr/bin/env bash
# Remote-ordered updates, end to end and headless (docs/remote-hosts.md): real
# Mulpex .app bundles, a local relay, and a local update server — nothing
# touches GitHub or the installed app.
#
# Two bundles are built from this checkout: BASE (the current version) and NEXT
# (a made-up higher one), both pointed at http://127.0.0.1:$WWW/latest.json,
# which offers NEXT. A is the host, B the Mac driving it.
#
#   1. late host:  A on BASE, B on NEXT → B orders A up; A updates, restarts,
#                  and B is connected to it on NEXT.
#   2. one click:  A and B on BASE; B's "Update & Restart" updates A first,
#                  then itself; both end on NEXT and connected.
#
# With CLAUDE=1, scenario 2 also puts a real, busy claude in A: it must get the
# checkpoint stop, and the wake after A's relaunch.
#
#   bash scripts/test-remote-update.sh          # build, run, clean up
#   KEEP=1 bash scripts/test-remote-update.sh   # leave the work dir
#   VISIBLE=1 CLAUDE=1 bash scripts/test-remote-update.sh
#       # for a person: windows on screen, scenario 2 only, and YOU click
#       # "Update & Restart" on B (the window whose host switcher shows ◆ vm)
#
# Needs: python3 with `cryptography`, ~/.mulpex/updater.key, and the signing
# identity in tauri.conf.json (the same as a release).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# Resolved: the updater refuses to run from a path with a symlink in it, and
# $TMPDIR is under /var → /private/var.
WORK="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/mulpex-ru-XXXXXX")" && pwd -P)"
PORT="${PORT:-18797}"
WWW="${WWW:-18798}"
RELAY_URL="http://127.0.0.1:$PORT"
CLAUDE_VERSION="$(claude --version 2>/dev/null | awk '{print $1}')"; export CLAUDE_VERSION
BASE="$(node -p "require('./src-tauri/tauri.conf.json').version")"
NEXT="${BASE%.*}.99"
KEY="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.mulpex/updater.key}"
export CARGO_TARGET_DIR="$ROOT/target/update-test"
# Run from inside a Mulpex claude, this shell carries that claude's hub identity;
# the apps under test must not inherit it.
unset CLAUDE_CODE_CHILD_SESSION CLAUDE_CODE_SESSION_ID CLAUDE_PID CLAUDE_CODE_MESSAGING_SOCKET \
  CLAUDE_CODE_MESSAGING_TOKEN MULPEX_INSTANCE_ID MULPEX_STATE_DIR MULPEX_PROJECT_DIR

for p in "$PORT" "$WWW"; do
  if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "port $p is in use — a previous run still going?" >&2
    exit 1
  fi
done

PIDS=()
cleanup() {
  pkill -f "$WORK/" 2>/dev/null || true
  kill "${PIDS[@]}" 2>/dev/null || true
  [ -n "${KEEP:-}" ] && { echo "kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

# ---- build BASE and NEXT -----------------------------------------------------
cargo build -q -p mulpex-relay
export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
export CI=true
build() { # $1 version → $WORK/build-$1/
  local v=$1 src="$WORK/src-$1"
  echo "› building v$v"
  mkdir -p "$src"
  rsync -a --exclude target --exclude node_modules --exclude .git "$ROOT/" "$src/"
  ln -s "$ROOT/node_modules" "$src/node_modules"
  # One shared target dir (the second build reuses the first's deps), but
  # bundle-helper.sh copies from ./target, so it must be reachable from here.
  mkdir -p "$CARGO_TARGET_DIR"; ln -s "$CARGO_TARGET_DIR" "$src/target"
  sed -i '' "s/^version = \"$BASE\"/version = \"$v\"/" "$src/src-tauri/Cargo.toml"
  local cfg
  cfg=$(printf '{"version":"%s","app":{"windows":[{"title":"Mulpex","width":1280,"height":820,"visible":%s,"backgroundThrottling":"disabled"}]},"bundle":{"targets":["app"]},"plugins":{"updater":{"endpoints":["http://127.0.0.1:%s/latest.json"],"dangerousInsecureTransportProtocol":true}}}' "$v" "$( [ -n "${VISIBLE:-}" ] && echo true || echo false )" "$WWW")
  (cd "$src" && npx tauri build --debug --bundles app --config "$cfg" >"$WORK/build-$v.log" 2>&1) || {
    tail -30 "$WORK/build-$v.log"; exit 1;
  }
  mkdir -p "$WORK/build-$v"
  cp -R "$CARGO_TARGET_DIR/debug/bundle/macos/Mulpex.app" "$WORK/build-$v/"
  cp "$CARGO_TARGET_DIR/debug/bundle/macos/Mulpex.app.tar.gz"* "$WORK/build-$v/"
}
build "$BASE"
build "$NEXT"

mkdir -p "$WORK/www"
cp "$WORK/build-$NEXT/Mulpex.app.tar.gz" "$WORK/www/"
SIG="$(cat "$WORK/build-$NEXT/Mulpex.app.tar.gz.sig")" NEXT="$NEXT" URL="http://127.0.0.1:$WWW/Mulpex.app.tar.gz" node -e '
  const out = { version: process.env.NEXT, notes: "test", pub_date: new Date().toISOString(),
    platforms: { "darwin-aarch64": { signature: process.env.SIG, url: process.env.URL } } };
  require("fs").writeFileSync(process.argv[1], JSON.stringify(out));
' "$WORK/www/latest.json"
# --directory puts $WORK on its command line, so cleanup's pkill finds it.
python3 -m http.server "$WWW" --bind 127.0.0.1 --directory "$WORK/www" >"$WORK/www.log" 2>&1 & PIDS+=($!)

version_of() { /usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$1/Contents/Info.plist"; }

# ---- one scenario -------------------------------------------------------------
setup() { # $1 name, $2 A's version, $3 B's version
  local d="$WORK/$1"
  mkdir -p "$d"/{A/remote,B/remote,B/drive,proj,relay}
  cp -R "$WORK/build-$2/Mulpex.app" "$d/A.app"
  cp -R "$WORK/build-$3/Mulpex.app" "$d/B.app"
  D="$d" RELAY_URL="$RELAY_URL" python3 - <<'EOF'
import json, base64, os, time
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives import serialization as s
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
def pair():
    k = X25519PrivateKey.generate()
    return (b64(k.private_bytes(s.Encoding.Raw, s.PrivateFormat.Raw, s.NoEncryption())),
            b64(k.public_key().public_bytes(s.Encoding.Raw, s.PublicFormat.Raw)))
d, relay = os.environ["D"], os.environ["RELAY_URL"]
hs, hp = pair(); ds, dp = pair(); now = int(time.time())
json.dump({"host_id": "ru-test", "relay_url": relay, "host_secret": hs, "relay_token": b64(os.urandom(32)),
           "vapid_secret": "", "enabled": True}, open(f"{d}/A/remote/config.json", "w"))
json.dump([{"id": "macB", "name": "TestMac", "public": dp, "added": now, "push": None, "kind": "desktop"}],
          open(f"{d}/A/remote/devices.json", "w"))
json.dump([{"slot": 1, "name": "vm", "renamed": True, "relay_url": relay, "host_id": "ru-test", "host_pub": hp,
            "device": "macB", "device_secret": ds, "added": now, "enabled": True}], open(f"{d}/B/remote/hosts.json", "w"))
open(f"{d}/A/open.txt", "w").write(f"{d}/proj\n")
# The claude in A gets its own config dir with the first-run dialogs (folder
# trust, onboarding, the bypass warning) answered: in real use none of them
# appear, and a claude parked on one never reads the checkpoint stop.
c = f"{d}/claude-config"; os.makedirs(c)
json.dump({"hasCompletedOnboarding": True, "lastOnboardingVersion": os.environ.get("CLAUDE_VERSION", "2.0.0"),
           "theme": "dark", "projects": {os.path.realpath(f"{d}/proj"): {"hasTrustDialogAccepted": True}}},
          open(f"{c}/.claude.json", "w"))
json.dump({"skipDangerousModePermissionPrompt": True}, open(f"{c}/settings.json", "w"))
EOF
  "$CARGO_TARGET_DIR/debug/mulpex-relay" --listen "127.0.0.1:$PORT" --static remote/dist --data "$d/relay" >"$d/relay.log" 2>&1 &
  RELAY=$!; PIDS+=($RELAY)
  CLAUDE_CONFIG_DIR="$d/claude-config" MULPEX_DEBUG_DUMP=1 MULPEX_DEBUG_REMOTE=1 MULPEX_HOME="$d/A" "$d/A.app/Contents/MacOS/mulpex" >>"$d/A.log" 2>&1 &
  PIDS+=($!); sleep 5
  MULPEX_DEBUG_DUMP=1 MULPEX_HOME="$d/B" "$d/B.app/Contents/MacOS/mulpex" >>"$d/B.log" 2>&1 &
  PIDS+=($!)
}
teardown() {
  pkill -f "$WORK/$1/" 2>/dev/null || true
  kill "$RELAY" 2>/dev/null || true
  sleep 2
}
# Wait up to $1 s for `$2` to succeed.
until_ok() { local t=$1; shift; for _ in $(seq "$t"); do "$@" && return 0; sleep 1; done; return 1; }
is_version() { [ "$(version_of "$1")" = "$2" ]; }
running() { pgrep -f "$1/Contents/MacOS/mulpex" >/dev/null; }
# B's own UI (its MULPEX_DEBUG_DUMP ui.txt) shows the host connected.
connected() {
  python3 -c 'import json,sys; sys.exit(0 if any(h["state"] == "connected" for h in json.load(open(sys.argv[1]))["hosts"]) else 1)' "$1/B/dump/ui.txt" 2>/dev/null
}
FAIL=0
ok() { echo "  ✓ $1"; }
bad() { echo "  ✗ $1"; FAIL=1; }

# ---- 1. late host ---------------------------------------------------------------
if [ -z "${VISIBLE:-}" ]; then
echo "› 1. late host (A v$BASE, B v$NEXT)"
S="$WORK/late"
setup late "$BASE" "$NEXT"
if until_ok 120 is_version "$S/A.app" "$NEXT"; then ok "A installed v$NEXT by itself"; else bad "A is still v$(version_of "$S/A.app")"; fi
if until_ok 30 running "$S/A.app"; then ok "A restarted"; else bad "A did not come back"; fi
sleep 3
if until_ok 60 connected "$S"; then ok "B is connected to A again"; else bad "B never reconnected"; fi
teardown late
fi

# ---- 2. one click ------------------------------------------------------------------
echo "› 2. one click on the Mac (A and B v$BASE)"
S="$WORK/click"
setup click "$BASE" "$BASE"
if [ -n "${CLAUDE:-}" ]; then
  sleep 5
  APID="$(pgrep -f "$S/A.app/Contents/MacOS/mulpex" | head -1)"
  mkdir -p "$TMPDIR/mulpex-$APID/1/spawn"
  printf '%s' '{"from":0,"tasks":["Run the shell command `sleep 25` three times, one after another, as separate Bash calls. Then reply DONE."]}' \
    >"$TMPDIR/mulpex-$APID/1/spawn/0001.json"
  sleep 25
fi
sleep 8
if [ -n "${VISIBLE:-}" ]; then
  echo "  → click \"Update & Restart\" on B (the window with ◆ vm in its tab bar). Waiting up to 15 min."
else
  printf '%s' '{"click":".card button.primary","text":"Update"}' >"$S/B/drive/1.json"
fi
if [ -n "${CLAUDE:-}" ]; then
  # A's scratch dir goes with A's restart, so look while it is still there.
  if until_ok 120 grep -qs "about to quit" "$TMPDIR/mulpex-$APID/1/messages.log"; then ok "the claude got the stop"; else bad "no stop message seen"; fi
fi
if until_ok "$( [ -n "${VISIBLE:-}" ] && echo 900 || echo 300 )" is_version "$S/A.app" "$NEXT"; then ok "the host updated first"; else
  bad "the host is still v$(version_of "$S/A.app") — B saw:"
  python3 -c 'import json,sys; [print("     ", h.get("state"), h.get("error"), json.dumps(h.get("update"))) for h in json.load(open(sys.argv[1]))["hosts"]]' "$S/B/dump/ui.txt"
fi
if until_ok 180 is_version "$S/B.app" "$NEXT"; then ok "then the Mac updated"; else bad "the Mac is still v$(version_of "$S/B.app")"; fi
if until_ok 30 running "$S/A.app" && until_ok 30 running "$S/B.app"; then ok "both restarted"; else bad "one of them did not come back"; fi
sleep 3
if until_ok 60 connected "$S"; then ok "B is connected to A again"; else bad "B never reconnected"; fi
if [ -n "${CLAUDE:-}" ]; then
  NEW="$(pgrep -f "$S/A.app/Contents/MacOS/mulpex" | head -1)"
  if until_ok 200 grep -qs "Mulpex is back. Before it quit" "$TMPDIR/mulpex-$NEW/1/messages.log"; then ok "the claude got the wake"; else bad "no wake message"; fi
fi
teardown click

[ "$FAIL" = 0 ] && echo "all passed" || { echo "FAILED — KEEP=1 keeps $WORK"; exit 1; }
