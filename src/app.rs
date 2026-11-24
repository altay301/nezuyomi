//! Application shell: main window (places sidebar, cover grid, series page,
//! status bar), the full-screen reader and the navigation between them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};

use crate::browse::BrowseView;
use crate::covers::Covers;
use crate::db::Db;
use crate::details::DetailsView;
use crate::ext::JsSource;
use crate::local::LocalSource;
use crate::reader::Reader;
use crate::source::{ChapterInfo, MangaSummary, Source};
use crate::{paths, stats};

const CURSOR_HIDE_AFTER: Duration = Duration::from_secs(2);
const BUNDLED_EXTENSIONS: [&str; 1] = [include_str!("../extensions/ExampleSite.js")];

pub fn run() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id("io.github.altay301.MangaReader").build();
    app.connect_activate(build);
    app.run_with_args(&["manga-reader"])
}

/// Runs `work` on a low-priority thread and delivers the result on the main loop.
pub fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        crate::loader::lower_priority();
        let _ = tx.send_blocking(work());
    });
    glib::spawn_future_local(async move {
        if let Ok(value) = rx.recv().await {
            done(value);
        }
    });
}

/// Local folders, bundled extensions and user extensions (which override bundled ones by id).
fn load_sources() -> Vec<Arc<dyn Source>> {
    let root = std::env::args_os().nth(1).map(std::path::PathBuf::from).unwrap_or_else(paths::manga_root);
    let mut sources: Vec<Arc<dyn Source>> = vec![Arc::new(LocalSource::new(root))];
    let mut codes: Vec<String> = BUNDLED_EXTENSIONS.iter().map(|s| s.to_string()).collect();
    if let Ok(rd) = std::fs::read_dir(paths::extensions_dir()) {
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "js")).collect();
        files.sort();
        codes.extend(files.into_iter().filter_map(|p| std::fs::read_to_string(p).ok()));
    }
    let mut js: Vec<JsSource> = Vec::new();
    for code in codes {
        match JsSource::from_code(code) {
            Ok(s) => {
                js.retain(|o| o.meta().id != s.meta().id);
                js.push(s);
            }
            Err(e) => eprintln!("[app] extension skipped: {e}"),
        }
    }
    sources.extend(js.into_iter().map(|s| Arc::new(s) as Arc<dyn Source>));
    sources
}

struct App {
    window: gtk::ApplicationWindow,
    gtk_app: gtk::Application,
    pages: gtk::Stack,
    content: gtk::Stack,
    status: gtk::Label,
    browse: Rc<BrowseView>,
    details: Rc<DetailsView>,
    reader: Rc<Reader>,
    db: Arc<Db>,
    sources: Vec<Arc<dyn Source>>,
    inhibit_cookie: Cell<u32>,
}

fn build(app: &gtk::Application) {
    load_css();
    if let Some(settings) = gtk::Settings::default() {
        // Triple tap: three touches within 400 ms and 40 px count as one sequence.
        settings.set_gtk_double_click_time(400);
        settings.set_gtk_double_click_distance(40);
    }
    let window = gtk::ApplicationWindow::builder().application(app).title("Manga Reader").default_width(1100).default_height(800).css_classes(["retro-window"]).build();
    window.maximize();
    let loading = gtk::Label::builder().label("Starting…").vexpand(true).hexpand(true).build();
    window.set_child(Some(&loading));
    window.present();

    std::thread::spawn(paths::trim_cache);

    let app = app.clone();
    let db = match Db::open(&paths::db_path()) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            loading.set_text(&format!("Cannot open the database: {e}"));
            return;
        }
    };
    background(load_sources, move |sources| finish_build(app, window, db, sources));
}

