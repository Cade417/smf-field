use crate::{Backend, DiskCache, GeoRaster, Mbtiles, TileSource};
use crossbeam_channel::{Receiver, Sender, unbounded};
use smf_geo::TileId;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Decoded RGBA8 256x256 tile, or a failure the UI should back off from.
pub enum TileEvent {
    Ready {
        source: String,
        id: TileId,
        rgba: Vec<u8>,
    },
    Failed {
        source: String,
        id: TileId,
    },
}

/// Tile bytes from a local provider: encoded image bytes (PNG/JPEG) or ready RGBA pixels.
pub enum TileData {
    Encoded(Vec<u8>),
    Rgba(Vec<u8>),
}

/// A tile source that needs no network (local files).
pub trait LocalProvider: Send + Sync {
    fn tile(&self, id: TileId) -> Option<TileData>;
}

struct MbtilesProvider(Arc<Mbtiles>);

impl LocalProvider for MbtilesProvider {
    fn tile(&self, id: TileId) -> Option<TileData> {
        self.0.get(id).map(TileData::Encoded)
    }
}

struct GeoTiffProvider(Arc<GeoRaster>);

impl LocalProvider for GeoTiffProvider {
    fn tile(&self, id: TileId) -> Option<TileData> {
        Some(TileData::Rgba(self.0.render_tile(id)))
    }
}

struct Request {
    source: Arc<TileSource>,
    id: TileId,
    priority: f32,
    generation: u64,
}

struct Shared {
    queue: Mutex<Vec<Request>>,
    wake: Condvar,
    in_flight: Mutex<HashSet<(String, TileId)>>,
    generation: Mutex<u64>,
    providers: Mutex<HashMap<String, Arc<dyn LocalProvider>>>,
    /// Read-only offline areas per source id, consulted before the cache and the network.
    mounts: Mutex<HashMap<String, Vec<Arc<Mbtiles>>>>,
    /// Work offline: never touch the network.
    offline: AtomicBool,
}

/// Prioritized background loader. Each frame the UI bumps the generation and
/// re-requests what it needs; queued work from older generations is dropped
/// before any IO so fast panning never builds a backlog of tiles now off-screen.
///
/// Lookup order per tile: local provider (MBTiles/GeoTIFF source) -> mounted offline
/// areas -> disk cache -> network (unless working offline).
pub struct TileService {
    shared: Arc<Shared>,
    rx: Receiver<TileEvent>,
    cache_dir: PathBuf,
    caches: CacheMap,
}

type CacheMap = Mutex<HashMap<String, Arc<DiskCache>>>;

const WORKERS: usize = 6;

impl TileService {
    /// `wake` is called whenever a tile arrives (the UI passes a repaint request).
    pub fn new(cache_dir: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            in_flight: Mutex::new(HashSet::new()),
            generation: Mutex::new(0),
            providers: Mutex::new(HashMap::new()),
            mounts: Mutex::new(HashMap::new()),
            offline: AtomicBool::new(false),
        });
        let (tx, rx) = unbounded();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let client = reqwest::blocking::Client::builder()
            // OSM and others require an identifying User-Agent.
            .user_agent(concat!(
                "ShowMeFireField/",
                env!("CARGO_PKG_VERSION"),
                " (+https://showmefire.org)"
            ))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client");
        for _ in 0..WORKERS {
            let (shared, tx, wake, client, dir) = (
                shared.clone(),
                tx.clone(),
                wake.clone(),
                client.clone(),
                cache_dir.clone(),
            );
            std::thread::spawn(move || worker(shared, tx, wake, client, dir));
        }
        Self {
            shared,
            rx,
            cache_dir,
            caches: Mutex::new(HashMap::new()),
        }
    }

    pub fn cache_for(&self, source_id: &str) -> Arc<DiskCache> {
        open_cache(&self.caches, &self.cache_dir, source_id).expect("tile cache")
    }

    pub fn set_offline(&self, offline: bool) {
        self.shared.offline.store(offline, Ordering::Relaxed);
    }

    pub fn is_offline(&self) -> bool {
        self.shared.offline.load(Ordering::Relaxed)
    }

    /// Make a source usable: opens its local file for MBTiles/GeoTIFF backends. HTTP and WMS need nothing.
    pub fn register_source(&self, source: &TileSource) -> Result<(), String> {
        let provider: Arc<dyn LocalProvider> = match &source.backend {
            Backend::Http | Backend::Wms { .. } => return Ok(()),
            Backend::Mbtiles { path } => {
                let m = Mbtiles::open_read_only(path)
                    .map_err(|e| format!("{}: not a usable MBTiles file ({e})", path.display()))?;
                Arc::new(MbtilesProvider(Arc::new(m)))
            }
            Backend::Geotiff { path } => {
                Arc::new(GeoTiffProvider(Arc::new(GeoRaster::open(path)?)))
            }
        };
        self.shared
            .providers
            .lock()
            .unwrap()
            .insert(source.id.clone(), provider);
        Ok(())
    }

    /// Serve `source_id` from an offline-area file before the cache or network.
    pub fn mount_offline(&self, source_id: &str, area: Arc<Mbtiles>) {
        let mut m = self.shared.mounts.lock().unwrap();
        m.entry(source_id.to_owned()).or_default().push(area);
    }

    pub fn unmount_all(&self) {
        self.shared.mounts.lock().unwrap().clear();
    }

    /// Start a new frame: requests tagged with older generations become stale.
    pub fn begin_frame(&self) -> u64 {
        let mut g = self.shared.generation.lock().unwrap();
        *g += 1;
        *g
    }

    /// Lower priority value loads first.
    pub fn request(&self, source: &Arc<TileSource>, id: TileId, priority: f32) {
        let generation = *self.shared.generation.lock().unwrap();
        let key = (source.id.clone(), id);
        if !self.shared.in_flight.lock().unwrap().insert(key) {
            // Already queued: refresh generation/priority so it isn't dropped as stale.
            let mut q = self.shared.queue.lock().unwrap();
            if let Some(r) = q
                .iter_mut()
                .find(|r| r.id == id && r.source.id == source.id)
            {
                r.generation = generation;
                r.priority = priority;
            }
            return;
        }
        self.shared.queue.lock().unwrap().push(Request {
            source: source.clone(),
            id,
            priority,
            generation,
        });
        self.shared.wake.notify_one();
    }

    pub fn try_recv(&self) -> Option<TileEvent> {
        self.rx.try_recv().ok()
    }
}

