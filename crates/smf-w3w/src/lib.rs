//! what3words support: parsing three-word addresses and a small blocking client for the
//! what3words v3 API (words -> position, position -> words, 3 m grid lines).
//!
//! Why online-only: the word list and the mapping are proprietary, so there is no offline
//! conversion. Every call needs an API key on a plan that includes the convert and grid
//! endpoints (the free plan covers only autosuggest). The key travels in the `X-Api-Key`
//! header, never in the URL, so it cannot end up in logs or error messages.

use serde::Deserialize;
use smf_geo::LngLat;
use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://api.what3words.com";
/// The API refuses grid boxes larger than about 4 km corner to corner.
pub const MAX_GRID_DIAGONAL_M: f64 = 4000.0;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum W3wError {
    #[error("no what3words API key is set")]
    NoKey,
    #[error("that is not a three-word address (expected word.word.word)")]
    BadFormat,
    #[error("{0}")]
    Api(String),
    #[error("could not reach what3words ({0})")]
    Network(String),
}

/// A 3 m x 3 m square and its address.
#[derive(Debug, Clone, PartialEq)]
pub struct Square {
    pub words: String,
    pub center: LngLat,
    pub sw: LngLat,
    pub ne: LngLat,
    pub nearest_place: String,
    pub country: String,
}

impl Square {
    pub fn contains(&self, p: LngLat) -> bool {
        p.lng >= self.sw.lng && p.lng <= self.ne.lng && p.lat >= self.sw.lat && p.lat <= self.ne.lat
    }

    /// Closed ring starting at the south-west corner.
    pub fn ring(&self) -> [LngLat; 5] {
        let (sw, ne) = (self.sw, self.ne);
        [
            sw,
            LngLat::new(ne.lng, sw.lat),
            ne,
            LngLat::new(sw.lng, ne.lat),
            sw,
        ]
    }
}

/// One grid line segment.
pub type Line = [LngLat; 2];

/// Accepts "///a.b.c", "a.b.c", "A B C" and the dot-like separators the service allows.
/// Returns lowercase "a.b.c".
pub fn normalize(input: &str) -> Result<String, W3wError> {
    let s = input.trim().trim_start_matches('/');
    let words: Vec<String> = s
        .split(|c: char| {
            matches!(c, '.' | '。' | '・' | '，' | ',' | '､' | '、') || c.is_whitespace()
        })
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    let ok = words.len() == 3
        && words.iter().all(|w| {
            w.chars().count() <= 40
                && w.chars()
                    .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
        });
    if ok {
        Ok(words.join("."))
    } else {
        Err(W3wError::BadFormat)
    }
}

/// True when the text is written as a three-word address (dotted, or with the `///` prefix),
/// so a search box can tell it apart from a place name without a network call.
pub fn looks_like_words(input: &str) -> bool {
    let t = input.trim();
    normalize(t).is_ok() && (t.starts_with("///") || t.contains(['.', '。']))
}

#[derive(Deserialize)]
struct RawPoint {
    lng: f64,
    lat: f64,
}
#[derive(Deserialize)]
struct RawSquare {
    southwest: RawPoint,
    northeast: RawPoint,
}
#[derive(Deserialize)]
struct RawConvert {
    words: String,
    square: RawSquare,
    coordinates: RawPoint,
    #[serde(default, rename = "nearestPlace")]
    nearest_place: String,
    #[serde(default)]
    country: String,
}
#[derive(Deserialize)]
struct RawLine {
    start: RawPoint,
    end: RawPoint,
}
#[derive(Deserialize)]
struct RawGrid {
    lines: Vec<RawLine>,
}
#[derive(Deserialize)]
struct RawErrorBody {
    error: RawError,
}
#[derive(Deserialize)]
struct RawError {
    code: String,
    #[serde(default)]
    message: String,
}

fn pt(p: &RawPoint) -> LngLat {
    LngLat::new(p.lng, p.lat)
}

fn parse_square(text: &str) -> Result<Square, W3wError> {
    let r: RawConvert =
        serde_json::from_str(text).map_err(|e| W3wError::Api(format!("unexpected reply ({e})")))?;
    Ok(Square {
        words: r.words,
        center: pt(&r.coordinates),
        sw: pt(&r.square.southwest),
        ne: pt(&r.square.northeast),
        nearest_place: r.nearest_place,
        country: r.country,
    })
}

fn parse_grid(text: &str) -> Result<Vec<Line>, W3wError> {
    let r: RawGrid =
        serde_json::from_str(text).map_err(|e| W3wError::Api(format!("unexpected reply ({e})")))?;
    Ok(r.lines.iter().map(|l| [pt(&l.start), pt(&l.end)]).collect())
}

