//! Where the app keeps its data (XDG base directories) and the page cache.

use std::path::PathBuf;
use std::time::SystemTime;

const APP: &str = "nezuyomi";
/// The name before the rename; data written under it is moved over (see `migrate_legacy_dirs`).
const LEGACY_APP: &str = "manga-reader";
/// The page cache is trimmed (oldest files first) to this size at startup.
const CACHE_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

/// Moves `<base>/manga-reader` to `<base>/nezuyomi` for the data, config and cache locations, so the
/// library, downloads, settings and caches survive the rename. Entries that already exist in the
/// new folder are never overwritten. Call once at start-up, before anything opens the database.
pub fn migrate_legacy_dirs() {
    for (var, fallback) in [("XDG_DATA_HOME", ".local/share"), ("XDG_CONFIG_HOME", ".config"), ("XDG_CACHE_HOME", ".cache")] {
        let base = xdg(var, fallback);
        if let Err(e) = merge_dir(&base.join(LEGACY_APP), &base.join(APP)) {
            eprintln!("[paths] could not move {} to {}: {e}", base.join(LEGACY_APP).display(), base.join(APP).display());
        }
    }
}

/// Moves every entry of `old` into `new` unless it already exists there, then removes `old` if empty.
/// Same file system, so each move is a cheap rename (open files of a still running old version stay valid).
pub(crate) fn merge_dir(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    if !old.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(new)?;
    for entry in std::fs::read_dir(old)? {
        let entry = entry?;
        let target = new.join(entry.file_name());
        if !target.exists() {
            std::fs::rename(entry.path(), target)?;
        }
    }
    // Leftovers (names present in both) stay where they are; an empty old folder is removed.
    let _ = std::fs::remove_dir(old);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nezuyomi-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_old_folder_is_moved_with_everything_in_it() {
        let base = tmp("move");
        let old = base.join("manga-reader");
        std::fs::create_dir_all(old.join("downloads/x")).unwrap();
        std::fs::write(old.join("library.db"), b"db").unwrap();
        std::fs::write(old.join("downloads/x/001.jpg"), b"img").unwrap();
        merge_dir(&old, &base.join("nezuyomi")).unwrap();
        assert_eq!(std::fs::read(base.join("nezuyomi/library.db")).unwrap(), b"db");
        assert!(base.join("nezuyomi/downloads/x/001.jpg").exists());
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_new_folder_is_merged_and_nothing_is_overwritten() {
        let base = tmp("merge");
        let (old, new) = (base.join("manga-reader"), base.join("nezuyomi"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("library.db"), b"old data").unwrap();
        std::fs::write(old.join("only-old"), b"1").unwrap();
        std::fs::write(new.join("library.db"), b"new, already there").unwrap(); // e.g. an empty database a first start created
        std::fs::write(new.join("only-new"), b"2").unwrap();
        merge_dir(&old, &new).unwrap();
        assert_eq!(std::fs::read(new.join("library.db")).unwrap(), b"new, already there", "never overwritten");
        assert!(new.join("only-old").exists() && new.join("only-new").exists());
        assert!(old.join("library.db").exists(), "the clashing file stays in the old folder, nothing is lost");
        merge_dir(&base.join("missing"), &base.join("anything")).unwrap(); // no old folder: nothing to do
        assert!(!base.join("anything").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
