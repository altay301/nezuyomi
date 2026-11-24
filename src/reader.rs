//! The full-screen reader: `StripView`, the triple-tap control panel, progress
//! saving and chapter navigation. Everything here is main-thread glue; all
//! blocking work goes through [`crate::app::background`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};

use crate::app::background;
use crate::db::{Db, Progress};
use crate::pages;
use crate::source::{ChapterInfo, Source};
use crate::strip::StripView;

/// A chapter counts as read once the reader got this far.
const READ_THRESHOLD: f64 = 0.97;
/// Progress is written at most this often (PLAN.md: batches every 2 s).
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

struct Context {
    source: Arc<dyn Source>,
    manga_id: String,
    manga_title: String,
    /// Ascending reading order.
    chapters: Vec<ChapterInfo>,
    current: usize,
    /// Bumped on every chapter switch so late results of an old one are dropped.
    token: u64,
}

pub struct Reader {
    pub root: gtk::Overlay,
    strip: StripView,
    scrim: gtk::Box,
    title: gtk::Label,
    chapter: gtk::Label,
    progress_bar: gtk::ProgressBar,
    prev: gtk::Button,
    next: gtk::Button,
    db: Arc<Db>,
    ctx: RefCell<Option<Context>>,
    dirty: Cell<bool>,
    save_timer: RefCell<Option<glib::SourceId>>,
    /// Called when the user leaves the reader (Back button).
    on_exit: RefCell<Option<Box<dyn Fn()>>>,
    status: RefCell<Option<Box<dyn Fn(&str)>>>,
}

