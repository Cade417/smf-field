//! A small HTTP layer shared by every crate, with one callback-style API.
//!
//! Natively each request runs on its own short-lived thread (blocking `reqwest`); in the
//! browser it uses the `fetch` API. Callers never block, and the same code runs on both, which
//! is what lets the desktop app and the web app share their network logic. Long waits (the
//! team service's long poll) are just requests that take a while to answer.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    #[cfg(target_arch = "wasm32")]
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Ignored in browsers, which have no per-request timeout in `fetch`.
    pub timeout: Duration,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(20),
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::Get, url)
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Set a JSON body and its content type.
    pub fn json_body(mut self, json: &str) -> Self {
        self.body = json.as_bytes().to_vec();
        self.header("Content-Type", "application/json")
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    /// HTTP Basic auth.
    pub fn basic(self, user: &str, password: &str) -> Self {
        self.header(
            "Authorization",
            &format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
        )
    }

    /// Append `?k=v&...` (values percent-encoded).
    pub fn query(mut self, pairs: &[(&str, String)]) -> Self {
        let mut sep = if self.url.contains('?') { '&' } else { '?' };
        for (k, v) in pairs {
            self.url.push(sep);
            sep = '&';
            self.url.push_str(&percent_encode(k));
            self.url.push('=');
            self.url.push_str(&percent_encode(v));
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Send a request; `done` runs once with the result (on another thread natively, from the
/// browser's event loop on the web). Transport failures are `Err`; HTTP error statuses are
/// still `Ok` so callers can read the body.
pub fn fetch(req: Request, done: impl FnOnce(Result<Response, String>) + Send + 'static) {
    imp::fetch(req, Box::new(done));
}

/// Blocking variant for tests and tools (not available in browsers).
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_blocking(req: &Request) -> Result<Response, String> {
    imp::blocking(req)
}

/// Run `job` off the UI thread where threads exist; on the web it runs right away (the web
/// app keeps such jobs short).
pub fn spawn_job(job: impl FnOnce() + Send + 'static) {
    imp::spawn_job(Box::new(job));
}

type Callback = Box<dyn FnOnce(Result<Response, String>) + Send + 'static>;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    use std::sync::OnceLock;

    fn client() -> &'static reqwest::blocking::Client {
        static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
        C.get_or_init(|| {
            reqwest::blocking::Client::builder()
                // Tile servers such as OSM require an identifying User-Agent.
                .user_agent(concat!(
                    "ShowMeFireField/",
                    env!("CARGO_PKG_VERSION"),
                    " (+https://showmefire.org)"
                ))
                .build()
                .expect("http client")
        })
    }

    pub fn blocking(req: &Request) -> Result<Response, String> {
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut b = client().request(method, &req.url).timeout(req.timeout);
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if !req.body.is_empty() {
            b = b.body(req.body.clone());
        }
        // Errors never include the URL, which can carry identifiers.
        let r = b.send().map_err(|e| e.without_url().to_string())?;
        let status = r.status().as_u16();
        let headers = r
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let bytes = r.bytes().map_err(|e| e.without_url().to_string())?.to_vec();
        Ok(Response {
            status,
            bytes,
            headers,
        })
    }

    pub fn fetch(req: Request, done: Callback) {
        std::thread::spawn(move || done(blocking(&req)));
    }

    pub fn spawn_job(job: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(job);
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    pub fn fetch(req: Request, done: Callback) {
        let mut r = ehttp::Request {
            method: req.method.as_str().to_string(),
            url: req.url,
            body: req.body,
            headers: ehttp::Headers::default(),
            mode: ehttp::Mode::default(),
            timeout: Some(req.timeout),
        };
        for (k, v) in &req.headers {
            // Browsers refuse to let pages set some headers (e.g. User-Agent); skip those.
            if !k.eq_ignore_ascii_case("user-agent") {
                r.headers.insert(k, v);
            }
        }
        ehttp::fetch(r, move |res| {
            done(res.map(|r| Response {
                status: r.status,
                headers: r.headers.headers.into_iter().collect(),
                bytes: r.bytes,
            }))
        });
    }

    pub fn spawn_job(job: Box<dyn FnOnce() + Send>) {
        job();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_helpers() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b":secret"), "OnNlY3JldA==");
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        let r = Request::get("https://x.test/p").query(&[("a", "1 2".into()), ("b", "x&y".into())]);
        assert_eq!(r.url, "https://x.test/p?a=1%202&b=x%26y");
        let r = Request::get("https://x.test/p?z=1").query(&[("a", "1".into())]);
        assert_eq!(r.url, "https://x.test/p?z=1&a=1");
        let r = Request::get("u").basic("", "pw");
        assert_eq!(r.headers[0], ("Authorization".into(), "Basic OnB3".into()));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn fetch_round_trip_with_headers_and_statuses() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.starts_with("POST /echo") && req.contains("x-test: 1") {
                    ("200 OK", "ok")
                } else {
                    ("404 Not Found", "nope")
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nX-Reply: yes\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        fetch(
            Request::new(Method::Post, format!("http://127.0.0.1:{port}/echo"))
                .header("X-Test", "1")
                .json_body("{}"),
            move |r| {
                let _ = tx2.send(r);
            },
        );
        let r = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert!(r.ok());
        assert_eq!(r.text(), "ok");
        assert_eq!(r.header("x-reply"), Some("yes"));
        // An HTTP error is still Ok(Response) so the body can be read.
        let r = fetch_blocking(&Request::get(format!("http://127.0.0.1:{port}/missing"))).unwrap();
        assert_eq!((r.status, r.ok(), r.text().as_str()), (404, false, "nope"));
        // Transport errors are Err and never contain the URL.
        let e =
            fetch_blocking(&Request::get("http://127.0.0.1:9/secret-path?key=abc")).unwrap_err();
        assert!(!e.contains("secret-path") && !e.contains("abc"), "{e}");
    }
}
