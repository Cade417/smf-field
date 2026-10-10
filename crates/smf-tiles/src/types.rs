//! Types shared by the desktop and browser tile services.

use smf_geo::TileId;

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

pub fn decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() != 256 || img.height() != 256 {
        img.resize_exact(256, 256, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    Some(img.to_rgba8().into_raw())
}
