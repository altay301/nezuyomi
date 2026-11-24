//! Cover grid used for both "Library" and a source's "Browse" page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;

use crate::app::background;
use crate::covers::{Covers, THUMB_H, THUMB_W};
use crate::db::{Db, LibraryEntry};
use crate::source::{MangaPage, MangaSummary, Source};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    Popular,
    Latest,
}

pub struct BrowseView {
    pub root: gtk::Box,
    toolbar: gtk::Box,
    search: gtk::SearchEntry,
    popular: gtk::ToggleButton,
    latest: gtk::ToggleButton,
    flow: gtk::FlowBox,
    scrolled: gtk::ScrolledWindow,
    covers: Rc<Covers>,
    source: RefCell<Option<Arc<dyn Source>>>,
    listing: Cell<Listing>,
    query: RefCell<String>,
    page: Cell<u32>,
    has_next: Cell<bool>,
    loading: Cell<bool>,
    /// Invalidates results of requests started before the last reset.
    epoch: Cell<u64>,
    /// (source id, series) of every card, in grid order.
    items: RefCell<Vec<(String, MangaSummary)>>,
    on_open: RefCell<Option<Rc<dyn Fn(Arc<dyn Source>, MangaSummary)>>>,
    on_status: RefCell<Option<Rc<dyn Fn(&str)>>>,
    sources: Vec<Arc<dyn Source>>,
}

impl BrowseView {
    pub fn new(covers: Rc<Covers>, sources: Vec<Arc<dyn Source>>) -> Rc<Self> {
        let search = gtk::SearchEntry::builder().css_classes(["retro"]).hexpand(true).placeholder_text("Search this source").build();
        let popular = gtk::ToggleButton::builder().label("Popular").css_classes(["retro"]).active(true).build();
        let latest = gtk::ToggleButton::builder().label("Latest").css_classes(["retro"]).group(&popular).build();
        let toolbar = gtk::Box::builder().spacing(8).css_classes(["retro-toolbar"]).build();
        toolbar.append(&search);
        toolbar.append(&popular);
        toolbar.append(&latest);

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .homogeneous(true)
            .valign(gtk::Align::Start)
            .min_children_per_line(2)
            .max_children_per_line(12)
            .row_spacing(10)
            .column_spacing(10)
            .css_classes(["cover-grid"])
            .build();
        let scrolled = gtk::ScrolledWindow::builder().child(&flow).vexpand(true).hexpand(true).css_classes(["retro-well"]).build();
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        root.append(&toolbar);
        root.append(&scrolled);

        let this = Rc::new(Self {
            root,
            toolbar,
            search: search.clone(),
            popular: popular.clone(),
            latest: latest.clone(),
            flow: flow.clone(),
            scrolled: scrolled.clone(),
            covers,
            source: RefCell::new(None),
            listing: Cell::new(Listing::Popular),
            query: RefCell::default(),
            page: Cell::new(0),
            has_next: Cell::new(false),
            loading: Cell::new(false),
            epoch: Cell::new(0),
            items: RefCell::default(),
            on_open: RefCell::new(None),
            on_status: RefCell::new(None),
            sources,
        });

        // Infinite scroll: ask for the next page when the bottom is reached.
        scrolled.connect_edge_reached({
            let w = Rc::downgrade(&this);
            move |_, pos| {
                if let (gtk::PositionType::Bottom, Some(t)) = (pos, w.upgrade()) {
                    t.load_more();
                }
            }
        });
        search.connect_activate({
            let w = Rc::downgrade(&this);
            move |e| {
                if let Some(t) = w.upgrade() {
                    *t.query.borrow_mut() = e.text().trim().to_string();
                    t.reload();
                }
            }
        });
        // Clearing the box shows the listing again.
        search.connect_search_changed({
            let w = Rc::downgrade(&this);
            move |e| {
                if let Some(t) = w.upgrade() {
                    if e.text().is_empty() && !t.query.borrow().is_empty() {
                        t.query.borrow_mut().clear();
                        t.reload();
                    }
                }
            }
        });
        for (btn, l) in [(&popular, Listing::Popular), (&latest, Listing::Latest)] {
            btn.connect_toggled({
                let w = Rc::downgrade(&this);
                move |b| {
                    if let (true, Some(t)) = (b.is_active(), w.upgrade()) {
                        t.listing.set(l);
                        t.search.set_text("");
                        t.query.borrow_mut().clear();
                        t.reload();
                    }
                }
            });
        }
        flow.connect_child_activated({
            let w = Rc::downgrade(&this);
            move |_, child| {
                let Some(t) = w.upgrade() else { return };
                let Some(item) = t.items.borrow().get(child.index() as usize).cloned() else { return };
                let source = t.sources.iter().find(|s| s.id() == item.0).cloned();
                if let (Some(source), Some(cb)) = (source, t.on_open.borrow().clone()) {
                    cb(source, item.1);
                }
            }
        });
        this
    }

