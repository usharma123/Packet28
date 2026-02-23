#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_ROOT="${COVY_INSTALL_ROOT:-$ROOT_DIR/hyperfine/.public-cargo}"
COVY_VERSION="${COVY_VERSION:-latest}"
COVY_FORCE_INSTALL="${COVY_FORCE_INSTALL:-0}"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

if ! command -v cargo >/dev/null 2>&1; then
  fail "cargo is required to install published covy-cli."
fi

BIN_PATH="$INSTALL_ROOT/bin/covy"
mkdir -p "$INSTALL_ROOT"

if [[ -x "$BIN_PATH" && "$COVY_FORCE_INSTALL" != "1" ]]; then
  echo "$BIN_PATH"
  exit 0
fi

INSTALL_ARGS=(install covy-cli --locked --root "$INSTALL_ROOT" --force)
if [[ "$COVY_VERSION" != "latest" ]]; then
  INSTALL_ARGS+=(--version "$COVY_VERSION")
fi

echo "Installing published covy-cli from crates.io (${COVY_VERSION})..." >&2
cargo "${INSTALL_ARGS[@]}"

if [[ ! -x "$BIN_PATH" ]]; then
  fail "installation completed but binary not found at $BIN_PATH"
fi

echo "$BIN_PATH"

