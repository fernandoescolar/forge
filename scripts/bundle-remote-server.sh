#!/usr/bin/env bash
# Builds dist/forge-remote-server-<version>-linux-<arch>.gz: the server Forge runs on Linux
# hosts it opens folders on over SSH (crates/forge-remote-server). Releases publish it next to
# the app; Forge downloads it on the host, or here and uploads it (forge-update's
# remote_server module), into ~/.forge_server/.
#
#   scripts/bundle-remote-server.sh
#   FORGE_TARGET=aarch64-unknown-linux-gnu scripts/bundle-remote-server.sh   # another architecture
#
# It is built in a cargo invocation of its own, as Zed builds its remote server: built together
# with the app, it would get the app's features (forge-languages' editor features, among others).
# It links against glibc like the app does (2.35 or later on the host).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)"/\1/')"
TARGET="${FORGE_TARGET:-}"
target_args=()
BINARY="target/release/forge-remote-server"
if [ -n "$TARGET" ]; then
  target_args=(--target "$TARGET")
  BINARY="target/$TARGET/release/forge-remote-server"
  ARCH="${TARGET%%-*}"
else
  ARCH="$(uname -m | sed 's/^arm64$/aarch64/')"
fi

echo "==> cargo build --release -p forge-remote-server ${target_args[*]:-}"
cargo build --release -p forge-remote-server ${target_args[@]+"${target_args[@]}"}

# Named like forge_update::remote_server::asset_name.
ASSET="$ROOT/dist/forge-remote-server-$VERSION-linux-$ARCH.gz"
mkdir -p "$ROOT/dist"
# Next to the asset rather than in $TMPDIR, which may not allow running programs.
STRIPPED="$ROOT/dist/.forge-remote-server-$ARCH"
trap 'rm -f "$STRIPPED"' EXIT
cp "$BINARY" "$STRIPPED"
strip "$STRIPPED" 2>/dev/null || true
"$STRIPPED" version > /dev/null
gzip -9 -c "$STRIPPED" > "$ASSET"
echo "==> done: $ASSET ($(du -h "$ASSET" | cut -f1))"
