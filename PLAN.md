# Manga Reader — Plan

A lightweight, extension-based manga/webtoon reader built for tablet-mode use with touch and pen.

## Target device

- ThinkPad X1 Yoga Gen 4 — 8th-gen Intel U-series, Intel UHD 620, 16 GB RAM
- Fedora Linux, KDE Plasma (Wayland)
- Folded 360° into tablet mode: touch + pen, fan mostly off
- The real constraint is **heat** (CPU/GPU load), not RAM

## Technology

| Layer | Choice | Why |
|---|---|---|
| Language | Rust (stable) | Fast, safe, single language |
| UI | GTK4 (`gtk4-rs`) + **our own retro CSS theme** (no libadwaita) | Mature touch support, kinetic scrolling, GPU rendering (GSK); libadwaita dropped because it enforces its own style |
| Reader | Custom `StripView` widget (`GtkScrollable`) inside a `GtkScrolledWindow` | No widget per image; visible textures are drawn straight to the GPU, heights are known up front → no layout jumps (see Smoothness architecture) |
| Async / network | `tokio` + `reqwest` (rustls) | |
| Images | `zune-jpeg`, `image` (png/webp), `fast_image_resize` (SIMD) | Fast decoding + downscaling to screen width |
| Database | `rusqlite` (bundled) | Library, progress, history |
| Extensions | `rquickjs` (QuickJS, sandboxed) + Rust host API (`fetch`, HTML parsing via `scraper`) | Lightweight; new sources without app updates |
| Packaging | `cargo-generate-rpm` (RPM), Flatpak later | |

## Visual style (retro desktop)

Reference: the late-90s "platinum" desktop look (grey windows, beveled 3D edges, pinstriped title bars, lavender accents). Inspired by, not copied: no logos, names or icons are reused; all icons are original.

- **Colors:** window grey `#DDDDDD`, bevel light `#FFFFFF` / dark `#888888` (1 px), accent lavender `#CCCCFF`, selection `#6666CC`, background light purple `#A8A8E0`
- **Title bars:** thin horizontal pinstripe pattern (repeating 2 px gradient), centered title
- **Buttons:** rectangular, 1–2 px raised bevel; sunken when pressed
- **Font:** an openly licensed (OFL/CC0) font with a pixel/bitmap feel, `Sans` fallback
- **Main window layout** (like a file manager window):
  - menu bar at the top (File · View · Sources · Help)
  - "Places" sidebar on the left: Library, Browse, Updates, Downloads, History, Extensions, Settings
  - cover grid in the center (each cover in a beveled frame like an icon, title below)
  - status bar at the bottom ("42 series · 3 new chapters")
- **Touch friendliness:** keep the retro look but scale it up: touch targets ≥ 44 px, larger font, thick scrollbars
- Performance bonus: flat colors + 1 px borders = no shadows or blur, almost no GPU load
- All UI strings are in English

## Reader: full screen

- When reading starts the window goes **true full screen** (`fullscreen()`): no menus, bars or borders, black background, the page fills 100% of the screen
- The mouse cursor hides after 2 s of inactivity; screen blanking is inhibited while reading (`GtkApplication::inhibit(IDLE)`)
- **Opening the control panel:**
  - touch: **triple tap** (≤ 400 ms apart, same spot ±40 px)
  - mouse: **triple click**
  - pen: side button or triple tap
  - keyboard: `Esc`
- Single and double taps **do nothing** → no accidental menus, no conflict with the triple tap (double-tap zoom removed; zoom = two-finger pinch)
- Optional (settings, off by default): top/bottom tap zones scroll one screen (delayed by the triple-tap window)
- **Control panel** = a small retro-styled window in the center of the screen (opaque, no effects):
  - title: series name · chapter
  - chapter progress bar
  - buttons: ◀ Previous chapter · Next chapter ▶ · Settings (side margins, brightness) · Back to library
  - tapping outside the panel closes it and reading continues

## Smoothness architecture (no stutter)

Goal: every frame ≤ 16.6 ms on a 60 Hz display; main-thread work per frame ≤ 3–4 ms.

**Causes of stutter and their fixes:**

| Cause | Fix |
|---|---|
| Image decoding on the main thread | Decoding/resizing only on background threads; results reach the main thread through a `glib` channel |
| Uploading large textures to the GPU causes hitches | Strips are split into ~500 px tiles (~3 MB each; measured, see Phase 0 findings); tiles become textures 3 screens before they come into view |
| Content jumps when an image loads | Every tile's size is known **before** it is added (stored in the cache database); placeholders have the exact height |
| `GtkListView` estimates variable row heights and jumps | Custom `StripView` instead of `GtkListView`: all tile positions are precomputed, `snapshot()` only calls `append_texture` for visible textures (microseconds per frame) |
| Disk/DB/JS work on the main thread | Progress saving, DB writes, extension JS, network: all in the background; progress saved in batches every 2 s |
| Thermal throttling | Minimal GPU/CPU work (no effects, downscaled textures) |

