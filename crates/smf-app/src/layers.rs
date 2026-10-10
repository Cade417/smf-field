//! Custom layers and offline-area planning: pure logic (validation, persistence, spec
//! building) kept separate from the egui dialogs so it can be unit tested.

use smf_tiles::{Backend, DownloadSpec, GeoRaster, Mbtiles, SourceKind, TileSource};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Xyz,
    Wms,
    Mbtiles,
    Geotiff,
}

impl LayerKind {
    pub const ALL: [LayerKind; 4] = [
        LayerKind::Xyz,
        LayerKind::Wms,
        LayerKind::Mbtiles,
        LayerKind::Geotiff,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayerKind::Xyz => "XYZ / TMS URL",
            LayerKind::Wms => "WMS",
            LayerKind::Mbtiles => "MBTiles file",
            LayerKind::Geotiff => "GeoTIFF file",
        }
    }
}

/// Everything the "Add layer" dialog collects.
#[derive(Debug, Clone)]
pub struct AddLayerInput {
    pub kind: LayerKind,
    pub name: String,
    pub url: String,
    pub wms_layers: String,
    pub wms_format: String,
    pub wms_version: String,
    pub wms_transparent: bool,
    pub path: String,
    pub max_zoom: u8,
    pub attribution: String,
    pub overlay: bool,
    pub offline_allowed: bool,
}

impl Default for AddLayerInput {
    fn default() -> Self {
        Self {
            kind: LayerKind::Xyz,
            name: String::new(),
            url: String::new(),
            wms_layers: String::new(),
            wms_format: "image/png".into(),
            wms_version: "1.1.1".into(),
            wms_transparent: true,
            path: String::new(),
            max_zoom: 18,
            attribution: String::new(),
            overlay: false,
            offline_allowed: false,
        }
    }
}

fn is_http(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() > 8
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-').chars().take(32).collect()
}

