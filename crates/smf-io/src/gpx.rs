//! GPX 1.0/1.1 import and export (waypoints, tracks, routes).

use crate::common::*;
use smf_features::{Geometry, Project, Style};
use smf_geo::LngLat;

fn child_text<'a>(n: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| c.has_tag_name(name))
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn point(n: roxmltree::Node) -> Option<LngLat> {
    let lat: f64 = n.attribute("lat")?.trim().parse().ok()?;
    let lng: f64 = n.attribute("lon")?.trim().parse().ok()?;
    valid_lnglat(lng, lat).then(|| LngLat::new(lng, lat))
}

/// Garmin's `gpxx:DisplayColor` names.
fn garmin_color(name: &str) -> Option<[u8; 3]> {
    Some(match name.to_ascii_lowercase().as_str() {
        "black" => [0, 0, 0],
        "darkred" => [139, 0, 0],
        "darkgreen" => [0, 100, 0],
        "darkyellow" => [153, 153, 0],
        "darkblue" => [0, 0, 139],
        "darkmagenta" => [139, 0, 139],
        "darkcyan" => [0, 139, 139],
        "lightgray" => [211, 211, 211],
        "darkgray" => [105, 105, 105],
        "red" => [255, 0, 0],
        "green" => [0, 200, 0],
        "yellow" => [255, 255, 0],
        "blue" => [0, 0, 255],
        "magenta" => [255, 0, 255],
        "cyan" => [0, 255, 255],
        "white" => [255, 255, 255],
        _ => return None,
    })
}

fn track_style(n: roxmltree::Node) -> Style {
    let mut s = Style::default();
    if let Some(c) = n
        .descendants()
        .find(|d| d.has_tag_name("DisplayColor"))
        .and_then(|d| d.text())
        .and_then(garmin_color)
    {
        s.color = c;
    }
    s
}

pub fn read_gpx(text: &str) -> Result<Imported, roxmltree::Error> {
    let doc = roxmltree::Document::parse(text)?;
    let mut imp = Imported::default();
    let root = doc.root_element();
    if root.tag_name().name() != "gpx" {
        imp.warn("File is XML but not GPX (no <gpx> root element).");
        return Ok(imp);
    }
    for n in root.children().filter(|c| c.is_element()) {
        match n.tag_name().name() {
            "wpt" => match point(n) {
                Some(p) => {
                    let notes = child_text(n, "desc")
                        .or_else(|| child_text(n, "cmt"))
                        .unwrap_or("");
                    imp.push(
                        None,
                        child_text(n, "name").unwrap_or(""),
                        Geometry::Marker(p),
                        Style::default(),
                        notes,
                    );
                }
                None => imp.warn("Skipped a waypoint with missing or invalid coordinates."),
            },
            "trk" => {
                let name = child_text(n, "name").unwrap_or("Track");
                let style = track_style(n);
                let notes = child_text(n, "desc")
                    .or_else(|| child_text(n, "cmt"))
                    .unwrap_or("");
                let segs: Vec<Vec<LngLat>> = n
                    .children()
                    .filter(|c| c.has_tag_name("trkseg"))
                    .map(|seg| {
                        seg.children()
                            .filter(|c| c.has_tag_name("trkpt"))
                            .filter_map(point)
                            .collect()
                    })
                    .collect();
                let multi = segs.iter().filter(|s| s.len() >= 2).count() > 1;
                for (i, pts) in segs.into_iter().filter(|s| s.len() >= 2).enumerate() {
                    let nm = if multi {
                        format!("{name} ({})", i + 1)
                    } else {
                        name.to_owned()
                    };
                    imp.push(None, &nm, Geometry::Line(pts), style, notes);
                }
            }
            "rte" => {
                let pts: Vec<LngLat> = n
                    .children()
                    .filter(|c| c.has_tag_name("rtept"))
                    .filter_map(point)
                    .collect();
                if pts.len() >= 2 {
                    imp.push(
                        None,
                        child_text(n, "name").unwrap_or("Route"),
                        Geometry::Line(pts),
                        track_style(n),
                        "",
                    );
                }
            }
            _ => {}
        }
    }
    Ok(imp)
}

