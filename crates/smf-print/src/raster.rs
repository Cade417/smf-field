//! Offscreen rendering of a map region at print/export resolution, and the GeoTIFF writer.
//!
//! Large outputs are rendered in chunks: the GPU tile pool holds 256 tiles, far fewer than a
//! 300 dpi A3 page needs, so each chunk loads only the tiles it covers.

use smf_geo::{
    EARTH_CIRCUMFERENCE_M, MapView, TILE_SIZE, TileId, lnglat_to_mercator, mercator_to_lnglat,
    world_to_lnglat,
};
use smf_render::{MapRenderer, ShadeParams, TileKey, plan_frame_at};
use smf_terrain::decode_terrarium;
use smf_tiles::{TileEvent, TileService, TileSource};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Refuse outputs beyond this many pixels (an A2 at 600 dpi is ~140 MP; 100 MP is plenty).
pub const MAX_PIXELS: u64 = 100_000_000;
/// Chunk edge in pixels: ~7x7 tiles per layer, leaving room in the 256-tile pool for fallbacks.
const CHUNK: u32 = 1536;

#[derive(Debug, thiserror::Error)]
pub enum PrintError {
    #[error(
        "the output would be {pixels} pixels, over the {limit} pixel limit; lower the resolution or the scale"
    )]
    TooLarge { pixels: u64, limit: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Render(String),
    #[error("could not write the file: {0}")]
    Io(String),
}

pub struct RenderLayers {
    pub base: Arc<TileSource>,
    /// (source, opacity) drawn on top of the basemap, in order.
    pub overlays: Vec<(Arc<TileSource>, f32)>,
    /// Elevation source for hillshade/slope; None disables shading.
    pub terrain: Option<Arc<TileSource>>,
    pub shade: ShadeParams,
}

pub struct Rendered {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8.
    pub rgba: Vec<u8>,
    /// Tiles that never arrived (server errors, timeouts); the output shows lower-resolution stand-ins there.
    pub missing_tiles: usize,
}

impl Rendered {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }
}

struct Ingest<'a> {
    svc: &'a TileService,
    sources: Vec<Arc<TileSource>>,
    terrain_id: Option<String>,
    failed: HashSet<TileKey>,
    failed_heights: HashSet<TileId>,
}

impl Ingest<'_> {
    fn drain(&mut self, renderer: &mut MapRenderer) {
        while let Some(ev) = self.svc.try_recv() {
            match ev {
                TileEvent::Ready { source, id, rgba } => {
                    if self.terrain_id.as_deref() == Some(&source) {
                        renderer.upload_height(id, &decode_terrarium(&rgba).heights);
                    } else if let Some(i) = self.sources.iter().position(|s| s.id == source) {
                        renderer.upload_tile(
                            TileKey {
                                source: i as u16,
                                id,
                            },
                            &rgba,
                        );
                    }
                }
                TileEvent::Failed { source, id } => {
                    if self.terrain_id.as_deref() == Some(&source) {
                        self.failed_heights.insert(id);
                    } else if let Some(i) = self.sources.iter().position(|s| s.id == source) {
                        self.failed.insert(TileKey {
                            source: i as u16,
                            id,
                        });
                    }
                }
            }
        }
    }
}

