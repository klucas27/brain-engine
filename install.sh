#!/usr/bin/env bash
# Brain Engine — one-command installer.
#
# Usage:
#   ./install.sh                 # install to ~/.local/bin
#   PREFIX=/usr/local ./install.sh  # install to /usr/local/bin
#   ./install.sh --uninstall     # remove the installed binary
#
# Requirements: Rust toolchain (rustup / cargo) ≥ 1.80

set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi
info()  { printf "${BOLD}[brain]${RESET}  %s\n"       "$*"; }
ok()    { printf "${GREEN}[brain]${RESET}  %s\n"       "$*"; }
warn()  { printf "${YELLOW}[brain]${RESET}  %s\n"      "$*" >&2; }
err()   { printf "${RED}[brain] error:${RESET}  %s\n"  "$*" >&2; }
die()   { err "$*"; exit 1; }

# ── Locate the script's own directory (repo root) ────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Install prefix ────────────────────────────────────────────────────────────
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
BINARY="$BIN_DIR/brain"

# ── Uninstall path ────────────────────────────────────────────────────────────
if [[ "${1:-}" == "--uninstall" ]]; then
    if [[ -f "$BINARY" ]]; then
        rm -f "$BINARY"
        ok "Removed $BINARY"
    else
        warn "brain not found at $BINARY — nothing to do."
    fi
    DAEMON_BIN="$BIN_DIR/brain-daemon"
    if [[ -f "$DAEMON_BIN" ]]; then
        rm -f "$DAEMON_BIN"
        ok "Removed $DAEMON_BIN"
    fi
    exit 0
fi

# ── Check prerequisites ───────────────────────────────────────────────────────
info "Checking prerequisites…"

if ! command -v cargo &>/dev/null; then
    die "cargo not found. Install the Rust toolchain from https://rustup.rs and try again."
fi

CARGO_VERSION="$(cargo --version 2>/dev/null | awk '{print $2}')"
REQUIRED_MINOR=80
ACTUAL_MINOR="$(echo "$CARGO_VERSION" | awk -F. '{print $2}')"
if (( ACTUAL_MINOR < REQUIRED_MINOR )); then
    warn "cargo $CARGO_VERSION is older than the required 1.$REQUIRED_MINOR. Run 'rustup update stable' first."
fi

ok "cargo $CARGO_VERSION found."

# ── Build ─────────────────────────────────────────────────────────────────────
info "Building brain (release)…"
cd "$SCRIPT_DIR"
cargo build --release --bin brain --bin brain-daemon 2>&1

BUILT="$SCRIPT_DIR/target/release/brain"
BUILT_DAEMON="$SCRIPT_DIR/target/release/brain-daemon"
[[ -f "$BUILT" ]] || die "Build succeeded but binary not found at $BUILT"
[[ -f "$BUILT_DAEMON" ]] || die "Build succeeded but brain-daemon not found at $BUILT_DAEMON"
ok "Build complete: $BUILT, $BUILT_DAEMON"

# ── Install ───────────────────────────────────────────────────────────────────
info "Installing to $BIN_DIR…"
mkdir -p "$BIN_DIR"
install -m 0755 "$BUILT" "$BINARY"
install -m 0755 "$BUILT_DAEMON" "$BIN_DIR/brain-daemon"
ok "Installed: $BINARY"
ok "Installed: $BIN_DIR/brain-daemon"

# ── PATH check ────────────────────────────────────────────────────────────────
if ! echo ":$PATH:" | grep -q ":$BIN_DIR:"; then
    warn "$BIN_DIR is not in your PATH."
    warn "Add the following to your shell profile (e.g. ~/.bashrc or ~/.zshrc):"
    warn "  export PATH=\"$BIN_DIR:\$PATH\""
fi

# ── Smoke test ────────────────────────────────────────────────────────────────
if command -v "$BINARY" &>/dev/null || [[ -x "$BINARY" ]]; then
    VERSION="$("$BINARY" --version 2>/dev/null || echo 'unknown')"
    ok "brain $VERSION is ready."
fi

echo
info "Quick start:"
printf "  brain init          # initialise the brain for the current project\n"
printf "  brain index         # index the project (builds embeddings)\n"
printf "  brain query \"...\"   # semantic search\n"
printf "  brain daemon start  # start the background daemon\n"
printf "  brain install-hooks # wire up Claude Code hooks\n"
printf "  brain stats         # show usage metrics\n"
echo
ok "Done. See README.md for full documentation."