fn open_cache(map: &CacheMap, dir: &Path, id: &str) -> Option<Arc<DiskCache>> {
    let mut m = map.lock().unwrap();
    if let Some(c) = m.get(id) {
        return Some(c.clone());
    }
    match DiskCache::open(&dir.join(format!("{id}.sqlite"))) {
        Ok(c) => {
            let c = Arc::new(c);
            m.insert(id.to_string(), c.clone());
            Some(c)
        }
        Err(e) => {
            // Tiles still load from the network; they just aren't cached until this succeeds.
            tracing::warn!("tile cache for {id} unavailable: {e}");
            None
        }
    }
}

fn worker(
    shared: Arc<Shared>,
    tx: Sender<TileEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    client: reqwest::blocking::Client,
    cache_dir: PathBuf,
) {
    // Each worker owns its connections; SQLite WAL allows concurrent readers and one writer.
    let caches: CacheMap = Mutex::new(HashMap::new());
    loop {
        let req = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                let current = *shared.generation.lock().unwrap();
                // The UI re-requests needed tiles every frame, so anything >1 generation old is off-screen.
                let stale: Vec<_> = q
                    .iter()
                    .filter(|r| r.generation + 1 < current)
                    .map(|r| (r.source.id.clone(), r.id))
                    .collect();
                if !stale.is_empty() {
                    q.retain(|r| r.generation + 1 >= current);
                    let mut f = shared.in_flight.lock().unwrap();
                    for k in stale {
                        f.remove(&k);
                    }
                }
                if let Some(i) = q
                    .iter()
                    .enumerate()
                    .min_by(|a, b| a.1.priority.total_cmp(&b.1.priority))
                    .map(|(i, _)| i)
                {
                    break q.swap_remove(i);
                }
                q = shared.wake.wait(q).unwrap();
            }
        };
        let rgba = load_tile(&shared, &client, &caches, &cache_dir, &req);
        let ev = match rgba {
            Some(rgba) => TileEvent::Ready {
                source: req.source.id.clone(),
                id: req.id,
                rgba,
            },
            None => TileEvent::Failed {
                source: req.source.id.clone(),
                id: req.id,
            },
        };
        shared
            .in_flight
            .lock()
            .unwrap()
            .remove(&(req.source.id.clone(), req.id));
        let _ = tx.send(ev);
        wake();
    }
}

