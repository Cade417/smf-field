//! GeoJSON import/export. Reads and writes the "simplestyle" conventions and the
//! CalTopo folder convention (features with `class: "Folder"` and `folderId`).

use crate::common::*;
use serde_json::{Map, Value, json};
use smf_features::{Feature, Geometry, Project, Style};
use smf_geo::{LngLat, geodesic_circle};

fn pt(v: &Value) -> Option<LngLat> {
    let a = v.as_array()?;
    let (lng, lat) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    valid_lnglat(lng, lat).then(|| LngLat::new(lng, lat))
}

fn line(v: &Value) -> Option<Vec<LngLat>> {
    v.as_array()?.iter().map(pt).collect()
}

fn str_prop<'a>(p: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| p.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

fn style_from(p: &Map<String, Value>, is_marker: bool) -> Style {
    let mut s = Style::default();
    let color_key = if is_marker { "marker-color" } else { "stroke" };
    if let Some(c) = p
        .get(color_key)
        .and_then(Value::as_str)
        .and_then(parse_hex_color)
    {
        s.color = c;
    } else if let Some(c) = p
        .get("stroke")
        .and_then(Value::as_str)
        .and_then(parse_hex_color)
    {
        s.color = c;
    }
    if let Some(w) = p.get("stroke-width").and_then(Value::as_f64) {
        s.width = (w as f32).clamp(0.5, 12.0);
    }
    if let Some(a) = p.get("fill-opacity").and_then(Value::as_f64) {
        s.fill_alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    s
}

pub fn read_geojson(text: &str) -> Result<Imported, serde_json::Error> {
    let root: Value = serde_json::from_str(text)?;
    let mut imp = Imported::default();
    // CalTopo stores folders as geometry-less features referenced by id.
    let mut folder_by_id: Vec<(String, usize)> = Vec::new();
    let items: Vec<&Value> = match root.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => root
            .get("features")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default(),
        Some("Feature") => vec![&root],
        _ => vec![],
    };
    if items.is_empty() && root.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
        // A bare geometry.
        if root.get("coordinates").is_some() || root.get("geometries").is_some() {
            add_geometry(&mut imp, &root, None, "", &Map::new());
        } else {
            imp.warn("File is JSON but not GeoJSON (no FeatureCollection, Feature or geometry).");
        }
        return Ok(imp);
    }
    // Folders first so features can refer to them regardless of order.
    for it in &items {
        let props = it.get("properties").and_then(Value::as_object);
        if let Some(p) = props
            && it.get("geometry").is_none_or(Value::is_null)
            && p.get("class").and_then(Value::as_str) == Some("Folder")
        {
            let name = str_prop(p, &["title", "name"]).unwrap_or("Folder");
            let idx = imp.folder_index(name);
            if let Some(id) = p
                .get("id")
                .map(|v| v.to_string().trim_matches('"').to_owned())
            {
                folder_by_id.push((id, idx));
            }
        }
    }
    for it in items {
        let empty = Map::new();
        let props = it
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let geom = it.get("geometry");
        if geom.is_none_or(Value::is_null) {
            if props.get("class").and_then(Value::as_str) != Some("Folder") {
                imp.warn("Skipped a feature with no geometry.");
            }
            continue;
        }
        let folder = props
            .get("folderId")
            .map(|v| v.to_string().trim_matches('"').to_owned())
            .and_then(|id| folder_by_id.iter().find(|(f, _)| *f == id).map(|(_, i)| *i));
        let name = str_prop(props, &["title", "name", "Name", "NAME"]).unwrap_or("");
        add_geometry(&mut imp, geom.unwrap(), folder, name, props);
    }
    Ok(imp)
}

