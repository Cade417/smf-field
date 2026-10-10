//! Page layout and the scale-accurate map projection used for printing.
//!
//! Accuracy rule: 1 mm on paper is `scale_denominator / 1000` real metres at the map
//! centre latitude. Web Mercator stretches ground distances by 1/cos(lat), so the zoom
//! is derived with the cos(lat) factor included; otherwise a "1:24,000" page at
//! Missouri's latitude would really be about 1:30,700.
//!
//! Web Mercator also treats the Earth as a sphere, so against the real ellipsoid its
//! east-west scale is slightly small and its north-south scale slightly large (about
//! +0.13% / -0.28% at 38 degrees N). The raster cannot be exact both ways, so the scale is
//! balanced with the Gaussian mean radius, leaving about +-0.2% in each direction.

use smf_geo::{
    EARTH_CIRCUMFERENCE_M, LngLat, MapView, TILE_SIZE, lnglat_to_world, world_to_lnglat,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paper {
    Letter,
    Legal,
    Tabloid,
    A4,
    A3,
    A2,
}

impl Paper {
    pub const ALL: [Paper; 6] = [
        Paper::Letter,
        Paper::Legal,
        Paper::Tabloid,
        Paper::A4,
        Paper::A3,
        Paper::A2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Paper::Letter => "Letter (8.5x11 in)",
            Paper::Legal => "Legal (8.5x14 in)",
            Paper::Tabloid => "Tabloid (11x17 in)",
            Paper::A4 => "A4",
            Paper::A3 => "A3",
            Paper::A2 => "A2",
        }
    }

    /// Portrait (width, height) in millimetres.
    pub fn size_mm(self) -> (f64, f64) {
        match self {
            Paper::Letter => (215.9, 279.4),
            Paper::Legal => (215.9, 355.6),
            Paper::Tabloid => (279.4, 431.8),
            Paper::A4 => (210.0, 297.0),
            Paper::A3 => (297.0, 420.0),
            Paper::A2 => (420.0, 594.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridKind {
    None,
    Utm,
    LatLon,
}

#[derive(Debug, Clone)]
pub struct PrintSpec {
    pub paper: Paper,
    pub landscape: bool,
    pub margin_mm: f64,
    /// 24000 means 1:24,000.
    pub scale_denominator: f64,
    pub dpi: u32,
    pub center: LngLat,
    pub title: String,
    pub subtitle: String,
    pub grid: GridKind,
    pub scale_bar: bool,
    pub north_arrow: bool,
    /// Magnetic declination in degrees, east positive. None omits the magnetic north arrow.
    pub declination_deg: Option<f64>,
    pub legend: bool,
    /// Required credit line for the data sources, printed on every page.
    pub attribution: String,
}

impl Default for PrintSpec {
    fn default() -> Self {
        Self {
            paper: Paper::Letter,
            landscape: false,
            margin_mm: 10.0,
            scale_denominator: 24_000.0,
            dpi: 200,
            center: LngLat::new(-92.3, 38.6),
            title: String::new(),
            subtitle: String::new(),
            grid: GridKind::Utm,
            scale_bar: true,
            north_arrow: true,
            declination_deg: None,
            legend: true,
            attribution: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectMm {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub page_w: f64,
    pub page_h: f64,
    pub title_h: f64,
    pub footer_h: f64,
    pub frame: RectMm,
}

pub const FOOTER_H_MM: f64 = 7.0;
const TITLE_H_MM: f64 = 13.0;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LayoutError {
    #[error("the page is too small for the margins")]
    TooSmall,
    #[error("scale must be between 1:100 and 1:50,000,000")]
    BadScale,
    #[error("resolution must be between 50 and 600 dpi")]
    BadDpi,
}

impl Layout {
    pub fn new(spec: &PrintSpec) -> Result<Layout, LayoutError> {
        if !(100.0..=50_000_000.0).contains(&spec.scale_denominator) {
            return Err(LayoutError::BadScale);
        }
        if !(50..=600).contains(&spec.dpi) {
            return Err(LayoutError::BadDpi);
        }
        let (pw, ph) = spec.paper.size_mm();
        let (page_w, page_h) = if spec.landscape { (ph, pw) } else { (pw, ph) };
        let title_h = if spec.title.is_empty() && spec.subtitle.is_empty() {
            0.0
        } else {
            TITLE_H_MM
        };
        let frame = RectMm {
            x: spec.margin_mm,
            y: spec.margin_mm + title_h,
            w: page_w - 2.0 * spec.margin_mm,
            h: page_h - 2.0 * spec.margin_mm - title_h - FOOTER_H_MM,
        };
        if frame.w < 40.0 || frame.h < 40.0 {
            return Err(LayoutError::TooSmall);
        }
        Ok(Layout {
            page_w,
            page_h,
            title_h,
            footer_h: FOOTER_H_MM,
            frame,
        })
    }
}

/// Ground metres per Web Mercator metre, averaged over east-west and north-south, at `lat`.
/// sqrt(N * M) / a, with N the prime-vertical and M the meridional radius of curvature.
pub fn ellipsoid_scale_factor(lat_deg: f64) -> f64 {
    const A: f64 = 6_378_137.0;
    const E2: f64 = 0.006_694_379_990_141_317;
    let s2 = lat_deg.to_radians().sin().powi(2);
    let w = 1.0 - E2 * s2;
    let n = A / w.sqrt();
    let m = A * (1.0 - E2) / w.powf(1.5);
    (n * m).sqrt() / A
}

/// Maps geography to page millimetres (origin top-left) for one print spec.
#[derive(Debug, Clone, Copy)]
pub struct PrintProjection {
    pub center: LngLat,
    /// Fractional web-map zoom whose pixels are exactly `mm_per_px` on paper at the centre latitude.
    pub zoom: f64,
    pub mm_per_px: f64,
    pub frame: RectMm,
}

impl PrintProjection {
    pub fn new(spec: &PrintSpec, layout: &Layout) -> Self {
        let mm_per_px = 25.4 / spec.dpi as f64;
        // Real ground metres one print pixel stands for.
        let ground_per_px = mm_per_px / 1000.0 * spec.scale_denominator;
        let k = ellipsoid_scale_factor(spec.center.lat);
        let zoom = (EARTH_CIRCUMFERENCE_M * spec.center.lat.to_radians().cos() * k
            / (TILE_SIZE * ground_per_px))
            .log2();
        Self {
            center: spec.center,
            zoom,
            mm_per_px,
            frame: layout.frame,
        }
    }

    pub fn frame_px(&self) -> [u32; 2] {
        [
            (self.frame.w / self.mm_per_px).round() as u32,
            (self.frame.h / self.mm_per_px).round() as u32,
        ]
    }

    /// The map view that renders exactly the frame at print resolution.
    pub fn map_view(&self) -> MapView {
        let s = self.frame_px();
        MapView::new(self.center, self.zoom, [s[0] as f64, s[1] as f64])
    }

    pub fn to_page_mm(&self, p: LngLat) -> (f64, f64) {
        let (cx, cy) = lnglat_to_world(self.center, self.zoom);
        let (x, y) = lnglat_to_world(p, self.zoom);
        (
            self.frame.x + self.frame.w / 2.0 + (x - cx) * self.mm_per_px,
            self.frame.y + self.frame.h / 2.0 + (y - cy) * self.mm_per_px,
        )
    }

    pub fn from_page_mm(&self, x: f64, y: f64) -> LngLat {
        let (cx, cy) = lnglat_to_world(self.center, self.zoom);
        world_to_lnglat(
            cx + (x - self.frame.x - self.frame.w / 2.0) / self.mm_per_px,
            cy + (y - self.frame.y - self.frame.h / 2.0) / self.mm_per_px,
            self.zoom,
        )
    }

    /// Geographic corners of the frame: (min_lng, min_lat, max_lng, max_lat).
    pub fn bounds(&self) -> [f64; 4] {
        let f = self.frame;
        let nw = self.from_page_mm(f.x, f.y);
        let se = self.from_page_mm(f.x + f.w, f.y + f.h);
        [nw.lng, se.lat, se.lng, nw.lat]
    }

    /// Real ground size of the frame in metres at the centre latitude.
    pub fn ground_size_m(&self, scale_denominator: f64) -> (f64, f64) {
        (
            self.frame.w / 1000.0 * scale_denominator,
            self.frame.h / 1000.0 * scale_denominator,
        )
    }
}

/// A round scale-bar length (metres) not above `max_m`, with sensible subdivisions.
/// Returns (length_m, divisions, label for the full length).
pub fn nice_bar(max_m: f64, imperial: bool) -> (f64, u32, String) {
    let unit = if imperial { 1609.344 } else { 1000.0 };
    let small = if imperial { 0.3048 } else { 1.0 };
    // Use the large unit only once the bar reaches one of it; shorter bars read better in m / ft.
    let (u, name) = if max_m >= unit {
        (unit, if imperial { "mi" } else { "km" })
    } else {
        (small, if imperial { "ft" } else { "m" })
    };
    let max_u = max_m / u;
    let pow = 10f64.powf(max_u.log10().floor());
    let n = max_u / pow;
    let (mult, div) = if n >= 5.0 {
        (5.0, 5)
    } else if n >= 4.0 {
        (4.0, 4)
    } else if n >= 2.0 {
        (2.0, 4)
    } else {
        (1.0, 4)
    };
    let len_u = mult * pow;
    let label = if len_u.fract() == 0.0 {
        format!("{len_u:.0} {name}")
    } else {
        format!("{len_u} {name}")
    };
    (len_u * u, div, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_geo::distance_m;

    #[test]
    fn paper_sizes_and_orientation() {
        let mut s = PrintSpec::default();
        let l = Layout::new(&s).unwrap();
        assert_eq!((l.page_w, l.page_h), (215.9, 279.4));
        s.landscape = true;
        let l = Layout::new(&s).unwrap();
        assert_eq!((l.page_w, l.page_h), (279.4, 215.9));
        // Margins, title block and footer all come out of the map frame.
        s.title = "Unit 3".into();
        let l = Layout::new(&s).unwrap();
        assert_eq!(l.frame.x, 10.0);
        assert_eq!(l.frame.y, 23.0);
        assert!((l.frame.w - 259.4).abs() < 1e-9);
        assert!((l.frame.h - (215.9 - 20.0 - 13.0 - 7.0)).abs() < 1e-9);
    }

    #[test]
    fn rejects_unprintable_specs() {
        assert_eq!(
            Layout::new(&PrintSpec {
                scale_denominator: 10.0,
                ..Default::default()
            }),
            Err(LayoutError::BadScale)
        );
        assert_eq!(
            Layout::new(&PrintSpec {
                dpi: 10,
                ..Default::default()
            }),
            Err(LayoutError::BadDpi)
        );
        assert_eq!(
            Layout::new(&PrintSpec {
                margin_mm: 120.0,
                ..Default::default()
            }),
            Err(LayoutError::TooSmall)
        );
    }

    /// The core accuracy property: distances measured on the page, converted at the scale, equal real
    /// geodesic distances. Web Mercator cannot be exact in both directions on an ellipsoid, so each
    /// direction is within 0.4% (the anisotropy peaks near the equator) and their mean is almost exact.
    #[test]
    fn one_page_millimetre_is_scale_over_1000_real_metres() {
        for (scale, lat) in [
            (24_000.0, 38.6),
            (24_000.0, 60.0),
            (100_000.0, 25.0),
            (5_000.0, 45.0),
            (24_000.0, 5.0),
        ] {
            let spec = PrintSpec {
                scale_denominator: scale,
                center: LngLat::new(-92.3, lat),
                dpi: 300,
                ..Default::default()
            };
            let layout = Layout::new(&spec).unwrap();
            let proj = PrintProjection::new(&spec, &layout);
            let (cx, cy) = (
                layout.frame.x + layout.frame.w / 2.0,
                layout.frame.y + layout.frame.h / 2.0,
            );
            // 100 mm east and 100 mm north of the centre, on the page.
            let c = proj.from_page_mm(cx, cy);
            let east = proj.from_page_mm(cx + 100.0, cy);
            let north = proj.from_page_mm(cx, cy - 100.0);
            let want = 100.0 / 1000.0 * scale;
            let (de, dn) = (distance_m(c, east), distance_m(c, north));
            assert!(
                (de - want).abs() / want < 0.004,
                "1:{scale} @{lat}: east {de} vs {want}"
            );
            assert!(
                (dn - want).abs() / want < 0.004,
                "1:{scale} @{lat}: north {dn} vs {want}"
            );
            let mean = (de + dn) / 2.0;
            assert!(
                (mean - want).abs() / want < 0.0008,
                "1:{scale} @{lat}: mean {mean} vs {want}"
            );
        }
    }

    #[test]
    fn ellipsoid_factor_matches_known_values() {
        // At the equator N = a and M = a(1-e^2), so the mean radius factor is sqrt(1-e^2).
        assert!(
            (ellipsoid_scale_factor(0.0) - (1.0f64 - 0.006_694_379_990_141_317).sqrt()).abs()
                < 1e-12
        );
        // Mid-latitudes sit just under 1; the poles just over.
        assert!(
            (ellipsoid_scale_factor(38.6) - 0.99925).abs() < 0.0002,
            "{}",
            ellipsoid_scale_factor(38.6)
        );
        assert!(ellipsoid_scale_factor(90.0) > 1.0);
    }

    #[test]
    fn mercator_correction_is_what_makes_it_accurate() {
        // Without the cos(lat) factor the page would be ~28% off at Missouri's latitude.
        let spec = PrintSpec::default();
        let layout = Layout::new(&spec).unwrap();
        let proj = PrintProjection::new(&spec, &layout);
        let naive_zoom =
            (EARTH_CIRCUMFERENCE_M / (TILE_SIZE * (25.4 / 200.0 / 1000.0 * 24_000.0))).log2();
        assert!(
            (naive_zoom - proj.zoom).abs() > 0.3,
            "{naive_zoom} vs {}",
            proj.zoom
        );
    }

    #[test]
    fn projection_roundtrips_and_centre_maps_to_frame_centre() {
        let spec = PrintSpec {
            title: "T".into(),
            ..Default::default()
        };
        let layout = Layout::new(&spec).unwrap();
        let proj = PrintProjection::new(&spec, &layout);
        let (x, y) = proj.to_page_mm(spec.center);
        assert!((x - (layout.frame.x + layout.frame.w / 2.0)).abs() < 1e-9);
        assert!((y - (layout.frame.y + layout.frame.h / 2.0)).abs() < 1e-9);
        let p = proj.from_page_mm(50.0, 80.0);
        let (x2, y2) = proj.to_page_mm(p);
        assert!((x2 - 50.0).abs() < 1e-6 && (y2 - 80.0).abs() < 1e-6);
        let b = proj.bounds();
        assert!(
            b[0] < spec.center.lng
                && spec.center.lng < b[2]
                && b[1] < spec.center.lat
                && spec.center.lat < b[3]
        );
    }

    #[test]
    fn frame_pixels_follow_dpi() {
        let spec = PrintSpec {
            dpi: 300,
            ..Default::default()
        };
        let layout = Layout::new(&spec).unwrap();
        let px = PrintProjection::new(&spec, &layout).frame_px();
        assert_eq!(
            px,
            [
                (195.9_f64 / (25.4 / 300.0)).round() as u32,
                (layout.frame.h / (25.4 / 300.0)).round() as u32
            ]
        );
    }

    #[test]
    fn scale_bar_lengths_are_round_and_fit() {
        for max in [250.0, 800.0, 1_700.0, 4_300.0, 12_000.0, 90_000.0] {
            let (len, div, label) = nice_bar(max, false);
            assert!(
                len <= max + 1e-9 && len > max * 0.19,
                "{max} -> {len} {label}"
            );
            assert!(div >= 4);
        }
        assert_eq!(nice_bar(4_300.0, false).2, "4 km");
        assert_eq!(nice_bar(800.0, false).2, "500 m");
        let (len, _, label) = nice_bar(5_000.0, true);
        assert_eq!(label, "2 mi");
        assert!((len - 2.0 * 1609.344).abs() < 1e-6);
        assert_eq!(nice_bar(300.0, true).2, "500 ft");
    }
}
