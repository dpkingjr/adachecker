//! Link checker engine: parses pasted links and checks each one over HTTP.
//!
//! Each link is requested with `HEAD` first (cheap: no body), falling back to `GET`
//! when the server rejects or mishandles `HEAD` — a common source of false "broken"
//! reports. Redirects are followed manually so the full chain can be shown.

use reqwest::blocking::Client;
use reqwest::{header, Method, StatusCode};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

const MAX_REDIRECTS: usize = 10;
pub const CONCURRENCY: usize = 8;

/// Identifies the tool but stays browser-compatible, since some sites reject
/// unknown user agents outright and would otherwise be reported as broken.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) ",
    "Version/17.0 Safari/605.1.15 ADAChecker-LinkChecker/",
    env!("CARGO_PKG_VERSION")
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 2xx without redirects.
    Ok,
    /// Ended in 2xx after one or more redirects.
    Redirected,
    /// 401/403/407/429: the link may work for people but refused this automated check.
    Restricted,
    /// 4xx/5xx.
    Broken,
    /// No HTTP response: DNS failure, refused connection, TLS error, timeout, bad URL.
    Error,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ok => "OK",
            Kind::Redirected => "Redirected",
            Kind::Restricted => "Restricted",
            Kind::Broken => "Broken",
            Kind::Error => "Error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hop {
    pub status: u16,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct LinkResult {
    pub kind: Kind,
    /// Final HTTP status code, if a response was received.
    pub status: Option<u16>,
    /// Reason phrase ("Not Found") or the error description.
    pub reason: String,
    /// Redirects followed, in order: each hop's status and the URL that returned it.
    pub hops: Vec<Hop>,
    pub final_url: String,
    pub method: &'static str,
    pub elapsed: Duration,
}

impl LinkResult {
    /// "404 Not Found", or the error text when there was no response.
    pub fn status_text(&self) -> String {
        match self.status {
            Some(code) if self.reason.is_empty() => code.to_string(),
            Some(code) => format!("{code} {}", self.reason),
            None => "—".into(),
        }
    }

    /// "301 → 302 → 200" when redirected.
    pub fn chain_text(&self) -> String {
        let mut parts: Vec<String> = self.hops.iter().map(|h| h.status.to_string()).collect();
        parts.push(self.status.map(|s| s.to_string()).unwrap_or_else(|| "error".into()));
        parts.join(" → ")
    }
}

/// The links found in pasted text, and how many duplicates were dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    pub links: Vec<String>,
    pub duplicates: usize,
}

fn clean_token(token: &str) -> &str {
    // Pull a URL out of Markdown/HTML wrappers such as [text](https://…) or href="…".
    let lower = token.to_ascii_lowercase();
    let start = lower.find("http://").or_else(|| lower.find("https://")).or_else(|| lower.find("www."));
    let t = start.map(|s| &token[s..]).unwrap_or(token);
    // Inside HTML attributes or quotes the URL ends at the closing quote/bracket.
    let t = t.trim_start_matches(['<', '(', '[', '"', '\'']);
    let t = &t[..t.find(['"', '\'', '<', '>']).unwrap_or(t.len())];
    t.trim_start_matches(['<', '(', '[', '"', '\''])
        .trim_end_matches(['>', ')', ']', '"', '\'', ',', ';', '.', ':', '!', '?'])
}

/// Extracts links from pasted text: one per line, or any `http(s)://`/`www.` tokens in a
/// line of prose. Lines starting with `#` are comments. Links without a scheme get
/// `https://`. Duplicates are removed (first occurrence kept).
pub fn parse_links(text: &str) -> Parsed {
    let mut seen = HashSet::new();
    let mut out = Parsed::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line
            .split_whitespace()
            .filter(|t| {
                let l = t.to_ascii_lowercase();
                l.contains("http://") || l.contains("https://") || l.contains("www.")
            })
            .collect();
        let candidates = if !tokens.is_empty() {
            tokens
        } else if !line.contains(char::is_whitespace) && line.contains('.') {
            vec![line] // bare "example.com/page"
        } else {
            continue; // prose without links
        };
        for c in candidates {
            let c = clean_token(c);
            if c.is_empty() {
                continue;
            }
            let link = if c.contains("://") { c.to_string() } else { format!("https://{c}") };
            if seen.insert(link.clone()) {
                out.links.push(link);
            } else {
                out.duplicates += 1;
            }
        }
    }
    out
}

pub fn build_client() -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
}

fn classify(status: StatusCode, redirected: bool) -> Kind {
    match status.as_u16() {
        401 | 403 | 407 | 429 => Kind::Restricted,
        400..=599 => Kind::Broken,
        _ if redirected => Kind::Redirected,
        _ => Kind::Ok,
    }
}

/// Describes a request error by its root cause, e.g. "dns error: failed to lookup address".
fn describe(e: &reqwest::Error) -> String {
    let mut root: &dyn std::error::Error = e;
    while let Some(next) = root.source() {
        root = next;
    }
    let kind = if e.is_timeout() {
        "Timed out"
    } else if e.is_connect() {
        "Could not connect"
    } else {
        "Request failed"
    };
    let detail = root.to_string();
    if detail.is_empty() || e.is_timeout() { kind.to_string() } else { format!("{kind}: {detail}") }
}

