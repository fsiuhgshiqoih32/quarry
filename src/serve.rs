//! A small read-only HTTP API on 127.0.0.1, for scripts, launchers and editor plugins.
//!
//! `GET /search?q=<query>&offset=0&limit=100&sort=name|path|size|modified&desc=1&hidden=0&match_path=1`
//! `GET /status`
//!
//! Requests whose Host header isn't a loopback name are refused, so web pages can't reach the API
//! through DNS rebinding.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Response, Server};

use crate::engine::Shared;
use crate::query::{Options, Query};
use crate::search::{self, SortKey};

pub struct Api {
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
    pub port: u16,
}

impl Api {
    pub fn start(shared: Arc<Shared>, port: u16, defaults: Options) -> Result<Api, String> {
        let server = Arc::new(Server::http(("127.0.0.1", port)).map_err(|e| format!("port {port}: {e}"))?);
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
        let s = server.clone();
        let thread = std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                for req in s.incoming_requests() {
                    let (status, body) = handle(&shared, port, defaults, req.method(), req.url(), host(&req));
                    let header = Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap();
                    let _ = req
                        .respond(Response::from_string(body.to_string()).with_status_code(status).with_header(header));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Api { server, thread: Some(thread), port })
    }

    /// Serves until the process ends (for `quarry serve`).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn host(req: &tiny_http::Request) -> Option<String> {
    req.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str().to_string())
}

fn loopback_host(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else { return false };
    let h = h.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| h == *name || h == format!("{name}:{port}"))
}

fn handle(
    shared: &Shared,
    port: u16,
    defaults: Options,
    method: &Method,
    url: &str,
    host: Option<String>,
) -> (u16, Value) {
    if !loopback_host(host.as_deref(), port) {
        return (403, json!({ "error": "only loopback Host headers are accepted" }));
    }
    if *method != Method::Get {
        return (405, json!({ "error": "only GET is supported" }));
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k), decode(v))
        })
        .collect();
    let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let flag = |k: &str, d: bool| get(k).map(|v| matches!(v, "1" | "true" | "yes")).unwrap_or(d);
    match path {
        "/status" => {
            let ix = shared.index.read();
            let st = shared.status.lock();
            (
                200,
                json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "entries": ix.live_count(),
                    "scanning": st.scanning,
                    "scanned_items": shared.progress.items.load(Ordering::Relaxed),
                }),
            )
        }
        "/search" => {
            let text = get("q").unwrap_or("");
            let offset: usize = get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
            let limit: usize = get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).min(10_000);
            let sort = get("sort").and_then(SortKey::parse).unwrap_or(SortKey::Name);
            let opts = Options {
                match_path: flag("match_path", defaults.match_path),
                show_hidden: flag("hidden", defaults.show_hidden),
            };
            let ix = shared.index.read();
            let q = match Query::parse(text, opts, Some(&ix)) {
                Ok(q) => q,
                Err(e) => return (400, json!({ "error": e })),
            };
            let ids = search::run(&ix, &q, sort, flag("desc", false), &|| false).unwrap_or_default();
            let results: Vec<Value> = ids.iter().skip(offset).take(limit).map(|&id| entry_json(&ix, id)).collect();
            (200, json!({ "total": ids.len(), "offset": offset, "results": results }))
        }
        _ => (404, json!({ "error": "not found; try /search?q=... or /status" })),
    }
}

pub fn entry_json(ix: &crate::index::Index, id: u32) -> Value {
    let e = ix.entry(id);
    json!({
        "name": ix.name(id),
        "path": ix.path(id),
        "folder": e.is_dir(),
        "size": e.size,
        "modified": e.mtime(),
    })
}

/// Percent-decoding for query strings (`+` is a space).
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_and_hosts() {
        assert_eq!(decode("a+b%20c%2Fd"), "a b c/d");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%e2%9c%93"), "\u{2713}");
        assert!(loopback_host(Some("127.0.0.1:7837"), 7837));
        assert!(loopback_host(Some("localhost:7837"), 7837));
        assert!(!loopback_host(Some("evil.example:7837"), 7837));
        assert!(!loopback_host(None, 7837));
    }
}
