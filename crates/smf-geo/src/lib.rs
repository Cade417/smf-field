//! Pure geographic math: Web Mercator, slippy-map tiles, coordinate formatting.
//! No IO and no GPU so it stays WASM-friendly and trivially testable.

mod geodesy;
mod view;
pub use geodesy::{destination, distance_m, geodesic_circle, path_length_m, polygon_area_m2};
pub use view::{MapView, VisibleTile};

use std::f64::consts::PI;

/// Web Mercator is undefined at the poles; this is the standard clamp.
pub const MAX_LAT: f64 = 85.051_128_779_806_59;
pub const TILE_SIZE: f64 = 256.0;
/// Equatorial circumference in metres (EPSG:3857).
pub const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.685_578_49;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LngLat {
    pub lng: f64,
    pub lat: f64,
}

impl LngLat {
    pub fn new(lng: f64, lat: f64) -> Self {
        Self { lng, lat }
    }
}

/// Position in world pixels at a given zoom: (0,0) is the NW corner of the world.
pub fn lnglat_to_world(p: LngLat, zoom: f64) -> (f64, f64) {
    let scale = TILE_SIZE * 2f64.powf(zoom);
    let lat = p.lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    let x = (p.lng + 180.0) / 360.0 * scale;
    let y = (0.5 - (lat.tan() + 1.0 / lat.cos()).ln() / (2.0 * PI)) * scale;
    (x, y)
}

pub fn world_to_lnglat(x: f64, y: f64, zoom: f64) -> LngLat {
    let scale = TILE_SIZE * 2f64.powf(zoom);
    let lng = x / scale * 360.0 - 180.0;
    let n = PI * (1.0 - 2.0 * y / scale);
    let lat = n.sinh().atan().to_degrees();
    LngLat { lng, lat }
}

/// Ground metres per screen pixel at a latitude and (fractional) zoom.
pub fn meters_per_pixel(lat: f64, zoom: f64) -> f64 {
    EARTH_CIRCUMFERENCE_M * lat.to_radians().cos() / (TILE_SIZE * 2f64.powf(zoom))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// Tile containing a point at zoom `z`.
    pub fn from_lnglat(p: LngLat, z: u8) -> Self {
        let (wx, wy) = lnglat_to_world(p, z as f64);
        let n = 1u32 << z;
        Self {
            z,
            x: ((wx / TILE_SIZE).floor().max(0.0) as u32).min(n - 1),
            y: ((wy / TILE_SIZE).floor().max(0.0) as u32).min(n - 1),
        }
    }

    pub fn parent(&self) -> Option<TileId> {
        (self.z > 0).then(|| TileId::new(self.z - 1, self.x / 2, self.y / 2))
    }

    /// Flip Y for TMS-scheme sources and MBTiles (which store TMS rows).
    pub fn tms_y(&self) -> u32 {
        (1u32 << self.z) - 1 - self.y
    }
}

/// Wrap a tile x index into range so panning across the antimeridian repeats the world.
pub fn wrap_x(x: i64, z: u8) -> u32 {
    x.rem_euclid(1i64 << z) as u32
}

pub fn format_dd(p: LngLat) -> String {
    format!("{:.5}, {:.5}", p.lat, p.lng)
}

pub fn format_dms(p: LngLat) -> String {
    fn one(v: f64, pos: char, neg: char) -> String {
        let h = if v >= 0.0 { pos } else { neg };
        let a = v.abs();
        let d = a.floor();
        let m = ((a - d) * 60.0).floor();
        let s = ((a - d) * 60.0 - m) * 60.0;
        format!("{d:.0}°{m:02.0}'{s:05.2}\"{h}")
    }
    format!("{} {}", one(p.lat, 'N', 'S'), one(p.lng, 'E', 'W'))
}

