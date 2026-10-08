#!/usr/bin/env bash
# Deploy the Remote Control relay and the phone web app to the relay VM.
#
#   MULPEX_RELAY_SSH=user@host scripts/deploy-relay.sh           # update
#   MULPEX_RELAY_SSH=user@host scripts/deploy-relay.sh --setup   # first time
#
# The relay is built ON the VM from the rsynced source (no cross-compiling),
# with this repo's Cargo.lock so it gets the versions tested here. The phone app
# is built here and served by the relay as static files.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=${MULPEX_RELAY_SSH:?set MULPEX_RELAY_SSH=user@host}
KEY=${MULPEX_RELAY_KEY:-$HOME/.ssh/mulpex_relay_ed25519}
DOMAIN=${MULPEX_RELAY_DOMAIN:-mulpex.dreamvps.com}
SSH="ssh -i $KEY -o BatchMode=yes"

if [ "${1:-}" = "--setup" ]; then
  $SSH "$TARGET" "DOMAIN=$DOMAIN bash -s" <scripts/relay-setup.sh
fi

npx vite build --config remote/vite.config.ts --logLevel warn

rsync -az --delete --exclude target -e "$SSH" crates/mulpex-relay/ "$TARGET:/opt/mulpex-relay/src/"
rsync -az -e "$SSH" Cargo.lock "$TARGET:/opt/mulpex-relay/src/Cargo.lock"
rsync -az --delete -e "$SSH" remote/dist/ "$TARGET:/opt/mulpex-relay/www/"

$SSH "$TARGET" 'set -e
  cd /opt/mulpex-relay/src
  "$HOME/.cargo/bin/cargo" build --release --quiet
  install -m 755 target/release/mulpex-relay /usr/local/bin/mulpex-relay
  systemctl restart mulpex-relay
  sleep 1
  systemctl is-active mulpex-relay'

code=$(curl -s -o /dev/null -w '%{http_code}' "https://$DOMAIN/")
echo "https://$DOMAIN/ → $code"
[ "$code" = 200 ]