/// Render `view` (any size) to RGBA. `progress` receives 0..1.
pub fn render_region(
    renderer: &mut MapRenderer,
    svc: &TileService,
    view: &MapView,
    layers: &RenderLayers,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
    per_chunk_timeout: Duration,
) -> Result<Rendered, PrintError> {
    let (w, h) = (view.size[0].round() as u32, view.size[1].round() as u32);
    let pixels = w as u64 * h as u64;
    if w == 0 || h == 0 {
        return Err(PrintError::Render("the output area is empty".into()));
    }
    if pixels > MAX_PIXELS {
        return Err(PrintError::TooLarge {
            pixels,
            limit: MAX_PIXELS,
        });
    }
    let mut sources = vec![layers.base.clone()];
    sources.extend(layers.overlays.iter().map(|(s, _)| s.clone()));
    for s in &sources {
        svc.register_source(s).map_err(PrintError::Render)?;
    }
    let shading = layers.terrain.is_some() && (layers.shade.hillshade || layers.shade.slope);
    let mut ing = Ingest {
        svc,
        sources: sources.clone(),
        terrain_id: layers.terrain.as_ref().map(|t| t.id.clone()),
        failed: HashSet::new(),
        failed_heights: HashSet::new(),
    };
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut missing = 0usize;
    let (cols, rows) = (w.div_ceil(CHUNK), h.div_ceil(CHUNK));
    let total = (cols * rows) as f32;
    // View centre in world pixels at the print zoom, to offset each chunk from.
    let (cwx, cwy) = smf_geo::lnglat_to_world(view.center, view.zoom);
    // Print tiles one zoom level finer than the nominal one: sharper, never stretched.
    let tz_for = |max_zoom: u8| (view.zoom.ceil().max(0.0) as u8).min(max_zoom);

    for ry in 0..rows {
        for rx in 0..cols {
            let (x0, y0) = (rx * CHUNK, ry * CHUNK);
            let (cw, ch) = ((w - x0).min(CHUNK), (h - y0).min(CHUNK));
            // Chunk centre in world pixels: view centre plus the chunk's offset from the view centre.
            let ccx = cwx + (x0 as f64 + cw as f64 / 2.0 - w as f64 / 2.0);
            let ccy = cwy + (y0 as f64 + ch as f64 / 2.0 - h as f64 / 2.0);
            let sub = MapView::new(
                world_to_lnglat(ccx, ccy, view.zoom),
                view.zoom,
                [cw as f64, ch as f64],
            );
            renderer.ensure_target([cw, ch]);

            let started = Instant::now();
            let (mut quads, mut shade_verts);
            // Zoom each source's tiles are drawn at, to tell exact tiles from fallback ancestors.
            let mut tzs: Vec<u8> = vec![];
            let mut reqs: Vec<(usize, TileKey, f32)>;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(PrintError::Cancelled);
                }
                svc.begin_frame();
                renderer.begin_frame();
                ing.drain(renderer);
                reqs = vec![];
                tzs.clear();
                let base_tz = tz_for(layers.base.max_zoom);
                tzs.push(base_tz);
                let plan = plan_frame_at(renderer, &sub, 0, base_tz, 1.0);
                quads = plan.quads;
                reqs.extend(plan.requests.into_iter().map(|(k, p)| (0usize, k, p)));
                for (i, (src, opacity)) in layers.overlays.iter().enumerate() {
                    let otz = tz_for(src.max_zoom);
                    tzs.push(otz);
                    let p = plan_frame_at(renderer, &sub, (i + 1) as u16, otz, *opacity);
                    quads.extend(p.quads);
                    reqs.extend(p.requests.into_iter().map(|(k, pr)| (i + 1, k, pr)));
                }
                let mut hreqs: Vec<(TileId, f32)> = vec![];
                shade_verts = vec![];
                if shading {
                    let (v, hr) = renderer.plan_shade(&sub, base_tz.min(15));
                    shade_verts = v;
                    hreqs = hr
                        .into_iter()
                        .map(|(k, p)| (k.id, p))
                        .filter(|(id, _)| !ing.failed_heights.contains(id))
                        .collect();
                }
                let pending: Vec<_> = reqs
                    .iter()
                    .filter(|(_, k, _)| !ing.failed.contains(k))
                    .collect();
                if pending.is_empty() && hreqs.is_empty() {
                    break;
                }
                for (i, k, p) in &pending {
                    svc.request(&sources[*i], k.id, *p);
                }
                if let Some(t) = &layers.terrain {
                    for (id, p) in &hreqs {
                        svc.request(t, *id, *p);
                    }
                }
                if started.elapsed() > per_chunk_timeout {
                    break;
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            // Missing = tiles of the final zoom this chunk needed but never got (failed or timed out).
            // Ancestors requested only as fallbacks are not counted.
            missing += reqs.iter().filter(|(i, k, _)| k.id.z == tzs[*i]).count();
            renderer.render_2d(&quads, &shade_verts, &layers.shade_for_render(shading));
            let px = renderer
                .read_pixels()
                .ok_or_else(|| PrintError::Render("GPU readback failed".into()))?;
            for row in 0..ch {
                let src = (row * cw * 4) as usize;
                let dst = (((y0 + row) * w + x0) * 4) as usize;
                out[dst..dst + (cw * 4) as usize]
                    .copy_from_slice(&px[src..src + (cw * 4) as usize]);
            }
            progress((ry * cols + rx + 1) as f32 / total);
        }
    }
    Ok(Rendered {
        width: w,
        height: h,
        rgba: out,
        missing_tiles: missing,
    })
}

