//! Types and helpers shared by all import/export formats.

use smf_features::{Editor, Feature, Geometry, Id, Style};
use smf_geo::LngLat;

/// Features read from a file, before they are added to a project.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Imported {
    pub folders: Vec<String>,
    pub features: Vec<ImportedFeature>,
    /// Things the file contained that we could not represent; shown to the user.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedFeature {
    /// Index into `Imported::folders`.
    pub folder: Option<usize>,
    pub feature: Feature,
}

impl Imported {
    pub fn folder_index(&mut self, name: &str) -> usize {
        if let Some(i) = self.folders.iter().position(|f| f == name) {
            return i;
        }
        self.folders.push(name.to_owned());
        self.folders.len() - 1
    }

    pub fn push(
        &mut self,
        folder: Option<usize>,
        name: &str,
        geometry: Geometry,
        style: Style,
        notes: &str,
    ) {
        self.features.push(ImportedFeature {
            folder,
            feature: Feature {
                id: 0,
                name: name.to_owned(),
                geometry,
                style,
                folder: None,
                visible: true,
                notes: notes.to_owned(),
            },
        });
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }
}

/// Add imported content to the editor as one undo step. Returns the new feature ids.
pub fn apply_import(editor: &mut Editor, imp: Imported) -> Vec<Id> {
    let folder_ids: Vec<Id> = imp
        .folders
        .iter()
        .map(|name| {
            // Reuse an existing folder of the same name so re-importing doesn't duplicate folders.
            match editor.project.folders.iter().find(|f| &f.name == name) {
                Some(f) => f.id,
                None => editor.project.add_folder(name),
            }
        })
        .collect();
    let features = imp
        .features
        .into_iter()
        .map(|mut f| {
            f.feature.folder = f.folder.and_then(|i| folder_ids.get(i).copied());
            f.feature
        })
        .collect();
    editor.add_features(features)
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters; drop them rather than write a broken file.
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// "#rrggbb" or "rrggbb" or "#rgb".
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    match h.len() {
        6 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
        }
        3 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some([(r * 17) as u8, (g * 17) as u8, (b * 17) as u8])
        }
        _ => None,
    }
}

pub fn hex_color(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Drop a duplicated closing vertex (KML/GeoJSON rings repeat the first point); our polygons are open rings.
pub fn open_ring(mut ring: Vec<LngLat>) -> Vec<LngLat> {
    if ring.len() > 1 && ring.first() == ring.last() {
        ring.pop();
    }
    ring
}

pub fn valid_lnglat(lng: f64, lat: f64) -> bool {
    lng.is_finite()
        && lat.is_finite()
        && (-180.0..=180.0).contains(&lng)
        && (-90.0..=90.0).contains(&lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_roundtrip() {
        assert_eq!(parse_hex_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex_color("f80"), Some([255, 136, 0]));
        assert_eq!(parse_hex_color("nope"), None);
        assert_eq!(hex_color([1, 2, 255]), "#0102ff");
    }

    #[test]
    fn escapes_xml_and_strips_control_chars() {
        assert_eq!(xml_escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
        assert_eq!(xml_escape("x\u{1}y\n"), "xy\n");
    }

    #[test]
    fn open_ring_drops_only_the_duplicate_close() {
        let a = LngLat::new(0.0, 0.0);
        let b = LngLat::new(1.0, 0.0);
        let c = LngLat::new(1.0, 1.0);
        assert_eq!(open_ring(vec![a, b, c, a]), vec![a, b, c]);
        assert_eq!(open_ring(vec![a, b, c]), vec![a, b, c]);
    }

    #[test]
    fn apply_import_reuses_folders_and_is_one_undo() {
        let mut e = Editor::default();
        let mk = || {
            let mut imp = Imported::default();
            let f = imp.folder_index("Ops");
            imp.push(
                Some(f),
                "a",
                Geometry::Marker(LngLat::new(0.0, 0.0)),
                Style::default(),
                "",
            );
            imp.push(
                None,
                "b",
                Geometry::Marker(LngLat::new(1.0, 1.0)),
                Style::default(),
                "",
            );
            imp
        };
        let ids = apply_import(&mut e, mk());
        assert_eq!(ids.len(), 2);
        assert_eq!(e.project.folders.len(), 1);
        assert_eq!(
            e.project.feature(ids[0]).unwrap().folder,
            Some(e.project.folders[0].id)
        );
        assert_eq!(e.project.feature(ids[1]).unwrap().folder, None);
        apply_import(&mut e, mk());
        assert_eq!(e.project.folders.len(), 1, "same-named folder is reused");
        e.undo();
        assert_eq!(
            e.project.features.len(),
            2,
            "second import undone as one step"
        );
    }
}
