//! Application shell: window, full screen, control panel, input glue.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};

use crate::library::{self, PageInfo, Series};
use crate::stats;
use crate::strip::StripView;

const CURSOR_HIDE_AFTER: Duration = Duration::from_secs(2);

pub fn run() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("io.github.altay301.MangaReader")
        .build();
    app.connect_activate(build);
    app.run_with_args(&["manga-reader"])
}

/// Runs `work` on a low-priority thread and delivers the result on the main loop.
fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
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

fn series_dir() -> PathBuf {
    std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Manga/TestSeri")
    })
}

struct Reader {
    window: gtk::ApplicationWindow,
    strip: StripView,
    scrim: gtk::Box,
    title: gtk::Label,
    chapter: gtk::Label,
    progress: gtk::ProgressBar,
    prev: gtk::Button,
    next: gtk::Button,
    series: RefCell<Option<Series>>,
    current: Cell<usize>,
    autotest: Cell<bool>,
}

fn build(app: &gtk::Application) {
    load_css();
    if let Some(settings) = gtk::Settings::default() {
        // Triple tap: three touches within 400 ms and 40 px count as one sequence.
        settings.set_gtk_double_click_time(400);
        settings.set_gtk_double_click_distance(40);
    }

    let strip = StripView::new();
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&strip)
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::External)
        .build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&scrolled));

    let stats_label = gtk::Label::builder()
        .css_classes(["frame-stats"])
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .can_target(false)
        .visible(stats::enabled())
        .build();
    overlay.add_overlay(&stats_label);

    // ---- control panel ----------------------------------------------------
    let title = gtk::Label::builder().css_classes(["retro-title"]).halign(gtk::Align::Center).build();
    let titlebar = gtk::Box::builder().css_classes(["retro-titlebar"]).halign(gtk::Align::Fill).build();
    titlebar.set_halign(gtk::Align::Fill);
    title.set_hexpand(true);
    title.set_halign(gtk::Align::Center);
    titlebar.append(&title);

    let chapter = gtk::Label::builder().css_classes(["retro-text"]).xalign(0.0).build();
    let progress = gtk::ProgressBar::builder().css_classes(["retro"]).build();
    let prev = gtk::Button::builder().label("◀ Previous").css_classes(["retro"]).build();
    let next = gtk::Button::builder().label("Next ▶").css_classes(["retro"]).build();
    // Phase 0 has no library yet, so this simply closes the app.
    let quit = gtk::Button::builder().label("Quit").css_classes(["retro"]).build();
    let buttons = gtk::Box::builder().spacing(10).homogeneous(true).build();
    for b in [&prev, &next, &quit] {
        buttons.append(b);
    }
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).css_classes(["retro-body"]).build();
    body.append(&chapter);
    body.append(&progress);
    body.append(&buttons);
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["retro-panel"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    panel.append(&titlebar);
    panel.append(&body);
    let scrim = gtk::Box::builder().css_classes(["scrim"]).hexpand(true).vexpand(true).visible(false).build();
    scrim.append(&panel);
    overlay.add_overlay(&scrim);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Manga Reader")
        .default_width(1100)
        .default_height(800)
        .css_classes(["reader"])
        .child(&overlay)
        .build();

    let reader = Rc::new(Reader {
        window: window.clone(),
        strip: strip.clone(),
        scrim: scrim.clone(),
        title,
        chapter,
        progress,
        prev: prev.clone(),
        next: next.clone(),
        series: RefCell::new(None),
        current: Cell::new(0),
        autotest: Cell::new(std::env::var_os("MANGA_AUTOSCROLL").is_some()),
    });

    // ---- panel behaviour ---------------------------------------------------
    strip.connect_panel_requested({
        let r = reader.clone();
        move |_| r.show_panel()
    });
    prev.connect_clicked({
        let r = reader.clone();
        move |_| r.step_chapter(-1)
    });
    next.connect_clicked({
        let r = reader.clone();
        move |_| r.step_chapter(1)
    });
    quit.connect_clicked({
        let w = window.clone();
        move |_| w.close()
    });
    // Tapping outside the panel closes it and reading continues.
    let outside = gtk::GestureClick::new();
    outside.connect_pressed({
        let r = reader.clone();
        let scrim = scrim.clone();
        move |_, _, x, y| {
            if scrim.pick(x, y, gtk::PickFlags::DEFAULT).is_some_and(|w| w == scrim.clone().upcast::<gtk::Widget>()) {
                r.hide_panel();
            }
        }
    });
    scrim.add_controller(outside);

    // Esc toggles the panel.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let r = reader.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if r.scrim.is_visible() { r.hide_panel() } else { r.show_panel() }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    window.add_controller(keys);

    // ---- cursor hiding -----------------------------------------------------
    install_cursor_hiding(&window);

    // ---- stats ---------------------------------------------------------------
    let frame_stats = stats::enabled().then(|| stats::install(&window, &stats_label));
    window.connect_close_request(move |_| {
        if let Some(s) = &frame_stats {
            eprintln!("[stats] {}", s.summary());
        }
        glib::Propagation::Proceed
    });

    if std::env::var_os("MANGA_WINDOWED").is_none() {
        window.fullscreen();
    }
    window.present();
    // Keep the screen from blanking while reading.
    app.inhibit(Some(&window), gtk::ApplicationInhibitFlags::IDLE, Some("Reading"));
    strip.grab_focus();

    // ---- load the series in the background ----------------------------------
    let dir = series_dir();
    background(move || library::scan_series(&dir), {
        let r = reader.clone();
        move |series: Series| {
            if series.chapters.is_empty() {
                eprintln!("[app] no chapters found; run tools/gen_test_strips.py or pass a folder");
                r.title.set_text("No chapters found");
                r.chapter.set_text("Put images in ~/Manga/TestSeri or pass a folder as argument.");
                r.show_panel();
                return;
            }
            *r.series.borrow_mut() = Some(series);
            r.open_chapter(0);
        }
    });
}