fn add_geometry(
    imp: &mut Imported,
    g: &Value,
    folder: Option<usize>,
    name: &str,
    props: &Map<String, Value>,
) {
    let notes = str_prop(props, &["description", "notes", "desc"]).unwrap_or("");
    let ty = g.get("type").and_then(Value::as_str).unwrap_or("");
    let coords = g.get("coordinates");
    let mut bad = || imp.warn(format!("Skipped a {ty} with invalid coordinates."));
    // Our own lossless extensions for shapes GeoJSON can't express.
    let kind = props.get("smf:kind").and_then(Value::as_str);
    match (ty, coords) {
        ("Point", Some(c)) => match pt(c) {
            Some(p) => {
                let (geometry, marker) = match kind {
                    Some("RangeRing") => {
                        let radii: Vec<f64> = props
                            .get("smf:radii")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_f64)
                                    .filter(|r| *r > 0.0)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let radii = if radii.is_empty() {
                            smf_features::default_radii()
                        } else {
                            radii
                        };
                        (
                            Geometry::RangeRing {
                                center: p,
                                radii_m: radii,
                            },
                            false,
                        )
                    }
                    Some("Label") => (Geometry::Label(p), false),
                    _ => (Geometry::Marker(p), true),
                };
                imp.push(folder, name, geometry, style_from(props, marker), notes);
            }
            None => bad(),
        },
        ("MultiPoint", Some(c)) => match line(c) {
            Some(ps) => ps.into_iter().for_each(|p| {
                imp.push(
                    folder,
                    name,
                    Geometry::Marker(p),
                    style_from(props, true),
                    notes,
                )
            }),
            None => bad(),
        },
        ("LineString", Some(c)) => match line(c) {
            Some(v) if v.len() >= 2 => imp.push(
                folder,
                name,
                Geometry::Line(v),
                style_from(props, false),
                notes,
            ),
            _ => bad(),
        },
        ("MultiLineString", Some(c)) => match c
            .as_array()
            .map(|a| a.iter().map(line).collect::<Option<Vec<_>>>())
        {
            Some(Some(parts)) => {
                for v in parts.into_iter().filter(|v| v.len() >= 2) {
                    imp.push(
                        folder,
                        name,
                        Geometry::Line(v),
                        style_from(props, false),
                        notes,
                    );
                }
            }
            _ => bad(),
        },
        ("Polygon", Some(c)) => match polygon_outer(c) {
            Some(r) => {
                if c.as_array().is_some_and(|a| a.len() > 1) {
                    imp.warn("Polygon holes were dropped (only outer rings are supported).");
                }
                imp.push(
                    folder,
                    name,
                    Geometry::Polygon(r),
                    style_from(props, false),
                    notes,
                );
            }
            None => bad(),
        },
        ("MultiPolygon", Some(c)) => match c
            .as_array()
            .map(|a| a.iter().map(polygon_outer).collect::<Option<Vec<_>>>())
        {
            Some(Some(rings)) => {
                for r in rings {
                    imp.push(
                        folder,
                        name,
                        Geometry::Polygon(r),
                        style_from(props, false),
                        notes,
                    );
                }
            }
            _ => bad(),
        },
        ("GeometryCollection", _) => {
            if let Some(gs) = g.get("geometries").and_then(Value::as_array) {
                for sub in gs {
                    add_geometry(imp, sub, folder, name, props);
                }
            }
        }
        _ => imp.warn(format!("Unsupported geometry type: {ty}")),
    }
}

fn polygon_outer(c: &Value) -> Option<Vec<LngLat>> {
    let ring = open_ring(line(c.as_array()?.first()?)?);
    (ring.len() >= 3).then_some(ring)
}

fn coord(p: LngLat) -> Value {
    json!([p.lng, p.lat])
}

