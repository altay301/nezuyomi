//! qBittorrent Web API v2 client (PLAN.md → Nyaa → "qBittorrent integration").
//!
//! The user's qBittorrent also holds their other torrents, so this client is deliberately
//! narrow: it only ever lists the `manga-reader` category, and every pause/resume/delete
//! call takes explicit info hashes (validated; `all` and malformed values are refused).
//! No global setting is read, modified or written back.

use std::fmt;
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;

use super::feed::url_encode;
use crate::net;

pub const CATEGORY: &str = "manga-reader";
pub const TAG: &str = "manga-reader";
pub const DEFAULT_PORT: u16 = 8080;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QbitError {
    /// Nothing listens on the port: qBittorrent is closed or its Web UI is off.
    NotRunning(u16),
    /// qBittorrent wants a login and we have none (or the password is wrong).
    AuthRequired(String),
    /// Host-header / CSRF protection rejected the request.
    Forbidden(String),
    Other(String),
}

impl fmt::Display for QbitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QbitError::NotRunning(port) => write!(
                f,
                "qBittorrent is not running, or its Web UI is switched off (nothing answers on 127.0.0.1:{port}). \
                 Start qBittorrent, then enable Tools → Options → Web UI → \"Web User Interface (Remote control)\", \
                 set a user name and password (qBittorrent 5.2 refuses to start the Web UI without them) and tick \
                 \"Bypass authentication for clients on localhost\"."
            ),
            QbitError::AuthRequired(why) => write!(f, "qBittorrent asks for a login: {why}. Tick \"Bypass authentication for clients on localhost\" in its Web UI options, or enter the password in Nyaa → Settings."),
            QbitError::Forbidden(why) => write!(f, "qBittorrent rejected the request ({why}). Check its Web UI \"host header validation\" and \"CSRF protection\" settings."),
            QbitError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QbitError {}

type Result<T> = std::result::Result<T, QbitError>;

/// What we need to know about one of our torrents.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TorrentStatus {
    pub hash: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub save_path: String,
    #[serde(default)]
    pub category: String,
}

