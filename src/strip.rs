//! `StripView`: the custom reader widget (see PLAN.md → Smoothness architecture).
//!
//! * Implements `GtkScrollable`, so a `GtkScrolledWindow` provides kinetic touch
//!   scrolling. Pen/mouse drags are handled here with our own fling curve.
//! * No widget per image: every page is split into ~1500 px tiles whose
//!   positions are computed up front from the image headers, so nothing moves
//!   when a tile arrives. `snapshot()` only appends textures that are visible.
//! * Decoding happens on worker threads (`loader`); the main thread only wraps
//!   the finished pixels in a `gdk::MemoryTexture`.
//! * Pinch zoom is a pure transform while the gesture runs. When it ends, the
//!   visible tiles are re-rendered at 2x resolution in the background.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::OnceLock;
use std::time::Instant;

use gtk::glib::subclass::Signal;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, prelude::*};

use crate::library::PageInfo;
use crate::loader::{Job, Loader, TileJob, TileResult};
use crate::stats;

/// Default tile height in device pixels at quality 1 (override with `MANGA_TILE_PX`).
/// Measured on the target laptop: with 1500 px tiles the first draw of a 2x
/// (zoomed) tile stalled the GPU for up to ~180 ms; 500 px tiles keep the worst
/// stall around 50 ms and a normal tile at ~3 MB.
const DEFAULT_TILE_PX: f64 = 500.0;

fn tile_px() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("MANGA_TILE_PX").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TILE_PX))
}
/// Tiles are requested this many screens ahead of / behind the viewport...
const REQUEST_SCREENS: f64 = 3.0;
/// ...and released again beyond this many (hysteresis avoids thrashing).
const KEEP_SCREENS: f64 = 5.0;
/// High-resolution (zoomed) tiles only within this distance.
const NEAR_SCREENS: f64 = 1.0;
const MAX_ZOOM: f64 = 3.0;
const WORKER_THREADS: usize = 2;

#[derive(Default)]
struct Layout {
    /// Logical width the layout was built for, and the surface scale.
    width: f64,
    scale: f64,
    tiles: Vec<TileInfo>,
    total_height: f64,
}

struct TileInfo {
    page: usize,
    /// Position and size in content coordinates (logical px, zoom 1).
    y: f64,
    h: f64,
    /// Matching rows in the source image.
    src_top: f64,
    src_h: f64,
}

impl Layout {
    fn build(pages: &[PageInfo], width: f64, scale: f64) -> Self {
        let snap = |v: f64| (v * scale).round() / scale;
        let tile_h = tile_px() / scale;
        let mut tiles = Vec::new();
        let mut y = 0.0;
        for (page, info) in pages.iter().enumerate() {
            let page_h = snap(info.height as f64 * width / info.width as f64).max(1.0 / scale);
            let n = (page_h / tile_h).ceil().max(1.0) as usize;
            for i in 0..n {
                // Equal split, boundaries on whole device pixels (no seams).
                let (a, b) = (snap(page_h * i as f64 / n as f64), snap(page_h * (i + 1) as f64 / n as f64));
                tiles.push(TileInfo {
                    page,
                    y: y + a,
                    h: b - a,
                    src_top: a / page_h * info.height as f64,
                    src_h: (b - a) / page_h * info.height as f64,
                });
            }
            y += page_h;
        }
        Self { width, scale, tiles, total_height: y }
    }

    /// Index range of tiles overlapping `[lo, hi)` in content coordinates.
    fn range(&self, lo: f64, hi: f64) -> (usize, usize) {
        let a = self.tiles.partition_point(|t| t.y + t.h <= lo);
        let b = self.tiles.partition_point(|t| t.y < hi);
        (a, b.max(a))
    }
}

struct TileTex {
    texture: gdk::Texture,
    /// Resolution multiplier the texture was rendered at (1 or 2).
    q: u8,
}

