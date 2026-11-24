//! Local folder source: `<root>/<Chapter>/<page images>`.
//!
//! All functions here touch the disk and must be called off the main thread.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

const EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// One page image with its original pixel size (read from the header only).
#[derive(Clone, Debug)]
pub struct PageInfo {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub name: String,
    pub dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Series {
    pub name: String,
    pub chapters: Vec<Chapter>,
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Compares file names the way people expect: `2.jpg` < `10.jpg`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut n = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        n.push(c);
                        it.next();
                    }
                    n.trim_start_matches('0').to_string()
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().next().cmp(&y.to_lowercase().next());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort_by(|a, b| {
        natural_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    entries
}

/// A folder with sub-folders is a series of chapters; a folder that directly
/// contains images is a single chapter.
pub fn scan_series(root: &Path) -> Series {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Series".into());
    let entries = sorted_entries(root);
    let mut chapters: Vec<Chapter> = entries
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| Chapter {
            name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            dir: p.clone(),
        })
        .collect();
    if chapters.is_empty() && entries.iter().any(|p| is_image(p)) {
        chapters.push(Chapter { name: name.clone(), dir: root.to_path_buf() });
    }
    Series { name, chapters }
}

/// Lists the pages of a chapter and reads each image size from its header.
pub fn read_pages(dir: &Path) -> Vec<PageInfo> {
    sorted_entries(dir)
        .into_iter()
        .filter(|p| p.is_file() && is_image(p))
        .filter_map(|path| {
            let (width, height) = image::ImageReader::open(&path)
                .ok()?
                .with_guessed_format()
                .ok()?
                .into_dimensions()
                .ok()?;
            (width > 0 && height > 0).then_some(PageInfo { path, width, height })
        })
        .collect()
}