fn finish_build(gtk_app: gtk::Application, window: gtk::ApplicationWindow, db: Arc<Db>, sources: Vec<Arc<dyn Source>>) {
    let covers = Covers::new();
    let browse = BrowseView::new(covers.clone(), sources.clone());
    let details = DetailsView::new(covers, db.clone());

    let stats_label = gtk::Label::builder().css_classes(["frame-stats"]).halign(gtk::Align::Start).valign(gtk::Align::Start).can_target(false).visible(stats::enabled()).build();
    let reader = Reader::new(db.clone(), stats::enabled().then_some(&stats_label));

    // ---- home page: sidebar | (grid | details) + status bar ------------------
    let content = gtk::Stack::builder().vexpand(true).hexpand(true).build();
    content.add_named(&browse.root, Some("grid"));
    content.add_named(&details.root, Some("details"));
    let status = gtk::Label::builder().xalign(0.0).css_classes(["retro-status"]).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let main_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["retro-main"]).build();
    main_col.append(&content);
    main_col.append(&status);

    let sidebar = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).css_classes(["retro-sidebar"]).build();
    sidebar.append(&gtk::Label::builder().label("Places").xalign(0.0).css_classes(["retro-heading"]).build());
    let library_btn = gtk::ToggleButton::builder().label("Library").css_classes(["retro-place"]).active(true).build();
    sidebar.append(&library_btn);
    sidebar.append(&gtk::Label::builder().label("Sources").xalign(0.0).css_classes(["retro-heading"]).build());
    let mut source_btns = Vec::new();
    for s in &sources {
        let b = gtk::ToggleButton::builder().label(s.name()).css_classes(["retro-place"]).group(&library_btn).build();
        sidebar.append(&b);
        source_btns.push((b, s.clone()));
    }
    let home = gtk::Box::builder().spacing(8).css_classes(["retro-home"]).build();
    home.append(&sidebar);
    home.append(&main_col);

    let pages = gtk::Stack::new();
    pages.add_named(&home, Some("home"));
    pages.add_named(&reader.root, Some("reader"));
    window.set_child(Some(&pages));

    let app = Rc::new(App {
        window: window.clone(),
        gtk_app: gtk_app.clone(),
        pages,
        content,
        status,
        browse: browse.clone(),
        details: details.clone(),
        reader: reader.clone(),
        db: db.clone(),
        sources,
        inhibit_cookie: Cell::new(0),
    });

    // ---- wiring ----------------------------------------------------------------
    let say = {
        let w = Rc::downgrade(&app);
        move |m: &str| {
            if let Some(a) = w.upgrade() {
                a.status.set_text(m)
            }
        }
    };
    browse.set_on_status(say.clone());
    details.set_on_status(say.clone());
    reader.set_status_sink(say);

    library_btn.connect_toggled({
        let a = app.clone();
        move |b| {
            if b.is_active() {
                a.show_library()
            }
        }
    });
    let source_buttons: Vec<(gtk::ToggleButton, Arc<dyn Source>)> = source_btns.clone();
    for (b, s) in source_btns {
        let a = app.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                a.content.set_visible_child_name("grid");
                a.browse.show_source(s.clone());
            }
        });
    }
    browse.set_on_open({
        let a = app.clone();
        move |source, manga| a.open_details(source, manga)
    });
    details.set_on_back({
        let a = app.clone();
        move || a.content.set_visible_child_name("grid")
    });
    details.set_on_read({
        let a = app.clone();
        move |source, manga, chapters, index, start| a.start_reading(source, manga, chapters, index, start)
    });
    reader.set_on_exit({
        let a = app.clone();
        move || a.leave_reader()
    });

    // Esc toggles the reader's control panel.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let a = app.clone();
        move |_, key, _, _| {
            if a.in_reader() && a.reader.handle_key(key) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        }
    });
    window.add_controller(keys);

    install_cursor_hiding(&window, {
        let a = Rc::downgrade(&app);
        move || a.upgrade().is_some_and(|a| a.in_reader())
    });
    let frame_stats = stats::enabled().then(|| stats::install(&window, &stats_label));
    window.connect_close_request(move |_| {
        if let Some(s) = &frame_stats {
            eprintln!("[stats] {}", s.summary());
        }
        glib::Propagation::Proceed
    });

    app.show_library();
    if let Ok(spec) = std::env::var("MANGA_OPEN") {
        app.auto_open(&spec);
    }
    if let Ok(view) = std::env::var("MANGA_VIEW") {
        app.auto_view(&view, &source_buttons);
    }
}