**Scrolling:**
- `StripView` implements `GtkScrollable` → placed inside a `GtkScrolledWindow`; GTK provides **kinetic touch scrolling** and edge overshoot
- Pen dragging: a custom drag gesture plus our own fling deceleration curve; it must feel the same as a finger
- Textures for tiles outside a ±3 screen buffer are released (cheap, no hitches)

**Pinch zoom:**
- Two fingers via `GtkGestureZoom`; during the gesture zoom is **only a GPU transform** (`snapshot` scale + translate), no relayout or re-decode
- Zoom is centered between the two fingers; range 1×–3×
- While zoomed, one finger pans in any direction (horizontal scrolling enabled)
- When the gesture ends (above 1×), visible tiles are re-decoded at higher resolution in the background and swapped in silently → sharp image, no stutter
- Texture filtering: linear (mipmapped when downscaling) for a smooth image

**Measurement (on the Yoga):**
- Debug builds show a frame-time overlay + log; every frame over 20 ms is recorded
- GTK Inspector recorder, `intel_gpu_top`, `sensors`
- If the renderer misbehaves, compare `GSK_RENDERER=ngl` / `vulkan`

## Tablet mode and input

- **Touch:** kinetic scrolling, pinch zoom (`GtkGestureZoom`), free panning while zoomed
- **Pen:** GTK treats the pen like a mouse → a custom drag gesture scrolls (`GtkGestureStylus` / `GtkGestureDrag`); pen side button → control panel
- **Orientation:** Plasma rotates the screen automatically; the app switches between portrait/landscape layouts based on window size (narrow reading column in landscape)
- **On-screen keyboard:** the search field brings up Plasma's virtual keyboard (Wayland text-input)

## Performance rules

1. Images are **downscaled to screen width** once and cached on disk
2. Long strips are split into **~500 px tiles** (no huge GPU textures; 1500 px tiles stalled the GPU for up to ~180 ms when a 2x zoomed tile was first drawn)
3. Off-screen tiles are released from memory (with 16 GB, a buffer of a few screens is kept)
4. Prefetching/decoding runs on **low-priority** threads (nice + idle I/O priority)
5. No effects: no blur, no shadows, no continuous animation
6. Targets: ~0% CPU when idle, 60 fps while scrolling, no heating/throttling during long sessions

## Extension model (JS)

### Layers

```
UI (GTK4)
  └─ Rust: trait Source  ← the rest of the app only knows this
       ├─ JsSource        (JS extension running in QuickJS)
       ├─ LocalSource     (folders / CBZ on disk)
       └─ SuwayomiSource  (later: bridge for Tachiyomi extensions)
```

### Division of work (for performance)

- **JS only provides the "recipe":** which URL to request, which fields to extract.
- **Rust does the heavy lifting:** HTTP, HTML parsing (`scraper`), image download/decode/resize, caching.
- JS never touches image bytes; `getPages` only returns a list of URLs (+ headers if needed).

### Extension interface

```js
export default {
  id: "example-site", name: "Example Site", lang: "en", version: 3,
  baseUrl: "https://example.com",
  domains: ["example.com", "cdn.example.com"],   // requests are only allowed to these domains
  settings: [{ key: "quality", type: "select", options: ["high", "low"], default: "high" }],
  filters: [{ key: "genre", type: "multi", options: [...] }],

  async popular(page) {},            // -> { items: [{ id, title, cover }], hasNext }
  async latest(page) {},             // -> same
  async search(query, page, filters) {},
  async getManga(id) {},             // -> { title, cover, author, status, genres, description }
  async getChapters(id) {},          // -> [{ id, name, number, date, scanlator }]
  async getPages(chapterId) {},      // -> [{ url, headers? }]
};
```

Example body:

```js
async search(query, page) {
  const doc = await http.getHtml(`${this.baseUrl}/search?q=${encodeURIComponent(query)}&p=${page}`);
  return {
    items: doc.select(".manga-card").map(el => ({
      id: el.selectFirst("a").attr("href"),
      title: el.selectFirst(".title").text(),
      cover: el.selectFirst("img").attr("data-src"),
    })),
    hasNext: !!doc.selectFirst(".next"),
  };
}
```

### Host API (Rust → JS)

| API | Purpose |
|---|---|
| `http.get / post / getHtml / getJson` | Rust `reqwest`; cookie jar, per-source rate limiting, requests outside `domains` are rejected |
| `doc.select / selectFirst / text / attr / html` | HTML is parsed in Rust; JS only receives lightweight handles |
| `settings.get(key)` | Extension settings (the app renders a settings page automatically) |
| `log` | Debugging |

