//! JavaScript extensions running in a sandboxed QuickJS runtime.
//!
//! * JS only describes *what* to request and extract; HTTP and HTML parsing are
//!   done in Rust (`net`, `scraper`). JS never sees image bytes.
//! * One runtime lives on its own low-priority thread, created on first use and
//!   dropped after [`IDLE_TIMEOUT`] without calls (lazy loading).
//! * Limits: 32 MB memory, 30 s per call, requests only to the extension's
//!   declared `domains`. No file system, no processes, no `eval` of remote code.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::Func;
use rquickjs::{Context, Ctx, Exception, Module, Object, Promise, Runtime};
use scraper::{ElementRef, Html, Selector};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::net;
use crate::source::{ChapterInfo, MangaDetails, MangaPage, PageRef, Source, SourceResult};

const PRELUDE: &str = include_str!("prelude.js");
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
/// Politeness: minimum gap between two requests of one extension.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(120);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub domains: Vec<String>,
}

struct Msg {
    method: String,
    args: String,
    reply: mpsc::Sender<Result<String, String>>,
}

pub struct JsSource {
    meta: Meta,
    code: Arc<String>,
    tx: Mutex<Option<mpsc::Sender<Msg>>>,
}

impl JsSource {
    /// Reads the extension's metadata (briefly starts a runtime) without keeping it alive.
    pub fn from_code(code: String) -> Result<Self, String> {
        let code = Arc::new(code);
        let host = Host::start(&code)?;
        Ok(Self { meta: host.meta.clone(), code, tx: Mutex::new(None) })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Raw call for tooling (`--probe-raw`).
    pub fn call_json(&self, method: &str, args: &str) -> SourceResult<String> {
        let args = serde_json::from_str(args).map_err(|e| format!("bad json args: {e}"))?;
        self.call_raw(method, args)
    }

    fn call<T: DeserializeOwned>(&self, method: &str, args: serde_json::Value) -> SourceResult<T> {
        let text = self.call_raw(method, args)?;
        serde_json::from_str(&text).map_err(|e| format!("{}: bad result of {method}(): {e}", self.meta.id))
    }

    fn call_raw(&self, method: &str, args: serde_json::Value) -> SourceResult<String> {
        // Two attempts: the host thread may have just timed out and exited.
        for _ in 0..2 {
            let (reply, wait) = mpsc::channel();
            let msg = Msg { method: method.into(), args: args.to_string(), reply };
            let mut slot = self.tx.lock().unwrap();
            if slot.is_none() {
                let (tx, rx) = mpsc::channel();
                let code = self.code.clone();
                std::thread::Builder::new()
                    .name(format!("ext-{}", self.meta.id))
                    .spawn(move || host_thread(&code, rx))
                    .map_err(|e| e.to_string())?;
                *slot = Some(tx);
            }
            if slot.as_ref().unwrap().send(msg).is_err() {
                *slot = None;
                continue;
            }
            drop(slot);
            return wait
                .recv()
                .map_err(|_| format!("{}: extension host stopped", self.meta.id))?
                .map_err(|e| format!("{}: {method}(): {e}", self.meta.id));
        }
        Err(format!("{}: cannot start extension host", self.meta.id))
    }
}

impl Source for JsSource {
    fn id(&self) -> &str {
        &self.meta.id
    }
    fn name(&self) -> &str {
        &self.meta.name
    }
    fn popular(&self, page: u32) -> SourceResult<MangaPage> {
        self.call("popular", serde_json::json!([page]))
    }
    fn latest(&self, page: u32) -> SourceResult<MangaPage> {
        self.call("latest", serde_json::json!([page]))
    }
    fn search(&self, query: &str, page: u32) -> SourceResult<MangaPage> {
        self.call("search", serde_json::json!([query, page, {}]))
    }
    fn manga(&self, id: &str) -> SourceResult<MangaDetails> {
        self.call("getManga", serde_json::json!([id]))
    }
    fn chapters(&self, manga_id: &str) -> SourceResult<Vec<ChapterInfo>> {
        self.call("getChapters", serde_json::json!([manga_id]))
    }
    fn pages(&self, chapter_id: &str) -> SourceResult<Vec<PageRef>> {
        self.call("getPages", serde_json::json!([chapter_id]))
    }
}

// ---------------------------------------------------------------------------
// Everything below runs on the extension's own thread.
// ---------------------------------------------------------------------------

struct DocData {
    html: Html,
    nodes: Vec<ego_tree::NodeId>,
}

#[derive(Default)]
struct Shared {
    domains: RefCell<Vec<String>>,
    docs: RefCell<Vec<DocData>>,
    last_request: Cell<Option<Instant>>,
}

struct Host {
    // Field order matters: the context must drop before the runtime.
    ctx: Context,
    _rt: Runtime,
    shared: std::rc::Rc<Shared>,
    deadline: Arc<AtomicU64>,
    meta: Meta,
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn describe(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    match err {
        rquickjs::Error::Exception => {
            let v = ctx.catch();
            match v.as_exception() {
                Some(ex) => ex.message().unwrap_or_else(|| "exception".into()),
                None => format!("{v:?}"),
            }
        }
        other => other.to_string(),
    }
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    authority.split(':').next().filter(|h| !h.is_empty())
}

fn domain_allowed(domains: &[String], url: &str) -> bool {
    host_of(url).is_some_and(|host| {
        let host = host.to_ascii_lowercase();
        domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
    })
}

impl Host {
    fn start(code: &str) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(MEMORY_LIMIT);
        rt.set_max_stack_size(STACK_LIMIT);
        let deadline = Arc::new(AtomicU64::new(u64::MAX));
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || now_ms() > d.load(Ordering::Relaxed))));
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        let shared = std::rc::Rc::new(Shared::default());

        deadline.store(now_ms() + CALL_TIMEOUT.as_millis() as u64, Ordering::Relaxed);
        let meta = ctx.with(|ctx| -> Result<Meta, String> {
            let load = || -> rquickjs::Result<String> {
                register_host_functions(&ctx, &shared)?;
                ctx.eval::<(), _>(PRELUDE)?;
                let (module, promise) = Module::declare(ctx.clone(), "extension", code)?.eval()?;
                promise.finish::<()>()?;
                let def: Object = module.get("default")?;
                ctx.globals().set("__ext", def.clone())?;
                Ok(ctx.json_stringify(def)?.map(|s| s.to_string()).transpose()?.unwrap_or_default())
            };
            let json = load().map_err(|e| describe(&ctx, e))?;
            serde_json::from_str::<Meta>(&json).map_err(|e| format!("invalid extension metadata: {e}"))
        })?;
        *shared.domains.borrow_mut() = meta.domains.iter().map(|d| d.to_ascii_lowercase()).collect();
        deadline.store(u64::MAX, Ordering::Relaxed);
        Ok(Self { ctx, _rt: rt, shared, deadline, meta })
    }

    fn call(&self, method: &str, args: &str) -> Result<String, String> {
        self.deadline.store(now_ms() + CALL_TIMEOUT.as_millis() as u64, Ordering::Relaxed);
        let result = self.ctx.with(|ctx| -> Result<String, String> {
            let run = || -> rquickjs::Result<String> {
                let f: rquickjs::Function = ctx.globals().get("__call")?;
                let promise: Promise = f.call((method, args))?;
                promise.finish::<String>()
            };
            run().map_err(|e| match e {
                rquickjs::Error::WouldBlock => "extension never finished (unresolved promise)".to_string(),
                e => describe(&ctx, e),
            })
        });
        self.deadline.store(u64::MAX, Ordering::Relaxed);
        self.shared.docs.borrow_mut().clear();
        result
    }
}

