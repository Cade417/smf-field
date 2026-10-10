use crate::{DiskCache, TileSource};
use crossbeam_channel::{Receiver, Sender, unbounded};
use smf_geo::TileId;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
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
}

/// Prioritized background loader. Each frame the UI bumps the generation and
/// re-requests what it needs; queued work from older generations is dropped
/// before any IO so fast panning never builds a backlog of tiles now off-screen.
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
        });
        let (tx, rx) = unbounded();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let client = reqwest::blocking::Client::builder()
            // OSM and others require an identifying User-Agent.
            .user_agent(concat!(
                "ShowMeFireField/",
                env!("CARGO_PKG_VERSION"),
                " (cwoolaway@centralcrossingfpd.org)"
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
        open_cache(&self.caches, &self.cache_dir, source_id)
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

fn open_cache(map: &CacheMap, dir: &Path, id: &str) -> Arc<DiskCache> {
    let mut m = map.lock().unwrap();
    m.entry(id.to_string())
        .or_insert_with(|| {
            Arc::new(DiskCache::open(&dir.join(format!("{id}.sqlite"))).expect("open tile cache"))
        })
        .clone()
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
        let cache = open_cache(&caches, &cache_dir, &req.source.id);
        let bytes = cache.get(req.id).or_else(|| {
            let url = req.source.tile_url(req.id);
            let resp = client.get(&url).send().ok()?.error_for_status().ok()?;
            let b = resp.bytes().ok()?.to_vec();
            // Only cache bytes that decode, so a captive-portal HTML page can't poison the cache.
            decode(&b)?;
            cache.put(req.id, &b);
            Some(b)
        });
        let ev = match bytes.and_then(|b| decode(&b)) {
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

fn decode(bytes: &[u8]) -> Option<Vec<u8>> {
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

    /// End to end against a local HTTP server: fetch, decode, cache, then serve from cache with the server gone.
    #[test]
    fn fetches_caches_and_works_offline() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = png.clone();
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

        let dir = std::env::temp_dir().join(format!("smf-sched-test-{}", std::process::id()));
        let svc = TileService::new(dir.clone(), || {});
        let src = Arc::new(TileSource {
            id: "t".into(),
            name: "t".into(),
            url: format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}"),
            subdomains: vec![],
            max_zoom: 10,
            attribution: String::new(),
            offline_allowed: true,
        });
        let id = TileId::new(1, 0, 0);
        let wait = |svc: &TileService| {
            for _ in 0..200 {
                if let Some(e) = svc.try_recv() {
                    return e;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("timeout");
        };
        svc.begin_frame();
        svc.request(&src, id, 0.0);
        match wait(&svc) {
            TileEvent::Ready { rgba, .. } => assert_eq!(&rgba[..4], &[10, 20, 30, 255]),
            _ => panic!("expected Ready"),
        }
        server.join().unwrap();
        // Server is gone; the same tile must now come from disk.
        svc.begin_frame();
        svc.request(&src, id, 0.0);
        assert!(matches!(wait(&svc), TileEvent::Ready { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }
}
