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
use crate::downloads::{self, Downloads};
use crate::ext::JsSource;
use crate::local::LocalSource;
use crate::reader::Reader;
use crate::nyaa::power;
use crate::nyaa::service::{Event as NyaaEvent, Service as NyaaService};
use crate::nyaa_ui::{self, NyaaPage};
use crate::player::{self, PlayRequest, PlayerEvent};
use crate::source::{ChapterInfo, MangaSummary, Source, SourceKind};
use crate::{paths, stats};

pub const APP_ID: &str = "io.github.altay301.Nezuyomi";
const CURSOR_HIDE_AFTER: Duration = Duration::from_secs(2);
const BUNDLED_EXTENSIONS: [&str; 2] = [include_str!("../extensions/ExampleSite.js"), include_str!("../extensions/anime.example")];

pub fn run() -> glib::ExitCode {
    // MANGA_NON_UNIQUE=1 lets a test run start next to a running instance instead of
    // being forwarded to it.
    let flags = if std::env::var_os("MANGA_NON_UNIQUE").is_some() { gtk::gio::ApplicationFlags::NON_UNIQUE } else { gtk::gio::ApplicationFlags::empty() };
    let app = gtk::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_activate(build);
    app.run_with_args(&["nezuyomi"])
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
fn load_sources(db: Arc<Db>) -> Vec<Arc<dyn Source>> {
    let root = std::env::args_os().nth(1).map(std::path::PathBuf::from).unwrap_or_else(paths::manga_root);
    // "Nyaa" (downloaded volumes) is its own source and tab; it is not a series of "Local".
    let nyaa_root = crate::nyaa::sync::Settings::load(&db).root;
    let hidden: Vec<String> = nyaa_root.parent().filter(|p| *p == root).and_then(|_| nyaa_root.file_name()).map(|n| vec![n.to_string_lossy().into_owned()]).unwrap_or_default();
    let hidden_refs: Vec<&str> = hidden.iter().map(String::as_str).collect();
    let mut sources: Vec<Arc<dyn Source>> = vec![
        Arc::new(LocalSource::new("local", "Local", root, &hidden_refs)),
        Arc::new(LocalSource::new("nyaa", "Nyaa", nyaa_root, &[])),
    ];
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
    player_events: async_channel::Sender<PlayerEvent>,
    nyaa: Rc<NyaaPage>,
    nyaa_btn: gtk::ToggleButton,
    nyaa_service: Arc<NyaaService>,
    /// Page of the content stack the series page returns to ("grid" or "nyaa").
    back_to: RefCell<String>,
}

/// App icon for the window, the task switcher and the panel. Installed copies come from
/// the icon theme; a checkout run (`cargo run`) falls back to `data/icons` in the sources.
fn setup_icon() {
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        if !theme.has_icon(APP_ID) {
            eprintln!("[app] icon '{APP_ID}' not found in the icon theme");
        }
    }
    gtk::Window::set_default_icon_name(APP_ID);
}

fn build(app: &gtk::Application) {
    setup_icon();
    load_css();
    if let Some(settings) = gtk::Settings::default() {
        // Triple tap: three touches within 400 ms and 40 px count as one sequence.
        settings.set_gtk_double_click_time(400);
        settings.set_gtk_double_click_distance(40);
    }
    let window = gtk::ApplicationWindow::builder().application(app).title("Nezuyomi").default_width(1100).default_height(800).css_classes(["retro-window"]).build();
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
    let db2 = db.clone();
    background(move || load_sources(db2), move |sources| finish_build(app, window, db, sources));
}