fn error_result(msg: String, hops: Vec<Hop>, final_url: String, method: &'static str, start: Instant) -> LinkResult {
    LinkResult { kind: Kind::Error, status: None, reason: msg, hops, final_url, method, elapsed: start.elapsed() }
}

/// Checks one link, following redirects.
pub fn check(client: &Client, link: &str) -> LinkResult {
    let start = Instant::now();
    let mut url = match Url::parse(link) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        Ok(u) => return error_result(format!("Unsupported scheme \"{}\"", u.scheme()), vec![], link.into(), "-", start),
        Err(e) => return error_result(format!("Invalid URL: {e}"), vec![], link.into(), "-", start),
    };
    let mut method = Method::HEAD;
    let mut hops = Vec::new();
    loop {
        let method_name = if method == Method::HEAD { "HEAD" } else { "GET" };
        match client.request(method.clone(), url.clone()).send() {
            Err(e) => {
                // Some servers drop HEAD requests; retry the same URL once with GET.
                if method == Method::HEAD && !e.is_timeout() {
                    method = Method::GET;
                    continue;
                }
                return error_result(describe(&e), hops, url.to_string(), method_name, start);
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_redirection() {
                    if let Some(next) = resp
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|l| l.to_str().ok())
                        .and_then(|l| url.join(l).ok())
                    {
                        hops.push(Hop { status: status.as_u16(), url: url.to_string() });
                        if hops.len() > MAX_REDIRECTS {
                            return error_result(format!("Too many redirects (> {MAX_REDIRECTS})"), hops, next.to_string(), method_name, start);
                        }
                        url = next;
                        continue;
                    }
                }
                // HEAD is often unsupported or mis-implemented; confirm failures with GET.
                if method == Method::HEAD && (status.is_client_error() || status.is_server_error()) {
                    method = Method::GET;
                    continue;
                }
                return LinkResult {
                    kind: classify(status, !hops.is_empty()),
                    status: Some(status.as_u16()),
                    reason: status.canonical_reason().unwrap_or("").to_string(),
                    hops,
                    final_url: url.to_string(),
                    method: method_name,
                    elapsed: start.elapsed(),
                };
            }
        }
    }
}

/// A batch of links being checked on background threads.
pub struct LinkRun {
    pub rx: Receiver<(usize, LinkResult)>,
    cancel: Arc<AtomicBool>,
}

impl LinkRun {
    /// Stops handing out new links; requests already in flight finish on their own.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Checks `links` with up to `CONCURRENCY` worker threads. Each result is sent with the
/// link's index, and `notify` is called so the UI can repaint.
pub fn start(links: Vec<String>, notify: Arc<dyn Fn() + Send + Sync>) -> LinkRun {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let workers = CONCURRENCY.min(links.len()).max(1);
    let queue = Arc::new(Mutex::new(links.into_iter().enumerate().collect::<VecDeque<_>>()));
    let client = build_client().map_err(|e| e.to_string());
    for _ in 0..workers {
        let (tx, cancel, queue, notify, client) = (tx.clone(), cancel.clone(), queue.clone(), notify.clone(), client.clone());
        std::thread::spawn(move || loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let job = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let Some((i, link)) = job else { break };
            let result = match &client {
                Ok(c) => check(c, &link),
                Err(e) => error_result(format!("Could not create HTTP client: {e}"), vec![], link.clone(), "-", Instant::now()),
            };
            if tx.send((i, result)).is_err() {
                break;
            }
            notify();
        });
    }
    LinkRun { rx, cancel }
}

