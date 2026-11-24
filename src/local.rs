//! `LocalSource`: series folders under a root directory (default `~/Manga`),
//! each containing one sub-folder per chapter.

use std::path::{Path, PathBuf};

use crate::library;
use crate::source::{ChapterInfo, MangaDetails, MangaPage, MangaSummary, PageRef, Source, SourceResult};

pub struct LocalSource {
    root: PathBuf,
}

impl LocalSource {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn series_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        dirs.sort_by(|a, b| library::natural_cmp(&name_of(a), &name_of(b)));
        dirs
    }

    fn first_image(dir: &Path) -> Option<String> {
        let series = library::scan_series(dir);
        let chapter = series.chapters.first()?;
        library::list_images(&chapter.dir).into_iter().next().map(|p| p.to_string_lossy().into_owned())
    }

    fn page(&self, items: Vec<PathBuf>) -> MangaPage {
        MangaPage {
            items: items
                .into_iter()
                .map(|d| MangaSummary { id: name_of(&d), title: name_of(&d), cover: Self::first_image(&d) })
                .collect(),
            has_next: false,
        }
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl Source for LocalSource {
    fn id(&self) -> &str {
        "local"
    }
    fn name(&self) -> &str {
        "Local"
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
            cover: Self::first_image(&dir),
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
                name: c.name.clone(),
                number: Some((i + 1) as f64),
                ..Default::default()
            })
            .collect())
    }
    fn pages(&self, chapter_id: &str) -> SourceResult<Vec<PageRef>> {
        // Series folders that directly contain images are a single chapter named like the series.
        let mut dir = self.root.join(chapter_id);
        if !dir.is_dir() {
            dir = self.root.join(chapter_id.split('/').next().unwrap_or_default());
        }
        Ok(library::read_pages(&dir)
            .into_iter()
            .map(|p| PageRef { url: p.path.to_string_lossy().into_owned(), width: p.width, height: p.height, ..Default::default() })
            .collect())
    }
}
