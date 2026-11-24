//! SQLite on its own thread: the GTK main thread never touches the database.
//!
//! * `exec` is fire-and-forget (progress saves, library edits).
//! * `call` blocks for the answer, so use it from background threads only.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::source::{ChapterInfo, MangaDetails};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

pub struct Db {
    tx: mpsc::Sender<Job>,
}

#[derive(Clone, Debug)]
pub struct LibraryEntry {
    pub source: String,
    pub manga_id: String,
    pub title: String,
    pub cover: Option<String>,
}

/// Reading state of one chapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    /// 0.0 – 1.0 position inside the chapter.
    pub fraction: f64,
    pub read: bool,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS library (
    source   TEXT NOT NULL,
    manga_id TEXT NOT NULL,
    title    TEXT NOT NULL,
    cover    TEXT,
    added    INTEGER NOT NULL,
    PRIMARY KEY (source, manga_id)
);
CREATE TABLE IF NOT EXISTS progress (
    source     TEXT NOT NULL,
    manga_id   TEXT NOT NULL,
    chapter_id TEXT NOT NULL,
    fraction   REAL NOT NULL,
    read       INTEGER NOT NULL,
    updated    INTEGER NOT NULL,
    PRIMARY KEY (source, chapter_id)
);
CREATE INDEX IF NOT EXISTS progress_manga ON progress (source, manga_id);
-- Downloaded chapters. One flat row per chapter; the series fields repeat so the
-- Downloads page and offline browsing need no joins.
CREATE TABLE IF NOT EXISTS downloads (
    source         TEXT NOT NULL,
    manga_id       TEXT NOT NULL,
    chapter_id     TEXT NOT NULL,
    manga_title    TEXT NOT NULL,
    cover          TEXT,
    details        TEXT NOT NULL DEFAULT '{}',
    chapter_name   TEXT NOT NULL,
    chapter_number REAL,
    chapter_date   TEXT,
    scanlator      TEXT,
    dir            TEXT NOT NULL,
    state          INTEGER NOT NULL DEFAULT 0, -- 0 queued, 1 downloading, 2 done, 3 failed
    done           INTEGER NOT NULL DEFAULT 0,
    total          INTEGER NOT NULL DEFAULT 0,
    added          INTEGER NOT NULL,
    PRIMARY KEY (source, chapter_id)
);
CREATE INDEX IF NOT EXISTS downloads_manga ON downloads (source, manga_id);
-- Key/value settings (Nyaa interval, qBittorrent port, ...). No secrets: passwords live in the Secret Service.
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Nyaa: followed uploaders, linked series, seen releases and the volumes we own.
CREATE TABLE IF NOT EXISTS nyaa_uploaders (
    name         TEXT PRIMARY KEY,
    enabled      INTEGER NOT NULL DEFAULT 1,
    last_checked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS nyaa_links (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    source   TEXT NOT NULL,
    manga_id TEXT NOT NULL,
    title    TEXT NOT NULL,
    uploader TEXT NOT NULL,
    query    TEXT NOT NULL,
    auto     INTEGER NOT NULL DEFAULT 1,
    exclude  TEXT NOT NULL DEFAULT '',
    folder   TEXT NOT NULL,
    -- 'standard' or 'colored': a series can be linked once per edition (separate folder and volumes)
    edition  TEXT NOT NULL DEFAULT 'standard',
    UNIQUE (source, manga_id, edition)
);
CREATE TABLE IF NOT EXISTS nyaa_releases (
    info_hash  TEXT PRIMARY KEY,
    uploader   TEXT NOT NULL,
    title      TEXT NOT NULL,
    size       INTEGER NOT NULL DEFAULT 0,
    pub_date   TEXT NOT NULL DEFAULT '',
    link       TEXT NOT NULL DEFAULT '',
    state      TEXT NOT NULL,
    link_id    INTEGER,
    volumes    TEXT NOT NULL DEFAULT '',
    note       TEXT NOT NULL DEFAULT '',
    first_seen INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS nyaa_releases_state ON nyaa_releases (state);
CREATE INDEX IF NOT EXISTS nyaa_releases_link ON nyaa_releases (link_id);
CREATE TABLE IF NOT EXISTS nyaa_volumes (
    link_id   INTEGER NOT NULL,
    volume    INTEGER NOT NULL, -- 0 = the whole run / a one-shot without volume numbers
    file_path TEXT NOT NULL,
    info_hash TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (link_id, volume)
);
-- Image sizes by URL: the reader must know every page height before layout.
CREATE TABLE IF NOT EXISTS sizes (
    url TEXT PRIMARY KEY,
    w   INTEGER NOT NULL,
    h   INTEGER NOT NULL
);
";

/// Upgrades databases created by older versions.
fn migrate(conn: &mut Connection) {
    // nyaa_links gained `edition` and a new uniqueness rule: copy the rows into the new table.
    if conn.prepare("SELECT edition FROM nyaa_links LIMIT 0").is_err() {
        let _ = conn.execute_batch(
            "BEGIN;
             ALTER TABLE nyaa_links RENAME TO nyaa_links_old;
             CREATE TABLE nyaa_links (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, source TEXT NOT NULL, manga_id TEXT NOT NULL, title TEXT NOT NULL,
                 uploader TEXT NOT NULL, query TEXT NOT NULL, auto INTEGER NOT NULL DEFAULT 1, exclude TEXT NOT NULL DEFAULT '',
                 folder TEXT NOT NULL, edition TEXT NOT NULL DEFAULT 'standard', UNIQUE (source, manga_id, edition));
             INSERT INTO nyaa_links (id, source, manga_id, title, uploader, query, auto, exclude, folder)
                 SELECT id, source, manga_id, title, uploader, query, auto, exclude, folder FROM nyaa_links_old;
             DROP TABLE nyaa_links_old;
             COMMIT;",
        );
    }
    // One time: releases skipped as 'nothing new' or 'other variant' under the old matching (which knew
    // no colour editions and no volume-style file names) are looked at again.
    let done: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'nyaa_editions_migrated'", [], |r| r.get(0)).optional().ok().flatten();
    if done.is_none() {
        let _ = conn.execute(
            "UPDATE nyaa_releases SET state = 'seen', link_id = NULL, note = '' WHERE state = 'skipped' AND link_id IS NOT NULL AND (note LIKE 'nothing new%' OR note LIKE 'another variant%')",
            [],
        );
        let _ = conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('nyaa_editions_migrated', '1')", []);
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        migrate(&mut conn);
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("db".into())
            .spawn(move || {
                crate::loader::lower_priority();
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { tx })
    }

    pub fn exec(&self, f: impl FnOnce(&mut Connection) + Send + 'static) {
        let _ = self.tx.send(Box::new(f));
    }

    pub fn call<T: Send + 'static>(&self, f: impl FnOnce(&mut Connection) -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx.send(Box::new(move |c| drop(tx.send(f(c))))).ok()?;
        rx.recv().ok()
    }

    // ---- library -----------------------------------------------------------

    pub fn add_to_library(&self, e: LibraryEntry) {
        self.exec(move |c| {
            let _ = c.execute(
                "INSERT OR REPLACE INTO library (source, manga_id, title, cover, added) VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))",
                params![e.source, e.manga_id, e.title, e.cover],
            );
        });
    }

    pub fn remove_from_library(&self, source: String, manga_id: String) {
        self.exec(move |c| {
            let _ = c.execute("DELETE FROM library WHERE source = ?1 AND manga_id = ?2", params![source, manga_id]);
        });
    }

    pub fn is_in_library(&self, source: String, manga_id: String) -> bool {
        self.call(move |c| {
            c.query_row("SELECT 1 FROM library WHERE source = ?1 AND manga_id = ?2", params![source, manga_id], |_| Ok(()))
                .optional()
                .ok()
                .flatten()
                .is_some()
        })
        .unwrap_or(false)
    }

    /// Most recently read first, then most recently added.
    pub fn library(&self) -> Vec<LibraryEntry> {
        self.call(|c| {
            let mut stmt = match c.prepare(
                "SELECT l.source, l.manga_id, l.title, l.cover FROM library l
                 ORDER BY COALESCE((SELECT MAX(p.updated) FROM progress p WHERE p.source = l.source AND p.manga_id = l.manga_id), 0) DESC,
                          l.added DESC",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |r| {
                Ok(LibraryEntry { source: r.get(0)?, manga_id: r.get(1)?, title: r.get(2)?, cover: r.get(3)? })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    // ---- progress ----------------------------------------------------------

    pub fn save_progress(&self, source: String, manga_id: String, chapter_id: String, p: Progress) {
        self.exec(move |c| {
            let _ = c.execute(
                "INSERT INTO progress (source, manga_id, chapter_id, fraction, read, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))
                 ON CONFLICT (source, chapter_id) DO UPDATE SET
                    fraction = excluded.fraction,
                    read = MAX(read, excluded.read),
                    updated = excluded.updated",
                params![source, manga_id, chapter_id, p.fraction, p.read],
            );
        });
    }

    pub fn progress_for_manga(&self, source: String, manga_id: String) -> HashMap<String, Progress> {
        self.call(move |c| {
            let mut stmt = match c.prepare("SELECT chapter_id, fraction, read FROM progress WHERE source = ?1 AND manga_id = ?2") {
                Ok(s) => s,
                Err(_) => return HashMap::new(),
            };
            stmt.query_map(params![source, manga_id], |r| {
                Ok((r.get::<_, String>(0)?, Progress { fraction: r.get(1)?, read: r.get::<_, i64>(2)? != 0 }))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// The chapter read most recently, if any.
    pub fn last_read_chapter(&self, source: String, manga_id: String) -> Option<(String, Progress)> {
        self.call(move |c| {
            c.query_row(
                "SELECT chapter_id, fraction, read FROM progress WHERE source = ?1 AND manga_id = ?2 ORDER BY updated DESC, rowid DESC LIMIT 1",
                params![source, manga_id],
                |r| Ok((r.get::<_, String>(0)?, Progress { fraction: r.get(1)?, read: r.get::<_, i64>(2)? != 0 })),
            )
            .optional()
            .ok()
            .flatten()
        })
        .flatten()
    }

    // ---- image sizes -------------------------------------------------------

    pub fn page_sizes(&self, urls: Vec<String>) -> HashMap<String, (u32, u32)> {
        self.call(move |c| {
            let mut out = HashMap::new();
            if let Ok(mut stmt) = c.prepare("SELECT w, h FROM sizes WHERE url = ?1") {
                for u in urls {
                    if let Ok(Some(wh)) = stmt.query_row([&u], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))).optional() {
                        out.insert(u, wh);
                    }
                }
            }
            out
        })
        .unwrap_or_default()
    }

    pub fn store_page_sizes(&self, sizes: Vec<(String, u32, u32)>) {
        self.exec(move |c| {
            if let Ok(tx) = c.transaction() {
                for (u, w, h) in sizes {
                    let _ = tx.execute("INSERT OR REPLACE INTO sizes (url, w, h) VALUES (?1, ?2, ?3)", params![u, w, h]);
                }
                let _ = tx.commit();
            }
        });
    }

    // ---- downloads -----------------------------------------------------------

    /// Adds (or re-queues) a chapter; a finished one is left alone.
    pub fn queue_download(&self, q: DownloadRow) {
        self.exec(move |c| {
            let _ = c.execute(
                "INSERT INTO downloads (source, manga_id, chapter_id, manga_title, cover, details, chapter_name, chapter_number, chapter_date, scanlator, dir, state, done, total, added)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, 0, 0, strftime('%s','now'))
                 ON CONFLICT (source, chapter_id) DO UPDATE SET state = 0, done = 0, total = 0
                 WHERE state <> 2",
                params![q.source, q.manga_id, q.chapter.id, q.manga_title, q.cover, q.details, q.chapter.name, q.chapter.number, q.chapter.date, q.chapter.scanlator, q.dir],
            );
        });
    }

    pub fn set_download_state(&self, source: String, chapter_id: String, state: i64, done: u32, total: u32) {
        self.exec(move |c| {
            let _ = c.execute(
                "UPDATE downloads SET state = ?3, done = ?4, total = ?5 WHERE source = ?1 AND chapter_id = ?2",
                params![source, chapter_id, state, done, total],
            );
        });
    }

    pub fn set_download_cover(&self, source: String, manga_id: String, cover: String) {
        self.exec(move |c| {
            let _ = c.execute("UPDATE downloads SET cover = ?3 WHERE source = ?1 AND manga_id = ?2", params![source, manga_id, cover]);
        });
    }

    /// chapter id -> (state, done pages, total pages)
    pub fn download_states(&self, source: String, manga_id: String) -> HashMap<String, (i64, u32, u32)> {
        self.call(move |c| {
            let mut stmt = match c.prepare("SELECT chapter_id, state, done, total FROM downloads WHERE source = ?1 AND manga_id = ?2") {
                Ok(s) => s,
                Err(_) => return HashMap::new(),
            };
            stmt.query_map(params![source, manga_id], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Directory of a fully downloaded chapter.
    pub fn downloaded_dir(&self, source: String, chapter_id: String) -> Option<String> {
        self.call(move |c| {
            c.query_row("SELECT dir FROM downloads WHERE source = ?1 AND chapter_id = ?2 AND state = 2", params![source, chapter_id], |r| r.get(0))
                .optional()
                .ok()
                .flatten()
        })
        .flatten()
    }

    /// Series that have at least one finished chapter.
    pub fn downloaded_series(&self) -> Vec<LibraryEntry> {
        self.call(|c| {
            let mut stmt = match c.prepare(
                "SELECT source, manga_id, MAX(manga_title), MAX(cover) FROM downloads WHERE state = 2 GROUP BY source, manga_id ORDER BY MAX(added) DESC",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |r| Ok(LibraryEntry { source: r.get(0)?, manga_id: r.get(1)?, title: r.get(2)?, cover: r.get(3)? }))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// What the series page shows without a network: stored details and the finished chapters.
    pub fn offline_series(&self, source: String, manga_id: String) -> Option<(MangaDetails, Vec<ChapterInfo>)> {
        self.call(move |c| {
            let mut stmt = c
                .prepare("SELECT details, manga_title, cover, chapter_id, chapter_name, chapter_number, chapter_date, scanlator FROM downloads WHERE source = ?1 AND manga_id = ?2 AND state = 2")
                .ok()?;
            let mut details: Option<MangaDetails> = None;
            let mut chapters = Vec::new();
            let rows = stmt
                .query_map(params![source, manga_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        ChapterInfo { id: r.get(3)?, name: r.get(4)?, number: r.get(5)?, date: r.get(6)?, scanlator: r.get(7)? },
                    ))
                })
                .ok()?;
            for (json, title, cover, ch) in rows.flatten() {
                if details.is_none() {
                    let mut d: MangaDetails = serde_json::from_str(&json).unwrap_or_default();
                    d.title = title;
                    d.cover = cover;
                    details = Some(d);
                }
                chapters.push(ch);
            }
            Some((details?, chapters))
        })
        .flatten()
    }

    /// Chapters that were queued or interrupted, oldest first.
    pub fn unfinished_downloads(&self) -> Vec<DownloadRow> {
        self.call(|c| {
            let mut stmt = match c.prepare(
                "SELECT source, manga_id, manga_title, cover, details, chapter_id, chapter_name, chapter_number, chapter_date, scanlator, dir FROM downloads WHERE state IN (0, 1) ORDER BY added, rowid",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |r| {
                Ok(DownloadRow {
                    source: r.get(0)?,
                    manga_id: r.get(1)?,
                    manga_title: r.get(2)?,
                    cover: r.get(3)?,
                    details: r.get(4)?,
                    chapter: ChapterInfo { id: r.get(5)?, name: r.get(6)?, number: r.get(7)?, date: r.get(8)?, scanlator: r.get(9)? },
                    dir: r.get(10)?,
                })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Forgets a chapter and returns its directory so the caller can delete the files.
    pub fn delete_download(&self, source: String, chapter_id: String) -> Option<String> {
        self.call(move |c| {
            let dir: Option<String> = c
                .query_row("SELECT dir FROM downloads WHERE source = ?1 AND chapter_id = ?2", params![source, chapter_id], |r| r.get(0))
                .optional()
                .ok()
                .flatten();
            let _ = c.execute("DELETE FROM downloads WHERE source = ?1 AND chapter_id = ?2", params![source, chapter_id]);
            dir
        })
        .flatten()
    }
}

/// A chapter to download together with everything needed to browse it offline.
#[derive(Clone, Debug)]
pub struct DownloadRow {
    pub source: String,
    pub manga_id: String,
    pub manga_title: String,
    pub cover: Option<String>,
    /// `MangaDetails` as JSON.
    pub details: String,
    pub chapter: ChapterInfo,
    pub dir: String,
}
