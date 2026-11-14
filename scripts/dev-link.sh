#!/usr/bin/env bash
# Point the installed symlinks at this checkout's debug build, so the keybind
# exercises the code you just compiled instead of a release copy on disk.
#
#   ./scripts/dev-link.sh        # ~/.local/bin -> target/debug
#   ./scripts/install.sh         # back to a release install
#
# The daemon holds its binary in memory, so a rebuild changes nothing until it is
# replaced:
#
#   cargo build && hippo-dictate quit
#
# and the next key press starts the new one. `toggle` spawns a daemon when none is
# listening, so there is no daemon to start by hand and nothing to keep running.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
debug="$root/target/debug"
prefix="${PREFIX:-$HOME/.local}"

for name in hippo-dictate hippo-dictate-daemon hippo-dictate-settings hippo-dictate-overlay; do
    if [ ! -x "$debug/$name" ]; then
        echo "missing $debug/$name; run 'cargo build --workspace' first" >&2
        exit 1
    fi
done

mkdir -p "$prefix/bin"
for name in hippo-dictate hippo-dictate-daemon hippo-dictate-settings hippo-dictate-overlay; do
    ln -sfn "$debug/$name" "$prefix/bin/$name"
done

echo "PATH now runs $debug"
echo
echo "after a rebuild, replace the running daemon:"
echo "  cargo build && hippo-dictate quit"
echo "then press the key: the first toggle starts the new build."
echo
echo "'scripts/install.sh' points these back at a release install."
