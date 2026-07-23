#!/usr/bin/env bash
# Stage mulpex-helper as a Tauri "externalBin" sidecar: build it in release and
# copy it to src-tauri/binaries/ with the host target-triple suffix Tauri expects.
# Tauri then places it (suffix stripped) in Mulpex.app/Contents/MacOS/mulpex-helper
# and signs it as part of the bundle — so the child `claude` hooks/MCP can exec it.
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE="$(rustc -vV | sed -n 's/host: //p')"
cargo build -p mulpex-helper --release
mkdir -p src-tauri/binaries
cp "target/release/mulpex-helper" "src-tauri/binaries/mulpex-helper-${TRIPLE}"
echo "staged sidecar: src-tauri/binaries/mulpex-helper-${TRIPLE}"
