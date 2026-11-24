#!/bin/bash
# Regenerates data/icons/ from branding/*.svg (needs ImageMagick with librsvg).
#
# Small sizes (title bar, task bar, up to SMALL_MAX px) use the simplified tile with only the book,
# because the mouse turns into mush there; larger sizes (launcher/search results, desktop) use the
# full logo with the mouse. The scalable SVG is the full logo. Change SMALL_MAX to move the line.
set -euo pipefail
cd "$(dirname "$0")/.."
ID=io.github.altay301.Nezuyomi
OUT=data/icons/hicolor
SMALL_MAX=24
for size in 16 22 24 32 48 64 128 256 512; do
    if [ "$size" -le "$SMALL_MAX" ]; then src=branding/logo-tile-small.svg; else src=branding/logo-tile.svg; fi
    mkdir -p "$OUT/${size}x${size}/apps"
    magick -background none -density 768 "$src" -resize "${size}x${size}" "PNG32:$OUT/${size}x${size}/apps/$ID.png"
done
mkdir -p "$OUT/scalable/apps"
cp branding/logo-tile.svg "$OUT/scalable/apps/$ID.svg"
echo "icons written to $OUT (book up to ${SMALL_MAX} px, mouse above)"
