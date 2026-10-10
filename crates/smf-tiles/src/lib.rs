//! Tile sources, disk cache and the background fetch pipeline. No GPU, no UI.

mod cache;
mod download;
mod geotiff;
mod mbtiles;
mod scheduler;
mod source;

pub use cache::DiskCache;
pub use download::{
    DEFAULT_MAX_TILES, Download, DownloadError, DownloadSpec, Estimate, OfflineArea, Progress,
    estimate, read_offline_area, scan_offline_dir, start as start_download,
};
pub use geotiff::{GeoRaster, RasterCrs, crs_from_epsg};
pub use mbtiles::Mbtiles;
pub use scheduler::{LocalProvider, TileData, TileEvent, TileService};
pub use source::{Backend, SourceKind, TileSource, basemaps, default_sources, terrain_source};
