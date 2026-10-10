//! Local GeoTIFF as a tile source: decoded once into RGBA, then resampled into Web
//! Mercator tiles on demand. Supports north-up rasters in WGS84/NAD83 geographic,
//! Web Mercator, and UTM (WGS84/NAD83), 8/16-bit gray/RGB/RGBA. Anything else is
//! refused with a message instead of being drawn in the wrong place.

use smf_geo::{
    LngLat, TILE_SIZE, TileId, lnglat_to_mercator, mercator_to_lnglat, tile_bounds_mercator,
};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

/// Largest raster held in memory (pixels). RGBA8 makes this 256 MB.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RasterCrs {
    Geographic,
    WebMercator,
    Utm { zone: u8, north: bool },
}

impl RasterCrs {
    fn project(self, p: LngLat) -> (f64, f64) {
        match self {
            RasterCrs::Geographic => (p.lng, p.lat),
            RasterCrs::WebMercator => lnglat_to_mercator(p),
            RasterCrs::Utm { zone, .. } => {
                let (n, e, _) = utm::to_utm_wgs84(p.lat, p.lng, zone);
                (e, n)
            }
        }
    }

    fn to_lnglat(self, x: f64, y: f64) -> Option<LngLat> {
        let p = match self {
            RasterCrs::Geographic => LngLat::new(x, y),
            RasterCrs::WebMercator => mercator_to_lnglat(x, y),
            RasterCrs::Utm { zone, north } => {
                let (lat, lng) =
                    utm::wsg84_utm_to_lat_lon(x, y, zone, if north { 'N' } else { 'M' }).ok()?;
                LngLat::new(lng, lat)
            }
        };
        (p.lng.is_finite() && p.lat.is_finite()).then_some(p)
    }
}

pub fn crs_from_epsg(code: u32) -> Result<RasterCrs, String> {
    Ok(match code {
        4326 | 4269 | 4283 | 4979 => RasterCrs::Geographic,
        3857 | 3785 | 900913 => RasterCrs::WebMercator,
        32601..=32660 => RasterCrs::Utm {
            zone: (code - 32600) as u8,
            north: true,
        },
        32701..=32760 => RasterCrs::Utm {
            zone: (code - 32700) as u8,
            north: false,
        },
        26901..=26923 => RasterCrs::Utm {
            zone: (code - 26900) as u8,
            north: true,
        },
        _ => {
            return Err(format!(
                "unsupported coordinate system EPSG:{code}; reproject the file to WGS84 (EPSG:4326), Web Mercator or UTM"
            ));
        }
    })
}

pub struct GeoRaster {
    pub width: u32,
    pub height: u32,
    rgba: Vec<u8>,
    crs: RasterCrs,
    /// CRS coordinates of the raster's top-left corner and pixel size (positive both).
    x0: f64,
    y0: f64,
    sx: f64,
    sy: f64,
}

fn open_err(e: impl std::fmt::Display) -> String {
    format!("GeoTIFF: {e}")
}

