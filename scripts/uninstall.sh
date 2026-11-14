#!/usr/bin/env bash
# Remove what install.sh installed, for the current user.
#
# The model is 640 MB and is the expensive part of a setup, so it is kept unless
# --purge is given, together with the config and the history.
#
# Usage:
#   ./uninstall.sh
#   ./uninstall.sh --purge
set -euo pipefail

prefix="${PREFIX:-$HOME/.local}"
purge=0

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            prefix="${2:?--prefix needs a directory}"
            shift 2
            ;;
        --purge) purge=1; shift ;;
        -h | --help)
            sed -n '2,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [ "$(id -u)" = "0" ]; then
    echo "run this as your user, not root" >&2
    exit 1
fi

# Stop the daemon before removing the binary it is running from.
if [ -x "$prefix/bin/hippo-dictate" ]; then
    "$prefix/bin/hippo-dictate" quit >/dev/null 2>&1 || true
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now hippo-dictate >/dev/null 2>&1 || true
fi
rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/hippo-dictate.service"
systemctl --user daemon-reload >/dev/null 2>&1 || true

rm -rf "$prefix/libexec/hippo-dictate"
for binary in hippo-dictate hippo-dictate-daemon hippo-dictate-settings; do
    rm -f "$prefix/bin/$binary"
done
rm -f "$prefix/share/applications/hippo-dictate.desktop"
# Only the sizes this app shipped, not the whole icon theme.
for size in 16x16 22x22 24x24 32x32 48x48 64x64 128x128 256x256 512x512; do
    rm -f "$prefix/share/icons/hicolor/$size/apps/hippo-dictate.png"
done
echo "removed the binaries, the launcher and the icons"

data="${XDG_DATA_HOME:-$HOME/.local/share}/hippo-dictate"
state="${XDG_STATE_HOME:-$HOME/.local/state}/hippo-dictate"
config="${XDG_CONFIG_HOME:-$HOME/.config}/hippo-dictate"

if [ "$purge" = "1" ]; then
    rm -rf "$data" "$state" "$config"
    echo "removed $data, $state and $config"
else
    echo "kept $data (the model), $state and $config; pass --purge to remove them"
fi
