//! `LocalSource`: series folders under a root directory (default `~/Manga`). A series folder
//! holds one sub-folder per chapter, or comic archives (CBZ/ZIP/CBR/RAR/7z) as chapters,
//! e.g. volumes downloaded from Nyaa.

use std::path::{Path, PathBuf};

use crate::source::{ChapterInfo, MangaDetails, MangaPage, MangaSummary, PageRef, Source, SourceResult};
use crate::{archive, library};

pub struct LocalSource {
    id: &'static str,
    name: &'static str,
    root: PathBuf,
    /// Folder names under `root` that are not series (e.g. `Nyaa`, which has its own source).
    hidden: Vec<String>,
}

impl LocalSource {
    pub fn new(id: &'static str, name: &'static str, root: PathBuf, hidden: &[&str]) -> Self {
        Self { id, name, root, hidden: hidden.iter().map(|s| s.to_string()).collect() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn series_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        dirs.retain(|d| !self.hidden.contains(&name_of(d)));
        dirs.sort_by(|a, b| library::natural_cmp(&name_of(a), &name_of(b)));
        dirs
    }

    /// First image of the first chapter (a file path), or the first page of the first archive.
    fn cover_of(dir: &Path) -> Option<String> {
        let series = library::scan_series(dir);
        let chapter = series.chapters.first()?;
        if chapter.dir.is_file() {
            return archive::cover_of(&chapter.dir).map(|p| p.to_string_lossy().into_owned());
        }
        library::list_images(&chapter.dir).into_iter().next().map(|p| p.to_string_lossy().into_owned())
    }

    fn page(&self, items: Vec<PathBuf>) -> MangaPage {
        MangaPage {
            items: items
                .into_iter()
                .map(|d| MangaSummary { id: name_of(&d), title: name_of(&d), cover: Self::cover_of(&d) })
                .collect(),
            has_next: false,
        }
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// "Series v02 (2026) (Digital) (Group)" -> "v02 (2026) (Digital) (Group)" for the chapter list.
fn display_name(series: &str, chapter: &str) -> String {
    let rest = chapter.strip_prefix(series).map(str::trim).filter(|r| !r.is_empty());
    rest.unwrap_or(chapter).to_string()
}

impl Source for LocalSource {
    fn id(&self) -> &str {
        self.id
    }
    fn name(&self) -> &str {
        self.name
    }
    fn popular(&self, page: u32) -> SourceResult<MangaPage> {
        Ok(if page > 1 { MangaPage::default() } else { self.page(self.series_dirs()) })
    }
    fn latest(&self, page: u32) -> SourceResult<MangaPage> {
        self.popular(page)
    }
    fn search(&self, query: &str, page: u32) -> SourceResult<MangaPage> {
        let q = query.to_lowercase();
        let hits = self.series_dirs().into_iter().filter(|d| name_of(d).to_lowercase().contains(&q)).collect();
        Ok(if page > 1 { MangaPage::default() } else { self.page(hits) })
    }
    fn manga(&self, id: &str) -> SourceResult<MangaDetails> {
        let dir = self.root.join(id);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        Ok(MangaDetails {
            title: id.to_string(),
            cover: Self::cover_of(&dir),
            description: Some(format!("Local folder {}", dir.display())),
            ..Default::default()
        })
    }
    fn chapters(&self, manga_id: &str) -> SourceResult<Vec<ChapterInfo>> {
        let series = library::scan_series(&self.root.join(manga_id));
        Ok(series
            .chapters
            .iter()
            .enumerate()
            .map(|(i, c)| ChapterInfo {
                id: format!("{manga_id}/{}", c.name),
                name: display_name(manga_id, &c.name),
                number: Some((i + 1) as f64),
                ..Default::default()
            })
            .collect())
    }
    fn pages(&self, chapter_id: &str) -> SourceResult<Vec<PageRef>> {
        let (series, chapter) = chapter_id.split_once('/').unwrap_or((chapter_id, chapter_id));
        let found = library::scan_series(&self.root.join(series)).chapters.into_iter().find(|c| c.name == chapter);
        let pages = match found {
            Some(c) if c.dir.is_file() => archive::pages(&c.dir)?,
            Some(c) => library::read_pages(&c.dir),
            None => return Err(format!("{chapter_id} was not found on disk")),
        };
        Ok(pages
            .into_iter()
            .map(|p| PageRef { url: p.path.to_string_lossy().into_owned(), width: p.width, height: p.height, ..Default::default() })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn archives_are_chapters_next_to_folders_and_hidden_dirs_are_not_series() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("nezuyomi-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let series = root.join("Ironclad Honeymoon");
        std::fs::create_dir_all(&series).unwrap();
        std::fs::create_dir_all(root.join("Nyaa").join("Other")).unwrap();
        make_zip(&series.join("Ironclad Honeymoon v02 (2026) (Digital).cbz"), &[("1.png", png(6, 9)), ("2.png", png(6, 9))]);
        make_zip(&series.join("Ironclad Honeymoon v01 (2025) (Digital).cbz"), &[("a.png", png(4, 4))]);
        std::fs::write(series.join("notes.txt"), "x").unwrap();
        // SAFETY: serialized by LOCK with the other test that sets the cache location.
        unsafe { std::env::set_var("XDG_CACHE_HOME", root.join("cache")) };

        let src = LocalSource::new("local", "Local", root.clone(), &["Nyaa", "cache"]);
        let listed = src.popular(1).unwrap().items;
        assert_eq!(listed.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["Ironclad Honeymoon"], "Nyaa and the cache folder are not series");
        assert!(listed[0].cover.is_some(), "cover comes from the first volume");

        let ch = src.chapters("Ironclad Honeymoon").unwrap();
        assert_eq!(ch.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["v01 (2025) (Digital)", "v02 (2026) (Digital)"]);
        let pages = src.pages(&ch[1].id).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[0].height), (6, 9));
        assert!(Path::new(&pages[0].url).exists());
        assert!(src.pages("Ironclad Honeymoon/nope").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