/// Quotes a CSV field when needed.
fn csv(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// CSV export of the results (unchecked links have empty result columns).
pub fn to_csv(rows: &[(String, Option<LinkResult>)]) -> String {
    let mut out = String::from("url,result,status,reason,redirect_chain,final_url,method,time_ms\n");
    for (link, res) in rows {
        let line = match res {
            Some(r) => [
                csv(link),
                r.kind.label().into(),
                r.status.map(|s| s.to_string()).unwrap_or_default(),
                csv(&r.reason),
                csv(&r.chain_text()),
                csv(&r.final_url),
                r.method.into(),
                r.elapsed.as_millis().to_string(),
            ]
            .join(","),
            None => format!("{},pending,,,,,,", csv(link)),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn parses_pasted_links() {
        let text = "# my links\n\
            https://example.com/a\n\
            example.org/page\n\
            See [docs](https://docs.rs/egui) and <https://www.rust-lang.org>.\n\
            just some words\n\
            https://example.com/a\n\
            www.w3.org/WAI/, http://x.test/q?a=1&b=2\n\
            <a href=\"https://html.example/link\">x</a>";
        let p = parse_links(text);
        assert_eq!(
            p.links,
            [
                "https://example.com/a",
                "https://example.org/page",
                "https://docs.rs/egui",
                "https://www.rust-lang.org",
                "https://www.w3.org/WAI/",
                "http://x.test/q?a=1&b=2",
                "https://html.example/link",
            ]
        );
        assert_eq!(p.duplicates, 1);
    }

    #[test]
    fn classifies_statuses() {
        assert_eq!(classify(StatusCode::OK, false), Kind::Ok);
        assert_eq!(classify(StatusCode::OK, true), Kind::Redirected);
        assert_eq!(classify(StatusCode::NOT_FOUND, false), Kind::Broken);
        assert_eq!(classify(StatusCode::NOT_FOUND, true), Kind::Broken);
        assert_eq!(classify(StatusCode::FORBIDDEN, false), Kind::Restricted);
        assert_eq!(classify(StatusCode::TOO_MANY_REQUESTS, false), Kind::Restricted);
        assert_eq!(classify(StatusCode::SERVICE_UNAVAILABLE, false), Kind::Broken);
    }

    #[test]
    fn csv_escaping() {
        assert_eq!(csv("plain"), "plain");
        assert_eq!(csv("a,b"), "\"a,b\"");
        assert_eq!(csv("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    /// Minimal local HTTP server: routes are (path, status line, extra headers).
    /// Records the request methods it receives.
    fn serve(routes: Vec<(&'static str, &'static str, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let mut parts = request.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                log2.lock().unwrap().push(format!("{method} {path}"));
                let route = path.split('?').next().unwrap_or(path);
                let (status, headers) = routes
                    .iter()
                    .find(|(p, _, _)| *p == route)
                    .map(|(_, s, h)| (*s, *h))
                    .unwrap_or(("404 Not Found", ""));
                // "/head-broken" rejects HEAD but serves GET.
                let status = if path == "/head-broken" && method == "HEAD" { "405 Method Not Allowed" } else { status };
                let mut s = stream;
                let _ = write!(s, "HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        (base, log)
    }

    #[test]
    fn checks_against_local_server() {
        let (base, log) = serve(vec![
            ("/ok", "200 OK", ""),
            ("/moved", "301 Moved Permanently", "Location: /also-moved\r\n"),
            ("/also-moved", "302 Found", "Location: /ok\r\n"),
            ("/head-broken", "200 OK", ""),
            ("/forbidden", "403 Forbidden", ""),
            ("/loop", "302 Found", "Location: /loop\r\n"),
        ]);
        let client = build_client().unwrap();
        let get = |p: &str| check(&client, &format!("{base}{p}"));

        let ok = get("/ok");
        assert_eq!((ok.kind, ok.status, ok.method), (Kind::Ok, Some(200), "HEAD"));
        assert_eq!(ok.status_text(), "200 OK");

        let missing = get("/missing");
        assert_eq!((missing.kind, missing.status, missing.method), (Kind::Broken, Some(404), "GET"), "404 confirmed with GET");
        assert_eq!(missing.status_text(), "404 Not Found");

        let moved = get("/moved");
        assert_eq!((moved.kind, moved.status), (Kind::Redirected, Some(200)));
        assert_eq!(moved.chain_text(), "301 → 302 → 200");
        assert_eq!(moved.final_url, format!("{base}/ok"));

        let head_broken = get("/head-broken");
        assert_eq!((head_broken.kind, head_broken.status, head_broken.method), (Kind::Ok, Some(200), "GET"), "HEAD 405 falls back to GET");

        assert_eq!(get("/forbidden").kind, Kind::Restricted);

        let looped = get("/loop");
        assert_eq!(looped.kind, Kind::Error);
        assert!(looped.reason.contains("Too many redirects"), "{}", looped.reason);

        let refused = check(&client, "http://127.0.0.1:9/");
        assert_eq!(refused.kind, Kind::Error);
        assert!(refused.reason.starts_with("Could not connect"), "{}", refused.reason);

        assert_eq!(check(&client, "ftp://example.com/x").kind, Kind::Error);
        assert!(log.lock().unwrap().iter().any(|l| l == "HEAD /ok"));
    }

    #[test]
    fn runs_batches_concurrently() {
        let (base, _) = serve(vec![("/a", "200 OK", ""), ("/b", "500 Internal Server Error", "")]);
        let links: Vec<String> = ["/a", "/b", "/c", "/a?x=1"].iter().map(|p| format!("{base}{p}")).collect();
        let run = start(links.clone(), Arc::new(|| {}));
        let mut got: Vec<(usize, LinkResult)> = run.rx.iter().take(links.len()).collect();
        got.sort_by_key(|(i, _)| *i);
        let codes: Vec<_> = got.iter().map(|(_, r)| r.status).collect();
        assert_eq!(codes, [Some(200), Some(500), Some(404), Some(200)]);
        let csv = to_csv(&links.iter().cloned().zip(got.into_iter().map(|(_, r)| Some(r))).collect::<Vec<_>>());
        assert!(csv.lines().nth(2).unwrap().contains(",Broken,500,Internal Server Error,"), "{csv}");
    }
}
