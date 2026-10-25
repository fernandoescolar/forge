#!/usr/bin/env bash
# Installs or updates Forge on macOS or Linux from its latest GitHub release:
#
#   curl -fsSL https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.sh | bash
#
# macOS: it downloads Forge-<version>-macos-<arch>.zip for this Mac, checks it is a validly signed
# Forge, quits a running Forge, puts Forge.app in /Applications (~/Applications when
# /Applications isn't writable). Downloaded this way, macOS doesn't quarantine the app, so it
# opens without Gatekeeper's prompts.
#
# Linux: it downloads Forge-<version>-linux-<arch>.tar.gz, checks its SHA-256 against the
# release's, puts it in ~/.local/opt/forge and adds Forge to the desktop's applications
# (~/.local/share/applications) with its icon. A running Forge keeps running; restart it
# to use the new one.
#
# Both add a `forge` command to ~/.local/bin.
#
#   FORGE_VERSION=0.0.2        install that version instead of the latest
#   FORGE_INSTALL_DIR=<dir>    where Forge.app (macOS) or the forge folder (Linux) goes
#   FORGE_BIN_DIR=<dir>        where the `forge` command goes (empty: don't add it)
#   FORGE_REPOSITORY=owner/repo
set -euo pipefail

tmp=""

main() {
  local repo="${FORGE_REPOSITORY:-fernandoescolar/forge}"
  local bundle_id="dev.forge.ide"

  local os
  case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) fail "Forge runs on macOS and Linux." ;;
  esac
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

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  if [ "$os" = linux ]; then
    install_linux "$repo" "$tag" "$version" "$arch"
  else
    install_macos "$repo" "$tag" "$version" "$arch" "$bundle_id"
  fi
}

install_macos() {
  local repo="$1" tag="$2" version="$3" arch="$4" bundle_id="$5"
  local install_dir="${FORGE_INSTALL_DIR:-}"
  if [ -z "$install_dir" ]; then
    if [ -w /Applications ]; then install_dir=/Applications; else install_dir="$HOME/Applications"; fi
  fi
  mkdir -p "$install_dir"
  local app="$install_dir/Forge.app"

  local zip="Forge-$version-macos-$arch.zip"
  say "Downloading Forge $version for $arch"
  if ! curl -fsL "https://github.com/$repo/releases/download/$tag/$zip" -o "$tmp/$zip"; then
    # Releases up to 0.0.1-rc.4 named it without the system.
    zip="Forge-$version-$arch.zip"
    curl -fL --progress-bar "https://github.com/$repo/releases/download/$tag/$zip" -o "$tmp/$zip" || fail "couldn't download Forge-$version-macos-$arch.zip from $repo's $tag release"
  fi
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
    path_hint "$bin_dir"
  fi

  say "Forge $version is installed. Open it from $install_dir, or run: forge ."
}

