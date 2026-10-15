#!/usr/bin/env bash
# Add the Linux build to a Mulpex release. Runs ON THE MAC — release.sh calls it
# while its release is still a draft, and publishes once this succeeds.
#
# The updater signing key never leaves this Mac: the Linux host only compiles.
#
#   1. ships this checkout's HEAD to the Linux host (git archive, so exactly the
#      committed code, nothing uncommitted) and builds the .deb + .AppImage there;
#   2. copies both back here and signs each with ~/.mulpex/updater.key;
#   3. uploads them to the SAME GitHub release release.sh created (a draft until
#      this finishes), and adds two
#      platforms to its latest.json:
#        linux-x86_64       → the AppImage (what an AppImage install updates from)
#        linux-x86_64-deb   → the .deb     (the updater looks this key up FIRST on a
#                                           deb install; without it a deb install
#                                           would fall back to the AppImage entry
#                                           and try to dpkg an AppImage)
#
#   scripts/release-linux.sh --host gidi@89.42.24.40            # build + publish
#   scripts/release-linux.sh --host gidi@89.42.24.40 --dry-run  # build + sign, upload nothing
#
# The host needs: Rust (rustup), Node via nvm, and the Tauri apt deps
# (docs/packaging.md, "Linux").
set -euo pipefail
cd "$(dirname "$0")/.."

KEY="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.mulpex/updater.key}"
REPO="gididaf/mulpex"
HOST="${MULPEX_LINUX_HOST:-}"
REMOTE_DIR="mulpex-release"
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$HOST" ] || { echo "ERROR: --host user@machine (or MULPEX_LINUX_HOST) is required" >&2; exit 2; }
[ -f "$KEY" ] || { echo "ERROR: no updater signing key at $KEY" >&2; exit 1; }

VERSION="$(node -p "require('./src-tauri/tauri.conf.json').version")"
TAG="v$VERSION"

if [ "$DRY_RUN" -eq 0 ]; then
  # The macOS half must already be out: this only ADDS to that release, and a
  # latest.json without darwin-aarch64 would stop every Mac from updating.
  gh release view "$TAG" -R "$REPO" >/dev/null 2>&1 || {
    echo "ERROR: release $TAG does not exist — run scripts/release.sh first." >&2
    exit 1
  }
  [ -z "$(git status --porcelain)" ] || { echo "ERROR: working tree is dirty." >&2; exit 1; }
fi

if [ "$DRY_RUN" -eq 1 ]; then DRY_LABEL=" [dry run]"; else DRY_LABEL=""; fi
echo "==> Linux build of Mulpex $VERSION on $HOST$DRY_LABEL"

# ---- build on the Linux host -------------------------------------------------
# A fresh tree each time: a stale src-tauri/binaries/ helper or node_modules from
# another version is exactly the kind of thing that ships silently.
git archive --format=tar HEAD | ssh "$HOST" "rm -rf ~/$REMOTE_DIR && mkdir -p ~/$REMOTE_DIR && tar -x -C ~/$REMOTE_DIR"
# `bash -lc` + explicit sources: a non-interactive ssh command reads neither
# .bashrc (where nvm installs itself) nor necessarily ~/.cargo/env.
ssh "$HOST" "bash -lc 'set -e; source ~/.nvm/nvm.sh; source ~/.cargo/env; cd ~/$REMOTE_DIR && npm ci && npm run tauri build -- --bundles deb,appimage'"

OUT="target/release/bundle/linux"
rm -rf "$OUT" && mkdir -p "$OUT"
APPIMAGE="Mulpex_${VERSION}_amd64.AppImage"
DEB="Mulpex_${VERSION}_amd64.deb"
scp "$HOST:~/$REMOTE_DIR/target/release/bundle/appimage/$APPIMAGE" "$OUT/"
scp "$HOST:~/$REMOTE_DIR/target/release/bundle/deb/$DEB" "$OUT/"

# ---- sign here ---------------------------------------------------------------
# Same key contents/password convention as release.sh.
export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
for f in "$OUT/$APPIMAGE" "$OUT/$DEB"; do
  npx tauri signer sign "$f" >/dev/null
  [ -f "$f.sig" ] || { echo "ERROR: signing produced no $f.sig" >&2; exit 1; }
done

# ---- latest.json -------------------------------------------------------------
LATEST="$OUT/latest.json"
if [ "$DRY_RUN" -eq 1 ]; then
  # Merge into whatever release.sh --dry-run left behind, or a bare skeleton.
  if [ -f target/release/bundle/latest.json ]; then
    cp target/release/bundle/latest.json "$LATEST"
  else
    echo "{\"version\":\"$VERSION\",\"notes\":\"Mulpex $VERSION\",\"platforms\":{}}" > "$LATEST"
  fi
else
  gh release download "$TAG" -R "$REPO" -p latest.json -O "$LATEST" --clobber
fi

BASE="https://github.com/$REPO/releases/download/$TAG"
APPIMAGE_SIG="$(cat "$OUT/$APPIMAGE.sig")" DEB_SIG="$(cat "$OUT/$DEB.sig")" \
APPIMAGE_URL="$BASE/$APPIMAGE" DEB_URL="$BASE/$DEB" VERSION="$VERSION" node -e '
  const fs = require("fs");
  const out = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (out.version !== process.env.VERSION) {
    console.error(`ERROR: latest.json is ${out.version}, this build is ${process.env.VERSION}`);
    process.exit(1);
  }
  out.platforms["linux-x86_64"] = { signature: process.env.APPIMAGE_SIG, url: process.env.APPIMAGE_URL };
  out.platforms["linux-x86_64-deb"] = { signature: process.env.DEB_SIG, url: process.env.DEB_URL };
  fs.writeFileSync(process.argv[1], JSON.stringify(out, null, 2));
' "$LATEST"

echo "==> wrote $LATEST"
cat "$LATEST"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> dry run: not publishing. Artifacts:"
  ls -1 "$OUT"
  exit 0
fi

# ---- publish -----------------------------------------------------------------
gh release upload "$TAG" -R "$REPO" \
  "$OUT/$APPIMAGE" "$OUT/$APPIMAGE.sig" "$OUT/$DEB" "$OUT/$DEB.sig"
# Last, and --clobber: until this lands, latest.json still names only the Mac
# build, so a half-finished run leaves every install on a working update path.
gh release upload "$TAG" -R "$REPO" --clobber "$LATEST"

echo "==> added Linux to $TAG"
