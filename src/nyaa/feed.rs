//! Nyaa RSS: URL building, conditional fetch and parsing. RSS only; the site itself is
//! never browsed. The server caches feeds for 5 minutes and answers `If-Modified-Since`
//! with `304`, so polling more often than every 15 minutes is pointless.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::net;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    /// The `.torrent` download URL.
    pub link: String,
    pub guid: String,
    pub pub_date: String,
    pub info_hash: String,
    pub size_bytes: u64,
    pub seeders: u32,
    pub category_id: String,
}

pub enum Fetched {
    NotModified,
    Items(Vec<Item>),
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `category` is a Nyaa category id such as `3_1` (Literature - English-translated) or `3_0` (all Literature).
pub fn feed_url(uploader: &str, category: &str, query: Option<&str>) -> String {
    let mut url = format!("https://nyaa.si/?page=rss&u={}&c={}", url_encode(uploader), url_encode(category));
    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        url.push_str(&format!("&q={}", url_encode(q.trim())));
    }
    url
}

pub fn user_agent() -> String {
    format!("nezuyomi/{}", env!("CARGO_PKG_VERSION"))
}

/// Fetches a feed; `since` (unix seconds) makes it a conditional request.
pub fn fetch(url: &str, since: Option<i64>) -> Result<Fetched, String> {
    let mut headers = vec![("User-Agent".to_string(), user_agent())];
    if let Some(t) = since.filter(|t| *t > 0) {
        headers.push(("If-Modified-Since".into(), http_date(t)));
    }
    let r = net::request("GET", url, None, &headers)?;
    match r.status {
        304 => Ok(Fetched::NotModified),
        200 => Ok(Fetched::Items(parse_rss(&String::from_utf8_lossy(&r.body))?)),
        s => Err(format!("nyaa.si answered HTTP {s}")),
    }
}

/// `651.5 MiB` -> bytes.
pub fn parse_size(text: &str) -> u64 {
    let mut it = text.split_whitespace();
    let (Some(n), Some(unit)) = (it.next().and_then(|n| n.parse::<f64>().ok()), it.next()) else { return 0 };
    let mult = match unit {
        "Bytes" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => return 0,
    };
    (n * mult) as u64
}

/// RFC 7231 date, e.g. `Wed, 25 Nov 2026 12:00:00 GMT`.
pub fn http_date(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {d:02} {} {y} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

pub fn parse_rss(xml: &str) -> Result<Vec<Item>, String> {
    let mut reader = Reader::from_str(xml);
    let mut items = Vec::new();
    let mut current: Option<Item> = None;
    let mut tag = String::new();
    // Text and entity references (`&amp;`) arrive as separate events: collect until the end tag.
    let mut text = String::new();
    loop {
        match reader.read_event().map_err(|e| format!("bad RSS: {e}"))? {
            Event::Start(e) => {
                tag = e.name().as_ref().to_string();
                text.clear();
                if tag == "item" {
                    current = Some(Item::default());
                }
            }
            Event::Text(t) => text.push_str(&t.xml_content(quick_xml::XmlVersion::Explicit1_0)),
            Event::CData(t) => text.push_str(&t.xml_content(quick_xml::XmlVersion::Explicit1_0)),
            Event::GeneralRef(r) => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    text.push(c);
                } else {
                    match r.as_ref() {
                        "amp" => text.push('&'),
                        "lt" => text.push('<'),
                        "gt" => text.push('>'),
                        "quot" => text.push('"'),
                        "apos" => text.push('\''),
                        other => {
                            text.push('&');
                            text.push_str(other);
                            text.push(';');
                        }
                    }
                }
            }
            Event::End(e) => {
                let name = e.name().as_ref().to_string();
                if name == "item" {
                    if let Some(item) = current.take().filter(|i| !i.info_hash.is_empty() && !i.title.is_empty()) {
                        items.push(item);
                    }
                } else if let (Some(item), true) = (current.as_mut(), name == tag) {
                    let value = text.trim().to_string();
                    match name.as_str() {
                        "title" => item.title = value,
                        "link" => item.link = value,
                        "guid" => item.guid = value,
                        "pubDate" => item.pub_date = value,
                        "nyaa:infoHash" => item.info_hash = value.to_lowercase(),
                        "nyaa:size" => item.size_bytes = parse_size(&value),
                        "nyaa:seeders" => item.seeders = value.parse().unwrap_or(0),
                        "nyaa:categoryId" => item.category_id = value,
                        _ => {}
                    }
                }
                tag.clear();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/nyaa/feed.xml");

    #[test]
    fn parses_real_feed_items() {
        let items = parse_rss(FIXTURE).unwrap();
        assert_eq!(items.len(), 3);
        let first = &items[0];
        assert!(first.title.contains("UploaderF"), "{}", first.title);
        assert!(first.link.starts_with("https://nyaa.si/download/") && first.link.ends_with(".torrent"));
        assert_eq!(first.info_hash.len(), 40);
        assert_eq!(first.category_id, "3_1");
        assert!(first.size_bytes > 1024 * 1024);
        assert!(items.iter().all(|i| i.guid.starts_with("https://nyaa.si/view/")));
    }

    #[test]
    fn urls_are_built_and_encoded() {
        assert_eq!(feed_url("UploaderA", "3_1", None), "https://nyaa.si/?page=rss&u=UploaderA&c=3_1");
        assert_eq!(feed_url("a b", "3_0", Some("Hero & Sage")), "https://nyaa.si/?page=rss&u=a%20b&c=3_0&q=Hero%20%26%20Sage");
        assert!(!feed_url("x", "3_1", Some("  ")).contains("&q="));
    }

    #[test]
    fn sizes_and_http_dates() {
        assert_eq!(parse_size("651.5 MiB"), 683_147_264);
        assert_eq!(parse_size("1.4 GiB"), 1_503_238_553);
        assert_eq!(parse_size("garbage"), 0);
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_795_551_323), "Tue, 24 Nov 2026 20:15:23 GMT");
    }

    #[test]
    fn entities_in_titles_survive() {
        let xml = r#"<rss xmlns:nyaa="x"><channel><item><title>Loner Life v01-14 [Seven Seas Siren] [Stick &amp; Oak] &#8211; it&apos;s</title>
            <link>https://nyaa.si/download/1.torrent</link><nyaa:infoHash>ABCDEF0123456789ABCDEF0123456789ABCDEF01</nyaa:infoHash></item></channel></rss>"#;
        let items = parse_rss(xml).unwrap();
        assert_eq!(items[0].title, "Loner Life v01-14 [Seven Seas Siren] [Stick & Oak] \u{2013} it's");
        assert_eq!(items[0].info_hash, "abcdef0123456789abcdef0123456789abcdef01");
    }

    #[test]
    fn broken_xml_is_an_error_not_a_panic() {
        assert!(parse_rss("<rss><channel><item><title>x</item>").is_err());
        assert_eq!(parse_rss("<rss></rss>").unwrap().len(), 0);
    }
}
