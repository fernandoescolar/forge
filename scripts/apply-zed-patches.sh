#!/usr/bin/env bash
# Applies Forge's changes to Zed (patches/zed/*.patch) to the vendor/zed submodule.
# Safe to run repeatedly: patches already applied are skipped.
#   scripts/apply-zed-patches.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZED="$ROOT/vendor/zed"
shopt -s nullglob
for patch in "$ROOT"/patches/zed/*.patch; do
    name="$(basename "$patch")"
    if git -C "$ZED" apply --reverse --check "$patch" 2>/dev/null; then
        echo "already applied: $name"
    elif git -C "$ZED" apply --check "$patch" 2>/dev/null; then
        git -C "$ZED" apply "$patch"
        echo "applied:         $name"
    else
        echo "error: $name no longer applies to vendor/zed ($(git -C "$ZED" rev-parse --short HEAD))." >&2
        echo "Rebase it: apply what you can with 'git -C vendor/zed apply --3way $patch'," >&2
        echo "fix the conflicts, then regenerate it with 'git -C vendor/zed diff -- <files> > $patch'." >&2
        exit 1
    fi
done
