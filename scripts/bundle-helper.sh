#!/usr/bin/env bash
# Stage mulpex-helper and mulpex-ptyd as Tauri "externalBin" sidecars: build them
# in release and copy them to src-tauri/binaries/ with the host target-triple
# suffix Tauri expects. Tauri then places them (suffix stripped) in
# Mulpex.app/Contents/MacOS/ and signs them as part of the bundle — so the child
# `claude` hooks/MCP can exec the helper, and the app can start the PTY daemon
# (docs/ptyd.md).
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE="$(rustc -vV | sed -n 's/host: //p')"
cargo build -p mulpex-helper -p mulpex-ptyd --release
mkdir -p src-tauri/binaries
for bin in mulpex-helper mulpex-ptyd; do
  cp "target/release/${bin}" "src-tauri/binaries/${bin}-${TRIPLE}"
  echo "staged sidecar: src-tauri/binaries/${bin}-${TRIPLE}"
done
