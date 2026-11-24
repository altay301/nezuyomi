#!/bin/bash
# Regenerates data/icons/ from branding/*.svg (needs ImageMagick with librsvg).
# 16-48 px use the simplified small tile, 64 px and up the full logo.
set -euo pipefail
cd "$(dirname "$0")/.."
ID=io.github.altay301.MangaReader
OUT=data/icons/hicolor
for size in 16 24 32 48; do
    mkdir -p "$OUT/${size}x${size}/apps"
    magick -background none -density 768 branding/logo-tile-small.svg -resize "${size}x${size}" "PNG32:$OUT/${size}x${size}/apps/$ID.png"
done
for size in 64 128 256 512; do
    mkdir -p "$OUT/${size}x${size}/apps"
    magick -background none -density 768 branding/logo-tile.svg -resize "${size}x${size}" "PNG32:$OUT/${size}x${size}/apps/$ID.png"
done
mkdir -p "$OUT/scalable/apps"
cp branding/logo-tile.svg "$OUT/scalable/apps/$ID.svg"
echo "icons written to $OUT"
