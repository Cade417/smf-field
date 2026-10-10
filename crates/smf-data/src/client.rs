//! Client for the public Show Me Fire API (callback style, see `smf-net`). No credentials are ever sent.

use crate::layers::DataError;
use smf_net::Request;
use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://api.showmefire.org";
/// Refuse absurd responses instead of exhausting memory.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// SMF_API_BASE overrides the server (e.g. http://localhost:8000 for a dev API).
pub fn default_base() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(b) = std::env::var("SMF_API_BASE")
        .ok()
        .and_then(|b| validate_base(&b).ok())
    {
        return b;
    }
    DEFAULT_BASE.to_owned()
}

/// http(s) only, no trailing slash.
pub fn validate_base(base: &str) -> Result<String, String> {
    let b = base.trim().trim_end_matches('/');
    let lower = b.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://")) && b.len() > 8 {
        Ok(b.to_owned())
    } else {
        Err("the API address must start with http:// or https://".into())
    }
}

#[derive(Debug, PartialEq)]
pub enum Fetched {
    /// The server confirmed our cached copy (HTTP 304).
    NotModified,
    Body {
        text: String,
        etag: Option<String>,
    },
}

#[derive(Clone)]
pub struct SmfClient {
    pub base: String,
}

impl SmfClient {
    pub fn new(base: &str) -> Result<Self, DataError> {
        let base = validate_base(base).map_err(DataError::Network)?;
        Ok(Self { base })
    }

    /// GET `path`, sending `If-None-Match` when we hold an ETag so unchanged data costs almost
    /// nothing. `done` runs on another thread (desktop) or the browser event loop (web).
    pub fn get(
        &self,
        path: &str,
        etag: Option<&str>,
        done: impl FnOnce(Result<Fetched, DataError>) + Send + 'static,
    ) {
        let mut req =
            Request::get(format!("{}{}", self.base, path)).timeout(Duration::from_secs(25));
        if let Some(e) = etag {
            req = req.header("If-None-Match", e);
        }
        smf_net::fetch(req, move |r| done(interpret(r)));
    }

    pub fn get_text(
        &self,
        path: &str,
        done: impl FnOnce(Result<String, DataError>) + Send + 'static,
    ) {
        self.get(path, None, move |r| {
            done(match r {
                Ok(Fetched::Body { text, .. }) => Ok(text),
                Ok(Fetched::NotModified) => Err(DataError::Status(304)),
                Err(e) => Err(e),
            })
        });
    }

    /// Blocking variants for tests and tools (not available in browsers).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_blocking(&self, path: &str, etag: Option<&str>) -> Result<Fetched, DataError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.get(path, etag, move |r| {
            let _ = tx.send(r);
        });
        rx.recv()
            .unwrap_or(Err(DataError::Network("no reply".into())))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_text_blocking(&self, path: &str) -> Result<String, DataError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.get_text(path, move |r| {
            let _ = tx.send(r);
        });
        rx.recv()
            .unwrap_or(Err(DataError::Network("no reply".into())))
    }
}

fn interpret(r: Result<smf_net::Response, String>) -> Result<Fetched, DataError> {
    let resp = r.map_err(DataError::Network)?;
    if resp.status == 304 {
        return Ok(Fetched::NotModified);
    }
    if !resp.ok() {
        return Err(DataError::Status(resp.status));
    }
    if resp.bytes.len() > MAX_BODY {
        return Err(DataError::Parse("response is unreasonably large".into()));
    }
    let etag = resp.header("etag").map(str::to_owned);
    let text = String::from_utf8(resp.bytes)
        .map_err(|_| DataError::Parse("response is not UTF-8 text".into()))?;
    Ok(Fetched::Body { text, etag })
}

