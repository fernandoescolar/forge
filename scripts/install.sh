#!/usr/bin/env bash
# Installs or updates Forge on macOS from its latest GitHub release:
#
#   curl -fsSL https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.sh | bash
#
# It downloads Forge-<version>-<arch>.zip for this Mac, checks it is a validly signed Forge,
# quits a running Forge, puts Forge.app in /Applications (~/Applications when /Applications
# isn't writable) and adds a `forge` command to ~/.local/bin. Downloaded this way, macOS
# doesn't quarantine the app, so it opens without Gatekeeper's prompts.
#
#   FORGE_VERSION=0.0.2        install that version instead of the latest
#   FORGE_INSTALL_DIR=<dir>    where Forge.app goes
#   FORGE_BIN_DIR=<dir>        where the `forge` command goes (empty: don't add it)
#   FORGE_REPOSITORY=owner/repo
set -euo pipefail

tmp=""

main() {
  local repo="${FORGE_REPOSITORY:-fernandoescolar/forge}"
  local bundle_id="dev.forge.ide"

  [ "$(uname -s)" = "Darwin" ] || fail "Forge runs on macOS only."
  local arch
  case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
  esac

  local tag
  if [ -n "${FORGE_VERSION:-}" ]; then
    tag="v${FORGE_VERSION#v}"
  else
    say "Looking for the latest release of $repo"
    tag="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$tag" ] || fail "couldn't find the latest release of $repo"
  fi
  local version="${tag#v}"

  local install_dir="${FORGE_INSTALL_DIR:-}"
  if [ -z "$install_dir" ]; then
    if [ -w /Applications ]; then install_dir=/Applications; else install_dir="$HOME/Applications"; fi
  fi
  mkdir -p "$install_dir"
  local app="$install_dir/Forge.app"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  local zip="Forge-$version-$arch.zip"
  say "Downloading Forge $version for $arch"
  curl -fL --progress-bar "https://github.com/$repo/releases/download/$tag/$zip" -o "$tmp/$zip" || fail "couldn't download $zip from $repo's $tag release"
  ditto -x -k "$tmp/$zip" "$tmp/unpacked"
  local new="$tmp/unpacked/Forge.app"
  [ -d "$new" ] || fail "$zip has no Forge.app"
  codesign --verify --deep --strict "$new" 2>/dev/null || fail "the downloaded app's signature isn't valid"
  [ "$(defaults read "$new/Contents/Info.plist" CFBundleIdentifier 2>/dev/null)" = "$bundle_id" ] || fail "the downloaded app isn't Forge"

  if pgrep -f "$app/Contents/MacOS/forge" >/dev/null 2>&1; then
    say "Quitting Forge"
    osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true
    local waited=0
    while pgrep -f "$app/Contents/MacOS/forge" >/dev/null 2>&1; do
      [ "$waited" -lt 20 ] || fail "Forge is still running: quit it and run this again"
      sleep 0.5
      waited=$((waited + 1))
    done
  fi

  say "Installing $app"
  if [ -e "$app" ]; then
    mv "$app" "$tmp/previous.app"
  fi
  if ! ditto "$new" "$app"; then
    [ -e "$tmp/previous.app" ] && mv "$tmp/previous.app" "$app"
    fail "couldn't write $app"
  fi
  xattr -dr com.apple.quarantine "$app" 2>/dev/null || true

  local bin_dir="${FORGE_BIN_DIR-$HOME/.local/bin}"
  if [ -n "$bin_dir" ]; then
    mkdir -p "$bin_dir"
    write_cli "$bin_dir/forge" "$app"
    case ":$PATH:" in
      *":$bin_dir:"*) ;;
      *) say "Add $bin_dir to your PATH to use the forge command, e.g. in ~/.zshrc: export PATH=\"$bin_dir:\$PATH\"" ;;
    esac
  fi

  say "Forge $version is installed. Open it from $install_dir, or run: forge ."
}

# `forge [paths…]` opens those paths in Forge (the current folder without any), whether or
# not Forge is running.
write_cli() {
  cat >"$1" <<CLI
#!/usr/bin/env bash
# Opens files and folders in Forge: forge [paths…] (the current folder without any).
set -euo pipefail
app="$2"
[ "\$#" -gt 0 ] || set -- .
paths=()
for path in "\$@"; do
  if [ -d "\$path" ]; then
    paths+=("\$(cd "\$path" && pwd -P)")
  elif [ -e "\$path" ]; then
    paths+=("\$(cd "\$(dirname "\$path")" && pwd -P)/\$(basename "\$path")")
  else
    echo "forge: no such file or folder: \$path" >&2
    exit 1
  fi
done
exec open -a "\$app" "\${paths[@]}"
CLI
  chmod +x "$1"
}

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
fail() {
  printf '\033[31merror:\033[0m %s\n' "$*" >&2
  exit 1
}

# Everything runs from here, so a download cut short never runs half a script.
main "$@"