#[derive(Default)]
struct State {
    pages: Vec<PageInfo>,
    layout: Layout,
    tex: Vec<Option<TileTex>>,
    failed: HashSet<usize>,
    generation: u64,
    /// (request range, near range, wanted quality) of the last refresh.
    last_key: Option<((usize, usize), (usize, usize), u8)>,
    keep: (usize, usize),
}

mod imp {
    use super::*;

    #[derive(glib::Properties)]
    #[properties(wrapper_type = super::StripView)]
    pub struct StripView {
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_hadjustment, explicit_notify, nullable)]
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vadjustment, explicit_notify, nullable)]
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_hscroll_policy, explicit_notify)]
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vscroll_policy, explicit_notify)]
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,

        pub(super) state: RefCell<State>,
        pub loader: RefCell<Option<Loader>>,
        pub zoom: Cell<f64>,
        pub want_q: Cell<u8>,

        // Pen / mouse dragging and our own fling.
        pub drag_start: Cell<(f64, f64)>,
        pub drag_samples: RefCell<VecDeque<(Instant, f64, f64)>>,
        pub drag_claimed: Cell<bool>,
        pub fling: Cell<(f64, f64)>,
        pub fling_tick: RefCell<Option<gtk::TickCallbackId>>,
        pub zoom_start: Cell<f64>,
    }

    impl Default for StripView {
        fn default() -> Self {
            Self {
                hadjustment: Default::default(),
                vadjustment: Default::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                state: Default::default(),
                loader: Default::default(),
                zoom: Cell::new(1.0),
                want_q: Cell::new(1),
                drag_start: Default::default(),
                drag_samples: Default::default(),
                drag_claimed: Default::default(),
                fling: Default::default(),
                fling_tick: Default::default(),
                zoom_start: Cell::new(1.0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StripView {
        const NAME: &'static str = "MangaStripView";
        type Type = super::StripView;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("stripview");
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for StripView {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("panel-requested").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.set_focusable(true);

            let (tx, rx) = async_channel::unbounded::<TileResult>();
            *self.loader.borrow_mut() = Some(Loader::new(tx, WORKER_THREADS));
            let weak = obj.downgrade();
            glib::spawn_future_local(async move {
                while let Ok(result) = rx.recv().await {
                    let Some(view) = weak.upgrade() else { break };
                    view.imp().on_tile(result);
                }
            });

            obj.setup_gestures();
        }
    }

    impl WidgetImpl for StripView {
        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let scale = self.surface_scale();
            let rebuild = {
                let st = self.state.borrow();
                (st.layout.width - width as f64).abs() > 0.5 || (st.layout.scale - scale).abs() > 1e-3
            };
            if width > 0 && height > 0 && rebuild {
                self.relayout(true);
            } else {
                self.configure_adjustments(None);
            }
            self.refresh();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let started = Instant::now();
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            let z = self.zoom.get();
            let (hv, vv) = self.offsets();
            let scale = self.surface_scale();
            let st = self.state.borrow();
            let top = vv / z;
            let bot = (vv + h as f64) / z;
            let (first, last) = st.layout.range(top, bot);

            // Background (also what unloaded tiles look like).
            snapshot.append_color(&gdk::RGBA::BLACK, &graphene::Rect::new(0.0, 0.0, w, h));

            // Visible tiles. Edges are rounded to device pixels so that
            //    neighbouring tiles never leave a seam.
            let px = |v: f64| ((v * scale).round() / scale) as f32;
            let x0 = px(-hv);
            let x1 = px(st.layout.width * z - hv);
            for i in first..last {
                let Some(tex) = &st.tex[i] else { continue };
                let t = &st.layout.tiles[i];
                let (y0, y1) = (px(t.y * z - vv), px((t.y + t.h) * z - vv));
                snapshot.append_scaled_texture(
                    &tex.texture,
                    gsk::ScalingFilter::Linear,
                    &graphene::Rect::new(x0, y0, x1 - x0, y1 - y0),
                );
            }
            drop(st);
            stats::note_snapshot(started.elapsed().as_micros() as u64);
        }
    }

    impl ScrollableImpl for StripView {}

    impl StripView {
        fn set_hadjustment(&self, adj: Option<gtk::Adjustment>) {
            self.hadjustment.replace(adj.clone());
            if let Some(adj) = adj {
                let weak = self.obj().downgrade();
                adj.connect_value_changed(move |_| {
                    if let Some(v) = weak.upgrade() {
                        v.queue_draw();
                    }
                });
            }
            self.configure_adjustments(None);
            self.obj().notify("hadjustment");
        }

        fn set_vadjustment(&self, adj: Option<gtk::Adjustment>) {
            self.vadjustment.replace(adj.clone());
            if let Some(adj) = adj {
                let weak = self.obj().downgrade();
                adj.connect_value_changed(move |_| {
                    if let Some(v) = weak.upgrade() {
                        v.queue_draw();
                        v.imp().refresh();
                    }
                });
            }
            self.configure_adjustments(None);
            self.obj().notify("vadjustment");
        }

        fn set_hscroll_policy(&self, p: gtk::ScrollablePolicy) {
            self.hscroll_policy.set(p);
            self.obj().notify("hscroll-policy");
        }

        fn set_vscroll_policy(&self, p: gtk::ScrollablePolicy) {
            self.vscroll_policy.set(p);
            self.obj().notify("vscroll-policy");
        }

        pub fn surface_scale(&self) -> f64 {
            let obj = self.obj();
            obj.native()
                .and_then(|n| n.surface())
                .map(|s| s.scale())
                .unwrap_or_else(|| obj.scale_factor() as f64)
                .max(1.0)
        }

        pub fn offsets(&self) -> (f64, f64) {
            let get = |a: &RefCell<Option<gtk::Adjustment>>| a.borrow().as_ref().map_or(0.0, |a| a.value());
            (get(&self.hadjustment), get(&self.vadjustment))
        }

        /// Sets both adjustments from the layout and zoom. `values` overrides the
        /// scroll position (used by zoom and relayout). Never call this while
        /// `state` is borrowed: changing a value re-enters `refresh`.
        pub fn configure_adjustments(&self, values: Option<(f64, f64)>) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f64, obj.height() as f64);
            let z = self.zoom.get();
            let total = self.state.borrow().layout.total_height;
            let (hv, vv) = values.unwrap_or_else(|| self.offsets());
            if let Some(a) = self.hadjustment.borrow().as_ref() {
                a.configure(hv, 0.0, (w * z).max(w), w * 0.1, w * 0.9, w);
            }
            if let Some(a) = self.vadjustment.borrow().as_ref() {
                a.configure(vv, 0.0, (total * z).max(h), h * 0.1, h * 0.9, h);
            }
        }

        /// Rebuilds tile positions after pages, width or scale changed.
        pub fn relayout(&self, keep_position: bool) {
            let obj = self.obj();
            let width = obj.width() as f64;
            let scale = self.surface_scale();
            let z = self.zoom.get();
            let ratio = match (keep_position, self.vadjustment.borrow().as_ref()) {
                (true, Some(a)) if a.upper() > 0.0 => a.value() / a.upper(),
                _ => 0.0,
            };
            let hv = self.offsets().0;
            let total = {
                let mut st = self.state.borrow_mut();
                st.layout = Layout::build(&st.pages.clone(), width.max(1.0), scale);
                st.generation += 1;
                st.tex = (0..st.layout.tiles.len()).map(|_| None).collect();
                st.failed.clear();
                st.last_key = None;
                st.keep = (0, 0);
                if let Some(l) = self.loader.borrow().as_ref() {
                    l.reset(st.generation);
                }
                st.layout.total_height
            };
            self.configure_adjustments(Some((hv, ratio * total * z)));
            obj.queue_draw();
        }

        /// Works out which tiles are needed, releases far-away textures and
        /// hands the missing ones to the workers. Cheap when nothing changed.
        pub fn refresh(&self) {
            let obj = self.obj();
            let h = obj.height() as f64;
            if h <= 0.0 {
                return;
            }
            let z = self.zoom.get();
            let vv = self.offsets().1;
            let want_q = self.want_q.get();
            let mut st = self.state.borrow_mut();
            if st.layout.tiles.is_empty() {
                return;
            }
            let (top, hs) = (vv / z, h / z);
            let bot = top + hs;
            let req = st.layout.range(top - REQUEST_SCREENS * hs, bot + REQUEST_SCREENS * hs);
            let near = st.layout.range(top - NEAR_SCREENS * hs, bot + NEAR_SCREENS * hs);
            let key = (req, near, want_q);
            if st.last_key == Some(key) {
                return;
            }
            st.last_key = Some(key);

            // Release textures far outside the window.
            let keep = st.layout.range(top - KEEP_SCREENS * hs, bot + KEEP_SCREENS * hs);
            st.keep = keep;
            let n = st.tex.len();
            for i in (0..keep.0).chain(keep.1..n) {
                st.tex[i] = None;
            }

            // Missing tiles, grouped per page so each page is decoded once.
            let centre = (top + bot) / 2.0;
            let scale = st.layout.scale;
            let width = st.layout.width;
            let mut groups: BTreeMap<(usize, u8), (f64, Vec<(f64, TileJob)>)> = BTreeMap::new();
            for i in req.0..req.1 {
                let need = if (near.0..near.1).contains(&i) { want_q } else { 1 };
                let have = st.tex[i].as_ref().map_or(0, |t| t.q);
                if have >= need || st.failed.contains(&i) {
                    continue;
                }
                let t = &st.layout.tiles[i];
                let dist = (t.y + t.h / 2.0 - centre).abs();
                let job = TileJob {
                    tile: i,
                    src_top: t.src_top,
                    src_h: t.src_h,
                    out_w: (width * scale * need as f64).round() as u32,
                    out_h: (t.h * scale * need as f64).round().max(1.0) as u32,
                };
                let e = groups.entry((t.page, need)).or_insert((f64::MAX, Vec::new()));
                e.0 = e.0.min(dist);
                e.1.push((dist, job));
            }
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|a, b| a.1.0.total_cmp(&b.1.0));
            let generation = st.generation;
            let jobs: Vec<Job> = groups
                .into_iter()
                .map(|((page, q), (_, mut tiles))| {
                    tiles.sort_by(|a, b| a.0.total_cmp(&b.0));
                    Job {
                        generation,
                        q,
                        path: st.pages[page].path.clone(),
                        tiles: tiles.into_iter().map(|(_, t)| t).collect(),
                    }
                })
                .collect();
            drop(st);
            if let Some(l) = self.loader.borrow().as_ref() {
                l.submit(jobs);
            }
        }

        /// A worker finished a tile: wrap the pixels in a texture (no copy, no
        /// GPU work yet) and ask for a redraw.
        pub fn on_tile(&self, r: TileResult) {
            if let Some(l) = self.loader.borrow().as_ref() {
                l.finish((r.generation, r.tile, r.q));
            }
            let mut st = self.state.borrow_mut();
            if r.generation != st.generation || r.tile >= st.tex.len() {
                return;
            }
            if r.rgb.is_empty() {
                st.failed.insert(r.tile);
                return;
            }
            if !(st.keep.0..st.keep.1).contains(&r.tile) || st.tex[r.tile].as_ref().is_some_and(|t| t.q >= r.q) {
                return;
            }
            let bytes = glib::Bytes::from_owned(r.rgb);
            let texture = gdk::MemoryTexture::new(
                r.w as i32,
                r.h as i32,
                gdk::MemoryFormat::R8g8b8a8Premultiplied,
                &bytes,
                r.w as usize * 4,
            );
            st.tex[r.tile] = Some(TileTex { texture: texture.upcast(), q: r.q });
            stats::note_tile();
            drop(st);
            self.obj().queue_draw();
        }

        pub fn apply_zoom(&self, new_zoom: f64, fx: f64, fy: f64) {
            let old = self.zoom.get();
            let mut z = new_zoom.clamp(1.0, MAX_ZOOM);
            if z < 1.03 {
                z = 1.0;
            }
            if (z - old).abs() < 1e-4 {
                return;
            }
            let (hv, vv) = self.offsets();
            let (cx, cy) = ((hv + fx) / old, (vv + fy) / old);
            self.zoom.set(z);
            self.configure_adjustments(Some((cx * z - fx, cy * z - fy)));
            self.obj().queue_draw();
        }

        /// Zoom gesture finished: re-render what is visible at higher resolution.
        pub fn zoom_settled(&self) {
            self.want_q.set(if self.zoom.get() > 1.2 { 2 } else { 1 });
            self.refresh();
        }

        pub fn stop_fling(&self) {
            self.fling.set((0.0, 0.0));
            if let Some(id) = self.fling_tick.borrow_mut().take() {
                id.remove();
            }
        }

        fn scroll_by(&self, dx: f64, dy: f64) {
            let clamp = |a: &RefCell<Option<gtk::Adjustment>>, d: f64| {
                if let Some(a) = a.borrow().as_ref() {
                    a.set_value((a.value() + d).clamp(0.0, (a.upper() - a.page_size()).max(0.0)));
                }
            };
            clamp(&self.hadjustment, dx);
            clamp(&self.vadjustment, dy);
        }

        /// Exponential-decay fling driven by the frame clock. Only ticks while
        /// the content is actually moving.
        pub fn start_fling(&self, vx: f64, vy: f64) {
            self.stop_fling();
            self.fling.set((vx, vy));
            let weak = self.obj().downgrade();
            let last = Cell::new(0i64);
            let id = self.obj().add_tick_callback(move |_, clock| {
                let Some(view) = weak.upgrade() else { return glib::ControlFlow::Break };
                let imp = view.imp();
                let now = clock.frame_time();
                let dt = if last.get() == 0 { 1.0 / 60.0 } else { ((now - last.get()) as f64 / 1e6).min(0.05) };
                last.set(now);
                let (vx, vy) = imp.fling.get();
                let decay = (-3.2 * dt).exp();
                let (nx, ny) = (vx * decay, vy * decay);
                imp.fling.set((nx, ny));
                imp.scroll_by(nx * dt, ny * dt);
                if nx.hypot(ny) < 25.0 {
                    imp.fling_tick.borrow_mut().take();
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            });
            *self.fling_tick.borrow_mut() = Some(id);
        }

        pub fn drag_update(&self, dx: f64, dy: f64) {
            let (sx, sy) = self.drag_start.get();
            let set = |a: &RefCell<Option<gtk::Adjustment>>, v: f64| {
                if let Some(a) = a.borrow().as_ref() {
                    a.set_value(v.clamp(0.0, (a.upper() - a.page_size()).max(0.0)));
                }
            };
            set(&self.hadjustment, sx - dx);
            set(&self.vadjustment, sy - dy);
            let mut s = self.drag_samples.borrow_mut();
            s.push_back((Instant::now(), dx, dy));
            while s.front().is_some_and(|f| f.0.elapsed().as_millis() > 120) {
                s.pop_front();
            }
        }

        pub fn drag_end(&self) {
            let s = self.drag_samples.borrow();
            if let (Some(a), Some(b)) = (s.front(), s.back()) {
                let dt = b.0.duration_since(a.0).as_secs_f64();
                if dt > 0.015 {
                    let (vx, vy) = (-(b.1 - a.1) / dt, -(b.2 - a.2) / dt);
                    if vx.hypot(vy) > 150.0 && b.0.elapsed().as_millis() < 80 {
                        drop(s);
                        self.start_fling(vx, vy);
                    }
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct StripView(ObjectSubclass<imp::StripView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl Default for StripView {
    fn default() -> Self {
        Self::new()
    }
}

impl StripView {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Replaces the content and scrolls to the top.
    pub fn set_pages(&self, pages: Vec<PageInfo>) {
        let imp = self.imp();
        imp.stop_fling();
        imp.state.borrow_mut().pages = pages;
        imp.zoom.set(1.0);
        imp.want_q.set(1);
        if self.width() > 0 {
            imp.relayout(false);
            imp.refresh();
        } else {
            // Not allocated yet: the first size_allocate builds the layout.
            imp.state.borrow_mut().layout = Layout::default();
        }
    }

    /// Reading progress through the chapter, 0.0 – 1.0.
    pub fn progress(&self) -> f64 {
        match self.vadjustment() {
            Some(a) if a.upper() > a.page_size() => a.value() / (a.upper() - a.page_size()),
            _ => 0.0,
        }
    }

    /// Zoom around a widget-space point (used by the pinch gesture and the
    /// automatic test mode).
    pub fn zoom_to(&self, zoom: f64, fx: f64, fy: f64) {
        self.imp().apply_zoom(zoom, fx, fy);
    }

    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    pub fn zoom_settled(&self) {
        self.imp().zoom_settled();
    }

    fn request_panel(&self) {
        self.emit_by_name::<()>("panel-requested", &[]);
    }

    pub fn connect_panel_requested<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_closure(
            "panel-requested",
            false,
            glib::closure_local!(move |view: &Self| f(view)),
        )
    }

    fn setup_gestures(&self) {
        // Pinch zoom: a GPU transform only while the fingers are down.
        let zoom = gtk::GestureZoom::new();
        zoom.set_propagation_phase(gtk::PropagationPhase::Capture);
        zoom.connect_begin(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, _| {
                view.imp().zoom_start.set(view.imp().zoom.get());
                view.imp().stop_fling();
                g.set_state(gtk::EventSequenceState::Claimed);
            }
        ));
        zoom.connect_scale_changed(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, scale| {
                let (fx, fy) = g.bounding_box_center().unwrap_or((view.width() as f64 / 2.0, view.height() as f64 / 2.0));
                view.imp().apply_zoom(view.imp().zoom_start.get() * scale, fx, fy);
            }
        ));
        zoom.connect_end(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _| view.imp().zoom_settled()
        ));
        zoom.connect_cancel(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _| view.imp().zoom_settled()
        ));
        self.add_controller(zoom);

        // Pen and mouse dragging scroll the content (GTK treats the pen like a
        // mouse, so the scrolled window would not do it). Touch is left to the
        // scrolled window's kinetic scrolling.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, _, _| {
                if g.device().is_some_and(|d| d.source() == gdk::InputSource::Touchscreen) {
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                let imp = view.imp();
                imp.stop_fling();
                imp.drag_start.set(imp.offsets());
                imp.drag_samples.borrow_mut().clear();
                imp.drag_claimed.set(false);
                view.grab_focus();
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, dx, dy| {
                let imp = view.imp();
                if !imp.drag_claimed.get() {
                    if dx.hypot(dy) < 8.0 {
                        return;
                    }
                    imp.drag_claimed.set(true);
                    g.set_state(gtk::EventSequenceState::Claimed);
                }
                imp.drag_update(dx, dy);
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, _| {
                if view.imp().drag_claimed.replace(false) {
                    view.imp().drag_end();
                }
            }
        ));
        self.add_controller(drag);

        // Triple tap / triple click opens the panel; so does the pen's side button.
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, _, _, _| {
                view.imp().stop_fling();
                let pen = g.device().is_some_and(|d| d.source() == gdk::InputSource::Pen);
                if pen && g.current_button() != gdk::BUTTON_PRIMARY {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    view.request_panel();
                }
            }
        ));
        click.connect_released(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |g, n_press, _, _| {
                if n_press == 3 && g.current_button() == gdk::BUTTON_PRIMARY {
                    view.request_panel();
                }
            }
        ));
        self.add_controller(click);
    }
}
