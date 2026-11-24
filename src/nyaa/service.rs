//! Background service around the Nyaa pipeline: runs cycles on worker threads (never two at
//! once), applies the power policy to qBittorrent, and reports back to the UI over a channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::qbit::{Client, QbitError};
use super::rules::{Edition, Rules};
use super::store::{self, Link, Release};
use super::sync::{self, Ctx, NyaaFetcher, Report, Settings};
use super::secret;
use crate::db::Db;

#[derive(Debug, Clone)]
pub enum Event {
    /// A cycle finished (or failed to start: see `Report::qbit_error`).
    Cycle(Report),
    /// Result of linking a series: the link and how many releases it matched so far.
    Linked(Result<(Link, usize), String>),
    /// Result of a manual "Download" of one release.
    Queued { title: String, result: Result<String, String> },
    /// The power policy paused or resumed our torrents.
    Power { paused: bool, result: Result<usize, String> },
    /// Connection test: qBittorrent's version, or what is wrong.
    Qbit(Result<String, String>),
    /// Data in the database changed; lists should reload.
    Changed,
    /// Result of a search across the followed uploaders: how many releases were new.
    Searched(Result<usize, String>),
    /// Whether any of our torrents is still downloading (the UI polls only then).
    Active(bool),
}

pub struct Service {
    db: Arc<Db>,
    rules: Arc<Rules>,
    busy: AtomicBool,
    events: async_channel::Sender<Event>,
}

fn build_client(settings: &Settings) -> Client {
    // The password never leaves the Secret Service except into this in-memory client.
    let password = settings.qbit_user.as_deref().and_then(secret::lookup);
    Client::new(settings.qbit_port, settings.qbit_user.clone(), password)
}

impl Service {
    pub fn new(db: Arc<Db>, events: async_channel::Sender<Event>) -> Arc<Self> {
        Arc::new(Self { db, rules: Arc::new(Rules::load()), busy: AtomicBool::new(false), events })
    }

    fn spawn(self: &Arc<Self>, name: &str, work: impl FnOnce(&Service) + Send + 'static) {
        let this = self.clone();
        std::thread::Builder::new()
            .name(format!("nyaa-{name}"))
            .spawn(move || {
                crate::loader::lower_priority();
                work(&this);
            })
            .expect("spawn nyaa worker");
    }

    fn with_ctx<T>(&self, f: impl FnOnce(&Ctx) -> T) -> T {
        let settings = Settings::load(&self.db);
        let qbit = build_client(&settings);
        let ctx = Ctx { db: &self.db, rules: &self.rules, settings: &settings, fetcher: &NyaaFetcher, qbit: &qbit };
        f(&ctx)
    }

    /// Feeds, matching, queueing, progress. A second request while one is running is ignored.
    pub fn run_cycle(self: &Arc<Self>, force: bool) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.spawn("cycle", move |s| {
            let report = s.with_ctx(|ctx| sync::run_cycle(ctx, force));
            s.busy.store(false, Ordering::SeqCst);
            let _ = s.events.send_blocking(Event::Cycle(report));
            let _ = s.events.send_blocking(Event::Active(s.has_active_downloads()));
            let _ = s.events.send_blocking(Event::Changed);
        });
    }

    /// Progress only (no network to Nyaa): used by the 15 s timer while downloads are active.
    pub fn poll(self: &Arc<Self>) {
        if self.busy.load(Ordering::SeqCst) {
            return;
        }
        self.spawn("poll", move |s| {
            let mut report = Report::default();
            s.with_ctx(|ctx| sync::poll_downloads(ctx, &mut report));
            if report.completed > 0 || report.qbit_error.is_some() {
                let _ = s.events.send_blocking(Event::Cycle(report));
            }
            let _ = s.events.send_blocking(Event::Active(s.has_active_downloads()));
            let _ = s.events.send_blocking(Event::Changed);
        });
    }

    /// Whether anything of ours is being downloaded (the UI keeps its poll timer only then).
    pub fn has_active_downloads(&self) -> bool {
        !self.db.nyaa_releases_in_states(&[store::DOWNLOADING]).is_empty()
    }

    pub fn link(self: &Arc<Self>, source: String, manga_id: String, title: String, uploader: String, query: String, auto: bool, colored: bool) {
        self.spawn("link", move |s| {
            let result = s.with_ctx(|ctx| {
                let edition = if colored { Edition::Colored } else { Edition::Standard };
                let link = sync::link_series(ctx, &source, &manga_id, &title, &uploader, &query, auto, edition)?;
                let n = s.db.nyaa_releases_of_link(link.id).iter().filter(|r| r.state != store::SKIPPED || r.note.contains("nothing new")).count();
                Ok::<_, String>((link, n))
            });
            let _ = s.events.send_blocking(Event::Linked(result));
            let _ = s.events.send_blocking(Event::Changed);
        });
    }

    /// Manual "Download" for a release (also lifts the size cap).
    pub fn queue(self: &Arc<Self>, release: Release) {
        self.spawn("queue", move |s| {
            let result = s
                .with_ctx(|ctx| sync::queue_release(ctx, &release.info_hash, true))
                .map(|q| format!("{q:?}"));
            if let Err(e) = &result {
                s.db.nyaa_set_release(&release.info_hash, store::MATCHED, None, None, e);
            }
            let _ = s.events.send_blocking(Event::Queued { title: release.title, result });
            let _ = s.events.send_blocking(Event::Active(s.has_active_downloads()));
            let _ = s.events.send_blocking(Event::Changed);
        });
    }

    /// Searches the followed uploaders' feeds for a phrase (RSS only).
    pub fn search(self: &Arc<Self>, query: String) {
        self.spawn("search", move |s| {
            let result = s.with_ctx(|ctx| sync::search_feeds(ctx, &query));
            let _ = s.events.send_blocking(Event::Searched(result));
            let _ = s.events.send_blocking(Event::Changed);
        });
    }

    /// Series title and whether it is a colored edition, from a release title (pre-fills the link window).
    pub fn series_info_of(&self, uploader: &str, release_title: &str) -> Option<(String, bool)> {
        self.rules.parse(uploader, release_title).ok().map(|p| (p.title, p.edition == Edition::Colored))
    }

    pub fn test_connection(self: &Arc<Self>) {
        self.spawn("qbit-test", move |s| {
            let result = s.with_ctx(|ctx| ctx.qbit.version().map_err(|e| e.to_string()));
            let _ = s.events.send_blocking(Event::Qbit(result));
        });
    }

    /// Power policy: pause our running torrents (remembering exactly which), or resume exactly those.
    /// Torrents of other categories and ones the user stopped by hand are never touched.
    pub fn apply_power(self: &Arc<Self>, paused: bool) {
        self.spawn("power", move |s| {
            let result = s.with_ctx(|ctx| apply_power_policy(ctx, paused));
            let _ = s.events.send_blocking(Event::Power { paused, result: result.map_err(|e| e.to_string()) });
        });
    }
}

