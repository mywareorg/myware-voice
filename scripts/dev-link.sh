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
#   cargo build && myware-voice quit && myware-voice daemon &
#
# `toggle` never starts a daemon itself, so the restart has to name one.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
debug="$root/target/debug"
prefix="${PREFIX:-$HOME/.local}"

for name in myware-voice myware-voice-daemon myware-voice-settings myware-voice-overlay; do
    if [ ! -x "$debug/$name" ]; then
        echo "missing $debug/$name; run 'cargo build --workspace' first" >&2
        exit 1
    fi
done

mkdir -p "$prefix/bin"
for name in myware-voice myware-voice-daemon myware-voice-settings myware-voice-overlay; do
    ln -sfn "$debug/$name" "$prefix/bin/$name"
done

# The launcher has an absolute Exec= written by install.sh, so it would keep
# running the installed release while the keybind runs the checkout — the exact
# confusion this script exists to avoid.
entry="$prefix/share/applications/myware-voice.desktop"
if [ -f "$entry" ]; then
    sed -i "s|^Exec=.*|Exec=$debug/myware-voice-settings|" "$entry"
    echo "launcher now runs $debug/myware-voice-settings"
fi

echo "PATH now runs $debug"
echo
echo "after a rebuild, replace the running daemon:"
echo "  cargo build && myware-voice quit && myware-voice daemon &"
echo
echo "'scripts/install.sh' points these back at a release install."
