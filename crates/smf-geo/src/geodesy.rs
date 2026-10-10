//! Ellipsoidal (WGS84) measurements via Karney's geodesic algorithms. Every
//! displayed distance/area goes through here; Mercator pixel distances are badly
//! wrong away from the equator (Missouri is ~25% off), so never use them.

use crate::LngLat;
use geographiclib_rs::{DirectGeodesic, Geodesic, InverseGeodesic, PolygonArea, Winding};

pub fn distance_m(a: LngLat, b: LngLat) -> f64 {
    Geodesic::wgs84().inverse(a.lat, a.lng, b.lat, b.lng)
}

/// Initial compass bearing from `a` to `b` in degrees clockwise from north, 0..360.
pub fn bearing_deg(a: LngLat, b: LngLat) -> f64 {
    let (_s12, azi1, _azi2, _a12): (f64, f64, f64, f64) =
        Geodesic::wgs84().inverse(a.lat, a.lng, b.lat, b.lng);
    azi1.rem_euclid(360.0)
}

pub fn path_length_m(pts: &[LngLat]) -> f64 {
    pts.windows(2).map(|w| distance_m(w[0], w[1])).sum()
}

/// Point reached from `from` travelling `dist_m` metres on initial bearing `azimuth_deg`.
pub fn destination(from: LngLat, azimuth_deg: f64, dist_m: f64) -> LngLat {
    let (lat, lng): (f64, f64) = Geodesic::wgs84().direct(from.lat, from.lng, azimuth_deg, dist_m);
    LngLat { lng, lat }
}

/// Absolute geodesic area of a simple polygon ring (open or closed) in m².
pub fn polygon_area_m2(ring: &[LngLat]) -> f64 {
    let g = Geodesic::wgs84();
    let mut pa = PolygonArea::new(&g, Winding::CounterClockwise);
    for p in ring {
        pa.add_point(p.lat, p.lng);
    }
    let (_, area, _) = pa.compute(true);
    area.abs()
}

/// `segments` points on a true geodesic circle (not a Mercator circle), closed.
pub fn geodesic_circle(center: LngLat, radius_m: f64, segments: usize) -> Vec<LngLat> {
    let mut v: Vec<LngLat> = (0..segments)
        .map(|i| destination(center, 360.0 * i as f64 / segments as f64, radius_m))
        .collect();
    v.push(v[0]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearings_follow_the_compass() {
        let o = LngLat::new(-92.0, 38.0);
        assert!(
            bearing_deg(o, LngLat::new(-92.0, 39.0)).abs() < 1e-6,
            "north"
        );
        assert!(
            (bearing_deg(o, LngLat::new(-91.0, 38.0)) - 90.0).abs() < 0.6,
            "east"
        );
        assert!(
            (bearing_deg(o, LngLat::new(-92.0, 37.0)) - 180.0).abs() < 1e-6,
            "south"
        );
        assert!(
            (bearing_deg(o, LngLat::new(-93.0, 38.0)) - 270.0).abs() < 0.6,
            "west"
        );
        // Round trip through `destination`.
        let d = destination(o, 123.0, 5000.0);
        assert!((bearing_deg(o, d) - 123.0).abs() < 1e-6);
    }

    #[test]
    fn known_distance_equator_degree() {
        // One degree of longitude on the equator is 111.319 km on WGS84.
        let d = distance_m(LngLat::new(0.0, 0.0), LngLat::new(1.0, 0.0));
        assert!((d - 111_319.49).abs() < 1.0, "{d}");
    }

    #[test]
    fn geodesic_vs_mercator_at_latitude() {
        // 1 degree of longitude at 38.5N is ~87 km, not 111 km.
        let d = distance_m(LngLat::new(-92.0, 38.5), LngLat::new(-91.0, 38.5));
        assert!((d - 87_160.0).abs() < 500.0, "{d}");
    }

    #[test]
    fn destination_roundtrip() {
        let a = LngLat::new(-92.3, 38.6);
        let b = destination(a, 73.0, 12_345.0);
        assert!((distance_m(a, b) - 12_345.0).abs() < 1e-3);
    }

    #[test]
    fn square_area_matches_reference() {
        // 1deg x 1deg at the equator is about 12,308.8 km².
        let ring = [
            LngLat::new(0.0, 0.0),
            LngLat::new(1.0, 0.0),
            LngLat::new(1.0, 1.0),
            LngLat::new(0.0, 1.0),
        ];
        let a = polygon_area_m2(&ring);
        assert!((a - 12_308_778_000.0).abs() / a < 0.001, "{a}");
        // winding order must not matter
        let mut r = ring;
        r.reverse();
        assert!((polygon_area_m2(&r) - a).abs() < 1.0);
    }

    #[test]
    fn circle_points_equidistant() {
        let c = LngLat::new(-92.0, 38.0);
        for p in geodesic_circle(c, 5000.0, 36) {
            assert!((distance_m(c, p) - 5000.0).abs() < 1e-3);
        }
    }
}
