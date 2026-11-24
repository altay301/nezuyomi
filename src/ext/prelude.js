// Host API for extensions (PLAN.md → Host API). `__http`, `__doc_*` and friends
// are implemented in Rust; everything here is thin sugar around them.
class El {
  constructor(d, n) { this._d = d; this._n = n; }
  select(sel) { return JSON.parse(__q(this._d, this._n, sel, false)).map(n => new El(this._d, n)); }
  selectFirst(sel) {
    const r = JSON.parse(__q(this._d, this._n, sel, true));
    return r.length ? new El(this._d, r[0]) : null;
  }
  text() { return __node_text(this._d, this._n); }
  attr(name) { const v = __node_attr(this._d, this._n, name); return v === undefined ? null : v; }
  html() { return __node_html(this._d, this._n); }
}
class Doc extends El { constructor(d) { super(d, -1); } }

function __opts(o) { return JSON.stringify((o && o.headers) || {}); }

globalThis.http = {
  async get(url, opts) { return JSON.parse(__http("GET", url, null, __opts(opts))); },
  async post(url, body, opts) {
    const headers = Object.assign({}, (opts && opts.headers) || {});
    if (body !== null && typeof body === "object") {
      body = JSON.stringify(body);
      if (!Object.keys(headers).some(k => k.toLowerCase() === "content-type")) headers["Content-Type"] = "application/json";
    }
    return JSON.parse(__http("POST", url, String(body ?? ""), JSON.stringify(headers)));
  },
  async getJson(url, opts) {
    const r = await http.get(url, opts);
    if (r.status >= 400) throw new Error("HTTP " + r.status + " for " + url);
    return JSON.parse(r.text);
  },
  async getHtml(url, opts) {
    const r = await http.get(url, opts);
    if (r.status >= 400) throw new Error("HTTP " + r.status + " for " + url);
    return new Doc(__doc_parse(r.text));
  },
};
globalThis.html = { parse(text) { return new Doc(__doc_parse(text)); } };
globalThis.util = {
  // base64 -> binary string (one char per byte)
  base64Decode(s) { return __b64(s); },
  // binary string -> text, decoding UTF-8
  utf8(bin) { return __utf8(bin); },
  // text/binary string XOR a repeating key string -> binary string
  xor(bin, key) { let o = ""; for (let i = 0; i < bin.length; i++) o += String.fromCharCode(bin.charCodeAt(i) ^ key.charCodeAt(i % key.length)); return o; },
};
globalThis.log = (...a) => __log(a.map(x => typeof x === "string" ? x : JSON.stringify(x)).join(" "));
globalThis.settings = { get(key) { const v = __setting(key); return v === undefined ? null : v; } };

globalThis.__call = async (method, argsJson) => {
  const fn = __ext[method];
  if (typeof fn !== "function") throw new Error("extension has no method '" + method + "'");
  const r = await fn.apply(__ext, JSON.parse(argsJson));
  return JSON.stringify(r === undefined ? null : r);
};
