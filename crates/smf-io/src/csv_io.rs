//! CSV with latitude/longitude columns (waypoint lists) and marker export.

use crate::IoError;
use crate::common::*;
use smf_features::{Geometry, Project, Style};
use smf_geo::LngLat;

const LAT: &[&str] = &["lat", "latitude", "y", "lat_dd", "point_y"];
const LNG: &[&str] = &["lon", "lng", "long", "longitude", "x", "lon_dd", "point_x"];
const NAME: &[&str] = &["name", "title", "label", "id", "waypoint"];
const NOTES: &[&str] = &[
    "notes",
    "description",
    "desc",
    "comment",
    "comments",
    "remarks",
];

fn find(headers: &[String], names: &[&str]) -> Option<usize> {
    names
        .iter()
        .find_map(|n| headers.iter().position(|h| h == n))
}

pub fn read_csv(text: &str) -> Result<Imported, IoError> {
    let text = text.trim_start_matches('\u{feff}'); // Excel adds a BOM
    let first = text.lines().next().unwrap_or("");
    // Excel in many locales writes semicolons, and tab-separated exports are common.
    let delim = *b",;\t"
        .iter()
        .max_by_key(|d| first.bytes().filter(|b| b == *d).count())
        .unwrap_or(&b',');
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| IoError::Format(format!("CSV header: {e}")))?
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let (Some(lat_i), Some(lng_i)) = (find(&headers, LAT), find(&headers, LNG)) else {
        return Err(IoError::Format(format!(
            "CSV needs latitude and longitude columns (found: {}). Name them lat/lon, latitude/longitude or y/x.",
            headers.join(", ")
        )));
    };
    let (name_i, notes_i) = (find(&headers, NAME), find(&headers, NOTES));
    let mut imp = Imported::default();
    let mut skipped = 0;
    for rec in rdr.records() {
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let num = |i: usize| {
            rec.get(i)
                .and_then(|s| s.replace(',', ".").trim().parse::<f64>().ok())
        };
        match (num(lng_i), num(lat_i)) {
            (Some(lng), Some(lat)) if valid_lnglat(lng, lat) => {
                let name = name_i.and_then(|i| rec.get(i)).unwrap_or("");
                let notes = notes_i.and_then(|i| rec.get(i)).unwrap_or("");
                imp.push(
                    None,
                    name,
                    Geometry::Marker(LngLat::new(lng, lat)),
                    Style::default(),
                    notes,
                );
            }
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        imp.warn(format!(
            "{skipped} row(s) skipped: missing or invalid latitude/longitude."
        ));
    }
    Ok(imp)
}

fn quote(s: &str) -> String {
    // Quote when needed, and defuse spreadsheet formula injection (=, +, -, @ at the start of a cell).
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// Markers and labels as rows; other shapes are listed in the returned warnings.
pub fn write_csv(project: &Project) -> (String, Vec<String>) {
    let mut out = String::from("name,latitude,longitude,notes,type\n");
    let mut skipped = 0;
    for f in &project.features {
        match &f.geometry {
            Geometry::Marker(p) | Geometry::Label(p) => out.push_str(&format!(
                "{},{},{},{},{}\n",
                quote(&f.name),
                p.lat,
                p.lng,
                quote(&f.notes),
                f.geometry.kind_name()
            )),
            _ => skipped += 1,
        }
    }
    let warnings = if skipped > 0 {
        vec![format!(
            "{skipped} line/polygon/ring feature(s) are not representable in CSV and were skipped."
        )]
    } else {
        vec![]
    };
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::Editor;

    #[test]
    fn imports_common_header_variants_and_bom() {
        let imp = read_csv("\u{feff}Name,Latitude,Longitude,Description\nCP,38.6,-92.3,\"Gate, north\"\nStaging,38.7,-92.2,\n").unwrap();
        assert_eq!(imp.features.len(), 2);
        assert_eq!(imp.features[0].feature.name, "CP");
        assert_eq!(imp.features[0].feature.notes, "Gate, north");
        assert_eq!(
            imp.features[1].feature.geometry,
            Geometry::Marker(LngLat::new(-92.2, 38.7))
        );
    }

    #[test]
    fn handles_semicolons_and_decimal_commas() {
        let imp = read_csv("name;lat;lon\nA;38,6;-92,3\n").unwrap();
        assert_eq!(
            imp.features[0].feature.geometry,
            Geometry::Marker(LngLat::new(-92.3, 38.6))
        );
    }

    #[test]
    fn skips_bad_rows_and_requires_coordinate_columns() {
        let imp = read_csv("lat,lon\n38.6,-92.3\nabc,1\n95,10\n,\n").unwrap();
        assert_eq!(imp.features.len(), 1);
        assert!(imp.warnings[0].starts_with("3 row(s) skipped"));
        let err = read_csv("name,city\nA,B\n").unwrap_err().to_string();
        assert!(err.contains("latitude and longitude"), "{err}");
    }

    #[test]
    fn export_roundtrips_and_neutralises_formulas() {
        let mut e = Editor::default();
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("=HYPERLINK(\"x\")".into()),
        );
        e.add(
            Geometry::Marker(LngLat::new(-92.2, 38.6)),
            Some("a, \"b\"".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(0.0, 0.0), LngLat::new(1.0, 1.0)]),
            None,
        );
        let (text, warnings) = write_csv(&e.project);
        assert_eq!(warnings.len(), 1);
        assert!(
            !text.contains("\n=HYPERLINK"),
            "formula must not start a cell: {text}"
        );
        let imp = read_csv(&text).unwrap();
        assert_eq!(imp.features.len(), 2);
        assert_eq!(imp.features[1].feature.name, "a, \"b\"");
        assert_eq!(
            imp.features[1].feature.geometry,
            e.project.features[1].geometry
        );
    }
}