fn host_thread(code: &str, rx: mpsc::Receiver<Msg>) {
    crate::loader::lower_priority();
    let host = match Host::start(code) {
        Ok(h) => h,
        Err(e) => {
            // Answer the first message with the startup error, then stop.
            if let Ok(msg) = rx.recv() {
                let _ = msg.reply.send(Err(e));
            }
            return;
        }
    };
    loop {
        match rx.recv_timeout(IDLE_TIMEOUT) {
            Ok(msg) => {
                let _ = msg.reply.send(host.call(&msg.method, &msg.args));
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn register_host_functions<'js>(ctx: &Ctx<'js>, shared: &std::rc::Rc<Shared>) -> rquickjs::Result<()> {
    let g = ctx.globals();

    let sh = shared.clone();
    g.set(
        "__http",
        Func::from(move |ctx: Ctx<'js>, method: String, url: String, body: Option<String>, headers: String| {
            if !domain_allowed(&sh.domains.borrow(), &url) {
                return Err(Exception::throw_message(&ctx, &format!("request to {url} blocked: host is not in the extension's domains")));
            }
            if let Some(last) = sh.last_request.get() {
                if let Some(wait) = MIN_REQUEST_GAP.checked_sub(last.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            let headers: std::collections::BTreeMap<String, String> = serde_json::from_str(&headers).unwrap_or_default();
            let headers: Vec<(String, String)> = headers.into_iter().collect();
            let resp = net::request(&method, &url, body.as_deref(), &headers);
            sh.last_request.set(Some(Instant::now()));
            match resp {
                Ok(r) => Ok(serde_json::json!({ "status": r.status, "text": String::from_utf8_lossy(&r.body) }).to_string()),
                Err(e) => Err(Exception::throw_message(&ctx, &e)),
            }
        }),
    )?;

    let sh = shared.clone();
    g.set(
        "__doc_parse",
        Func::from(move |text: String| -> u32 {
            let mut docs = sh.docs.borrow_mut();
            docs.push(DocData { html: Html::parse_document(&text), nodes: Vec::new() });
            (docs.len() - 1) as u32
        }),
    )?;

    let sh = shared.clone();
    g.set(
        "__q",
        Func::from(move |ctx: Ctx<'js>, doc: u32, node: i32, sel: String, first: bool| {
            let selector = Selector::parse(&sel)
                .map_err(|e| Exception::throw_message(&ctx, &format!("bad selector '{sel}': {e}")))?;
            let mut docs = sh.docs.borrow_mut();
            let d = docs.get_mut(doc as usize).ok_or_else(|| Exception::throw_message(&ctx, "stale document handle"))?;
            let mut found: Vec<ego_tree::NodeId> = Vec::new();
            if node < 0 {
                found.extend(d.html.select(&selector).map(|e| e.id()).take(if first { 1 } else { usize::MAX }));
            } else {
                let id = *d.nodes.get(node as usize).ok_or_else(|| Exception::throw_message(&ctx, "stale node handle"))?;
                let el = d.html.tree.get(id).and_then(ElementRef::wrap);
                if let Some(el) = el {
                    found.extend(el.select(&selector).map(|e| e.id()).take(if first { 1 } else { usize::MAX }));
                }
            }
            let handles: Vec<usize> = found
                .into_iter()
                .map(|id| {
                    d.nodes.push(id);
                    d.nodes.len() - 1
                })
                .collect();
            Ok::<String, rquickjs::Error>(serde_json::to_string(&handles).unwrap_or_else(|_| "[]".into()))
        }),
    )?;

    let sh = shared.clone();
    let with_node = move |doc: u32, node: i32, f: &dyn Fn(ElementRef) -> Option<String>| -> Option<String> {
        let docs = sh.docs.borrow();
        let d = docs.get(doc as usize)?;
        if node < 0 {
            return f(d.html.root_element());
        }
        let el = d.html.tree.get(*d.nodes.get(node as usize)?).and_then(ElementRef::wrap)?;
        f(el)
    };
    let w = with_node.clone();
    g.set(
        "__node_text",
        Func::from(move |doc: u32, node: i32| -> String {
            w(doc, node, &|e| Some(e.text().collect::<String>().trim().to_string())).unwrap_or_default()
        }),
    )?;
    let w = with_node.clone();
    g.set(
        "__node_html",
        Func::from(move |doc: u32, node: i32| -> String { w(doc, node, &|e| Some(e.inner_html())).unwrap_or_default() }),
    )?;
    let w = with_node;
    g.set(
        "__node_attr",
        Func::from(move |doc: u32, node: i32, name: String| -> Option<String> {
            w(doc, node, &|e| e.value().attr(&name).map(str::to_string))
        }),
    )?;

    g.set("__log", Func::from(|msg: String| eprintln!("[ext] {msg}")))?;
    g.set("__setting", Func::from(|_key: String| -> Option<String> { None }))?;
    Ok(())
}
