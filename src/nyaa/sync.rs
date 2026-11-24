//! The Nyaa pipeline: feeds -> releases -> links -> qBittorrent -> volumes on disk.
//! Blocking; run it on a background thread. Every step is idempotent, so a cycle that is
//! interrupted (qBittorrent closed, no network) simply continues at the next one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::feed::{self, Fetched, Item};
use super::matcher;
use super::qbit::{Client, QbitError};
use super::rules::{Parsed, Rules, Skip};
use super::store::{self, Link, Release};
use super::torrent;
use crate::db::Db;

/// Free space that must remain after a download.
const DISK_MARGIN: u64 = 1024 * 1024 * 1024;

/// All user-adjustable values with their defaults (PLAN.md → Open decisions, answered).
#[derive(Clone, Debug)]
pub struct Settings {
    pub category: String,
    pub root: PathBuf,
    /// Releases whose wanted files exceed this wait for a manual "Download".
    pub size_cap: u64,
    pub qbit_port: u16,
    pub qbit_user: Option<String>,
    pub seed_ratio: f64,
    pub seed_minutes: i64,
    pub interval_min: u32,
}

impl Settings {
    pub fn defaults() -> Self {
        Self {
            category: super::DEFAULT_CATEGORY.to_string(),
            root: crate::paths::manga_root().join("Nyaa"),
            size_cap: 4 * 1024 * 1024 * 1024,
            qbit_port: super::qbit::DEFAULT_PORT,
            qbit_user: None,
            seed_ratio: 1.0,
            seed_minutes: 24 * 60,
            interval_min: 60,
        }
    }

    pub fn load(db: &Db) -> Self {
        let mut s = Self::defaults();
        let get = |k: &str| db.setting(k).filter(|v| !v.trim().is_empty());
        if let Some(v) = get("nyaa_category") {
            s.category = v;
        }
        if let Some(v) = get("nyaa_root") {
            s.root = PathBuf::from(v);
        }
        if let Some(v) = get("nyaa_size_cap_mb").and_then(|v| v.parse::<u64>().ok()) {
            s.size_cap = v * 1024 * 1024;
        }
        if let Some(v) = get("qbit_port").and_then(|v| v.parse().ok()) {
            s.qbit_port = v;
        }
        s.qbit_user = get("qbit_user");
        if let Some(v) = get("nyaa_seed_ratio").and_then(|v| v.parse().ok()) {
            s.seed_ratio = v;
        }
        if let Some(v) = get("nyaa_seed_minutes").and_then(|v| v.parse().ok()) {
            s.seed_minutes = v;
        }
        // Nyaa caches feeds for 5 minutes; asking more often than every 15 is pointless.
        s.interval_min = get("nyaa_interval_min").and_then(|v| v.parse::<u32>().ok()).unwrap_or(60).max(15);
        s
    }
}

/// Where feeds and `.torrent` files come from (real Nyaa, or a fake in tests).
pub trait Fetcher: Send + Sync {
    fn feed(&self, uploader: &str, category: &str, query: Option<&str>, since: Option<i64>) -> Result<Fetched, String>;
    fn torrent(&self, link: &str) -> Result<Vec<u8>, String>;
}

pub struct NyaaFetcher;

impl Fetcher for NyaaFetcher {
    fn feed(&self, uploader: &str, category: &str, query: Option<&str>, since: Option<i64>) -> Result<Fetched, String> {
        feed::fetch(&feed::feed_url(uploader, category, query), since)
    }

    fn torrent(&self, link: &str) -> Result<Vec<u8>, String> {
        // The link comes from a feed: only ever fetch Nyaa's own download URLs.
        if !(link.starts_with("https://nyaa.si/download/") && link.ends_with(".torrent")) {
            return Err(format!("refusing to download {link}: not a nyaa.si torrent link"));
        }
        crate::net::get_bytes(link, &[("User-Agent".to_string(), feed::user_agent())])
    }
}

