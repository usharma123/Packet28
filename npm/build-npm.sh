#!/usr/bin/env bash
set -euo pipefail

# Build platform-specific npm packages for Packet28.
# Usage: ./npm/build-npm.sh [--publish]
#
# This script:
# 1. Builds release binaries for each target
# 2. Creates per-platform npm packages (@packet28/darwin-arm64, etc.)
# 3. Stages the root packet28 package with vendor/ fallback
# 4. Optionally publishes to npm

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
NPM_DIR="$SCRIPT_DIR"
VERSION=$(grep -m1 'version' "$REPO_ROOT/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')
DIST_DIR="$REPO_ROOT/dist/npm"
PUBLISH=false

if [[ "${1:-}" == "--publish" ]]; then
  PUBLISH=true
fi

echo "Building Packet28 npm packages v${VERSION}"

# Platform configs: key=npm-suffix, value=rust-target|os|cpu
declare -A PLATFORMS=(
  ["darwin-arm64"]="aarch64-apple-darwin|darwin|arm64"
  ["darwin-x64"]="x86_64-apple-darwin|darwin|x64"
  ["linux-x64"]="x86_64-unknown-linux-musl|linux|x64"
  ["linux-arm64"]="aarch64-unknown-linux-musl|linux|arm64"
)

BINARIES=("Packet28" "packet28d")

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# --- Build release binaries for each target ---
for platform_key in "${!PLATFORMS[@]}"; do
  IFS='|' read -r target os cpu <<< "${PLATFORMS[$platform_key]}"
  echo ""
  echo "=== Building for $target ==="

  # Check if cross-compilation target is installed
  if ! rustup target list --installed | grep -q "$target"; then
    echo "Installing target $target..."
    rustup target add "$target"
  fi

  cargo build --release --target "$target" 2>&1 | tail -3

  # Create platform package
  pkg_dir="$DIST_DIR/@packet28/${platform_key}"
  mkdir -p "$pkg_dir/bin"

  for bin in "${BINARIES[@]}"; do
    cp "$REPO_ROOT/target/$target/release/$bin" "$pkg_dir/bin/"
  done

  # Generate package.json from template
  sed -e "s/PLATFORM/$platform_key/g" \
      -e "s/\"OS\"/\"$os\"/g" \
      -e "s/\"CPU\"/\"$cpu\"/g" \
      -e "s/0\.2\.0/$VERSION/g" \
      "$NPM_DIR/platform-template/package.json" > "$pkg_dir/package.json"

  echo "  Created @packet28/$platform_key with $(ls "$pkg_dir/bin/" | wc -l | tr -d ' ') binaries"
done

# --- Stage root package ---
echo ""
echo "=== Staging root packet28 package ==="
root_dir="$DIST_DIR/packet28"
cp -r "$NPM_DIR/packet28/" "$root_dir/"
sed -i.bak "s/0\.2\.0/$VERSION/g" "$root_dir/package.json" && rm -f "$root_dir/package.json.bak"
chmod +x "$root_dir/bin/packet28.js" "$root_dir/bin/packet28-mcp.js"

# Also stage vendor/ with native platform binary as fallback
native_key=""
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) native_key="darwin-arm64" ;;
  Darwin-x86_64) native_key="darwin-x64" ;;
  Linux-x86_64) native_key="linux-x64" ;;
  Linux-aarch64) native_key="linux-arm64" ;;
esac

if [[ -n "$native_key" ]]; then
  mkdir -p "$root_dir/vendor/$native_key"
  for bin in "${BINARIES[@]}"; do
    cp "$DIST_DIR/@packet28/$native_key/bin/$bin" "$root_dir/vendor/$native_key/"
  done
  echo "  Vendored native binaries for $native_key"
fi

echo ""
echo "=== Packages staged in $DIST_DIR ==="
echo ""
echo "Root package:"
echo "  $root_dir/package.json"
echo ""
echo "Platform packages:"
for platform_key in "${!PLATFORMS[@]}"; do
  pkg_dir="$DIST_DIR/@packet28/${platform_key}"
  echo "  $pkg_dir/package.json"
  ls -lh "$pkg_dir/bin/" | grep -v total | awk '{print "    " $NF " (" $5 ")"}'
done

# --- Publish ---
if $PUBLISH; then
  echo ""
  echo "=== Publishing to npm ==="

  for platform_key in "${!PLATFORMS[@]}"; do
    pkg_dir="$DIST_DIR/@packet28/${platform_key}"
    echo "Publishing @packet28/$platform_key..."
    (cd "$pkg_dir" && npm publish --access public)
  done

  echo "Publishing packet28..."
  (cd "$root_dir" && npm publish --access public)

  echo ""
  echo "Done! Users can now install with:"
  echo "  npm install -g packet28"
  echo "  bunx packet28 --version"
else
  echo ""
  echo "Dry run complete. To publish:"
  echo "  ./npm/build-npm.sh --publish"
  echo ""
  echo "To test locally:"
  echo "  cd $root_dir && npm link"
  echo "  packet28 --version"
fi
