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

## TODO

- Export the PNG icon set (16–512 px; small sizes from `logo-tile-small.svg`) with `rsvg-convert` or Inkscape when the app gets its desktop entry.
