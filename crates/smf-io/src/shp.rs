//! ESRI Shapefile import/export.
//!
//! Import understands the `.prj` for the cases field data actually arrives in:
//! geographic (WGS84/NAD83, treated as equal: they differ by about a metre), UTM zones
//! in metres, and Web Mercator. Anything else is refused with a clear message rather
//! than being placed in the wrong spot.

use crate::IoError;
use crate::common::*;
use shapefile::dbase::{self, FieldValue, TableWriterBuilder};
use shapefile::{Point, Polygon, PolygonRing, Polyline, Shape};
use smf_features::{Geometry, Project, Style};
use smf_geo::{LngLat, mercator_to_lnglat};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Crs {
    Geographic,
    Utm { zone: u8, north: bool },
    WebMercator,
}

const WGS84_PRJ: &str = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;

fn parse_prj(wkt: &str) -> Result<Crs, String> {
    let lower = wkt.to_ascii_lowercase();
    let head = lower.trim_start();
    if lower.contains("foot") || lower.contains("feet") || lower.contains("us_survey") {
        return Err(
            "projection uses feet (e.g. State Plane); reproject to WGS84 or UTM first".into(),
        );
    }
    if head.starts_with("geogcs")
        || head.starts_with("geodcrs")
        || head.starts_with("geographiccrs")
    {
        return Ok(Crs::Geographic);
    }
    if lower.contains("web_mercator") || lower.contains("pseudo-mercator") || lower.contains("3857")
    {
        return Ok(Crs::WebMercator);
    }
    if let Some(i) = lower.find("utm") {
        // "utm_zone_15n", "utm zone 15n", "utm_zone_15" (+ hemisphere elsewhere)
        let rest = &lower[i..];
        if let Some(z) = rest.find("zone") {
            let tail = rest[z + 4..].trim_start_matches(['_', ' ']);
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(zone) = digits.parse::<u8>()
                && (1..=60).contains(&zone)
            {
                let hemi = tail[digits.len()..].chars().next();
                let north = match hemi {
                    Some('s') => false,
                    Some('n') => true,
                    // Southern-hemisphere UTM carries a false northing of 10,000,000.
                    _ => !lower.contains("false_northing\",10000000"),
                };
                return Ok(Crs::Utm { zone, north });
            }
        }
    }
    let name = wkt.split('"').nth(1).unwrap_or("unknown");
    Err(format!(
        "unsupported coordinate system \"{name}\"; reproject the file to WGS84 (EPSG:4326) or UTM"
    ))
}

fn to_lnglat(crs: Crs, x: f64, y: f64) -> Option<LngLat> {
    let p = match crs {
        Crs::Geographic => LngLat::new(x, y),
        Crs::WebMercator => mercator_to_lnglat(x, y),
        Crs::Utm { zone, north } => {
            let (lat, lng) =
                utm::wsg84_utm_to_lat_lon(x, y, zone, if north { 'N' } else { 'M' }).ok()?;
            LngLat::new(lng, lat)
        }
    };
    valid_lnglat(p.lng, p.lat).then_some(p)
}

/// Human-readable attribute value (dbase's own Display prints Rust debug text).
fn fmt_value(v: &FieldValue) -> Option<String> {
    let s = match v {
        FieldValue::Character(Some(s)) => s.trim().to_owned(),
        FieldValue::Numeric(Some(n)) => fmt_num(*n),
        FieldValue::Float(Some(n)) => fmt_num(*n as f64),
        FieldValue::Integer(n) => n.to_string(),
        FieldValue::Double(n) => fmt_num(*n),
        FieldValue::Logical(Some(b)) => b.to_string(),
        FieldValue::Date(Some(d)) => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n}")
    }
}

