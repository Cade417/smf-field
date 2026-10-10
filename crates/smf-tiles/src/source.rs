use serde::Deserialize;
use smf_geo::TileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Display imagery (basemaps and overlays).
    #[default]
    Raster,
    /// Terrarium-encoded elevation tiles.
    Terrarium,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TileSource {
    #[serde(default)]
    pub kind: SourceKind,
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
}

impl TileSource {
    pub fn tile_url(&self, t: TileId) -> String {
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
