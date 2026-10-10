use smf_geo::{LngLat, TILE_SIZE, TileId, lnglat_to_world};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Terrarium tiles stop at z15 (~4.8 m/px at the equator); deeper views reuse these.
pub const TERRAIN_MAX_ZOOM: u8 = 15;
const TILE_PX: usize = 256;
/// Half-width of the slope stencil: about the native resolution of the 10-30 m DEM underneath.
const SLOPE_STEP_M: f64 = 15.0;
/// Memory cap for the CPU copy of heights: 600 tiles * 256 KB = ~150 MB.
const MAX_TILES: usize = 600;

pub struct HeightTile {
    /// Row-major metres, 256x256.
    pub heights: Vec<f32>,
    pub min: f32,
    pub max: f32,
}

/// Terrarium encoding: height_m = R*256 + G + B/256 - 32768.
pub fn decode_terrarium(rgba: &[u8]) -> HeightTile {
    let mut heights = Vec::with_capacity(TILE_PX * TILE_PX);
    let (mut min, mut max) = (f32::MAX, f32::MIN);
    for px in rgba.chunks_exact(4) {
        let h = px[0] as f32 * 256.0 + px[1] as f32 + px[2] as f32 / 256.0 - 32768.0;
        min = min.min(h);
        max = max.max(h);
        heights.push(h);
    }
    HeightTile { heights, min, max }
}

impl HeightTile {
    /// Bilinear sample with u,v in 0..=1 mapped onto texel centres 0..255, matching the shader.
    pub fn sample(&self, u: f64, v: f64) -> f32 {
        let (x, y) = (u.clamp(0.0, 1.0) * 255.0, v.clamp(0.0, 1.0) * 255.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(255), (y0 + 1).min(255));
        let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
        let at = |xx: usize, yy: usize| self.heights[yy * TILE_PX + xx];
        let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
        let bot = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
        top * (1.0 - fy) + bot * fy
    }
}

/// CPU-side elevation store: serves picking, profiles, camera clearance and LOD bounds.
#[derive(Default)]
pub struct HeightField {
    tiles: HashMap<TileId, Arc<HeightTile>>,
    order: VecDeque<TileId>,
}

impl HeightField {
    pub fn insert(&mut self, id: TileId, tile: HeightTile) {
        if self.tiles.insert(id, Arc::new(tile)).is_none() {
            self.order.push_back(id);
            while self.order.len() > MAX_TILES {
                if let Some(old) = self.order.pop_front() {
                    self.tiles.remove(&old);
                }
            }
        }
    }

    pub fn contains(&self, id: TileId) -> bool {
        self.tiles.contains_key(&id)
    }

    pub fn get(&self, id: TileId) -> Option<&Arc<HeightTile>> {
        self.tiles.get(&id)
    }

    /// Elevation in metres from the most detailed loaded tile covering the point.
    pub fn sample(&self, p: LngLat) -> Option<f32> {
        for z in (0..=TERRAIN_MAX_ZOOM).rev() {
            let (wx, wy) = lnglat_to_world(p, z as f64);
            let (tx, ty) = ((wx / TILE_SIZE).floor(), (wy / TILE_SIZE).floor());
            let n = (1u64 << z) as f64;
            if tx < 0.0 || ty < 0.0 || tx >= n || ty >= n {
                continue;
            }
            let id = TileId::new(z, tx as u32, ty as u32);
            if let Some(t) = self.tiles.get(&id) {
                return Some(t.sample(wx / TILE_SIZE - tx, wy / TILE_SIZE - ty));
            }
        }
        None
    }

    /// Terrain slope (degrees from horizontal) and aspect (compass bearing the slope faces,
    /// i.e. the downhill direction, 0 = north) at a point, from central differences over
    /// `SLOPE_STEP_M`. None where any needed sample is not loaded yet.
    pub fn slope_aspect(&self, p: LngLat) -> Option<(f64, f64)> {
        let dlat = SLOPE_STEP_M / 111_320.0;
        let dlng = dlat / p.lat.to_radians().cos().max(0.05);
        let at = |dx: f64, dy: f64| self.sample(LngLat::new(p.lng + dx, p.lat + dy));
        let (e, w) = (at(dlng, 0.0)?, at(-dlng, 0.0)?);
        let (n, s) = (at(0.0, dlat)?, at(0.0, -dlat)?);
        let dz_dx = (e - w) as f64 / (2.0 * SLOPE_STEP_M);
        let dz_dy = (n - s) as f64 / (2.0 * SLOPE_STEP_M);
        let slope = dz_dx.hypot(dz_dy).atan().to_degrees();
        let aspect = (-dz_dx).atan2(-dz_dy).to_degrees().rem_euclid(360.0);
        Some((slope, aspect))
    }

