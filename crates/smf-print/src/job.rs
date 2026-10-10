//! End-to-end print and export jobs: render the map, compose the page, return the file bytes.

use crate::compose::{LegendOverlay, compose_pdf};
use crate::layout::{Layout, PrintProjection, PrintSpec};
use crate::raster::{
    GeoTiffRegion, PrintError, RenderLayers, encode_jpeg, render_region, utc_date, write_geotiff,
};
use smf_features::Project;
use smf_geo::{LngLat, distance_m};
use smf_render::MapRenderer;
use smf_tiles::TileService;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Standard map scales, finest first. The "fit" helper rounds up to one of these.
pub const STANDARD_SCALES: [f64; 20] = [
    1_000.0,
    2_000.0,
    2_500.0,
    5_000.0,
    10_000.0,
    12_000.0,
    15_000.0,
    20_000.0,
    24_000.0,
    25_000.0,
    40_000.0,
    50_000.0,
    62_500.0,
    100_000.0,
    125_000.0,
    250_000.0,
    500_000.0,
    1_000_000.0,
    2_500_000.0,
    5_000_000.0,
];

/// The finest standard scale at which `bounds` (min_lng, min_lat, max_lng, max_lat) fit the page's map
/// frame with ~10% padding. Falls back to the coarsest scale for continent-sized areas.
pub fn fit_scale(bounds: [f64; 4], spec: &PrintSpec) -> Result<f64, crate::layout::LayoutError> {
    let layout = Layout::new(&PrintSpec {
        scale_denominator: 24_000.0,
        ..spec.clone()
    })?;
    let mid = LngLat::new((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    let width_m = distance_m(
        LngLat::new(bounds[0], mid.lat),
        LngLat::new(bounds[2], mid.lat),
    ) * 1.1;
    let height_m = distance_m(
        LngLat::new(mid.lng, bounds[1]),
        LngLat::new(mid.lng, bounds[3]),
    ) * 1.1;
    let need = (width_m / (layout.frame.w / 1000.0)).max(height_m / (layout.frame.h / 1000.0));
    Ok(STANDARD_SCALES
        .into_iter()
        .find(|s| *s >= need)
        .unwrap_or(*STANDARD_SCALES.last().unwrap()))
}

pub struct PdfOutput {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
    pub missing_tiles: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn print_to_pdf(
    renderer: &mut MapRenderer,
    svc: &TileService,
    spec: &PrintSpec,
    layers: &RenderLayers,
    project: &Project,
    // Drawn on the map (beneath the project's markup) but not listed individually in the legend.
    extra_features: &[smf_features::Feature],
    legend_overlays: &[LegendOverlay],
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<PdfOutput, PrintError> {
    let layout = Layout::new(spec).map_err(|e| PrintError::Render(e.to_string()))?;
    let pj = PrintProjection::new(spec, &layout);
    // Render takes ~90% of the work; leave the rest for JPEG encoding and composing.
    let mut scaled = |p: f32| progress(p * 0.9);
    let rendered = render_region(
        renderer,
        svc,
        &pj.map_view(),
        layers,
        &mut scaled,
        cancel,
        Duration::from_secs(90),
    )?;
    let jpeg = encode_jpeg(&rendered, 90)?;
    let mut spec = spec.clone();
    if spec.attribution.is_empty() {
        spec.attribution = layers.base.attribution.clone();
    }
    let composed = compose_pdf(
        &spec,
        jpeg,
        [rendered.width, rendered.height],
        project,
        extra_features,
        legend_overlays,
        &utc_date(),
    )
    .map_err(|e| PrintError::Render(e.to_string()))?;
    let mut warnings = composed.warnings;
    if rendered.missing_tiles > 0 {
        warnings.push(format!(
            "{} map tile(s) could not be loaded; those areas show lower-resolution imagery.",
            rendered.missing_tiles
        ));
    }
    progress(1.0);
    Ok(PdfOutput {
        bytes: composed.bytes,
        warnings,
        missing_tiles: rendered.missing_tiles,
    })
}

#[allow(clippy::too_many_arguments)]
/// Render `bounds` at integer `zoom` and write an EPSG:3857 GeoTIFF. Returns the missing-tile count.
pub fn export_geotiff(
    renderer: &mut MapRenderer,
    svc: &TileService,
    bounds: [f64; 4],
    zoom: u8,
    layers: &RenderLayers,
    path: &Path,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<usize, PrintError> {
    let region = GeoTiffRegion::new(bounds, zoom);
    let rendered = render_region(
        renderer,
        svc,
        &region.map_view(),
        layers,
        progress,
        cancel,
        Duration::from_secs(90),
    )?;
    write_geotiff(path, &rendered, &region)?;
    Ok(rendered.missing_tiles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Paper;

    #[test]
    fn fit_scale_picks_the_finest_scale_that_fits() {
        let spec = PrintSpec {
            paper: Paper::Letter,
            ..PrintSpec::default()
        };
        let layout = Layout::new(&spec).unwrap();
        // A 3 km x 3 km area at 38.6 N.
        let c = LngLat::new(-92.3, 38.6);
        let dlat = 1500.0 / 111_000.0;
        let dlng = 1500.0 / (111_320.0 * 38.6f64.to_radians().cos());
        let b = [c.lng - dlng, c.lat - dlat, c.lng + dlng, c.lat + dlat];
        let s = fit_scale(b, &spec).unwrap();
        // The chosen scale must fit (with padding) and the next finer standard scale must not.
        let ground_w = layout.frame.w / 1000.0 * s;
        let ground_h = layout.frame.h / 1000.0 * s;
        assert!(
            ground_w >= 3000.0 * 1.1 - 1.0 && ground_h >= 3000.0 * 1.1 - 1.0,
            "1:{s}"
        );
        let finer = STANDARD_SCALES.iter().rev().find(|x| **x < s).copied();
        if let Some(f) = finer {
            assert!(
                layout.frame.w / 1000.0 * f < 3300.0 + 1.0
                    || layout.frame.h / 1000.0 * f < 3300.0 + 1.0,
                "1:{f} would also fit"
            );
        }
        // Landscape gives a wider frame, never a coarser scale for a wide area.
        let wide = [
            c.lng - 4.0 * dlng,
            c.lat - dlat,
            c.lng + 4.0 * dlng,
            c.lat + dlat,
        ];
        let p = fit_scale(wide, &spec).unwrap();
        let l = fit_scale(
            wide,
            &PrintSpec {
                landscape: true,
                ..spec.clone()
            },
        )
        .unwrap();
        assert!(l <= p, "landscape 1:{l} vs portrait 1:{p}");
        // Continent-sized areas clamp to the coarsest scale instead of failing.
        assert_eq!(
            fit_scale([-130.0, 20.0, -60.0, 55.0], &spec).unwrap(),
            5_000_000.0
        );
    }

    #[test]
    fn standard_scales_are_sorted() {
        assert!(STANDARD_SCALES.windows(2).all(|w| w[0] < w[1]));
    }
}