pub struct Ctx<'a> {
    pub db: &'a Db,
    pub rules: &'a Rules,
    pub settings: &'a Settings,
    pub fetcher: &'a dyn Fetcher,
    pub qbit: &'a Client,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub feeds_fetched: usize,
    pub new_releases: usize,
    pub matched: usize,
    pub queued: usize,
    pub completed: usize,
    /// Problems with one feed or release; the cycle carried on.
    pub errors: Vec<String>,
    /// qBittorrent could not be reached: queued work waits and is retried next cycle.
    pub qbit_error: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Free bytes on the file system holding `path` (or its nearest existing parent).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let c = std::ffi::CString::new(p.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized, writable out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut st) == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
}

pub fn sanitize_folder(title: &str) -> String {
    let s: String = title.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { ' ' } else { c }).collect();
    // Words made only of dots ("..") would be traversal tokens in a path; drop them.
    let s = s.split_whitespace().filter(|w| w.chars().any(|c| c != '.')).collect::<Vec<_>>().join(" ");
    let s = s.trim_matches('.').trim();
    let s: String = s.chars().take(120).collect();
    if s.is_empty() { "Untitled".into() } else { s }
}

/// Stores new items of a feed; classification only, nothing is downloaded here.
fn ingest(ctx: &Ctx, uploader: &str, items: &[Item]) -> usize {
    let mut new = 0;
    for item in items {
        let (state, note) = match ctx.rules.parse(uploader, &item.title) {
            Ok(_) => (store::SEEN, String::new()),
            Err(Skip::Extension) => (store::SKIPPED, "not an archive".to_string()),
            Err(Skip::Rejected) => (store::SKIPPED, "not manga (audiobook, EPUB, pack, JPEG XL)".to_string()),
            Err(Skip::Unparsed) => (store::UNPARSED, "title not understood by the rules".to_string()),
        };
        let release = Release {
            info_hash: item.info_hash.clone(),
            uploader: uploader.to_string(),
            title: item.title.clone(),
            size: item.size_bytes as i64,
            pub_date: item.pub_date.clone(),
            link: item.link.clone(),
            state: state.to_string(),
            note,
            ..Default::default()
        };
        if ctx.db.nyaa_insert_release(release) {
            new += 1;
        }
    }
    new
}

/// Seen releases that belong to a link become `matched` (with the volumes they would add) or
/// `skipped` (nothing new, or another variant of the same release is preferred).
pub fn match_pending(ctx: &Ctx) -> usize {
    let links = ctx.db.nyaa_links();
    if links.is_empty() {
        return 0;
    }
    let seen = ctx.db.nyaa_releases_in_states(&[store::SEEN]);
    let mut matched = 0;
    for link in &links {
        let owned = ctx.db.nyaa_owned_volumes(link.id);
        let queued = ctx.db.nyaa_queued_volumes(link.id, "");
        let candidates: Vec<(usize, &Release, Parsed)> = seen
            .iter()
            .enumerate()
            .filter(|(_, r)| r.uploader == link.uploader)
            .filter_map(|(i, r)| {
                let p = ctx.rules.parse(&r.uploader, &r.title).ok()?;
                matcher::matches_link(&p, &r.title, &link.query, &link.exclude).then_some((i, r, p))
            })
            .collect();
        let refs: Vec<(usize, &Parsed)> = candidates.iter().map(|(i, _, p)| (*i, p)).collect();
        let keep = matcher::pick_variants(ctx.rules, &link.uploader, &refs);
        // Newest first would be natural, but older ones must not be starved: process oldest first.
        let mut claimed: HashSet<u32> = queued.clone();
        for (i, r, p) in &candidates {
            if !keep.contains(i) {
                ctx.db.nyaa_set_release(&r.info_hash, store::SKIPPED, Some(link.id), None, "another variant of this release is preferred");
                continue;
            }
            let fresh = matcher::new_volumes(p, &owned, &claimed);
            if fresh.is_empty() {
                ctx.db.nyaa_set_release(&r.info_hash, store::SKIPPED, Some(link.id), None, "nothing new for this series");
            } else {
                claimed.extend(fresh.iter().copied());
                ctx.db.nyaa_set_release(&r.info_hash, store::MATCHED, Some(link.id), Some(&store::join_volumes(&fresh)), "");
                matched += 1;
            }
        }
    }
    matched
}

