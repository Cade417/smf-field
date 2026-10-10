//! Browser stand-ins for the desktop-only tile features (offline areas, downloads, local
//! MBTiles and GeoTIFF files). They have the same shapes as the real types so the app's
//! screens compile unchanged, but anything that would touch the file system refuses with a
//! clear message. The browser's own HTTP cache takes over the caching job.

use crate::{SourceKind, TileSource};
use smf_geo::{TileId, tile_range};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const DESKTOP_ONLY: &str = "This is only available in the desktop app.";

pub const DEFAULT_MAX_TILES: u64 = 400_000;

#[derive(Debug)]
pub struct Mbtiles;

impl Mbtiles {
    pub fn open_read_only(_path: &Path) -> Result<Self, String> {
        Err(DESKTOP_ONLY.into())
    }
    pub fn zoom_range(&self) -> Option<(u8, u8)> {
        None
    }
    pub fn meta(&self, _name: &str) -> Option<String> {
        None
    }
    pub fn get(&self, _t: TileId) -> Option<Vec<u8>> {
        None
    }
    pub fn count(&self) -> u64 {
        0
    }
}

#[derive(Debug)]
pub struct GeoRaster;

impl GeoRaster {
    pub fn open(_path: &Path) -> Result<Self, String> {
        Err(DESKTOP_ONLY.into())
    }
    pub fn bounds(&self) -> Option<[f64; 4]> {
        None
    }
    pub fn native_max_zoom(&self) -> u8 {
        0
    }
    pub fn render_tile(&self, _id: TileId) -> Vec<u8> {
        vec![]
    }
    pub fn intersects(&self, _id: TileId) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub source: TileSource,
    /// (min_lng, min_lat, max_lng, max_lat)
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub out: PathBuf,
    pub max_tiles: u64,
    pub workers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub tiles: u64,
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
                "{n} does not allow bulk/offline download in its terms of use."
            ),
            DownloadError::Unsupported(m)
            | DownloadError::InvalidArea(m)
            | DownloadError::Io(m) => write!(f, "{m}"),
            DownloadError::TooLarge { tiles, limit } => {
                write!(f, "{tiles} tiles is over the {limit} tile limit.")
            }
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
    pub done: AtomicU64,
    pub downloaded: AtomicU64,
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
        true
    }
    pub fn join(self) {}
}

pub fn start_download(_spec: DownloadSpec) -> Result<Download, DownloadError> {
    Err(DownloadError::Unsupported(DESKTOP_ONLY.into()))
}

#[derive(Debug, Clone)]
pub struct OfflineArea {
    pub path: PathBuf,
    pub name: String,
    pub source_id: Option<String>,
    pub kind: Option<String>,
    pub tiles: u64,
    pub zooms: Option<(u8, u8)>,
    pub bytes: u64,
    pub bounds: Option<[f64; 4]>,
}

impl OfflineArea {
    pub fn open(&self) -> Result<Arc<Mbtiles>, String> {
        Err(DESKTOP_ONLY.into())
    }
}

pub fn read_offline_area(_path: &Path) -> Option<OfflineArea> {
    None
}

pub fn scan_offline_dir(_dir: &Path) -> Vec<OfflineArea> {
    vec![]
}
