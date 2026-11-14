#!/usr/bin/env bash
# Install hippo-voice for the current user.
#
# No root, no system-wide files: the binaries and the sherpa runtime go under
# $PREFIX/libexec/hippo-voice, and the only thing on PATH is a symlink. The
# binaries are looked up beside this script (an extracted release) or in
# target/release (a checkout).
#
# Usage:
#   ./install.sh                    # ~/.local
#   ./install.sh --prefix /some/dir
#   ./install.sh --systemd          # also install a user unit; read the caveat
#   ./install.sh --no-icons
set -euo pipefail

prefix="${PREFIX:-$HOME/.local}"
source_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
with_systemd=0
with_icons=1

usage() {
    sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            prefix="${2:?--prefix needs a directory}"
            shift 2
            ;;
        --systemd) with_systemd=1; shift ;;
        --no-icons) with_icons=0; shift ;;
        -h | --help) usage; exit 0 ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$prefix" in
    /*) ;;
    *)
        echo "--prefix must be an absolute path (got $prefix)" >&2
        exit 2
        ;;
esac

if [ "$(id -u)" = "0" ]; then
    echo "run this as your user, not root: it installs into $prefix" >&2
    exit 1
fi

# Where the three binaries and the sherpa runtime are coming from. A release
# folder has them beside the script; a checkout has them under target/release and
# vendor/sherpa/lib.
if [ -x "$source_dir/hippo-voice" ]; then
    bin_dir="$source_dir"
elif [ -x "$source_dir/../target/release/hippo-voice" ]; then
    bin_dir="$(cd "$source_dir/../target/release" && pwd)"
else
    echo "no hippo-voice binary beside $source_dir or in target/release" >&2
    echo "run 'cargo build --release -p hippo-voice -p hippo-voice-settings' first" >&2
    exit 1
fi

if [ -d "$source_dir/lib" ]; then
    lib_dir="$source_dir/lib"
elif [ -d "$source_dir/../vendor/sherpa/lib" ]; then
    lib_dir="$(cd "$source_dir/../vendor/sherpa/lib" && pwd)"
else
    echo "no sherpa runtime found; run scripts/fetch-sherpa.sh first" >&2
    exit 1
fi

for required in hippo-voice hippo-voice-daemon hippo-voice-settings; do
    if [ ! -x "$bin_dir/$required" ]; then
        echo "missing $bin_dir/$required; build the whole workspace" >&2
        exit 1
    fi
done

dest="$prefix/libexec/hippo-voice"
mkdir -p "$dest/lib" "$prefix/bin"

install -m 755 "$bin_dir/hippo-voice" "$dest/hippo-voice"
install -m 755 "$bin_dir/hippo-voice-daemon" "$dest/hippo-voice-daemon"
install -m 755 "$bin_dir/hippo-voice-settings" "$dest/hippo-voice-settings"
# Every .so in lib/: sherpa resolves onnxruntime through its own $ORIGIN rpath, so
# they have to stay together.
install -m 644 "$lib_dir"/*.so "$dest/lib/"

for binary in hippo-voice hippo-voice-daemon hippo-voice-settings; do
    ln -sfn "../libexec/hippo-voice/$binary" "$prefix/bin/$binary"
done
echo "installed to $dest"

# The icons and the desktop entry: a menu entry for the settings window, since
# the daemon has no window of its own.
source_tree() {
    for candidate in "$source_dir/$1" "$source_dir/../packaging/$1"; do
        if [ -e "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

if [ "$with_icons" = "1" ]; then
    if icons="$(source_tree hicolor)"; then
        mkdir -p "$prefix/share/icons"
        cp -R "$icons" "$prefix/share/icons/"
        echo "icons installed to $prefix/share/icons/hicolor"
        if command -v gtk-update-icon-cache >/dev/null 2>&1; then
            gtk-update-icon-cache -q -t -f "$prefix/share/icons/hicolor" || true
        fi
    else
        echo "no hicolor tree found; skipping icons" >&2
    fi
fi

if desktop="$(source_tree hippo-voice.desktop)"; then
    mkdir -p "$prefix/share/applications"
    install -m 644 "$desktop" "$prefix/share/applications/hippo-voice.desktop"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$prefix/share/applications" || true
    fi
else
    echo "no desktop entry found; skipping the launcher" >&2
fi

# The model is 640 MB and is not something an installer should decide to fetch.
if [ ! -d "${XDG_DATA_HOME:-$HOME/.local/share}/hippo-voice/models/parakeet-v3-int8" ] &&
    [ ! -d "$source_dir/../vendor/models/parakeet-v3-int8" ]; then
    echo
    echo "next: hippo-voice model download    # 640 MB, resumable"
fi

if [ "$with_systemd" = "1" ]; then
    unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    mkdir -p "$unit_dir"
    unit_src="$(source_tree systemd/hippo-voice.service)" || {
        echo "no unit file found; skipping --systemd" >&2
        exit 0
    }
    sed "s|%h/\.local/libexec/hippo-voice|$dest|" "$unit_src" >"$unit_dir/hippo-voice.service"
    echo "unit written to $unit_dir/hippo-voice.service"
    echo
    echo "A user unit does not inherit the compositor's environment on Wayland."
    echo "Add this to ~/.config/sway/config before starting it:"
    echo "  exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK"
    echo "  exec systemctl --user enable --now hippo-voice"
    echo "or skip the unit and use:  exec hippo-voice daemon"
fi

case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on PATH; add it before the keybind can find hippo-voice" ;;
esac

echo
echo "bind it in ~/.config/sway/config:"
echo "  exec hippo-voice daemon"
echo "  bindsym --release --no-repeat ctrl+space exec hippo-voice toggle"
echo "  bindsym --release --no-repeat ctrl+shift+space exec hippo-voice stream"
echo
echo "then: hippo-voice doctor"
