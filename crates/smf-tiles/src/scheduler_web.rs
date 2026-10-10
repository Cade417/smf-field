//! The browser's tile service: the same API as the desktop `TileService`, built on `fetch`.
//!
//! There are no threads, files or SQLite in a browser, so there is no disk cache here (the
//! browser's own HTTP cache does that job), no offline areas and no local-file sources. Requests
//! are limited to a handful in flight; queued work from older frames is dropped before it is
//! sent, so fast panning never builds a backlog of tiles that are no longer on screen.

use crate::types::{TileEvent, decode};
use crate::{Backend, TileSource};
use smf_geo::TileId;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Browsers cap connections per host (about six for HTTP/1.1); a few more keeps them full.
const MAX_ACTIVE: usize = 8;

struct Pending {
    source: Arc<TileSource>,
    id: TileId,
    priority: f32,
    generation: u64,
}

struct State {
    generation: u64,
    queue: Vec<Pending>,
    in_flight: HashSet<(String, TileId)>,
    active: usize,
    offline: bool,
}

#[derive(Clone)]
pub struct TileService {
    state: Arc<Mutex<State>>,
    tx: Sender<TileEvent>,
    rx: Arc<Mutex<Receiver<TileEvent>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl TileService {
    /// `cache_dir` is ignored in browsers. `wake` is called whenever a tile arrives.
    pub fn new(_cache_dir: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        Self {
            state: Arc::new(Mutex::new(State {
                generation: 0,
                queue: vec![],
                in_flight: HashSet::new(),
                active: 0,
                offline: false,
            })),
            tx,
            rx: Arc::new(Mutex::new(rx)),
            wake: Arc::new(wake),
        }
    }

    pub fn set_offline(&self, offline: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.offline = offline;
        }
    }

    pub fn is_offline(&self) -> bool {
        self.state.lock().map(|s| s.offline).unwrap_or(false)
    }

    /// Network sources need no setup; local files do not exist in a browser.
    pub fn register_source(&self, source: &TileSource) -> Result<(), String> {
        match source.backend {
            Backend::Http | Backend::Wms { .. } => Ok(()),
            _ => Err("local tile files are only available in the desktop app".into()),
        }
    }

    pub fn unmount_all(&self) {}

    /// Offline areas do not exist in browsers.
    pub fn mount_offline(&self, _source_id: &str, _area: Arc<crate::Mbtiles>) {}

    /// Start a new frame: requests tagged with older generations become stale.
    pub fn begin_frame(&self) -> u64 {
        match self.state.lock() {
            Ok(mut s) => {
                s.generation += 1;
                s.generation
            }
            Err(_) => 0,
        }
    }

    /// Lower priority value loads first.
    pub fn request(&self, source: &Arc<TileSource>, id: TileId, priority: f32) {
        {
            let Ok(mut s) = self.state.lock() else { return };
            let generation = s.generation;
            let key = (source.id.clone(), id);
            if !s.in_flight.insert(key) {
                // Already queued or loading: refresh it so it is not dropped as stale.
                if let Some(p) = s
                    .queue
                    .iter_mut()
                    .find(|p| p.id == id && p.source.id == source.id)
                {
                    p.generation = generation;
                    p.priority = priority;
                }
                return;
            }
            if s.offline {
                s.in_flight.remove(&(source.id.clone(), id));
                drop(s);
                let _ = self.tx.send(TileEvent::Failed {
                    source: source.id.clone(),
                    id,
                });
                (self.wake)();
                return;
            }
            s.queue.push(Pending {
                source: source.clone(),
                id,
                priority,
                generation,
            });
        }
        pump(&self.state, &self.tx, &self.wake);
    }

    pub fn try_recv(&self) -> Option<TileEvent> {
        self.rx.lock().ok()?.try_recv().ok()
    }
}

/// Start queued requests while there is room, best priority first.
fn pump(state: &Arc<Mutex<State>>, tx: &Sender<TileEvent>, wake: &Arc<dyn Fn() + Send + Sync>) {
    loop {
        let job = {
            let Ok(mut s) = state.lock() else { return };
            if s.active >= MAX_ACTIVE {
                return;
            }
            // The UI re-requests needed tiles every frame, so anything more than one frame old is off-screen.
            let current = s.generation;
            let stale: Vec<(String, TileId)> = s
                .queue
                .iter()
                .filter(|p| p.generation + 1 < current)
                .map(|p| (p.source.id.clone(), p.id))
                .collect();
            if !stale.is_empty() {
                s.queue.retain(|p| p.generation + 1 >= current);
                for k in stale {
                    s.in_flight.remove(&k);
                }
            }
            let Some(i) = s
                .queue
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.priority.total_cmp(&b.1.priority))
                .map(|(i, _)| i)
            else {
                return;
            };
            s.active += 1;
            s.queue.swap_remove(i)
        };
        let (state2, tx2, wake2) = (state.clone(), tx.clone(), wake.clone());
        let url = job.source.tile_url(job.id);
        let (source_id, id) = (job.source.id.clone(), job.id);
        smf_net::fetch(smf_net::Request::get(url), move |res| {
            let rgba = res
                .ok()
                .filter(|r| r.ok())
                // Only accept bytes that decode, so an error page is not shown as a tile.
                .and_then(|r| decode(&r.bytes));
            let ev = match rgba {
                Some(rgba) => TileEvent::Ready {
                    source: source_id.clone(),
                    id,
                    rgba,
                },
                None => TileEvent::Failed {
                    source: source_id.clone(),
                    id,
                },
            };
            if let Ok(mut s) = state2.lock() {
                s.active = s.active.saturating_sub(1);
                s.in_flight.remove(&(source_id, id));
            }
            let _ = tx2.send(ev);
            wake2();
            pump(&state2, &tx2, &wake2);
        });
    }
}