impl App {
    fn in_reader(&self) -> bool {
        self.pages.visible_child_name().as_deref() == Some("reader")
    }

    fn show_library(&self) {
        self.content.set_visible_child_name("grid");
        self.browse.show_library(self.db.clone());
    }

    fn open_details(&self, source: Arc<dyn Source>, manga: MangaSummary) {
        self.content.set_visible_child_name("details");
        self.details.open(source, manga);
    }

    fn start_reading(&self, source: Arc<dyn Source>, manga: MangaSummary, chapters: Vec<ChapterInfo>, index: usize, start: f64) {
        self.pages.set_visible_child_name("reader");
        if std::env::var_os("MANGA_WINDOWED").is_none() {
            self.window.fullscreen();
        }
        self.inhibit_cookie.set(self.gtk_app.inhibit(Some(&self.window), gtk::ApplicationInhibitFlags::IDLE, Some("Reading")));
        self.reader.open(source, manga.id, manga.title, chapters, index, start);
        self.reader.strip().grab_focus();
    }

    fn leave_reader(self: &Rc<Self>) {
        if self.inhibit_cookie.replace(0) != 0 {
            // Screen blanking is allowed again.
            self.gtk_app.uninhibit(self.inhibit_cookie.get());
        }
        self.window.unfullscreen();
        self.window.set_cursor(None);
        self.pages.set_visible_child_name("home");
        self.details.refresh_progress();
    }

    /// Dev/test hook: `MANGA_VIEW=source:<id>` or `details:<source id>:<manga id>`.
    fn auto_view(self: &Rc<Self>, spec: &str, buttons: &[(gtk::ToggleButton, Arc<dyn Source>)]) {
        let mut it = spec.splitn(3, ':');
        let (kind, sid) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
        let Some((btn, source)) = buttons.iter().find(|(_, s)| s.id() == sid) else { return };
        btn.set_active(true);
        if kind == "details" {
            let id = it.next().unwrap_or_default().to_string();
            self.open_details(source.clone(), MangaSummary { title: id.clone(), id, cover: None });
        }
    }

