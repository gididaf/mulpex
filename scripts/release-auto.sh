#!/usr/bin/env bash
# Release Mulpex without anyone at the Mac. Runs on the VM (or anywhere with the
# GitLab token): bumps the version, commits and pushes it, then triggers the
# GitLab pipeline that runs `npm run release` on the Mac's runner
# (ci/mac-release.gitlab-ci.yml), and follows it to the end.
#
#   scripts/release-auto.sh              patch bump (0.58.3 → 0.58.4), release, wait
#   scripts/release-auto.sh minor        0.58.3 → 0.59.0   (also: major, or an exact X.Y.Z)
#   scripts/release-auto.sh --retry      release HEAD's version again, no bump — after
#                                        a failed job, once the cause is fixed; refused
#                                        if that version is already published
#   scripts/release-auto.sh --no-wait    trigger and return; follow with --wait <id>
#   scripts/release-auto.sh --wait <id>  follow a pipeline already running
#
# Exit 0 only when the release is published. A run takes 10-30 min (Mac build,
# then the Linux build back on this VM), so a claude starts it in the background.
#
# The signing identity and the updater key never leave the Mac; this side only
# holds a GitLab token for root/mulpex-release (id 67), Maintainer, scopes api +
# write_repository, in ~/.mulpex/secrets/gitlab-mulpex-release.env as
# GITLAB_MULPEX_RELEASE_TOKEN. It goes to curl on stdin, never on a command line.
# docs/packaging.md, "Unattended release".
set -euo pipefail
cd "$(dirname "$0")/.."

API="https://gitlab.dreamvps.com/api/v4"
PROJECT=67
CI_FILE="ci/mac-release.gitlab-ci.yml"
SECRETS="$HOME/.mulpex/secrets/gitlab-mulpex-release.env"
REPO="gididaf/mulpex"
POLL=15

die() { echo "ERROR: $*" >&2; exit 1; }

[ -f "$SECRETS" ] || die "no $SECRETS (GITLAB_MULPEX_RELEASE_TOKEN)"
TOKEN="$(sed -n 's/^GITLAB_MULPEX_RELEASE_TOKEN=//p' "$SECRETS" | head -1)"
[ -n "$TOKEN" ] || die "GITLAB_MULPEX_RELEASE_TOKEN is empty in $SECRETS"

# api METHOD PATH [curl args...] — the token rides in a header read from stdin.
api() {
  local m="$1" p="$2"; shift 2
  curl -fsS -X "$m" -H @- "$@" "$API$p" <<<"PRIVATE-TOKEN: $TOKEN"
}
# A JSON body goes in a temp file: stdin is taken by the token header.
BODY="$(mktemp)"; trap 'rm -f "$BODY"' EXIT

# ---- follow a pipeline -------------------------------------------------------
follow() {
  local id="$1" last="" st job
  echo "==> pipeline $id: https://gitlab.dreamvps.com/root/mulpex-release/-/pipelines/$id"
  while :; do
    st="$(api GET "/projects/$PROJECT/pipelines/$id" | jq -r .status)" || st="unreachable"
    [ "$st" != "$last" ] && { echo "    $(date -u +%H:%M:%SZ) $st"; last="$st"; }
    case "$st" in
      success)
        echo "==> released: https://github.com/$REPO/releases/latest"
        return 0 ;;
      failed|canceled|skipped)
        job="$(api GET "/projects/$PROJECT/pipelines/$id/jobs" | jq -r '.[0].id // empty')"
        if [ -n "$job" ]; then
          echo "---- last 80 lines of job $job ----" >&2
          api GET "/projects/$PROJECT/jobs/$job/trace" | tail -n 80 >&2 || true
        fi
        die "pipeline $id $st — see above. Fix the cause, then: scripts/release-auto.sh --retry" ;;
    esac
    sleep "$POLL"
  done
}

BUMP="patch"; WAIT=1; RETRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --wait) [ -n "${2:-}" ] || die "--wait needs a pipeline id"; follow "$2"; exit ;;
    --no-wait) WAIT=0; shift ;;
    --retry) RETRY=1; shift ;;
    patch|minor|major) BUMP="$1"; shift ;;
    [0-9]*.[0-9]*.[0-9]*) BUMP="$1"; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

# One release at a time from this machine.
exec 9>"$HOME/.mulpex/release-auto.lock"
flock -n 9 || die "another release-auto.sh is running"

