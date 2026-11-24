//! Frame-time statistics (debug builds, or `MANGA_FRAME_STATS=1`).
//!
//! Hooks the frame clock's `after-paint` signal, so it costs nothing while the
//! app is idle. Every frame whose interval exceeds 20 ms is logged to stderr.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use gtk::prelude::*;

pub const SLOW_FRAME_US: i64 = 20_000;
/// Longer gaps mean the app was idle, not that a frame was slow.
const IDLE_GAP_US: i64 = 250_000;

static SNAPSHOT_MAX_US: AtomicU64 = AtomicU64::new(0);
static TILES_ARRIVED: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    cfg!(debug_assertions) || std::env::var_os("MANGA_FRAME_STATS").is_some()
}

/// Called by `StripView::snapshot` with its own run time.
pub fn note_snapshot(us: u64) {
    SNAPSHOT_MAX_US.fetch_max(us, Ordering::Relaxed);
}

/// Called by `StripView` for every tile that reaches the main thread.
pub fn note_tile() {
    TILES_ARRIVED.fetch_add(1, Ordering::Relaxed);
}

#[derive(Default)]
pub struct Stats {
    update_at: Cell<Option<Instant>>,
    cpu_sum_us: Cell<u64>,
    started_us: Cell<i64>,
    tiles_seen: Cell<u64>,
    last_frame_us: Cell<i64>,
    last_label_us: Cell<i64>,
    frames: Cell<u64>,
    slow: Cell<u64>,
    worst_us: Cell<i64>,
    recent_worst_us: Cell<i64>,
}

impl Stats {
    pub fn summary(&self) -> String {
        format!(
            "frames: {}  over 20 ms: {}  worst: {:.1} ms  avg app cpu/frame: {:.2} ms  worst snapshot: {:.2} ms",
            self.frames.get(),
            self.slow.get(),
            self.worst_us.get() as f64 / 1000.0,
            self.cpu_sum_us.get() as f64 / 1000.0 / self.frames.get().max(1) as f64,
            SNAPSHOT_MAX_US.load(Ordering::Relaxed) as f64 / 1000.0,
        )
    }

    /// The frame clock starts a frame: remember when, to time our own work.
    fn on_update(&self) {
        self.update_at.set(Some(Instant::now()));
    }

    fn on_frame(&self, now_us: i64, label: &gtk::Label) {
        if self.started_us.get() == 0 {
            self.started_us.set(now_us);
        }
        let dt = now_us - self.last_frame_us.replace(now_us);
        // CPU time between the start of the frame and the end of painting.
        let cpu = self.update_at.take().map_or(0, |t| t.elapsed().as_micros() as u64);
        let tiles = TILES_ARRIVED.load(Ordering::Relaxed);
        let new_tiles = tiles - self.tiles_seen.replace(tiles);
        if dt <= 0 || dt > IDLE_GAP_US {
            return;
        }
        self.frames.set(self.frames.get() + 1);
        self.cpu_sum_us.set(self.cpu_sum_us.get() + cpu);
        self.worst_us.set(self.worst_us.get().max(dt));
        self.recent_worst_us.set(self.recent_worst_us.get().max(dt));
        if dt > SLOW_FRAME_US {
            self.slow.set(self.slow.get() + 1);
            eprintln!(
                "[frame] {:.1} ms at t={:.2}s, app cpu {:.1} ms, {} new tiles (slow frame #{})",
                dt as f64 / 1000.0,
                (now_us - self.started_us.get()) as f64 / 1e6,
                cpu as f64 / 1000.0,
                new_tiles,
                self.slow.get()
            );
        }
        // Refresh the on-screen label twice a second while frames keep coming.
        if now_us - self.last_label_us.get() > 500_000 {
            self.last_label_us.set(now_us);
            label.set_text(&format!(
                "worst {:.1} ms (500 ms) · slow {} / {}\nsnapshot max {:.2} ms",
                self.recent_worst_us.replace(0) as f64 / 1000.0,
                self.slow.get(),
                self.frames.get(),
                SNAPSHOT_MAX_US.swap(0, Ordering::Relaxed) as f64 / 1000.0,
            ));
        }
    }
}

/// Starts collecting once the window has a surface (and so a frame clock).
pub fn install(window: &gtk::ApplicationWindow, label: &gtk::Label) -> Rc<Stats> {
    let stats = Rc::new(Stats::default());
    let (s, label) = (stats.clone(), label.clone());
    window.connect_realize(move |w| {
        let Some(clock) = w.surface().map(|surface| surface.frame_clock()) else { return };
        let (s, label) = (s.clone(), label.clone());
        let s2 = s.clone();
        clock.connect_update(move |_| s2.on_update());
        clock.connect_after_paint(move |c| s.on_frame(c.frame_time(), &label));
    });
    stats
}