fn name_and_notes(rec: &dbase::Record) -> (String, String) {
    let mut fields: Vec<(String, String)> = rec
        .clone()
        .into_iter()
        .filter_map(|(k, v)| fmt_value(&v).map(|s| (k, s)))
        .collect();
    fields.sort_by_key(|f| f.0.to_ascii_lowercase());
    let pref = [
        "name",
        "title",
        "label",
        "fire_name",
        "incidentna",
        "incident",
        "incident_n",
        "poly_incid",
    ];
    let name_idx = pref
        .iter()
        .find_map(|p| fields.iter().position(|(k, _)| k.eq_ignore_ascii_case(p)))
        .or_else(|| fields.iter().position(|(_, v)| v.parse::<f64>().is_err()));
    let name = name_idx.map(|i| fields.remove(i).1).unwrap_or_default();
    // A dedicated notes column (our own exports write one) is the note text itself.
    let mut notes = fields
        .iter()
        .position(|(k, _)| {
            ["notes", "description", "desc"]
                .iter()
                .any(|n| k.eq_ignore_ascii_case(n))
        })
        .map(|i| fields.remove(i).1)
        .unwrap_or_default();
    // Keep the other attributes visible instead of silently discarding them (acres, dates...).
    for (k, v) in fields.iter().take(14) {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&format!("{k}: {v}"));
    }
    (name, notes)
}

pub fn read_shapefile(path: &Path) -> Result<Imported, IoError> {
    let mut imp = Imported::default();
    let crs = match std::fs::read_to_string(path.with_extension("prj")) {
        Ok(wkt) => parse_prj(&wkt).map_err(IoError::Format)?,
        Err(_) => {
            imp.warn("No .prj file: assuming longitude/latitude (WGS84).");
            Crs::Geographic
        }
    };
    let mut reader = shapefile::Reader::from_path(path)
        .map_err(|e| IoError::Format(format!("shapefile: {e}")))?;
    let mut bad_coords = 0usize;
    for item in reader.iter_shapes_and_records() {
        let (shape, rec) = item.map_err(|e| IoError::Format(format!("shapefile: {e}")))?;
        let (name, notes) = name_and_notes(&rec);
        let conv = |pts: Vec<(f64, f64)>, bad: &mut usize| -> Vec<LngLat> {
            let out: Vec<LngLat> = pts
                .iter()
                .filter_map(|(x, y)| to_lnglat(crs, *x, *y))
                .collect();
            if out.len() != pts.len() {
                *bad += 1;
            }
            out
        };
        macro_rules! xy {
            ($v:expr) => {
                $v.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>()
            };
        }
        let st = Style::default();
        match shape {
            Shape::Point(p) => {
                push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords)
            }
            Shape::PointM(p) => {
                push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords)
            }
            Shape::PointZ(p) => {
                push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords)
            }
            Shape::Multipoint(m) => {
                for p in m.points() {
                    push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords);
                }
            }
            Shape::MultipointM(m) => {
                for p in m.points() {
                    push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords);
                }
            }
            Shape::MultipointZ(m) => {
                for p in m.points() {
                    push_point(&mut imp, crs, p.x, p.y, &name, &notes, st, &mut bad_coords);
                }
            }
            Shape::Polyline(l) => {
                for part in l.parts() {
                    push_line(
                        &mut imp,
                        conv(xy!(part), &mut bad_coords),
                        &name,
                        &notes,
                        st,
                    );
                }
            }
            Shape::PolylineM(l) => {
                for part in l.parts() {
                    push_line(
                        &mut imp,
                        conv(xy!(part), &mut bad_coords),
                        &name,
                        &notes,
                        st,
                    );
                }
            }
            Shape::PolylineZ(l) => {
                for part in l.parts() {
                    push_line(
                        &mut imp,
                        conv(xy!(part), &mut bad_coords),
                        &name,
                        &notes,
                        st,
                    );
                }
            }
            Shape::Polygon(g) => {
                for ring in g.rings() {
                    match ring {
                        PolygonRing::Outer(pts) => {
                            push_poly(&mut imp, conv(xy!(pts), &mut bad_coords), &name, &notes, st)
                        }
                        PolygonRing::Inner(_) => {
                            imp.warn("Polygon holes were dropped (only outer rings are supported).")
                        }
                    }
                }
            }
            Shape::PolygonM(g) => {
                for ring in g.rings() {
                    match ring {
                        PolygonRing::Outer(pts) => {
                            push_poly(&mut imp, conv(xy!(pts), &mut bad_coords), &name, &notes, st)
                        }
                        PolygonRing::Inner(_) => {
                            imp.warn("Polygon holes were dropped (only outer rings are supported).")
                        }
                    }
                }
            }
            Shape::PolygonZ(g) => {
                for ring in g.rings() {
                    match ring {
                        PolygonRing::Outer(pts) => {
                            push_poly(&mut imp, conv(xy!(pts), &mut bad_coords), &name, &notes, st)
                        }
                        PolygonRing::Inner(_) => {
                            imp.warn("Polygon holes were dropped (only outer rings are supported).")
                        }
                    }
                }
            }
            Shape::NullShape => {}
            Shape::Multipatch(_) => imp.warn("Multipatch (3D surface) shapes are not supported."),
        }
    }
    if bad_coords > 0 {
        imp.warn(format!("{bad_coords} shape(s) had coordinates outside the valid range for this projection and were trimmed."));
    }
    Ok(imp)
}