impl Reader {
    pub fn new(db: Arc<Db>, stats_label: Option<&gtk::Label>) -> Rc<Self> {
        let strip = StripView::new();
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&strip)
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::External)
            .build();
        let root = gtk::Overlay::new();
        root.set_child(Some(&scrolled));
        if let Some(l) = stats_label {
            root.add_overlay(l);
        }

        // ---- control panel ------------------------------------------------
        let title = gtk::Label::builder().css_classes(["retro-title"]).hexpand(true).halign(gtk::Align::Center).ellipsize(gtk::pango::EllipsizeMode::Middle).max_width_chars(40).build();
        let titlebar = gtk::Box::builder().css_classes(["retro-titlebar"]).build();
        titlebar.append(&title);
        let chapter = gtk::Label::builder().css_classes(["retro-text"]).xalign(0.0).build();
        let progress_bar = gtk::ProgressBar::builder().css_classes(["retro"]).build();
        let prev = gtk::Button::builder().label("◀ Previous").css_classes(["retro"]).build();
        let next = gtk::Button::builder().label("Next ▶").css_classes(["retro"]).build();
        let back = gtk::Button::builder().label("Back to library").css_classes(["retro"]).build();
        let buttons = gtk::Box::builder().spacing(10).homogeneous(true).build();
        for b in [&prev, &next, &back] {
            buttons.append(b);
        }
        let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).css_classes(["retro-body"]).build();
        body.append(&chapter);
        body.append(&progress_bar);
        body.append(&buttons);
        let panel = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["retro-panel"]).halign(gtk::Align::Center).valign(gtk::Align::Center).build();
        panel.append(&titlebar);
        panel.append(&body);
        let scrim = gtk::Box::builder().css_classes(["scrim"]).hexpand(true).vexpand(true).visible(false).build();
        scrim.append(&panel);
        root.add_overlay(&scrim);

        let this = Rc::new(Self {
            root,
            strip: strip.clone(),
            scrim: scrim.clone(),
            title,
            chapter,
            progress_bar,
            prev: prev.clone(),
            next: next.clone(),
            db,
            ctx: RefCell::new(None),
            dirty: Cell::new(false),
            save_timer: RefCell::new(None),
            on_exit: RefCell::new(None),
            status: RefCell::new(None),
        });

        strip.connect_panel_requested({
            let r = this.clone();
            move |_| r.show_panel()
        });
        prev.connect_clicked({
            let r = this.clone();
            move |_| r.step_chapter(-1)
        });
        next.connect_clicked({
            let r = this.clone();
            move |_| r.step_chapter(1)
        });
        back.connect_clicked({
            let r = this.clone();
            move |_| r.exit()
        });
        // Tapping outside the panel closes it and reading continues.
        let outside = gtk::GestureClick::new();
        outside.connect_pressed({
            let r = this.clone();
            let scrim = scrim.clone();
            move |_, _, x, y| {
                if scrim.pick(x, y, gtk::PickFlags::DEFAULT).is_some_and(|w| w == scrim.clone().upcast::<gtk::Widget>()) {
                    r.hide_panel();
                }
            }
        });
        scrim.add_controller(outside);

        // Progress saving: dirty flag on every scroll, one timer while dirty.
        if let Some(adj) = strip.vadjustment() {
            adj.connect_value_changed({
                let r = Rc::downgrade(&this);
                move |_| {
                    if let Some(r) = r.upgrade() {
                        r.mark_dirty();
                    }
                }
            });
        }
        this
    }

    pub fn set_on_exit(&self, f: impl Fn() + 'static) {
        *self.on_exit.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_status_sink(&self, f: impl Fn(&str) + 'static) {
        *self.status.borrow_mut() = Some(Box::new(f));
    }

    fn say(&self, msg: &str) {
        if let Some(f) = self.status.borrow().as_ref() {
            f(msg);
        }
    }

    pub fn strip(&self) -> &StripView {
        &self.strip
    }

    pub fn is_panel_open(&self) -> bool {
        self.scrim.is_visible()
    }

    pub fn toggle_panel(&self) {
        if self.is_panel_open() { self.hide_panel() } else { self.show_panel() }
    }

    /// Starts reading `chapters[index]` (ascending order) at `start_fraction`.
    pub fn open(
        self: &Rc<Self>,
        source: Arc<dyn Source>,
        manga_id: String,
        manga_title: String,
        chapters: Vec<ChapterInfo>,
        index: usize,
        start_fraction: f64,
    ) {
        self.flush_progress();
        let token = self.ctx.borrow().as_ref().map_or(0, |c| c.token) + 1;
        *self.ctx.borrow_mut() = Some(Context { source, manga_id, manga_title, chapters, current: index, token });
        self.load_current(start_fraction);
    }

    fn load_current(self: &Rc<Self>, start_fraction: f64) {
        let (source, chapter, title, index, count, token) = {
            let ctx = self.ctx.borrow();
            let Some(c) = ctx.as_ref() else { return };
            (c.source.clone(), c.chapters[c.current].clone(), c.manga_title.clone(), c.current, c.chapters.len(), c.token)
        };
        self.title.set_text(&format!("{title} · {}", chapter.name));
        self.chapter.set_text(&format!("Chapter {} of {}", index + 1, count));
        self.prev.set_sensitive(index > 0);
        self.next.set_sensitive(index + 1 < count);
        self.progress_bar.set_fraction(start_fraction);
        // Drop the old chapter's content, then forget the scroll events that caused.
        self.strip.set_pages(Vec::new(), 0.0);
        self.dirty.set(false);
        self.say(&format!("Loading {}…", chapter.name));

        let db = self.db.clone();
        let this = self.clone();
        background(
            move || source.pages(&chapter.id).map(|refs| pages::resolve(&db, refs)),
            move |result| {
                if this.ctx.borrow().as_ref().map(|c| c.token) != Some(token) {
                    return; // the user already moved on
                }
                match result {
                    Ok(pages) if !pages.is_empty() => {
                        this.say("");
                        this.strip.set_pages(pages, start_fraction);
                        if std::env::var_os("MANGA_OPEN_PANEL").is_some() {
                            this.show_panel();
                        }
                    }
                    Ok(_) => this.fail("This chapter has no pages."),
                    Err(e) => this.fail(&e),
                }
            },
        );
    }

    fn fail(&self, msg: &str) {
        eprintln!("[reader] {msg}");
        self.title.set_text("Cannot open chapter");
        self.chapter.set_text(msg);
        self.show_panel();
    }

    fn step_chapter(self: &Rc<Self>, delta: isize) {
        self.flush_progress();
        let moved = {
            let mut ctx = self.ctx.borrow_mut();
            let Some(c) = ctx.as_mut() else { return };
            let next = c.current as isize + delta;
            if (0..c.chapters.len() as isize).contains(&next) {
                // Going forward means the old chapter was finished.
                if delta > 0 {
                    let (s, m, ch) = (c.source.id().to_string(), c.manga_id.clone(), c.chapters[c.current].id.clone());
                    self.db.save_progress(s, m, ch, Progress { fraction: 1.0, read: true });
                }
                c.current = next as usize;
                c.token += 1;
                true
            } else {
                false
            }
        };
        if moved {
            self.hide_panel();
            self.load_current(0.0);
        }
    }

    fn exit(self: &Rc<Self>) {
        self.flush_progress();
        self.hide_panel();
        if let Some(f) = self.on_exit.borrow().as_ref() {
            f();
        }
    }

    pub fn show_panel(&self) {
        self.progress_bar.set_fraction(self.strip.progress());
        self.scrim.set_visible(true);
        self.prev.grab_focus();
    }

    pub fn hide_panel(&self) {
        self.scrim.set_visible(false);
        self.strip.grab_focus();
    }

    // ---- progress saving -----------------------------------------------------

    fn mark_dirty(self: &Rc<Self>) {
        self.dirty.set(true);
        if self.save_timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(SAVE_INTERVAL, move || {
            let Some(r) = weak.upgrade() else { return glib::ControlFlow::Break };
            r.save_timer.borrow_mut().take();
            r.flush_progress();
            glib::ControlFlow::Break
        });
        *self.save_timer.borrow_mut() = Some(id);
    }

    /// Hands the current position to the database thread (non-blocking).
    pub fn flush_progress(&self) {
        if !self.dirty.replace(false) {
            return;
        }
        let ctx = self.ctx.borrow();
        let Some(c) = ctx.as_ref() else { return };
        let fraction = self.strip.progress();
        self.db.save_progress(
            c.source.id().to_string(),
            c.manga_id.clone(),
            c.chapters[c.current].id.clone(),
            Progress { fraction, read: fraction >= READ_THRESHOLD },
        );
    }

    /// Esc toggles the panel; returns whether the key was handled.
    pub fn handle_key(&self, key: gdk::Key) -> bool {
        if key == gdk::Key::Escape {
            self.toggle_panel();
            return true;
        }
        false
    }
}
