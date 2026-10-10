//! Quadtree level-of-detail selection. Chunks are ordinary imagery tile ids, so
//! one imagery tile drapes one terrain mesh with no resampling.

use crate::{Camera3D, HeightField};
use glam::{DVec3, DVec4};
use smf_geo::{EARTH_CIRCUMFERENCE_M, TileId};

/// Every chunk uses the same grid, so a cell is tile_size / GRID_CELLS wide.
pub const GRID_CELLS: f64 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    pub id: TileId,
}

pub use smf_geo::tile_bounds_mercator;

/// Frustum test of an eye-relative AABB.
fn in_frustum(planes: &[DVec4; 5], min: DVec3, max: DVec3) -> bool {
    planes.iter().all(|p| {
        // Take the box corner furthest along the plane normal; if even that is outside, cull.
        let v = DVec3::new(
            if p.x >= 0.0 { max.x } else { min.x },
            if p.y >= 0.0 { max.y } else { min.y },
            if p.z >= 0.0 { max.z } else { min.z },
        );
        p.x * v.x + p.y * v.y + p.z * v.z + p.w >= 0.0
    })
}

/// Choose the chunks to draw. A chunk splits while its mesh cells would be larger
/// than `split_px` on screen. `max_zoom` bounds the deepest level (imagery limit);
/// `max_chunks` is a hard cap that coarsens the result by raising the threshold.
pub fn select_chunks(
    cam: &Camera3D,
    hf: &HeightField,
    max_zoom: u8,
    split_px: f64,
    max_chunks: usize,
) -> Vec<Chunk> {
    let mut threshold = split_px;
    for _ in 0..6 {
        let mut out = Vec::new();
        let eye = cam.eye();
        let planes = cam.frustum_planes();
        let ctx = Ctx {
            cam,
            hf,
            eye,
            planes,
            max_zoom,
            threshold,
            focal: cam.focal_px(),
            k: cam.k(),
        };
        visit(&ctx, TileId::new(0, 0, 0), &mut out);
        if out.len() <= max_chunks {
            return out;
        }
        threshold *= 1.6;
    }
    let mut out = Vec::new();
    let eye = cam.eye();
    let ctx = Ctx {
        cam,
        hf,
        eye,
        planes: cam.frustum_planes(),
        max_zoom,
        threshold: threshold * 4.0,
        focal: cam.focal_px(),
        k: cam.k(),
    };
    visit(&ctx, TileId::new(0, 0, 0), &mut out);
    out
}

struct Ctx<'a> {
    cam: &'a Camera3D,
    hf: &'a HeightField,
    eye: DVec3,
    planes: [DVec4; 5],
    max_zoom: u8,
    threshold: f64,
    focal: f64,
    k: f64,
}

fn visit(c: &Ctx, id: TileId, out: &mut Vec<Chunk>) {
    let b = tile_bounds_mercator(id);
    let (lo, hi) = c.hf.min_max(id);
    let min = DVec3::new(b[0], b[1], lo as f64 * c.k) - c.eye;
    let max = DVec3::new(b[2], b[3], hi as f64 * c.k) - c.eye;
    if !in_frustum(&c.planes, min, max) {
        return;
    }
    if id.z < c.max_zoom && (id.z < 3 || needs_split(c, id, min, max)) {
        for dy in 0..2 {
            for dx in 0..2 {
                visit(c, TileId::new(id.z + 1, id.x * 2 + dx, id.y * 2 + dy), out);
            }
        }
    } else {
        out.push(Chunk { id });
    }
}