const POLICY_KEY: &str = "nyaa_policy_paused";

fn apply_power_policy(ctx: &Ctx, paused: bool) -> Result<usize, QbitError> {
    let mine = ctx.qbit.list();
    // qBittorrent not running is not an error for a policy that has nothing to pause.
    let mine = match mine {
        Ok(m) => m,
        Err(QbitError::NotRunning(_)) => return Ok(0),
        Err(e) => return Err(e),
    };
    if paused {
        let running: Vec<String> = mine.iter().filter(|t| !t.is_stopped()).map(|t| t.hash.clone()).collect();
        if running.is_empty() {
            return Ok(0);
        }
        ctx.qbit.stop(&running)?;
        // Remember what we stopped, also across restarts of this app.
        let mut known: Vec<String> = ctx.db.setting(POLICY_KEY).unwrap_or_default().split(',').filter(|h| !h.is_empty()).map(String::from).collect();
        known.extend(running.iter().cloned());
        known.sort();
        known.dedup();
        ctx.db.set_setting(POLICY_KEY, &known.join(","));
        Ok(running.len())
    } else {
        let ours: Vec<String> = ctx.db.setting(POLICY_KEY).unwrap_or_default().split(',').filter(|h| !h.is_empty()).map(String::from).collect();
        // Only torrents that are still ours and still stopped.
        let resume: Vec<String> = mine.iter().filter(|t| t.is_stopped() && ours.contains(&t.hash)).map(|t| t.hash.clone()).collect();
        if !resume.is_empty() {
            ctx.qbit.start(&resume)?;
        }
        ctx.db.set_setting(POLICY_KEY, "");
        Ok(resume.len())
    }
}

#[cfg(test)]
mod tests {
    // The policy's "only our torrents, only those it paused" behaviour is exercised against a real
    // qBittorrent in `nyaa::service::tests::power_policy_isolated` (ignored by default).
    use super::*;
    use std::path::PathBuf;

