//! Anime playback in mpv (PLAN.md → Phase 4).
//!
//! mpv decodes through VA-API (`--hwdec=vaapi`, hardware decoding on the UHD 620 keeps
//! the CPU and the heat low) and shows its own full-screen window. We only watch it
//! over mpv's JSON IPC socket: every few seconds we ask for the playback position and
//! store it, so "Continue watching" resumes where the episode was left.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::{Db, Progress};
use crate::source::Video;

/// Watched at least this far counts as "watched".
const WATCHED_THRESHOLD: f64 = 0.9;
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct PlayRequest {
    pub video: Video,
    pub title: String,
    pub source: String,
    pub manga_id: String,
    pub chapter_id: String,
    /// 0.0 – 1.0; ignored when (almost) finished.
    pub start_fraction: f64,
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    /// mpv has exited; the stored progress is up to date.
    Finished { source: String, manga_id: String, chapter_id: String, watched: bool },
    Failed(String),
}

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("manga-reader-mpv-{}.sock", std::process::id()))
}

/// Arguments for mpv; separate from `play` so they can be tested.
pub fn mpv_args(req: &PlayRequest, socket: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--hwdec=vaapi".into(),
        "--fs".into(),
        format!("--input-ipc-server={socket}"),
        format!("--force-media-title={}", req.title),
        "--msg-level=all=warn".into(),
    ];
    let mut extra_headers = Vec::new();
    for (k, v) in &req.video.headers {
        if k.eq_ignore_ascii_case("referer") {
            a.push(format!("--referrer={v}"));
        } else {
            extra_headers.push(format!("{k}: {v}"));
        }
    }
    if !extra_headers.is_empty() {
        a.push(format!("--http-header-fields={}", extra_headers.join(",")));
    }
    // The default subtitle track, else the first one.
    if let Some(sub) = req.video.subtitles.iter().find(|s| s.default).or(req.video.subtitles.first()) {
        a.push(format!("--sub-file={}", sub.url));
    }
    if req.start_fraction > 0.01 && req.start_fraction < WATCHED_THRESHOLD {
        a.push(format!("--start={:.1}%", req.start_fraction * 100.0));
    }
    a.push("--".into());
    a.push(req.video.url.clone());
    a
}

/// Starts mpv and returns immediately; `events` gets one event when it ends.
pub fn play(db: Arc<Db>, req: PlayRequest, events: async_channel::Sender<PlayerEvent>) {
    std::thread::Builder::new()
        .name("mpv-watch".into())
        .spawn(move || {
            crate::loader::lower_priority(); // only this watcher thread, not mpv itself
            let event = match run(&db, &req) {
                Ok(watched) => PlayerEvent::Finished { source: req.source, manga_id: req.manga_id, chapter_id: req.chapter_id, watched },
                Err(e) => PlayerEvent::Failed(e),
            };
            let _ = events.send_blocking(event);
        })
        .expect("spawn mpv watcher");
}

fn run(db: &Db, req: &PlayRequest) -> Result<bool, String> {
    let socket = socket_path();
    let _ = std::fs::remove_file(&socket);
    let mut args = mpv_args(req, &socket.to_string_lossy());
    // Test hook: MANGA_MPV_ARGS="--vo=null --ao=null --length=14" (later options win in mpv).
    if let Ok(extra) = std::env::var("MANGA_MPV_ARGS") {
        let at = args.len() - 2; // before the "--" separator and the URL
        args.splice(at..at, extra.split_whitespace().map(String::from));
    }
    let mut child = Command::new("mpv")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "mpv is not installed (sudo dnf install mpv)".to_string() } else { format!("cannot start mpv: {e}") })?;

    let mut position: Option<(f64, f64)> = None; // (seconds, duration)
    let mut conn: Option<(UnixStream, BufReader<UnixStream>)> = None;
    let mut last_poll = Instant::now();
    let save = |pos: Option<(f64, f64)>| {
        if let Some((p, d)) = pos.filter(|(_, d)| *d > 0.0) {
            let fraction = (p / d).clamp(0.0, 1.0);
            db.save_progress(req.source.clone(), req.manga_id.clone(), req.chapter_id.clone(), Progress { fraction, read: fraction >= WATCHED_THRESHOLD });
        }
    };

    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        if conn.is_none() {
            if let Ok(s) = UnixStream::connect(&socket) {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                if let Ok(r) = s.try_clone() {
                    conn = Some((s, BufReader::new(r)));
                }
            }
        }
        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            if let Some((w, r)) = conn.as_mut() {
                let (pos, dur) = (get_property(w, r, "time-pos"), get_property(w, r, "duration"));
                if let (Some(p), Some(d)) = (pos, dur) {
                    position = Some((p, d));
                    save(position);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&socket);
    // mpv exited: the last polled position is as good as it gets (up to 5 s stale).
    save(position);
    Ok(position.is_some_and(|(p, d)| d > 0.0 && p / d >= WATCHED_THRESHOLD))
}

/// One `get_property` round trip; events and other replies in between are skipped.
fn get_property(w: &mut UnixStream, r: &mut BufReader<UnixStream>, name: &str) -> Option<f64> {
    const ID: i64 = 4242;
    writeln!(w, "{}", serde_json::json!({ "command": ["get_property", name], "request_id": ID })).ok()?;
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("request_id").and_then(|i| i.as_i64()) == Some(ID) {
            return v.get("data").and_then(|d| d.as_f64());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Subtitle;

    fn request(start: f64) -> PlayRequest {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Referer".to_string(), "https://embed.example/".to_string());
        headers.insert("X-Test".to_string(), "1".to_string());
        PlayRequest {
            video: Video {
                url: "https://cdn.example/master.m3u8".into(),
                label: "Sub".into(),
                headers,
                subtitles: vec![
                    Subtitle { url: "https://cdn.example/fr.vtt".into(), label: "French".into(), default: false },
                    Subtitle { url: "https://cdn.example/en.vtt".into(), label: "English".into(), default: true },
                ],
            },
            title: "Show — Episode 1".into(),
            source: "s".into(),
            manga_id: "m".into(),
            chapter_id: "c".into(),
            start_fraction: start,
        }
    }

    #[test]
    fn arguments_use_hardware_decoding_referrer_and_default_subtitles() {
        let a = mpv_args(&request(0.0), "/run/x.sock");
        assert!(a.contains(&"--hwdec=vaapi".to_string()));
        assert!(a.contains(&"--referrer=https://embed.example/".to_string()));
        assert!(a.contains(&"--http-header-fields=X-Test: 1".to_string()));
        assert!(a.contains(&"--sub-file=https://cdn.example/en.vtt".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--start")));
        assert_eq!(a.last().unwrap(), "https://cdn.example/master.m3u8");
    }

    #[test]
    fn resumes_only_when_partly_watched() {
        let start = |f| mpv_args(&request(f), "s").into_iter().find(|a| a.starts_with("--start"));
        assert_eq!(start(0.5).as_deref(), Some("--start=50.0%"));
        assert_eq!(start(0.95), None, "a finished episode restarts from the beginning");
        assert_eq!(start(0.0), None);
    }
}
