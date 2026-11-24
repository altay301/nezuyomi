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
- Zoom is centered between the two fingers; range 0.5×–3×. Below 1× the page is narrower than the screen and centred (plain linear filtering: trilinear cost ~20 % more slow frames); pinching within ±8 % of 1× snaps to exactly 1× (page fills the screen width) so the exact fit is easy to hit from either side
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

**2. SiteB — second (blocked, see `extensions/experimental/README.md`).** *Update 2026-11-24:* example.org has no challenge for `curl`, but rejects the app's TLS fingerprint, its image host rejects every request, and its pages are JPEG XL. example.org still shows the original challenge.

Original note: **example.org** Shows a Cloudflare challenge page ("Just a moment…") on load that did not pass by itself in an embedded browser. Needs the Cloudflare window flow (the user passes the check); structure to be reviewed after that.

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

## Scope decisions (2026-11-24)

- **Phase 2 reduced to downloads.** Extension repository, history, update checks and the extra places/menu bar are not planned for now.
- **Downloads (done):** `src/downloads.rs`. One low-priority worker, one chapter at a time, pages sequentially with a 60 ms pause. Files in `~/.local/share/manga-reader/downloads/<source>/<series>/<chapter>/` plus a `pages.json` manifest. The queue survives restarts. Finished chapters open from disk, and the series page falls back to stored details when the source is unreachable (verified in a network-less namespace with `unshare -rn`). The sidebar has a "Downloads" place.
- **SiteB (blocked):** see `extensions/experimental/README.md`.
- **Phase 3 skipped.** The Suwayomi bridge needs a JVM process (typically 300–600 MB RAM, seconds of start-up, constant GC work), which conflicts with the low-heat goal. Revisit only if a needed source exists nowhere else. Categories, backup and tracking are also postponed.
- **Phase 4 (anime, done):** extensions declare `type: "anime"` and implement `popular/latest/search/getAnime/getEpisodes/getVideos`; the same `Source` trait serves both (`kind()`, `videos()`). `extensions/anime.example` is ported from anime-cli (search page → episode list API → server list API → EmbedHost embed → config blob `base64(json XOR "otaku-embed-v1")` → HLS master playlist). Playback is mpv with `--hwdec=vaapi` (measured: 5.1 s of CPU for 10 s of 1080p versus 11.8 s in software), a full-screen mpv window, and progress read over mpv's JSON IPC socket every 5 s (resume with `--start=<percent>%`, watched at 90 %). The sidebar lists manga and anime sources separately. Not done: the triple-tap panel inside mpv, quality/sub-dub settings (Sub is played, Dub if it is the only stream is not special-cased), anime downloads.

## Nyaa torrent downloads (Phase 2 addition, implemented)

Status: **implemented** (milestones 1–5 below). The "Open decisions" were answered by the owner: every proposed default was accepted; Nyaa downloads live in their own place, apart from the other sources; "Start qBittorrent" is a button, never automatic; port 8080; interval 60 minutes. Implementation notes and findings are at the end of this section.

### Goal

Keep selected library series up to date with volumes released by a small, fixed set of Nyaa uploaders, without ever browsing the site: poll RSS, match releases to series the user linked once, hand the torrents to the user's own qBittorrent, and read the finished CBZ/ZIP/CBR/7z archives in the existing reader as "downloaded" local content.

Non-goals: searching or browsing Nyaa in the UI, running our own torrent engine, touching any torrent that this app did not add, EPUB/light-novel/audiobook handling.

### What the research showed (2026-11-25, real data)