    #[test]
    #[ignore = "needs an isolated qBittorrent instance (see tools/qbt-test-instance.sh)"]
    fn power_policy_isolated() {
        let port: u16 = std::env::var("QBT_TEST_PORT").expect("QBT_TEST_PORT").parse().unwrap();
        let qdir = PathBuf::from(std::env::var("QBT_TEST_DIR").expect("QBT_TEST_DIR"));
        let tmp = std::env::temp_dir().join(format!("nezuyomi-power-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Db::open(&tmp.join("db.sqlite")).unwrap();
        let rules = Rules::from_toml(include_str!("rules.toml"), None).unwrap();
        let settings = Settings { root: qdir.join("dl"), qbit_port: port, ..Settings::defaults() };
        let qb = Client::new(port, None, None);
        let ctx = Ctx { db: &db, rules: &rules, settings: &settings, fetcher: &NyaaFetcher, qbit: &qb };
        for t in qb.list().unwrap() {
            qb.delete(&t.hash, false).unwrap();
        }
        let save = qdir.join("dl");
        qb.ensure_category(&save.to_string_lossy()).unwrap();

        // Two of ours (one running, one stopped by the user) and one foreign running torrent.
        let mk = |name: &str, category: Option<&str>, running: bool| -> String {
            std::fs::write(save.join(name), vec![1u8; 1000]).unwrap();
            let mut t = b"d4:infod6:lengthi1000e4:name".to_vec();
            t.extend(format!("{}:{name}", name.len()).bytes());
            t.extend(b"12:piece lengthi16384e6:pieces20:");
            t.extend([0u8; 20]);
            t.extend(b"ee");
            let hash = super::super::torrent::info_hash(&t).unwrap();
            if let Some(cat) = category {
                // ours: through the client (category manga-reader, paused), then start it if it should run
                assert_eq!(cat, super::super::qbit::CATEGORY);
                qb.add(&t, name, &save.to_string_lossy(), true).unwrap();
            } else {
                let b = "----f";
                let mut body = Vec::new();
                for (k, v) in [("category", "foreign-test"), ("savepath", save.to_str().unwrap()), ("skip_checking", "true"), ("paused", "false"), ("stopped", "false")] {
                    body.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").bytes());
                }
                body.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"f.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").bytes());
                body.extend(&t);
                body.extend(format!("\r\n--{b}--\r\n").bytes());
                let mut headers = vec![("Referer".to_string(), format!("http://127.0.0.1:{port}/")), ("Origin".to_string(), format!("http://127.0.0.1:{port}")), ("Content-Type".to_string(), format!("multipart/form-data; boundary={b}"))];
                headers.push(("User-Agent".into(), "test".into()));
                crate::net::request("POST", &format!("http://127.0.0.1:{port}/api/v2/torrents/add"), Some(&body), &headers).unwrap();
            }
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if category.is_some() {
                    if qb.list().unwrap().iter().any(|x| x.hash == hash && !x.state.starts_with("checking")) {
                        break;
                    }
                } else if foreign_states(port).iter().any(|(h, s)| *h == hash && !s.starts_with("checking")) {
                    break;
                }
            }
            if running && category.is_some() {
                qb.start(&[hash.clone()]).unwrap();
            }
            hash
        };
        let runs = mk(&format!("PolicyRuns{}.cbz", std::process::id()), Some("manga-reader"), true);
        let stopped_by_user = mk(&format!("PolicyStopped{}.cbz", std::process::id()), Some("manga-reader"), false);
        let foreign = mk(&format!("PolicyForeign{}.cbz", std::process::id()), None, true);
        let state = |h: &str| qb.list().unwrap().into_iter().find(|t| t.hash == h).map(|t| t.state);
        let is_stopped = |h: &str| state(h).is_some_and(|s| s.starts_with("stopped") || s.starts_with("paused"));
        // qBittorrent applies stop/start asynchronously: wait for the state to settle.
        let settle = |h: &str, stopped: bool| {
            for _ in 0..50 {
                if is_stopped(h) == stopped {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        };

        assert_eq!(apply_power_policy(&ctx, true).unwrap(), 1, "only the running torrent of ours is paused");
        settle(&runs, true);
        assert!(is_stopped(&runs));
        assert!(!foreign_states(port).iter().any(|(h, s)| *h == foreign && (s.starts_with("stopped") || s.starts_with("paused"))), "foreign torrent untouched");

        assert_eq!(apply_power_policy(&ctx, false).unwrap(), 1, "only what the policy paused is resumed");
        settle(&runs, false);
        assert!(!is_stopped(&runs));
        assert!(is_stopped(&stopped_by_user), "the torrent the user stopped stays stopped ({:?})", state(&stopped_by_user));
        assert_eq!(apply_power_policy(&ctx, false).unwrap(), 0, "idempotent");

        for h in [&runs, &stopped_by_user] {
            qb.delete(h, false).unwrap();
        }
        let _ = crate::net::request(
            "POST",
            &format!("http://127.0.0.1:{port}/api/v2/torrents/delete"),
            Some(format!("hashes={foreign}&deleteFiles=false").as_bytes()),
            &[("Referer".into(), format!("http://127.0.0.1:{port}/")), ("Origin".into(), format!("http://127.0.0.1:{port}")), ("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn foreign_states(port: u16) -> Vec<(String, String)> {
        let r = crate::net::request("GET", &format!("http://127.0.0.1:{port}/api/v2/torrents/info?category=foreign-test"), None, &[("Referer".into(), format!("http://127.0.0.1:{port}/")), ("Origin".into(), format!("http://127.0.0.1:{port}"))]).unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&r.body).unwrap();
        v.into_iter().map(|t| (t["hash"].as_str().unwrap_or_default().to_string(), t["state"].as_str().unwrap_or_default().to_string())).collect()
    }
}
