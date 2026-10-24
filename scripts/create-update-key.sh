#!/usr/bin/env bash
# Creates the key pair Forge's releases are signed with (Ed25519). Updates verify every
# download against the public key built into Forge, so a release asset swapped on GitHub
# (or a stolen token) can't make Forge install it.
#   scripts/create-update-key.sh                  writes update-key.pem here and prints what to set
#   scripts/create-update-key.sh --github         also sets the secret and the variable with `gh`
#   scripts/create-update-key.sh --public-of K    prints the public key of K, as Forge reads it
# Keep update-key.pem out of the repository, somewhere safe: losing it means releases signed
# with a new key, which Forges built with the old one refuse (they must be updated by hand).
set -euo pipefail

# The public key as Forge reads it: base64 of its 32 raw bytes (the end of its DER form).
public_of() {
    openssl pkey -in "$1" -pubout -outform DER | tail -c 32 | base64
}

if [[ "${1:-}" == "--public-of" ]]; then
    public_of "${2:?usage: create-update-key.sh --public-of <key.pem>}"
    exit 0
fi

KEY=update-key.pem
if [[ -e "$KEY" ]]; then
    echo "$KEY already exists; move it away first" >&2
    exit 1
fi
openssl genpkey -algorithm ed25519 -out "$KEY"
chmod 600 "$KEY"
PUBLIC="$(public_of "$KEY")"
echo "Created $KEY (the private key: keep it safe, out of the repository)."
echo "Public key: $PUBLIC"
if [[ "${1:-}" == "--github" ]]; then
    gh secret set UPDATE_SIGNING_KEY < "$KEY"
    gh variable set UPDATE_PUBLIC_KEY --body "$PUBLIC"
    echo "Set the UPDATE_SIGNING_KEY secret and the UPDATE_PUBLIC_KEY variable."
else
    echo "Set the repository secret UPDATE_SIGNING_KEY to the contents of $KEY,"
    echo "and the repository variable UPDATE_PUBLIC_KEY to $PUBLIC."
fi
