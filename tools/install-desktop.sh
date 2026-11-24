#!/bin/bash
# Installs the launcher entry and icons for the current user (no root needed):
#   ~/.local/share/applications/io.github.altay301.Nezuyomi.desktop
#   ~/.local/share/icons/hicolor/...
# The entry starts the release binary of this checkout; run `cargo build --release` first.
# Entries and icons of the old name (io.github.altay301.MangaReader) are removed.
set -euo pipefail
cd "$(dirname "$0")/.."
ID=io.github.altay301.Nezuyomi
OLD=io.github.altay301.MangaReader
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$DATA/applications" "$DATA/icons"
rm -f "$DATA/applications/$OLD.desktop" "$DATA"/icons/hicolor/*/apps/"$OLD".*
cp -r data/icons/hicolor "$DATA/icons/"
sed "s|^Exec=.*|Exec=$PWD/target/release/nezuyomi|" "data/$ID.desktop" > "$DATA/applications/$ID.desktop"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$DATA/icons/hicolor" || true
command -v update-desktop-database >/dev/null && update-desktop-database -q "$DATA/applications" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
echo "installed $ID for $USER (icon + launcher)"
