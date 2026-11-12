#!/usr/bin/env bash
# Fetch the Parakeet TDT v3 int8 package into vendor/models/.
#
# 640 MB, once. Ungated (no Hugging Face token needed) and CC BY 4.0, so the
# attribution in models.lock has to reach the app's About text.
#
# Resumable: a partial file is kept as <name>.part and continued on the next
# run. Every file is verified against models.lock before it is moved into place.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock="$repo/models.lock"

base="$(sed -n 's/^base = "\(.*\)"/\1/p' "$lock")"
dir="$(sed -n 's/^dir = "\(.*\)"/\1/p' "$lock")"
dest="$repo/vendor/models/$dir"

mkdir -p "$dest"

# name<TAB>sha256 pairs, in file order.
manifest="$(awk '
    /^\[\[file\]\]/ { n=""; s=""; next }
    /^name = "/      { n=$0; sub(/^name = "/,"",n); sub(/"$/,"",n) }
    /^sha256 = "/    { s=$0; sub(/^sha256 = "/,"",s); sub(/"$/,"",s) }
    n != "" && s != "" { print n "\t" s; n=""; s="" }
' "$lock")"

[ -n "$manifest" ] || { echo "models.lock has no [[file]] entries" >&2; exit 1; }

while IFS=$'\t' read -r name sha; do
    target="$dest/$name"
    if [ -f "$target" ] && echo "$sha  $target" | sha256sum -c --status -; then
        echo "ok      $name"
        continue
    fi

    part="$target.part"
    echo "fetch   $name"
    curl -fSL --retry 3 -C - -o "$part" "$base/$name"

    if ! echo "$sha  $part" | sha256sum -c -; then
        echo "checksum mismatch for $name; keeping $part" >&2
        exit 1
    fi
    mv "$part" "$target"
done <<< "$manifest"

echo "model ready: $dest"
