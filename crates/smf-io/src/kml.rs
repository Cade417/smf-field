//! KML and KMZ import/export, including folders and basic styles. KML colours are `aabbggrr`.

use crate::common::*;
use smf_features::{Geometry, Project, Style};
use smf_geo::LngLat;
use std::collections::HashMap;
use std::io::{Read, Seek, Write};

/// A KMZ entry larger than this is refused (zip bombs).
const MAX_KML_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Default, Clone)]
struct KmlStyle {
    line: Option<([u8; 3], u8)>,
    width: Option<f32>,
    fill: Option<([u8; 3], u8)>,
    icon: Option<[u8; 3]>,
}

fn kml_color(s: &str) -> Option<([u8; 3], u8)> {
    let h = s.trim();
    if h.len() != 8 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(([v as u8, (v >> 8) as u8, (v >> 16) as u8], (v >> 24) as u8))
}

fn to_kml_color(c: [u8; 3], alpha: u8) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}", alpha, c[2], c[1], c[0])
}

fn text<'a>(n: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| c.has_tag_name(name))
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn parse_style(n: roxmltree::Node) -> KmlStyle {
    let mut st = KmlStyle::default();
    for c in n.children().filter(|c| c.is_element()) {
        match c.tag_name().name() {
            "LineStyle" => {
                st.line = text(c, "color").and_then(kml_color);
                st.width = text(c, "width").and_then(|w| w.parse().ok());
            }
            "PolyStyle" => {
                st.fill = text(c, "color")
                    .and_then(kml_color)
                    .or(Some(([255, 255, 255], 255)));
                if text(c, "fill") == Some("0") {
                    st.fill = st.fill.map(|(c, _)| (c, 0));
                }
            }
            "IconStyle" => st.icon = text(c, "color").and_then(kml_color).map(|(c, _)| c),
            _ => {}
        }
    }
    st
}

fn coords(s: &str) -> Vec<LngLat> {
    s.split_whitespace()
        .filter_map(|t| {
            let mut it = t.split(',');
            let lng: f64 = it.next()?.parse().ok()?;
            let lat: f64 = it.next()?.parse().ok()?;
            valid_lnglat(lng, lat).then(|| LngLat::new(lng, lat))
        })
        .collect()
}

struct Reader<'a> {
    imp: Imported,
    styles: HashMap<String, KmlStyle>,
    style_maps: HashMap<String, String>,
    _doc: std::marker::PhantomData<&'a ()>,
}

impl Reader<'_> {
    fn resolve(&self, url: &str) -> Option<&KmlStyle> {
        let id = url.trim().trim_start_matches('#');
        let id = self
            .style_maps
            .get(id)
            .map_or(id, |s| s.trim_start_matches('#'));
        self.styles.get(id)
    }

    fn style_for(&self, pm: roxmltree::Node, is_point: bool, is_poly: bool) -> Style {
        let inline = pm
            .children()
            .find(|c| c.has_tag_name("Style"))
            .map(parse_style);
        let ks = inline
            .or_else(|| text(pm, "styleUrl").and_then(|u| self.resolve(u)).cloned())
            .unwrap_or_default();
        let mut s = Style::default();
        let main = if is_point {
            ks.icon.map(|c| (c, 255)).or(ks.line)
        } else {
            ks.line.or(ks.fill)
        };
        if let Some((c, _)) = main {
            s.color = c;
        }
        if let Some(w) = ks.width {
            s.width = w.clamp(0.5, 12.0);
        }
        if is_poly && let Some((c, a)) = ks.fill {
            s.fill_alpha = a;
            if ks.line.is_none() {
                s.color = c;
            }
        }
        s
    }

    fn walk(&mut self, node: roxmltree::Node, folder: Option<usize>, path: &str) {
        for c in node.children().filter(|c| c.is_element()) {
            match c.tag_name().name() {
                "Folder" | "Document" => {
                    let name = text(c, "name").unwrap_or("");
                    let (sub, new_path) = if c.tag_name().name() == "Document" || name.is_empty() {
                        (folder, path.to_owned())
                    } else {
                        let full = if path.is_empty() {
                            name.to_owned()
                        } else {
                            format!("{path} / {name}")
                        };
                        (Some(self.imp.folder_index(&full)), full)
                    };
                    self.walk(c, sub, &new_path);
                }
                "Placemark" => self.placemark(c, folder),
                "NetworkLink" => self
                    .imp
                    .warn("Network links were not followed (offline import)."),
                "GroundOverlay" | "ScreenOverlay" | "PhotoOverlay" => self
                    .imp
                    .warn("Image overlays are not supported and were skipped."),
                _ => {}
            }
        }
    }

    fn placemark(&mut self, pm: roxmltree::Node, folder: Option<usize>) {
        let name = text(pm, "name").unwrap_or("").to_owned();
        let notes = text(pm, "description").unwrap_or("").to_owned();
        let mut geoms: Vec<roxmltree::Node> = Vec::new();
        collect_geometries(pm, &mut geoms);
        if geoms.is_empty() {
            if pm.descendants().any(|d| d.tag_name().name() == "Track") {
                self.imp
                    .warn("gx:Track (timed tracks) is not supported and was skipped.");
            }
            return;
        }
        for g in geoms {
            let cs = g
                .descendants()
                .find(|d| d.has_tag_name("coordinates"))
                .and_then(|d| d.text())
                .map(coords)
                .unwrap_or_default();
            match g.tag_name().name() {
                "Point" => {
                    if let Some(p) = cs.first() {
                        let st = self.style_for(pm, true, false);
                        self.imp
                            .push(folder, &name, Geometry::Marker(*p), st, &notes);
                    }
                }
                "LineString" if cs.len() >= 2 => {
                    let st = self.style_for(pm, false, false);
                    self.imp.push(folder, &name, Geometry::Line(cs), st, &notes);
                }
                "Polygon" | "LinearRing" => {
                    // Outer boundary only: the first coordinates element is the outer ring.
                    let ring = open_ring(cs);
                    if ring.len() >= 3 {
                        if g.descendants()
                            .filter(|d| d.has_tag_name("innerBoundaryIs"))
                            .count()
                            > 0
                        {
                            self.imp.warn(
                                "Polygon holes were dropped (only outer rings are supported).",
                            );
                        }
                        let st = self.style_for(pm, false, true);
                        self.imp
                            .push(folder, &name, Geometry::Polygon(ring), st, &notes);
                    }
                }
                _ => {}
            }
        }
    }
}