pub fn write_gpx(project: &Project) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gpx version=\"1.1\" creator=\"ShowMeFire Field\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
    );
    let trk = |out: &mut String, name: &str, notes: &str, pts: &[LngLat]| {
        out.push_str(&format!("  <trk>\n    <name>{}</name>\n", xml_escape(name)));
        if !notes.is_empty() {
            out.push_str(&format!("    <desc>{}</desc>\n", xml_escape(notes)));
        }
        out.push_str("    <trkseg>\n");
        for p in pts {
            out.push_str(&format!(
                "      <trkpt lat=\"{}\" lon=\"{}\"/>\n",
                p.lat, p.lng
            ));
        }
        out.push_str("    </trkseg>\n  </trk>\n");
    };
    for f in &project.features {
        match &f.geometry {
            Geometry::Marker(p) | Geometry::Label(p) => {
                out.push_str(&format!(
                    "  <wpt lat=\"{}\" lon=\"{}\">\n    <name>{}</name>\n",
                    p.lat,
                    p.lng,
                    xml_escape(&f.name)
                ));
                if !f.notes.is_empty() {
                    out.push_str(&format!("    <desc>{}</desc>\n", xml_escape(&f.notes)));
                }
                out.push_str("  </wpt>\n");
            }
            Geometry::Line(v) => trk(&mut out, &f.name, &f.notes, v),
            Geometry::Polygon(v) => {
                let mut ring = v.clone();
                ring.push(v[0]);
                trk(&mut out, &f.name, &f.notes, &ring);
                warnings.push(format!(
                    "\"{}\": GPX has no polygons, so it was written as a closed track.",
                    f.name
                ));
            }
            Geometry::RangeRing { center, radii_m } => {
                for (r, ring) in radii_m
                    .iter()
                    .zip(crate::geojson::ring_outlines(*center, radii_m))
                {
                    trk(
                        &mut out,
                        &format!("{} {}", f.name, smf_geo::format_distance(*r)),
                        &f.notes,
                        &ring,
                    );
                }
                warnings.push(format!(
                    "\"{}\": range rings were written as circle tracks.",
                    f.name
                ));
            }
        }
    }
    out.push_str("</gpx>\n");
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::Editor;

    const GARMIN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="Garmin Desktop App" xmlns="http://www.topografix.com/GPX/1/1"
     xmlns:gpxx="http://www.garmin.com/xmlschemas/GpxExtensions/v3">
  <wpt lat="38.6" lon="-92.3"><ele>210.5</ele><name>Water &amp; Fuel</name><cmt>Gate code 1234</cmt></wpt>
  <wpt lat="95" lon="10"><name>Bad</name></wpt>
  <rte><name>Route 1</name><rtept lat="38.0" lon="-92.0"/><rtept lat="38.1" lon="-91.9"/></rte>
  <trk><name>Hike</name>
    <extensions><gpxx:TrackExtension><gpxx:DisplayColor>Red</gpxx:DisplayColor></gpxx:TrackExtension></extensions>
    <trkseg><trkpt lat="38.0" lon="-92.0"><ele>1</ele><time>2024-01-01T00:00:00Z</time></trkpt><trkpt lat="38.01" lon="-92.0"/></trkseg>
    <trkseg><trkpt lat="38.5" lon="-92.0"/><trkpt lat="38.51" lon="-92.0"/><trkpt lat="38.52" lon="-92.0"/></trkseg>
  </trk>
</gpx>"#;

    #[test]
    fn imports_garmin_file() {
        let imp = read_gpx(GARMIN).unwrap();
        let names: Vec<_> = imp
            .features
            .iter()
            .map(|f| f.feature.name.as_str())
            .collect();
        assert_eq!(names, ["Water & Fuel", "Route 1", "Hike (1)", "Hike (2)"]);
        assert_eq!(imp.features[0].feature.notes, "Gate code 1234");
        assert_eq!(
            imp.features[2].feature.style.color,
            [255, 0, 0],
            "Garmin DisplayColor"
        );
        assert!(
            imp.warnings
                .iter()
                .any(|w| w.contains("invalid coordinates")),
            "bad waypoint reported"
        );
        match &imp.features[3].feature.geometry {
            Geometry::Line(v) => assert_eq!(v.len(), 3),
            g => panic!("{g:?}"),
        }
    }

    #[test]
    fn export_reimports_with_markers_and_lines_intact() {
        let mut e = Editor::default();
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("A <b> & c".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.1)]),
            Some("L".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(0.0, 0.0),
                LngLat::new(1.0, 0.0),
                LngLat::new(1.0, 1.0),
            ]),
            Some("P".into()),
        );
        let (text, warnings) = write_gpx(&e.project);
        assert_eq!(warnings.len(), 1);
        let imp = read_gpx(&text).unwrap();
        assert_eq!(imp.features.len(), 3);
        assert_eq!(imp.features[0].feature.name, "A <b> & c");
        assert_eq!(
            imp.features[0].feature.geometry,
            e.project.features[0].geometry
        );
        assert_eq!(
            imp.features[1].feature.geometry,
            e.project.features[1].geometry
        );
        match &imp.features[2].feature.geometry {
            Geometry::Line(v) => assert_eq!(
                (v.len(), v.first(), v.last()),
                (4, v.first(), v.first()),
                "closed track"
            ),
            g => panic!("{g:?}"),
        }
    }

    #[test]
    fn rejects_non_gpx_and_malformed() {
        assert!(read_gpx("<gpx><wpt").is_err());
        let imp = read_gpx("<kml/>").unwrap();
        assert!(imp.features.is_empty() && !imp.warnings.is_empty());
    }
}
