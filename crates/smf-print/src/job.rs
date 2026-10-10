//! End-to-end print and export jobs: render the map, compose the page, return the file bytes.

use crate::compose::{LegendOverlay, compose_pdf};
use crate::layout::{Layout, PrintProjection, PrintSpec};
use crate::raster::{
    GeoTiffRegion, PrintError, RegionJob, RenderLayers, Rendered, Step, encode_jpeg, geotiff_bytes,
    utc_date,
};
use smf_features::Project;
use smf_geo::{LngLat, distance_m};
use smf_render::MapRenderer;
use smf_tiles::TileService;
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

/// A PDF print as a resumable task: call `step` repeatedly (every frame in a browser) until it
/// returns `Done`. Rendering the map is about 90% of the work; JPEG encoding and composing the
/// page happen in the final step.
pub struct PdfTask {
    region: RegionJob,
    spec: PrintSpec,
    base_attribution: String,
    project: Project,
    extra: Vec<smf_features::Feature>,
    legend: Vec<LegendOverlay>,
}

impl PdfTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        svc: &TileService,
        spec: &PrintSpec,
        layers: &RenderLayers,
        project: &Project,
        // Drawn on the map (beneath the project's markup) but not listed individually in the legend.
        extra_features: &[smf_features::Feature],
        legend_overlays: &[LegendOverlay],
    ) -> Result<Self, PrintError> {
        let layout = Layout::new(spec).map_err(|e| PrintError::Render(e.to_string()))?;
        let pj = PrintProjection::new(spec, &layout);
        let region = RegionJob::new(svc, &pj.map_view(), layers.clone(), Duration::from_secs(90))?;
        Ok(Self {
            region,
            spec: spec.clone(),
            base_attribution: layers.base.attribution.clone(),
            project: project.clone(),
            extra: extra_features.to_vec(),
            legend: legend_overlays.to_vec(),
        })
    }

    pub fn step(
        &mut self,
        renderer: &mut MapRenderer,
        svc: &TileService,
        cancel: &AtomicBool,
    ) -> Step<PdfOutput> {
        match self.region.step(renderer, svc, cancel) {
            Step::Pending(p) => Step::Pending(p * 0.9),
            Step::Done(Err(e)) => Step::Done(Err(e)),
            Step::Done(Ok(rendered)) => Step::Done(self.finish(rendered)),
        }
    }

    fn finish(&mut self, rendered: Rendered) -> Result<PdfOutput, PrintError> {
        let jpeg = encode_jpeg(&rendered, 90)?;
        let mut spec = self.spec.clone();
        if spec.attribution.is_empty() {
            spec.attribution = self.base_attribution.clone();
        }
        let composed = compose_pdf(
            &spec,
            jpeg,
            [rendered.width, rendered.height],
            &self.project,
            &self.extra,
            &self.legend,
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
        Ok(PdfOutput {
            bytes: composed.bytes,
            warnings,
            missing_tiles: rendered.missing_tiles,
        })
    }
}

/// A GeoTIFF export as a resumable task; finishes with the file bytes and the missing-tile count.
pub struct GeoTiffTask {
    region: RegionJob,
    geo: GeoTiffRegion,
}

impl GeoTiffTask {
    pub fn new(
        svc: &TileService,
        bounds: [f64; 4],
        zoom: u8,
        layers: &RenderLayers,
    ) -> Result<Self, PrintError> {
        let geo = GeoTiffRegion::new(bounds, zoom);
        let region = RegionJob::new(
            svc,
            &geo.map_view(),
            layers.clone(),
            Duration::from_secs(90),
        )?;
        Ok(Self { region, geo })
    }

    pub fn step(
        &mut self,
        renderer: &mut MapRenderer,
        svc: &TileService,
        cancel: &AtomicBool,
    ) -> Step<(Vec<u8>, usize)> {
        match self.region.step(renderer, svc, cancel) {
            Step::Pending(p) => Step::Pending(p),
            Step::Done(Err(e)) => Step::Done(Err(e)),
            Step::Done(Ok(r)) => {
                Step::Done(geotiff_bytes(&r, &self.geo).map(|b| (b, r.missing_tiles)))
            }
        }
    }
}

/// Render and compose a PDF, blocking until done (desktop).
#[cfg(not(target_arch = "wasm32"))]
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
    let mut task = PdfTask::new(svc, spec, layers, project, extra_features, legend_overlays)?;
    loop {
        match task.step(renderer, svc, cancel) {
            Step::Pending(p) => {
                progress(p);
                std::thread::sleep(Duration::from_millis(4));
            }
            Step::Done(r) => {
                if r.is_ok() {
                    progress(1.0);
                }
                return r;
            }
        }
    }
}

/// Render `bounds` at integer `zoom` and write an EPSG:3857 GeoTIFF (desktop). Returns the
/// missing-tile count.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub fn export_geotiff(
    renderer: &mut MapRenderer,
    svc: &TileService,
    bounds: [f64; 4],
    zoom: u8,
    layers: &RenderLayers,
    path: &std::path::Path,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<usize, PrintError> {
    let mut task = GeoTiffTask::new(svc, bounds, zoom, layers)?;
    loop {
        match task.step(renderer, svc, cancel) {
            Step::Pending(p) => {
                progress(p);
                std::thread::sleep(Duration::from_millis(4));
            }
            Step::Done(Err(e)) => return Err(e),
            Step::Done(Ok((bytes, missing))) => {
                let tmp = path.with_extension("tif.tmp");
                std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(&tmp, path))
                    .map_err(|e| PrintError::Io(e.to_string()))?;
                return Ok(missing);
            }
        }
    }
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
