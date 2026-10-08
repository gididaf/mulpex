#!/usr/bin/env bash
# One-time (and safe to re-run) setup of the Remote Control relay VM. Runs ON
# the VM as root, piped in by `deploy-relay.sh --setup`. Needs DOMAIN.
#
#   Caddy      — TLS (Let's Encrypt, automatic) in front of the relay, on 80/443
#   systemd    — runs mulpex-relay as its own unprivileged user, on localhost only
#   rustup     — the relay is built here, from the source the deploy rsyncs over
set -euo pipefail
: "${DOMAIN:?DOMAIN is required}"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq caddy build-essential pkg-config curl rsync >/dev/null

if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi

id mulpex-relay >/dev/null 2>&1 ||
  useradd --system --no-create-home --shell /usr/sbin/nologin mulpex-relay
# hosts.json lives here: which Mac owns which room (token hashes only).
install -d -o mulpex-relay -g mulpex-relay -m 700 /var/lib/mulpex-relay
install -d /opt/mulpex-relay/src /opt/mulpex-relay/www

cat >/etc/systemd/system/mulpex-relay.service <<'EOF'
[Unit]
Description=Mulpex Remote Control relay
After=network-online.target
Wants=network-online.target

[Service]
User=mulpex-relay
Group=mulpex-relay
ExecStart=/usr/local/bin/mulpex-relay --listen 127.0.0.1:8787 --static /opt/mulpex-relay/www --data /var/lib/mulpex-relay
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/mulpex-relay

[Install]
WantedBy=multi-user.target
EOF

cat >/etc/caddy/Caddyfile <<EOF
$DOMAIN {
	encode gzip
	reverse_proxy 127.0.0.1:8787
}
EOF

ufw allow 80/tcp >/dev/null
ufw allow 443/tcp >/dev/null

systemctl daemon-reload
systemctl enable --quiet mulpex-relay caddy
systemctl reload-or-restart caddy
echo "setup done"
