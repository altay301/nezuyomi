//! Chapter downloads for offline reading.
//!
//! One worker thread at low CPU/I/O priority, one chapter at a time, pages one
//! after another with a short pause: slow on purpose, so a long queue neither
//! heats the laptop nor hammers a site. Files live under
//! `~/.local/share/manga-reader/downloads/<source>/<series>/<chapter>/` with a
//! `pages.json` manifest, so a finished chapter opens without any network.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::{Db, DownloadRow};
use crate::library::PageInfo;
use crate::source::Source;
use crate::{net, pages, paths};

const PAUSE_BETWEEN_PAGES: Duration = Duration::from_millis(60);
pub const STATE_QUEUED: i64 = 0;
pub const STATE_RUNNING: i64 = 1;
pub const STATE_DONE: i64 = 2;
pub const STATE_FAILED: i64 = 3;

#[derive(Clone, Debug)]
pub enum Event {
    Progress { source: String, manga_id: String, chapter_id: String, done: u32, total: u32 },
    Finished { source: String, manga_id: String, chapter_id: String, name: String },
    Failed { source: String, manga_id: String, chapter_id: String, name: String, error: String },
    Removed { source: String, manga_id: String, chapter_id: String },
}

#[derive(Serialize, Deserialize)]
pub struct ManifestPage {
    pub file: String,
    pub w: u32,
    pub h: u32,
}

/// Writes `pages.json` next to the page files (shared by downloads and extracted archives).
pub fn write_manifest(dir: &Path, pages: &[ManifestPage]) -> Result<(), String> {
    let json = serde_json::to_vec(pages).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("pages.json"), json).map_err(|e| format!("{}: {e}", dir.display()))
}

struct Task {
    source: Arc<dyn Source>,
    row: DownloadRow,
}

struct Shared {
    queue: Mutex<VecDeque<Task>>,
    wake: Condvar,
    /// (source id, chapter id) of chapters whose download must stop.
    cancelled: Mutex<HashSet<(String, String)>>,
}

pub struct Downloads {
    shared: Arc<Shared>,
    db: Arc<Db>,
    events: async_channel::Sender<Event>,
}

/// `<downloads>/<source>/<series hash>/<chapter hash>`
pub fn chapter_dir(source: &str, manga_id: &str, chapter_id: &str) -> PathBuf {
    paths::downloads_dir()
        .join(source)
        .join(format!("{:016x}", paths::fnv1a(manga_id)))
        .join(format!("{:016x}", paths::fnv1a(chapter_id)))
}

impl Downloads {
    pub fn new(db: Arc<Db>, events: async_channel::Sender<Event>) -> Arc<Self> {
        let shared = Arc::new(Shared { queue: Mutex::default(), wake: Condvar::new(), cancelled: Mutex::default() });
        let this = Arc::new(Self { shared: shared.clone(), db: db.clone(), events: events.clone() });
        std::thread::Builder::new()
            .name("downloads".into())
            .spawn(move || {
                crate::loader::lower_priority();
                loop {
                    let task = {
                        let mut q = shared.queue.lock().unwrap();
                        loop {
                            if let Some(t) = q.pop_front() {
                                break t;
                            }
                            q = shared.wake.wait(q).unwrap();
                        }
                    };
                    let (source, chapter_id) = (task.row.source.clone(), task.row.chapter.id.clone());
                    let cancelled = || shared.cancelled.lock().unwrap().contains(&(source.clone(), chapter_id.clone()));
                    if cancelled() {
                        shared.cancelled.lock().unwrap().remove(&(source.clone(), chapter_id.clone()));
                        continue;
                    }
                    let result = run(&db, &task, &events, &cancelled);
                    shared.cancelled.lock().unwrap().remove(&(source.clone(), chapter_id.clone()));
                    let ev = match result {
                        Ok(()) => Event::Finished { source, manga_id: task.row.manga_id.clone(), chapter_id, name: task.row.chapter.name.clone() },
                        Err(e) if e == CANCELLED => continue,
                        Err(error) => {
                            eprintln!("[downloads] {}: {error}", task.row.chapter.name);
                            db.set_download_state(source.clone(), chapter_id.clone(), STATE_FAILED, 0, 0);
                            Event::Failed { source, manga_id: task.row.manga_id.clone(), chapter_id, name: task.row.chapter.name.clone(), error }
                        }
                    };
                    let _ = events.send_blocking(ev);
                }
            })
            .expect("spawn download worker");
        this
    }

    /// Queues a chapter. Never blocks: the database write is asynchronous too.
    pub fn enqueue(&self, source: Arc<dyn Source>, row: DownloadRow) {
        self.db.queue_download(row.clone());
        self.shared.cancelled.lock().unwrap().remove(&(row.source.clone(), row.chapter.id.clone()));
        self.shared.queue.lock().unwrap().push_back(Task { source, row });
        self.shared.wake.notify_one();
    }

