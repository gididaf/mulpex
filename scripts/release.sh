#!/usr/bin/env bash
# Cut a Mulpex release that the in-app updater will accept.
#
# The updater does NOT consume the .dmg. It reads `latest.json` from the GitHub
# "latest" release, downloads the `Mulpex.app.tar.gz` named there, and verifies
# it against the minisign signature in that same JSON — which must match the
# pubkey baked into src-tauri/tauri.conf.json. So all four artifacts (dmg for
# first installs, tar.gz + sig + latest.json for updates) have to land on the
# SAME release, or existing installs see nothing.
#
#   scripts/release.sh                       # build, publish, notes from git log
#   scripts/release.sh --notes "what's new"  # explicit release notes
#   scripts/release.sh --dry-run             # build + write latest.json, upload nothing
#
# Version comes from src-tauri/tauri.conf.json — bump it (and Cargo.toml) first.
set -euo pipefail
cd "$(dirname "$0")/.."

KEY="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.mulpex/updater.key}"
REPO="gididaf/mulpex"
DRY_RUN=0
NOTES=""

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --notes) NOTES="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# ---- preflight -------------------------------------------------------------
# Every one of these is a failure that would otherwise only show up as "the
# update button does nothing" on a user's machine days later.

[ -f "$KEY" ] || {
  echo "ERROR: no updater signing key at $KEY" >&2
  echo "  Without it the build cannot produce a .sig and no install can verify" >&2
  echo "  an update. Generate one with:  npx tauri signer generate -w $KEY" >&2
  exit 1
}

VERSION="$(node -p "require('./src-tauri/tauri.conf.json').version")"
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml | head -1)"
[ "$VERSION" = "$CARGO_VERSION" ] || {
  echo "ERROR: version mismatch — tauri.conf.json $VERSION vs Cargo.toml $CARGO_VERSION" >&2
  exit 1
}
TAG="v$VERSION"

if [ "$DRY_RUN" -eq 0 ]; then
  # A tag that already exists means this version shipped; the updater keys off
  # the version string, so re-publishing it would be silently ignored by clients.
  if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    echo "ERROR: release $TAG already exists — bump the version first." >&2
    exit 1
  fi
  if [ -n "$(git status --porcelain)" ]; then
    echo "ERROR: working tree is dirty — commit before releasing." >&2
    exit 1
  fi
fi

echo "==> releasing Mulpex $VERSION (tag $TAG)${DRY_RUN:+ [dry run]}"

# ---- build -----------------------------------------------------------------
# The KEY CONTENTS, not the path. `tauri signer generate` advertises
# TAURI_SIGNING_PRIVATE_KEY_PATH, but the v2 bundler reads only
# TAURI_SIGNING_PRIVATE_KEY — with the path set and this unset it builds the
# .tar.gz, then fails at the signing step with "A public key has been found, but
# no private key", *after* a full release compile.
export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
npm run tauri build

BUNDLE="target/release/bundle"
TARBALL="$BUNDLE/macos/Mulpex.app.tar.gz"
SIGFILE="$TARBALL.sig"
DMG="$BUNDLE/dmg/Mulpex_${VERSION}_aarch64.dmg"

for f in "$TARBALL" "$SIGFILE" "$DMG"; do
  [ -f "$f" ] || { echo "ERROR: build did not produce $f" >&2; exit 1; }
done

# ---- latest.json -----------------------------------------------------------
# `darwin-aarch64` only: Mulpex builds Apple Silicon. An Intel Mac asking this
# endpoint finds no matching platform key and is told it's up to date, which is
# the correct outcome — better than handing it a binary it cannot run.
if [ -z "$NOTES" ]; then
  LAST_TAG="$(git describe --tags --abbrev=0 2>/dev/null || true)"
  if [ -n "$LAST_TAG" ]; then
    NOTES="$(git log --pretty=format:'- %s' "$LAST_TAG"..HEAD | head -20)"
  fi
  [ -n "$NOTES" ] || NOTES="Mulpex $VERSION"
fi

LATEST="$BUNDLE/latest.json"
SIGNATURE="$(cat "$SIGFILE")"
PUB_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
URL="https://github.com/$REPO/releases/download/$TAG/Mulpex.app.tar.gz"

VERSION="$VERSION" NOTES="$NOTES" PUB_DATE="$PUB_DATE" \
SIGNATURE="$SIGNATURE" URL="$URL" node -e '
  const out = {
    version: process.env.VERSION,
    notes: process.env.NOTES,
    pub_date: process.env.PUB_DATE,
    platforms: {
      "darwin-aarch64": {
        signature: process.env.SIGNATURE,
        url: process.env.URL,
      },
    },
  };
  require("fs").writeFileSync(process.argv[1], JSON.stringify(out, null, 2));
' "$LATEST"

echo "==> wrote $LATEST"
cat "$LATEST"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> dry run: not publishing. Artifacts:"
  ls -1 "$TARBALL" "$SIGFILE" "$DMG" "$LATEST"
  exit 0
fi

# ---- publish ---------------------------------------------------------------
# Not a draft and not a prerelease: the endpoint is /releases/latest/download/,
# which resolves to the newest *published, non-prerelease* release. A draft here
# would ship an update nobody can see.
gh release create "$TAG" -R "$REPO" \
  --title "Mulpex $TAG" \
  --notes "$NOTES" \
  "$DMG" "$TARBALL" "$SIGFILE" "$LATEST"

echo "==> published $TAG"
echo "    updater endpoint: https://github.com/$REPO/releases/latest/download/latest.json"