No file system, no processes, no loading external code via `eval`. QuickJS runs with a memory limit (e.g. 32 MB) and a timeout (interrupt handler).

### Templates (multisrc)

Many sites use the same WordPress theme (Madara, MangaThemesia, etc.). One template JS + a small per-site config (name, domain, language) → a single template covers dozens of sites.

### Repository and lifecycle

- A repository = one `index.json`: `[{ id, name, lang, version, url, sha256, icon, nsfw, template? }]`
- Install: download → verify sha256 → save under `~/.local/share/<app>/extensions/`
- Extensions are **loaded lazily**: a QuickJS context is created only when used and closed after being idle for a while
- Version check on startup (or manually), update notification
- Multiple repositories can be added

### Cloudflare / bot protection

If a site returns a challenge page, the app opens a small WebKitGTK window and **the user** passes the check themselves; the app then takes the resulting cookies and user agent and uses them for that source's requests. No automatic CAPTCHA solving.

### Local source

`~/Manga/<Series>/<Chapter>/` folders and CBZ files show up in the library through the same `Source` interface (also handy for prototype testing).

### First sources (reviewed 2026-11-24)

**1. example.com — first extension.** Clean JSON API (observed in a browser):
- `GET /api/search?q=…` → `{ manga_list: [{ id, title, photo, … }] }` (parameter names to be verified during implementation)
- `GET /api/manga/{id}` → manga details (JSON)
- `GET /api/manga/{id}/chapters/list` → `[{ id, chapter_number, chapter_title, language, group_name, date_added, page_count }]`
- `GET /api/chapters/{chapterId}/images` → `{ chapter: { is_longstrip, … }, images: [{ url: "/chapters/…/001.webp", w, h }] }`
- `is_longstrip` can tell webtoon from manga; images are WebP
- `w`/`h` can be 0 → read the size from the WebP header (first few hundred bytes); enough for `StripView`
- Risk: the home page returned 403 to an automated (non-browser) client → test the API endpoints with `curl`/`reqwest` on the Yoga; fall back to the WebView cookie flow if needed

**2. example.org — second.** Shows a Cloudflare challenge page ("Just a moment…") on load that did not pass by itself in an embedded browser. Needs the Cloudflare window flow (the user passes the check); structure to be reviewed after that.

### ExtRepo (Mihon extension repository) — reference source

- https://ExtRepo.github.io/extensions/ — ~1700 extensions / ~2800 sources; source code: `github.com/ExtRepo/extensions-source` (Kotlin)
- **Used as a porting reference.** API endpoints, headers, selectors and rate limits for each site are already worked out; check the Kotlin code there before writing a JS extension.
- Both candidate sites have extensions there:
  - `src/all/examplesite` (ExampleSite)
  - `src/all/SiteB` (SiteB, v1.6.42) — API on `example.org`; requests need an `x-integrity-token` header, fetched via `GET {baseUrl}/api/integrity` and cached until its `exp`. Easy to do in a JS extension.
- Templates under `lib-multisrc` (Madara, etc.) get ported once to our template system → dozens of sites at once
- License: the source repository's license is credited in ported extensions (check `LICENSE` before porting)
- Note: the repository moved to a new format (`index.pb`, Mihon 0.21.3+); compatibility with the Suwayomi bridge must be verified separately

### Later: Suwayomi bridge

Suwayomi-Server (Java) runs as a separate process only when the user enables it; `SuwayomiSource` maps its HTTP API to the `Source` interface. This makes Tachiyomi/Mihon extensions usable too.

## Phases

0. **Scrolling prototype** — local sample webtoon chapters, `StripView`, tiling, memory release, kinetic scrolling, pinch zoom, pen, triple-tap panel. **Acceptance criteria (Yoga, tablet mode, on battery):**
   - 10 min of continuous scrolling + zooming: **0** frames over 20 ms (or very rare and imperceptible)
   - No visible hitch or content jump when a new tile comes into view
   - ~0% CPU when idle, low average CPU while scrolling, fan stays off, no thermal throttling
   - If the criteria are not met, do not move on to the MVP; find the cause first (renderer, tile size, texture upload)