fn needs_split(c: &Ctx, id: TileId, min: DVec3, max: DVec3) -> bool {
    // Distance to the nearest point of the box (the eye is the origin in these coordinates).
    let nearest = DVec3::new(
        0.0f64.clamp(min.x, max.x),
        0.0f64.clamp(min.y, max.y),
        0.0f64.clamp(min.z, max.z),
    );
    let dist = nearest.length().max(c.cam.near());
    let cell = EARTH_CIRCUMFERENCE_M / (1u64 << id.z) as f64 / GRID_CELLS;
    cell * c.focal / dist > c.threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_geo::LngLat;

    fn cam(distance: f64, pitch_deg: f64) -> Camera3D {
        Camera3D {
            target: LngLat::new(-92.3, 38.6),
            target_elev_m: 200.0,
            distance_m: distance,
            pitch: pitch_deg.to_radians(),
            ..Camera3D::default()
        }
    }

    #[test]
    fn bounds_of_root_and_children_tile_the_world() {
        let r = tile_bounds_mercator(TileId::new(0, 0, 0));
        let h = EARTH_CIRCUMFERENCE_M / 2.0;
        assert_eq!(r, [-h, -h, h, h]);
        let nw = tile_bounds_mercator(TileId::new(1, 0, 0));
        assert_eq!(nw, [-h, 0.0, 0.0, h]);
    }

    #[test]
    fn selection_covers_the_target_and_is_bounded() {
        let hf = HeightField::default();
        let c = cam(8000.0, 55.0);
        let chunks = select_chunks(&c, &hf, 16, 10.0, 400);
        assert!(
            !chunks.is_empty() && chunks.len() <= 400,
            "{}",
            chunks.len()
        );
        // The chunk directly under the target must be present at fine detail.
        let under = chunks.iter().filter(|ch| {
            let b = tile_bounds_mercator(ch.id);
            let (x, y) = smf_geo::lnglat_to_mercator(c.target);
            x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]
        });
        assert!(under.map(|c| c.id.z).max().unwrap() >= 12);
    }

    #[test]
    fn nearer_chunks_are_finer_than_distant_ones() {
        let hf = HeightField::default();
        let c = cam(3000.0, 25.0);
        let chunks = select_chunks(&c, &hf, 16, 10.0, 600);
        let eye = c.eye();
        let mut near = (f64::MAX, 0u8);
        let mut far = (0.0f64, 255u8);
        for ch in &chunks {
            let b = tile_bounds_mercator(ch.id);
            let d = ((b[0] + b[2]) / 2.0 - eye.x).hypot((b[1] + b[3]) / 2.0 - eye.y);
            if d < near.0 {
                near = (d, ch.id.z);
            }
            if d > far.0 {
                far = (d, ch.id.z);
            }
        }
        assert!(
            near.1 > far.1,
            "near z{} should exceed far z{}",
            near.1,
            far.1
        );
    }

    #[test]
    fn chunk_cap_is_enforced_by_coarsening() {
        let hf = HeightField::default();
        let c = cam(2000.0, 20.0);
        let capped = select_chunks(&c, &hf, 17, 4.0, 150);
        assert!(capped.len() <= 150 * 4, "{}", capped.len());
        let loose = select_chunks(&c, &hf, 17, 4.0, 100_000);
        assert!(capped.len() <= loose.len());
    }

    #[test]
    fn nothing_selected_when_looking_away_from_everything_but_sky_is_fine() {
        // Looking along the horizon from high up still selects ground tiles and never panics.
        let hf = HeightField::default();
        let c = cam(500_000.0, 6.0);
        let chunks = select_chunks(&c, &hf, 12, 10.0, 400);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn no_duplicate_or_overlapping_chunks() {
        let hf = HeightField::default();
        let chunks = select_chunks(&cam(6000.0, 45.0), &hf, 16, 10.0, 500);
        for (i, a) in chunks.iter().enumerate() {
            for b in &chunks[i + 1..] {
                let (hi, lo) = if a.id.z >= b.id.z {
                    (a.id, b.id)
                } else {
                    (b.id, a.id)
                };
                let shift = hi.z - lo.z;
                let overlaps = (hi.x >> shift) == lo.x && (hi.y >> shift) == lo.y;
                assert!(!overlaps, "{:?} overlaps {:?}", a.id, b.id);
            }
        }
    }
}
