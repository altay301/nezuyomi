//! Comic archives (CBZ/ZIP, CBR/RAR, 7z) as chapters of a local series (PLAN.md → Nyaa →
//! "Archives as downloaded local content").
//!
//! Opening a volume extracts its images once into `~/.cache/manga-reader/archives/<id>/`
//! (with a `pages.json` manifest like finished downloads) and the normal reader pipeline takes
//! over. ZIP/CBZ is read in-process; RAR/CBR/7z go through the installed `7z` (or `unar`).
//! Archives come from the internet: entry names are sanitised, only supported image types are
//! written, and entry count and sizes are capped.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::downloads::{self, ManifestPage};
use crate::library::{natural_cmp, PageInfo};
use crate::paths;

const ARCHIVE_EXTS: [&str; 5] = ["cbz", "zip", "cbr", "rar", "7z"];
/// Image types the reader can decode. JPEG XL and AVIF pages are recognised but unsupported.
const IMAGE_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
const UNSUPPORTED_IMAGE_EXTS: [&str; 3] = ["jxl", "avif", "heic"];
const MAX_ENTRIES: usize = 5_000;
const MAX_ENTRY_BYTES: u64 = 200 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(180);

pub fn is_archive(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| ARCHIVE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Cache directory of one archive; changes when the file changes.
pub fn cache_dir_of(archive: &Path) -> PathBuf {
    let (size, mtime) = std::fs::metadata(archive)
        .map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())))
        .unwrap_or((0, 0));
    paths::cache_dir().join("archives").join(format!("{:016x}-{size:x}-{mtime:x}", paths::fnv1a(&archive.to_string_lossy())))
}

/// The pages of an archive (extracting it first if needed). Blocking; background threads only.
pub fn pages(archive: &Path) -> Result<Vec<PageInfo>, String> {
    let dir = cache_dir_of(archive);
    if let Some(p) = downloads::read_manifest(&dir) {
        // Touch the manifest: it is what cache trimming treats as "last used".
        if let Ok(f) = std::fs::OpenOptions::new().append(true).open(dir.join("pages.json")) {
            let _ = f.set_modified(SystemTime::now());
        }
        return Ok(p);
    }
    let tmp = dir.with_extension(format!("tmp{}-{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let result = extract_to(archive, &tmp).and_then(|files| build_manifest(&tmp, files));
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if std::fs::rename(&tmp, &dir).is_err() {
        // Someone else extracted it at the same time: use theirs.
        let _ = std::fs::remove_dir_all(&tmp);
    }
    downloads::read_manifest(&dir).ok_or_else(|| "the archive contains no readable pages".to_string())
}

/// Page files in reading order, named `0001.jpg`, … inside `dir`.
fn extract_to(archive: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let ext = archive.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    // A ".cbr" is often really a zip; look at the magic bytes, not the name.
    let mut magic = [0u8; 4];
    let _ = std::fs::File::open(archive).and_then(|mut f| f.read_exact(&mut magic));
    let is_zip = &magic[..2] == b"PK";
    if is_zip || ext == "cbz" || ext == "zip" {
        if is_zip { extract_zip(archive, dir) } else { Err(format!("{} is not a valid zip file", archive.display())) }
    } else {
        extract_with_tool(archive, dir)
    }
}

fn extract_zip(archive: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", archive.display()))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("the archive has {} entries (limit {MAX_ENTRIES})", zip.len()));
    }
    // (sort key, entry index, extension), images only; `enclosed_name` drops `..` and absolute paths.
    let mut wanted: Vec<(String, usize, String)> = Vec::new();
    let mut unsupported = 0;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.enclosed_name().map(|p| p.to_string_lossy().replace('\\', "/")) else { continue };
        let ext = ext_of(&name);
        if IMAGE_EXTS.contains(&ext.as_str()) {
            wanted.push((name, i, ext));
        } else if UNSUPPORTED_IMAGE_EXTS.contains(&ext.as_str()) {
            unsupported += 1;
        }
    }
    if wanted.is_empty() {
        return Err(no_pages_message(unsupported));
    }
    wanted.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let (mut files, mut total) = (Vec::new(), 0u64);
    for (n, (_, index, ext)) in wanted.iter().enumerate() {
        let mut entry = zip.by_index(*index).map_err(|e| e.to_string())?;
        let out_name = format!("{:04}.{ext}", n + 1);
        let mut out = std::fs::File::create(dir.join(&out_name)).map_err(|e| e.to_string())?;
        // `take` bounds the copy even if the header lies about the size (zip bomb).
        let written = std::io::copy(&mut (&mut entry).take(MAX_ENTRY_BYTES + 1), &mut out).map_err(|e| format!("{}: {e}", archive.display()))?;
        if written > MAX_ENTRY_BYTES {
            return Err("a page in the archive is larger than 200 MB".into());
        }
        total += written;
        if total > MAX_TOTAL_BYTES {
            return Err("the archive expands to more than 4 GiB".into());
        }
        files.push(out_name);
    }
    Ok(files)
}