install_linux() {
  local repo="$1" tag="$2" version="$3" arch="$4"
  local install_dir="${FORGE_INSTALL_DIR:-$HOME/.local/opt}"
  local prefix="$install_dir/forge"
  local tarball="Forge-$version-linux-$arch.tar.gz"
  mkdir -p "$install_dir"

  say "Downloading Forge $version for Linux ($arch)"
  curl -fL --progress-bar "https://github.com/$repo/releases/download/$tag/$tarball" -o "$tmp/$tarball" || fail "couldn't download $tarball from $repo's $tag release"
  # The release's SHA256SUMS lists every download's SHA-256; the download must match it.
  local expected actual
  expected="$(curl -fsSL "https://github.com/$repo/releases/download/$tag/SHA256SUMS" 2>/dev/null | grep -E "^[0-9a-f]{64} [ *]?$tarball\$" | cut -d' ' -f1 || true)"
  if [ -z "$expected" ] && command -v python3 >/dev/null 2>&1; then
    # Releases from before SHA256SUMS: the digest GitHub lists for the asset.
    expected="$(curl -fsSL "https://api.github.com/repos/$repo/releases/tags/$tag" | python3 -c 'import json, sys
for asset in json.load(sys.stdin).get("assets", []):
    if asset.get("name") == sys.argv[1]:
        print((asset.get("digest") or "").removeprefix("sha256:"))' "$tarball")"
  fi
  [ -n "$expected" ] || fail "the release doesn't list $tarball's SHA-256"
  actual="$(sha256sum "$tmp/$tarball" | cut -d' ' -f1)"
  [ "$expected" = "$actual" ] || fail "$tarball's SHA-256 isn't the one the release lists"

  mkdir -p "$tmp/unpacked"
  tar -xzf "$tmp/$tarball" -C "$tmp/unpacked"
  local new="$tmp/unpacked/forge"
  [ -x "$new/bin/forge" ] && [ -d "$new/share/forge" ] || fail "$tarball isn't a Forge tarball"

  say "Installing $prefix"
  if [ -e "$prefix" ]; then
    mv "$prefix" "$tmp/previous"
  fi
  if ! mv "$new" "$prefix"; then
    [ -e "$tmp/previous" ] && mv "$tmp/previous" "$prefix"
    fail "couldn't write $prefix"
  fi

  # The desktop's applications menu, with the icon, starting the installed Forge.
  local data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
  mkdir -p "$data_home/applications" "$data_home/icons/hicolor/512x512/apps"
  sed "s|^Exec=forge |Exec=\"$prefix/bin/forge\" |" "$prefix/share/applications/dev.forge.ide.desktop" >"$data_home/applications/dev.forge.ide.desktop"
  cp "$prefix/share/icons/hicolor/512x512/apps/dev.forge.ide.png" "$data_home/icons/hicolor/512x512/apps/dev.forge.ide.png"
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$data_home/applications" >/dev/null 2>&1 || true

  local bin_dir="${FORGE_BIN_DIR-$HOME/.local/bin}"
  if [ -n "$bin_dir" ]; then
    mkdir -p "$bin_dir"
    write_linux_cli "$bin_dir/forge" "$prefix/bin/forge"
    path_hint "$bin_dir"
  fi

  if pgrep -f "$prefix/bin/forge" >/dev/null 2>&1; then
    say "Forge $version is installed. Restart Forge to use it."
  else
    say "Forge $version is installed. Open it from your applications, or run: forge ."
  fi
}

path_hint() {
  case ":$PATH:" in
    *":$1:"*) return ;;
  esac
  case "$(basename "${SHELL:-}")" in
    fish) say "Add $1 to your PATH to use the forge command: fish_add_path $1" ;;
    zsh) say "Add $1 to your PATH to use the forge command, in ~/.zshrc: export PATH=\"$1:\$PATH\"" ;;
    *) say "Add $1 to your PATH to use the forge command, in ~/.bashrc (or your shell's profile): export PATH=\"$1:\$PATH\"" ;;
  esac
}

# Linux: `forge [paths…]` starts Forge in the background (or hands the paths to the running
# one) and gives the terminal back.
write_linux_cli() {
  cat >"$1" <<CLI
#!/usr/bin/env bash
# Opens files and folders in Forge: forge [paths…]. Without any, Forge just opens (with the
# windows of its last session); \`forge .\` opens the current folder.
set -euo pipefail
paths=()
for path in "\$@"; do
  if [ -e "\$path" ]; then
    paths+=("\$(realpath "\$path")")
  else
    echo "forge: no such file or folder: \$path" >&2
    exit 1
  fi
done
setsid "$2" \${paths[@]+"\${paths[@]}"} </dev/null >/dev/null 2>&1 &
CLI
  chmod +x "$1"
}

# `forge [paths…]` opens those paths in Forge (the current folder without any), whether or
# not Forge is running.
write_cli() {
  cat >"$1" <<CLI
#!/usr/bin/env bash
# Opens files and folders in Forge: forge [paths…]. Without any, Forge just opens (with the
# windows of its last session); \`forge .\` opens the current folder.
set -euo pipefail
app="$2"
[ "\$#" -gt 0 ] || exec open -a "\$app"
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
