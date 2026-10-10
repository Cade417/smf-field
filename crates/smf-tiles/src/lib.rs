//! Tile sources, caching and the background fetch pipeline. No GPU, no UI.
//!
//! On the desktop tiles come from worker threads with an SQLite disk cache, offline areas and
//! local files. In the browser there are no threads, files or SQLite, so a smaller service
//! built on `fetch` provides the same `TileService` API for network sources.

#[cfg(not(target_arch = "wasm32"))]
mod cache;
#[cfg(not(target_arch = "wasm32"))]
mod download;
#[cfg(not(target_arch = "wasm32"))]
mod geotiff;
#[cfg(not(target_arch = "wasm32"))]
mod mbtiles;
#[cfg(not(target_arch = "wasm32"))]
mod scheduler;
#[cfg(target_arch = "wasm32")]
#[path = "scheduler_web.rs"]
mod scheduler;
mod source;
mod types;
#[cfg(target_arch = "wasm32")]
mod web_stubs;

#[cfg(not(target_arch = "wasm32"))]
pub use cache::DiskCache;
#[cfg(not(target_arch = "wasm32"))]
pub use download::{
    DEFAULT_MAX_TILES, Download, DownloadError, DownloadSpec, Estimate, OfflineArea, Progress,
    estimate, read_offline_area, scan_offline_dir, start as start_download,
};
#[cfg(not(target_arch = "wasm32"))]
pub use geotiff::{GeoRaster, RasterCrs, crs_from_epsg};
#[cfg(not(target_arch = "wasm32"))]
pub use mbtiles::Mbtiles;
pub use scheduler::TileService;
pub use source::{Backend, SourceKind, TileSource, basemaps, default_sources, terrain_source};
pub use types::{LocalProvider, TileData, TileEvent, decode};
#[cfg(target_arch = "wasm32")]
pub use web_stubs::{
    DEFAULT_MAX_TILES, Download, DownloadError, DownloadSpec, Estimate, GeoRaster, Mbtiles,
    OfflineArea, Progress, estimate, read_offline_area, scan_offline_dir, start_download,
};