- **RSS:** `https://nyaa.si/?page=rss&u=<uploader>&c=3_0` filters by uploader and category on the server. Items carry `title`, `link` (the `.torrent`), `guid` (view page), `pubDate`, `nyaa:infoHash`, `nyaa:size`, `nyaa:seeders`, `nyaa:categoryId` — **no uploader field**, so the uploader is known only from which feed an item came from. A feed returns the newest 75 items. `q=<phrase>` can be combined with `u=` (verified: `u=UploaderE&q=shangri`). The feed is cached server-side for 5 minutes (`cache-control: max-age=300`) and answers `If-Modified-Since` with `304`.
- **Categories:** `3_0` = all of Literature, `3_1` English-translated, `3_2` Non-English-translated, `3_3` Raw. The six uploaders post almost only `3_1`.
- **Our HTTP client works** for both the RSS feeds and `.torrent` downloads (no Cloudflare/TLS-fingerprint problem, unlike SiteB).
- **Not everything in Literature is manga.** UploaderC's feed is mostly `[Audiobook]` and `.epub` (light novels); UploaderF posts `.epub` too. Only archives are wanted.
- **Torrent layouts seen:** a single `.cbz` as the torrent root (UploaderA, UploaderF), or a folder with one `.cbz` per volume (UploaderB, UploaderD, UploaderE). One torrent can hold many volumes (`v01-27`, 15 GiB), and a later torrent often supersedes an earlier one (`v01-13` then `v01-14`), so "new volume" detection must work per file, not per torrent.
- **Title/file name shapes** (all real, used as test fixtures later):

| Example | Shape |
|---|---|
| `Prince Freya v01-13 (2020-2026) (Digital) (UploaderA)` | title, volume range, year range, `(Digital)`, `(Group)` |
| `Ultimate Exorcist Kiyoshi v01,009-090 (2024-2026) (Digital) (UploaderA)` | volumes plus loose chapters after a comma |
| `Queen's Quality (2017-2026) (Digital) (UploaderA)` | no volume number at all (whole run or one-shot) |
| `The God-Slaying Demon King 001-021 as v01-05 (Digital-Compilation) (Square Enix) (UploaderE)` | chapters compiled into volumes, publisher in parentheses |
| `Weekly Viz, ShonenJump and MangaPlus Chapter Updates - Week 47 2026 (Digital) (UploaderB)` | a pack of many series, not one series |
| `Unnamed Memory v01-08 (2022-2026) (Digital) (GroupG, GroupH)` | several release groups (UploaderD posts as "GroupH") |
| `Reincarnated Onmyoji v01-02 [Hanashi Media] [UpC]` | bracket style (UploaderC, UploaderF) |
| `Mitama Security - Spirit Busters (2021) (Digital) (UploaderF-repack)` / `(UploaderF-Empire)` | two variants of the same release |
| inside the torrent: `Teppeki Honeymoon v02 (2026) (Digital) (UploaderE).cbz` | one file per volume |

### Architecture

A native Rust module `src/nyaa/` (not a JS extension: it needs the qBittorrent client, the power policy and the database). Everything blocking runs on background threads at low priority, like the rest of the app.

```
nyaa/
  feed.rs       RSS polling (conditional GET), item parsing
  rules.rs      per-uploader title/file-name rules  -> ParsedRelease
  matcher.rs    series links, owned-volume bookkeeping, "is this release new?"
  torrent.rs    minimal bencode reader: file list of a .torrent (size-capped)
  qbit.rs       qBittorrent Web API client, category/save-path, scoped pause/resume
  power.rs      UPower + KWin tablet-mode watcher (D-Bus via gio, no polling)
  secret.rs     qBittorrent password in the Secret Service (KWallet)
  archive.rs    CBZ/ZIP/CBR/7z page extraction into the page cache
  mod.rs        scheduler tying the above together
```

Dependencies: `quick-xml` for RSS, `regex`, `zip` for CBZ/ZIP; CBR/7z through the installed `7z` (or `unar`) command so no non-free unrar library is linked. No async runtime (consistent with the blocking `ureq` decision).

### Data model (SQLite, same database file)

- `nyaa_uploaders(name PRIMARY KEY, enabled, category DEFAULT '3_1')` — seeded with UploaderA, UploaderB, UploaderC, UploaderD, UploaderE, UploaderF; add/remove/disable in Settings.
- `nyaa_releases(info_hash PRIMARY KEY, uploader, title, size, pub_date, link, state, link_id, qbit_hash, first_seen)` — `state`: `seen`, `matched`, `queued`, `downloading`, `done`, `skipped`, `failed`. Dedup key is the info hash.
- `nyaa_links(id, source, manga_id, uploader, query, mode, include_regex, exclude_regex, folder)` — one row per linked series: the library series (`source`+`manga_id`), the uploader, the search phrase, the mode (auto-download / notify only) and the local folder the volumes go to.
- `nyaa_volumes(link_id, volume, file_path, info_hash)` — what is already on disk; filled from finished torrents and from a folder scan, so manually added files count too.

