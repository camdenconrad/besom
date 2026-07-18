#!/usr/bin/env bash
# Build a cFS carrying the Besom timebase, the OSAL simulated-time changes and besom_io.
#
# This is the README's build instructions, executable. Keeping it a script rather than prose
# in two places is the point: CI runs exactly what a human is told to run, so the instructions
# cannot quietly rot while the harness keeps working on the author's machine.
#
# Usage:  ci/build-cfs.sh [dest]      (default: ./.cfs)
# Prints the directory to put in $BESOM_CFS_DIR on success.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-$REPO_ROOT/.cfs}"

# Pinned upstream base. This is the nasa/cFS bundle commit the Besom patches were developed
# against -- NOT a moving branch. cFS integration candidates land often and an unpinned clone
# would turn "did my change break determinism?" into "did the flight software change under me?",
# which is the one question this harness exists to be able to answer cleanly.
CFS_REPO="https://github.com/nasa/cFS.git"
CFS_REF="${CFS_REF:-d74cc5e}"

if [ -x "$DEST/build-native_std/exe/cpu1/core-cpu1" ]; then
    echo "cFS already built at $DEST" >&2
    echo "$DEST/build-native_std/exe/cpu1"
    exit 0
fi

if [ ! -d "$DEST/.git" ]; then
    echo "--- cloning cFS @ $CFS_REF ---" >&2
    git clone --filter=blob:none "$CFS_REPO" "$DEST"
    git -C "$DEST" checkout "$CFS_REF"
fi

echo "--- submodules ---" >&2
git -C "$DEST" submodule update --init --recursive

echo "--- besom_io app ---" >&2
rm -rf "$DEST/apps/besom_io"
cp -r "$REPO_ROOT/cfs/besom_io" "$DEST/apps/besom_io"

# Idempotent: `git apply` fails on an already-patched tree, so check first. Re-running this
# script after a partial failure should not need a manual `git checkout .`.
apply_patch() { # subdir, patch
    local sub="$1" patch="$2"
    if git -C "$DEST/$sub" apply --check "$patch" 2>/dev/null; then
        git -C "$DEST/$sub" apply "$patch"
        echo "    applied $(basename "$patch") to $sub" >&2
    elif git -C "$DEST/$sub" apply --reverse --check "$patch" 2>/dev/null; then
        echo "    $(basename "$patch") already applied to $sub" >&2
    else
        echo "!!! $(basename "$patch") does not apply to $sub (wrong CFS_REF?)" >&2
        exit 1
    fi
}

echo "--- patches ---" >&2
apply_patch psp  "$REPO_ROOT/patches/psp-timebase-besom.patch"
apply_patch osal "$REPO_ROOT/patches/osal-simulated-time.patch"
apply_patch .    "$REPO_ROOT/patches/cfs-mission-config.patch"
# Upstream nasa/PSP typo; -Werror=header-guard fails the coverage targets that `make install`
# builds. Not a Besom change -- see patches/psp-header-guard.patch.
apply_patch psp  "$REPO_ROOT/patches/psp-header-guard.patch"

echo "--- building ---" >&2
cd "$DEST"
# cmake >= 4 dropped compatibility with the <3.5 minimums cFS declares.
CMAKE_POLICY_VERSION_MINIMUM=3.5 make native_std.install

EXE="$DEST/build-native_std/exe/cpu1"
[ -x "$EXE/core-cpu1" ] || { echo "build produced no core-cpu1" >&2; exit 1; }
echo "$EXE"
