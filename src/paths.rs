//! Where the app keeps its data (XDG base directories) and the page cache.

use std::path::PathBuf;
use std::time::SystemTime;

const APP: &str = "manga-reader";
/// The page cache is trimmed (oldest files first) to this size at startup.
const CACHE_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join(APP)
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join(APP)
}

pub fn extensions_dir() -> PathBuf {
    data_dir().join("extensions")
}

pub fn downloads_dir() -> PathBuf {
    data_dir().join("downloads")
}

pub fn db_path() -> PathBuf {
    data_dir().join("library.db")
}

pub fn manga_root() -> PathBuf {
    home().join("Manga")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join(APP)
}

pub fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Cache file of a remote image (`pages/`) or cover thumbnail (`covers/`).
pub fn cache_file(kind: &str, url: &str) -> PathBuf {
    let ext = url.split(['?', '#']).next().and_then(|u| u.rsplit_once('.')).map(|(_, e)| e).filter(|e| e.len() <= 4).unwrap_or("img");
    cache_dir().join(kind).join(format!("{:016x}.{ext}", fnv1a(url)))
}

/// Deletes the oldest cached files until the cache fits [`CACHE_LIMIT_BYTES`]. Extracted
/// archives (`archives/<id>/`) count and are removed as one unit.
pub fn trim_cache() {
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for kind in ["pages", "covers"] {
        if let Ok(rd) = std::fs::read_dir(cache_dir().join(kind)) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    entries.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), e.path()));
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(cache_dir().join("archives")) {
        for e in rd.flatten() {
            let dir = e.path();
            let size: u64 = std::fs::read_dir(&dir).map(|d| d.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0);
            // The manifest is written last and touched whenever the volume is opened.
            let used = std::fs::metadata(dir.join("pages.json")).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push((used, size, dir));
        }
    }
    let mut total: u64 = entries.iter().map(|f| f.1).sum();
    entries.sort();
    for (_, size, path) in entries {
        if total <= CACHE_LIMIT_BYTES {
            break;
        }
        let removed = if path.is_dir() { std::fs::remove_dir_all(&path).is_ok() } else { std::fs::remove_file(&path).is_ok() };
        if removed {
            total -= size;
        }
    }
}