fn load_tile(
    shared: &Shared,
    client: &reqwest::blocking::Client,
    caches: &CacheMap,
    cache_dir: &Path,
    req: &Request,
) -> Option<Vec<u8>> {
    let (source, id) = (&req.source, req.id);
    if let Some(p) = shared.providers.lock().unwrap().get(&source.id).cloned() {
        return match p.tile(id)? {
            TileData::Rgba(v) => Some(v),
            TileData::Encoded(b) => decode(&b),
        };
    }
    // Offline areas win over the cache: they are explicit, complete downloads.
    let mounted = shared
        .mounts
        .lock()
        .unwrap()
        .get(&source.id)
        .cloned()
        .unwrap_or_default();
    for m in &mounted {
        if let Some(b) = m.get(id)
            && let Some(rgba) = decode(&b)
        {
            return Some(rgba);
        }
    }
    let cache = open_cache(caches, cache_dir, &source.id);
    let fetch = |cache: &Option<Arc<DiskCache>>| -> Option<Vec<u8>> {
        let url = source.tile_url(id);
        let resp = client.get(&url).send().ok()?.error_for_status().ok()?;
        let b = resp.bytes().ok()?.to_vec();
        // Only cache bytes that decode, so a captive-portal HTML page can't poison the cache.
        decode(&b)?;
        if let Some(c) = cache {
            c.put(id, &b);
        }
        Some(b)
    };
    let offline = shared.offline.load(Ordering::Relaxed);
    let bytes = match cache.as_ref().and_then(|c| c.get_with_age(id)) {
        // Live-data layers expire: refresh when old, but a stale tile beats no tile (offline, server down).
        Some((stale, age)) if source.max_age_secs.is_some_and(|max| age > max) => {
            if offline {
                Some(stale)
            } else {
                fetch(&cache).or(Some(stale))
            }
        }
        Some((fresh, _)) => Some(fresh),
        None if offline => None,
        None => fetch(&cache),
    };
    bytes.and_then(|b| decode(&b))
}