fn tool_candidates() -> [(&'static str, bool); 4] {
    // (command, is 7-Zip style)
    [("7z", true), ("7zz", true), ("7za", true), ("unar", false)]
}

fn extract_with_tool(archive: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let raw = dir.join("_raw");
    std::fs::create_dir_all(&raw).map_err(|e| e.to_string())?;
    let mut last_err = String::new();
    let mut ran = false;
    for (tool, seven) in tool_candidates() {
        let mut cmd = Command::new("nice");
        cmd.args(["-n", "10", tool]);
        if seven {
            cmd.args(["x", "-y", "-mmt=1", "-bd", &format!("-o{}", raw.display()), "--"]).arg(archive);
        } else {
            cmd.args(["-q", "-f", "-o"]).arg(&raw).arg(archive);
        }
        let mut child = match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(_) => continue,
        };
        ran = true;
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if started.elapsed() > TOOL_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break None,
            }
        };
        match status {
            Some(s) if s.success() => {
                last_err.clear();
                break;
            }
            Some(_) => {
                let mut msg = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut msg);
                }
                last_err = format!("{tool} could not extract {}: {}", archive.display(), msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("unknown error"));
            }
            None => last_err = format!("{tool} timed out on {}", archive.display()),
        }
        let _ = std::fs::remove_dir_all(&raw);
        let _ = std::fs::create_dir_all(&raw);
    }
    if !ran {
        return Err("no tool to open RAR/7z archives: install 7zip (sudo dnf install 7zip) or unar".into());
    }
    if !last_err.is_empty() {
        return Err(last_err);
    }
    // Collect the images from what the tool wrote; symlinks and anything else are ignored.
    let mut found: Vec<(String, PathBuf, String)> = Vec::new();
    let mut unsupported = 0;
    let mut stack = vec![raw.clone()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(format!("the archive has more than {MAX_ENTRIES} entries"));
            }
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let rel = path.strip_prefix(&raw).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                let ext = ext_of(&rel);
                if IMAGE_EXTS.contains(&ext.as_str()) {
                    found.push((rel, path, ext));
                } else if UNSUPPORTED_IMAGE_EXTS.contains(&ext.as_str()) {
                    unsupported += 1;
                }
            }
        }
    }
    if found.is_empty() {
        return Err(no_pages_message(unsupported));
    }
    found.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let mut total = 0u64;
    let mut files = Vec::new();
    for (n, (_, path, ext)) in found.iter().enumerate() {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total += len;
        if len > MAX_ENTRY_BYTES || total > MAX_TOTAL_BYTES {
            return Err("the archive is larger than the extraction limits".into());
        }
        let out_name = format!("{:04}.{ext}", n + 1);
        std::fs::rename(path, dir.join(&out_name)).map_err(|e| e.to_string())?;
        files.push(out_name);
    }
    let _ = std::fs::remove_dir_all(&raw);
    Ok(files)
}