fn feature_json(f: &Feature, folder_id: Option<u64>) -> Value {
    let mut props = Map::new();
    props.insert("title".into(), json!(f.name));
    props.insert("name".into(), json!(f.name));
    if !f.notes.is_empty() {
        props.insert("description".into(), json!(f.notes));
    }
    let color = hex_color(f.style.color);
    let geometry = match &f.geometry {
        Geometry::Marker(p) => {
            props.insert("marker-color".into(), json!(color));
            json!({"type": "Point", "coordinates": coord(*p)})
        }
        Geometry::Label(p) => {
            props.insert("marker-color".into(), json!(color));
            props.insert("smf:kind".into(), json!("Label"));
            json!({"type": "Point", "coordinates": coord(*p)})
        }
        Geometry::RangeRing { center, radii_m } => {
            props.insert("stroke".into(), json!(color));
            props.insert("smf:kind".into(), json!("RangeRing"));
            props.insert("smf:radii".into(), json!(radii_m));
            json!({"type": "Point", "coordinates": coord(*center)})
        }
        Geometry::Line(v) => {
            props.insert("stroke".into(), json!(color));
            props.insert("stroke-width".into(), json!(f.style.width));
            json!({"type": "LineString", "coordinates": v.iter().map(|p| coord(*p)).collect::<Vec<_>>()})
        }
        Geometry::Polygon(v) => {
            props.insert("stroke".into(), json!(color));
            props.insert("stroke-width".into(), json!(f.style.width));
            props.insert("fill".into(), json!(color));
            props.insert(
                "fill-opacity".into(),
                json!((f.style.fill_alpha as f64 / 255.0 * 100.0).round() / 100.0),
            );
            let mut ring: Vec<Value> = v.iter().map(|p| coord(*p)).collect();
            ring.push(coord(v[0]));
            json!({"type": "Polygon", "coordinates": [ring]})
        }
    };
    if let Some(id) = folder_id {
        props.insert("folderId".into(), json!(id.to_string()));
    }
    json!({"type": "Feature", "properties": props, "geometry": geometry})
}

pub fn write_geojson(project: &Project) -> String {
    let mut items: Vec<Value> = project
        .folders
        .iter()
        .map(|fo| json!({"type": "Feature", "geometry": null, "properties": {"class": "Folder", "id": fo.id.to_string(), "title": fo.name}}))
        .collect();
    items.extend(project.features.iter().map(|f| feature_json(f, f.folder)));
    serde_json::to_string_pretty(&json!({"type": "FeatureCollection", "features": items}))
        .expect("json")
}

