//! SQLite on its own thread: the GTK main thread never touches the database.
//!
//! * `exec` is fire-and-forget (progress saves, library edits).
//! * `call` blocks for the answer, so use it from background threads only.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

use rusqlite::{params, Connection, OptionalExtension};

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
-- Image sizes by URL: the reader must know every page height before layout.
CREATE TABLE IF NOT EXISTS sizes (
    url TEXT PRIMARY KEY,
    w   INTEGER NOT NULL,
    h   INTEGER NOT NULL
);
";

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
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
}
