#!/usr/bin/env bash
# Builds dist/Forge.app: release binary, Info.plist, icon, the extensions that ship with Forge,
# signature; then dist/Forge-<version>-<arch>.zip, the file releases and updates use.
#
#   scripts/bundle-macos.sh                 # runtime-compiled Metal shaders (no Xcode needed)
#   FORGE_PRECOMPILED_SHADERS=1 scripts/bundle-macos.sh   # needs full Xcode (`xcrun metal`)
#   FORGE_SIGN_IDENTITY="Developer ID Application: …" scripts/bundle-macos.sh
#   FORGE_NOTARY_PROFILE=forge scripts/bundle-macos.sh    # also notarizes and staples
#   FORGE_TARGET=x86_64-apple-darwin scripts/bundle-macos.sh   # for another architecture
#
# Signing with a Developer ID uses the hardened runtime and scripts/Forge.entitlements.
# Without FORGE_SIGN_IDENTITY, the app is signed with "Forge Signing" when that identity is in
# the keychain (scripts/create-signing-identity.sh makes it; CI gets it from the signing secrets),
# so that every build is the same app to the keychain and "Always Allow" sticks; else ad hoc,
# and the keychain asks again after each build for every saved password.
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
# The architecture to build for: this Mac's, or FORGE_TARGET (Apple silicon builds Intel too).
TARGET="${FORGE_TARGET:-}"
target_args=()
BINARY="target/release/forge"
if [ -n "$TARGET" ]; then
  target_args=(--target "$TARGET")
  BINARY="target/$TARGET/release/forge"
fi

features=()
if [ "${FORGE_PRECOMPILED_SHADERS:-0}" = "1" ]; then
  features=(--no-default-features)
fi
echo "==> cargo build --release ${features[*]:-} ${target_args[*]:-}"
cargo build --release -p forge-native ${features[@]+"${features[@]}"} ${target_args[@]+"${target_args[@]}"}

# The extensions that ship with Forge. The others in extensions/ are examples for extension
# authors (debug builds load them all).
BUNDLED_EXTENSIONS=(db-explorer containers)

echo "==> building and packing the bundled extensions: ${BUNDLED_EXTENSIONS[*]}"
PACKAGES="$(mktemp -d)"
for name in "${BUNDLED_EXTENSIONS[@]}"; do
  ext="extensions/$name/"
  [ -f "${ext}package.json" ] || { echo "error: no extension at $ext" >&2; exit 1; }
  # Extensions with sidecars build them first (`npm run sidecar`), for the app's architecture.
  if node -e "process.exit(require('./${ext}package.json').scripts?.sidecar ? 0 : 1)"; then
    if [ -n "$TARGET" ]; then
      (cd "$ext" && npm run --silent sidecar -- --target "$TARGET")
    else
      (cd "$ext" && npm run --silent sidecar -- --host-only)
    fi
  fi
  node packages/forge-api/bin/forge-ext.mjs pack "$ext" -o "$PACKAGES/$name.forgeext" >/dev/null
done

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/extensions"
cp "$BINARY" "$APP/Contents/MacOS/forge"
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

LOCAL_IDENTITY="Forge Signing"
if [ -n "${FORGE_SIGN_IDENTITY:-}" ]; then
  IDENTITY="$FORGE_SIGN_IDENTITY"
elif security find-identity -v -p codesigning 2>/dev/null | grep -q "\"$LOCAL_IDENTITY\""; then
  IDENTITY="$LOCAL_IDENTITY"
else
  IDENTITY="-"
fi
# A Developer ID gets the hardened runtime and a timestamp (what notarizing needs); a local or
# ad-hoc signature just identifies the build.
case "$IDENTITY" in
  "Developer ID"*) RELEASE_SIGN=(--options runtime --timestamp) ;;
  *) RELEASE_SIGN=() ;;
esac
echo "==> codesign (${IDENTITY})"
# Extensions' sidecars are programs too, outside the places --deep signs: sign them first.
find "$APP/Contents/Resources/extensions" -path '*/bin/*' -type f -perm -u+x -print0 | while IFS= read -r -d '' sidecar; do
  codesign --force ${RELEASE_SIGN[@]+"${RELEASE_SIGN[@]}"} --sign "$IDENTITY" "$sidecar"
done
if [ ${#RELEASE_SIGN[@]} -gt 0 ]; then
  codesign --force --deep "${RELEASE_SIGN[@]}" --entitlements scripts/Forge.entitlements --sign "$IDENTITY" "$APP"
else
  codesign --force --deep --sign "$IDENTITY" "$APP"
fi
if [ "$IDENTITY" = "-" ]; then
  echo "    (ad hoc: the keychain will ask again for saved passwords; scripts/create-signing-identity.sh fixes that)"
fi
codesign --verify --deep --strict "$APP"

# Named like Rust's std::env::consts::ARCH, which forge-update looks for.
if [ -n "$TARGET" ]; then
  ARCH="${TARGET%%-*}"
else
  ARCH="$(uname -m | sed 's/^arm64$/aarch64/')"
fi
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
