#!/bin/sh
# Mulpex installer.
#   curl -fsSL https://raw.githubusercontent.com/gididaf/mulpex/master/install.sh | bash
#
# Downloads the prebuilt binary for your platform from the latest GitHub release
# and installs it to ~/.local/bin (override with MULPEX_BIN_DIR). If no prebuilt
# binary matches your platform, falls back to building from source with cargo.
set -eu

REPO="gididaf/mulpex"
BIN_DIR="${MULPEX_BIN_DIR:-$HOME/.local/bin}"

c_reset=''; c_bold=''; c_green=''; c_yellow=''; c_red=''
if [ -t 1 ]; then
  c_reset='\033[0m'; c_bold='\033[1m'; c_green='\033[32m'; c_yellow='\033[33m'; c_red='\033[31m'
fi
say()  { printf "%b%s%b\n" "$c_bold" "$1" "$c_reset"; }
ok()   { printf "%b✓%b %s\n" "$c_green" "$c_reset" "$1"; }
warn() { printf "%b!%b %s\n" "$c_yellow" "$c_reset" "$1"; }
die()  { printf "%b✗%b %s\n" "$c_red" "$c_reset" "$1" >&2; exit 1; }

say "Installing mulpex…"

# --- detect platform -------------------------------------------------------
target=''
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)        target="x86_64-apple-darwin" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
    esac ;;
esac

build_from_source() {
  warn "No prebuilt binary for ${os}/${arch} — building from source (needs Rust)."
  command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs/ and re-run."
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  git clone --depth 1 "https://github.com/${REPO}.git" "$tmp/mulpex" >/dev/null 2>&1 \
    || die "git clone failed."
  ( cd "$tmp/mulpex" && cargo install --path . --locked --root "${BIN_DIR%/bin}" ) \
    || die "cargo install failed."
  ok "Built and installed to ${BIN_DIR}/mulpex"
}

if [ -z "$target" ]; then
  build_from_source
else
  # --- download prebuilt binary -------------------------------------------
  url="https://github.com/${REPO}/releases/latest/download/mulpex-${target}.tar.gz"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  say "Downloading ${target}…"
  if curl -fsSL "$url" -o "$tmp/mulpex.tar.gz" 2>/dev/null; then
    tar -xzf "$tmp/mulpex.tar.gz" -C "$tmp" || die "Failed to unpack archive."
    mkdir -p "$BIN_DIR"
    install -m 0755 "$tmp/mulpex" "$BIN_DIR/mulpex" 2>/dev/null \
      || { cp "$tmp/mulpex" "$BIN_DIR/mulpex" && chmod 0755 "$BIN_DIR/mulpex"; }
    ok "Installed to ${BIN_DIR}/mulpex"
  else
    warn "No release asset for ${target} yet."
    build_from_source
  fi
fi

# --- PATH check ------------------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "${BIN_DIR} is not on your PATH. Add this to your shell profile:"
     printf '\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR" ;;
esac

say "Done. Run mulpex from inside a project directory:"
printf "\n    cd /path/to/your/project && mulpex\n\n"
ok "mulpex installed"
