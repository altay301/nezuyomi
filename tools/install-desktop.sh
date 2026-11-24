#!/bin/bash
# Installs the launcher entry and icons for the current user (no root needed):
#   ~/.local/share/applications/io.github.altay301.MangaReader.desktop
#   ~/.local/share/icons/hicolor/...
# The entry starts the release binary of this checkout; run `cargo build --release` first.
set -euo pipefail
cd "$(dirname "$0")/.."
ID=io.github.altay301.MangaReader
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$DATA/applications" "$DATA/icons"
cp -r data/icons/hicolor "$DATA/icons/"
sed "s|^Exec=.*|Exec=$PWD/target/release/manga-reader|" "data/$ID.desktop" > "$DATA/applications/$ID.desktop"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$DATA/icons/hicolor" || true
command -v update-desktop-database >/dev/null && update-desktop-database -q "$DATA/applications" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
echo "installed $ID for $USER (icon + launcher)"