#[allow(clippy::too_many_arguments)]
fn push_point(
    imp: &mut Imported,
    crs: Crs,
    x: f64,
    y: f64,
    name: &str,
    notes: &str,
    st: Style,
    bad: &mut usize,
) {
    match to_lnglat(crs, x, y) {
        Some(p) => imp.push(None, name, Geometry::Marker(p), st, notes),
        None => *bad += 1,
    }
}

fn push_line(imp: &mut Imported, pts: Vec<LngLat>, name: &str, notes: &str, st: Style) {
    if pts.len() >= 2 {
        imp.push(None, name, Geometry::Line(pts), st, notes);
    }
}

fn push_poly(imp: &mut Imported, pts: Vec<LngLat>, name: &str, notes: &str, st: Style) {
    let ring = open_ring(pts);
    if ring.len() >= 3 {
        imp.push(None, name, Geometry::Polygon(ring), st, notes);
    }
}

fn clip(s: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

fn record(name: &str, notes: &str) -> dbase::Record {
    let mut r = dbase::Record::default();
    r.insert("name".into(), FieldValue::Character(Some(clip(name, 80))));
    r.insert(
        "notes".into(),
        FieldValue::Character(Some(clip(notes, 254))),
    );
    r
}

fn table() -> TableWriterBuilder {
    TableWriterBuilder::new()
        .add_character_field("name".try_into().expect("field name"), 80)
        .add_character_field("notes".try_into().expect("field name"), 254)
}

/// Writes up to three shapefile sets (a shapefile holds one geometry type):
/// `<base>_points`, `<base>_lines`, `<base>_polygons`. Returns the `.shp` paths written.
pub fn write_shapefiles(
    project: &Project,
    dir: &Path,
    base: &str,
) -> Result<(Vec<PathBuf>, Vec<String>), IoError> {
    let fe = |e: shapefile::Error| IoError::Format(format!("shapefile: {e}"));
    let mut points: Vec<(Point, dbase::Record)> = vec![];
    let mut lines: Vec<(Polyline, dbase::Record)> = vec![];
    let mut polys: Vec<(Polygon, dbase::Record)> = vec![];
    let mut warnings = Vec::new();
    let pts = |v: &[LngLat]| {
        v.iter()
            .map(|p| Point::new(p.lng, p.lat))
            .collect::<Vec<_>>()
    };
    for f in &project.features {
        let rec = record(&f.name, &f.notes);
        match &f.geometry {
            Geometry::Marker(p) | Geometry::Label(p) => {
                points.push((Point::new(p.lng, p.lat), rec))
            }
            Geometry::Line(v) => lines.push((Polyline::new(pts(v)), rec)),
            Geometry::Polygon(v) => {
                let mut ring = pts(v);
                ring.push(ring[0]);
                polys.push((Polygon::new(PolygonRing::Outer(ring)), rec));
            }
            Geometry::RangeRing { center, radii_m } => {
                for ring in crate::geojson::ring_outlines(*center, radii_m) {
                    lines.push((Polyline::new(pts(&ring)), record(&f.name, &f.notes)));
                }
                warnings.push(format!(
                    "\"{}\": range rings were written as circle lines.",
                    f.name
                ));
            }
        }
    }
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    macro_rules! dump {
        ($items:expr, $suffix:literal) => {
            if !$items.is_empty() {
                let path = dir.join(format!("{base}_{}.shp", $suffix));
                let mut w = shapefile::Writer::from_path(&path, table()).map_err(fe)?;
                for (shape, rec) in &$items {
                    w.write_shape_and_record(shape, rec).map_err(fe)?;
                }
                drop(w);
                std::fs::write(path.with_extension("prj"), WGS84_PRJ)?;
                written.push(path);
            }
        };
    }
    dump!(points, "points");
    dump!(lines, "lines");
    dump!(polys, "polygons");
    Ok((written, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::Editor;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("smf-shp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn prj_parsing_covers_common_cases() {
        assert_eq!(parse_prj(WGS84_PRJ), Ok(Crs::Geographic));
        assert_eq!(
            parse_prj(
                r#"PROJCS["NAD_1983_UTM_Zone_15N",GEOGCS["GCS_North_American_1983"],PROJECTION["Transverse_Mercator"],UNIT["Meter",1.0]]"#
            ),
            Ok(Crs::Utm {
                zone: 15,
                north: true
            })
        );
        assert_eq!(
            parse_prj(r#"PROJCS["WGS 84 / UTM zone 33S",GEOGCS["WGS 84"],UNIT["metre",1]]"#),
            Ok(Crs::Utm {
                zone: 33,
                north: false
            })
        );
        assert_eq!(
            parse_prj(r#"PROJCS["WGS_1984_Web_Mercator_Auxiliary_Sphere",UNIT["Meter",1.0]]"#),
            Ok(Crs::WebMercator)
        );
        let feet = parse_prj(
            r#"PROJCS["NAD_1983_StatePlane_Missouri_Central_FIPS_2402_Feet",UNIT["Foot_US",0.3048006]]"#,
        );
        assert!(feet.unwrap_err().contains("feet"));
        let other = parse_prj(r#"PROJCS["Albers_Conic_Equal_Area",UNIT["Meter",1.0]]"#);
        assert!(other.unwrap_err().contains("unsupported"));
    }

    #[test]
    fn utm_conversion_matches_known_geometry() {
        // Zone 15 is centred on -93 degrees: easting 500000 must be exactly that meridian.
        let p = to_lnglat(
            Crs::Utm {
                zone: 15,
                north: true,
            },
            500_000.0,
            4_266_000.0,
        )
        .unwrap();
        assert!((p.lng + 93.0).abs() < 1e-9, "{}", p.lng);
        assert!((p.lat - 38.52).abs() < 0.05, "{}", p.lat);
        // Inverse of the forward projection.
        let (n, e, _) = utm::to_utm_wgs84(38.6, -92.3, 15);
        let q = to_lnglat(
            Crs::Utm {
                zone: 15,
                north: true,
            },
            e,
            n,
        )
        .unwrap();
        assert!((q.lng + 92.3).abs() < 1e-6 && (q.lat - 38.6).abs() < 1e-6);
        assert!(
            to_lnglat(
                Crs::Utm {
                    zone: 15,
                    north: true
                },
                5.0,
                5.0
            )
            .is_none(),
            "out-of-range easting"
        );
    }

    #[test]
    fn export_then_import_roundtrips() {
        let mut e = Editor::default();
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("Pin".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.1)]),
            Some("Line".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-92.0, 38.0),
                LngLat::new(-91.9, 38.0),
                LngLat::new(-91.9, 38.1),
            ]),
            Some("Poly".into()),
        );
        let mut f = e.project.features[0].clone();
        f.notes = "gate 7".into();
        e.replace(f);
        let dir = tmp("rt");
        let (files, warnings) = write_shapefiles(&e.project, &dir, "map").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(files.len(), 3);
        for f in &files {
            assert!(
                f.with_extension("shx").exists()
                    && f.with_extension("dbf").exists()
                    && f.with_extension("prj").exists()
            );
        }
        let mut all = Imported::default();
        for f in &files {
            all.features.extend(read_shapefile(f).unwrap().features);
        }
        let by = |n: &str| all.features.iter().find(|f| f.feature.name == n).unwrap();
        assert_eq!(by("Pin").feature.geometry, e.project.features[0].geometry);
        assert_eq!(by("Pin").feature.notes, "gate 7");
        assert_eq!(by("Line").feature.geometry, e.project.features[1].geometry);
        match &by("Poly").feature.geometry {
            Geometry::Polygon(r) => {
                assert_eq!(r.len(), 3, "closing vertex dropped on import");
                // Shapefile rings are re-wound clockwise on write; compare as a set.
                for p in match &e.project.features[2].geometry {
                    Geometry::Polygon(v) => v.clone(),
                    _ => unreachable!(),
                } {
                    assert!(r.contains(&p));
                }
            }
            g => panic!("{g:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_utm_perimeter_with_attributes() {
        // A "NIFC-like" polygon in NAD83 UTM 15N, with attribute columns.
        let dir = tmp("utm");
        let path = dir.join("perimeter.shp");
        let (n0, e0, _) = utm::to_utm_wgs84(38.6, -92.3, 15);
        let ring = vec![
            Point::new(e0, n0),
            Point::new(e0, n0 + 1000.0),
            Point::new(e0 + 1000.0, n0 + 1000.0),
            Point::new(e0 + 1000.0, n0),
            Point::new(e0, n0),
        ];
        let tb = TableWriterBuilder::new()
            .add_character_field("INCIDENTNA".try_into().unwrap(), 40)
            .add_numeric_field("GISACRES".try_into().unwrap(), 12, 2);
        let mut w = shapefile::Writer::from_path(&path, tb).unwrap();
        let mut rec = dbase::Record::default();
        rec.insert(
            "INCIDENTNA".into(),
            FieldValue::Character(Some("Pine Ridge".into())),
        );
        rec.insert("GISACRES".into(), FieldValue::Numeric(Some(247.5)));
        w.write_shape_and_record(&Polygon::new(PolygonRing::Outer(ring)), &rec)
            .unwrap();
        drop(w);
        std::fs::write(
            path.with_extension("prj"),
            r#"PROJCS["NAD_1983_UTM_Zone_15N",GEOGCS["GCS_North_American_1983"],PROJECTION["Transverse_Mercator"],UNIT["Meter",1.0]]"#,
        )
        .unwrap();
        let imp = read_shapefile(&path).unwrap();
        assert_eq!(imp.features.len(), 1);
        let f = &imp.features[0].feature;
        assert_eq!(f.name, "Pine Ridge");
        assert!(f.notes.contains("GISACRES: 247.5"), "{}", f.notes);
        match &f.geometry {
            Geometry::Polygon(r) => {
                assert!(
                    r.iter()
                        .all(|p| (p.lng + 92.3).abs() < 0.02 && (p.lat - 38.6).abs() < 0.02),
                    "{r:?}"
                );
                // A 1 km square: area within 1% of 1 km^2.
                let a = smf_geo::polygon_area_m2(r);
                assert!((a - 1_000_000.0).abs() < 10_000.0, "{a}");
            }
            g => panic!("{g:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_projection_is_refused_not_misplaced() {
        let dir = tmp("badprj");
        let path = dir.join("x.shp");
        let mut w = shapefile::Writer::from_path(&path, table()).unwrap();
        w.write_shape_and_record(&Point::new(1_000_000.0, 500_000.0), &record("p", ""))
            .unwrap();
        drop(w);
        std::fs::write(
            path.with_extension("prj"),
            r#"PROJCS["NAD_1983_StatePlane_Missouri_Central_FIPS_2402_Feet",UNIT["Foot_US",0.3]]"#,
        )
        .unwrap();
        let err = read_shapefile(&path).unwrap_err().to_string();
        assert!(err.contains("feet"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_prj_assumes_lnglat_with_a_warning() {
        let dir = tmp("noprj");
        let path = dir.join("x.shp");
        let mut w = shapefile::Writer::from_path(&path, table()).unwrap();
        w.write_shape_and_record(&Point::new(-92.0, 38.0), &record("p", ""))
            .unwrap();
        drop(w);
        let imp = read_shapefile(&path).unwrap();
        assert_eq!(imp.features.len(), 1);
        assert!(imp.warnings.iter().any(|w| w.contains(".prj")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clip_respects_utf8_boundaries() {
        assert_eq!(clip("héllo", 3), "hé");
        assert_eq!(clip("abc", 10), "abc");
    }
}
