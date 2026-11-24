//! Series page: info, library toggle and the chapter list.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;

use crate::app::background;
use crate::covers::Covers;
use crate::db::{Db, DownloadRow, LibraryEntry, Progress};
use crate::downloads::{self, Downloads, Event};
use crate::source::{ChapterInfo, MangaDetails, MangaSummary, Source};

struct Loaded {
    source: Arc<dyn Source>,
    manga: MangaSummary,
    /// Ascending reading order.
    chapters: Vec<ChapterInfo>,
    progress: HashMap<String, Progress>,
    in_library: bool,
    details: MangaDetails,
    /// Showing stored data because the network failed.
    offline: bool,
}

/// chapter id -> (download state, pages done, pages total)
type DlStates = HashMap<String, (i64, u32, u32)>;

pub struct DetailsView {
    pub root: gtk::Box,
    cover: gtk::Picture,
    title: gtk::Label,
    meta: gtk::Label,
    genres: gtk::Label,
    description: gtk::Label,
    library_btn: gtk::Button,
    continue_btn: gtk::Button,
    download_btn: gtk::Button,
    dl: Arc<Downloads>,
    dl_state: RefCell<DlStates>,
    list: gtk::ListView,
    model: gtk::StringList,
    covers: Rc<Covers>,
    db: Arc<Db>,
    loaded: RefCell<Option<Loaded>>,
    on_back: RefCell<Option<Box<dyn Fn()>>>,
    on_read: RefCell<Option<Rc<dyn Fn(Arc<dyn Source>, MangaSummary, Vec<ChapterInfo>, usize, f64)>>>,
    on_status: RefCell<Option<Box<dyn Fn(&str)>>>,
}

