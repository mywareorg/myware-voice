#!/usr/bin/env bash
# Fetch the pinned sherpa-onnx runtime into vendor/sherpa/.
#
# Two shared objects are all the binary needs at run time:
#   libsherpa-onnx-c-api.so   the C API we dlopen
#   libonnxruntime.so         its SONAME dependency, resolved by $ORIGIN rpath
#
# Nothing is linked at build time, so `cargo build` works on a machine with no
# native artifacts at all. Run this before `cargo run` or `doctor`.
#
# On Windows use scripts/fetch-sherpa.ps1 instead: the asset name, the DLL names
# and the archive kind all differ.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock="$repo/sherpa.lock"

# Read `key = "value"` from one [section] of the lock.
read_lock() {
    awk -v want="[$1]" -v key="$2" '
        /^\[/ { in_section = ($0 == want); next }
        in_section && $1 == key {
            line = $0
            sub(/^[^=]*=[ \t]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' "$lock"
}

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$lock")"
url="$(read_lock linux url)"
sha="$(read_lock linux sha256)"
archive="$(read_lock linux archive)"

[ -n "$url" ] || { echo "sherpa.lock has no [linux] url" >&2; exit 1; }

dest="$repo/vendor"
out="$dest/sherpa"
tarball="$dest/$archive.tar.bz2"

mkdir -p "$dest"

if [ -f "$out/lib/libsherpa-onnx-c-api.so" ]; then
    echo "sherpa-onnx $version already vendored at $out"
    exit 0
fi

if [ ! -f "$tarball" ]; then
    echo "downloading sherpa-onnx $version"
    curl -fSL --retry 3 -o "$tarball" "$url"
fi

echo "$sha  $tarball" | sha256sum -c -

echo "extracting"
tar -xjf "$tarball" -C "$dest"
rm -rf "$out"
mv "$dest/$archive" "$out"

test -f "$out/lib/libsherpa-onnx-c-api.so" || {
    echo "libsherpa-onnx-c-api.so missing after extraction" >&2
    exit 1
}
echo "sherpa-onnx $version ready: $out/lib"
