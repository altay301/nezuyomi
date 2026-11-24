//! Turns the page list of a source into `PageInfo`s whose sizes are all known,
//! so the reader can lay out the whole chapter before a single pixel is decoded.
//! Blocking: background threads only.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use crate::db::Db;
use crate::library::{PageInfo, Remote};
use crate::net;
use crate::paths;
use crate::source::{PageRef, Source};

const HEADER_BYTES: usize = 16 * 1024;
const PARALLEL_HEADER_FETCHES: usize = 6;

pub fn is_local(url: &str) -> bool {
    url.starts_with('/')
}

fn dims_of(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok().filter(|d| d.0 > 0 && d.1 > 0)
}

/// Image size from the first bytes of the file (HTTP range request); falls back
/// to the full download if the header is not within the first few KB.
fn fetch_dims(url: &str, headers: &[(String, String)]) -> Option<(u32, u32)> {
    let mut ranged = headers.to_vec();
    ranged.push(("Range".into(), format!("bytes=0-{}", HEADER_BYTES - 1)));
    if let Ok(r) = net::request("GET", url, None, &ranged) {
        if r.status < 400 {
            if let Some(d) = dims_of(&r.body) {
                return Some(d);
            }
        }
    }
    dims_of(&net::get_bytes(url, headers).ok()?)
}

/// Pages of a chapter: from disk if it was downloaded, otherwise from the source.
pub fn load(db: &Db, source: &dyn Source, chapter_id: &str) -> Result<Vec<PageInfo>, String> {
    if let Some(dir) = db.downloaded_dir(source.id().to_string(), chapter_id.to_string()) {
        if let Some(pages) = crate::downloads::read_manifest(std::path::Path::new(&dir)) {
            return Ok(pages);
        }
    }
    Ok(resolve(db, source.pages(chapter_id)?))
}

pub fn resolve(db: &Db, refs: Vec<PageRef>) -> Vec<PageInfo> {
    let remote_urls: Vec<String> = refs.iter().filter(|r| !is_local(&r.url) && (r.width == 0 || r.height == 0)).map(|r| r.url.clone()).collect();
    let mut known: HashMap<String, (u32, u32)> = if remote_urls.is_empty() { HashMap::new() } else { db.page_sizes(remote_urls) };

    // Still unknown: read the headers, a few requests at a time.
    let missing: Vec<&PageRef> = refs
        .iter()
        .filter(|r| (r.width == 0 || r.height == 0) && !known.contains_key(&r.url))
        .collect();
    let mut fetched: Vec<(String, u32, u32)> = Vec::new();
    for chunk in missing.chunks(PARALLEL_HEADER_FETCHES) {
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|r| {
                    s.spawn(move || {
                        let headers: Vec<(String, String)> = r.headers.clone().into_iter().collect();
                        let dims = if is_local(&r.url) {
                            image::ImageReader::open(&r.url).ok().and_then(|x| x.with_guessed_format().ok()).and_then(|x| x.into_dimensions().ok())
                        } else {
                            fetch_dims(&r.url, &headers)
                        };
                        (r.url.clone(), dims)
                    })
                })
                .collect();
            for h in handles {
                if let Ok((url, Some((w, hh)))) = h.join() {
                    fetched.push((url, w, hh));
                }
            }
        });
    }
    for (u, w, h) in &fetched {
        known.insert(u.clone(), (*w, *h));
    }
    let to_store: Vec<_> = fetched.into_iter().filter(|(u, _, _)| !is_local(u)).collect();
    if !to_store.is_empty() {
        db.store_page_sizes(to_store);
    }

    refs.into_iter()
        .filter_map(|r| {
            let (width, height) = if r.width > 0 && r.height > 0 { (r.width, r.height) } else { *known.get(&r.url)? };
            if is_local(&r.url) {
                Some(PageInfo { path: PathBuf::from(&r.url), width, height, remote: None })
            } else {
                Some(PageInfo {
                    path: paths::cache_file("pages", &r.url),
                    width,
                    height,
                    remote: Some(Remote { url: r.url, headers: r.headers.into_iter().collect() }),
                })
            }
        })
        .collect()
}
