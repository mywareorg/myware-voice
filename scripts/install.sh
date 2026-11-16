#!/usr/bin/env bash
# Install myware-voice for the current user.
#
# No root, no system-wide files: the four binaries and the sherpa runtime go under
# $PREFIX/libexec/myware-voice, and the only thing on PATH is a symlink. The
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
if [ -x "$source_dir/myware-voice" ]; then
    bin_dir="$source_dir"
elif [ -x "$source_dir/../target/release/myware-voice" ]; then
    bin_dir="$(cd "$source_dir/../target/release" && pwd)"
else
    echo "no myware-voice binary beside $source_dir or in target/release" >&2
    echo "run 'cargo build --release -p myware-voice -p myware-voice-settings' first" >&2
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

for required in myware-voice myware-voice-daemon myware-voice-settings myware-voice-overlay; do
    if [ ! -x "$bin_dir/$required" ]; then
        echo "missing $bin_dir/$required; build the whole workspace" >&2
        exit 1
    fi
done

dest="$prefix/libexec/myware-voice"
mkdir -p "$dest/lib" "$prefix/bin"

install -m 755 "$bin_dir/myware-voice" "$dest/myware-voice"
install -m 755 "$bin_dir/myware-voice-daemon" "$dest/myware-voice-daemon"
install -m 755 "$bin_dir/myware-voice-settings" "$dest/myware-voice-settings"
install -m 755 "$bin_dir/myware-voice-overlay" "$dest/myware-voice-overlay"
# Every .so in lib/: sherpa resolves onnxruntime through its own $ORIGIN rpath, so
# they have to stay together.
if ! compgen -G "$lib_dir/*.so" >/dev/null; then
    echo "no shared objects in $lib_dir; run scripts/fetch-sherpa.sh" >&2
    exit 1
fi
install -m 644 "$lib_dir"/*.so "$dest/lib/"

for binary in myware-voice myware-voice-daemon myware-voice-settings myware-voice-overlay; do
    ln -sfn "../libexec/myware-voice/$binary" "$prefix/bin/$binary"
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

if desktop="$(source_tree myware-voice.desktop)"; then
    mkdir -p "$prefix/share/applications"
    # `Exec=` is made absolute here. A session started by a display manager has a
    # minimal PATH — sway's is /usr/local/bin:/usr/bin — so a bare
    # `myware-voice-settings` found nothing and the launcher did nothing at all.
    # The file is per-user already, so baking the path in costs nothing.
    sed "s|^Exec=.*|Exec=$dest/myware-voice-settings|" "$desktop" \
        >"$prefix/share/applications/myware-voice.desktop"
    chmod 644 "$prefix/share/applications/myware-voice.desktop"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$prefix/share/applications" || true
    fi
else
    echo "no desktop entry found; skipping the launcher" >&2
fi

# The model is 640 MB and is not something an installer should decide to fetch.
if [ ! -d "${XDG_DATA_HOME:-$HOME/.local/share}/myware-voice/models/parakeet-v3-int8" ] &&
    [ ! -d "$source_dir/../vendor/models/parakeet-v3-int8" ]; then
    echo
    echo "next: myware-voice model download    # 640 MB, resumable"
fi

if [ "$with_systemd" = "1" ]; then
    unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    mkdir -p "$unit_dir"
    unit_src="$(source_tree systemd/myware-voice.service)" || {
        echo "no unit file found; skipping --systemd" >&2
        exit 0
    }
    sed "s|%h/\.local/libexec/myware-voice|$dest|" "$unit_src" >"$unit_dir/myware-voice.service"
    echo "unit written to $unit_dir/myware-voice.service"
    echo
    echo "A user unit does not inherit the compositor's environment on Wayland."
    echo "Add this to ~/.config/sway/config before starting it:"
    echo "  exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK"
    echo "  exec systemctl --user enable --now myware-voice"
    echo "or skip the unit and use:  exec myware-voice daemon"
fi

case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on PATH; add it before the keybind can find myware-voice" ;;
esac

echo
echo "bind it in ~/.config/sway/config. Absolute paths: sway runs these through"
echo "\`sh\` with its own PATH, which does not include $prefix/bin."
echo "  exec $prefix/bin/myware-voice daemon"
echo "  bindsym --release --no-repeat ctrl+space exec $prefix/bin/myware-voice toggle"
echo "  bindsym --release --no-repeat ctrl+shift+space exec $prefix/bin/myware-voice stream"
echo
echo "then: myware-voice doctor"
