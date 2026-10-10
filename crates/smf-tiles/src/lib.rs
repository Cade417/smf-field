//! Tile sources, disk cache and the background fetch pipeline. No GPU, no UI.

mod cache;
mod scheduler;
mod source;

pub use cache::DiskCache;
pub use scheduler::{TileEvent, TileService};
pub use source::{SourceKind, TileSource, basemaps, default_sources, terrain_source};