fn collect_geometries<'a, 'b>(n: roxmltree::Node<'a, 'b>, out: &mut Vec<roxmltree::Node<'a, 'b>>) {
    for c in n.children().filter(|c| c.is_element()) {
        match c.tag_name().name() {
            "Point" | "LineString" | "Polygon" | "LinearRing" => out.push(c),
            "MultiGeometry" => collect_geometries(c, out),
            _ => {}
        }
    }
}

pub fn read_kml(text_in: &str) -> Result<Imported, roxmltree::Error> {
    let doc = roxmltree::Document::parse(text_in)?;
    let mut r = Reader {
        imp: Imported::default(),
        styles: HashMap::new(),
        style_maps: HashMap::new(),
        _doc: Default::default(),
    };
    for n in doc.descendants() {
        match n.tag_name().name() {
            "Style" => {
                if let Some(id) = n.attribute("id") {
                    r.styles.insert(id.to_owned(), parse_style(n));
                }
            }
            "StyleMap" => {
                if let Some(id) = n.attribute("id") {
                    let normal = n
                        .children()
                        .filter(|c| c.has_tag_name("Pair"))
                        .find(|p| text(*p, "key") == Some("normal"))
                        .and_then(|p| text(p, "styleUrl"));
                    if let Some(u) = normal {
                        r.style_maps.insert(id.to_owned(), u.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    let root = doc.root_element();
    if root.tag_name().name() != "kml" {
        r.imp
            .warn("File is XML but not KML (no <kml> root element).");
        return Ok(r.imp);
    }
    r.walk(root, None, "");
    Ok(r.imp)
}

pub fn read_kmz<R: Read + Seek>(rd: R) -> Result<Imported, crate::IoError> {
    let mut zip = zip::ZipArchive::new(rd)?;
    let name = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_owned()))
        .filter(|n| n.to_ascii_lowercase().ends_with(".kml"))
        .min_by_key(|n| (!n.eq_ignore_ascii_case("doc.kml"), n.len()))
        .ok_or_else(|| crate::IoError::Format("KMZ contains no .kml file".into()))?;
    let mut s = String::new();
    zip.by_name(&name)?
        .take(MAX_KML_BYTES)
        .read_to_string(&mut s)?;
    read_kml(&s).map_err(|e| crate::IoError::Format(format!("KML inside KMZ is invalid: {e}")))
}

pub fn write_kml(project: &Project) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n<Document>\n",
    );
    out.push_str(&format!("<name>{}</name>\n", xml_escape(&project.name)));
    for f in &project.features {
        let alpha = f.style.fill_alpha;
        out.push_str(&format!(
            "<Style id=\"s{}\"><IconStyle><color>{}</color></IconStyle><LineStyle><color>{}</color><width>{}</width></LineStyle><PolyStyle><color>{}</color></PolyStyle></Style>\n",
            f.id,
            to_kml_color(f.style.color, 255),
            to_kml_color(f.style.color, 255),
            f.style.width,
            to_kml_color(f.style.color, alpha),
        ));
    }
    let placemark = |out: &mut String, f: &smf_features::Feature| {
        out.push_str(&format!("<Placemark><name>{}</name>", xml_escape(&f.name)));
        if !f.notes.is_empty() {
            out.push_str(&format!(
                "<description>{}</description>",
                xml_escape(&f.notes)
            ));
        }
        out.push_str(&format!("<styleUrl>#s{}</styleUrl>", f.id));
        let c = |p: &LngLat| format!("{},{},0", p.lng, p.lat);
        let line = |v: &[LngLat]| v.iter().map(c).collect::<Vec<_>>().join(" ");
        match &f.geometry {
            Geometry::Marker(p) | Geometry::Label(p) => out.push_str(&format!(
                "<Point><coordinates>{}</coordinates></Point>",
                c(p)
            )),
            Geometry::Line(v) => out.push_str(&format!(
                "<LineString><tessellate>1</tessellate><coordinates>{}</coordinates></LineString>",
                line(v)
            )),
            Geometry::Polygon(v) => {
                let mut ring = v.clone();
                ring.push(v[0]);
                out.push_str(&format!(
                    "<Polygon><tessellate>1</tessellate><outerBoundaryIs><LinearRing><coordinates>{}</coordinates></LinearRing></outerBoundaryIs></Polygon>",
                    line(&ring)
                ));
            }
            Geometry::RangeRing { center, radii_m } => {
                out.push_str("<MultiGeometry>");
                for ring in crate::geojson::ring_outlines(*center, radii_m) {
                    out.push_str(&format!(
                        "<LineString><coordinates>{}</coordinates></LineString>",
                        line(&ring)
                    ));
                }
                out.push_str("</MultiGeometry>");
            }
        }
        out.push_str("</Placemark>\n");
    };
    for fo in &project.folders {
        out.push_str(&format!("<Folder><name>{}</name>\n", xml_escape(&fo.name)));
        for f in project.features.iter().filter(|f| f.folder == Some(fo.id)) {
            placemark(&mut out, f);
        }
        out.push_str("</Folder>\n");
    }
    for f in project.features.iter().filter(|f| {
        f.folder
            .is_none_or(|id| project.folders.iter().all(|x| x.id != id))
    }) {
        placemark(&mut out, f);
    }
    out.push_str("</Document>\n</kml>\n");
    out
}

pub fn write_kmz<W: Write + Seek>(project: &Project, w: W) -> Result<(), crate::IoError> {
    let mut zip = zip::ZipWriter::new(w);
    zip.start_file("doc.kml", zip::write::SimpleFileOptions::default())?;
    zip.write_all(write_kml(project).as_bytes())?;
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::Editor;
    use std::io::Cursor;

    const GOOGLE_EARTH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2">
<Document>
  <name>Test</name>
  <Style id="redline"><LineStyle><color>ff0000ff</color><width>5</width></LineStyle></Style>
  <StyleMap id="m"><Pair><key>normal</key><styleUrl>#redline</styleUrl></Pair><Pair><key>highlight</key><styleUrl>#other</styleUrl></Pair></StyleMap>
  <Style id="poly"><LineStyle><color>ffff0000</color></LineStyle><PolyStyle><color>8000ff00</color></PolyStyle></Style>
  <Folder><name>Division A</name>
    <Folder><name>Crew 1</name>
      <Placemark><name>Fire line</name><description>Hand line</description><styleUrl>#m</styleUrl>
        <LineString><coordinates>-92.0,38.0,0 -91.9,38.1,0</coordinates></LineString></Placemark>
    </Folder>
    <Placemark><name>Unit</name><styleUrl>#poly</styleUrl>
      <Polygon><outerBoundaryIs><LinearRing><coordinates>-92,38,0 -91.9,38,0 -91.9,38.1,0 -92,38,0</coordinates></LinearRing></outerBoundaryIs>
        <innerBoundaryIs><LinearRing><coordinates>-91.97,38.03 -91.95,38.03 -91.95,38.05 -91.97,38.03</coordinates></LinearRing></innerBoundaryIs></Polygon></Placemark>
  </Folder>
  <Placemark><name>Pin</name><Point><coordinates>-92.3,38.6,0</coordinates></Point></Placemark>
  <Placemark><name>Multi</name><MultiGeometry><Point><coordinates>1,2</coordinates></Point><LineString><coordinates>0,0 1,1</coordinates></LineString></MultiGeometry></Placemark>
  <Placemark><name>Timed</name><gx:Track><when>2024</when></gx:Track></Placemark>
  <NetworkLink><name>Live</name><Link><href>http://example.com/x.kml</href></Link></NetworkLink>
</Document></kml>"#;

    #[test]
    fn imports_google_earth_file_with_nested_folders_and_styles() {
        let imp = read_kml(GOOGLE_EARTH).unwrap();
        assert_eq!(imp.folders, vec!["Division A", "Division A / Crew 1"]);
        let by = |n: &str| imp.features.iter().find(|f| f.feature.name == n).unwrap();
        let line = by("Fire line");
        assert_eq!(line.folder, Some(1));
        assert_eq!(
            line.feature.style.color,
            [255, 0, 0],
            "aabbggrr ff0000ff is red"
        );
        assert_eq!(line.feature.style.width, 5.0, "via StyleMap normal pair");
        assert_eq!(line.feature.notes, "Hand line");
        let unit = by("Unit");
        assert_eq!(unit.folder, Some(0));
        assert_eq!(
            unit.feature.style.color,
            [0, 0, 255],
            "outline colour ffff0000 is blue"
        );
        assert_eq!(unit.feature.style.fill_alpha, 0x80);
        match &unit.feature.geometry {
            Geometry::Polygon(r) => assert_eq!(r.len(), 3),
            g => panic!("{g:?}"),
        }
        assert_eq!(by("Pin").folder, None);
        assert_eq!(
            imp.features
                .iter()
                .filter(|f| f.feature.name == "Multi")
                .count(),
            2,
            "multigeometry split"
        );
        assert!(imp.features.iter().all(|f| f.feature.name != "Timed"));
        assert!(imp.warnings.iter().any(|w| w.contains("holes")));
        assert!(imp.warnings.iter().any(|w| w.contains("gx:Track")));
        assert!(imp.warnings.iter().any(|w| w.contains("Network")));
    }

    fn sample() -> Editor {
        let mut e = Editor::default();
        let fid = e.project.add_folder("Ops & <Plans>");
        e.current_style.color = [10, 200, 30];
        e.current_style.fill_alpha = 100;
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("Pin \"1\"".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.1)]),
            Some("L".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-92.0, 38.0),
                LngLat::new(-91.9, 38.0),
                LngLat::new(-91.9, 38.1),
            ]),
            Some("P".into()),
        );
        let mut f = e.project.features[1].clone();
        f.folder = Some(fid);
        f.notes = "a & b".into();
        e.replace(f);
        e
    }

    #[test]
    fn kml_roundtrip_preserves_geometry_style_folders_and_escaping() {
        let e = sample();
        let imp = read_kml(&write_kml(&e.project)).unwrap();
        assert_eq!(imp.folders, vec!["Ops & <Plans>"]);
        assert_eq!(imp.features.len(), 3);
        let by = |n: &str| imp.features.iter().find(|f| f.feature.name == n).unwrap();
        assert_eq!(
            by("Pin \"1\"").feature.geometry,
            e.project.features[0].geometry
        );
        assert_eq!(by("L").folder, Some(0));
        assert_eq!(by("L").feature.notes, "a & b");
        assert_eq!(by("L").feature.geometry, e.project.features[1].geometry);
        assert_eq!(by("L").feature.style.color, [10, 200, 30]);
        assert_eq!(by("L").feature.style.width, 3.0);
        assert_eq!(by("P").feature.geometry, e.project.features[2].geometry);
        assert_eq!(by("P").feature.style.fill_alpha, 100);
    }

    #[test]
    fn kmz_roundtrip_and_rejects_empty_archive() {
        let e = sample();
        let mut buf = Cursor::new(Vec::new());
        write_kmz(&e.project, &mut buf).unwrap();
        buf.set_position(0);
        assert_eq!(read_kmz(buf).unwrap().features.len(), 3);
        let mut empty = Cursor::new(Vec::new());
        zip::ZipWriter::new(&mut empty).finish().unwrap();
        empty.set_position(0);
        assert!(read_kmz(empty).is_err());
    }

    #[test]
    fn dtd_and_malformed_xml_are_rejected_not_expanded() {
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE kml [<!ENTITY a "aaaa"><!ENTITY b "&a;&a;&a;&a;">]><kml>&b;</kml>"#;
        assert!(read_kml(bomb).is_err(), "DTDs are not allowed");
        assert!(read_kml("<kml><Placemark>").is_err());
        let imp = read_kml("<gpx/>").unwrap();
        assert!(!imp.warnings.is_empty());
    }

    #[test]
    fn color_conversion_is_symmetric() {
        assert_eq!(kml_color("ff0000ff"), Some(([255, 0, 0], 255)));
        assert_eq!(to_kml_color([255, 0, 0], 255), "ff0000ff");
        assert_eq!(
            kml_color(&to_kml_color([1, 2, 3], 77)),
            Some(([1, 2, 3], 77))
        );
        assert_eq!(kml_color("zz"), None);
    }
}
