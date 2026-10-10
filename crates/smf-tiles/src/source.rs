use serde::{Deserialize, Serialize};
use smf_geo::{TileId, tile_bounds_mercator};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Display imagery (basemaps and overlays).
    #[default]
    Raster,
    /// Terrarium-encoded elevation tiles.
    Terrarium,
}

/// Where a source's tiles come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Backend {
    /// XYZ URL template (`url`).
    #[default]
    Http,
    /// WMS GetMap against `url`, requested per tile in EPSG:3857.
    Wms {
        layers: String,
        format: String,
        transparent: bool,
        version: String,
    },
    /// A local MBTiles file.
    Mbtiles { path: PathBuf },
    /// A local georeferenced GeoTIFF.
    Geotiff { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileSource {
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub backend: Backend,
    /// Drawn on top of the basemap (with opacity) instead of replacing it.
    #[serde(default)]
    pub overlay: bool,
    pub id: String,
    pub name: String,
    /// Template with {z} {x} {y} {-y} (TMS row) and {s} (subdomain).
    pub url: String,
    #[serde(default)]
    pub subdomains: Vec<String>,
    pub max_zoom: u8,
    pub attribution: String,
    /// Gate for the offline downloader; restricted providers forbid bulk download.
    pub offline_allowed: bool,
    /// Cached tiles older than this are re-fetched (live data layers). None = cached tiles never expire.
    #[serde(default)]
    pub max_age_secs: Option<u64>,
}

impl TileSource {
    pub fn tile_url(&self, t: TileId) -> String {
        if let Backend::Wms {
            layers,
            format,
            transparent,
            version,
        } = &self.backend
        {
            let b = tile_bounds_mercator(t);
            let sep = if self.url.contains('?') {
                if self.url.ends_with('?') || self.url.ends_with('&') {
                    ""
                } else {
                    "&"
                }
            } else {
                "?"
            };
            // WMS 1.3.0 renamed SRS to CRS; EPSG:3857 keeps x,y axis order in both versions.
            let srs = if version.starts_with("1.3") {
                "CRS"
            } else {
                "SRS"
            };
            return format!(
                "{}{sep}SERVICE=WMS&VERSION={version}&REQUEST=GetMap&LAYERS={}&STYLES=&{srs}=EPSG:3857&BBOX={},{},{},{}&WIDTH=256&HEIGHT=256&FORMAT={}&TRANSPARENT={}",
                self.url,
                url_encode(layers),
                b[0],
                b[1],
                b[2],
                b[3],
                url_encode(format),
                if *transparent { "TRUE" } else { "FALSE" },
            );
        }
        let mut u = self
            .url
            .replace("{z}", &t.z.to_string())
            .replace("{x}", &t.x.to_string())
            .replace("{-y}", &t.tms_y().to_string())
            .replace("{y}", &t.y.to_string());
        if !self.subdomains.is_empty() {
            // Spread by tile coords so a given tile always maps to the same host (cache-friendly).
            let s = &self.subdomains[(t.x + t.y) as usize % self.subdomains.len()];
            u = u.replace("{s}", s);
        }
        u
    }
}

/// Minimal percent-encoding for query values (keeps `,` so multi-layer lists stay readable).
fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b','
            | b'/'
            | b':' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
struct Catalog {
    source: Vec<TileSource>,
}

/// Built-in catalog from assets/sources.toml, embedded so the app has basemaps with no config.
pub fn default_sources() -> Vec<TileSource> {
    let c: Catalog = toml::from_str(include_str!("../../../assets/sources.toml"))
        .expect("assets/sources.toml is invalid");
    c.source
}

/// Display basemaps only (elevation sources are excluded).
pub fn basemaps() -> Vec<TileSource> {
    default_sources()
        .into_iter()
        .filter(|s| s.kind == SourceKind::Raster)
        .collect()
}

/// The elevation source used for 3D terrain, hillshade, slope and profiles.
pub fn terrain_source() -> TileSource {
    default_sources()
        .into_iter()
        .find(|s| s.kind == SourceKind::Terrarium)
        .expect("assets/sources.toml must define a terrarium source")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wms_urls_use_tile_bbox_in_3857() {
        let mut s = default_sources().remove(0);
        s.url = "https://example.org/wms".into();
        s.backend = Backend::Wms {
            layers: "a b,c".into(),
            format: "image/png".into(),
            transparent: true,
            version: "1.1.1".into(),
        };
        let u = s.tile_url(TileId::new(1, 0, 0));
        assert!(u.starts_with("https://example.org/wms?SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS=a%20b,c&STYLES=&SRS=EPSG:3857&BBOX=-20037508"), "{u}");
        assert!(u.contains("WIDTH=256&HEIGHT=256&FORMAT=image/png&TRANSPARENT=TRUE"));
        // A URL that already has a query string gets '&', and 1.3.0 uses CRS.
        s.url = "https://example.org/wms?map=x.map".into();
        s.backend = Backend::Wms {
            layers: "l".into(),
            format: "image/jpeg".into(),
            transparent: false,
            version: "1.3.0".into(),
        };
        let u = s.tile_url(TileId::new(0, 0, 0));
        assert!(
            u.starts_with("https://example.org/wms?map=x.map&SERVICE=WMS&VERSION=1.3.0"),
            "{u}"
        );
        assert!(u.contains("&CRS=EPSG:3857&") && u.contains("TRANSPARENT=FALSE"));
    }

    #[test]
    fn custom_sources_roundtrip_through_json() {
        let mut s = default_sources().remove(0);
        s.backend = Backend::Mbtiles {
            path: PathBuf::from("a/b.mbtiles"),
        };
        s.overlay = true;
        let back: TileSource = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.backend, s.backend);
        assert!(back.overlay);
        // Catalog entries without the new fields still parse to Http / basemap.
        assert_eq!(default_sources()[0].backend, Backend::Http);
        assert!(!default_sources()[0].overlay);
    }

    #[test]
    fn terrain_source_is_separate_from_basemaps() {
        assert!(basemaps().iter().all(|s| s.kind == SourceKind::Raster));
        let t = terrain_source();
        assert_eq!(t.max_zoom, 15);
        assert!(t.offline_allowed);
        assert!(
            t.tile_url(TileId::new(3, 1, 2))
                .ends_with("/terrarium/3/1/2.png")
        );
    }

    #[test]
    fn catalog_parses_and_flags_restricted() {
        let s = default_sources();
        assert!(s.len() >= 6);
        for id in ["osm", "opentopomap", "esri-imagery"] {
            assert!(
                !s.iter().find(|x| x.id == id).unwrap().offline_allowed,
                "{id}"
            );
        }
        assert!(
            s.iter()
                .find(|x| x.id == "usgs-topo")
                .unwrap()
                .offline_allowed
        );
    }

    #[test]
    fn url_templates() {
        let s = default_sources();
        let t = TileId::new(5, 7, 12);
        let usgs = s.iter().find(|x| x.id == "usgs-topo").unwrap();
        assert!(usgs.tile_url(t).ends_with("/tile/5/12/7")); // y before x
        let otm = s.iter().find(|x| x.id == "opentopomap").unwrap();
        assert_eq!(otm.tile_url(t), "https://b.tile.opentopomap.org/5/7/12.png");
        let tms = TileSource {
            url: "x/{z}/{x}/{-y}".into(),
            ..usgs.clone()
        };
        assert_eq!(tms.tile_url(t), "x/5/7/19");
    }
}