# ---- the commit to release ---------------------------------------------------
[ "$(git rev-parse --abbrev-ref HEAD)" = "master" ] || die "not on master"
git fetch --quiet --tags origin
# Behind or diverged: pulling is a tree-wide op in a shared checkout, so it is
# never done here — whoever runs this pulls first.
git merge-base --is-ancestor origin/master HEAD \
  || die "master is behind origin/master — pull first (git pull --rebase), then rerun"

FILES=(src-tauri/tauri.conf.json src-tauri/Cargo.toml Cargo.lock)
CUR="$(node -p "require('./src-tauri/tauri.conf.json').version")"

if [ "$RETRY" -eq 1 ]; then
  VERSION="$CUR"
  [ "$(git rev-list --count origin/master..HEAD)" = "0" ] \
    || die "--retry releases pushed code only; push or drop the $(git rev-list --count origin/master..HEAD) local commit(s) first"
else
  [ -z "$(git status --porcelain -- "${FILES[@]}")" ] || die "uncommitted changes in ${FILES[*]}"
  VERSION="$(node -e '
    const [cur, b] = process.argv.slice(1);
    if (/^\d+\.\d+\.\d+$/.test(b)) { console.log(b); process.exit(0); }
    let [M, m, p] = cur.split(".").map(Number);
    if (b === "major") { M++; m = 0; p = 0; } else if (b === "minor") { m++; p = 0; } else p++;
    console.log([M, m, p].join("."));' "$CUR" "$BUMP")"
  [ "$VERSION" != "$CUR" ] || die "version is already $CUR"
fi
TAG="v$VERSION"
if gh release view "$TAG" -R "$REPO" --json isDraft -q .isDraft 2>/dev/null | grep -qx false; then
  die "$TAG is already published"
fi

if [ "$RETRY" -eq 0 ]; then
  echo "==> bumping $CUR → $VERSION"
  V="$VERSION" node -e '
    const fs = require("fs"), v = process.env.V;
    const sub = (f, re, to) => {
      const s = fs.readFileSync(f, "utf8"), t = s.replace(re, to);
      if (s === t) { console.error("no version line replaced in " + f); process.exit(1); }
      fs.writeFileSync(f, t);
    };
    sub("src-tauri/tauri.conf.json", /("version":\s*")[^"]+(")/, `$1${v}$2`);
    sub("src-tauri/Cargo.toml", /^(version = ")[^"]+(")/m, `$1${v}$2`);
    sub("Cargo.lock", /(\[\[package\]\]\nname = "mulpex"\nversion = ")[^"]+(")/, `$1${v}$2`);'
  # Only these three files, even if others in the shared tree are dirty.
  git commit --quiet -m "chore: v$VERSION" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- "${FILES[@]}"
fi
echo "==> pushing master"
git push --quiet origin master
SHA="$(git rev-parse HEAD)"

# ---- the CI file: this repo's copy is the source of truth ---------------------
ENC_PATH=".gitlab-ci.yml"
REMOTE_CI="$(api GET "/projects/$PROJECT/repository/files/$ENC_PATH/raw?ref=main" 2>/dev/null || true)"
if [ "$REMOTE_CI" != "$(cat "$CI_FILE")" ]; then
  if [ -z "$REMOTE_CI" ]; then M=POST; else M=PUT; fi
  echo "==> uploading $CI_FILE to root/mulpex-release ($M)"
  jq -n --rawfile c "$CI_FILE" --arg msg "ci: sync from mulpex $(git rev-parse --short HEAD)" \
    '{branch: "main", content: $c, commit_message: $msg}' >"$BODY"
  api "$M" "/projects/$PROJECT/repository/files/$ENC_PATH" -H "Content-Type: application/json" --data @"$BODY" >/dev/null
fi

# ---- trigger -----------------------------------------------------------------
echo "==> triggering the Mac release of $TAG ($SHA)"
jq -n --arg sha "$SHA" --arg v "$VERSION" \
  '{ref: "main", variables: [{key: "MULPEX_SHA", value: $sha}, {key: "MULPEX_VERSION", value: $v}]}' >"$BODY"
PID="$(api POST "/projects/$PROJECT/pipeline" -H "Content-Type: application/json" --data @"$BODY" | jq -r .id)"
[[ "$PID" =~ ^[0-9]+$ ]] || die "the pipeline did not start"

if [ "$WAIT" -eq 0 ]; then
  echo "==> started pipeline $PID; follow it with: scripts/release-auto.sh --wait $PID"
  exit 0
fi
follow "$PID"