### Polling (RSS only, no site browsing)

- One request per enabled uploader per cycle, `c=3_0`/`3_1` as configured, spaced ~2 s apart, with `If-Modified-Since` (a `304` costs nothing). Default interval **60 minutes**, minimum 15 (the server caches for 5). Honest `User-Agent` (`manga-reader/<version>`).
- Per link, a targeted query `u=<uploader>&q=<phrase>` is used only when the link is created, on "Check now", and about once a day, because the 75-item uploader feed only shows recent releases; this is how back-catalogue volumes are found.
- The timer only exists while the app runs and is skipped while on battery in tablet mode (see Power policy). No wake-ups otherwise.

### Matching and number extraction (requirement 4)

- Rules live in `rules.toml` (defaults embedded in the binary, an optional user copy under `~/.config/manga-reader/` overrides it). Each uploader has an ordered list of regexes with named groups (`title`, `vol`, `vol_end`, `ch`, `ch_end`, `year`, `group`, `publisher`), plus filters:

```toml
[uploader.UploaderA]
accept_ext   = ["cbz", "zip", "cbr", "7z", "rar"]
reject_title = ['\[Audiobook\]', 'Chapter Updates - Week']
patterns = [
  '^(?P<title>.+?) v(?P<vol>\d+)(?:-(?P<vol_end>\d+))?(?:,(?P<ch>\d+)(?:-(?P<ch_end>\d+))?)? \(',
  '^(?P<title>.+?) (?P<ch>\d+)-(?P<ch_end>\d+) as v(?P<vol>\d+)-(?P<vol_end>\d+) \(',
  '^(?P<title>.+?) \(\d{4}',          # no volume: whole run / one-shot
]
[uploader.UploaderF]
prefer_variant = ["Empire", "repack"]  # which of two variants of one release to take
```

- A parsed release yields a title plus a set of volume numbers and chapter ranges. Titles are normalised (case, punctuation, `&`/`and`, leading `The`) before comparing with the link's phrase, which is an include phrase plus optional exclude regex.
- The same rules parse **file names inside a torrent** (one volume per `.cbz`), which is what decides what to download.
- Unit tests are table-driven from the real titles above and the file names seen inside the torrents; every rule change must keep them green.

### Link once, find new volumes automatically (requirement 3)

On a library series: **"Link to Nyaa…"** asks for an uploader (dropdown of the followed ones) and a search phrase (prefilled with the series title), runs the targeted query, shows what it found (parsed volumes), and saves the link. After that the scheduler checks each cycle: parse new items from that uploader, keep those whose normalised title matches, compute the volumes they would add versus `nyaa_volumes`, and either queue them (mode *auto*) or show "New: v14" for one tap (mode *notify*).

### qBittorrent integration (Web API v2, localhost)

- **Endpoint:** `http://127.0.0.1:<port>/api/v2/` (default port 8080, configurable). Installed here: qBittorrent 5.2.4 (v5 uses `torrents/stop` and `torrents/start`; the client reads `app/webapiVersion` and falls back to `pause`/`resume` on older versions).
- **Own scope, strictly:**
  - category `manga-reader`, created with its own save path if missing, and never edited otherwise; every torrent we add also gets the tag `manga-reader`;
  - we remember the info hashes we added (`nyaa_releases.qbit_hash`); status queries use `torrents/info?category=manga-reader`; pause/resume/delete always pass **explicit hashes of our own torrents**, never `all`, never torrents of other categories; no global qBittorrent setting is read-modified-written.
