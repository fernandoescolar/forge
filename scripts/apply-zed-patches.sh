#!/usr/bin/env bash
# Applies Forge's changes to Zed (patches/zed/*.patch) to the vendor/zed submodule.
# Safe to run repeatedly: patches already applied are skipped.
#   scripts/apply-zed-patches.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZED="$ROOT/vendor/zed"
shopt -s nullglob
PATCHES=("$ROOT"/patches/zed/*.patch)
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

# Copies the files the patches touch from vendor/zed to a fresh scratch dir.
reset_scratch() {
    rm -rf "$SCRATCH" && mkdir -p "$SCRATCH"
    sed -n 's|^diff --git a/.* b/||p' "${PATCHES[@]}" | sort -u | while read -r file; do
        if [[ -f "$ZED/$file" ]]; then
            mkdir -p "$SCRATCH/$(dirname "$file")" && cp "$ZED/$file" "$SCRATCH/$file"
        fi
    done
}

# Later patches can change hunks of earlier ones, so a patch can't be checked on its own:
# find the longest prefix of patches that reverse-applies as a stack, newest first, on a copy.
applied=${#PATCHES[@]}
while (( applied > 0 )); do
    reset_scratch
    ok=1
    for (( i = applied - 1; i >= 0; i-- )); do
        git -C "$SCRATCH" apply --reverse "${PATCHES[i]}" 2>/dev/null || { ok=0; break; }
    done
    (( ok )) && break
    applied=$(( applied - 1 ))
done

for (( i = 0; i < ${#PATCHES[@]}; i++ )); do
    patch="${PATCHES[i]}"
    name="$(basename "$patch")"
    if (( i < applied )); then
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
