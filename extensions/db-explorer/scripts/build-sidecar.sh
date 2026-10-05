#!/usr/bin/env bash
# Build the forge-sql sidecar for macOS and copy the binaries into extensions/db-explorer/bin/.
#
#   scripts/build-sidecar.sh             # aarch64-apple-darwin + x86_64-apple-darwin
#   scripts/build-sidecar.sh --host-only # only the current architecture
set -euo pipefail

EXT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIDECAR_DIR="$EXT_DIR/sidecar"

host_only=0
for arg in "$@"; do
  case "$arg" in
    --host-only) host_only=1 ;;
    -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

case "$(uname -m)" in
  arm64|aarch64) host_target=aarch64-apple-darwin ;;
  x86_64) host_target=x86_64-apple-darwin ;;
  *) echo "unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [[ $host_only == 1 ]]; then
  targets=("$host_target")
else
  targets=(aarch64-apple-darwin x86_64-apple-darwin)
fi

for target in "${targets[@]}"; do
  case "$target" in
    aarch64-apple-darwin) dir=darwin-arm64 ;;
    x86_64-apple-darwin) dir=darwin-x64 ;;
  esac
  echo "==> building forge-sql for $target"
  cargo build --release --locked --manifest-path "$SIDECAR_DIR/Cargo.toml" --target "$target"
  mkdir -p "$EXT_DIR/bin/$dir"
  cp "$SIDECAR_DIR/target/$target/release/forge-sql" "$EXT_DIR/bin/$dir/forge-sql"
  chmod +x "$EXT_DIR/bin/$dir/forge-sql"
  echo "    -> bin/$dir/forge-sql ($(du -h "$EXT_DIR/bin/$dir/forge-sql" | cut -f1))"
done