- **Adding a release:** the app downloads the small `.torrent` itself, reads its file list (bencode, size-capped), then calls `torrents/add` **paused** with `category`, `tags`, `savepath=<series folder>` and `contentLayout=NoSubfolder`; sets `torrents/filePrio` to 0 for non-archive files (epub/pdf/audio/images) and for volumes already owned; then starts it. If nothing remains to download, the torrent is not added at all. Free disk space is checked first, and a per-torrent size cap applies (default 4 GiB, see Open decisions).
- **Progress:** `torrents/info?category=manga-reader` every ~15 s **only while something of ours is active and the app is open**; no timer otherwise. On completion (progress 1.0) the files are indexed into `nyaa_volumes`, the series shows "downloaded", and the seeding policy is applied with `torrents/setShareLimits` on our hashes only.
- **Authentication:** default is "Bypass authentication for clients on localhost", i.e. no credentials. If the user has a password instead: username in settings, **password only in the Secret Service** (KWallet's `ksecretd` is running here) written with `secret-tool store` (password through stdin, never argv, never logged, never in `config` or the repository). The `SID` cookie is kept in memory only. Requests send a matching `Referer`/`Origin` for qBittorrent's CSRF check.
- **qBittorrent not reachable — clear messages instead of silent failure:** a connection error shows "qBittorrent is not running or its Web UI is disabled" with the exact steps (Tools → Options → Web UI → *Web User Interface (Remote control)*, and optionally *Bypass authentication for clients on localhost*) and a *Retry* button; `401` → "qBittorrent wants a password" with a field that stores it in the Secret Service; `403` → "qBittorrent rejected the request (host header / CSRF settings)". Queued items wait in the database and are retried; nothing is lost. **Current state of this machine:** qBittorrent is installed but not running and its profile has no Web UI settings, so the Web UI is disabled — the app cannot enable it from outside and will show the setup steps.

### Power policy (battery + tablet mode)

- Signals, no polling: UPower `org.freedesktop.UPower.OnBattery` (system bus) and KWin `org.kde.KWin.TabletModeManager.tabletMode` (session bus), both through `gio::DBusProxy` property-change notifications.
- **Pause** our hashes when `on_battery && tablet_mode`; **resume** them when that condition ends (charger plugged in, or tablet mode left). Only `manga-reader` torrents are paused or resumed, and only ones this policy paused (so a torrent the user paused by hand stays paused). RSS polling is also skipped under that condition.
- Limitation to state in the UI: the policy runs inside this app, so it only acts while the app is open; if the app is closed qBittorrent keeps downloading. (A background service would be a separate, larger feature.)

### Archives as "downloaded" local content (requirement 5)

- **Placement:** torrents download straight into `<nyaa root>/<Series>/` (per-torrent `savepath`), so qBittorrent keeps seeding the very files the reader opens — no copies, no hardlinks.
- **Local source extension:** `LocalSource` learns that a series folder may contain archives: each `.cbz/.zip/.cbr/.rar/.7z` is one "chapter" (named after the volume, ordered by parsed volume number), next to the existing folder-per-chapter layout. These series appear in the Library with a "downloaded" mark and in the Downloads place.
- **Reading:** the first open extracts the volume's image entries (natural order, images only) into the page cache `~/.cache/manga-reader/archives/<hash>/` on a low-priority thread, writes a `pages.json` like finished downloads do, and the existing StripView pipeline takes over unchanged (sizes from image headers, tiling, progress saving). ZIP/CBZ via the `zip` crate; RAR/CBR/7z via `7z x -o<dir>`. Extracted volumes count towards the existing 1 GiB cache trimming (oldest first); archives are never modified.
- **Safety:** entry names are sanitised (no `..`, no absolute paths), only image extensions are written, and entry count/size caps stop zip bombs; the external tool runs with a timeout and low priority.
- **Series page:** an online series (e.g. ExampleSite) cannot be merged chapter-by-chapter with volume files (different numbering). The link therefore creates/uses a **separate local series** and the linked library entry gets a button "Volumes on disk (N)" that opens it.

### UI

- Sidebar place **Nyaa**, three small pages: *Releases* (recent items from the followed uploaders with their state, filter box, "Download"/"Skip"), *Links* (linked series, mode, last check, "Check now", unlink), *Settings* (uploaders add/remove/enable, interval, category scope, size cap, seeding policy, qBittorrent address/port/username, connection test with the status banner above).
- Library series page: **Link to Nyaa…** and **Volumes on disk**. Retro theme, touch targets ≥ 44 px, no animations.

### Resource budget

No timers while idle; RSS ≤ 6 small requests per hour with `304`s; qBittorrent polling only while torrents of ours are active; D-Bus is event-driven; extraction and parsing at nice 10 / idle I/O; the heavy network and disk work happens in qBittorrent and is paused on battery in tablet mode.

### Testing strategy

1. Table-driven rule tests from the real titles and torrent file names (fixtures saved under `tests/fixtures/nyaa/`).
2. RSS parser tests from saved feeds; a fake HTTP server for conditional GET / `304`.
3. qBittorrent client against a **throw-away real instance** started with `qbittorrent --profile=<tmpdir> --webui-port=<free port>` (own profile, own port, never the user's torrents); plus a tiny mock server for error paths (refused, 401, 403). A locally generated torrent whose data already exists completes instantly after a recheck, which exercises the completion path without any network.
4. Archive tests with generated CBZ/ZIP/7z/RAR samples including hostile ones (`../` names, huge entry counts).
5. Power policy tested with a fake D-Bus service for `OnBattery`/`tabletMode` transitions; the "only our hashes" rule has a dedicated test.

### Milestones

1. Rules + RSS parser + database tables (no network side effects), fixtures and tests.
2. qBittorrent client, status banner, secret storage, isolated-instance integration test.
3. Links, matcher and the add-torrent pipeline (file selection, size/space guards).
4. Archive support in `LocalSource`, extraction cache, "downloaded" display and Library integration.
5. Power policy and the Nyaa pages/Settings UI.

### Risks

- Name patterns change or are inconsistent between releases → rules are data, covered by fixtures, and unmatched items stay visible in *Releases* instead of being dropped.
- Large packs (15 GiB) can fill the disk → size cap and free-space check before adding.
- qBittorrent's Web UI is off by default → explicit onboarding message; the app cannot enable it.
- RAR support depends on the installed `7z`/`unar` → missing tools produce a clear "install 7zip" message for that volume only.

### Decisions (answered 2026-11-25)

1. **Category scope:** `3_0` (all Literature incl. Raw and Non-English) or only `3_1` English-translated? *Proposed: `3_1`.*
2. **Non-manga files:** skip EPUB/audiobooks/PDF entirely (UploaderC and UploaderF post many)? *Proposed: skip.*
3. **Auto vs notify:** new volumes of a linked series are queued automatically, or shown first for one tap? And the size cap per torrent. *Proposed: auto for linked series, cap 4 GiB, larger ones need one tap.*
4. **Download folder:** category save path `~/Manga/Nyaa/` with a sub-folder per series. *Proposed as above.*
5. **Seeding:** keep seeding until ratio 1.0 / 24 h, or stop immediately after completion? *Proposed: ratio 1.0 or 24 h, whichever first.*
6. **Power policy wording:** pause only when on battery **and** in tablet mode; resume when plugged in **or** tablet mode ends — correct? And is "only while the app is open" acceptable?
7. **Linked series model:** separate local series cross-linked to the online entry (proposed) versus trying to merge volumes into the online series page.
8. **Variants and packs:** for UploaderF's repack/Empire pairs which variant wins; skip UploaderB's weekly chapter packs and "Digital-Compilation" releases? *Proposed: prefer Empire, skip weekly packs, include compilations.*
9. **qBittorrent start-up:** show the setup steps only, or also launch `qbittorrent` when it is not running? Web UI port (8080?).
10. **Poll interval:** default 60 minutes OK?

### Implementation notes and findings (2026-11-25)

- **qBittorrent 5.2.4 will not start its Web UI without credentials.** With a profile that has no user name/password the log says `(C) WebUI: Credentials are not set` and nothing listens. With credentials set **and** "Bypass authentication for clients on localhost" ticked, the API needs no login (Web API 2.15.1). So the setup steps the app shows are: enable the Web UI, set *any* user name and password, tick the bypass. The app never needs that password in bypass mode; if the owner prefers to keep authentication, user name in Settings and password in the Secret Service work as planned.
- **Web API 2.15** requires `shareLimitAction` in `torrents/setShareLimits` (found by the integration test); we send `Stop`.
- **Verified against real data:** all 450 items of the six followed feeds are classified (parsed, or skipped as not manga). Skipped on purpose: audiobooks, EPUB, weekly chapter packs, "app rip" packs and **JPEG XL** releases (the reader cannot decode JXL pages; an archive with only JXL/AVIF pages gets a clear message too).
- **Real end-to-end run** (isolated qBittorrent, isolated data folder, one real 81 MiB release from the live swarm): cycle → match → `.torrent` fetched and hash-verified → added paused → file priorities → started → downloaded → timer poll → `done` → volume recorded → the CBZ appears in the Nyaa place with its cover and opens in the normal reader. That run also caught a real bug (an early `return` in the UI event loop ended event handling at start-up).
- **Safety nets that exist and are tested:** only our category/hashes are ever passed to qBittorrent (`all` and malformed hashes are refused; a torrent in another category and one the user stopped by hand survive stop/start/policy calls untouched); the downloaded `.torrent`'s info hash must equal the feed's; torrent files, archive entries and folder names are sanitised; the torrent URL must be a `nyaa.si/download/*.torrent` link; size cap, free-space check and per-torrent file selection (owned volumes, EPUB and other series are set to priority 0).
- **Where things are:** `src/nyaa/` (`rules` + `rules.toml`, `feed`, `matcher`, `torrent`, `qbit`, `secret`, `store`, `sync`, `service`, `power`), `src/nyaa_ui.rs` (the Nyaa place and the link window), `src/archive.rs` (CBZ/ZIP in-process, CBR/RAR/7z through `7z`), `LocalSource` (archives as chapters; a second instance "Nyaa").
- **Testing tools:** `manga-reader --nyaa-probe` classifies the live feeds; `tools/qbt-test-instance.sh start|stop` runs a throw-away headless qBittorrent (own profile and port); the integration tests marked `#[ignore]` use it: `QBT_TEST_PORT=18080 QBT_TEST_DIR=<dir> cargo test --release qbit_isolated nyaa_end_to_end power_policy_isolated -- --ignored`.
- **Editions (colored vs black & white), added after a real report:** a colored release has the same volume numbers as the standard one, so volume bookkeeping per series made it look "already owned" (and its files, `Volume 01 - Title.cbz`, carry no series title and no `v01`, so they were not understood at all). Now a release is classified as `standard` or `colored` from its title (the word *Colored*/*Colour*, or *Full Color*/*Digital Colored*, standing alone; a hyphenated title word such as "Seven-Colored" does not count), a link has an edition, and a series can be linked once per edition: the colored link downloads into its own folder `<Series> (Colored)` and keeps its own volume list. File names without the series title ("Volume 01 - …", "Vol. 3", "Altercolor. Vol26-Ch105") are understood and inherit the match of their torrent. Databases from earlier versions are upgraded in place, and releases that were skipped as "nothing new" or "other variant" are looked at again once.
- **Not done / limits:** the power policy runs only while the app is open; no per-series custom exclude field in the link window (the column exists in the database, the isolated run used it); series linked in the library are matched by phrase only (no cover/ID matching); CBR/7z covers are not generated (only CBZ), such series show a placeholder until a volume is opened.

## Development and testing

- Hands-free measurement: `MANGA_FRAME_STATS=1 MANGA_AUTOSCROLL=<px/s> [MANGA_AUTOZOOM=1] MANGA_AUTOSCROLL_SECS=<n> target/release/manga-reader` prints every frame over 20 ms and a summary at exit. `MANGA_WINDOWED=1` skips full screen, `MANGA_OPEN_PANEL=1` opens the panel at start.
- Test data: `python3 tools/gen_test_strips.py` writes sample chapters to `~/Manga/TestSeri`.

- Development happens directly on the Yoga (Fedora KDE, Claude Code CLI) — touch/pen/heat can only be tested on the real device
- Build speed: `mold` linker, dependencies prebuilt with `opt-level`
- Testing: `cargo run --release`, in tablet mode
- Measurement (Yoga): `top`, `intel_gpu_top`, `sensors`, `powertop`