fn short_hash(s: &str) -> String {
    // FNV-1a: stable across runs, unlike std's randomized hasher, so ids survive restarts.
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

impl AddLayerInput {
    /// Validate and turn the dialog contents into a tile source. Errors are user-facing sentences.
    pub fn build(&self, existing_ids: &[String]) -> Result<TileSource, String> {
        let mut max_zoom = self.max_zoom.min(22);
        let (backend, url, mut name, key) = match self.kind {
            LayerKind::Xyz => {
                let u = self.url.trim();
                if !is_http(u) {
                    return Err("The URL must start with http:// or https://".into());
                }
                let has = |t: &str| u.contains(t);
                if !(has("{z}") && has("{x}") && (has("{y}") || has("{-y}"))) {
                    return Err("The URL template needs {z}, {x} and {y} (or {-y} for TMS), e.g. https://example.org/tiles/{z}/{x}/{y}.png".into());
                }
                (Backend::Http, u.to_owned(), host_of(u), u.to_owned())
            }
            LayerKind::Wms => {
                let u = self.url.trim();
                if !is_http(u) {
                    return Err("The WMS URL must start with http:// or https://".into());
                }
                if self.wms_layers.trim().is_empty() {
                    return Err("Enter the WMS layer name(s), comma separated.".into());
                }
                let backend = Backend::Wms {
                    layers: self.wms_layers.trim().to_owned(),
                    format: if self.wms_format.trim().is_empty() {
                        "image/png".into()
                    } else {
                        self.wms_format.trim().to_owned()
                    },
                    transparent: self.wms_transparent,
                    version: self.wms_version.clone(),
                };
                (
                    backend,
                    u.to_owned(),
                    self.wms_layers.trim().to_owned(),
                    format!("{u}|{}", self.wms_layers),
                )
            }
            LayerKind::Mbtiles => {
                let p = Path::new(self.path.trim());
                if !p.is_file() {
                    return Err("Choose an existing .mbtiles file.".into());
                }
                let m = Mbtiles::open_read_only(p)
                    .map_err(|e| format!("Not a usable MBTiles file: {e}"))?;
                if let Some((_, hi)) = m.zoom_range() {
                    max_zoom = hi;
                }
                let nm = m.meta("name").unwrap_or_else(|| {
                    p.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                (
                    Backend::Mbtiles { path: p.to_owned() },
                    String::new(),
                    nm,
                    p.display().to_string(),
                )
            }
            LayerKind::Geotiff => {
                let p = Path::new(self.path.trim());
                if !p.is_file() {
                    return Err("Choose an existing .tif / .tiff file.".into());
                }
                let r = GeoRaster::open(p)?;
                // Allow zooming a little past the raster's native resolution.
                max_zoom = (r.native_max_zoom() + 1).min(22);
                let nm = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    Backend::Geotiff { path: p.to_owned() },
                    String::new(),
                    nm,
                    p.display().to_string(),
                )
            }
        };
        if !self.name.trim().is_empty() {
            name = self.name.trim().to_owned();
        }
        if name.is_empty() {
            name = "Custom layer".into();
        }
        let mut id = format!("custom-{}-{}", slug(&name), short_hash(&key));
        let mut n = 2;
        while existing_ids.contains(&id) {
            id = format!("custom-{}-{}-{n}", slug(&name), short_hash(&key));
            n += 1;
        }
        let local = matches!(backend, Backend::Mbtiles { .. } | Backend::Geotiff { .. });
        Ok(TileSource {
            kind: SourceKind::Raster,
            backend,
            // GeoTIFFs and WMS "transparent" layers are almost always meant to sit on a basemap.
            overlay: self.overlay,
            id,
            name,
            url,
            subdomains: vec![],
            max_zoom,
            attribution: if self.attribution.trim().is_empty() {
                "Custom layer".into()
            } else {
                self.attribution.trim().to_owned()
            },
            // Local files are already offline; network layers need the user to assert they may be downloaded.
            offline_allowed: !local && self.offline_allowed,
            max_age_secs: None,
        })
    }
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?']).next())
        .unwrap_or("Custom layer")
        .to_owned()
}

/// Custom layers persisted as JSON in the settings store (`layers.json`).
pub struct LayerStore;

impl LayerStore {
    pub fn new() -> Self {
        Self
    }

    pub fn load(&self) -> Vec<TileSource> {
        crate::store::load_json("layers.json")
    }

    pub fn save(&self, layers: &[TileSource]) -> std::io::Result<()> {
        crate::store::save_json("layers.json", &layers);
        Ok(())
    }
}

/// Inputs of the "Download area" dialog.
#[derive(Debug, Clone)]
pub struct DownloadInput {
    pub source: TileSource,
    pub terrain: Option<TileSource>,
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub name: String,
    pub dir: PathBuf,
}

/// Terrain tiles end at z15 and are needed far less deeply than imagery, so cap them at z14.
const TERRAIN_DOWNLOAD_MAX_ZOOM: u8 = 14;

impl DownloadInput {
    pub fn file_stem(&self) -> String {
        let s = slug(&self.name);
        if s.is_empty() {
            slug(&self.source.id)
        } else {
            s
        }
    }

    pub fn specs(&self) -> Vec<DownloadSpec> {
        let stem = self.file_stem();
        let mut v = vec![DownloadSpec {
            source: self.source.clone(),
            bounds: self.bounds,
            min_zoom: self.min_zoom,
            max_zoom: self.max_zoom.min(self.source.max_zoom),
            out: self.dir.join(format!("{stem}.mbtiles")),
            max_tiles: 0,
            workers: 0,
        }];
        if let Some(t) = &self.terrain {
            v.push(DownloadSpec {
                source: t.clone(),
                bounds: self.bounds,
                min_zoom: self.min_zoom.min(TERRAIN_DOWNLOAD_MAX_ZOOM),
                max_zoom: self.max_zoom.min(TERRAIN_DOWNLOAD_MAX_ZOOM).min(t.max_zoom),
                out: self.dir.join(format!("{stem}-terrain.mbtiles")),
                max_tiles: 0,
                workers: 0,
            });
        }
        v
    }
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("smf-layers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn xyz_validation_catches_common_mistakes() {
        let mut i = AddLayerInput::default();
        assert!(i.build(&[]).unwrap_err().contains("http"));
        i.url = "ftp://x/{z}/{x}/{y}.png".into();
        assert!(i.build(&[]).unwrap_err().contains("http"));
        i.url = "https://tiles.example.org/{z}/{x}.png".into();
        assert!(i.build(&[]).unwrap_err().contains("{y}"));
        i.url = "https://tiles.example.org/{z}/{x}/{y}.png".into();
        let s = i.build(&[]).unwrap();
        assert_eq!(s.name, "tiles.example.org");
        assert_eq!(s.backend, Backend::Http);
        assert!(!s.offline_allowed, "downloading must be opted into");
        assert!(!s.overlay);
        i.url = "https://tiles.example.org/{z}/{x}/{-y}.png".into();
        i.name = "My TMS".into();
        i.overlay = true;
        i.offline_allowed = true;
        let s = i.build(&[]).unwrap();
        assert_eq!(
            (s.name.as_str(), s.overlay, s.offline_allowed),
            ("My TMS", true, true)
        );
    }

    #[test]
    fn ids_are_stable_and_unique() {
        let mut i = AddLayerInput {
            url: "https://a.org/{z}/{x}/{y}.png".into(),
            name: "Roads!".into(),
            ..Default::default()
        };
        let a = i.build(&[]).unwrap();
        let b = i.build(&[]).unwrap();
        assert_eq!(a.id, b.id, "same input, same id (survives restarts)");
        assert!(a.id.starts_with("custom-roads-"), "{}", a.id);
        let c = i.build(std::slice::from_ref(&a.id)).unwrap();
        assert_ne!(a.id, c.id);
        i.url = "https://b.org/{z}/{x}/{y}.png".into();
        assert_ne!(i.build(&[]).unwrap().id, a.id);
    }

    #[test]
    fn wms_requires_layers_and_builds_backend() {
        let mut i = AddLayerInput {
            kind: LayerKind::Wms,
            url: "https://gis.example.gov/wms".into(),
            ..Default::default()
        };
        assert!(i.build(&[]).unwrap_err().contains("layer name"));
        i.wms_layers = "fire_perimeters".into();
        let s = i.build(&[]).unwrap();
        match &s.backend {
            Backend::Wms {
                layers,
                format,
                transparent,
                version,
            } => {
                assert_eq!(
                    (
                        layers.as_str(),
                        format.as_str(),
                        *transparent,
                        version.as_str()
                    ),
                    ("fire_perimeters", "image/png", true, "1.1.1")
                );
            }
            b => panic!("{b:?}"),
        }
        assert!(
            s.tile_url(smf_geo::TileId::new(3, 1, 2))
                .contains("LAYERS=fire_perimeters")
        );
    }

    #[test]
    fn local_files_are_validated_and_never_downloadable() {
        let dir = tmp("files");
        let mut i = AddLayerInput {
            kind: LayerKind::Mbtiles,
            path: dir.join("missing.mbtiles").display().to_string(),
            ..Default::default()
        };
        assert!(i.build(&[]).unwrap_err().contains("existing"));
        let p = dir.join("a.mbtiles");
        let m = Mbtiles::create(&p).unwrap();
        m.set_meta("name", "County aerials").unwrap();
        m.put(smf_geo::TileId::new(9, 1, 1), b"x").unwrap();
        m.put(smf_geo::TileId::new(13, 1, 1), b"x").unwrap();
        drop(m);
        i.path = p.display().to_string();
        i.offline_allowed = true;
        let s = i.build(&[]).unwrap();
        assert_eq!(s.name, "County aerials");
        assert_eq!(s.max_zoom, 13, "max zoom comes from the file");
        assert!(!s.offline_allowed, "local files can't be downloaded");
        std::fs::write(dir.join("junk.mbtiles"), b"nope").unwrap();
        i.path = dir.join("junk.mbtiles").display().to_string();
        assert!(i.build(&[]).unwrap_err().contains("MBTiles"));
        i.kind = LayerKind::Geotiff;
        i.path = dir.join("junk.mbtiles").display().to_string();
        assert!(i.build(&[]).is_err(), "a non-TIFF is refused");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn layer_store_roundtrips_and_survives_corruption() {
        // Storage is redirected to a temporary folder so real settings are never touched.
        let dir = crate::store::use_temp_root("layers");
        let store = LayerStore::new();
        assert!(store.load().is_empty(), "missing file is fine");
        let s = AddLayerInput {
            url: "https://a.org/{z}/{x}/{y}.png".into(),
            overlay: true,
            ..Default::default()
        }
        .build(&[])
        .unwrap();
        store.save(std::slice::from_ref(&s)).unwrap();
        let back = store.load();
        assert_eq!(back.len(), 1);
        assert_eq!(
            (back[0].id.as_str(), back[0].overlay),
            (s.id.as_str(), true)
        );
        crate::store::write("layers.json", "{ not json");
        assert!(
            store.load().is_empty(),
            "corrupt config must not crash the app"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn download_specs_name_files_and_clip_terrain_zoom() {
        let mut src = smf_tiles::default_sources().remove(0);
        src.max_zoom = 16;
        let terrain = smf_tiles::terrain_source();
        let input = DownloadInput {
            source: src.clone(),
            terrain: Some(terrain),
            bounds: [-92.4, 38.5, -92.2, 38.7],
            min_zoom: 9,
            max_zoom: 17,
            name: "Pine Ridge / Unit 3!".into(),
            dir: PathBuf::from("offline"),
        };
        let specs = input.specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[0].out,
            PathBuf::from("offline/pine-ridge---unit-3.mbtiles")
        );
        assert_eq!(specs[0].max_zoom, 16, "clipped to the source's max zoom");
        assert_eq!(
            specs[1].out,
            PathBuf::from("offline/pine-ridge---unit-3-terrain.mbtiles")
        );
        assert_eq!((specs[1].min_zoom, specs[1].max_zoom), (9, 14));
        let none = DownloadInput {
            terrain: None,
            name: String::new(),
            ..input
        };
        assert_eq!(none.specs().len(), 1);
        assert_eq!(none.file_stem(), slug(&src.id));
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