fn no_pages_message(unsupported: usize) -> String {
    if unsupported > 0 {
        format!("this volume's {unsupported} pages are JPEG XL/AVIF images, which the reader cannot decode")
    } else {
        "the archive contains no readable pages".into()
    }
}

/// Reads every page's size from its header and writes the manifest. Pages that are not valid
/// images are dropped (a broken page must not hide the rest of the volume).
fn build_manifest(dir: &Path, files: Vec<String>) -> Result<(), String> {
    let mut manifest = Vec::new();
    for f in files {
        let dims = image::ImageReader::open(dir.join(&f)).ok().and_then(|r| r.with_guessed_format().ok()).and_then(|r| r.into_dimensions().ok());
        match dims {
            Some((w, h)) if w > 0 && h > 0 => manifest.push(ManifestPage { file: f, w, h }),
            _ => {
                let _ = std::fs::remove_file(dir.join(&f));
            }
        }
    }
    if manifest.is_empty() {
        return Err("none of the pages in the archive could be read as an image".into());
    }
    downloads::write_manifest(dir, &manifest)
}

/// Cover for a series: the first image of an archive, cached; ZIP/CBZ only (extracting a whole
/// RAR/7z just for a thumbnail on every grid load would be wasteful). `None` if not possible.
pub fn cover_of(archive: &Path) -> Option<PathBuf> {
    let target = paths::cache_dir().join("covers").join(format!("archive-{:016x}.img", paths::fnv1a(&format!("{}{:?}", archive.display(), std::fs::metadata(archive).ok().and_then(|m| m.modified().ok())))));
    if target.exists() {
        return Some(target);
    }
    // Already extracted for reading: take its first page.
    if let Some(first) = downloads::read_manifest(&cache_dir_of(archive)).and_then(|p| p.into_iter().next()) {
        return Some(first.path);
    }
    let mut magic = [0u8; 2];
    std::fs::File::open(archive).and_then(|mut f| f.read_exact(&mut magic)).ok()?;
    if &magic != b"PK" {
        return None;
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).ok()?).ok()?;
    let mut names: Vec<(String, usize)> = (0..zip.len().min(MAX_ENTRIES))
        .filter_map(|i| {
            let e = zip.by_index(i).ok()?;
            let n = e.enclosed_name()?.to_string_lossy().replace('\\', "/");
            (!e.is_dir() && IMAGE_EXTS.contains(&ext_of(&n).as_str())).then_some((n, i))
        })
        .collect();
    names.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let (_, index) = names.first()?;
    let mut entry = zip.by_index(*index).ok()?;
    std::fs::create_dir_all(target.parent()?).ok()?;
    let mut out = std::fs::File::create(&target).ok()?;
    std::io::copy(&mut (&mut entry).take(MAX_ENTRY_BYTES), &mut out).ok()?;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("manga-reader-arch-{tag}-{}-{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::new_rgb8(w, h).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn make_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    /// Run the cache under a private XDG_CACHE_HOME so tests never touch the real cache.
    fn with_cache<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmp(tag);
        // SAFETY: serialized by LOCK; no other test thread reads the environment concurrently.
        unsafe { std::env::set_var("XDG_CACHE_HOME", dir.join("cache")) };
        let r = f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn cbz_pages_come_out_in_natural_order_with_sizes() {
        with_cache("cbz", |dir| {
            let a = dir.join("Series v01.cbz");
            make_zip(&a, &[("b/10.png", png(30, 40)), ("b/2.png", png(10, 20)), ("b/1.png", png(10, 20)), ("notes.txt", b"hi".to_vec()), ("ComicInfo.xml", b"<x/>".to_vec())]);
            let pages = pages(&a).unwrap();
            assert_eq!(pages.len(), 3);
            assert_eq!((pages[0].width, pages[0].height), (10, 20));
            assert_eq!((pages[2].width, pages[2].height), (30, 40), "10.png sorts after 2.png");
            assert!(pages.iter().all(|p| p.path.exists() && p.remote.is_none()));
            // Second call hits the manifest: same pages, no re-extraction.
            let again = super::pages(&a).unwrap();
            assert_eq!(again[0].path, pages[0].path);
        });
    }

    #[test]
    fn hostile_zip_entries_are_not_written_outside_the_cache() {
        with_cache("evil", |dir| {
            let a = dir.join("evil.cbz");
            make_zip(&a, &[("../../escape.png", png(5, 5)), ("/abs/path.png", png(5, 5)), ("ok.png", png(7, 7))]);
            let pages = pages(&a).unwrap();
            // The traversal entry is dropped; the absolute-path entry is harmless because pages are
            // always written under our own numbered names inside the cache folder.
            assert_eq!(pages.len(), 2);
            assert!(!dir.join("escape.png").exists() && !dir.parent().unwrap().join("escape.png").exists());
            assert!(!Path::new("/abs/path.png").exists());
            assert!(pages.iter().all(|p| p.path.starts_with(dir.join("cache"))), "{pages:?}");
        });
    }

    #[test]
    fn archives_of_jpeg_xl_pages_say_so() {
        with_cache("jxl", |dir| {
            let a = dir.join("jxl.cbz");
            make_zip(&a, &[("001.jxl", vec![0; 10]), ("002.jxl", vec![0; 10])]);
            let e = pages(&a).unwrap_err();
            assert!(e.contains("JPEG XL"), "{e}");
        });
    }

    #[test]
    fn broken_pages_do_not_hide_the_rest_and_all_broken_is_an_error() {
        with_cache("broken", |dir| {
            let a = dir.join("b.cbz");
            make_zip(&a, &[("1.png", png(4, 4)), ("2.png", b"not an image".to_vec())]);
            assert_eq!(pages(&a).unwrap().len(), 1);
            let b = dir.join("c.cbz");
            make_zip(&b, &[("1.png", b"nope".to_vec())]);
            assert!(pages(&b).is_err());
            assert!(pages(&dir.join("missing.cbz")).is_err());
        });
    }

    #[test]
    fn a_cbr_that_is_really_a_zip_opens_and_real_7z_goes_through_the_tool() {
        with_cache("tool", |dir| {
            let a = dir.join("fake.cbr");
            make_zip(&a, &[("1.png", png(6, 6))]);
            assert_eq!(pages(&a).unwrap().len(), 1, "detected by magic bytes");
            // A genuine 7z archive, if the tool is installed.
            let src = dir.join("src");
            std::fs::create_dir_all(src.join("sub")).unwrap();
            std::fs::write(src.join("sub/1.png"), png(8, 9)).unwrap();
            std::fs::write(src.join("sub/2.png"), png(8, 9)).unwrap();
            let out = dir.join("real.7z");
            let ok = Command::new("7z").args(["a", "-bd"]).arg(&out).arg(src.join("sub")).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            if ok {
                let p = pages(&out).unwrap();
                assert_eq!(p.len(), 2);
                assert_eq!((p[0].width, p[0].height), (8, 9));
            } else {
                eprintln!("7z not installed: skipped the 7z part");
            }
        });
    }

    #[test]
    fn cover_is_the_first_image_of_a_cbz() {
        with_cache("cover", |dir| {
            let a = dir.join("c.cbz");
            make_zip(&a, &[("002.png", png(9, 9)), ("001.png", png(3, 3))]);
            let c = cover_of(&a).unwrap();
            let (w, _) = image::ImageReader::open(&c).unwrap().with_guessed_format().unwrap().into_dimensions().unwrap();
            assert_eq!(w, 3);
            assert!(cover_of(&dir.join("none.7z")).is_none());
        });
    }

    #[test]
    fn archive_extensions() {
        for n in ["a.cbz", "a.ZIP", "a.cbr", "a.rar", "a.7z"] {
            assert!(is_archive(Path::new(n)), "{n}");
        }
        for n in ["a.epub", "a.jpg", "a", "a.cbz.part"] {
            assert!(!is_archive(Path::new(n)), "{n}");
        }
    }
}
