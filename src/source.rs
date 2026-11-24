//! The `Source` trait: the only thing the rest of the app knows about where
//! manga come from (PLAN.md → Extension model). All methods block, so call
//! them from a background thread.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MangaSummary {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub cover: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MangaPage {
    #[serde(default)]
    pub items: Vec<MangaSummary>,
    #[serde(default)]
    pub has_next: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MangaDetails {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChapterInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub number: Option<f64>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub scanlator: Option<String>,
}

/// One page image. `width`/`height` of 0 mean "unknown, read the header".
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PageRef {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

pub type SourceResult<T> = Result<T, String>;

pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn popular(&self, page: u32) -> SourceResult<MangaPage>;
    fn latest(&self, page: u32) -> SourceResult<MangaPage>;
    fn search(&self, query: &str, page: u32) -> SourceResult<MangaPage>;
    fn manga(&self, id: &str) -> SourceResult<MangaDetails>;
    fn chapters(&self, manga_id: &str) -> SourceResult<Vec<ChapterInfo>>;
    fn pages(&self, chapter_id: &str) -> SourceResult<Vec<PageRef>>;
}