    /// Dev/test hook: `MANGA_OPEN=<source id>:<manga id>[:<chapter index>]` opens a chapter at start-up.
    fn auto_open(self: &Rc<Self>, spec: &str) {
        let mut it = spec.splitn(3, ':');
        let (Some(sid), Some(mid)) = (it.next(), it.next()) else { return };
        let index: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let Some(source) = self.sources.iter().find(|s| s.id() == sid).cloned() else {
            eprintln!("[app] MANGA_OPEN: unknown source '{sid}'");
            return;
        };
        let (mid, this, src) = (mid.to_string(), self.clone(), source.clone());
        let id = mid.clone();
        background(
            move || src.chapters(&id).map(|mut c| {
                c.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));
                c
            }),
            move |r| match r {
                Ok(ch) if index < ch.len() => {
                    let title = mid.clone();
                    this.start_reading(source, MangaSummary { id: mid, title, cover: None }, ch, index, 0.0);
                    if std::env::var_os("MANGA_AUTOSCROLL").is_some() {
                        let strip = this.reader.strip().clone();
                        let window = this.window.clone();
                        // Start once the first pixels are laid out.
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || start_autotest(&strip, &window));
                    }
                }
                Ok(_) => eprintln!("[app] MANGA_OPEN: chapter index out of range"),
                Err(e) => eprintln!("[app] MANGA_OPEN: {e}"),
            },
        );
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("theme.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// Hides the pointer after 2 s without movement while reading. One timer runs
/// only while the cursor is visible, so an idle app has no timers at all.
fn install_cursor_hiding(window: &gtk::ApplicationWindow, reading: impl Fn() -> bool + 'static) {
    let reading = Rc::new(reading);
    let last_move = Rc::new(Cell::new(std::time::Instant::now()));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    motion.connect_motion({
        let window = window.clone();
        move |_, _, _| {
            last_move.set(std::time::Instant::now());
            window.set_cursor(None);
            if timer.borrow().is_some() || !reading() {
                return;
            }
            let (window, last_move, slot, reading) = (window.clone(), last_move.clone(), timer.clone(), reading.clone());
            let id = glib::timeout_add_local(Duration::from_millis(500), move || {
                if !reading() {
                    slot.borrow_mut().take();
                    return glib::ControlFlow::Break;
                }
                if last_move.get().elapsed() >= CURSOR_HIDE_AFTER {
                    window.set_cursor_from_name(Some("none"));
                    slot.borrow_mut().take();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            *timer.borrow_mut() = Some(id);
        }
    });
    window.add_controller(motion);
}

/// Hands-free test mode for measuring frame times without touching the screen:
/// `MANGA_AUTOSCROLL=<px/s>` scrolls up and down, `MANGA_AUTOZOOM=1` also zooms
/// 1x <-> 2x every few seconds, `MANGA_AUTOSCROLL_SECS=<n>` quits after n seconds.
/// Needs `MANGA_OPEN=<source>:<manga>` to have something to scroll.
fn start_autotest(strip: &crate::strip::StripView, window: &gtk::ApplicationWindow) {
    let speed: f64 = std::env::var("MANGA_AUTOSCROLL").ok().and_then(|v| v.parse().ok()).unwrap_or(1200.0);
    let secs: f64 = std::env::var("MANGA_AUTOSCROLL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let zooming = std::env::var_os("MANGA_AUTOZOOM").is_some();
    let started = Cell::new(0i64);
    let last = Cell::new(0i64);
    let direction = Cell::new(1.0f64);
    let settled = Cell::new(true);
    let window = window.clone();
    eprintln!("[autotest] scrolling at {speed} px/s, zoom test: {zooming}, duration: {secs} s");
    strip.add_tick_callback(move |strip, clock| {
        let now = clock.frame_time();
        if started.get() == 0 {
            started.set(now);
            last.set(now);
        }
        let dt = ((now - last.replace(now)) as f64 / 1e6).min(0.05);
        let elapsed = (now - started.get()) as f64 / 1e6;
        if secs > 0.0 && elapsed > secs {
            window.close();
            return glib::ControlFlow::Break;
        }
        if let Some(a) = strip.vadjustment() {
            let max = a.upper() - a.page_size();
            let v = a.value() + direction.get() * speed * dt;
            if v >= max || v <= 0.0 {
                direction.set(-direction.get());
            }
            a.set_value(v.clamp(0.0, max.max(0.0)));
        }
        if zooming {
            // 8 s cycle: 1x, ramp to 2x, hold, ramp back.
            let phase = elapsed % 8.0;
            let target = match phase {
                p if p < 2.0 => 1.0,
                p if p < 3.0 => 1.0 + (p - 2.0),
                p if p < 5.0 => 2.0,
                p if p < 6.0 => 2.0 - (p - 5.0),
                _ => 1.0,
            };
            let (w, h) = (strip.width() as f64, strip.height() as f64);
            let ramping = (2.0..3.0).contains(&phase) || (5.0..6.0).contains(&phase);
            if ramping || (strip.zoom() - target).abs() > 1e-3 {
                strip.zoom_to(target, w / 2.0, h / 2.0);
                settled.set(false);
            } else if !settled.replace(true) {
                strip.zoom_settled();
            }
        }
        glib::ControlFlow::Continue
    });
}