    /// Re-queues what a previous run left unfinished.
    pub fn resume(&self, rows: Vec<DownloadRow>, sources: &[Arc<dyn Source>]) {
        for row in rows {
            if let Some(source) = sources.iter().find(|s| s.id() == row.source) {
                self.enqueue(source.clone(), row);
            }
        }
    }

    /// Cancels a queued/running download or deletes a finished one (files included).
    pub fn remove(&self, source: String, manga_id: String, chapter_id: String) {
        self.shared.cancelled.lock().unwrap().insert((source.clone(), chapter_id.clone()));
        self.shared.queue.lock().unwrap().retain(|t| !(t.row.source == source && t.row.chapter.id == chapter_id));
        let (db, events) = (self.db.clone(), self.events.clone());
        std::thread::spawn(move || {
            crate::loader::lower_priority();
            if let Some(dir) = db.delete_download(source.clone(), chapter_id.clone()) {
                let _ = std::fs::remove_dir_all(&dir);
                // Drop the series folder (and its cover) once its last chapter is gone.
                if let Some(parent) = Path::new(&dir).parent() {
                    let only_cover = std::fs::read_dir(parent).map(|d| d.flatten().all(|e| e.file_name() == "cover")).unwrap_or(false);
                    if only_cover {
                        let _ = std::fs::remove_dir_all(parent);
                    }
                }
            }
            let _ = events.send_blocking(Event::Removed { source, manga_id, chapter_id });
        });
    }
}

const CANCELLED: &str = "cancelled";

fn ext_of(url: &str) -> &str {
    url.split(['?', '#']).next().and_then(|u| u.rsplit_once('.')).map(|(_, e)| e).filter(|e| (1..=4).contains(&e.len())).unwrap_or("img")
}

fn run(db: &Db, task: &Task, events: &async_channel::Sender<Event>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let row = &task.row;
    let dir = PathBuf::from(&row.dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    db.set_download_state(row.source.clone(), row.chapter.id.clone(), STATE_RUNNING, 0, 0);

    let refs = task.source.pages(&row.chapter.id)?;
    let pages: Vec<PageInfo> = pages::resolve(db, refs);
    if pages.is_empty() {
        return Err("the chapter has no pages".into());
    }
    let total = pages.len() as u32;
    let mut manifest = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        if cancelled() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(CANCELLED.into());
        }
        let Some(remote) = &page.remote else { return Err("local pages cannot be downloaded".into()) };
        let file = format!("{:03}.{}", i + 1, ext_of(&remote.url));
        let target = dir.join(&file);
        if !target.exists() {
            let bytes = net::get_bytes(&remote.url, &remote.headers)?;
            let tmp = target.with_extension("part");
            std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
        }
        manifest.push(ManifestPage { file, w: page.width, h: page.height });
        let done = i as u32 + 1;
        db.set_download_state(row.source.clone(), row.chapter.id.clone(), STATE_RUNNING, done, total);
        let _ = events.send_blocking(Event::Progress { source: row.source.clone(), manga_id: row.manga_id.clone(), chapter_id: row.chapter.id.clone(), done, total });
        std::thread::sleep(PAUSE_BETWEEN_PAGES);
    }
    let json = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("pages.json"), json).map_err(|e| e.to_string())?;
    save_cover(db, row);
    db.set_download_state(row.source.clone(), row.chapter.id.clone(), STATE_DONE, total, total);
    Ok(())
}

/// Keeps one copy of the cover next to the series so the Downloads page works offline.
fn save_cover(db: &Db, row: &DownloadRow) {
    let Some(url) = row.cover.as_deref().filter(|u| u.starts_with("http")) else { return };
    let Some(series_dir) = Path::new(&row.dir).parent() else { return };
    let target = series_dir.join("cover");
    if !target.exists() {
        let referer = url.split('/').take(3).collect::<Vec<_>>().join("/") + "/";
        let Ok(bytes) = net::get_bytes(url, &[("Referer".into(), referer)]) else { return };
        if std::fs::write(&target, bytes).is_err() {
            return;
        }
    }
    db.set_download_cover(row.source.clone(), row.manga_id.clone(), target.to_string_lossy().into_owned());
}

/// Pages of a finished download, straight from its manifest.
pub fn read_manifest(dir: &Path) -> Option<Vec<PageInfo>> {
    let list: Vec<ManifestPage> = serde_json::from_slice(&std::fs::read(dir.join("pages.json")).ok()?).ok()?;
    let pages: Vec<PageInfo> = list
        .into_iter()
        .map(|p| PageInfo { path: dir.join(p.file), width: p.w, height: p.h, remote: None })
        .collect();
    (!pages.is_empty()).then_some(pages)
}
