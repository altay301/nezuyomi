//! Nyaa torrent downloads for followed uploaders (PLAN.md → "Plan: Nyaa torrent downloads").

pub mod feed;
pub mod matcher;
pub mod power;
pub mod qbit;
pub mod rules;
pub mod secret;
pub mod service;
pub mod store;
pub mod sync;
pub mod torrent;

/// Uploaders followed out of the box (editable in Settings later).
pub const DEFAULT_UPLOADERS: [&str; 6] = ["UploaderA", "UploaderB", "UploaderC", "UploaderD", "UploaderE", "UploaderF"];
/// Literature - English-translated.
pub const DEFAULT_CATEGORY: &str = "3_1";

/// `nezuyomi --nyaa-probe [category]`: fetches the followed uploaders' feeds and reports how
/// the rules classify every item (no database, no qBittorrent, no downloads).
pub fn probe(args: &[String]) -> i32 {
    let category = args.first().map_or(DEFAULT_CATEGORY, String::as_str);
    let rules = rules::Rules::load();
    let mut total_unparsed = 0;
    for uploader in DEFAULT_UPLOADERS {
        let items = match feed::fetch(&feed::feed_url(uploader, category, None), None) {
            Ok(feed::Fetched::Items(i)) => i,
            Ok(feed::Fetched::NotModified) => Vec::new(),
            Err(e) => {
                eprintln!("{uploader}: {e}");
                return 1;
            }
        };
        let (mut ok, mut skipped, mut unparsed) = (0, 0, Vec::new());
        let mut examples = Vec::new();
        for item in &items {
            match rules.parse(uploader, &item.title) {
                Ok(p) => {
                    ok += 1;
                    if examples.len() < 2 {
                        examples.push(format!("{:?} v{:?} ch{:?} [{}] {:?}", p.title, p.volumes, p.chapters, p.variant, p.kind));
                    }
                }
                Err(rules::Skip::Unparsed) => unparsed.push(item.title.clone()),
                Err(_) => skipped += 1,
            }
        }
        println!("{uploader}: {} items -> {ok} parsed, {skipped} skipped (not manga), {} unparsed", items.len(), unparsed.len());
        for e in examples {
            println!("    e.g. {e}");
        }
        for u in &unparsed {
            println!("    UNPARSED: {u}");
        }
        total_unparsed += unparsed.len();
        std::thread::sleep(std::time::Duration::from_secs(2)); // be polite
    }
    if total_unparsed == 0 { 0 } else { 3 }
}