/// Turn an error body or status into a message a field user can act on.
fn api_error(status: u16, body: &str) -> W3wError {
    let (code, message) = match serde_json::from_str::<RawErrorBody>(body) {
        Ok(b) => (b.error.code, b.error.message),
        Err(_) => (String::new(), String::new()),
    };
    let text = match (status, code.as_str()) {
        (_, "BadWords") => "That three-word address doesn't exist.".to_string(),
        (_, "BadCoordinates") => "what3words doesn't cover that position.".to_string(),
        (_, "BadBoundingBoxTooBig") => "The grid area was too large.".to_string(),
        (401, _) => "what3words rejected the API key (missing or invalid).".to_string(),
        (402, _) => "Your what3words plan doesn't include this feature, or its quota is used up."
            .to_string(),
        (429, _) => "what3words rate limit reached; try again in a moment.".to_string(),
        _ if !message.is_empty() => format!("what3words: {message}"),
        _ => format!("what3words returned HTTP {status}."),
    };
    W3wError::Api(text)
}

#[derive(Clone)]
pub struct W3wClient {
    base: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl std::fmt::Debug for W3wClient {
    // Keeps the key out of any log line that prints the client.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "W3wClient({})", self.base)
    }
}

impl W3wClient {
    pub fn new(base: &str, key: &str) -> Result<Self, W3wError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(W3wError::NoKey);
        }
        let base = base.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(W3wError::Api(
                "what3words server address must be http(s)".into(),
            ));
        }
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| W3wError::Network(e.to_string()))?;
        Ok(Self {
            base,
            key: key.to_string(),
            http,
        })
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String, W3wError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("X-Api-Key", &self.key)
            .query(query)
            .send()
            .map_err(|e| W3wError::Network(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| W3wError::Network(e.without_url().to_string()))?;
        if status == 200 {
            Ok(body)
        } else {
            Err(api_error(status, &body))
        }
    }

    pub fn to_square(&self, words: &str) -> Result<Square, W3wError> {
        let words = normalize(words)?;
        parse_square(&self.get("/v3/convert-to-coordinates", &[("words", words)])?)
    }

    pub fn to_words(&self, p: LngLat) -> Result<Square, W3wError> {
        let c = format!("{:.6},{:.6}", p.lat, p.lng);
        parse_square(&self.get("/v3/convert-to-3wa", &[("coordinates", c)])?)
    }

    /// Grid lines inside the box `[west, south, east, north]`; the caller keeps it within
    /// `MAX_GRID_DIAGONAL_M` (see `clamp_grid_box`).
    pub fn grid(&self, bbox: [f64; 4]) -> Result<Vec<Line>, W3wError> {
        // The API wants south_lat,west_lng,north_lat,east_lng.
        let b = format!(
            "{:.6},{:.6},{:.6},{:.6}",
            bbox[1], bbox[0], bbox[3], bbox[2]
        );
        parse_grid(&self.get("/v3/grid-section", &[("bounding-box", b)])?)
    }
}

