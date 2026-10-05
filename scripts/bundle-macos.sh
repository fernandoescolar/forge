#!/usr/bin/env bash
# Builds dist/Forge.app: release binary, Info.plist, icon, bundled example extensions,
# signature; then dist/Forge-<version>-<arch>.zip, the file releases and updates use.
#
#   scripts/bundle-macos.sh                 # runtime-compiled Metal shaders (no Xcode needed)
#   FORGE_PRECOMPILED_SHADERS=1 scripts/bundle-macos.sh   # needs full Xcode (`xcrun metal`)
#   FORGE_SIGN_IDENTITY="Developer ID Application: …" scripts/bundle-macos.sh
#   FORGE_NOTARY_PROFILE=forge scripts/bundle-macos.sh    # also notarizes and staples
#
# Signing with an identity uses the hardened runtime and scripts/Forge.entitlements.
# Notarizing needs a notarytool keychain profile, created once with
#   xcrun notarytool store-credentials forge --apple-id <id> --team-id <team> --password <app password>
# FORGE_UPDATE_REPOSITORY=owner/repo builds an app that updates itself from that GitHub
# repository's releases (CI sets it).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)"/\1/')"
# The app's own version fields take numbers only (0.0.1 for 0.0.1-beta); the zip keeps it all.
BUNDLE_VERSION="${VERSION%%-*}"
APP="$ROOT/dist/Forge.app"

features=()
if [ "${FORGE_PRECOMPILED_SHADERS:-0}" = "1" ]; then
  features=(--no-default-features)
fi
echo "==> cargo build --release ${features[*]:-}"
cargo build --release -p forge-native ${features[@]+"${features[@]}"}

echo "==> building and packing the bundled extensions"
PACKAGES="$(mktemp -d)"
for ext in extensions/*/; do
  name="$(basename "$ext")"
  # Extensions with sidecars build them first (`npm run sidecar`), for this Mac's architecture
  # like the app.
  if node -e "process.exit(require('./${ext}package.json').scripts?.sidecar ? 0 : 1)"; then
    (cd "$ext" && npm run --silent sidecar -- --host-only)
  fi
  node packages/forge-api/bin/forge-ext.mjs pack "$ext" -o "$PACKAGES/$name.forgeext" >/dev/null
done

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/extensions"
cp target/release/forge "$APP/Contents/MacOS/forge"
# Each extension as its package holds it: what it needs at run time, sidecars included.
for package in "$PACKAGES"/*.forgeext; do
  name="$(basename "$package" .forgeext)"
  ditto -x -k "$package" "$APP/Contents/Resources/extensions/$name"
done
rm -rf "$PACKAGES"

ICONSET="$(mktemp -d)/Forge.iconset"
mkdir -p "$ICONSET"
python3 scripts/make-icon.py "$ICONSET/icon_512x512@2x.png"
for size in 16 32 128 256 512; do
  sips -z $size $size "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Forge.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Forge</string>
  <key>CFBundleDisplayName</key><string>Forge</string>
  <key>CFBundleIdentifier</key><string>dev.forge.ide</string>
  <key>CFBundleExecutable</key><string>forge</string>
  <key>CFBundleIconFile</key><string>Forge</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$BUNDLE_VERSION</string>
  <key>CFBundleVersion</key><string>$BUNDLE_VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Folder</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSItemContentTypes</key><array><string>public.folder</string></array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>Text</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key><array><string>public.text</string><string>public.source-code</string><string>public.data</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

IDENTITY="${FORGE_SIGN_IDENTITY:--}"
echo "==> codesign (${IDENTITY})"
# Extensions' sidecars are programs too, outside the places --deep signs: sign them first.
find "$APP/Contents/Resources/extensions" -path '*/bin/*' -type f -perm -u+x -print0 | while IFS= read -r -d '' sidecar; do
  if [ "$IDENTITY" = "-" ]; then
    codesign --force --sign - "$sidecar"
  else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$sidecar"
  fi
done
if [ "$IDENTITY" = "-" ]; then
  codesign --force --deep --sign - "$APP"
else
  codesign --force --deep --options runtime --timestamp --entitlements scripts/Forge.entitlements --sign "$IDENTITY" "$APP"
fi
codesign --verify --deep --strict "$APP"

# Named like Rust's std::env::consts::ARCH, which forge-update looks for.
ARCH="$(uname -m | sed 's/^arm64$/aarch64/')"
ZIP="$ROOT/dist/Forge-$VERSION-$ARCH.zip"
if [ -n "${FORGE_NOTARY_PROFILE:-}" ]; then
  [ "$IDENTITY" != "-" ] || { echo "error: notarizing needs FORGE_SIGN_IDENTITY (a Developer ID)" >&2; exit 1; }
  echo "==> notarizing with profile $FORGE_NOTARY_PROFILE"
  rm -f "$ZIP"
  ditto -c -k --keepParent "$APP" "$ZIP"
  xcrun notarytool submit "$ZIP" --keychain-profile "$FORGE_NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
  spctl --assess --type execute --verbose "$APP"
fi
echo "==> $ZIP"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "==> done: $APP ($(du -sh "$APP" | cut -f1))"