/// Reads archives already in the link's folder, so files added by hand count as owned.
pub fn rescan_folder(ctx: &Ctx, link: &Link) {
    let Ok(rd) = std::fs::read_dir(&link.folder) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(p) = ctx.rules.parse(&link.uploader, &name) else { continue };
        if !matcher::matches_link(&p, &name, &link.query, &link.exclude) {
            continue;
        }
        for v in matcher::volumes_of(&p) {
            ctx.db.nyaa_add_volume(link.id, v, &entry.path().to_string_lossy(), "");
        }
    }
}

/// Outcome of [`queue_release`].
#[derive(Debug, PartialEq, Eq)]
pub enum Queued {
    Added,
    NothingNew,
    /// Over the size cap; waits for `force`.
    NeedsConfirmation(u64),
}

/// Downloads the `.torrent`, picks the wanted files and hands it to qBittorrent (paused while the
/// file priorities are set, then started). `force` ignores the size cap (manual "Download").
pub fn queue_release(ctx: &Ctx, hash: &str, force: bool) -> Result<Queued, String> {
    let rel = ctx.db.nyaa_release(hash).ok_or("unknown release")?;
    let link = rel.link_id.and_then(|id| ctx.db.nyaa_link(id)).ok_or("this release is not linked to a series")?;
    let bytes = ctx.fetcher.torrent(&rel.link)?;
    let real_hash = torrent::info_hash(&bytes)?;
    if real_hash != rel.info_hash {
        return Err(format!("the .torrent's hash {real_hash} differs from the feed's {}: refusing it", rel.info_hash));
    }
    let info = torrent::read(&bytes)?;
    let owned = ctx.db.nyaa_owned_volumes(link.id);
    let queued = ctx.db.nyaa_queued_volumes(link.id, &rel.info_hash);
    let selected = matcher::select_files(ctx.rules, &link.uploader, &link.query, &link.exclude, &info, &owned, &queued);
    if selected.is_empty() {
        ctx.db.nyaa_set_release(hash, store::SKIPPED, None, None, "nothing new in this torrent");
        return Ok(Queued::NothingNew);
    }
    let wanted: u64 = selected.iter().map(|(i, _)| info.files[*i].size).sum();
    if wanted > ctx.settings.size_cap && !force {
        let note = format!("{} to download, over the {} cap: press Download to confirm", human(wanted), human(ctx.settings.size_cap));
        ctx.db.nyaa_set_release(hash, store::CONFIRM, None, None, &note);
        return Ok(Queued::NeedsConfirmation(wanted));
    }
    let folder = PathBuf::from(&link.folder);
    match free_bytes(&folder) {
        Some(free) if free < wanted + DISK_MARGIN => {
            return Err(format!("not enough disk space: {} needed, {} free", human(wanted + DISK_MARGIN), human(free)));
        }
        _ => {}
    }
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;

    let qbit_err = |e: QbitError| e.to_string();
    ctx.qbit.ensure_category(&ctx.settings.root.to_string_lossy()).map_err(qbit_err)?;
    // A duplicate ("Fails.") means it is already in qBittorrent: carry on with it.
    let already = ctx.qbit.list().map_err(qbit_err)?.iter().any(|t| t.hash == real_hash);
    if !already {
        ctx.qbit.add(&bytes, &rel.title, &folder.to_string_lossy(), false).map_err(qbit_err)?;
    }
    // The torrent shows up a moment after the call; wait for it before touching priorities.
    let mut found = false;
    for _ in 0..25 {
        if ctx.qbit.list().map_err(qbit_err)?.iter().any(|t| t.hash == real_hash) {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !found {
        return Err("qBittorrent did not list the torrent after adding it".into());
    }
    let wanted_idx: HashSet<usize> = selected.iter().map(|(i, _)| *i).collect();
    let skip: Vec<usize> = (0..info.files.len()).filter(|i| !wanted_idx.contains(i)).collect();
    // qBittorrent needs a moment to load the file list of a fresh torrent.
    let mut prio_ok = false;
    for _ in 0..25 {
        if ctx.qbit.files(&real_hash).map(|f| f.len() == info.files.len()).unwrap_or(false) {
            prio_ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !prio_ok {
        return Err("qBittorrent did not report the torrent's files".into());
    }
    ctx.qbit.set_file_priority(&real_hash, &skip, 0).map_err(qbit_err)?;
    ctx.qbit.set_share_limits(&[real_hash.clone()], ctx.settings.seed_ratio, ctx.settings.seed_minutes).map_err(qbit_err)?;
    ctx.qbit.start(&[real_hash.clone()]).map_err(qbit_err)?;
    let volumes: Vec<u32> = selected.iter().flat_map(|(_, v)| v.iter().copied()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    ctx.db.nyaa_set_release(hash, store::DOWNLOADING, None, Some(&store::join_volumes(&volumes)), "");
    Ok(Queued::Added)
}

fn human(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes as f64 >= G { format!("{:.1} GiB", bytes as f64 / G) } else { format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}

/// Queues every matched release of an auto-download link. Returns how many were added; a qBittorrent
/// error stops the loop (nothing else could succeed) and is returned for the report.
fn process_matched(ctx: &Ctx, report: &mut Report) {
    for rel in ctx.db.nyaa_releases_in_states(&[store::MATCHED]) {
        let Some(link) = rel.link_id.and_then(|id| ctx.db.nyaa_link(id)) else { continue };
        if !link.auto {
            continue;
        }
        match queue_release(ctx, &rel.info_hash, false) {
            Ok(Queued::Added) => report.queued += 1,
            Ok(_) => {}
            Err(e) if e.contains("qBittorrent is not running") || e.contains("asks for a login") || e.contains("rejected the request") => {
                report.qbit_error = Some(e);
                return;
            }
            Err(e) => {
                // Keep it `matched` with the reason, so the Releases page shows why and the next cycle retries.
                ctx.db.nyaa_set_release(&rel.info_hash, store::MATCHED, None, None, &e);
                report.errors.push(format!("{}: {e}", rel.title));
            }
        }
    }
}

/// Moves finished downloads to `done` and records their volume files.
pub fn poll_downloads(ctx: &Ctx, report: &mut Report) {
    let downloading = ctx.db.nyaa_releases_in_states(&[store::DOWNLOADING]);
    if downloading.is_empty() {
        return;
    }
    let statuses = match ctx.qbit.list() {
        Ok(s) => s,
        Err(e) => {
            report.qbit_error = Some(e.to_string());
            return;
        }
    };
    for rel in downloading {
        let Some(status) = statuses.iter().find(|t| t.hash == rel.info_hash) else {
            ctx.db.nyaa_set_release(&rel.info_hash, store::FAILED, None, None, "removed from qBittorrent before it finished");
            continue;
        };
        if status.is_error() {
            ctx.db.nyaa_set_release(&rel.info_hash, store::FAILED, None, None, &format!("qBittorrent reports {}", status.state));
            continue;
        }
        if !status.is_complete() {
            continue;
        }
        let (Some(link), Ok(files)) = (rel.link_id.and_then(|id| ctx.db.nyaa_link(id)), ctx.qbit.files(&rel.info_hash)) else { continue };
        for f in files.iter().filter(|f| f.priority > 0 && f.progress >= 1.0) {
            let file_name = f.name.rsplit('/').next().unwrap_or(&f.name);
            let Ok(p) = ctx.rules.parse(&link.uploader, file_name) else { continue };
            let path = Path::new(&status.save_path).join(&f.name);
            for v in matcher::volumes_of(&p) {
                ctx.db.nyaa_add_volume(link.id, v, &path.to_string_lossy(), &rel.info_hash);
            }
        }
        ctx.db.nyaa_set_release(&rel.info_hash, store::DONE, None, None, "");
        report.completed += 1;
    }
}

/// One full cycle: check the followed uploaders' feeds (conditional requests), match, queue, poll.
/// `force` ignores `If-Modified-Since`.
pub fn run_cycle(ctx: &Ctx, force: bool) -> Report {
    let mut report = Report::default();
    ctx.db.nyaa_seed_uploaders(&super::DEFAULT_UPLOADERS);
    for (n, up) in ctx.db.nyaa_uploaders().into_iter().filter(|u| u.enabled).enumerate() {
        if n > 0 {
            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) { 0 } else { 2000 })); // be polite to Nyaa
        }
        let since = (!force && up.last_checked > 0).then_some(up.last_checked);
        match ctx.fetcher.feed(&up.name, &ctx.settings.category, None, since) {
            Ok(Fetched::NotModified) => ctx.db.nyaa_touch_uploader(&up.name),
            Ok(Fetched::Items(items)) => {
                report.feeds_fetched += 1;
                report.new_releases += ingest(ctx, &up.name, &items);
                ctx.db.nyaa_touch_uploader(&up.name);
            }
            Err(e) => report.errors.push(format!("{}: {e}", up.name)),
        }
    }
    for link in ctx.db.nyaa_links() {
        rescan_folder(ctx, &link);
    }
    report.matched = match_pending(ctx);
    process_matched(ctx, &mut report);
    poll_downloads(ctx, &mut report);
    report
}

/// Links a library series to an uploader and a search phrase: stores the link, asks Nyaa once for
/// that phrase (so back-catalogue volumes outside the 75-item feed are found), and matches.
/// Nothing is downloaded yet; the next cycle (or "Check now") does that.
pub fn link_series(ctx: &Ctx, source: &str, manga_id: &str, title: &str, uploader: &str, query: &str, auto: bool) -> Result<Link, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("the search phrase is empty".into());
    }
    let folder = ctx.settings.root.join(sanitize_folder(title));
    let id = ctx
        .db
        .nyaa_add_link(Link { id: 0, source: source.into(), manga_id: manga_id.into(), title: title.into(), uploader: uploader.into(), query: query.into(), auto, exclude: String::new(), folder: folder.to_string_lossy().into_owned() })
        .ok_or("could not store the link")?;
    let link = ctx.db.nyaa_link(id).ok_or("could not read the link back")?;
    match ctx.fetcher.feed(uploader, &ctx.settings.category, Some(query), None)? {
        Fetched::Items(items) => {
            ingest(ctx, uploader, &items);
        }
        Fetched::NotModified => {}
    }
    rescan_folder(ctx, &link);
    match_pending(ctx);
    Ok(link)
}

/// For the "Check now" button: unix time of the last cycle is kept per uploader; this is for logs.
pub fn timestamp() -> i64 {
    now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut v = format!("{}:", b.len()).into_bytes();
        v.extend(b);
        v
    }

    /// A folder torrent with all-zero piece hashes (see qbit tests for why that is enough).
    fn make_torrent(name: &str, files: &[(&str, usize)]) -> Vec<u8> {
        let total: usize = files.iter().map(|f| f.1).sum();
        let mut t = b"d4:infod5:filesl".to_vec();
        for (path, len) in files {
            t.extend(format!("d6:lengthi{len}e4:pathl").bytes());
            t.extend(bstr(path.as_bytes()));
            t.extend(b"ee");
        }
        t.extend(b"e4:name");
        t.extend(bstr(name.as_bytes()));
        t.extend(b"12:piece lengthi16384e6:pieces");
        t.extend(bstr(&vec![0u8; 20 * total.div_ceil(16384)]));
        t.extend(b"ee");
        t
    }

    #[derive(Default)]
    struct Fake {
        feeds: Mutex<HashMap<String, Vec<Item>>>,
        torrents: Mutex<HashMap<String, Vec<u8>>>,
        since_seen: Mutex<Vec<(String, Option<i64>)>>,
    }

    impl Fake {
        fn publish(&self, uploader: &str, title: &str, torrent: Vec<u8>, size: u64) -> String {
            let hash = torrent::info_hash(&torrent).unwrap();
            let link = format!("https://nyaa.si/download/{hash}.torrent");
            self.torrents.lock().unwrap().insert(link.clone(), torrent);
            self.feeds.lock().unwrap().entry(uploader.into()).or_default().insert(
                0,
                Item { title: title.into(), link, info_hash: hash.clone(), size_bytes: size, category_id: "3_1".into(), ..Default::default() },
            );
            hash
        }
    }

    impl Fetcher for Fake {
        fn feed(&self, uploader: &str, _c: &str, _q: Option<&str>, since: Option<i64>) -> Result<Fetched, String> {
            self.since_seen.lock().unwrap().push((uploader.into(), since));
            Ok(Fetched::Items(self.feeds.lock().unwrap().get(uploader).cloned().unwrap_or_default()))
        }
        fn torrent(&self, link: &str) -> Result<Vec<u8>, String> {
            self.torrents.lock().unwrap().get(link).cloned().ok_or_else(|| "404".to_string())
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("manga-reader-test-{tag}-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn settings(root: &Path, port: u16) -> Settings {
        Settings { root: root.to_path_buf(), qbit_port: port, ..Settings::defaults() }
    }

    const FILES: [(&str, usize); 3] = [
        ("IsoSeries v01 (2026) (Digital) (UploaderA).cbz", 1000),
        ("IsoSeries v02 (2026) (Digital) (UploaderA).cbz", 1000),
        ("IsoSeries v03 (2026) (Digital) (UploaderA).epub", 1000),
    ];

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(sanitize_folder("Re:Zero / Season 2?"), "Re Zero Season 2");
        assert_eq!(sanitize_folder("../../etc"), "etc");
        assert_eq!(sanitize_folder("  ...  "), "Untitled");
        assert!(sanitize_folder(&"x".repeat(500)).len() <= 120);
    }

    #[test]
    fn unreachable_qbittorrent_keeps_the_release_waiting_and_loses_nothing() {
        let dir = temp_dir("noqbit");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let rules = Rules::from_toml(include_str!("rules.toml"), None).unwrap();
        let st = settings(&dir.join("Nyaa"), 1); // nothing listens on port 1
        let fake = Fake::default();
        fake.publish("UploaderA", "IsoSeries v01-02 (2026) (Digital) (UploaderA)", make_torrent("IsoSeries", &FILES[..2]), 2000);
        fake.publish("UploaderA", "Unrelated Series v01 (2026) (Digital) (UploaderA)", make_torrent("Unrelated", &[("Unrelated Series v01 (2026) (Digital) (UploaderA).cbz", 10)]), 10);
        fake.publish("UploaderC", "Some Book v01 [Audiobook] [UpC]", make_torrent("Book", &[("book.m4b", 10)]), 10);
        let qb = Client::new(1, None, None);
        let ctx = Ctx { db: &db, rules: &rules, settings: &st, fetcher: &fake, qbit: &qb };

        link_series(&ctx, "ExampleSite", "42", "IsoSeries", "UploaderA", "isoseries", true).unwrap();
        let report = run_cycle(&ctx, true);
        // (the match itself already happened while linking, so the cycle reports 0 new matches)
        assert_eq!(report.queued, 0);
        assert!(report.qbit_error.as_deref().is_some_and(|e| e.contains("not running")), "{report:?}");
        let waiting = db.nyaa_releases_in_states(&[store::MATCHED]);
        assert_eq!(waiting.len(), 1, "the matching release waits for qBittorrent");
        assert_eq!(store::parse_volumes(&waiting[0].volumes), vec![1, 2]);
        assert_eq!(db.nyaa_releases_in_states(&[store::SKIPPED]).len(), 1, "the audiobook is skipped as not manga");
        assert_eq!(db.nyaa_releases_in_states(&[store::SEEN]).len(), 1, "the unrelated series stays merely seen");

        // A second cycle is idempotent and conditional: it sends If-Modified-Since now.
        let again = run_cycle(&ctx, false);
        assert_eq!(again.new_releases, 0);
        assert!(fake.since_seen.lock().unwrap().iter().any(|(u, s)| u == "UploaderA" && s.is_some()));
        assert_eq!(db.nyaa_releases_in_states(&[store::MATCHED]).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manual_files_in_the_folder_count_as_owned() {
        let dir = temp_dir("owned");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let rules = Rules::from_toml(include_str!("rules.toml"), None).unwrap();
        let st = settings(&dir.join("Nyaa"), 1);
        let fake = Fake::default();
        fake.publish("UploaderA", "IsoSeries v01-02 (2026) (Digital) (UploaderA)", make_torrent("IsoSeries", &FILES[..2]), 2000);
        let qb = Client::new(1, None, None);
        let ctx = Ctx { db: &db, rules: &rules, settings: &st, fetcher: &fake, qbit: &qb };
        let folder = st.root.join("IsoSeries");
        std::fs::create_dir_all(&folder).unwrap();
        for (name, _) in &FILES[..2] {
            std::fs::write(folder.join(name), b"x").unwrap();
        }
        link_series(&ctx, "ExampleSite", "42", "IsoSeries", "UploaderA", "isoseries", true).unwrap();
        let report = run_cycle(&ctx, true);
        assert_eq!(report.matched, 0, "both volumes are already on disk: {report:?}");
        assert_eq!(db.nyaa_releases_in_states(&[store::SKIPPED]).len(), 1);
        assert_eq!(db.nyaa_owned_volumes(1), [1u32, 2].into_iter().collect());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_torrent_hash_is_refused() {
        let dir = temp_dir("hash");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let rules = Rules::from_toml(include_str!("rules.toml"), None).unwrap();
        let st = settings(&dir.join("Nyaa"), 1);
        let fake = Fake::default();
        let hash = fake.publish("UploaderA", "IsoSeries v01-02 (2026) (Digital) (UploaderA)", make_torrent("IsoSeries", &FILES[..2]), 2000);
        // The server hands out a different torrent than the feed announced.
        let link = format!("https://nyaa.si/download/{hash}.torrent");
        fake.torrents.lock().unwrap().insert(link, make_torrent("Other", &[("IsoSeries v09 (2026) (Digital) (UploaderA).cbz", 5)]));
        let qb = Client::new(1, None, None);
        let ctx = Ctx { db: &db, rules: &rules, settings: &st, fetcher: &fake, qbit: &qb };
        link_series(&ctx, "ExampleSite", "42", "IsoSeries", "UploaderA", "isoseries", true).unwrap();
        run_cycle(&ctx, true);
        let err = queue_release(&ctx, &hash, false).unwrap_err();
        assert!(err.contains("differs"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_nyaa_links_only() {
        let f = NyaaFetcher;
        assert!(f.torrent("https://evil.example/download/1.torrent").is_err());
        assert!(f.torrent("https://nyaa.si/view/1").is_err());
        assert!(f.torrent("http://nyaa.si/download/1.torrent").is_err());
    }

    // ---- end to end against a throw-away qBittorrent --------------------------------------
    //   tools/qbt-test-instance.sh start
    //   QBT_TEST_PORT=18080 QBT_TEST_DIR=<dir> cargo test --release nyaa_end_to_end -- --ignored
    #[test]
    #[ignore = "needs an isolated qBittorrent instance (see tools/qbt-test-instance.sh)"]
    fn nyaa_end_to_end() {
        let port: u16 = std::env::var("QBT_TEST_PORT").expect("QBT_TEST_PORT").parse().unwrap();
        let qdir = PathBuf::from(std::env::var("QBT_TEST_DIR").expect("QBT_TEST_DIR"));
        let dir = temp_dir("e2e");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let rules = Rules::from_toml(include_str!("rules.toml"), None).unwrap();
        let st = settings(&qdir.join("dl").join("Nyaa"), port);
        let qb = Client::new(port, None, None);
        for leftover in qb.list().unwrap() {
            qb.delete(&leftover.hash, false).unwrap();
        }
        let unique = format!("IsoSeries{}", std::process::id());
        let file = |v: u32, ext: &str| format!("{unique} v{v:02} (2026) (Digital) (UploaderA).{ext}");
        let files: Vec<(String, usize)> = vec![(file(1, "cbz"), 1000), (file(2, "cbz"), 1000), (file(3, "cbz"), 1000), (file(4, "epub"), 1000)];
        let file_refs: Vec<(&str, usize)> = files.iter().map(|(n, l)| (n.as_str(), *l)).collect();
        let torrent = make_torrent(&unique, &file_refs);
        let fake = Fake::default();
        let hash = fake.publish("UploaderA", &format!("{unique} v01-04 (2026) (Digital) (UploaderA)"), torrent.clone(), 4000);
        let ctx = Ctx { db: &db, rules: &rules, settings: &st, fetcher: &fake, qbit: &qb };

        // Volume 1 is already in the series folder (added by hand). The torrent's data sits in a
        // separate staging folder that qBittorrent is told to trust, so it completes without any
        // network; the series folder must not already contain volumes 2-3 or they would count as owned.
        let folder = st.root.join(&unique);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(file(1, "cbz")), vec![5u8; 1000]).unwrap();
        let stage = qdir.join("dl").join(format!("stage-{unique}"));
        std::fs::create_dir_all(&stage).unwrap();
        for (name, _) in &files {
            std::fs::write(stage.join(name), vec![5u8; 1000]).unwrap();
        }
        qb.ensure_category(&st.root.to_string_lossy()).unwrap();
        qb.add(&torrent, &unique, &stage.to_string_lossy(), true).unwrap();

        link_series(&ctx, "ExampleSite", "42", &unique, "UploaderA", &unique.to_lowercase(), true).unwrap();
        // volume 1 is owned (found by the folder scan): only 2 and 3 are wanted; the epub never
        let report = run_cycle(&ctx, true);
        assert_eq!(report.queued, 1, "{report:?}");
        assert_eq!(report.errors, Vec::<String>::new());
        let rel = db.nyaa_release(&hash).unwrap();
        // (with the data already present, the same cycle's poll may already have completed it)
        assert!(rel.state == store::DOWNLOADING || rel.state == store::DONE, "{rel:?}");
        assert_eq!(store::parse_volumes(&rel.volumes), vec![2, 3]);

        // File priorities in the real qBittorrent: owned volume and epub off, volumes 2-3 on.
        let prios: Vec<(String, i64)> = qb.files(&hash).unwrap().into_iter().map(|f| (f.name, f.priority)).collect();
        assert_eq!(prios.iter().filter(|(_, p)| *p > 0).count(), 2, "{prios:?}");
        assert!(prios.iter().any(|(n, p)| n.contains("v01") && *p == 0));
        assert!(prios.iter().any(|(n, p)| n.ends_with(".epub") && *p == 0));

        // The data is already there, so the torrent completes; the next cycle records the volumes.
        let mut done = false;
        for _ in 0..30 {
            let r = run_cycle(&ctx, false);
            assert!(r.qbit_error.is_none(), "{r:?}");
            if db.nyaa_release(&hash).unwrap().state == store::DONE {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        assert!(done, "download did not complete: {:?}", db.nyaa_release(&hash));
        let owned = db.nyaa_owned_volumes(1);
        assert_eq!(owned, [1u32, 2, 3].into_iter().collect(), "volume 4 is an epub and must not appear");

        // A later release that only adds nothing new is skipped.
        let hash2 = fake.publish("UploaderA", &format!("{unique} v01-03 (2026) (Digital) (UploaderA)"), make_torrent(&format!("{unique}b"), &file_refs[..3]), 3000);
        run_cycle(&ctx, true);
        assert_eq!(db.nyaa_release(&hash2).unwrap().state, store::SKIPPED);

        qb.delete(&hash, false).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
