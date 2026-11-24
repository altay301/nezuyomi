//! Cover thumbnails: downloaded, cropped to a 2:3 portrait and scaled on two
//! low-priority threads; the main thread only wraps the pixels in a texture.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use gtk::{gdk, glib, prelude::*};

use crate::{net, paths};

pub const THUMB_W: u32 = 240;
pub const THUMB_H: u32 = 340;

struct Req {
    id: u64,
    url: String,
}

struct Done {
    id: u64,
    rgba: Option<Vec<u8>>,
}

pub struct Covers {
    tx: mpsc::Sender<Req>,
    targets: RefCell<HashMap<u64, glib::WeakRef<gtk::Picture>>>,
    next: Cell<u64>,
}

impl Covers {
    pub fn new() -> Rc<Self> {
        let (tx, rx) = mpsc::channel::<Req>();
        let rx = Arc::new(Mutex::new(rx));
        let (done_tx, done_rx) = async_channel::unbounded::<Done>();
        for n in 0..2 {
            let (rx, done_tx) = (rx.clone(), done_tx.clone());
            std::thread::Builder::new()
                .name(format!("cover-{n}"))
                .spawn(move || {
                    crate::loader::lower_priority();
                    let mut resizer = Resizer::new();
                    loop {
                        let Ok(req) = rx.lock().unwrap().recv() else { return };
                        let rgba = thumbnail(&mut resizer, &req.url);
                        if done_tx.send_blocking(Done { id: req.id, rgba }).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn cover worker");
        }
        let this = Rc::new(Self { tx, targets: RefCell::default(), next: Cell::new(1) });
        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            while let Ok(done) = done_rx.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let Some(pic) = this.targets.borrow_mut().remove(&done.id).and_then(|w| w.upgrade()) else { continue };
                if let Some(rgba) = done.rgba {
                    let bytes = glib::Bytes::from_owned(rgba);
                    let tex = gdk::MemoryTexture::new(
                        THUMB_W as i32,
                        THUMB_H as i32,
                        gdk::MemoryFormat::R8g8b8x8,
                        &bytes,
                        THUMB_W as usize * 4,
                    );
                    pic.set_paintable(Some(&tex));
                }
            }
        });
        this
    }

    /// Fills `picture` with the cover at `url` (http(s) URL or absolute local path) once it is ready.
    pub fn load(&self, picture: &gtk::Picture, url: &str) {
        let id = self.next.replace(self.next.get() + 1);
        self.targets.borrow_mut().insert(id, picture.downgrade());
        let _ = self.tx.send(Req { id, url: url.to_string() });
    }
}

fn thumbnail(resizer: &mut Resizer, url: &str) -> Option<Vec<u8>> {
    let bytes = if url.starts_with('/') {
        std::fs::read(url).ok()?
    } else {
        let cached = paths::cache_file("covers", url);
        match std::fs::read(&cached) {
            Ok(b) => b,
            Err(_) => {
                let b = net::get_bytes(url, &[("Referer".into(), referer_of(url))]).ok()?;
                if let Some(dir) = cached.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&cached, &b);
                b
            }
        }
    };
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    // Largest 2:3 portrait crop, anchored at the top (tall strips show their first panel).
    let target = THUMB_W as f64 / THUMB_H as f64;
    let (cw, ch) = if (w as f64 / h as f64) > target { (h as f64 * target, h as f64) } else { (w as f64, w as f64 / target) };
    let left = (w as f64 - cw) / 2.0;
    let src = ImageRef::new(w, h, img.as_raw(), PixelType::U8x4).ok()?;
    let mut dst = Image::new(THUMB_W, THUMB_H, PixelType::U8x4);
    let opts = ResizeOptions::new()
        .crop(left, 0.0, cw, ch)
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .use_alpha(false);
    resizer.resize(&src, &mut dst, &opts).ok()?;
    Some(dst.into_vec())
}

fn referer_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or_default();
    format!("{}://{}/", url.split("://").next().unwrap_or("https"), rest.split('/').next().unwrap_or_default())
}
