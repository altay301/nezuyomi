<p align="center">
  <img src="data/icons/hicolor/512x512/apps/io.github.altay301.MangaReader.png" alt="Manga Reader logo" width="360">
</p>

<h1 align="center">Manga Reader</h1>

<p align="center">
  A lightweight, extension-based manga and webtoon reader for Linux, built for tablet mode with touch and pen.
</p>

---

## Highlights

- **Smooth vertical scrolling** — a custom `StripView` reader that tiles long strips, decodes off the main thread and keeps frames under 16.6 ms on an Intel UHD 620
- **Tablet first** — kinetic touch scrolling, pinch zoom, pen scrolling, true full screen; a triple tap opens the control panel
- **Low heat** — no blur, shadows or animations; ~0% CPU when idle
- **Extensions** — sources are small sandboxed JavaScript files running in QuickJS; the heavy lifting (HTTP, HTML parsing, images) happens in Rust
- **Retro look** — a late-90s "platinum" desktop theme in plain GTK4 CSS

## Built with

Rust · GTK4 (`gtk4-rs`) · QuickJS (`rquickjs`) · SQLite (`rusqlite`)

## Building

Requires Rust (stable), GTK 4.14+ development files and `mold` (Fedora: `sudo dnf install gtk4-devel gcc mold`).

```sh
cargo build --release
./target/release/manga-reader
```

Install the desktop entry and icons:

```sh
./tools/install-desktop.sh
```

## Status

Early development. See [PLAN.md](PLAN.md) for the full design, phases and measurements.
