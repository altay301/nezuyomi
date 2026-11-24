# Branding

App icon, based on a hand-drawn sketch of a mouse holding a book.

| File | Use |
|---|---|
| `logo-tile.svg` | Main app icon, 64 px and up: retro platinum tile with the mouse |
| `logo-tile-small.svg` | App icon at 48 px and below: the same tile with only the book |
| `logo-line.svg` | Line-art alternative, transparent background |

Palette: tile grey `#DDDDDD`, bevel `#FFFFFF` / `#888888`, background lavender `#CCCCFF`, book `#D1564D`, ink `#1a1a2e`.

## Preview

`preview.template.html` shows all variants at several sizes. Build a self-contained `preview.html` (SVGs inlined) with:

```powershell
./branding/build-preview.ps1
```

`preview.html` is generated and not committed.

## Using the icon

- `tools/export-icons.sh` renders `data/icons/hicolor/`: up to 24 px (title bar, task bar) from `logo-tile-small.svg` (book only), larger sizes and the scalable SVG from `logo-tile.svg` (the mouse). `SMALL_MAX` in the script moves that line. Needs ImageMagick with librsvg.
- `data/io.github.altay301.Nezuyomi.desktop` is the launcher entry; the file name equals the GTK application id, which is how Plasma on Wayland matches the window to the icon.
- `tools/install-desktop.sh` installs both for the current user under `~/.local/share` (no root) and points the launcher at this checkout's release binary.
- The app sets the same icon name as default window icon and also searches `data/icons` of the source tree, so `cargo run` shows it without installing.