/// Shrink a requested box about its centre so its diagonal stays within the API limit.
pub fn clamp_grid_box(b: [f64; 4]) -> [f64; 4] {
    let diag = smf_geo::distance_m(LngLat::new(b[0], b[1]), LngLat::new(b[2], b[3]));
    let limit = MAX_GRID_DIAGONAL_M * 0.95;
    if diag <= limit {
        return b;
    }
    let k = limit / diag;
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (hx, hy) = ((b[2] - b[0]) / 2.0 * k, (b[3] - b[1]) / 2.0 * k);
    [cx - hx, cy - hy, cx + hx, cy + hy]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // Shapes follow the documented v3 responses.
    const CONVERT: &str = r#"{"country":"GB","square":{"southwest":{"lng":-0.195543,"lat":51.520833},"northeast":{"lng":-0.195499,"lat":51.52086}},"nearestPlace":"Bayswater, London","coordinates":{"lng":-0.195521,"lat":51.520847},"words":"filled.count.soap","language":"en","map":"https://w3w.co/filled.count.soap"}"#;
    const GRID: &str = r#"{"lines":[{"start":{"lng":-0.1955,"lat":51.5208},"end":{"lng":-0.1955,"lat":51.5209}},{"start":{"lng":-0.1954,"lat":51.5208},"end":{"lng":-0.1953,"lat":51.5208}}]}"#;

    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// Serves one canned reply per request and records (request line, X-Api-Key header).
    fn serve(status: u16, body: &'static str) -> (u16, Seen) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen: Seen = Arc::new(Mutex::new(vec![]));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for sock in l.incoming() {
                let mut sock = sock.unwrap();
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let key = req
                    .lines()
                    .find_map(|h| {
                        h.to_lowercase()
                            .strip_prefix("x-api-key:")
                            .map(|v| v.trim().to_string())
                    })
                    .unwrap_or_default();
                s2.lock().unwrap().push((line, key));
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (port, seen)
    }

    #[test]
    fn normalizes_the_forms_people_type() {
        for s in [
            "///Filled.Count.Soap",
            " filled.count.soap ",
            "filled count soap",
            "filled。count。soap",
        ] {
            assert_eq!(normalize(s).unwrap(), "filled.count.soap", "{s:?}");
        }
        for s in [
            "",
            "filled.count",
            "a.b.c.d",
            "38.5,-92.5",
            "fil1ed.count.soap",
            "///",
        ] {
            assert_eq!(normalize(s), Err(W3wError::BadFormat), "{s:?}");
        }
        assert!(looks_like_words("///a.b.c"));
        assert!(looks_like_words("filled.count.soap"));
        assert!(!looks_like_words("Jefferson City"));
        assert!(!looks_like_words("one two three"));
    }

    #[test]
    fn converts_words_and_sends_key_in_header_only() {
        let (port, seen) = serve(200, CONVERT);
        let c = W3wClient::new(&format!("http://127.0.0.1:{port}"), "SECRETKEY").unwrap();
        let sq = c.to_square("///Filled.Count.Soap").unwrap();
        assert_eq!(sq.words, "filled.count.soap");
        assert_eq!(sq.nearest_place, "Bayswater, London");
        assert!(sq.contains(sq.center));
        let ring = sq.ring();
        assert_eq!(ring[0], ring[4]);
        let (line, key) = seen.lock().unwrap()[0].clone();
        assert!(
            line.contains("/v3/convert-to-coordinates?words=filled.count.soap"),
            "{line}"
        );
        assert_eq!(key, "secretkey");
        assert!(!line.to_lowercase().contains("secretkey"), "key in URL");
        assert!(!format!("{c:?}").to_lowercase().contains("secretkey"));
    }

    #[test]
    fn position_to_words_and_grid_box_order() {
        let (port, seen) = serve(200, CONVERT);
        let c = W3wClient::new(&format!("http://127.0.0.1:{port}"), "k").unwrap();
        c.to_words(LngLat::new(-0.195521, 51.520847)).unwrap();
        let l = seen.lock().unwrap()[0].0.clone();
        assert!(
            l.contains("coordinates=51.520847%2C-0.195521")
                || l.contains("coordinates=51.520847,-0.195521"),
            "{l}"
        );

        let (port, seen) = serve(200, GRID);
        let c = W3wClient::new(&format!("http://127.0.0.1:{port}"), "k").unwrap();
        let lines = c.grid([-0.2, 51.5, -0.19, 51.51]).unwrap();
        assert_eq!(lines.len(), 2);
        let l = seen.lock().unwrap()[0].0.clone();
        assert!(
            l.contains("51.500000%2C-0.200000%2C51.510000%2C-0.190000")
                || l.contains("51.500000,-0.200000,51.510000,-0.190000"),
            "{l}"
        );
    }

    #[test]
    fn errors_become_readable_messages() {
        for (status, body, want) in [
            (
                400,
                r#"{"error":{"code":"BadWords","message":"x"}}"#,
                "doesn't exist",
            ),
            (
                401,
                r#"{"error":{"code":"InvalidKey","message":"x"}}"#,
                "API key",
            ),
            (
                402,
                r#"{"error":{"code":"QuotaExceeded","message":"x"}}"#,
                "plan",
            ),
            (429, "", "rate limit"),
            (500, "<html>", "HTTP 500"),
        ] {
            let e = api_error(status, body).to_string();
            assert!(e.contains(want), "{status}: {e}");
        }
    }

    #[test]
    fn end_to_end_error_and_missing_key() {
        let (port, _) = serve(401, r#"{"error":{"code":"InvalidKey","message":"bad"}}"#);
        let c = W3wClient::new(&format!("http://127.0.0.1:{port}"), "k").unwrap();
        assert!(matches!(c.to_square("a.b.c"), Err(W3wError::Api(m)) if m.contains("API key")));
        assert_eq!(
            W3wClient::new(DEFAULT_BASE, "  ").unwrap_err(),
            W3wError::NoKey
        );
        assert!(W3wClient::new("ftp://x", "k").is_err());
        // Nothing listening: a message, not a panic, and it never contains the key.
        let c = W3wClient::new("http://127.0.0.1:9", "TOPSECRET").unwrap();
        let e = c.to_square("a.b.c").unwrap_err().to_string();
        assert!(
            e.contains("could not reach") && !e.contains("TOPSECRET"),
            "{e}"
        );
    }

    #[test]
    fn grid_box_is_kept_within_the_api_limit() {
        let small = [-92.0, 38.0, -91.999, 38.001];
        assert_eq!(clamp_grid_box(small), small);
        let big = clamp_grid_box([-93.0, 38.0, -92.0, 39.0]);
        let d = smf_geo::distance_m(LngLat::new(big[0], big[1]), LngLat::new(big[2], big[3]));
        assert!(d <= MAX_GRID_DIAGONAL_M, "{d}");
        assert!(
            ((big[0] + big[2]) / 2.0 - (-92.5)).abs() < 1e-9,
            "stays centred"
        );
    }
}