    pub fn set_on_open(&self, f: impl Fn(Arc<dyn Source>, MangaSummary) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_on_status(&self, f: impl Fn(&str) + 'static) {
        *self.on_status.borrow_mut() = Some(Rc::new(f));
    }

    fn say(&self, msg: &str) {
        if let Some(f) = self.on_status.borrow().clone() {
            f(msg);
        }
    }

    fn clear(&self) {
        self.epoch.set(self.epoch.get() + 1);
        while let Some(c) = self.flow.first_child() {
            self.flow.remove(&c);
        }
        self.items.borrow_mut().clear();
        self.page.set(0);
        self.has_next.set(false);
        self.loading.set(false);
    }

    fn add_card(&self, source_id: &str, m: &MangaSummary) {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .width_request(THUMB_W as i32 * 3 / 4)
            .height_request(THUMB_H as i32 * 3 / 4)
            .halign(gtk::Align::Center)
            .css_classes(["cover-picture"])
            .build();
        if let Some(url) = &m.cover {
            self.covers.load(&picture, url);
        }
        let title = gtk::Label::builder()
            .label(&m.title)
            .lines(2)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(16)
            .width_chars(16)
            .css_classes(["cover-title"])
            .build();
        let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).css_classes(["cover-card"]).build();
        card.append(&picture);
        card.append(&title);
        let child = gtk::FlowBoxChild::new();
        child.set_child(Some(&card));
        self.flow.append(&child);
        self.items.borrow_mut().push((source_id.to_string(), m.clone()));
    }

    // ---- modes ---------------------------------------------------------------

    pub fn show_library(self: &Rc<Self>, db: Arc<Db>) {
        self.toolbar.set_visible(false);
        self.clear();
        let epoch = self.epoch.get();
        let this = self.clone();
        background(move || db.library(), move |entries: Vec<LibraryEntry>| {
            if this.epoch.get() != epoch {
                return;
            }
            for e in &entries {
                this.add_card(&e.source, &MangaSummary { id: e.manga_id.clone(), title: e.title.clone(), cover: e.cover.clone() });
            }
            this.say(&match entries.len() {
                0 => "Your library is empty. Open Browse and add a series.".to_string(),
                n => format!("{n} series in your library"),
            });
        });
    }

    pub fn show_source(self: &Rc<Self>, source: Arc<dyn Source>) {
        self.toolbar.set_visible(true);
        self.search.set_placeholder_text(Some(&format!("Search {}", source.name())));
        *self.source.borrow_mut() = Some(source);
        self.search.set_text("");
        self.query.borrow_mut().clear();
        self.listing.set(Listing::Popular);
        self.popular.set_active(true);
        self.reload();
    }

    fn reload(self: &Rc<Self>) {
        self.clear();
        self.scrolled.vadjustment().set_value(0.0);
        self.load_more();
    }

    fn load_more(self: &Rc<Self>) {
        let Some(source) = self.source.borrow().clone() else { return };
        if self.loading.get() || (self.page.get() > 0 && !self.has_next.get()) {
            return;
        }
        self.loading.set(true);
        self.say("Loading…");
        let page = self.page.get() + 1;
        let (query, listing, epoch) = (self.query.borrow().clone(), self.listing.get(), self.epoch.get());
        let this = self.clone();
        let src = source.clone();
        background(
            move || {
                if !query.is_empty() {
                    src.search(&query, page)
                } else if listing == Listing::Latest {
                    src.latest(page)
                } else {
                    src.popular(page)
                }
            },
            move |result: Result<MangaPage, String>| {
                if this.epoch.get() != epoch {
                    return;
                }
                this.loading.set(false);
                match result {
                    Ok(p) => {
                        this.page.set(page);
                        this.has_next.set(p.has_next);
                        for m in &p.items {
                            this.add_card(source.id(), m);
                        }
                        this.say(&format!("{} series from {}", this.items.borrow().len(), source.name()));
                    }
                    Err(e) => {
                        eprintln!("[browse] {e}");
                        this.say(&format!("Error: {e}"));
                    }
                }
            },
        );
    }
}