/// UTM, e.g. "15S 500000E 4300000N". UTM is only defined for -80..84 latitude.
pub fn format_utm(p: LngLat) -> Option<String> {
    if !(-80.0..=84.0).contains(&p.lat) {
        return None;
    }
    let (n, e, conv) = {
        let (northing, easting, meridian) = utm::to_utm_wgs84_no_zone(p.lat, p.lng);
        (northing, easting, meridian)
    };
    let _ = conv;
    let zone = utm::lat_lon_to_zone_number(p.lat, p.lng);
    let band = utm::lat_to_zone_letter(p.lat)?;
    Some(format!("{zone}{band} {e:.0}E {n:.0}N"))
}

/// "850 m (2789 ft)" / "12.40 km (7.70 mi)" so both unit systems field crews use are visible.
pub fn format_distance(m: f64) -> String {
    let metric = if m >= 1000.0 {
        format!("{:.2} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
    };
    let ft = m * 3.280_839_895;
    let imperial = if ft >= 5280.0 {
        format!("{:.2} mi", ft / 5280.0)
    } else {
        format!("{ft:.0} ft")
    };
    format!("{metric} ({imperial})")
}

/// Area in hectares/acres, switching to km2/mi2 for large areas.
pub fn format_area(m2: f64) -> String {
    if m2 >= 1_000_000.0 {
        format!("{:.2} km² ({:.2} mi²)", m2 / 1e6, m2 / 2_589_988.110_336)
    } else {
        format!("{:.2} ha ({:.2} ac)", m2 / 10_000.0, m2 / 4_046.856_422_4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn world_origin_and_center() {
        let (x, y) = lnglat_to_world(LngLat::new(0.0, 0.0), 0.0);
        assert!((x - 128.0).abs() < 1e-9 && (y - 128.0).abs() < 1e-9);
        let (x, y) = lnglat_to_world(LngLat::new(-180.0, MAX_LAT), 3.0);
        assert!(x.abs() < 1e-9 && y.abs() < 1e-6);
    }

    #[test]
    fn known_tile_jefferson_city() {
        // Jefferson City, MO at z10; expected value worked out by hand from the Mercator formulas.
        let t = TileId::from_lnglat(LngLat::new(-92.1735, 38.5767), 10);
        assert_eq!((t.x, t.y), (249, 392));
    }

    #[test]
    fn tms_flip_and_parent() {
        let t = TileId::new(3, 2, 1);
        assert_eq!(t.tms_y(), 6);
        assert_eq!(t.parent(), Some(TileId::new(2, 1, 0)));
        assert_eq!(TileId::new(0, 0, 0).parent(), None);
    }

    #[test]
    fn wraps() {
        assert_eq!(wrap_x(-1, 3), 7);
        assert_eq!(wrap_x(8, 3), 0);
    }

    #[test]
    fn meters_per_pixel_z0_equator() {
        assert!((meters_per_pixel(0.0, 0.0) - 156_543.03).abs() < 0.01);
    }

    #[test]
    fn unit_formats() {
        assert_eq!(format_distance(850.0), "850 m (2789 ft)");
        assert_eq!(format_distance(12_000.0), "12.00 km (7.46 mi)");
        assert_eq!(format_area(10_000.0), "1.00 ha (2.47 ac)");
        assert_eq!(format_area(2_589_988.110_336), "2.59 km² (1.00 mi²)");
    }

    #[test]
    fn formats() {
        let p = LngLat::new(-92.1735, 38.5767);
        assert_eq!(format_dd(p), "38.57670, -92.17350");
        assert!(format_dms(p).starts_with("38°34'36.12\"N"));
        assert!(format_utm(p).unwrap().starts_with("15S"));
    }

    proptest! {
        #[test]
        fn roundtrip(lng in -179.9f64..179.9, lat in -85.0f64..85.0, z in 0.0f64..22.0) {
            let (x, y) = lnglat_to_world(LngLat::new(lng, lat), z);
            let p = world_to_lnglat(x, y, z);
            prop_assert!((p.lng - lng).abs() < 1e-6);
            prop_assert!((p.lat - lat).abs() < 1e-6);
        }
    }
}
