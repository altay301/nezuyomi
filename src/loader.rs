//! Background tile decoding. Nothing in here runs on the GTK main thread.
//!
//! The main thread submits an ordered list of [`Job`]s (nearest to the viewport
//! first). Worker threads run at low CPU and idle I/O priority, decode the page
//! once, resize one crop per tile and send the RGB pixels back over a channel.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::RgbaImage;

/// One tile of a page: which source rows to crop and the output pixel size.
pub struct TileJob {
    pub tile: usize,
    pub src_top: f64,
    pub src_h: f64,
    pub out_w: u32,
    pub out_h: u32,
}

/// All wanted tiles of one page at one quality level (decoded once).
pub struct Job {
    pub generation: u64,
    pub q: u8,
    pub path: PathBuf,
    pub tiles: Vec<TileJob>,
}

/// Pixels are RGBA8 (opaque), the GPU's native layout, so the main thread never
/// converts anything. An empty `rgb` means decoding failed.
pub struct TileResult {
    pub generation: u64,
    pub tile: usize,
    pub q: u8,
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

/// (layout generation, tile index, quality)
pub type Key = (u64, usize, u8);

const CACHE_SLOTS: usize = 2;

struct Inner {
    jobs: VecDeque<Job>,
    running: HashSet<Key>,
    generation: u64,
    cache: Vec<(PathBuf, Arc<RgbaImage>)>,
    quit: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

pub struct Loader {
    shared: Arc<Shared>,
}

impl Loader {
    pub fn new(tx: async_channel::Sender<TileResult>, threads: usize) -> Self {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                jobs: VecDeque::new(),
                running: HashSet::new(),
                generation: 0,
                cache: Vec::new(),
                quit: false,
            }),
            wake: Condvar::new(),
        });
        for n in 0..threads {
            let (shared, tx) = (shared.clone(), tx.clone());
            std::thread::Builder::new()
                .name(format!("tile-worker-{n}"))
                .spawn(move || {
                    lower_priority();
                    worker(&shared, &tx);
                })
                .expect("spawn tile worker");
        }
        Self { shared }
    }

    /// A new layout invalidates everything queued or running.
    pub fn reset(&self, generation: u64) {
        let mut g = self.shared.inner.lock().unwrap();
        g.generation = generation;
        g.jobs.clear();
        g.running.clear();
    }

    /// Replaces the not-yet-started jobs with `jobs` (already in priority order).
    /// Tiles that a worker is already decoding are left out.
    pub fn submit(&self, jobs: Vec<Job>) {
        let mut g = self.shared.inner.lock().unwrap();
        g.jobs.clear();
        for mut job in jobs {
            job.tiles.retain(|t| !g.running.contains(&(job.generation, t.tile, job.q)));
            if !job.tiles.is_empty() && job.generation == g.generation {
                g.jobs.push_back(job);
            }
        }
        drop(g);
        self.shared.wake.notify_all();
    }

    /// The main thread is done with a result (stored or discarded).
    pub fn finish(&self, key: Key) {
        self.shared.inner.lock().unwrap().running.remove(&key);
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.shared.inner.lock().unwrap().quit = true;
        self.shared.wake.notify_all();
    }
}

/// Nice 10 and the idle I/O class for the calling thread (Linux).
pub fn lower_priority() {
    // SAFETY: plain syscalls with integer arguments; `who = 0` means this thread.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0 as libc::c_long, IOPRIO_CLASS_IDLE << 13);
    }
}

fn worker(shared: &Shared, tx: &async_channel::Sender<TileResult>) {
    let mut resizer = Resizer::new();
    loop {
        let job = {
            let mut g = shared.inner.lock().unwrap();
            loop {
                if g.quit {
                    return;
                }
                if let Some(job) = g.jobs.pop_front() {
                    for t in &job.tiles {
                        g.running.insert((job.generation, t.tile, job.q));
                    }
                    break job;
                }
                g = shared.wake.wait(g).unwrap();
            }
        };

        let img = decode_cached(shared, &job);
        for t in &job.tiles {
            if shared.inner.lock().unwrap().generation != job.generation {
                break; // layout changed while we were working
            }
            let rgb = img
                .as_ref()
                .and_then(|img| render_tile(&mut resizer, img, t))
                .unwrap_or_default();
            let result = TileResult { generation: job.generation, tile: t.tile, q: job.q, w: t.out_w, h: t.out_h, rgb };
            if tx.send_blocking(result).is_err() {
                return; // UI is gone
            }
        }
    }
}

fn decode_cached(shared: &Shared, job: &Job) -> Option<Arc<RgbaImage>> {
    {
        let g = shared.inner.lock().unwrap();
        if let Some((_, img)) = g.cache.iter().find(|(p, _)| *p == job.path) {
            return Some(img.clone());
        }
    }
    let img = match image::ImageReader::open(&job.path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())
        .and_then(|r| r.decode().map_err(|e| e.to_string()))
    {
        Ok(img) => Arc::new(img.into_rgba8()),
        Err(e) => {
            eprintln!("[loader] cannot decode {}: {e}", job.path.display());
            return None;
        }
    };
    let mut g = shared.inner.lock().unwrap();
    g.cache.push((job.path.clone(), img.clone()));
    if g.cache.len() > CACHE_SLOTS {
        g.cache.remove(0);
    }
    Some(img)
}

/// Crops the tile's rows out of the full-size page and resizes them to the
/// output size (SIMD convolution in `fast_image_resize`).
fn render_tile(resizer: &mut Resizer, img: &RgbaImage, t: &TileJob) -> Option<Vec<u8>> {
    let (sw, sh) = img.dimensions();
    let top = t.src_top.clamp(0.0, sh as f64 - 1.0);
    let height = t.src_h.min(sh as f64 - top).max(1.0);
    let src = ImageRef::new(sw, sh, img.as_raw(), PixelType::U8x4).ok()?;
    let mut dst = Image::new(t.out_w, t.out_h, PixelType::U8x4);
    let options = ResizeOptions::new()
        .crop(0.0, top, sw as f64, height)
        .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
        // Pages are opaque; skip the premultiply / divide passes.
        .use_alpha(false);
    resizer.resize(&src, &mut dst, &options).ok()?;
    Some(dst.into_vec())
}