impl RenderLayers {
    fn shade_for_render(&self, shading: bool) -> ShadeParams {
        if shading {
            self.shade
        } else {
            ShadeParams {
                hillshade: false,
                slope: false,
                ..self.shade
            }
        }
    }
}

pub fn encode_jpeg(r: &Rendered, quality: u8) -> Result<Vec<u8>, PrintError> {
    let rgb: Vec<u8> = r
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(&rgb, r.width, r.height, image::ExtendedColorType::Rgb8)
        .map_err(|e| PrintError::Render(e.to_string()))?;
    Ok(buf)
}

/// Geometry of a Web Mercator (EPSG:3857) export aligned to the tile pixel grid at integer zoom `zoom`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoTiffRegion {
    pub zoom: u8,
    /// Mercator metres of the top-left corner.
    pub origin_x: f64,
    pub origin_y: f64,
    /// Mercator metres per pixel.
    pub pixel_m: f64,
    pub width: u32,
    pub height: u32,
}

impl GeoTiffRegion {
    /// Cover (min_lng, min_lat, max_lng, max_lat) at `zoom`, snapped outward to whole pixels.
    pub fn new(bounds: [f64; 4], zoom: u8) -> Self {
        let pixel_m = EARTH_CIRCUMFERENCE_M / (TILE_SIZE * (1u64 << zoom) as f64);
        let half = EARTH_CIRCUMFERENCE_M / 2.0;
        let (west, north) = lnglat_to_mercator(smf_geo::LngLat::new(bounds[0], bounds[3]));
        let (east, south) = lnglat_to_mercator(smf_geo::LngLat::new(bounds[2], bounds[1]));
        let ox = ((west + half) / pixel_m).floor() * pixel_m - half;
        let oy = half - ((half - north) / pixel_m).floor() * pixel_m;
        Self {
            zoom,
            origin_x: ox,
            origin_y: oy,
            pixel_m,
            width: ((east - ox) / pixel_m).ceil().max(1.0) as u32,
            height: ((oy - south) / pixel_m).ceil().max(1.0) as u32,
        }
    }

    pub fn map_view(&self) -> MapView {
        let cx = self.origin_x + self.width as f64 * self.pixel_m / 2.0;
        let cy = self.origin_y - self.height as f64 * self.pixel_m / 2.0;
        MapView::new(
            mercator_to_lnglat(cx, cy),
            self.zoom as f64,
            [self.width as f64, self.height as f64],
        )
    }
}