pub fn decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() != 256 || img.height() != 256 {
        img.resize_exact(256, 256, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    Some(img.to_rgba8().into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn png() -> Vec<u8> {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    fn source(url: String) -> Arc<TileSource> {
        Arc::new(TileSource {
            kind: Default::default(),
            backend: Backend::Http,
            overlay: false,
            id: "t".into(),
            name: "t".into(),
            url,
            subdomains: vec![],
            max_zoom: 10,
            attribution: String::new(),
            offline_allowed: true,
            max_age_secs: None,
        })
    }

    fn wait(svc: &TileService) -> Option<TileEvent> {
        for _ in 0..200 {
            if let Some(e) = svc.try_recv() {
                return Some(e);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("smf-sched-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// End to end against a local HTTP server: fetch, decode, cache, then serve from cache with the server gone.
    #[test]
    fn fetches_caches_and_works_offline() {
        let body = png();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(&body);
        });
        let dir = tmp("net");
        let svc = TileService::new(dir.clone(), || {});
        let src = source(format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}"));
        let id = TileId::new(1, 0, 0);
        svc.begin_frame();
        svc.request(&src, id, 0.0);
        match wait(&svc) {
            Some(TileEvent::Ready { rgba, .. }) => assert_eq!(&rgba[..4], &[10, 20, 30, 255]),
            _ => panic!("expected Ready"),
        }
        server.join().unwrap();
        // Server is gone; the same tile must now come from disk.
        svc.begin_frame();
        svc.request(&src, id, 0.0);
        assert!(matches!(wait(&svc), Some(TileEvent::Ready { .. })));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn work_offline_never_touches_the_network_but_serves_cache_and_mounts() {
        // Nothing listens on this port: any network attempt would fail, so success proves it came from local data.
        let src = source("http://127.0.0.1:9/{z}/{x}/{y}".into());
        let dir = tmp("off");
        let svc = TileService::new(dir.clone(), || {});
        svc.set_offline(true);
        assert!(svc.is_offline());
        let cached = TileId::new(2, 1, 1);
        svc.cache_for("t").put(cached, &png());
        let mounted = TileId::new(3, 2, 2);
        let area = Mbtiles::create(&dir.join("area.mbtiles")).unwrap();
        area.put(mounted, &png()).unwrap();
        area.flush().unwrap();
        svc.mount_offline(
            "t",
            Arc::new(Mbtiles::open_read_only(&dir.join("area.mbtiles")).unwrap()),
        );
        let missing = TileId::new(4, 3, 3);
        svc.begin_frame();
        for (id, p) in [(cached, 0.0), (mounted, 1.0), (missing, 2.0)] {
            svc.request(&src, id, p);
        }
        let mut ready = vec![];
        let mut failed = vec![];
        for _ in 0..3 {
            match wait(&svc) {
                Some(TileEvent::Ready { id, .. }) => ready.push(id),
                Some(TileEvent::Failed { id, .. }) => failed.push(id),
                None => panic!("timeout"),
            }
        }
        ready.sort();
        assert_eq!(ready, vec![cached, mounted]);
        assert_eq!(
            failed,
            vec![missing],
            "uncached tile fails fast instead of hitting the network"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn png_color(c: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([c[0], c[1], c[2], 255]))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Multi-connection HTTP server returning a fixed PNG; counts requests.
    fn serve_png(body: Vec<u8>) -> (u16, Arc<std::sync::atomic::AtomicUsize>, Arc<AtomicBool>) {
        use std::sync::atomic::AtomicUsize;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let (h, body) = (h.clone(), body.clone());
                        std::thread::spawn(move || {
                            sock.set_nonblocking(false).unwrap();
                            let mut buf = [0u8; 2048];
                            let _ = sock.read(&mut buf);
                            h.fetch_add(1, Ordering::Relaxed);
                            let _ = write!(
                                sock,
                                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(&body);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        (port, hits, stop)
    }

    fn first_pixel(e: Option<TileEvent>) -> [u8; 3] {
        match e {
            Some(TileEvent::Ready { rgba, .. }) => [rgba[0], rgba[1], rgba[2]],
            _ => panic!("expected a tile"),
        }
    }

    #[test]
    fn live_layer_tiles_expire_but_stale_beats_nothing() {
        let (port, hits, stop) = serve_png(png_color([0, 0, 255])); // fresh data from the server is blue
        let dir = tmp("ttl");
        let svc = TileService::new(dir.clone(), || {});
        let mut src = (*source(format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}"))).clone();
        src.max_age_secs = Some(60);
        let src = Arc::new(src);
        let id = TileId::new(3, 2, 2);
        let cache = svc.cache_for("t");
        cache.put(id, &png_color([255, 0, 0])); // old cached tile is red
        let ask = |svc: &TileService| {
            svc.begin_frame();
            svc.request(&src, id, 0.0);
            wait(svc)
        };
        // Fresh (age 0 < 60 s): served from cache, server untouched.
        assert_eq!(first_pixel(ask(&svc)), [255, 0, 0]);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
        // Expired: refetched from the server and the cache is updated.
        cache.backdate_all(3600);
        assert_eq!(first_pixel(ask(&svc)), [0, 0, 255]);
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        assert!(
            cache.get_with_age(id).unwrap().1 < 5,
            "cache entry was refreshed"
        );
        // Expired but offline: the stale tile is used rather than showing nothing.
        cache.put(id, &png_color([255, 0, 0]));
        cache.backdate_all(3600);
        svc.set_offline(true);
        assert_eq!(first_pixel(ask(&svc)), [255, 0, 0]);
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "offline never touches the network"
        );
        // Expired with the server down: also falls back to the stale tile.
        svc.set_offline(false);
        stop.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(60));
        cache.backdate_all(3600);
        let dead = Arc::new(TileSource {
            url: "http://127.0.0.1:9/{z}/{x}/{y}".into(),
            ..(*src).clone()
        });
        svc.begin_frame();
        svc.request(&dead, id, 0.0);
        assert_eq!(first_pixel(wait(&svc)), [255, 0, 0]);
        // A source without max_age never expires.
        let mut forever = (*dead).clone();
        forever.max_age_secs = None;
        cache.backdate_all(10_000_000);
        svc.begin_frame();
        svc.request(&Arc::new(forever), id, 0.0);
        assert_eq!(first_pixel(wait(&svc)), [255, 0, 0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mbtiles_and_geotiff_backends_serve_local_tiles() {
        let dir = tmp("local");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local.mbtiles");
        let m = Mbtiles::create(&path).unwrap();
        m.put(TileId::new(5, 4, 3), &png()).unwrap();
        drop(m);
        let mut src = (*source(String::new())).clone();
        src.id = "local".into();
        src.backend = Backend::Mbtiles { path: path.clone() };
        let svc = TileService::new(dir.clone(), || {});
        svc.register_source(&src).unwrap();
        let src = Arc::new(src);
        svc.begin_frame();
        svc.request(&src, TileId::new(5, 4, 3), 0.0);
        assert!(matches!(wait(&svc), Some(TileEvent::Ready { .. })));
        svc.request(&src, TileId::new(5, 0, 0), 0.0);
        assert!(
            matches!(wait(&svc), Some(TileEvent::Failed { .. })),
            "tile not in the file"
        );
        // A bad path is reported at registration, not at draw time.
        let mut bad = (*src).clone();
        bad.backend = Backend::Mbtiles {
            path: dir.join("missing.mbtiles"),
        };
        assert!(svc.register_source(&bad).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
