#!/usr/bin/env bash
# Builds vendor/tree-sitter-c-sharp: the C# grammar from crates.io with Forge's patches
# (patches/tree-sitter-c-sharp/*.patch) and its parser regenerated. Cargo.toml points the
# grammar crate there. Needs Node (npx). Skipped when it is already built from these patches.
#   scripts/patch-grammars.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="0.23.5"
CLI="tree-sitter-cli@0.25.10"   # writes ABI 15, what Forge's tree-sitter reads
DEST="$ROOT/vendor/tree-sitter-c-sharp"
PATCHES=("$ROOT"/patches/tree-sitter-c-sharp/*.patch)
STAMP="$(cat "${PATCHES[@]}" | shasum | cut -d' ' -f1)-$VERSION"

if [[ -f "$DEST/.forge-stamp" && "$(cat "$DEST/.forge-stamp")" == "$STAMP" ]]; then
    echo "up to date:      tree-sitter-c-sharp $VERSION"
    exit 0
fi
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
curl -fsSL "https://static.crates.io/crates/tree-sitter-c-sharp/tree-sitter-c-sharp-$VERSION.crate" | tar -xz -C "$WORK"
SRC="$WORK/tree-sitter-c-sharp-$VERSION"
for patch in "${PATCHES[@]}"; do
    patch -s -p1 -d "$SRC" < "$patch"
done
# npm 12+ blocks install-time scripts of packages npx fetches unless they are allowed; the
# CLI's install script downloads the tree-sitter binary. Older npm ignores the flag.
(cd "$SRC" && npx --yes --allow-scripts=tree-sitter-cli "$CLI" generate --abi 15)
rm -rf "$DEST" && mkdir -p "$(dirname "$DEST")" && mv "$SRC" "$DEST"
echo "$STAMP" > "$DEST/.forge-stamp"
echo "built:           tree-sitter-c-sharp $VERSION with $(basename -a "${PATCHES[@]}" | tr '\n' ' ')"
