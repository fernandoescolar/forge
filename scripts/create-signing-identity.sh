#!/usr/bin/env bash
# Creates Forge's code-signing certificate, "Forge Signing": self-signed, for the Forge.app
# you build here (scripts/bundle-macos.sh signs with it when it is in your keychain) and, with
# --github, for the releases CI builds (it goes to the repository's signing secrets).
#
# Why: macOS remembers "Always Allow" for a keychain item (a password Forge saved) by the
# app's signature. Signed ad hoc, each build or update is a different app and the keychain asks
# again for every saved password. Signed with one certificate, every build and every release
# is the same app ("dev.forge.ide" signed by it), and the answer sticks.
#
#   scripts/create-signing-identity.sh            # create it, trust it here (asks for your password)
#   scripts/create-signing-identity.sh --github   # also set the repository's secrets (needs `gh`)
#   scripts/create-signing-identity.sh --github owner/repo
#
# The certificate and its key are kept in ~/.config/forge/signing (forge-signing.p12 and its
# password). Back them up: a release signed with another certificate is a different app to the
# keychain again. On another Mac, run this script there with that folder copied over: it uses
# the saved certificate instead of making a new one.
#
# It isn't an Apple certificate: Gatekeeper doesn't vouch for it. The install script and Forge's
# updates don't go through Gatekeeper; a zip downloaded by hand still needs right-click › Open.
# For that, sign with a Developer ID instead (see .github/workflows/release.yml).
set -euo pipefail
NAME="Forge Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
STORE="$HOME/.config/forge/signing"
P12="$STORE/forge-signing.p12"
PASSWORD_FILE="$STORE/forge-signing.password"

github=0
repo=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --github)
      github=1
      if [[ $# -gt 1 && "$2" != -* ]]; then repo="$2"; shift; fi
      ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# 1. The certificate: the saved one, else a new one (10 years).
if [[ -f "$P12" && -f "$PASSWORD_FILE" ]]; then
  echo "==> using the certificate saved in $STORE"
else
  echo "==> creating \"$NAME\" (saved in $STORE)"
  mkdir -p "$STORE"
  chmod 700 "$STORE"
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  cat >"$WORK/cert.conf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
  # The system's LibreSSL writes a PKCS#12 file the keychain imports (OpenSSL 3's default
  # encryption it doesn't).
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$WORK/cert.conf" -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null
  password="$(/usr/bin/openssl rand -hex 16)"
  /usr/bin/openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" -out "$P12" -passout "pass:$password"
  printf '%s' "$password" >"$PASSWORD_FILE"
  chmod 600 "$P12" "$PASSWORD_FILE"
fi
password="$(cat "$PASSWORD_FILE")"

# 2. This Mac: import it (codesign may use its key) and trust it for code signing.
if security find-identity -v -p codesigning | grep -q "\"$NAME\""; then
  echo "==> \"$NAME\" is already in your keychain"
else
  echo "==> importing \"$NAME\" into your login keychain"
  security import "$P12" -k "$KEYCHAIN" -P "$password" -T /usr/bin/codesign >/dev/null
  cert="$(mktemp)"
  /usr/bin/openssl pkcs12 -in "$P12" -nokeys -passin "pass:$password" 2>/dev/null | /usr/bin/openssl x509 -out "$cert"
  echo "==> trusting it for code signing (macOS asks for your password)"
  security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$cert"
  rm -f "$cert"
  security find-identity -v -p codesigning | grep -q "\"$NAME\"" || { echo "error: \"$NAME\" isn't valid for code signing; check it in Keychain Access." >&2; exit 1; }
fi

# 3. The repository's release builds: the same certificate, as secrets.
if [[ $github == 1 ]]; then
  command -v gh >/dev/null || { echo "error: --github needs the GitHub CLI (gh)" >&2; exit 1; }
  if [[ -z "$repo" ]]; then
    repo="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
  fi
  echo "==> setting $repo's secrets MACOS_CERTIFICATE and MACOS_CERTIFICATE_PASSWORD"
  base64 -i "$P12" | gh secret set MACOS_CERTIFICATE --repo "$repo"
  printf '%s' "$password" | gh secret set MACOS_CERTIFICATE_PASSWORD --repo "$repo"
fi

echo "Done. scripts/bundle-macos.sh signs with \"$NAME\"$([[ $github == 1 ]] && echo ", and so do $repo's releases from the next tag on")."
echo "The first launch of a build signed with it asks for each saved password once more (\"Always Allow\"); later builds and updates don't."