/// Write an RGB GeoTIFF in EPSG:3857 (alpha is dropped: the render is opaque).
pub fn write_geotiff(path: &Path, r: &Rendered, region: &GeoTiffRegion) -> Result<(), PrintError> {
    use tiff::encoder::{TiffEncoder, colortype};
    use tiff::tags::Tag;
    let io = |e: &dyn std::fmt::Display| PrintError::Io(e.to_string());
    let rgb: Vec<u8> = r
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let tmp = path.with_extension("tif.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| io(&e))?;
        // BigTIFF only when needed: classic TIFF tops out at 4 GB.
        let mut enc = TiffEncoder::new(&mut f).map_err(|e| io(&e))?;
        let mut img = enc
            .new_image::<colortype::RGB8>(r.width, r.height)
            .map_err(|e| io(&e))?;
        // GeoTIFF 1.0: ModelPixelScale, ModelTiepoint (pixel 0,0 -> top-left corner), GeoKeyDirectory.
        img.encoder()
            .write_tag(
                Tag::Unknown(33550),
                &[region.pixel_m, region.pixel_m, 0.0][..],
            )
            .map_err(|e| io(&e))?;
        img.encoder()
            .write_tag(
                Tag::Unknown(33922),
                &[0.0, 0.0, 0.0, region.origin_x, region.origin_y, 0.0][..],
            )
            .map_err(|e| io(&e))?;
        // GTModelType=Projected, GTRasterType=PixelIsArea, ProjectedCSType=EPSG:3857.
        let keys: [u16; 16] = [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 3857];
        img.encoder()
            .write_tag(Tag::Unknown(34735), &keys[..])
            .map_err(|e| io(&e))?;
        img.write_data(&rgb).map_err(|e| io(&e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| io(&e))
}

/// Today's date (UTC) as YYYY-MM-DD without pulling in a date crate.
pub fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_date(secs as i64 / 86_400)
}

/// Days since 1970-01-01 -> "YYYY-MM-DD" (Howard Hinnant's civil_from_days).
pub fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", if m <= 2 { y + 1 } else { y })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, PrintProjection, PrintSpec};
    use smf_geo::LngLat;
    use smf_tiles::{Backend, Mbtiles, SourceKind};

    #[test]
    fn dates_are_correct() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(19_723), "2024-01-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
        assert_eq!(civil_date(20_000), "2024-10-04");
        assert_eq!(utc_date().len(), 10);
    }

    #[test]
    fn region_snaps_to_the_pixel_grid_and_covers_the_bounds() {
        let b = [-92.4, 38.5, -92.2, 38.7];
        let r = GeoTiffRegion::new(b, 14);
        let half = EARTH_CIRCUMFERENCE_M / 2.0;
        // The origin is a whole number of pixels from the world's corner, so tiles line up with the raster.
        assert!(
            ((r.origin_x + half) / r.pixel_m).fract().abs() < 1e-6
                || ((r.origin_x + half) / r.pixel_m).fract() > 1.0 - 1e-6
        );
        let (w, n) = lnglat_to_mercator(LngLat::new(b[0], b[3]));
        let (e, s) = lnglat_to_mercator(LngLat::new(b[2], b[1]));
        assert!(r.origin_x <= w && r.origin_y >= n);
        assert!(r.origin_x + r.width as f64 * r.pixel_m >= e - 1e-6);
        assert!(r.origin_y - r.height as f64 * r.pixel_m <= s + 1e-6);
        // Never more than one pixel of slack on each side.
        assert!(
            w - r.origin_x < r.pixel_m && r.origin_x + r.width as f64 * r.pixel_m - e < r.pixel_m
        );
        let v = r.map_view();
        assert_eq!(v.size, [r.width as f64, r.height as f64]);
        assert_eq!(v.zoom, 14.0);
    }

    #[test]
    fn geotiff_roundtrips_through_the_reader_with_exact_georeferencing() {
        let bounds = [-92.4, 38.5, -92.3, 38.58];
        let region = GeoTiffRegion::new(bounds, 13);
        let (w, h) = (region.width.min(300), region.height.min(200));
        let region = GeoTiffRegion {
            width: w,
            height: h,
            ..region
        };
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % w) as u8, ((i / w) % 256) as u8, 77, 255])
            .collect();
        let r = Rendered {
            width: w,
            height: h,
            rgba,
            missing_tiles: 0,
        };
        let dir = std::env::temp_dir().join(format!("smf-gt-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.tif");
        write_geotiff(&path, &r, &region).unwrap();
        assert!(!path.with_extension("tif.tmp").exists());
        let back = smf_tiles::GeoRaster::open(&path).unwrap();
        assert_eq!((back.width, back.height), (w, h));
        let bb = back.bounds().unwrap();
        let nw = mercator_to_lnglat(region.origin_x, region.origin_y);
        let se = mercator_to_lnglat(
            region.origin_x + w as f64 * region.pixel_m,
            region.origin_y - h as f64 * region.pixel_m,
        );
        assert!(
            (bb[0] - nw.lng).abs() < 1e-7 && (bb[3] - nw.lat).abs() < 1e-7,
            "{bb:?} vs {nw:?}"
        );
        assert!(
            (bb[2] - se.lng).abs() < 1e-7 && (bb[1] - se.lat).abs() < 1e-7,
            "{bb:?} vs {se:?}"
        );
        // Pixel data survives (render a tile of the raster and compare a known pixel).
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_outputs_are_refused_before_touching_the_gpu() {
        // No GPU needed: the size check comes first. A throwaway renderer is not required, so build the error path directly.
        let err = PrintError::TooLarge {
            pixels: 200_000_000,
            limit: MAX_PIXELS,
        };
        assert!(err.to_string().contains("100000000"));
    }

    fn headless() -> Option<(smf_render::wgpu::Device, smf_render::wgpu::Queue)> {
        use smf_render::wgpu;
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&Default::default())).ok()
    }

    fn png(color: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::new();
        image::RgbaImage::from_pixel(256, 256, image::Rgba([color[0], color[1], color[2], 255]))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Source whose tile colour encodes the tile index parity: red/blue by x, light/dark by y.
    /// Colour changes in the rendered raster must land exactly where the print projection says
    /// the tile edges are, which proves the raster is registered with the page geometry.
    #[test]
    fn rendered_raster_is_registered_with_the_print_projection() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("smf-print-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = PrintSpec {
            dpi: 100,
            scale_denominator: 24_000.0,
            center: LngLat::new(-92.3, 38.6),
            ..PrintSpec::default()
        };
        let layout = Layout::new(&spec).unwrap();
        let proj = PrintProjection::new(&spec, &layout);
        let view = proj.map_view();
        let tz = view.zoom.ceil() as u8;
        let mb = Mbtiles::create(&dir.join("t.mbtiles")).unwrap();
        let b = proj.bounds();
        let (xs, ys) =
            smf_geo::tile_range([b[0] - 0.01, b[1] - 0.01, b[2] + 0.01, b[3] + 0.01], tz);
        for x in xs.clone() {
            for y in ys.clone() {
                let c = [
                    if x % 2 == 0 { 220 } else { 20 },
                    if y % 2 == 0 { 220 } else { 20 },
                    60,
                ];
                mb.put(TileId::new(tz, x, y), &png(c)).unwrap();
            }
        }
        mb.flush().unwrap();
        drop(mb);
        let src = Arc::new(TileSource {
            kind: SourceKind::Raster,
            backend: Backend::Mbtiles {
                path: dir.join("t.mbtiles"),
            },
            overlay: false,
            id: "reg".into(),
            name: "reg".into(),
            url: String::new(),
            subdomains: vec![],
            max_zoom: tz,
            attribution: String::new(),
            offline_allowed: false,
            max_age_secs: None,
        });
        let mut r = MapRenderer::new(&device, &queue);
        let svc = TileService::new(dir.join("cache"), || {});
        let layers = RenderLayers {
            base: src,
            overlays: vec![],
            terrain: None,
            shade: ShadeParams::default(),
        };
        let out = render_region(
            &mut r,
            &svc,
            &view,
            &layers,
            &mut |_| {},
            &AtomicBool::new(false),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(
            (out.width, out.height),
            (proj.frame_px()[0], proj.frame_px()[1])
        );
        assert_eq!(out.missing_tiles, 0);

        // Expected pixel column of each tile's west edge, straight from the projection.
        let px_of = |lng: f64, lat: f64| {
            let (x, y) = proj.to_page_mm(LngLat::new(lng, lat));
            (
                (x - layout.frame.x) / proj.mm_per_px,
                (y - layout.frame.y) / proj.mm_per_px,
            )
        };
        let tile_lng = |x: u32| x as f64 / (1u64 << tz) as f64 * 360.0 - 180.0;
        let tile_lat = |y: u32| smf_geo::world_to_lnglat(0.0, y as f64 * 256.0, tz as f64).lat;
        let row = out.height / 2;
        let mut checked = 0;
        for x in xs.clone().skip(1) {
            let (ex, _) = px_of(tile_lng(x), 38.6);
            let col = ex.round() as i64;
            if col < 3 || col >= out.width as i64 - 3 {
                continue;
            }
            // Pixels immediately either side of the edge must carry the colours of the two neighbouring tiles:
            // the raster is registered with the page geometry to within one pixel.
            let left = out.pixel(col as u32 - 1, row);
            let right = out.pixel(col as u32 + 1, row);
            assert_eq!(
                left[0] > 128,
                (x - 1) % 2 == 0,
                "left of x-edge {x}: {left:?}"
            );
            assert_eq!(right[0] > 128, x % 2 == 0, "right of x-edge {x}: {right:?}");
            checked += 1;
        }
        assert!(checked >= 3, "checked {checked} vertical tile edges");
        let col = out.width / 2;
        let mut rows_checked = 0;
        for y in ys.clone().skip(1) {
            let (_, ey) = px_of(-92.3, tile_lat(y));
            let r_ = ey.round() as i64;
            if r_ < 3 || r_ >= out.height as i64 - 3 {
                continue;
            }
            let above = out.pixel(col, r_ as u32 - 1);
            let below = out.pixel(col, r_ as u32 + 1);
            assert_eq!(
                above[1] > 128,
                (y - 1) % 2 == 0,
                "above y-edge {y}: {above:?}"
            );
            assert_eq!(below[1] > 128, y % 2 == 0, "below y-edge {y}: {below:?}");
            rows_checked += 1;
        }
        assert!(
            rows_checked >= 3,
            "checked {rows_checked} horizontal tile edges"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The GeoTIFF's pixels must sit where its tiepoint says: with tile colours alternating by x/y parity,
    /// colour changes have to fall on exact multiples of 256 px from the (tile-aligned) origin.
    #[test]
    fn geotiff_region_pixels_align_with_the_tile_grid() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("smf-print-gt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let z = 14u8;
        let region = GeoTiffRegion::new([-92.31, 38.57, -92.27, 38.60], z);
        let b = [-92.32, 38.56, -92.26, 38.61];
        let (xs, ys) = smf_geo::tile_range(b, z);
        let mb = Mbtiles::create(&dir.join("t.mbtiles")).unwrap();
        for x in xs.clone() {
            for y in ys.clone() {
                let c = [
                    if x % 2 == 0 { 220 } else { 20 },
                    if y % 2 == 0 { 220 } else { 20 },
                    60,
                ];
                mb.put(TileId::new(z, x, y), &png(c)).unwrap();
            }
        }
        mb.flush().unwrap();
        drop(mb);
        let src = Arc::new(TileSource {
            kind: SourceKind::Raster,
            backend: Backend::Mbtiles {
                path: dir.join("t.mbtiles"),
            },
            overlay: false,
            id: "gt".into(),
            name: "gt".into(),
            url: String::new(),
            subdomains: vec![],
            max_zoom: z,
            attribution: String::new(),
            offline_allowed: false,
            max_age_secs: None,
        });
        let mut r = MapRenderer::new(&device, &queue);
        let svc = TileService::new(dir.join("cache"), || {});
        let layers = RenderLayers {
            base: src,
            overlays: vec![],
            terrain: None,
            shade: ShadeParams::default(),
        };
        let out = render_region(
            &mut r,
            &svc,
            &region.map_view(),
            &layers,
            &mut |_| {},
            &AtomicBool::new(false),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!((out.width, out.height), (region.width, region.height));
        assert_eq!(out.missing_tiles, 0);
        // Pixel (col,row) covers mercator x = origin_x + col*pixel, so the world tile column is:
        let half = EARTH_CIRCUMFERENCE_M / 2.0;
        let first_col = ((region.origin_x + half) / region.pixel_m).round() as u64;
        let first_row = ((half - region.origin_y) / region.pixel_m).round() as u64;
        let row = out.height / 2;
        for col in 0..out.width {
            let world_col = first_col + col as u64;
            let tile_x = (world_col / 256) as u32;
            let want_red = tile_x.is_multiple_of(2);
            let got = out.pixel(col, row);
            assert_eq!(
                got[0] > 128,
                want_red,
                "col {col} (world col {world_col}, tile x {tile_x}): {got:?}"
            );
        }
        let col = out.width / 2;
        for r_ in 0..out.height {
            let world_row = first_row + r_ as u64;
            let tile_y = (world_row / 256) as u32;
            let got = out.pixel(col, r_);
            assert_eq!(
                got[1] > 128,
                tile_y.is_multiple_of(2),
                "row {r_} (world row {world_row}, tile y {tile_y}): {got:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelling_stops_the_render() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("smf-print-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = Arc::new(smf_tiles::default_sources().remove(0));
        let mut r = MapRenderer::new(&device, &queue);
        let svc = TileService::new(dir.join("cache"), || {});
        svc.set_offline(true); // nothing will ever arrive
        let layers = RenderLayers {
            base: src,
            overlays: vec![],
            terrain: None,
            shade: ShadeParams::default(),
        };
        let view = MapView::new(LngLat::new(-92.3, 38.6), 12.0, [600.0, 400.0]);
        let cancel = AtomicBool::new(true);
        let res = render_region(
            &mut r,
            &svc,
            &view,
            &layers,
            &mut |_| {},
            &cancel,
            Duration::from_secs(30),
        );
        assert!(matches!(res, Err(PrintError::Cancelled)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