#[cfg(test)]
pub(crate) mod testserver {
    //! A tiny multi-connection HTTP server for tests: serves JSON with ETag/304 support.
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    pub struct Server {
        pub port: u16,
        pub hits: Arc<AtomicUsize>,
        pub not_modified: Arc<AtomicUsize>,
        /// (etag, body) currently served; set `up` false to simulate the server vanishing.
        pub current: Arc<Mutex<(String, String)>>,
        pub status: Arc<Mutex<u16>>,
        stop: Arc<AtomicBool>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn serve(etag: &str, body: &str) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hits, not_modified) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let current = Arc::new(Mutex::new((etag.to_owned(), body.to_owned())));
        let status = Arc::new(Mutex::new(200u16));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, nm, cur, st, sp) = (
            hits.clone(),
            not_modified.clone(),
            current.clone(),
            status.clone(),
            stop.clone(),
        );
        std::thread::spawn(move || {
            while !sp.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let (h, nm, cur, st) = (h.clone(), nm.clone(), cur.clone(), st.clone());
                        std::thread::spawn(move || {
                            sock.set_nonblocking(false).unwrap();
                            let mut buf = [0u8; 4096];
                            let n = sock.read(&mut buf).unwrap_or(0);
                            let req = String::from_utf8_lossy(&buf[..n]).to_string();
                            h.fetch_add(1, Ordering::Relaxed);
                            let (etag, body) = cur.lock().unwrap().clone();
                            let code = *st.lock().unwrap();
                            let inm = req.lines().find_map(|l| {
                                l.strip_prefix("if-none-match: ")
                                    .or_else(|| l.strip_prefix("If-None-Match: "))
                            });
                            if code != 200 {
                                let _ = write!(
                                    sock,
                                    "HTTP/1.1 {code} Err\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                );
                            } else if inm.map(str::trim) == Some(etag.as_str()) {
                                nm.fetch_add(1, Ordering::Relaxed);
                                let _ = write!(
                                    sock,
                                    "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nConnection: close\r\n\r\n"
                                );
                            } else {
                                let _ = write!(
                                    sock,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: {etag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(3)),
                }
            }
        });
        Server {
            port,
            hits,
            not_modified,
            current,
            status,
            stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::serve;
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn base_validation() {
        assert_eq!(
            validate_base("https://api.showmefire.org/").unwrap(),
            "https://api.showmefire.org"
        );
        assert_eq!(
            validate_base(" http://localhost:8000 ").unwrap(),
            "http://localhost:8000"
        );
        for bad in [
            "ftp://x",
            "api.showmefire.org",
            "javascript:alert(1)",
            "http://",
            "",
        ] {
            assert!(validate_base(bad).is_err(), "{bad:?}");
        }
        assert!(SmfClient::new("file:///etc/passwd").is_err());
    }

    #[test]
    fn etag_roundtrip_and_304() {
        let s = serve("\"v1\"", r#"{"features":[]}"#);
        let c = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        let first = c.get_blocking("/x", None).unwrap();
        let etag = match &first {
            Fetched::Body { text, etag } => {
                assert_eq!(text, r#"{"features":[]}"#);
                etag.clone().unwrap()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(etag, "\"v1\"");
        assert_eq!(
            c.get_blocking("/x", Some(&etag)).unwrap(),
            Fetched::NotModified
        );
        assert_eq!(s.not_modified.load(Ordering::Relaxed), 1);
        // The data changes: the old ETag no longer matches, so the body comes back.
        *s.current.lock().unwrap() = ("\"v2\"".into(), r#"{"features":[1]}"#.into());
        assert!(matches!(
            c.get_blocking("/x", Some(&etag)).unwrap(),
            Fetched::Body { .. }
        ));
    }

    /// Network test against the real production API: `cargo test -p smf-data -- --ignored`.
    #[test]
    #[ignore = "needs the network"]
    fn live_api_smoke_test() {
        let c = SmfClient::new(&default_base()).unwrap();
        for kind in crate::DataLayerKind::ALL {
            let text = c
                .get_text_blocking(kind.path())
                .unwrap_or_else(|e| panic!("{}: {e}", kind.id()));
            let parsed =
                crate::parse_layer(kind, &text).unwrap_or_else(|e| panic!("{}: {e}", kind.id()));
            eprintln!(
                "{}: {} features ({} skipped)",
                kind.id(),
                parsed.features.len(),
                parsed.skipped
            );
            assert_eq!(parsed.skipped, 0, "{}", kind.id());
        }
        let cat =
            crate::parse_catalog(&c.get_text_blocking("/api/conditions/layers").unwrap()).unwrap();
        assert!(cat.iter().any(|l| l.id == "fire_danger"));
        let cond = crate::parse_conditions(
            &c.get_text_blocking("/api/conditions/at?lat=38.58&lon=-92.17")
                .unwrap(),
        )
        .unwrap();
        assert!(!cond.values.is_empty());
        // ETag round trip against the real server (if it sends one).
        if let Fetched::Body { etag: Some(e), .. } = c
            .get_blocking("/api/burn-bans/active.geojson", None)
            .unwrap()
        {
            assert!(matches!(
                c.get_blocking("/api/burn-bans/active.geojson", Some(&e)),
                Ok(Fetched::NotModified | Fetched::Body { .. })
            ));
        }
    }

    #[test]
    fn errors_are_typed() {
        let s = serve("\"v\"", "{}");
        let c = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        *s.status.lock().unwrap() = 503;
        assert!(matches!(
            c.get_blocking("/x", None),
            Err(DataError::Status(503))
        ));
        // Nothing listens on port 9.
        let dead = SmfClient::new("http://127.0.0.1:9").unwrap();
        assert!(matches!(
            dead.get_blocking("/x", None),
            Err(DataError::Network(_))
        ));
    }
}
