use serde::{Deserialize, Serialize};
use smf_geo::LngLat;

pub type Id = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Geometry {
    Marker(LngLat),
    Line(Vec<LngLat>),
    /// Open ring: the closing edge is implied.
    Polygon(Vec<LngLat>),
    RangeRing {
        center: LngLat,
        radii_m: Vec<f64>,
    },
    /// Text comes from `Feature::name`.
    Label(LngLat),
}

impl Geometry {
    /// Editable vertices in drag order.
    pub fn vertices(&self) -> Vec<LngLat> {
        match self {
            Geometry::Marker(p) | Geometry::Label(p) => vec![*p],
            Geometry::Line(v) | Geometry::Polygon(v) => v.clone(),
            Geometry::RangeRing { center, .. } => vec![*center],
        }
    }

    pub fn set_vertex(&mut self, i: usize, p: LngLat) {
        match self {
            Geometry::Marker(q) | Geometry::Label(q) => *q = p,
            Geometry::Line(v) | Geometry::Polygon(v) => {
                if let Some(q) = v.get_mut(i) {
                    *q = p;
                }
            }
            Geometry::RangeRing { center, .. } => *center = p,
        }
    }

    /// Remove vertex `i` if the shape stays valid (line >= 2, polygon >= 3).
    pub fn remove_vertex(&mut self, i: usize) -> bool {
        match self {
            Geometry::Line(v) if v.len() > 2 && i < v.len() => {
                v.remove(i);
                true
            }
            Geometry::Polygon(v) if v.len() > 3 && i < v.len() => {
                v.remove(i);
                true
            }
            _ => false,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Geometry::Marker(_) => "Marker",
            Geometry::Line(_) => "Line",
            Geometry::Polygon(_) => "Polygon",
            Geometry::RangeRing { .. } => "Range rings",
            Geometry::Label(_) => "Label",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub color: [u8; 3],
    pub width: f32,
    /// Polygon fill opacity, 0-255.
    pub fill_alpha: u8,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: [220, 38, 38],
            width: 3.0,
            fill_alpha: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    pub id: Id,
    pub name: String,
    pub geometry: Geometry,
    pub style: Style,
    pub folder: Option<Id>,
    pub visible: bool,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: Id,
    pub name: String,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub center: LngLat,
    pub zoom: f64,
    pub angle: f64,
}

pub const FORMAT_VERSION: u32 = 1;

/// A saved map: features, folders, and the view/basemap to restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub format_version: u32,
    pub name: String,
    pub features: Vec<Feature>,
    pub folders: Vec<Folder>,
    pub next_id: Id,
    pub view: Option<SavedView>,
    pub basemap: Option<String>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            name: "Untitled map".into(),
            features: vec![],
            folders: vec![],
            next_id: 1,
            view: None,
            basemap: None,
        }
    }
}

pub fn default_radii() -> Vec<f64> {
    vec![1000.0, 2000.0, 5000.0]
}

impl Project {
    pub fn alloc_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn feature(&self, id: Id) -> Option<&Feature> {
        self.features.iter().find(|f| f.id == id)
    }

    pub fn index_of(&self, id: Id) -> Option<usize> {
        self.features.iter().position(|f| f.id == id)
    }

    pub fn add_folder(&mut self, name: &str) -> Id {
        let id = self.alloc_id();
        self.folders.push(Folder {
            id,
            name: name.into(),
            visible: true,
        });
        id
    }

    /// Visible when the feature and its folder (if any) are both visible.
    pub fn is_visible(&self, f: &Feature) -> bool {
        f.visible
            && f.folder.is_none_or(|fid| {
                self.folders
                    .iter()
                    .find(|x| x.id == fid)
                    .is_none_or(|x| x.visible)
            })
    }

    /// (min_lng, min_lat, max_lng, max_lat) of all vertices, if any.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        let mut b: Option<[f64; 4]> = None;
        for f in &self.features {
            for p in f.geometry.vertices() {
                let e = b.get_or_insert([p.lng, p.lat, p.lng, p.lat]);
                e[0] = e[0].min(p.lng);
                e[1] = e[1].min(p.lat);
                e[2] = e[2].max(p.lng);
                e[3] = e[3].max(p.lat);
            }
        }
        b
    }
}