1. **MVP** — JS extension engine + first source, search, manga details, chapter list, reader, library, progress saving
2. Extension repository, downloads/offline, history, update checks
3. Suwayomi bridge, categories, backup, tracking (AniList/MAL)
4. **Anime support** (inspired by anime-cli, without calling it):
   - anime-cli is an interactive shell script (fzf menus) → wrapping it as a subprocess is fragile; the same logic is written as an **anime extension** instead
   - Anime type in the extension interface: `search`, `getAnime`, `getEpisodes`, `getVideos(episodeId)` → `[{ url, quality, headers?, subtitles? }]`
   - Playback: **mpv** (already on Fedora), full screen, `--hwdec=vaapi` → hardware decoding on UHD 620, low heat
   - Playback position read via mpv's JSON IPC socket → resume, watched markers, next episode
   - UI: same retro library with a "Manga / Anime" tab; same triple-tap logic in mpv via keys/IPC

## Phase 0 findings (measured on the Yoga, 2560×1440 @ 1.25, release build)

- **Renderer:** GTK picks the Vulkan renderer here: ~10 ms of app CPU per frame and ~20 % slow frames when full screen. The GL renderer (`GSK_RENDERER=gl`) needs ~1.7 ms per frame. The app now forces `gl` unless `GSK_RENDERER` is set.
- **Pixel format:** workers produce RGBA8 (the GPU's native layout) so the main thread never converts pixels.
- **Tile size:** 500 px default (`MANGA_TILE_PX` overrides it). Pre-uploading textures off-screen ("warm-up") made no measurable difference and was removed.
- **Idle:** 0 CPU ticks over 8 s; RSS stays at ~220–350 MB while scrolling 20 000 px pages at 6000 px/s.
- Still to verify by hand: touch, pen, pinch, triple tap, heat/fan (cannot be simulated here).

## Phase 1 notes (implemented)

- **Extension engine:** `src/ext/` — QuickJS (`rquickjs`) on its own low-priority thread per extension, started on first use and dropped after 60 s idle. 32 MB memory limit, 30 s call timeout, requests only to the extension's `domains`. HTML via `scraper` behind `doc.select/selectFirst/text/attr/html` handles.
- **HTTP is blocking (`ureq`, rustls), not `tokio`/`reqwest`:** every network call already runs on a background thread, so an async runtime would only add binary size and wake-ups.
- **Bundled extension:** `extensions/ExampleSite.js` (endpoints verified against the live API: `/api/search?search=&sortBy=&page=`, `/api/manga/{id}`, `/api/manga/{id}/chapters/list`, chapters with `source: "user"` use `/api/uploads/{id}/images`, others `/api/chapters/{id}/images`; images need a `Referer`). Extra extensions: drop `*.js` into `~/.local/share/manga-reader/extensions/` (same `id` overrides a bundled one). Test one without the UI: `manga-reader --probe <file.js> [query]`, `--probe-raw <file.js> <method> '<json args>'`.
- **Local source:** `~/Manga/<Series>/<Chapter>/` (or the folder given as the first argument) appears as the "Local" source.
- **Data:** SQLite at `~/.local/share/manga-reader/library.db` (library, per-chapter progress, image-size cache) on a dedicated DB thread; reader progress is flushed at most every 2 s. Downloaded pages and covers are cached under `~/.cache/manga-reader/` and trimmed to 1 GB at start-up.
- **Page sizes:** taken from the API when present, else read from the first 16 KB of each image (HTTP range request) and cached, so the whole chapter is laid out before any pixel is decoded.
- **Main window:** places sidebar (Library + one entry per source), cover grid with infinite scroll, series page with chapter list, status bar. The menu bar from the visual plan is postponed (touch-first; nothing in it is needed yet).
- **Dev hooks:** `MANGA_OPEN=<source>:<manga id>[:<chapter index>]` opens a chapter at start-up, `MANGA_VIEW=source:<id>` / `details:<source>:<manga id>` shows a page, plus the Phase 0 autoscroll variables.
- **Bug found by screenshot:** tiles uploaded as premultiplied RGBA let a not-quite-255 alpha at tile edges blend with the black background (a faint line at every tile boundary). Tiles now use `R8g8b8x8` (alpha ignored) and each tile's bottom edge reuses the next tile's top edge.

## Development and testing

- Hands-free measurement: `MANGA_FRAME_STATS=1 MANGA_AUTOSCROLL=<px/s> [MANGA_AUTOZOOM=1] MANGA_AUTOSCROLL_SECS=<n> target/release/manga-reader` prints every frame over 20 ms and a summary at exit. `MANGA_WINDOWED=1` skips full screen, `MANGA_OPEN_PANEL=1` opens the panel at start.
- Test data: `python3 tools/gen_test_strips.py` writes sample chapters to `~/Manga/TestSeri`.

- Development happens directly on the Yoga (Fedora KDE, Claude Code CLI) — touch/pen/heat can only be tested on the real device
- Build speed: `mold` linker, dependencies prebuilt with `opt-level`
- Testing: `cargo run --release`, in tablet mode
- Measurement (Yoga): `top`, `intel_gpu_top`, `sensors`, `powertop`
