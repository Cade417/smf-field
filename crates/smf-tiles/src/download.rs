//! Offline area downloader: fetch every tile of an area/zoom range into an MBTiles file.
//! Resumable (existing tiles are skipped), pausable, cancellable, and it refuses sources
//! whose terms forbid bulk download.

use crate::{Backend, Mbtiles, SourceKind, TileSource};
use smf_geo::{TileId, tile_range};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

/// Refuse a single job above this many tiles unless the caller raises it explicitly.
pub const DEFAULT_MAX_TILES: u64 = 400_000;

#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub source: TileSource,
    /// (min_lng, min_lat, max_lng, max_lat)
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub out: PathBuf,
    /// 0 means `DEFAULT_MAX_TILES`.
    pub max_tiles: u64,
    /// 0 means 4. Capped at 8 to stay polite to tile servers.
    pub workers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub tiles: u64,
    /// Rough size: real tiles vary a lot with content, so this is only for the "are you sure?" prompt.
    pub approx_bytes: u64,
}

#[derive(Debug, PartialEq)]
pub enum DownloadError {
    NotAllowed(String),
    Unsupported(String),
    TooLarge { tiles: u64, limit: u64 },
    InvalidArea(String),
    Io(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::NotAllowed(n) => write!(
                f,
                "{n} does not allow bulk/offline download in its terms of use. Choose a different basemap (e.g. USGS)."
            ),
            DownloadError::Unsupported(m)
            | DownloadError::InvalidArea(m)
            | DownloadError::Io(m) => write!(f, "{m}"),
            DownloadError::TooLarge { tiles, limit } => write!(
                f,
                "{tiles} tiles is over the {limit} tile limit; choose a smaller area or lower maximum zoom."
            ),
        }
    }
}

pub fn estimate(source: &TileSource, bounds: [f64; 4], min_zoom: u8, max_zoom: u8) -> Estimate {
    let tiles: u64 = (min_zoom..=max_zoom)
        .map(|z| {
            let (xs, ys) = tile_range(bounds, z);
            xs.count() as u64 * ys.count() as u64
        })
        .sum();
    let avg = if source.kind == SourceKind::Terrarium {
        40_000
    } else {
        25_000
    };
    Estimate {
        tiles,
        approx_bytes: tiles * avg,
    }
}

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    /// Tiles handled so far (downloaded + skipped + failed).
    pub done: AtomicU64,
    pub downloaded: AtomicU64,
    /// Already present (resume) or reported missing by the server (404).
    pub skipped: AtomicU64,
    pub failed: AtomicU64,
    pub bytes: AtomicU64,
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
    pub finished: AtomicBool,
    pub last_error: Mutex<Option<String>>,
}

pub struct Download {
    pub progress: Arc<Progress>,
    pub out: PathBuf,
    handle: Option<JoinHandle<()>>,
}