/// Ring outlines for tools that can't draw our range rings natively (used by other exporters).
pub fn ring_outlines(center: LngLat, radii: &[f64]) -> Vec<Vec<LngLat>> {
    radii
        .iter()
        .map(|r| geodesic_circle(center, *r, 96))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::Editor;

    const CALTOPO: &str = r##"{
      "type": "FeatureCollection",
      "features": [
        {"type":"Feature","geometry":null,"properties":{"class":"Folder","id":"f1","title":"Division A"}},
        {"type":"Feature","properties":{"title":"Drop point","class":"Marker","marker-color":"#FF0000","folderId":"f1","description":"Helispot"},
         "geometry":{"type":"Point","coordinates":[-92.3,38.6,210.5]}},
        {"type":"Feature","properties":{"title":"Dozer line","class":"Shape","stroke":"#00ff00","stroke-width":4,"folderId":"f1"},
         "geometry":{"type":"LineString","coordinates":[[-92.3,38.6],[-92.2,38.65],[-92.1,38.6]]}},
        {"type":"Feature","properties":{"title":"Unit 1","stroke":"#0000ff","fill":"#0000ff","fill-opacity":0.5},
         "geometry":{"type":"Polygon","coordinates":[[[-92,38],[-91.9,38],[-91.9,38.1],[-92,38.1],[-92,38]],[[-91.97,38.03],[-91.95,38.03],[-91.95,38.05],[-91.97,38.03]]]}}
      ]}"##;

    #[test]
    fn imports_caltopo_style_file() {
        let imp = read_geojson(CALTOPO).unwrap();
        assert_eq!(imp.folders, vec!["Division A"]);
        assert_eq!(imp.features.len(), 3);
        let marker = &imp.features[0];
        assert_eq!(marker.folder, Some(0));
        assert_eq!(marker.feature.name, "Drop point");
        assert_eq!(marker.feature.notes, "Helispot");
        assert_eq!(marker.feature.style.color, [255, 0, 0]);
        assert_eq!(
            marker.feature.geometry,
            Geometry::Marker(LngLat::new(-92.3, 38.6)),
            "altitude is ignored"
        );
        let line = &imp.features[1];
        assert_eq!(line.feature.style.width, 4.0);
        assert_eq!(line.feature.style.color, [0, 255, 0]);
        let poly = &imp.features[2];
        match &poly.feature.geometry {
            Geometry::Polygon(r) => assert_eq!(r.len(), 4, "closing vertex dropped"),
            g => panic!("{g:?}"),
        }
        assert_eq!(poly.feature.style.fill_alpha, 128);
        assert!(imp.warnings.iter().any(|w| w.contains("holes")));
    }

    #[test]
    fn roundtrips_all_geometry_kinds() {
        let mut e = Editor::default();
        let fid = e.project.add_folder("Ops");
        e.add(Geometry::Marker(LngLat::new(-92.1, 38.5)), Some("M".into()));
        e.add(
            Geometry::Label(LngLat::new(-92.0, 38.5)),
            Some("Hello".into()),
        );
        e.add(
            Geometry::RangeRing {
                center: LngLat::new(-92.0, 38.0),
                radii_m: vec![500.0, 1500.5],
            },
            Some("Rings".into()),
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
        let mut f = e.project.features[0].clone();
        f.folder = Some(fid);
        f.notes = "has \"quotes\" & <tags>".into();
        e.replace(f);
        let text = write_geojson(&e.project);
        let imp = read_geojson(&text).unwrap();
        assert_eq!(imp.folders, vec!["Ops"]);
        assert_eq!(imp.features.len(), 5);
        for (orig, back) in e.project.features.iter().zip(&imp.features) {
            assert_eq!(orig.name, back.feature.name);
            assert_eq!(orig.geometry, back.feature.geometry, "{}", orig.name);
            assert_eq!(orig.notes, back.feature.notes);
        }
        assert_eq!(imp.features[0].folder, Some(0));
        assert!(imp.warnings.is_empty(), "{:?}", imp.warnings);
    }

    #[test]
    fn handles_bare_geometry_collections_and_multis() {
        let imp = read_geojson(r#"{"type":"MultiPoint","coordinates":[[1,2],[3,4]]}"#).unwrap();
        assert_eq!(imp.features.len(), 2);
        let imp = read_geojson(
            r#"{"type":"Feature","properties":{"name":"x"},"geometry":{"type":"GeometryCollection","geometries":[
               {"type":"Point","coordinates":[1,2]},{"type":"LineString","coordinates":[[0,0],[1,1]]}]}}"#,
        )
        .unwrap();
        assert_eq!(imp.features.len(), 2);
        assert!(imp.features.iter().all(|f| f.feature.name == "x"));
        let imp = read_geojson(
            r#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]],[[[5,5],[6,5],[6,6],[5,5]]]]}"#,
        )
        .unwrap();
        assert_eq!(imp.features.len(), 2);
    }

    #[test]
    fn bad_input_reports_instead_of_panicking() {
        assert!(read_geojson("not json").is_err());
        let imp = read_geojson(r#"{"hello":"world"}"#).unwrap();
        assert!(imp.features.is_empty() && !imp.warnings.is_empty());
        let imp = read_geojson(r#"{"type":"Point","coordinates":[999,999]}"#).unwrap();
        assert!(
            imp.features.is_empty() && !imp.warnings.is_empty(),
            "out-of-range coords are rejected"
        );
        let imp = read_geojson(r#"{"type":"Feature","geometry":{"type":"Circle","coordinates":[1,2]},"properties":{}}"#).unwrap();
        assert!(imp.warnings.iter().any(|w| w.contains("Unsupported")));
    }
}
