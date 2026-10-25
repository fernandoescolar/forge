#!/usr/bin/env bash
# Builds dist/forge (a folder to install anywhere) and dist/Forge-<version>-linux-<arch>.tar.gz,
# the file releases and updates use:
#
#   forge/bin/forge                                  the app
#   forge/share/forge/extensions/<name>/             the extensions that ship with Forge
#   forge/share/applications/dev.forge.ide.desktop  the launcher entry (install.sh fills in Exec)
#   forge/share/icons/hicolor/512x512/apps/dev.forge.ide.png
#
#   scripts/bundle-linux.sh
#   FORGE_TARGET=aarch64-unknown-linux-gnu scripts/bundle-linux.sh   # for another architecture
#
# FORGE_UPDATE_REPOSITORY=owner/repo builds a Forge that updates itself from that GitHub
# repository's releases (CI sets it).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)"/\1/')"
TARGET="${FORGE_TARGET:-}"
target_args=()
BINARY="target/release/forge"
if [ -n "$TARGET" ]; then
  target_args=(--target "$TARGET")
  BINARY="target/$TARGET/release/forge"
fi

echo "==> cargo build --release ${target_args[*]:-}"
cargo build --release -p forge-native ${target_args[@]+"${target_args[@]}"}

# The extensions that ship with Forge (the same as on macOS).
BUNDLED_EXTENSIONS=(db-explorer containers forge-icons modern-icons colored-icons vscode-great-icons seti-icons)

echo "==> building and packing the bundled extensions: ${BUNDLED_EXTENSIONS[*]}"
PACKAGES="$(mktemp -d)"
trap 'rm -rf "$PACKAGES"' EXIT
for name in "${BUNDLED_EXTENSIONS[@]}"; do
  ext="extensions/$name/"
  [ -f "${ext}package.json" ] || { echo "error: no extension at $ext" >&2; exit 1; }
  if node -e "process.exit(require('./${ext}package.json').scripts?.sidecar ? 0 : 1)"; then
    if [ -n "$TARGET" ]; then
      (cd "$ext" && npm run --silent sidecar -- --target "$TARGET")
    else
      (cd "$ext" && npm run --silent sidecar -- --host-only)
    fi
  fi
  node packages/forge-api/bin/forge-ext.mjs pack "$ext" -o "$PACKAGES/$name.forgeext" >/dev/null
done

TREE="$ROOT/dist/forge"
echo "==> assembling $TREE"
rm -rf "$TREE"
mkdir -p "$TREE/bin" "$TREE/share/forge/extensions" "$TREE/share/applications" "$TREE/share/icons/hicolor/512x512/apps"
cp "$BINARY" "$TREE/bin/forge"
strip "$TREE/bin/forge" 2>/dev/null || true
for package in "$PACKAGES"/*.forgeext; do
  name="$(basename "$package" .forgeext)"
  # A .forgeext is a zip; unpacking keeps sidecars executable.
  python3 - "$package" "$TREE/share/forge/extensions/$name" <<'PY'
import os, sys, zipfile
src, dest = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(src) as z:
    for info in z.infolist():
        path = z.extract(info, dest)
        mode = info.external_attr >> 16
        if mode:
            os.chmod(path, mode & 0o777)
PY
done

python3 scripts/make-icon.py "$TREE/share/icons/hicolor/512x512/apps/dev.forge.ide.png" 512

cat > "$TREE/share/applications/dev.forge.ide.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Forge
GenericName=Code Editor
Comment=The opinionated IDE for .NET, Go, Rust and JavaScript, with agents built in
Exec=forge %F
Icon=dev.forge.ide
Terminal=false
Categories=Development;IDE;TextEditor;
MimeType=inode/directory;text/plain;
StartupWMClass=dev.forge.ide
Keywords=editor;ide;code;agents;
DESKTOP

# Named like Rust's std::env::consts::ARCH, which forge-update looks for.
if [ -n "$TARGET" ]; then
  ARCH="${TARGET%%-*}"
else
  ARCH="$(uname -m | sed 's/^arm64$/aarch64/')"
fi
TARBALL="$ROOT/dist/Forge-$VERSION-linux-$ARCH.tar.gz"
echo "==> $TARBALL"
rm -f "$TARBALL"
tar -czf "$TARBALL" -C "$ROOT/dist" forge
echo "==> done: $TREE ($(du -sh "$TREE" | cut -f1))"
