//! Blocking HTTP helpers. Always call from a background thread, never from the
//! GTK main thread.

use std::sync::OnceLock;
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
const MAX_BODY: u64 = 64 * 1024 * 1024;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .user_agent(USER_AGENT)
                .build(),
        )
    })
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// Response headers, names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// `body` makes it a POST-style request (the method is still taken from `method`).
pub fn request(method: &str, url: &str, body: Option<&[u8]>, headers: &[(String, String)]) -> Result<Response, String> {
    let agent = agent();
    let mut resp = match body {
        None => {
            let mut req = agent.get(url);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req.call()
        }
        Some(b) => {
            let mut req = agent.post(url);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req.send(b)
        }
    }
    .map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status().as_u16();
    let response_headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| format!("{method} {url}: reading body: {e}"))?;
    Ok(Response { status, body, headers: response_headers })
}

/// Downloads `url` and fails on any HTTP error status.
pub fn get_bytes(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let r = request("GET", url, None, headers)?;
    if r.status >= 400 {
        return Err(format!("GET {url}: HTTP {}", r.status));
    }
    Ok(r.body)
}