impl Reader {
    fn open_chapter(self: &Rc<Self>, index: usize) {
        let Some((chapter, series_name, count)) = self.series.borrow().as_ref().and_then(|s| {
            s.chapters.get(index).map(|c| (c.clone(), s.name.clone(), s.chapters.len()))
        }) else {
            return;
        };
        self.current.set(index);
        self.title.set_text(&format!("{} · {}", series_name, chapter.name));
        self.chapter.set_text(&format!("Chapter {} of {}", index + 1, count));
        self.prev.set_sensitive(index > 0);
        self.next.set_sensitive(index + 1 < count);
        let this = self.clone();
        background(move || library::read_pages(&chapter.dir), move |pages: Vec<PageInfo>| {
            this.strip.set_pages(pages);
            if std::env::var_os("MANGA_OPEN_PANEL").is_some() {
                this.show_panel();
            }
            if this.autotest.replace(false) {
                start_autotest(&this.strip, &this.window);
            }
        });
    }

    fn step_chapter(self: &Rc<Self>, delta: isize) {
        let next = self.current.get() as isize + delta;
        let count = self.series.borrow().as_ref().map_or(0, |s| s.chapters.len()) as isize;
        if (0..count).contains(&next) {
            self.hide_panel();
            self.open_chapter(next as usize);
        }
    }

    fn show_panel(&self) {
        self.progress.set_fraction(self.strip.progress());
        self.scrim.set_visible(true);
        self.prev.grab_focus();
    }

    fn hide_panel(&self) {
        self.scrim.set_visible(false);
        self.strip.grab_focus();
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("theme.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// Hides the pointer after 2 s without movement. One timer runs only while the
/// cursor is visible, so an idle app has no timers at all.
fn install_cursor_hiding(window: &gtk::ApplicationWindow) {
    let last_move = Rc::new(Cell::new(std::time::Instant::now()));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    motion.connect_motion({
        let window = window.clone();
        move |_, _, _| {
            last_move.set(std::time::Instant::now());
            window.set_cursor(None);
            if timer.borrow().is_some() {
                return;
            }
            let (window, last_move, slot) = (window.clone(), last_move.clone(), timer.clone());
            let id = glib::timeout_add_local(Duration::from_millis(500), move || {
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
fn start_autotest(strip: &StripView, window: &gtk::ApplicationWindow) {
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
