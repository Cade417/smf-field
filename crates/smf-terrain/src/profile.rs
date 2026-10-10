//! Elevation profiles along a path.

use crate::{HeightField, TERRAIN_MAX_ZOOM};
use smf_geo::{LngLat, TileId, distance_m};

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Cumulative distance from the start in metres, one per sample.
    pub dist_m: Vec<f64>,
    /// Elevation in metres; None where terrain isn't loaded yet.
    pub elev_m: Vec<Option<f32>>,
    pub gain_m: f64,
    pub loss_m: f64,
    pub min_m: f32,
    pub max_m: f32,
    pub total_m: f64,
    /// Fraction of samples with elevation (1.0 = fully loaded).
    pub coverage: f32,
}

/// `n` evenly spaced points along the path (by geodesic distance), with their cumulative distances.
pub fn resample(path: &[LngLat], n: usize) -> (Vec<LngLat>, Vec<f64>) {
    if path.len() < 2 || n < 2 {
        return (path.to_vec(), vec![0.0; path.len()]);
    }
    let mut cum = vec![0.0];
    for w in path.windows(2) {
        cum.push(cum.last().unwrap() + distance_m(w[0], w[1]));
    }
    let total = *cum.last().unwrap();
    let (mut pts, mut ds) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut seg = 0;
    for i in 0..n {
        let d = total * i as f64 / (n - 1) as f64;
        while seg + 2 < cum.len() && cum[seg + 1] < d {
            seg += 1;
        }
        let span = cum[seg + 1] - cum[seg];
        let t = if span > 0.0 {
            ((d - cum[seg]) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (a, b) = (path[seg], path[seg + 1]);
        pts.push(LngLat::new(
            a.lng + (b.lng - a.lng) * t,
            a.lat + (b.lat - a.lat) * t,
        ));
        ds.push(d);
    }
    (pts, ds)
}

/// Tiles at `zoom` (capped to the terrain max) that a profile along `path` needs.
pub fn profile_tiles(path: &[LngLat], zoom: u8) -> Vec<TileId> {
    let z = zoom.min(TERRAIN_MAX_ZOOM);
    let (pts, _) = resample(path, 256);
    let mut out: Vec<TileId> = Vec::new();
    for p in pts {
        let t = TileId::from_lnglat(p, z);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

pub fn elevation_profile(hf: &HeightField, path: &[LngLat], samples: usize) -> Profile {
    let (pts, dist) = resample(path, samples.max(2));
    let elev: Vec<Option<f32>> = pts.iter().map(|p| hf.sample(*p)).collect();
    let (mut gain, mut loss) = (0.0f64, 0.0f64);
    let mut prev: Option<f32> = None;
    let (mut min, mut max) = (f32::MAX, f32::MIN);
    let mut have = 0;
    for e in elev.iter().flatten() {
        min = min.min(*e);
        max = max.max(*e);
        have += 1;
        if let Some(p) = prev {
            // Ignore sub-metre jitter so DEM noise doesn't inflate gain/loss on flat ground.
            let d = (*e - p) as f64;
            if d.abs() >= 1.0 {
                if d > 0.0 {
                    gain += d
                } else {
                    loss -= d
                }
                prev = Some(*e);
            }
        } else {
            prev = Some(*e);
        }
    }
    Profile {
        total_m: dist.last().copied().unwrap_or(0.0),
        dist_m: dist,
        coverage: have as f32 / elev.len().max(1) as f32,
        elev_m: elev,
        gain_m: gain,
        loss_m: loss,
        min_m: if have > 0 { min } else { 0.0 },
        max_m: if have > 0 { max } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeightTile;

    fn field_with_ramp(id: TileId, per_col: f32) -> HeightField {
        let mut hf = HeightField::default();
        let heights: Vec<f32> = (0..256 * 256).map(|i| (i % 256) as f32 * per_col).collect();
        hf.insert(
            id,
            HeightTile {
                heights,
                min: 0.0,
                max: 255.0 * per_col,
            },
        );
        hf
    }

    #[test]
    fn resample_is_evenly_spaced_and_hits_endpoints() {
        let path = [
            LngLat::new(-92.0, 38.0),
            LngLat::new(-91.9, 38.0),
            LngLat::new(-91.9, 38.1),
        ];
        let (pts, d) = resample(&path, 11);
        assert_eq!(pts.len(), 11);
        assert_eq!(pts[0], path[0]);
        assert!(smf_geo::distance_m(pts[10], path[2]) < 1e-6);
        let step = d[1] - d[0];
        for w in d.windows(2) {
            assert!((w[1] - w[0] - step).abs() < 1e-6);
        }
        let total = smf_geo::path_length_m(&path);
        assert!((d[10] - total).abs() < 1e-6);
    }

    #[test]
    fn profile_of_known_ramp() {
        // z8 tile around the point; ramp rises 1 m per texel eastward. Walk east and check gain.
        let start = LngLat::new(-92.3, 38.6);
        let id = TileId::from_lnglat(start, 8);
        let hf = field_with_ramp(id, 1.0);
        let end = LngLat::new(start.lng + 0.05, start.lat);
        let p = elevation_profile(&hf, &[start, end], 50);
        assert_eq!(p.coverage, 1.0);
        assert!(
            p.gain_m > 3.0 && p.loss_m == 0.0,
            "gain {} loss {}",
            p.gain_m,
            p.loss_m
        );
        // Walking back west is the same climb as loss.
        let q = elevation_profile(&hf, &[end, start], 50);
        assert!((q.loss_m - p.gain_m).abs() < 1e-3 && q.gain_m == 0.0);
        assert!((p.total_m - smf_geo::distance_m(start, end)).abs() < 1e-6);
    }

    #[test]
    fn flat_noise_below_one_metre_is_ignored() {
        let id = TileId::from_lnglat(LngLat::new(-92.3, 38.6), 8);
        let mut hf = HeightField::default();
        let heights: Vec<f32> = (0..256 * 256)
            .map(|i| 100.0 + if i % 2 == 0 { 0.4 } else { 0.0 })
            .collect();
        hf.insert(
            id,
            HeightTile {
                heights,
                min: 100.0,
                max: 100.4,
            },
        );
        let p = elevation_profile(
            &hf,
            &[LngLat::new(-92.3, 38.6), LngLat::new(-92.25, 38.6)],
            80,
        );
        assert_eq!((p.gain_m, p.loss_m), (0.0, 0.0));
    }

    #[test]
    fn missing_terrain_reports_partial_coverage() {
        let hf = HeightField::default();
        let p = elevation_profile(
            &hf,
            &[LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.0)],
            20,
        );
        assert_eq!(p.coverage, 0.0);
        assert!(p.elev_m.iter().all(Option::is_none));
    }

    #[test]
    fn profile_tiles_lists_each_tile_once() {
        let tiles = profile_tiles(&[LngLat::new(-92.0, 38.0), LngLat::new(-91.0, 38.0)], 10);
        let mut dedup = tiles.clone();
        dedup.dedup();
        assert_eq!(tiles.len(), dedup.len());
        assert!(
            tiles.len() >= 4,
            "a 1-degree line crosses several z10 tiles: {}",
            tiles.len()
        );
        // Zoom is capped at the terrain maximum.
        assert!(
            profile_tiles(&[LngLat::new(0.0, 0.0), LngLat::new(0.001, 0.0)], 20)
                .iter()
                .all(|t| t.z == 15)
        );
    }
}