impl DetailsView {
    pub fn new(covers: Rc<Covers>, db: Arc<Db>, dl: Arc<Downloads>) -> Rc<Self> {
        let back = gtk::Button::builder().label("◀ Back").css_classes(["retro"]).halign(gtk::Align::Start).build();
        let cover = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).width_request(130).height_request(185).css_classes(["cover-picture"]).valign(gtk::Align::Start).build();
        let title = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["details-title"]).build();
        let meta = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["retro-text"]).build();
        let genres = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["retro-text"]).build();
        let library_btn = gtk::Button::builder().label("Add to library").css_classes(["retro"]).build();
        let continue_btn = gtk::Button::builder().label("Start reading").css_classes(["retro"]).sensitive(false).build();
        let download_btn = gtk::Button::builder().label("Download next 10").css_classes(["retro"]).sensitive(false).build();
        let actions = gtk::Box::builder().spacing(8).build();
        actions.append(&library_btn);
        actions.append(&continue_btn);
        actions.append(&download_btn);
        let info = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).build();
        for w in [title.upcast_ref::<gtk::Widget>(), meta.upcast_ref(), genres.upcast_ref(), actions.upcast_ref()] {
            info.append(w);
        }
        let head = gtk::Box::builder().spacing(14).build();
        head.append(&cover);
        head.append(&info);

        let description = gtk::Label::builder().xalign(0.0).yalign(0.0).wrap(true).selectable(false).css_classes(["retro-text"]).build();
        let desc_scroll = gtk::ScrolledWindow::builder().child(&description).min_content_height(70).max_content_height(120).propagate_natural_height(true).hscrollbar_policy(gtk::PolicyType::Never).css_classes(["retro-well"]).build();

        let model = gtk::StringList::new(&[]);
        let factory = gtk::SignalListItemFactory::new();
        let list = gtk::ListView::builder().model(&gtk::NoSelection::new(Some(model.clone()))).factory(&factory).single_click_activate(true).vexpand(true).css_classes(["chapter-list"]).build();
        let list_scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).css_classes(["retro-well"]).build();

        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        root.append(&back);
        root.append(&head);
        root.append(&desc_scroll);
        root.append(&list_scroll);

        let this = Rc::new(Self {
            root,
            cover,
            title,
            meta,
            genres,
            description,
            library_btn: library_btn.clone(),
            continue_btn: continue_btn.clone(),
            download_btn: download_btn.clone(),
            dl,
            dl_state: RefCell::default(),
            list: list.clone(),
            model,
            covers,
            db,
            loaded: RefCell::new(None),
            on_back: RefCell::new(None),
            on_read: RefCell::new(None),
            on_status: RefCell::new(None),
        });

        // Row layout: name, note, download button. The chapter index travels in the row's widget name.
        factory.connect_setup({
            let w = Rc::downgrade(&this);
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let name = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
                let note = gtk::Label::builder().xalign(1.0).css_classes(["chapter-note"]).build();
                let button = gtk::Button::builder().css_classes(["retro", "retro-small"]).valign(gtk::Align::Center).build();
                button.connect_clicked({
                    let w = w.clone();
                    move |b| {
                        let idx = b.parent().and_then(|r| r.widget_name().parse::<usize>().ok());
                        if let (Some(t), Some(i)) = (w.upgrade(), idx) {
                            t.row_button(i);
                        }
                    }
                });
                let row = gtk::Box::builder().spacing(10).css_classes(["chapter-row"]).build();
                row.append(&name);
                row.append(&note);
                row.append(&button);
                item.set_child(Some(&row));
            }
        });
        download_btn.connect_clicked({
            let w = Rc::downgrade(&this);
            move |_| {
                if let Some(t) = w.upgrade() {
                    t.download_next(10)
                }
            }
        });

        // Row contents are (re)bound from the loaded chapter data.
        factory.connect_bind({
            let w = Rc::downgrade(&this);
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(t) = w.upgrade() else { return };
                let Some(idx) = item.item().and_downcast::<gtk::StringObject>().and_then(|s| s.string().parse::<usize>().ok()) else { return };
                let loaded = t.loaded.borrow();
                let Some(l) = loaded.as_ref() else { return };
                let Some(c) = l.chapters.get(idx) else { return };
                let row = item.child().and_downcast::<gtk::Box>().unwrap();
                let name = row.first_child().and_downcast::<gtk::Label>().unwrap();
                let button = row.last_child().and_downcast::<gtk::Button>().unwrap();
                let note = button.prev_sibling().and_downcast::<gtk::Label>().unwrap();
                let p = l.progress.get(&c.id).copied().unwrap_or_default();
                name.set_text(&c.name);
                row.set_widget_name(&idx.to_string());
                let dl_state = t.dl_state.borrow().get(&c.id).copied();
                button.set_visible(l.source.id() != "local" && !l.offline);
                let (label, dl_note) = match dl_state {
                    Some((downloads::STATE_DONE, ..)) => ("Remove", Some("saved".to_string())),
                    Some((downloads::STATE_RUNNING, done, total)) => ("Cancel", Some(format!("{done}/{total}"))),
                    Some((downloads::STATE_QUEUED, ..)) => ("Cancel", Some("queued".to_string())),
                    Some((downloads::STATE_FAILED, ..)) => ("Retry", Some("failed".to_string())),
                    _ => ("Download", None),
                };
                button.set_label(label);
                let mut parts = Vec::new();
                if let Some(n) = dl_note { parts.push(n) }
                if let Some(d) = &c.date { parts.push(d.clone()); }
                if let Some(s) = &c.scanlator { parts.push(s.clone()); }
                if p.read { parts.push("✓ read".into()) } else if p.fraction > 0.01 { parts.push(format!("{:.0}%", p.fraction * 100.0)) }
                note.set_text(&parts.join(" · "));
                row.set_css_classes(if p.read { &["chapter-row", "chapter-read"] } else { &["chapter-row"] });
            }
        });
        back.connect_clicked({
            let w = Rc::downgrade(&this);
            move |_| {
                if let Some(t) = w.upgrade() {
                    if let Some(f) = t.on_back.borrow().as_ref() { f() }
                }
            }
        });
        list.connect_activate({
            let w = Rc::downgrade(&this);
            move |_, pos| {
                if let Some(t) = w.upgrade() {
                    let idx = t.model.string(pos).and_then(|s| s.parse::<usize>().ok());
                    if let Some(i) = idx { t.read(i, None) }
                }
            }
        });
        library_btn.connect_clicked({
            let w = Rc::downgrade(&this);
            move |_| if let Some(t) = w.upgrade() { t.toggle_library() }
        });
        continue_btn.connect_clicked({
            let w = Rc::downgrade(&this);
            move |_| if let Some(t) = w.upgrade() { t.continue_reading() }
        });
        this
    }

    pub fn set_on_back(&self, f: impl Fn() + 'static) {
        *self.on_back.borrow_mut() = Some(Box::new(f));
    }
    pub fn set_on_status(&self, f: impl Fn(&str) + 'static) {
        *self.on_status.borrow_mut() = Some(Box::new(f));
    }
    pub fn set_on_read(&self, f: impl Fn(Arc<dyn Source>, MangaSummary, Vec<ChapterInfo>, usize, f64) + 'static) {
        *self.on_read.borrow_mut() = Some(Rc::new(f));
    }
    fn say(&self, msg: &str) {
        if let Some(f) = self.on_status.borrow().as_ref() { f(msg) }
    }

    /// Shows `manga` immediately from what the grid knew, then fills in the rest.
    pub fn open(self: &Rc<Self>, source: Arc<dyn Source>, manga: MangaSummary) {
        *self.loaded.borrow_mut() = None;
        self.model.splice(0, self.model.n_items(), &[]);
        self.title.set_text(&manga.title);
        self.meta.set_text("");
        self.genres.set_text("");
        self.description.set_text("");
        self.cover.set_paintable(gtk::gdk::Paintable::NONE);
        if let Some(url) = &manga.cover { self.covers.load(&self.cover, url) }
        self.continue_btn.set_sensitive(false);
        self.library_btn.set_sensitive(false);
        self.download_btn.set_sensitive(false);
        self.dl_state.borrow_mut().clear();
        self.say("Loading series…");

        let (db, src, id) = (self.db.clone(), source.clone(), manga.id.clone());
        let this = self.clone();
        let m2 = manga.clone();
        background(
            move || {
                let sid = src.id().to_string();
                // Online first; if the network fails, fall back to what downloads stored.
                let (details, mut chapters, offline) = match src.manga(&id).and_then(|d| src.chapters(&id).map(|c| (d, c))) {
                    Ok((d, c)) => (d, c, false),
                    Err(e) => match db.offline_series(sid.clone(), id.clone()) {
                        Some((d, c)) => (d, c, true),
                        None => return Err(e),
                    },
                };
                chapters.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));
                let progress = db.progress_for_manga(sid.clone(), id.clone());
                let in_library = db.is_in_library(sid.clone(), id.clone());
                let dl = db.download_states(sid, id);
                Ok::<_, String>((details, chapters, progress, in_library, dl, offline))
            },
            move |result: Result<(MangaDetails, Vec<ChapterInfo>, HashMap<String, Progress>, bool, DlStates, bool), String>| {
                match result {
                    Err(e) => {
                        eprintln!("[details] {e}");
                        this.say(&format!("Error: {e}"));
                    }
                    Ok((d, chapters, progress, in_library, dl, offline)) => {
                        this.title.set_text(&d.title);
                        this.meta.set_text(&[d.author.clone(), d.status.clone(), Some(format!("{} chapters", chapters.len()))].into_iter().flatten().collect::<Vec<_>>().join(" · "));
                        this.genres.set_text(&d.genres.join(", "));
                        this.description.set_text(d.description.as_deref().unwrap_or(""));
                        let mut manga = m2;
                        if manga.cover.is_none() {
                            manga.cover = d.cover.clone();
                            if let Some(url) = &d.cover { this.covers.load(&this.cover, url) }
                        }
                        manga.title = d.title.clone();
                        *this.dl_state.borrow_mut() = dl;
                        *this.loaded.borrow_mut() = Some(Loaded { source, manga, chapters, progress, in_library, details: d, offline });
                        this.refresh_rows();
                        this.say(&if offline {
                            format!("Offline: showing {} downloaded chapters", this.model.n_items())
                        } else {
                            format!("{} chapters", this.model.n_items())
                        });
                    }
                }
            },
        );
    }

    // ---- downloads -----------------------------------------------------------

    fn download_row(&self, idx: usize) -> Option<(Arc<dyn Source>, DownloadRow)> {
        let loaded = self.loaded.borrow();
        let l = loaded.as_ref()?;
        let chapter = l.chapters.get(idx)?.clone();
        let dir = downloads::chapter_dir(l.source.id(), &l.manga.id, &chapter.id);
        Some((
            l.source.clone(),
            DownloadRow {
                source: l.source.id().to_string(),
                manga_id: l.manga.id.clone(),
                manga_title: l.manga.title.clone(),
                cover: l.manga.cover.clone(),
                details: serde_json::to_string(&l.details).unwrap_or_else(|_| "{}".into()),
                chapter,
                dir: dir.to_string_lossy().into_owned(),
            },
        ))
    }

    fn enqueue(&self, idx: usize) {
        let Some((source, row)) = self.download_row(idx) else { return };
        self.dl_state.borrow_mut().insert(row.chapter.id.clone(), (downloads::STATE_QUEUED, 0, 0));
        self.dl.enqueue(source, row);
        self.update_row(idx);
    }

    /// The per-row button: download / cancel / remove / retry, depending on the state.
    fn row_button(&self, idx: usize) {
        let Some((_, row)) = self.download_row(idx) else { return };
        let state = self.dl_state.borrow().get(&row.chapter.id).map(|s| s.0);
        match state {
            Some(downloads::STATE_DONE | downloads::STATE_RUNNING | downloads::STATE_QUEUED) => {
                self.dl_state.borrow_mut().remove(&row.chapter.id);
                self.dl.remove(row.source, row.manga_id, row.chapter.id);
                self.update_row(idx);
            }
            _ => self.enqueue(idx),
        }
    }

    /// Queues the next `count` chapters after the furthest one read.
    fn download_next(&self, count: usize) {
        let start = {
            let loaded = self.loaded.borrow();
            let Some(l) = loaded.as_ref() else { return };
            l.chapters.iter().rposition(|c| l.progress.get(&c.id).is_some_and(|p| p.read || p.fraction > 0.01)).unwrap_or(0)
        };
        let n = self.loaded.borrow().as_ref().map_or(0, |l| l.chapters.len());
        let mut queued = 0;
        for idx in start..n {
            if queued >= count { break }
            let Some((_, row)) = self.download_row(idx) else { continue };
            let busy = self.dl_state.borrow().get(&row.chapter.id).is_some_and(|s| s.0 != downloads::STATE_FAILED);
            if !busy {
                self.enqueue(idx);
                queued += 1;
            }
        }
        self.say(&format!("Queued {queued} chapters for download"));
    }

    /// Rebinds a single row (the list shows the newest chapter first).
    fn update_row(&self, idx: usize) {
        let n = self.model.n_items() as usize;
        if idx < n {
            let pos = (n - 1 - idx) as u32;
            self.model.splice(pos, 1, &[&idx.to_string()]);
        }
    }

    /// Live updates from the download worker.
    pub fn on_download_event(&self, ev: &Event) {
        let (source, manga_id, chapter_id) = match ev {
            Event::Progress { source, manga_id, chapter_id, .. }
            | Event::Finished { source, manga_id, chapter_id, .. }
            | Event::Failed { source, manga_id, chapter_id, .. }
            | Event::Removed { source, manga_id, chapter_id } => (source, manga_id, chapter_id),
        };
        let idx = {
            let loaded = self.loaded.borrow();
            let Some(l) = loaded.as_ref() else { return };
            if l.source.id() != source || &l.manga.id != manga_id { return }
            let Some(idx) = l.chapters.iter().position(|c| &c.id == chapter_id) else { return };
            idx
        };
        let mut states = self.dl_state.borrow_mut();
        match ev {
            Event::Progress { done, total, .. } => { states.insert(chapter_id.clone(), (downloads::STATE_RUNNING, *done, *total)); }
            Event::Finished { .. } => { states.insert(chapter_id.clone(), (downloads::STATE_DONE, 0, 0)); }
            Event::Failed { .. } => { states.insert(chapter_id.clone(), (downloads::STATE_FAILED, 0, 0)); }
            Event::Removed { .. } => { states.remove(chapter_id); }
        }
        drop(states);
        self.update_row(idx);
    }

    /// Re-reads progress after returning from the reader.
    pub fn refresh_progress(self: &Rc<Self>) {
        let Some((source, id)) = self.loaded.borrow().as_ref().map(|l| (l.source.id().to_string(), l.manga.id.clone())) else { return };
        let (db, this) = (self.db.clone(), self.clone());
        background(move || db.progress_for_manga(source, id), move |p| {
            if let Some(l) = this.loaded.borrow_mut().as_mut() { l.progress = p }
            this.refresh_rows();
        });
    }

    fn refresh_rows(&self) {
        let loaded = self.loaded.borrow();
        let Some(l) = loaded.as_ref() else { return };
        // Newest chapter first. Re-splicing rebinds every visible row.
        let items: Vec<String> = (0..l.chapters.len()).rev().map(|i| i.to_string()).collect();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        self.model.splice(0, self.model.n_items(), &refs);
        self.library_btn.set_label(if l.in_library { "Remove from library" } else { "Add to library" });
        self.library_btn.set_sensitive(!l.offline);
        self.download_btn.set_sensitive(!l.offline && l.source.id() != "local" && !l.chapters.is_empty());
        self.continue_btn.set_sensitive(!l.chapters.is_empty());
        let started = l.progress.values().any(|p| p.read || p.fraction > 0.01);
        self.continue_btn.set_label(if started { "Continue reading" } else { "Start reading" });
    }

    fn toggle_library(&self) {
        let mut loaded = self.loaded.borrow_mut();
        let Some(l) = loaded.as_mut() else { return };
        l.in_library = !l.in_library;
        if l.in_library {
            self.db.add_to_library(LibraryEntry { source: l.source.id().to_string(), manga_id: l.manga.id.clone(), title: l.manga.title.clone(), cover: l.manga.cover.clone() });
        } else {
            self.db.remove_from_library(l.source.id().to_string(), l.manga.id.clone());
        }
        self.library_btn.set_label(if l.in_library { "Remove from library" } else { "Add to library" });
    }

    fn continue_reading(self: &Rc<Self>) {
        let target = {
            let loaded = self.loaded.borrow();
            let Some(l) = loaded.as_ref() else { return };
            // The chapter read last; if it is finished, the one after it.
            let last = l.progress.iter().max_by_key(|(id, _)| l.chapters.iter().position(|c| &c.id == *id).unwrap_or(0)).map(|(id, p)| (id.clone(), *p));
            match last.and_then(|(id, p)| l.chapters.iter().position(|c| c.id == id).map(|i| (i, p))) {
                Some((i, p)) if p.read && i + 1 < l.chapters.len() => (i + 1, 0.0),
                Some((i, p)) => (i, if p.read { 0.0 } else { p.fraction }),
                None => (0, 0.0),
            }
        };
        self.read(target.0, Some(target.1));
    }

    fn read(&self, index: usize, fraction: Option<f64>) {
        let loaded = self.loaded.borrow();
        let Some(l) = loaded.as_ref() else { return };
        let start = fraction.unwrap_or_else(|| {
            let p = l.progress.get(&l.chapters[index].id).copied().unwrap_or_default();
            if p.read { 0.0 } else { p.fraction }
        });
        if let Some(f) = self.on_read.borrow().clone() {
            f(l.source.clone(), l.manga.clone(), l.chapters.clone(), index, start);
        }
    }
}
