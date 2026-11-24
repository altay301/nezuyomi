//! `manga-reader --probe <extension.js> [query]`: exercises an extension from the
//! command line (no GTK) so extension authors can test without the UI.

use crate::ext::JsSource;
use crate::source::Source;

pub fn run(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: manga-reader --probe <extension.js> [query]");
        return 2;
    };
    let code = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return fail(format!("{path}: {e}")),
    };
    let src = match JsSource::from_code(code) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    println!("extension: {} ({}) v{}", src.name(), src.id(), src.meta().version);
    let query = args.get(1).cloned().unwrap_or_else(|| "solo".into());
    let result = (|| -> Result<(), String> {
        let p = src.popular(1)?;
        println!("popular: {} items, hasNext={}, first: {:?}", p.items.len(), p.has_next, p.items.first().map(|m| &m.title));
        let l = src.latest(1)?;
        println!("latest: {} items, first: {:?}", l.items.len(), l.items.first().map(|m| &m.title));
        let s = src.search(&query, 1)?;
        println!("search '{query}': {} items", s.items.len());
        let m = s.items.first().ok_or("search returned nothing")?;
        println!("  -> {} (id {}, cover {:?})", m.title, m.id, m.cover);
        let d = src.manga(&m.id)?;
        println!("details: {} | author {:?} | status {:?} | genres {:?}", d.title, d.author, d.status, d.genres);
        let desc: String = d.description.unwrap_or_default().chars().take(80).collect();
        println!("  description: {desc}…");
        let ch = src.chapters(&m.id)?;
        println!("chapters: {} (first {:?}, last {:?})", ch.len(), ch.first().map(|c| &c.name), ch.last().map(|c| &c.name));
        let c = ch.last().ok_or("no chapters")?;
        if src.kind() == crate::source::SourceKind::Anime {
            let videos = src.videos(&c.id)?;
            println!("videos of '{}': {}", c.name, videos.len());
            for v in &videos {
                println!("  [{}] {} headers={:?} subs={:?}", v.label, v.url, v.headers, v.subtitles.iter().map(|s| &s.label).collect::<Vec<_>>());
            }
            return Ok(());
        }
        let pages = src.pages(&c.id)?;
        println!("pages of '{}': {} (first {:?})", c.name, pages.len(), pages.first());
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

fn fail(msg: String) -> i32 {
    eprintln!("error: {msg}");
    1
}

/// `--probe-raw <extension.js> <method> <json-args>` prints the raw JSON result.
pub fn run_raw(args: &[String]) -> i32 {
    let (Some(path), Some(method)) = (args.first(), args.get(1)) else {
        eprintln!("usage: manga-reader --probe-raw <extension.js> <method> [json-args]");
        return 2;
    };
    let src = match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(JsSource::from_code) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let started = std::time::Instant::now();
    let r = src.call_json(method, args.get(2).map_or("[1]", String::as_str));
    eprintln!("(took {:.1}s)", started.elapsed().as_secs_f64());
    match r {
        Ok(t) => {
            println!("{t}");
            0
        }
        Err(e) => fail(e),
    }
}