impl GeoRaster {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut dec =
            Decoder::new(BufReader::new(File::open(path).map_err(open_err)?)).map_err(open_err)?;
        let (width, height) = dec.dimensions().map_err(open_err)?;
        if width as u64 * height as u64 > MAX_PIXELS {
            return Err(format!(
                "GeoTIFF is {width}x{height} pixels, over the {} megapixel limit; convert it to MBTiles (e.g. gdal2tiles) instead",
                MAX_PIXELS / 1024 / 1024
            ));
        }
        // ModelPixelScale (33550), ModelTiepoint (33922), GeoKeyDirectory (34735)
        let scale = dec.get_tag_f64_vec(Tag::Unknown(33550)).map_err(|_| "GeoTIFF has no georeferencing (missing ModelPixelScale; rotated/transformed rasters are not supported)".to_string())?;
        let tie = dec
            .get_tag_f64_vec(Tag::Unknown(33922))
            .map_err(|_| "GeoTIFF has no tiepoint".to_string())?;
        let keys = dec
            .get_tag_u16_vec(Tag::Unknown(34735))
            .map_err(|_| "GeoTIFF has no GeoKey directory".to_string())?;
        if scale.len() < 2 || tie.len() < 6 || keys.len() < 4 {
            return Err("GeoTIFF georeferencing tags are malformed".into());
        }
        let key = |id: u16| -> Option<u16> {
            keys[4..]
                .chunks_exact(4)
                .find(|k| k[0] == id && k[1] == 0)
                .map(|k| k[3])
        };
        let model = key(1024).unwrap_or(0);
        let epsg = match model {
            2 => key(2048),
            1 => key(3072),
            _ => None,
        }
        .ok_or("GeoTIFF has no EPSG code (user-defined coordinate systems are not supported)")?;
        let crs = crs_from_epsg(epsg as u32)?;
        let point_is_area = key(1025) != Some(2);
        let (sx, sy) = (scale[0], scale[1]);
        let (mut x0, mut y0) = (tie[3] - tie[0] * sx, tie[4] + tie[1] * sy);
        if !point_is_area {
            // PixelIsPoint: the tiepoint names pixel centres, so the corner is half a pixel out.
            x0 -= sx / 2.0;
            y0 += sy / 2.0;
        }
        let color = dec.colortype().map_err(open_err)?;
        let data = dec.read_image().map_err(open_err)?;
        let to8 = |v: &DecodingResult| -> Result<Vec<u8>, String> {
            match v {
                DecodingResult::U8(d) => Ok(d.clone()),
                DecodingResult::U16(d) => Ok(d.iter().map(|x| (x >> 8) as u8).collect()),
                _ => Err("GeoTIFF sample type not supported (need 8 or 16-bit integers)".into()),
            }
        };
        let raw = to8(&data)?;
        use tiff::ColorType as C;
        let n = (width * height) as usize;
        let mut rgba = Vec::with_capacity(n * 4);
        match color {
            C::Gray(8) | C::Gray(16) => raw
                .iter()
                .for_each(|g| rgba.extend_from_slice(&[*g, *g, *g, 255])),
            C::GrayA(8) | C::GrayA(16) => raw
                .chunks_exact(2)
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
            C::RGB(8) | C::RGB(16) => raw
                .chunks_exact(3)
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            C::RGBA(8) | C::RGBA(16) => rgba = raw,
            other => {
                return Err(format!(
                    "GeoTIFF color type {other:?} not supported (need gray, RGB or RGBA)"
                ));
            }
        }
        if rgba.len() != n * 4 {
            return Err("GeoTIFF pixel data has an unexpected size".into());
        }
        Ok(Self {
            width,
            height,
            rgba,
            crs,
            x0,
            y0,
            sx,
            sy,
        })
    }

    /// (min_lng, min_lat, max_lng, max_lat) covering the raster, from its corners and edge midpoints.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        let (w, h) = (self.width as f64, self.height as f64);
        let mut b: Option<[f64; 4]> = None;
        for (fx, fy) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (0.5, 0.0),
            (0.5, 1.0),
            (0.0, 0.5),
            (1.0, 0.5),
        ] {
            let p = self
                .crs
                .to_lnglat(self.x0 + fx * w * self.sx, self.y0 - fy * h * self.sy)?;
            let e = b.get_or_insert([p.lng, p.lat, p.lng, p.lat]);
            *e = [
                e[0].min(p.lng),
                e[1].min(p.lat),
                e[2].max(p.lng),
                e[3].max(p.lat),
            ];
        }
        b
    }

    /// Zoom at which one tile pixel is about one raster pixel.
    pub fn native_max_zoom(&self) -> u8 {
        let lat = self.bounds().map_or(0.0, |b| (b[1] + b[3]) / 2.0);
        // Ground metres per raster pixel.
        let m = match self.crs {
            RasterCrs::Geographic => self.sx * 111_320.0 * lat.to_radians().cos(),
            RasterCrs::WebMercator => self.sx * lat.to_radians().cos(),
            RasterCrs::Utm { .. } => self.sx,
        };
        let z = (smf_geo::EARTH_CIRCUMFERENCE_M * lat.to_radians().cos()
            / (TILE_SIZE * m.max(1e-6)))
        .log2();
        z.ceil().clamp(0.0, 22.0) as u8
    }

    /// Pixel with coordinates clamped to the raster, so bilinear sampling at the edge repeats the border.
    fn px(&self, col: i64, row: i64) -> [f32; 4] {
        let c = col.clamp(0, self.width as i64 - 1) as usize;
        let r = row.clamp(0, self.height as i64 - 1) as usize;
        let i = (r * self.width as usize + c) * 4;
        [
            self.rgba[i] as f32,
            self.rgba[i + 1] as f32,
            self.rgba[i + 2] as f32,
            self.rgba[i + 3] as f32,
        ]
    }

    /// RGBA 256x256 for a Web Mercator tile; pixels outside the raster are transparent.
    pub fn render_tile(&self, id: TileId) -> Vec<u8> {
        let b = tile_bounds_mercator(id);
        let step = (b[2] - b[0]) / 256.0;
        let mut out = vec![0u8; 256 * 256 * 4];
        for j in 0..256 {
            for i in 0..256 {
                let mx = b[0] + (i as f64 + 0.5) * step;
                let my = b[3] - (j as f64 + 0.5) * step;
                let (x, y) = self.crs.project(mercator_to_lnglat(mx, my));
                // Continuous pixel coordinates with pixel centres at integers.
                let (fx, fy) = ((x - self.x0) / self.sx - 0.5, (self.y0 - y) / self.sy - 0.5);
                // Outside the raster footprint: leave transparent (crisp edge, no fade).
                if !(fx >= -0.5
                    && fy >= -0.5
                    && fx <= self.width as f64 - 0.5
                    && fy <= self.height as f64 - 0.5)
                {
                    continue;
                }
                let (c0, r0) = (fx.floor() as i64, fy.floor() as i64);
                let (tx, ty) = ((fx - c0 as f64) as f32, (fy - r0 as f64) as f32);
                let (p00, p10, p01, p11) = (
                    self.px(c0, r0),
                    self.px(c0 + 1, r0),
                    self.px(c0, r0 + 1),
                    self.px(c0 + 1, r0 + 1),
                );
                let o = (j * 256 + i) * 4;
                for k in 0..4 {
                    let top = p00[k] * (1.0 - tx) + p10[k] * tx;
                    let bot = p01[k] * (1.0 - tx) + p11[k] * tx;
                    out[o + k] = (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        out
    }

    /// Does the tile touch the raster at all (cheap bounding-box test)?
    pub fn intersects(&self, id: TileId) -> bool {
        let Some(bb) = self.bounds() else { return true };
        let b = tile_bounds_mercator(id);
        let (sw, ne) = (
            mercator_to_lnglat(b[0], b[1]),
            mercator_to_lnglat(b[2], b[3]),
        );
        !(ne.lng < bb[0] || sw.lng > bb[2] || ne.lat < bb[1] || sw.lat > bb[3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::encoder::{TiffEncoder, colortype};

    /// Write an RGB GeoTIFF whose red channel equals the column index (so georeferencing is checkable).
    #[allow(clippy::too_many_arguments)] // test fixture: each argument is one GeoTIFF field
    fn write_gradient(
        path: &Path,
        w: u32,
        h: u32,
        epsg: u16,
        model: u16,
        origin: (f64, f64),
        scale: (f64, f64),
        point: bool,
    ) {
        let mut data = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[(x % 256) as u8, 100, 50]);
            }
        }
        let mut f = File::create(path).unwrap();
        let mut enc = TiffEncoder::new(&mut f).unwrap();
        let mut img = enc.new_image::<colortype::RGB8>(w, h).unwrap();
        img.encoder()
            .write_tag(Tag::Unknown(33550), &[scale.0, scale.1, 0.0][..])
            .unwrap();
        img.encoder()
            .write_tag(
                Tag::Unknown(33922),
                &[0.0, 0.0, 0.0, origin.0, origin.1, 0.0][..],
            )
            .unwrap();
        let geo_key = if model == 2 { 2048u16 } else { 3072 };
        let keys: Vec<u16> = vec![
            1,
            1,
            0,
            3,
            1024,
            0,
            1,
            model,
            1025,
            0,
            1,
            if point { 2 } else { 1 },
            geo_key,
            0,
            1,
            epsg,
        ];
        img.encoder()
            .write_tag(Tag::Unknown(34735), &keys[..])
            .unwrap();
        img.write_data(&data).unwrap();
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("smf-gt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("t.tif")
    }

    #[test]
    fn geographic_raster_lines_up_within_one_pixel() {
        // 200x100 px covering lng -92.10..-91.90, lat 38.55..38.45 (0.001 deg/px).
        let path = tmp("geo");
        write_gradient(
            &path,
            200,
            100,
            4326,
            2,
            (-92.10, 38.55),
            (0.001, 0.001),
            false,
        );
        let r = GeoRaster::open(&path).unwrap();
        let b = r.bounds().unwrap();
        assert!((b[0] + 92.10).abs() < 1e-9 && (b[2] + 91.90).abs() < 1e-9);
        assert!((b[3] - 38.55).abs() < 1e-9 && (b[1] - 38.45).abs() < 1e-9);
        // Render the z12 tile at the raster's centre and find the column where red == 100 (lng = -92.0).
        let centre = LngLat::new(-92.0, 38.5);
        let id = TileId::from_lnglat(centre, 12);
        let tile = r.render_tile(id);
        let mb = tile_bounds_mercator(id);
        let step = (mb[2] - mb[0]) / 256.0;
        // The tile pixel whose centre is nearest lng=-92.0 should carry red ~= 100.
        let (mx, _) = lnglat_to_mercator(LngLat::new(-92.0, 38.5));
        let col = ((mx - mb[0]) / step).floor() as usize;
        let row = 128;
        let red = tile[(row * 256 + col) * 4] as i32;
        assert!(
            (red - 100).abs() <= 1,
            "expected column 100 at lng -92.0, got {red}"
        );
        assert_eq!(
            tile[(row * 256 + col) * 4 + 3],
            255,
            "inside the raster is opaque"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn utm_and_web_mercator_rasters_are_georeferenced_correctly() {
        // UTM 15N: easting 500000 is -93.0 exactly.
        let path = tmp("utm");
        let (n0, e0, _) = utm::to_utm_wgs84(38.5, -93.0, 15);
        write_gradient(
            &path,
            100,
            100,
            32615,
            1,
            (e0 - 5000.0, n0 + 5000.0),
            (100.0, 100.0),
            false,
        );
        let r = GeoRaster::open(&path).unwrap();
        let b = r.bounds().unwrap();
        assert!(((b[0] + b[2]) / 2.0 + 93.0).abs() < 0.01, "{b:?}");
        let id = TileId::from_lnglat(LngLat::new(-93.0, 38.5), 13);
        let t = r.render_tile(id);
        assert!(
            t.chunks_exact(4).any(|p| p[3] == 255),
            "tile inside the raster has pixels"
        );
        // Web Mercator.
        let path2 = path.with_file_name("m.tif");
        let (mx, my) = lnglat_to_mercator(LngLat::new(-92.3, 38.6));
        write_gradient(
            &path2,
            64,
            64,
            3857,
            1,
            (mx - 3200.0, my + 3200.0),
            (100.0, 100.0),
            true,
        );
        let r2 = GeoRaster::open(&path2).unwrap();
        let id = TileId::from_lnglat(LngLat::new(-92.3, 38.6), 12);
        assert!(r2.intersects(id));
        assert!(r2.render_tile(id).chunks_exact(4).any(|p| p[3] == 255));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn tiles_away_from_the_raster_are_transparent_and_intersect_test_agrees() {
        let path = tmp("away");
        write_gradient(
            &path,
            50,
            50,
            4326,
            2,
            (-92.10, 38.55),
            (0.001, 0.001),
            false,
        );
        let r = GeoRaster::open(&path).unwrap();
        let far = TileId::from_lnglat(LngLat::new(-100.0, 30.0), 10);
        assert!(!r.intersects(far));
        assert!(r.render_tile(far).chunks_exact(4).all(|p| p[3] == 0));
        // 0.001 deg is ~87 m per pixel at 38.5N, which matches a tile pixel at about z11.
        assert!(
            (10..=12).contains(&r.native_max_zoom()),
            "{}",
            r.native_max_zoom()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unsupported_inputs_are_refused_with_reasons() {
        // Unsupported CRS (State Plane Missouri Central, ftUS: EPSG:3602).
        let path = tmp("badcrs");
        write_gradient(&path, 10, 10, 3602, 1, (0.0, 0.0), (1.0, 1.0), false);
        let err = GeoRaster::open(&path).err().unwrap();
        assert!(err.contains("EPSG:3602"), "{err}");
        // A plain TIFF with no georeferencing.
        let plain = path.with_file_name("plain.tif");
        let mut f = File::create(&plain).unwrap();
        TiffEncoder::new(&mut f)
            .unwrap()
            .write_image::<colortype::Gray8>(4, 4, &[0u8; 16])
            .unwrap();
        drop(f);
        let err = GeoRaster::open(&plain).err().unwrap();
        assert!(err.contains("georeferencing"), "{err}");
        assert!(GeoRaster::open(&path.with_file_name("missing.tif")).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn epsg_mapping() {
        assert_eq!(
            crs_from_epsg(32615),
            Ok(RasterCrs::Utm {
                zone: 15,
                north: true
            })
        );
        assert_eq!(
            crs_from_epsg(32715),
            Ok(RasterCrs::Utm {
                zone: 15,
                north: false
            })
        );
        assert_eq!(
            crs_from_epsg(26915),
            Ok(RasterCrs::Utm {
                zone: 15,
                north: true
            })
        );
        assert_eq!(crs_from_epsg(4326), Ok(RasterCrs::Geographic));
        assert!(crs_from_epsg(2277).is_err());
    }
}