impl TorrentStatus {
    pub fn is_complete(&self) -> bool {
        self.progress >= 1.0
    }
    /// Paused/stopped by anyone (qBittorrent 5 says `stoppedDL`/`stoppedUP`, 4.x `pausedDL`/`pausedUP`).
    pub fn is_stopped(&self) -> bool {
        self.state.starts_with("paused") || self.state.starts_with("stopped")
    }
    pub fn is_error(&self) -> bool {
        self.state == "error" || self.state == "missingFiles"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileStatus {
    pub name: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub priority: i64,
}

pub struct Client {
    port: u16,
    base: String,
    username: Option<String>,
    password: Option<String>,
    sid: Mutex<Option<String>>,
}

/// Info hashes are 40 (v1) or 64 (v2) hex characters. Anything else, in particular `all`, is refused.
pub fn check_hashes(hashes: &[String]) -> Result<String> {
    if hashes.is_empty() {
        return Err(QbitError::Other("no torrent hashes given".into()));
    }
    for h in hashes {
        let ok = (h.len() == 40 || h.len() == 64) && h.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok {
            return Err(QbitError::Other(format!("refusing to touch {h:?}: not an info hash")));
        }
    }
    Ok(hashes.iter().map(|h| h.to_lowercase()).collect::<Vec<_>>().join("|"))
}

impl Client {
    /// `password` comes from the Secret Service and only lives in memory. Without credentials the
    /// client relies on qBittorrent's "bypass authentication for clients on localhost".
    pub fn new(port: u16, username: Option<String>, password: Option<String>) -> Self {
        Self { port, base: format!("http://127.0.0.1:{port}"), username, password, sid: Mutex::new(None) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn is_listening(&self) -> bool {
        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok()
    }

    fn headers(&self, content_type: Option<&str>) -> Vec<(String, String)> {
        let mut h = vec![
            ("Referer".to_string(), format!("{}/", self.base)),
            ("Origin".to_string(), self.base.clone()),
            ("User-Agent".to_string(), super::feed::user_agent()),
        ];
        if let Some(ct) = content_type {
            h.push(("Content-Type".into(), ct.into()));
        }
        if let Some(sid) = self.sid.lock().unwrap().as_ref() {
            h.push(("Cookie".into(), format!("SID={sid}")));
        }
        h
    }

    fn raw(&self, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<net::Response> {
        let url = format!("{}/api/v2/{path}", self.base);
        let (ct, bytes) = match &body {
            Some((ct, b)) => (Some(*ct), Some(b.as_slice())),
            None => (None, None),
        };
        net::request(method, &url, bytes, &self.headers(ct)).map_err(QbitError::Other)
    }

    fn login(&self) -> Result<()> {
        let (Some(user), Some(pass)) = (&self.username, &self.password) else {
            return Err(QbitError::AuthRequired("no credentials are configured".into()));
        };
        let form = format!("username={}&password={}", url_encode(user), url_encode(pass));
        let r = self.raw("POST", "auth/login", Some(("application/x-www-form-urlencoded", form.into_bytes())))?;
        let ok = r.status == 200 && String::from_utf8_lossy(&r.body).trim() == "Ok.";
        if !ok {
            return Err(QbitError::AuthRequired("the user name or password is wrong".into()));
        }
        let sid = r
            .headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, v)| v.split(';').next()?.strip_prefix("SID=").map(str::to_string));
        *self.sid.lock().unwrap() = sid;
        Ok(())
    }

    /// One call with the error classification the UI needs.
    fn call(&self, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<net::Response> {
        if !self.is_listening() {
            return Err(QbitError::NotRunning(self.port));
        }
        let mut r = self.raw(method, path, body.clone())?;
        if r.status == 403 && self.username.is_some() && self.password.is_some() {
            self.login()?;
            r = self.raw(method, path, body)?;
        }
        match r.status {
            200..=299 => Ok(r),
            403 => Err(QbitError::AuthRequired("it answered 403 Forbidden".into())),
            401 => Err(QbitError::Forbidden("401 Unauthorized, usually the host header or CSRF check".into())),
            s => Err(QbitError::Other(format!("qBittorrent answered HTTP {s} for {path}: {}", String::from_utf8_lossy(&r.body).trim()))),
        }
    }

    fn form(&self, path: &str, fields: &[(&str, String)]) -> Result<net::Response> {
        let body = fields.iter().map(|(k, v)| format!("{k}={}", url_encode(v))).collect::<Vec<_>>().join("&");
        self.call("POST", path, Some(("application/x-www-form-urlencoded", body.into_bytes())))
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let r = self.call("GET", path, None)?;
        serde_json::from_slice(&r.body).map_err(|e| QbitError::Other(format!("unexpected answer from qBittorrent ({path}): {e}")))
    }

    // ---- read-only ---------------------------------------------------------

    pub fn version(&self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.call("GET", "app/version", None)?.body).trim().to_string())
    }

    /// `(major, minor)` of the Web API, e.g. `(2, 15)`.
    pub fn webapi_version(&self) -> Result<(u32, u32)> {
        let text = String::from_utf8_lossy(&self.call("GET", "app/webapiVersion", None)?.body).trim().to_string();
        let mut parts = text.split('.').filter_map(|p| p.parse().ok());
        Ok((parts.next().unwrap_or(2), parts.next().unwrap_or(0)))
    }

    /// Our torrents, and only ours.
    pub fn list(&self) -> Result<Vec<TorrentStatus>> {
        self.json(&format!("torrents/info?category={}", url_encode(CATEGORY)))
    }

    pub fn files(&self, hash: &str) -> Result<Vec<FileStatus>> {
        let h = check_hashes(&[hash.to_string()])?;
        self.json(&format!("torrents/files?hash={h}"))
    }

    // ---- writes, all scoped ------------------------------------------------

    /// Creates our category with its own save path if it does not exist; an existing one is left alone.
    pub fn ensure_category(&self, save_path: &str) -> Result<()> {
        let cats: serde_json::Value = self.json("torrents/categories")?;
        if cats.get(CATEGORY).is_some() {
            return Ok(());
        }
        self.form("torrents/createCategory", &[("category", CATEGORY.into()), ("savePath", save_path.into())])?;
        Ok(())
    }

    /// Adds a `.torrent` paused, in our category with our tag, saved to `save_path` without an extra
    /// top-level folder. `skip_checking` is for tests and for data that is already on disk.
    pub fn add(&self, torrent: &[u8], file_name: &str, save_path: &str, skip_checking: bool) -> Result<()> {
        let boundary = format!("----mangareader{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let mut body: Vec<u8> = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").bytes());
        };
        field("category", CATEGORY);
        field("tags", TAG);
        field("savepath", save_path);
        field("autoTMM", "false");
        field("contentLayout", "NoSubfolder");
        field("paused", "true"); // WebAPI < 2.11
        field("stopped", "true"); // WebAPI >= 2.11
        field("skip_checking", if skip_checking { "true" } else { "false" });
        let safe_name: String = file_name.chars().filter(|c| c.is_ascii_alphanumeric() || " ._-()[]".contains(*c)).collect();
        body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"{safe_name}.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").bytes());
        body.extend(torrent);
        body.extend(format!("\r\n--{boundary}--\r\n").bytes());
        let r = self.call("POST", "torrents/add", Some((&format!("multipart/form-data; boundary={boundary}"), body)))?;
        if String::from_utf8_lossy(&r.body).contains("Fails") {
            return Err(QbitError::Other("qBittorrent did not accept the torrent (already added, or invalid)".into()));
        }
        Ok(())
    }

    /// File priorities: 0 = do not download, 1 = normal.
    pub fn set_file_priority(&self, hash: &str, file_ids: &[usize], priority: u8) -> Result<()> {
        let h = check_hashes(&[hash.to_string()])?;
        if file_ids.is_empty() {
            return Ok(());
        }
        let ids = file_ids.iter().map(usize::to_string).collect::<Vec<_>>().join("|");
        self.form("torrents/filePrio", &[("hash", h), ("id", ids), ("priority", priority.to_string())])?;
        Ok(())
    }

    pub fn stop(&self, hashes: &[String]) -> Result<()> {
        let h = check_hashes(hashes)?;
        let path = if self.webapi_version()? >= (2, 11) { "torrents/stop" } else { "torrents/pause" };
        self.form(path, &[("hashes", h)])?;
        Ok(())
    }

    pub fn start(&self, hashes: &[String]) -> Result<()> {
        let h = check_hashes(hashes)?;
        let path = if self.webapi_version()? >= (2, 11) { "torrents/start" } else { "torrents/resume" };
        self.form(path, &[("hashes", h)])?;
        Ok(())
    }

    /// Stop seeding after `ratio` or `minutes`, whichever comes first (only for the given torrents).
    /// WebAPI 2.15 requires `shareLimitAction`; `Stop` pauses the torrent, it never deletes anything.
    pub fn set_share_limits(&self, hashes: &[String], ratio: f64, minutes: i64) -> Result<()> {
        let h = check_hashes(hashes)?;
        self.form(
            "torrents/setShareLimits",
            &[("hashes", h), ("ratioLimit", ratio.to_string()), ("seedingTimeLimit", minutes.to_string()), ("inactiveSeedingTimeLimit", "-2".into()), ("shareLimitAction", "Stop".into())],
        )?;
        Ok(())
    }

    pub fn delete(&self, hash: &str, delete_files: bool) -> Result<()> {
        let h = check_hashes(&[hash.to_string()])?;
        self.form("torrents/delete", &[("hashes", h), ("deleteFiles", delete_files.to_string())])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_validated_and_all_is_refused() {
        let good = "a".repeat(40);
        assert_eq!(check_hashes(&[good.clone(), "B".repeat(40)]).unwrap(), format!("{good}|{}", "b".repeat(40)));
        assert!(check_hashes(&["all".to_string()]).is_err());
        assert!(check_hashes(&[]).is_err());
        assert!(check_hashes(&["abc".to_string()]).is_err());
        assert!(check_hashes(&["g".repeat(40)]).is_err());
        assert!(check_hashes(&[good, "all".to_string()]).is_err(), "one bad hash spoils the batch");
        assert!(check_hashes(&["a".repeat(64)]).is_ok());
    }

    #[test]
    fn state_helpers_cover_old_and_new_names() {
        let s = |state: &str| TorrentStatus { state: state.into(), ..Default::default() };
        assert!(s("pausedUP").is_stopped() && s("stoppedDL").is_stopped());
        assert!(!s("downloading").is_stopped() && !s("stalledUP").is_stopped());
        assert!(s("missingFiles").is_error());
    }

    #[test]
    fn closed_port_is_a_clear_not_running_error() {
        // Port 1 is never open for us.
        let c = Client::new(1, None, None);
        let e = c.version().unwrap_err();
        assert_eq!(e, QbitError::NotRunning(1));
        let msg = e.to_string();
        assert!(msg.contains("not running") && msg.contains("Web UI") && msg.contains("Bypass authentication"));
    }

    // ---- integration test against a throw-away qBittorrent --------------------------------
    //
    // Needs an isolated instance (own profile directory and port, never the user's one):
    //   tools/qbt-test-instance.sh start      # prints the port and directory
    //   QBT_TEST_PORT=18080 QBT_TEST_DIR=<dir> cargo test --release qbit_isolated -- --ignored
    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut v = format!("{}:", b.len()).into_bytes();
        v.extend(b);
        v
    }

    /// A single-file torrent with all-zero piece hashes: with `skip_checking` qBittorrent trusts
    /// data that already exists, so it completes instantly without any network.
    fn make_torrent(name: &str, len: usize) -> Vec<u8> {
        let mut t = b"d4:infod6:lengthi".to_vec();
        t.extend(len.to_string().bytes());
        t.extend(b"e4:name");
        t.extend(bstr(name.as_bytes()));
        t.extend(b"12:piece lengthi16384e6:pieces");
        t.extend(bstr(&vec![0u8; 20 * len.div_ceil(16384)]));
        t.extend(b"ee");
        t
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..50 {
            if let Some(v) = f() {
                return v;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("timed out waiting for qBittorrent");
    }

    #[test]
    #[ignore = "needs an isolated qBittorrent instance (see tools/qbt-test-instance.sh)"]
    fn qbit_isolated() {
        let port: u16 = std::env::var("QBT_TEST_PORT").expect("QBT_TEST_PORT").parse().unwrap();
        let dir = std::path::PathBuf::from(std::env::var("QBT_TEST_DIR").expect("QBT_TEST_DIR"));
        let save = dir.join("dl");
        std::fs::create_dir_all(&save).unwrap();
        let c = Client::new(port, None, None);

        assert!(c.version().unwrap().starts_with('v'));
        assert!(c.webapi_version().unwrap() >= (2, 11));
        c.ensure_category(&save.to_string_lossy()).unwrap();
        c.ensure_category("/somewhere/else").unwrap(); // idempotent, does not edit the existing category
        for leftover in c.list().unwrap() {
            c.delete(&leftover.hash, false).unwrap(); // from an earlier failed run of this throw-away instance
        }

        // Ours: data already on disk -> completes immediately.
        let ours = format!("IsoTest Ours {}.cbz", std::process::id());
        std::fs::write(save.join(&ours), vec![7u8; 1000]).unwrap();
        c.add(&make_torrent(&ours, 1000), &ours, &save.to_string_lossy(), true).unwrap();
        // (it first shows up in a transient "checking" state)
        let mine = wait_for(|| c.list().unwrap().into_iter().find(|t| t.name == ours && !t.state.starts_with("checking")));
        assert_eq!(mine.category, CATEGORY);
        assert!(mine.is_stopped(), "added paused, state {}", mine.state);

        // Someone else's torrent in another category, running. It must never be touched.
        let foreign = format!("IsoTest Foreign {}.cbz", std::process::id());
        std::fs::write(save.join(&foreign), vec![9u8; 1000]).unwrap();
        let boundary = "----foreign";
        let mut body = Vec::new();
        for (k, v) in [("category", "foreign-test"), ("savepath", save.to_str().unwrap()), ("skip_checking", "true"), ("paused", "false"), ("stopped", "false")] {
            body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").bytes());
        }
        body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"f.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").bytes());
        body.extend(make_torrent(&foreign, 1000));
        body.extend(format!("\r\n--{boundary}--\r\n").bytes());
        c.call("POST", "torrents/add", Some((&format!("multipart/form-data; boundary={boundary}"), body))).unwrap();
        let foreign_info = |c: &Client| -> Vec<TorrentStatus> { c.json("torrents/info?category=foreign-test").unwrap() };
        let before = wait_for(|| foreign_info(&c).into_iter().find(|t| t.name == foreign && !t.state.starts_with("checking")));
        assert!(!before.is_stopped(), "the foreign torrent starts running (state {})", before.state);

        // The list only ever shows our category.
        assert!(c.list().unwrap().iter().all(|t| t.category == CATEGORY && t.name != foreign));

        // Start, stop, limits, files: only our hash is passed.
        c.start(&[mine.hash.clone()]).unwrap();
        wait_for(|| c.list().unwrap().into_iter().find(|t| t.hash == mine.hash && !t.is_stopped()));
        c.set_share_limits(&[mine.hash.clone()], 1.0, 1440).unwrap();
        c.stop(&[mine.hash.clone()]).unwrap();
        wait_for(|| c.list().unwrap().into_iter().find(|t| t.hash == mine.hash && t.is_stopped()));
        let files = c.files(&mine.hash).unwrap();
        assert_eq!(files.len(), 1);
        c.set_file_priority(&mine.hash, &[0], 0).unwrap();
        assert_eq!(c.files(&mine.hash).unwrap()[0].priority, 0);

        // The foreign torrent was not stopped, restarted or changed by any of that.
        let after = foreign_info(&c).into_iter().find(|t| t.name == foreign).unwrap();
        assert!(!after.is_stopped(), "foreign torrent must be untouched, state {}", after.state);
        assert_eq!(after.category, "foreign-test");

        // `all` and garbage hashes never reach qBittorrent.
        assert!(c.stop(&["all".to_string()]).is_err());
        assert!(c.delete("all", true).is_err());

        // Clean up: our torrent through the client, the foreign one directly.
        c.delete(&mine.hash, false).unwrap();
        wait_for(|| c.list().unwrap().iter().all(|t| t.hash != mine.hash).then_some(()));
        c.form("torrents/delete", &[("hashes", before.hash.clone()), ("deleteFiles", "false".into())]).unwrap();
    }
}
