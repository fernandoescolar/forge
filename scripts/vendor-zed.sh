#!/usr/bin/env bash
# Moves the vendor/zed submodule to another Zed tag or commit.
#   scripts/vendor-zed.sh v0.234.0
# Afterwards, refresh the integration workspace (see docs/ZED_INTEGRATION.md) and commit
# the submodule pointer together with Cargo.toml / Cargo.lock.
set -euo pipefail
REV="${1:?usage: scripts/vendor-zed.sh <zed tag or commit>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
git submodule update --init --depth 1 vendor/zed
git -C vendor/zed fetch --depth 1 origin "$REV"
# Forge's changes to Zed live in patches/zed; drop them here and re-apply them on the new rev.
git -C vendor/zed reset -q --hard
git -C vendor/zed checkout -q FETCH_HEAD
echo "vendor/zed now at $(git -C vendor/zed rev-parse --short HEAD) ($REV)"
scripts/apply-zed-patches.sh
echo "Next: update [patch]/[profile] sections and Cargo.lock from vendor/zed, build, test, then:"
echo "  git add vendor/zed Cargo.toml Cargo.lock && git commit -m \"Update Zed to $REV\""