fn finish_build(gtk_app: gtk::Application, window: gtk::ApplicationWindow, db: Arc<Db>, sources: Vec<Arc<dyn Source>>) {
    let covers = Covers::new();
    let browse = BrowseView::new(covers.clone(), sources.clone());
    let (pl_tx, pl_rx) = async_channel::unbounded::<PlayerEvent>();
    let (dl_tx, dl_rx) = async_channel::unbounded::<downloads::Event>();
    let dl = Downloads::new(db.clone(), dl_tx);
    let details = DetailsView::new(covers.clone(), db.clone(), dl.clone());
    let (nyaa_tx, nyaa_rx) = async_channel::unbounded::<NyaaEvent>();
    let nyaa_service = NyaaService::new(db.clone(), nyaa_tx);
    let interval_min = Rc::new(Cell::new(60u32));
    let nyaa_page = NyaaPage::new(covers, sources.clone(), db.clone(), nyaa_service.clone(), interval_min.clone());

    let stats_label = gtk::Label::builder().css_classes(["frame-stats"]).halign(gtk::Align::Start).valign(gtk::Align::Start).can_target(false).visible(stats::enabled()).build();
    let reader = Reader::new(db.clone(), stats::enabled().then_some(&stats_label));

    // ---- home page: sidebar | (grid | details) + status bar ------------------
    let content = gtk::Stack::builder().vexpand(true).hexpand(true).build();
    content.add_named(&browse.root, Some("grid"));
    content.add_named(&details.root, Some("details"));
    content.add_named(&nyaa_page.root, Some("nyaa"));
    let status = gtk::Label::builder().xalign(0.0).css_classes(["retro-status"]).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let main_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["retro-main"]).build();
    main_col.append(&content);
    main_col.append(&status);

    let sidebar = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).css_classes(["retro-sidebar"]).build();
    sidebar.append(&gtk::Label::builder().label("Places").xalign(0.0).css_classes(["retro-heading"]).build());
    let library_btn = gtk::ToggleButton::builder().label("Library").css_classes(["retro-place"]).active(true).build();
    sidebar.append(&library_btn);
    let downloads_btn = gtk::ToggleButton::builder().label("Downloads").css_classes(["retro-place"]).group(&library_btn).build();
    sidebar.append(&downloads_btn);
    let nyaa_btn = gtk::ToggleButton::builder().label("Nyaa").css_classes(["retro-place"]).group(&library_btn).build();
    sidebar.append(&nyaa_btn);
    let mut source_btns = Vec::new();
    for (heading, kind) in [("Manga", SourceKind::Manga), ("Anime", SourceKind::Anime)] {
        // "Nyaa" has its own place above, not an entry among the manga sources.
        let of_kind: Vec<_> = sources.iter().filter(|s| s.kind() == kind && s.id() != "nyaa").collect();
        if of_kind.is_empty() {
            continue;
        }
        sidebar.append(&gtk::Label::builder().label(heading).xalign(0.0).css_classes(["retro-heading"]).build());
        for s in of_kind {
            let b = gtk::ToggleButton::builder().label(s.name()).css_classes(["retro-place"]).group(&library_btn).build();
            sidebar.append(&b);
            source_btns.push((b, s.clone()));
        }
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
        player_events: pl_tx,
        nyaa: nyaa_page.clone(),
        nyaa_btn: nyaa_btn.clone(),
        nyaa_service: nyaa_service.clone(),
        back_to: RefCell::new("grid".into()),
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
    downloads_btn.connect_toggled({
        let a = app.clone();
        move |b| {
            if b.is_active() {
                a.content.set_visible_child_name("grid");
                a.browse.show_downloads(a.db.clone());
            }
        }
    });
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
        move || {
            let page = a.back_to.borrow().clone();
            a.content.set_visible_child_name(&page);
        }
    });
    nyaa_page.series.set_on_open({
        let a = app.clone();
        move |source, manga| a.open_details(source, manga)
    });
    nyaa_page.series.set_on_status({
        let a = app.clone();
        move |m: &str| a.status.set_text(m)
    });
    nyaa_btn.connect_toggled({
        let a = app.clone();
        move |b| {
            if b.is_active() {
                a.content.set_visible_child_name("nyaa");
                a.nyaa.show();
            }
        }
    });
    nyaa_page.set_on_link_release({
        let a = app.clone();
        move |release| a.link_release(release)
    });
    details.set_on_nyaa_link({
        let a = app.clone();
        move |source, manga| a.open_link_dialog(source, manga)
    });
    details.set_on_nyaa_volumes({
        let a = app.clone();
        move |folder| a.open_nyaa_series(folder)
    });
    details.set_on_read({
        let a = app.clone();
        move |source, manga, chapters, index, start| a.start_reading(source, manga, chapters, index, start)
    });
    reader.set_on_exit({
        let a = app.clone();
        move || a.leave_reader()
    });

    // Download progress: live row updates on the series page and a status line.
    glib::spawn_future_local({
        let w = Rc::downgrade(&app);
        async move {
            while let Ok(ev) = dl_rx.recv().await {
                let Some(a) = w.upgrade() else { break };
                a.details.on_download_event(&ev);
                match &ev {
                    downloads::Event::Progress { done, total, .. } => a.status.set_text(&format!("Downloading… {done}/{total} pages")),
                    downloads::Event::Finished { name, .. } => a.status.set_text(&format!("Downloaded {name}")),
                    downloads::Event::Failed { name, error, .. } => a.status.set_text(&format!("Download of {name} failed: {error}")),
                    downloads::Event::Removed { .. } => {}
                }
            }
        }
    });
    // mpv finished: refresh the episode list's progress and say what happened.
    glib::spawn_future_local({
        let w = Rc::downgrade(&app);
        async move {
            while let Ok(ev) = pl_rx.recv().await {
                let Some(a) = w.upgrade() else { break };
                match ev {
                    PlayerEvent::Finished { watched, .. } => {
                        a.status.set_text(if watched { "Episode watched" } else { "Playback stopped" });
                        a.details.refresh_progress();
                    }
                    PlayerEvent::Failed(e) => {
                        eprintln!("[player] {e}");
                        a.status.set_text(&format!("Cannot play: {e}"));
                    }
                }
            }
        }
    });
    // Pick up downloads a previous run left unfinished.
    background({
        let db = db.clone();
        move || db.unfinished_downloads()
    }, {
        let (dl, sources) = (dl.clone(), app.sources.clone());
        move |rows| dl.resume(rows, &sources)
    });

    // ---- Nyaa: events, timers, power policy ------------------------------------------------------
    let active = Rc::new(Cell::new(false));
    let power_paused = Rc::new(Cell::new(false));
    glib::spawn_future_local({
        let (w, active) = (Rc::downgrade(&app), active.clone());
        async move {
            while let Ok(ev) = nyaa_rx.recv().await {
                let Some(a) = w.upgrade() else { break };
                match ev {
                    NyaaEvent::Cycle(report) => {
                        a.nyaa.show_report(&report);
                        if report.queued > 0 || report.completed > 0 || report.new_releases > 0 {
                            a.status.set_text(&format!("Nyaa: {} new releases, {} queued, {} finished", report.new_releases, report.queued, report.completed));
                        }
                    }
                    NyaaEvent::Linked(Ok((link, n))) => {
                        a.status.set_text(&format!("Linked \"{}\" to {}: {n} matching releases found", link.title, link.uploader));
                        a.details.refresh_nyaa();
                        a.nyaa_service.run_cycle(false); // queue what is new, right away
                    }
                    NyaaEvent::Linked(Err(e)) => a.status.set_text(&format!("Cannot link: {e}")),
                    NyaaEvent::Queued { title, result } => a.status.set_text(&match result {
                        Ok(_) => format!("Nyaa: added {title}"),
                        Err(e) => format!("Nyaa: {title}: {e}"),
                    }),
                    NyaaEvent::Power { paused, result } => a.status.set_text(&match (paused, result) {
                        (true, Ok(n)) => format!("On battery in tablet mode: paused {n} Nyaa downloads"),
                        (false, Ok(n)) if n > 0 => format!("Resumed {n} Nyaa downloads"),
                        (_, Ok(_)) => continue, // nothing paused or resumed: keep the loop running
                        (_, Err(e)) => format!("Nyaa power policy: {e}"),
                    }),
                    NyaaEvent::Searched(r) => {
                        a.nyaa.clear_banner();
                        a.status.set_text(&match r {
                        Ok(0) => "Nyaa search: nothing new (see the list below)".to_string(),
                        Ok(n) => format!("Nyaa search: {n} new releases found"),
                        Err(e) => format!("Nyaa search: {e}"),
                        });
                    }
                    NyaaEvent::Qbit(r) => a.nyaa.show_connection_result(&r),
                    NyaaEvent::Changed => a.nyaa.refresh(),
                    NyaaEvent::Active(v) => active.set(v),
                }
            }
        }
    });
    // Feed check: first one shortly after start, then every `interval` minutes. Never while
    // paused by the power policy (battery + tablet mode): that also saves the wake-ups.
    fn schedule_cycle(svc: Arc<NyaaService>, interval: Rc<Cell<u32>>, paused: Rc<Cell<bool>>, delay_secs: u32) {
        glib::timeout_add_seconds_local_once(delay_secs, move || {
            if !paused.get() {
                svc.run_cycle(false);
            }
            let next = interval.get().max(15) * 60;
            schedule_cycle(svc, interval, paused, next);
        });
    }
    // The interval setting is read once in the background; the Nyaa page updates the cell when it is saved.
    background({
        let db = db.clone();
        move || crate::nyaa::sync::Settings::load(&db).interval_min
    }, {
        let (cell, svc, paused) = (interval_min.clone(), nyaa_service.clone(), power_paused.clone());
        move |minutes| {
            cell.set(minutes);
            schedule_cycle(svc, cell, paused, 30);
        }
    });
    // Progress: every 15 s, only while something of ours is downloading and not paused.
    glib::timeout_add_seconds_local(15, {
        let (svc, active, paused) = (nyaa_service.clone(), active.clone(), power_paused.clone());
        move || {
            if active.get() && !paused.get() {
                svc.poll();
            }
            glib::ControlFlow::Continue
        }
    });
    nyaa_service.poll(); // learn once whether something is already downloading
    power::watch({
        let (svc, paused) = (nyaa_service.clone(), power_paused.clone());
        move |p| {
            paused.set(p);
            svc.apply_power(p);
        }
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
    if let Ok(spec) = std::env::var("MANGA_SCREENSHOT") {
        schedule_screenshots(&window, &spec);
    }
    if let Ok(spec) = std::env::var("MANGA_OPEN") {
        app.auto_open(&spec);
    }
    if let Ok(spec) = std::env::var("MANGA_DOWNLOAD") {
        app.auto_download(&spec, dl.clone());
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
        if let Some(from) = self.content.visible_child_name().filter(|n| n != "details") {
            *self.back_to.borrow_mut() = from.to_string();
        }
        self.content.set_visible_child_name("details");
        self.details.open(source, manga);
    }

    fn start_reading(&self, source: Arc<dyn Source>, manga: MangaSummary, chapters: Vec<ChapterInfo>, index: usize, start: f64) {
        if source.kind() == SourceKind::Anime {
            return self.watch(source, manga, &chapters[index], start);
        }
        self.pages.set_visible_child_name("reader");
        if std::env::var_os("MANGA_WINDOWED").is_none() {
            self.window.fullscreen();
        }
        self.inhibit_cookie.set(self.gtk_app.inhibit(Some(&self.window), gtk::ApplicationInhibitFlags::IDLE, Some("Reading")));
        self.reader.open(source, manga.id, manga.title, chapters, index, start);
        self.reader.strip().grab_focus();
    }

    /// Anime: resolve the streams on a background thread, then hand the first one to mpv.
    /// "Link to Nyaa…" on a series page: choose an uploader and a phrase once.
    fn open_link_dialog(self: &Rc<Self>, _source: Arc<dyn Source>, manga: MangaSummary) {
        let (db, this) = (self.db.clone(), self.clone());
        let source_id = _source.id().to_string();
        background(
            move || {
                db.nyaa_seed_uploaders(&crate::nyaa::DEFAULT_UPLOADERS);
                db.nyaa_uploaders().into_iter().filter(|u| u.enabled).map(|u| u.name).collect::<Vec<_>>()
            },
            move |uploaders| {
                if uploaders.is_empty() {
                    this.status.set_text("No followed uploaders: add some in Nyaa → Settings");
                    return;
                }
                let (svc, title, id) = (this.nyaa_service.clone(), manga.title.clone(), manga.id.clone());
                nyaa_ui::show_link_dialog(this.window.upcast_ref(), uploaders, &manga.title, false, move |uploader, phrase, auto, colored| {
                    svc.link(source_id.clone(), id.clone(), title.clone(), uploader, phrase, auto, colored);
                });
            },
        );
    }

    /// "Link & download…" on a release from a search: links the Nyaa series itself (title from the
    /// release name) to that uploader, then the next cycle queues everything the link matches.
    fn link_release(self: &Rc<Self>, release: crate::nyaa::store::Release) {
        let (title, colored) = self.nyaa_service.series_info_of(&release.uploader, &release.title).unwrap_or_else(|| (release.title.clone(), false));
        let svc = self.nyaa_service.clone();
        let (uploader, shown) = (release.uploader.clone(), title.clone());
        nyaa_ui::show_link_dialog(self.window.upcast_ref(), vec![uploader], &shown, colored, move |uploader, phrase, auto, colored| {
            // The id of a series in the Nyaa place is its folder name; the colored edition's folder says so.
            let id = if colored { format!("{} (Colored)", crate::nyaa::sync::sanitize_folder(&title)) } else { crate::nyaa::sync::sanitize_folder(&title) };
            svc.link("nyaa".into(), id, title.clone(), uploader, phrase, auto, colored);
        });
    }

    /// "Volumes on disk": the downloaded series in the Nyaa place.
    fn open_nyaa_series(&self, folder: String) {
        if let Some(src) = self.sources.iter().find(|s| s.id() == "nyaa").cloned() {
            *self.back_to.borrow_mut() = self.content.visible_child_name().filter(|n| n != "details").map(|n| n.to_string()).unwrap_or_else(|| "grid".into());
            self.content.set_visible_child_name("details");
            self.details.open(src, MangaSummary { id: folder.clone(), title: folder, cover: None });
        }
    }

    fn watch(&self, source: Arc<dyn Source>, manga: MangaSummary, episode: &ChapterInfo, start: f64) {
        self.status.set_text(&format!("Starting {}…", episode.name));
        let (db, tx, ep) = (self.db.clone(), self.player_events.clone(), episode.clone());
        let status = self.status.clone();
        let sid = source.id().to_string();
        background(
            move || source.videos(&ep.id).map(|v| (v, ep)),
            move |r| match r {
                Ok((videos, ep)) => {
                    // Sub first (the extension lists it first); a dub-only episode falls back to the dub.
                    let Some(video) = videos.into_iter().next() else { return status.set_text("No stream found") };
                    status.set_text(&format!("Playing {} ({}) in mpv", ep.name, video.label));
                    player::play(db, PlayRequest { video, title: format!("{} — {}", manga.title, ep.name), source: sid, manga_id: manga.id, chapter_id: ep.id, start_fraction: start }, tx);
                }
                Err(e) => {
                    eprintln!("[watch] {e}");
                    status.set_text(&format!("Error: {e}"));
                }
            },
        );
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

    /// Dev/test hook: `MANGA_DOWNLOAD=<source id>:<manga id>:<chapter index>[:<count>]` queues chapters at start-up.
    fn auto_download(self: &Rc<Self>, spec: &str, dl: Arc<Downloads>) {
        let parts: Vec<&str> = spec.split(':').collect();
        let [sid, mid, idx, rest @ ..] = parts.as_slice() else { return };
        let (index, count): (usize, usize) = (idx.parse().unwrap_or(0), rest.first().and_then(|c| c.parse().ok()).unwrap_or(1));
        let Some(source) = self.sources.iter().find(|s| s.id() == *sid).cloned() else { return };
        let (mid, src) = (mid.to_string(), source.clone());
        background(
            move || {
                let d = src.manga(&mid)?;
                let mut ch = src.chapters(&mid)?;
                ch.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));
                Ok::<_, String>((mid, d, ch))
            },
            move |r| match r {
                Ok((mid, d, ch)) => {
                    for c in ch.into_iter().skip(index).take(count) {
                        let dir = downloads::chapter_dir(source.id(), &mid, &c.id);
                        dl.enqueue(source.clone(), crate::db::DownloadRow {
                            source: source.id().to_string(),
                            manga_id: mid.clone(),
                            manga_title: d.title.clone(),
                            cover: d.cover.clone(),
                            details: serde_json::to_string(&d).unwrap_or_default(),
                            chapter: c,
                            dir: dir.to_string_lossy().into_owned(),
                        });
                    }
                }
                Err(e) => eprintln!("[app] MANGA_DOWNLOAD: {e}"),
            },
        );
    }

    /// Dev/test hook: `MANGA_VIEW=source:<id>` or `details:<source id>:<manga id>`.
    fn auto_view(self: &Rc<Self>, spec: &str, buttons: &[(gtk::ToggleButton, Arc<dyn Source>)]) {
        let mut it = spec.splitn(3, ':');
        let (kind, sid) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
        if kind == "nyaa" {
            // MANGA_VIEW=nyaa:<series|releases|links|settings>
            self.nyaa_btn.set_active(true);
            // MANGA_VIEW=nyaa:releases:<phrase> also runs that search.
            match it.next() {
                Some(phrase) if sid == "releases" => self.nyaa.search_for(phrase),
                _ => self.nyaa.select_tab(if sid.is_empty() { "series" } else { sid }),
            }
            return;
        }
        let Some((btn, source)) = buttons.iter().find(|(_, s)| s.id() == sid) else { return };
        btn.set_active(true);
        if kind == "link" {
            // MANGA_VIEW=link:<source>:<manga id>: opens the "Link to Nyaa" window (test hook).
            let id = it.next().unwrap_or_default().to_string();
            self.open_link_dialog(source.clone(), MangaSummary { title: id.clone(), id, cover: None });
            return;
        }
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
        let (id, db) = (mid.clone(), self.db.clone());
        background(
            move || {
                let chapters = src.chapters(&id).or_else(|e| db.offline_series(src.id().to_string(), id.clone()).map(|(_, c)| c).ok_or(e));
                chapters.map(|mut c| {
                    c.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));
                    c
                })
            },
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

/// Dev hook: `MANGA_SCREENSHOT=<file.png>@<seconds>[,<file.png>@<seconds>…]` renders
/// this app's own window to PNG (never touches other windows on the desktop).
fn schedule_screenshots(window: &gtk::ApplicationWindow, spec: &str) {
    for item in spec.split(',') {
        let Some((path, secs)) = item.rsplit_once('@') else { continue };
        let (Ok(secs), path) = (secs.parse::<f64>(), path.to_string()) else { continue };
        let window = window.clone();
        glib::timeout_add_local_once(Duration::from_secs_f64(secs), move || {
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
            let renderer = gtk::prelude::NativeExt::renderer(&window);
            match (snapshot.to_node(), renderer) {
                (Some(node), Some(r)) => match r.render_texture(&node, None).save_to_png(&path) {
                    Ok(()) => eprintln!("[screenshot] {path}"),
                    Err(e) => eprintln!("[screenshot] {path}: {e}"),
                },
                _ => eprintln!("[screenshot] nothing to render"),
            }
        });
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
            // 12 s cycle: 1x, zoom in to 2x, back to 1x, zoom out to 0.5x, back to 1x.
            const KEYS: [(f64, f64); 9] = [
                (0.0, 1.0), (1.5, 1.0), (2.5, 2.0), (4.0, 2.0), (5.0, 1.0),
                (6.0, 1.0), (7.0, 0.5), (9.0, 0.5), (10.0, 1.0),
            ];
            let phase = elapsed % 12.0;
            let target = KEYS.windows(2).find(|k| phase < k[1].0).map_or(1.0, |k| {
                let f = (phase - k[0].0) / (k[1].0 - k[0].0);
                k[0].1 + (k[1].1 - k[0].1) * f
            });
            let (w, h) = (strip.width() as f64, strip.height() as f64);
            if (strip.zoom() - target).abs() > 1e-3 {
                strip.zoom_to(target, w / 2.0, h / 2.0);
                settled.set(false);
            } else if !settled.replace(true) {
                strip.zoom_settled();
            }
        }
        glib::ControlFlow::Continue
    });
}
