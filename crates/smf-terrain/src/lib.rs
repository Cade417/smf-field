//! Terrain logic with no GPU or IO: elevation decoding/sampling, the 3D orbit
//! camera, frustum culling and LOD chunk selection.
//!
//! Scene units are Web Mercator metres. Elevations are multiplied by
//! `1 / cos(lat)` (the Mercator scale factor at the camera target) so slopes and
//! relief look right instead of being flattened at mid latitudes.

mod camera;
mod height;
mod lod;
mod profile;

pub use camera::Camera3D;
pub use height::{HeightField, HeightTile, TERRAIN_MAX_ZOOM, decode_terrarium};
pub use lod::{Chunk, select_chunks, tile_bounds_mercator};
pub use profile::{Profile, elevation_profile, profile_tiles, resample};
