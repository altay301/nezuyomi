//! Nyaa tables of the shared SQLite database (all calls go to the database thread).

use std::collections::HashSet;

use rusqlite::{params, OptionalExtension};

use crate::db::Db;

pub const SEEN: &str = "seen";
pub const SKIPPED: &str = "skipped";
pub const UNPARSED: &str = "unparsed";
/// Matches a link, waits to be queued (notify mode, qBittorrent down, no disk space, …).
pub const MATCHED: &str = "matched";
/// Over the size cap: waits for a manual "Download".
pub const CONFIRM: &str = "confirm";
pub const DOWNLOADING: &str = "downloading";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uploader {
    pub name: String,
    pub enabled: bool,
    pub last_checked: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: i64,
    pub source: String,
    pub manga_id: String,
    pub title: String,
    pub uploader: String,
    /// Phrase a release title must contain (whole words, case/punctuation-insensitive).
    pub query: String,
    pub auto: bool,
    /// Optional regex; a release title matching it is ignored.
    pub exclude: String,
    pub folder: String,
    /// `standard` or `colored`.
    pub edition: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Release {
    pub info_hash: String,
    pub uploader: String,
    pub title: String,
    pub size: i64,
    pub pub_date: String,
    pub link: String,
    pub state: String,
    pub link_id: Option<i64>,
    /// Comma-separated volume numbers this release is expected to add (0 = whole run).
    pub volumes: String,
    pub note: String,
    pub first_seen: i64,
}

pub fn parse_volumes(csv: &str) -> Vec<u32> {
    csv.split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

pub fn join_volumes(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

const RELEASE_COLS: &str = "info_hash, uploader, title, size, pub_date, link, state, link_id, volumes, note, first_seen";

fn release_row(r: &rusqlite::Row) -> rusqlite::Result<Release> {
    Ok(Release {
        info_hash: r.get(0)?,
        uploader: r.get(1)?,
        title: r.get(2)?,
        size: r.get(3)?,
        pub_date: r.get(4)?,
        link: r.get(5)?,
        state: r.get(6)?,
        link_id: r.get(7)?,
        volumes: r.get(8)?,
        note: r.get(9)?,
        first_seen: r.get(10)?,
    })
}

fn link_row(r: &rusqlite::Row) -> rusqlite::Result<Link> {
    Ok(Link { id: r.get(0)?, source: r.get(1)?, manga_id: r.get(2)?, title: r.get(3)?, uploader: r.get(4)?, query: r.get(5)?, auto: r.get::<_, i64>(6)? != 0, exclude: r.get(7)?, folder: r.get(8)?, edition: r.get(9)? })
}

const LINK_COLS: &str = "id, source, manga_id, title, uploader, query, auto, exclude, folder, edition";

impl Db {
    // ---- settings ----------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        let key = key.to_string();
        self.call(move |c| c.query_row("SELECT value FROM settings WHERE key = ?1", [&key], |r| r.get(0)).optional().ok().flatten()).flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let (key, value) = (key.to_string(), value.to_string());
        self.exec(move |c| {
            let _ = c.execute("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, value]);
        });
    }

    // ---- uploaders -----------------------------------------------------------------

    pub fn nyaa_uploaders(&self) -> Vec<Uploader> {
        self.call(|c| {
            let Ok(mut st) = c.prepare("SELECT name, enabled, last_checked FROM nyaa_uploaders ORDER BY name COLLATE NOCASE") else { return Vec::new() };
            st.query_map([], |r| Ok(Uploader { name: r.get(0)?, enabled: r.get::<_, i64>(1)? != 0, last_checked: r.get(2)? }))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    pub fn nyaa_add_uploader(&self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.exec(move |c| {
            let _ = c.execute("INSERT OR IGNORE INTO nyaa_uploaders (name) VALUES (?1)", [name]);
        });
    }

    pub fn nyaa_remove_uploader(&self, name: &str) {
        let name = name.to_string();
        self.exec(move |c| {
            let _ = c.execute("DELETE FROM nyaa_uploaders WHERE name = ?1", [name]);
        });
    }

    pub fn nyaa_set_uploader_enabled(&self, name: &str, enabled: bool) {
        let name = name.to_string();
        self.exec(move |c| {
            let _ = c.execute("UPDATE nyaa_uploaders SET enabled = ?2 WHERE name = ?1", params![name, enabled]);
        });
    }

    pub fn nyaa_touch_uploader(&self, name: &str) {
        let name = name.to_string();
        self.exec(move |c| {
            let _ = c.execute("UPDATE nyaa_uploaders SET last_checked = ?2 WHERE name = ?1", params![name, now()]);
        });
    }

    // ---- links -------------------------------------------------------------------------

    pub fn nyaa_add_link(&self, l: Link) -> Option<i64> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO nyaa_links (source, manga_id, title, uploader, query, auto, exclude, folder, edition) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (source, manga_id, edition) DO UPDATE SET title = excluded.title, uploader = excluded.uploader, query = excluded.query, auto = excluded.auto, exclude = excluded.exclude, folder = excluded.folder",
                params![l.source, l.manga_id, l.title, l.uploader, l.query, l.auto, l.exclude, l.folder, l.edition],
            )
            .ok()?;
            c.query_row("SELECT id FROM nyaa_links WHERE source = ?1 AND manga_id = ?2 AND edition = ?3", params![l.source, l.manga_id, l.edition], |r| r.get(0)).ok()
        })
        .flatten()
    }

    pub fn nyaa_links(&self) -> Vec<Link> {
        self.call(|c| {
            let Ok(mut st) = c.prepare(&format!("SELECT {LINK_COLS} FROM nyaa_links ORDER BY title COLLATE NOCASE")) else { return Vec::new() };
            st.query_map([], link_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }

    pub fn nyaa_link(&self, id: i64) -> Option<Link> {
        self.call(move |c| c.query_row(&format!("SELECT {LINK_COLS} FROM nyaa_links WHERE id = ?1"), [id], link_row).optional().ok().flatten()).flatten()
    }

    pub fn nyaa_link_for_series(&self, source: &str, manga_id: &str) -> Option<Link> {
        let (s, m) = (source.to_string(), manga_id.to_string());
        self.call(move |c| c.query_row(&format!("SELECT {LINK_COLS} FROM nyaa_links WHERE source = ?1 AND manga_id = ?2 ORDER BY edition = 'colored', id LIMIT 1"), params![s, m], link_row).optional().ok().flatten()).flatten()
    }

    pub fn nyaa_set_link_auto(&self, id: i64, auto: bool) {
        self.exec(move |c| {
            let _ = c.execute("UPDATE nyaa_links SET auto = ?2 WHERE id = ?1", params![id, auto]);
        });
    }

    pub fn nyaa_remove_link(&self, id: i64) {
        self.exec(move |c| {
            let _ = c.execute("DELETE FROM nyaa_links WHERE id = ?1", [id]);
            // Everything that was decided because of this link is open again (skipped ones included).
            let _ = c.execute("UPDATE nyaa_releases SET link_id = NULL, state = 'seen', note = '' WHERE link_id = ?1 AND state IN ('matched', 'confirm', 'skipped')", [id]);
        });
    }

    // ---- releases ----------------------------------------------------------------------

    /// Stores a release the first time it is seen; returns whether it was new.
    pub fn nyaa_insert_release(&self, r: Release) -> bool {
        self.call(move |c| {
            c.execute(
                &format!("INSERT OR IGNORE INTO nyaa_releases ({RELEASE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
                params![r.info_hash, r.uploader, r.title, r.size, r.pub_date, r.link, r.state, r.link_id, r.volumes, r.note, now()],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
        })
        .unwrap_or(false)
    }

    pub fn nyaa_release(&self, hash: &str) -> Option<Release> {
        let hash = hash.to_string();
        self.call(move |c| c.query_row(&format!("SELECT {RELEASE_COLS} FROM nyaa_releases WHERE info_hash = ?1"), [hash], release_row).optional().ok().flatten()).flatten()
    }

    pub fn nyaa_releases_in_states(&self, states: &[&str]) -> Vec<Release> {
        let list = states.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(",");
        self.call(move |c| {
            let Ok(mut st) = c.prepare(&format!("SELECT {RELEASE_COLS} FROM nyaa_releases WHERE state IN ({list}) ORDER BY first_seen, rowid")) else { return Vec::new() };
            st.query_map([], release_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Newest first, for the Releases page.
    pub fn nyaa_recent_releases(&self, limit: u32) -> Vec<Release> {
        self.call(move |c| {
            let Ok(mut st) = c.prepare(&format!("SELECT {RELEASE_COLS} FROM nyaa_releases ORDER BY first_seen DESC, rowid DESC LIMIT {limit}")) else { return Vec::new() };
            st.query_map([], release_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }

    pub fn nyaa_releases_of_link(&self, link_id: i64) -> Vec<Release> {
        self.call(move |c| {
            let Ok(mut st) = c.prepare(&format!("SELECT {RELEASE_COLS} FROM nyaa_releases WHERE link_id = ?1 ORDER BY first_seen, rowid")) else { return Vec::new() };
            st.query_map([link_id], release_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }

    pub fn nyaa_set_release(&self, hash: &str, state: &str, link_id: Option<i64>, volumes: Option<&str>, note: &str) {
        let (hash, state, volumes, note) = (hash.to_string(), state.to_string(), volumes.map(str::to_string), note.to_string());
        self.exec(move |c| {
            let _ = c.execute(
                "UPDATE nyaa_releases SET state = ?2, link_id = COALESCE(?3, link_id), volumes = COALESCE(?4, volumes), note = ?5 WHERE info_hash = ?1",
                params![hash, state, link_id, volumes, note],
            );
        });
    }

    // ---- volumes -----------------------------------------------------------------------

    pub fn nyaa_owned_volumes(&self, link_id: i64) -> HashSet<u32> {
        self.call(move |c| {
            let Ok(mut st) = c.prepare("SELECT volume FROM nyaa_volumes WHERE link_id = ?1") else { return HashSet::new() };
            st.query_map([link_id], |r| r.get::<_, u32>(0)).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Volumes of releases that are queued or downloading, optionally ignoring one release.
    pub fn nyaa_queued_volumes(&self, link_id: i64, except_hash: &str) -> HashSet<u32> {
        let except = except_hash.to_string();
        self.call(move |c| {
            let Ok(mut st) = c.prepare("SELECT volumes FROM nyaa_releases WHERE link_id = ?1 AND state = 'downloading' AND info_hash <> ?2") else { return HashSet::new() };
            st.query_map(params![link_id, except], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).flat_map(|v| parse_volumes(&v)).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    pub fn nyaa_add_volume(&self, link_id: i64, volume: u32, file_path: &str, info_hash: &str) {
        let (path, hash) = (file_path.to_string(), info_hash.to_string());
        self.exec(move |c| {
            let _ = c.execute("INSERT OR REPLACE INTO nyaa_volumes (link_id, volume, file_path, info_hash) VALUES (?1, ?2, ?3, ?4)", params![link_id, volume, path, hash]);
        });
    }

    pub fn nyaa_volumes_of_link(&self, link_id: i64) -> Vec<(u32, String)> {
        self.call(move |c| {
            let Ok(mut st) = c.prepare("SELECT volume, file_path FROM nyaa_volumes WHERE link_id = ?1 ORDER BY volume") else { return Vec::new() };
            st.query_map([link_id], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
        })
        .unwrap_or_default()
    }
}