    /// Conservative (min, max) metres for culling: the tile itself, else its nearest loaded ancestor.
    pub fn min_max(&self, id: TileId) -> (f32, f32) {
        let mut cur = Some(id);
        while let Some(t) = cur {
            if let Some(h) = self.tiles.get(&t) {
                return (h.min, h.max);
            }
            cur = t.parent();
        }
        (-500.0, 9000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_tile(h: f32) -> HeightTile {
        HeightTile {
            heights: vec![h; TILE_PX * TILE_PX],
            min: h,
            max: h,
        }
    }

    #[test]
    fn terrarium_known_values() {
        // 1000 m -> 33768 = 0x83E8 -> R=131, G=232, B=0
        let t = decode_terrarium(&[131, 232, 0, 255]);
        assert!((t.heights[0] - 1000.0).abs() < 1e-3);
        // sea level -> R=128,G=0,B=0
        assert_eq!(decode_terrarium(&[128, 0, 0, 255]).heights[0], 0.0);
        // fraction from blue channel: 0.5 m
        assert!((decode_terrarium(&[128, 0, 128, 255]).heights[0] - 0.5).abs() < 1e-4);
        // below sea level (Dead Sea ~ -430 m)
        let b = 32768.0 - 430.0; // 32338 = 0x7E52
        let t = decode_terrarium(&[(b as u32 >> 8) as u8, (b as u32 & 255) as u8, 0, 255]);
        assert!((t.heights[0] + 430.0).abs() < 1e-3);
    }

    /// A plane tilted by `deg` that rises toward `east_per_px` / `north_per_px` of a z10 tile.
    fn plane(deg: f64, rises_east: bool, rises_north: bool) -> (HeightField, LngLat) {
        let p = LngLat::new(-92.17, 38.58);
        let id = TileId::from_lnglat(p, 10);
        let px_m = smf_geo::meters_per_pixel(p.lat, 10.0) * 256.0 / 255.0;
        let per_px = deg.to_radians().tan() * px_m;
        let mut t = flat_tile(0.0);
        for y in 0..TILE_PX {
            for x in 0..TILE_PX {
                let mut h = 3000.0;
                if rises_east {
                    h += x as f64 * per_px;
                }
                if rises_north {
                    h += (TILE_PX - 1 - y) as f64 * per_px;
                }
                t.heights[y * TILE_PX + x] = h as f32;
            }
        }
        let mut hf = HeightField::default();
        hf.insert(id, t);
        (hf, p)
    }

    #[test]
    fn slope_and_aspect_of_known_planes() {
        // Rising to the east means the slope faces west (270); rising north faces south (180).
        let (hf, p) = plane(25.0, true, false);
        let (s, a) = hf.slope_aspect(p).unwrap();
        assert!((s - 25.0).abs() < 0.6, "{s}");
        assert!((a - 270.0).abs() < 1.0, "{a}");
        let (hf, p) = plane(35.0, false, true);
        let (s, a) = hf.slope_aspect(p).unwrap();
        assert!((s - 35.0).abs() < 0.8, "{s}");
        assert!((a - 180.0).abs() < 1.0, "{a}");
        // Rising both ways faces south-west.
        let (hf, p) = plane(20.0, true, true);
        let (_, a) = hf.slope_aspect(p).unwrap();
        assert!((a - 225.0).abs() < 2.0, "{a}");
        // Flat ground has zero slope, and unloaded terrain gives None rather than a guess.
        let (hf, p) = plane(0.0, true, false);
        assert!(hf.slope_aspect(p).unwrap().0 < 0.1);
        assert!(HeightField::default().slope_aspect(p).is_none());
    }

    #[test]
    fn bilinear_sampling_of_ramp() {
        let mut t = flat_tile(0.0);
        for y in 0..TILE_PX {
            for x in 0..TILE_PX {
                t.heights[y * TILE_PX + x] = x as f32; // height == column index
            }
        }
        assert!((t.sample(0.0, 0.5) - 0.0).abs() < 1e-4);
        assert!((t.sample(1.0, 0.5) - 255.0).abs() < 1e-3);
        assert!((t.sample(0.5, 0.5) - 127.5).abs() < 1e-3);
    }

    #[test]
    fn field_prefers_most_detailed_tile_and_falls_back() {
        let mut f = HeightField::default();
        let p = LngLat::new(-92.3, 38.6);
        assert!(f.sample(p).is_none());
        f.insert(TileId::from_lnglat(p, 8), flat_tile(100.0));
        assert_eq!(f.sample(p), Some(100.0));
        f.insert(TileId::from_lnglat(p, 12), flat_tile(250.0));
        assert_eq!(f.sample(p), Some(250.0));
        // A point in the z8 tile but not the z12 tile falls back to z8.
        let far = LngLat::new(p.lng + 0.5, p.lat);
        if TileId::from_lnglat(far, 8) == TileId::from_lnglat(p, 8) {
            assert_eq!(f.sample(far), Some(100.0));
        }
    }

    #[test]
    fn min_max_walks_up_to_ancestor() {
        let mut f = HeightField::default();
        let id = TileId::new(10, 250, 400);
        let mut t = flat_tile(0.0);
        t.min = 120.0;
        t.max = 480.0;
        f.insert(id.parent().unwrap(), t);
        assert_eq!(f.min_max(id), (120.0, 480.0));
        assert_eq!(f.min_max(TileId::new(3, 0, 0)).0, -500.0);
    }

    #[test]
    fn cache_is_bounded() {
        let mut f = HeightField::default();
        for i in 0..(MAX_TILES as u32 + 50) {
            f.insert(TileId::new(15, i, 0), flat_tile(1.0));
        }
        assert!(!f.contains(TileId::new(15, 0, 0)));
        assert!(f.contains(TileId::new(15, MAX_TILES as u32 + 49, 0)));
    }
}