impl Download {
    pub fn pause(&self) {
        self.progress.paused.store(true, Ordering::Relaxed);
    }
    pub fn resume(&self) {
        self.progress.paused.store(false, Ordering::Relaxed);
    }
    pub fn cancel(&self) {
        self.progress.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn is_finished(&self) -> bool {
        self.progress.finished.load(Ordering::Relaxed)
    }
    /// Block until the job ends (tests, shutdown).
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn sniff_format(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else {
        "png"
    }
}

pub fn start(spec: DownloadSpec) -> Result<Download, DownloadError> {
    if !spec.source.offline_allowed {
        return Err(DownloadError::NotAllowed(spec.source.name.clone()));
    }
    if !matches!(spec.source.backend, Backend::Http | Backend::Wms { .. }) {
        return Err(DownloadError::Unsupported(
            "Only network sources can be downloaded (local files are already offline).".into(),
        ));
    }
    let b = spec.bounds;
    if !(b.iter().all(|v| v.is_finite())
        && b[0] < b[2]
        && b[1] < b[3]
        && (-180.0..=180.0).contains(&b[0])
        && (-180.0..=180.0).contains(&b[2])
        && (-90.0..=90.0).contains(&b[1])
        && (-90.0..=90.0).contains(&b[3]))
    {
        return Err(DownloadError::InvalidArea(
            "The area is empty or outside the valid longitude/latitude range.".into(),
        ));
    }
    if spec.min_zoom > spec.max_zoom || spec.max_zoom > 22 {
        return Err(DownloadError::InvalidArea(
            "Zoom range is invalid (need min <= max <= 22).".into(),
        ));
    }
    if spec.max_zoom > spec.source.max_zoom {
        return Err(DownloadError::InvalidArea(format!(
            "{} has no tiles above zoom {}.",
            spec.source.name, spec.source.max_zoom
        )));
    }
    let est = estimate(&spec.source, b, spec.min_zoom, spec.max_zoom);
    let limit = if spec.max_tiles == 0 {
        DEFAULT_MAX_TILES
    } else {
        spec.max_tiles
    };
    if est.tiles > limit {
        return Err(DownloadError::TooLarge {
            tiles: est.tiles,
            limit,
        });
    }
    let mb = Arc::new(
        Mbtiles::create(&spec.out)
            .map_err(|e| DownloadError::Io(format!("{}: {e}", spec.out.display())))?,
    );
    // Resuming into a file from a different source would silently mix imagery.
    match mb.meta("smf_source") {
        Some(s) if s != spec.source.id => {
            return Err(DownloadError::Io(format!(
                "{} already holds tiles from another source ({s}); choose a new file name.",
                spec.out.display()
            )));
        }
        _ => {}
    }
    let name = spec
        .out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = |k: &str, v: &str| {
        mb.set_meta(k, v)
            .map_err(|e| DownloadError::Io(e.to_string()))
    };
    meta("name", &name)?;
    meta("type", "baselayer")?;
    meta("version", "1.0")?;
    meta(
        "description",
        &format!(
            "Offline area of {} made by ShowMeFire Field",
            spec.source.name
        ),
    )?;
    meta("bounds", &format!("{},{},{},{}", b[0], b[1], b[2], b[3]))?;
    meta("minzoom", &spec.min_zoom.to_string())?;
    meta("maxzoom", &spec.max_zoom.to_string())?;
    meta("attribution", &spec.source.attribution)?;
    meta("smf_source", &spec.source.id)?;
    meta(
        "smf_kind",
        if spec.source.kind == SourceKind::Terrarium {
            "terrarium"
        } else {
            "raster"
        },
    )?;
    mb.flush().map_err(|e| DownloadError::Io(e.to_string()))?;

    // Low zooms first: a cancelled job still leaves a usable, if coarse, area.
    let mut queue: VecDeque<TileId> = VecDeque::with_capacity(est.tiles as usize);
    for z in spec.min_zoom..=spec.max_zoom {
        let (xs, ys) = tile_range(b, z);
        for y in ys.clone() {
            for x in xs.clone() {
                queue.push_back(TileId::new(z, x, y));
            }
        }
    }
    let progress = Arc::new(Progress::default());
    progress.total.store(queue.len() as u64, Ordering::Relaxed);
    let queue = Arc::new(Mutex::new(queue));
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "ShowMeFireField/",
            env!("CARGO_PKG_VERSION"),
            " (+https://showmefire.org)"
        ))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    let workers = if spec.workers == 0 {
        4
    } else {
        spec.workers.min(8)
    };
    let source = Arc::new(spec.source);
    let out = spec.out.clone();
    let p2 = progress.clone();
    let handle = std::thread::spawn(move || {
        let format: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let threads: Vec<_> = (0..workers)
            .map(|_| {
                let (queue, mb, client, source, progress, format) = (
                    queue.clone(),
                    mb.clone(),
                    client.clone(),
                    source.clone(),
                    p2.clone(),
                    format.clone(),
                );
                std::thread::spawn(move || worker(queue, mb, client, source, progress, format))
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
        if let Some(f) = *format.lock().unwrap() {
            let _ = mb.set_meta("format", f);
        }
        let _ = mb.flush();
        p2.finished.store(true, Ordering::Relaxed);
    });
    Ok(Download {
        progress,
        out,
        handle: Some(handle),
    })
}

fn worker(
    queue: Arc<Mutex<VecDeque<TileId>>>,
    mb: Arc<Mbtiles>,
    client: reqwest::blocking::Client,
    source: Arc<TileSource>,
    progress: Arc<Progress>,
    format: Arc<Mutex<Option<&'static str>>>,
) {
    loop {
        if progress.cancelled.load(Ordering::Relaxed) {
            return;
        }
        while progress.paused.load(Ordering::Relaxed) && !progress.cancelled.load(Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let Some(id) = queue.lock().unwrap().pop_front() else {
            return;
        };
        if mb.has(id) {
            progress.skipped.fetch_add(1, Ordering::Relaxed);
            progress.done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match fetch(&client, &source.tile_url(id)) {
            Ok(Some(bytes)) => {
                if image::load_from_memory(&bytes).is_err() {
                    fail(
                        &progress,
                        format!("tile {}/{}/{} is not a valid image", id.z, id.x, id.y),
                    );
                } else if let Err(e) = mb.put(id, &bytes) {
                    fail(&progress, format!("could not write tile: {e}"));
                } else {
                    format.lock().unwrap().get_or_insert(sniff_format(&bytes));
                    progress
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    progress.downloaded.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(None) => {
                progress.skipped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => fail(&progress, e),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
}

fn fail(progress: &Progress, msg: String) {
    progress.failed.fetch_add(1, Ordering::Relaxed);
    *progress.last_error.lock().unwrap() = Some(msg);
}

/// Ok(None) for "no such tile" (404/204/410): normal at the edge of coverage, not an error.
fn fetch(client: &reqwest::blocking::Client, url: &str) -> Result<Option<Vec<u8>>, String> {
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300 * attempt));
        }
        match client.get(url).send() {
            Ok(r) => {
                let s = r.status();
                if matches!(s.as_u16(), 404 | 204 | 410) {
                    return Ok(None);
                }
                if s.is_success() {
                    match r.bytes() {
                        Ok(b) => return Ok(Some(b.to_vec())),
                        Err(e) => last = e.to_string(),
                    }
                } else if s.is_client_error() && s.as_u16() != 429 {
                    return Err(format!("server refused tile ({s})"));
                } else {
                    last = format!("server error ({s})");
                }
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// An offline area file found on disk.
#[derive(Debug, Clone)]
pub struct OfflineArea {
    pub path: PathBuf,
    pub name: String,
    /// Which tile source this file can stand in for (our own files record it).
    pub source_id: Option<String>,
    pub kind: Option<String>,
    pub tiles: u64,
    pub zooms: Option<(u8, u8)>,
    pub bytes: u64,
    pub bounds: Option<[f64; 4]>,
}

impl OfflineArea {
    pub fn open(&self) -> Result<Arc<Mbtiles>, String> {
        Mbtiles::open_read_only(&self.path)
            .map(Arc::new)
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

pub fn read_offline_area(path: &Path) -> Option<OfflineArea> {
    let m = Mbtiles::open_read_only(path).ok()?;
    let bounds = m.meta("bounds").and_then(|s| {
        let v: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
    });
    Some(OfflineArea {
        path: path.to_owned(),
        name: m.meta("name").unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        source_id: m.meta("smf_source"),
        kind: m.meta("smf_kind"),
        tiles: m.count(),
        zooms: m.zoom_range(),
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        bounds,
    })
}

/// All usable `.mbtiles` files in a directory, sorted by name.
pub fn scan_offline_dir(dir: &Path) -> Vec<OfflineArea> {
    let mut v: Vec<OfflineArea> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mbtiles"))
        })
        .filter_map(|p| read_offline_area(&p))
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    fn png() -> Vec<u8> {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([9, 99, 199, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    /// A tiny multi-connection HTTP server: paths containing "/404/" give 404, the rest a PNG.
    /// Returns (port, request counter, stop flag).
    fn serve() -> (u16, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s, body) = (hits.clone(), stop.clone(), png());
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let (h, body) = (h.clone(), body.clone());
                        std::thread::spawn(move || {
                            sock.set_nonblocking(false).unwrap();
                            let mut buf = [0u8; 2048];
                            let n = sock.read(&mut buf).unwrap_or(0);
                            let req = String::from_utf8_lossy(&buf[..n]).to_string();
                            h.fetch_add(1, Ordering::Relaxed);
                            if req.contains("/404/") {
                                let _ = write!(
                                    sock,
                                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                );
                            } else {
                                let _ = write!(
                                    sock,
                                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = sock.write_all(&body);
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        (port, hits, stop)
    }

    fn spec(port: u16, out: PathBuf) -> DownloadSpec {
        let mut s = crate::default_sources().remove(0);
        s.id = "test".into();
        s.name = "Test source".into();
        s.url = format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}");
        s.offline_allowed = true;
        s.max_zoom = 12;
        DownloadSpec {
            source: s,
            bounds: [-92.35, 38.55, -92.25, 38.62],
            min_zoom: 8,
            max_zoom: 10,
            out,
            max_tiles: 0,
            workers: 3,
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("smf-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn downloads_an_area_into_standard_mbtiles_and_resumes() {
        let (port, hits, stop) = serve();
        let dir = tmp("ok");
        let s = spec(port, dir.join("area.mbtiles"));
        let est = estimate(&s.source, s.bounds, s.min_zoom, s.max_zoom);
        assert!(est.tiles >= 3 && est.approx_bytes == est.tiles * 25_000);
        let d = start(s.clone()).unwrap();
        let p = d.progress.clone();
        d.join();
        assert!(p.finished.load(Ordering::Relaxed));
        assert_eq!(p.total.load(Ordering::Relaxed), est.tiles);
        assert_eq!(p.downloaded.load(Ordering::Relaxed), est.tiles);
        assert_eq!(
            p.failed.load(Ordering::Relaxed),
            0,
            "{:?}",
            p.last_error.lock().unwrap()
        );
        assert_eq!(hits.load(Ordering::Relaxed) as u64, est.tiles);

        let area = read_offline_area(&dir.join("area.mbtiles")).unwrap();
        assert_eq!(area.tiles, est.tiles);
        assert_eq!(area.zooms, Some((8, 10)));
        assert_eq!(area.source_id.as_deref(), Some("test"));
        assert_eq!(area.bounds, Some([-92.35, 38.55, -92.25, 38.62]));
        let m = area.open().unwrap();
        assert_eq!(m.meta("format").as_deref(), Some("png"));
        assert!(m.has(TileId::from_lnglat(smf_geo::LngLat::new(-92.3, 38.58), 10)));

        // Second run: everything is already there, so nothing hits the network.
        let before = hits.load(Ordering::Relaxed);
        let d2 = start(s).unwrap();
        let p2 = d2.progress.clone();
        d2.join();
        assert_eq!(
            hits.load(Ordering::Relaxed),
            before,
            "resume must not refetch"
        );
        assert_eq!(p2.skipped.load(Ordering::Relaxed), est.tiles);
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_restricted_sources_oversized_jobs_and_bad_input() {
        let dir = tmp("refuse");
        let mut s = spec(9, dir.join("a.mbtiles"));
        s.source.offline_allowed = false;
        let err = start(s.clone()).err().unwrap();
        assert!(matches!(err, DownloadError::NotAllowed(_)));
        assert!(err.to_string().contains("Test source"));
        assert!(
            !dir.join("a.mbtiles").exists(),
            "refusal must not create a file"
        );

        s.source.offline_allowed = true;
        s.max_tiles = 2;
        assert!(matches!(
            start(s.clone()).err().unwrap(),
            DownloadError::TooLarge { .. }
        ));
        s.max_tiles = 0;
        s.min_zoom = 11;
        s.max_zoom = 10;
        assert!(matches!(
            start(s.clone()).err().unwrap(),
            DownloadError::InvalidArea(_)
        ));
        s.min_zoom = 8;
        s.max_zoom = 13; // beyond the source's max zoom of 12
        assert!(
            start(s.clone())
                .err()
                .unwrap()
                .to_string()
                .contains("no tiles above zoom 12")
        );
        s.max_zoom = 10;
        s.bounds = [-92.0, 38.0, -93.0, 39.0]; // inverted
        assert!(matches!(
            start(s.clone()).err().unwrap(),
            DownloadError::InvalidArea(_)
        ));
        s.bounds = [f64::NAN, 38.0, -93.0, 39.0];
        assert!(matches!(
            start(s.clone()).err().unwrap(),
            DownloadError::InvalidArea(_)
        ));
        s.bounds = [-92.0, 38.0, -91.0, 39.0];
        s.source.backend = Backend::Mbtiles { path: "x".into() };
        assert!(matches!(
            start(s).err().unwrap(),
            DownloadError::Unsupported(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_tiles_are_skipped_not_failed_and_cancel_stops_early() {
        let (port, _hits, stop) = serve();
        let dir = tmp("skip");
        let mut s = spec(port, dir.join("holes.mbtiles"));
        s.source.url = format!("http://127.0.0.1:{port}/404/{{z}}/{{x}}/{{y}}");
        let d = start(s).unwrap();
        let p = d.progress.clone();
        d.join();
        assert_eq!(p.failed.load(Ordering::Relaxed), 0);
        assert_eq!(p.downloaded.load(Ordering::Relaxed), 0);
        assert_eq!(
            p.skipped.load(Ordering::Relaxed),
            p.total.load(Ordering::Relaxed),
            "404s count as skipped"
        );

        // Cancel before any work: workers exit at once, the job still finishes cleanly.
        let mut s = spec(port, dir.join("cancel.mbtiles"));
        s.min_zoom = 8;
        s.max_zoom = 12;
        s.bounds = [-92.6, 38.3, -92.0, 38.9];
        let d = start(s).unwrap();
        d.pause();
        d.cancel();
        let p = d.progress.clone();
        d.join();
        assert!(p.finished.load(Ordering::Relaxed));
        assert!(p.done.load(Ordering::Relaxed) < p.total.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn will_not_mix_two_sources_in_one_file() {
        let (port, _h, stop) = serve();
        let dir = tmp("mix");
        let s = spec(port, dir.join("a.mbtiles"));
        start(s.clone()).unwrap().join();
        let mut other = s;
        other.source.id = "other".into();
        let err = start(other).err().unwrap().to_string();
        assert!(err.contains("another source"), "{err}");
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_lists_only_valid_mbtiles() {
        let dir = tmp("scan");
        let m = Mbtiles::create(&dir.join("b.mbtiles")).unwrap();
        m.put(TileId::new(1, 0, 0), &png()).unwrap();
        drop(m);
        std::fs::write(dir.join("junk.mbtiles"), b"nope").unwrap();
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();
        let v = scan_offline_dir(&dir);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "b");
        assert_eq!(v[0].source_id, None, "foreign files have no smf_source");
        assert!(scan_offline_dir(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
