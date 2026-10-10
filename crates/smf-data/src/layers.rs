//! Show Me Fire vector data layers: what each one is, how to parse it, how to colour it,
//! and what to show in its info popup.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use smf_features::Geometry;
use smf_geo::LngLat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DataLayerKind {
    Incidents,
    IncidentShapes,
    Detections,
    Events,
    BurnBans,
    WeatherAlerts,
}

impl DataLayerKind {
    pub const ALL: [DataLayerKind; 6] = [
        DataLayerKind::Incidents,
        DataLayerKind::IncidentShapes,
        DataLayerKind::Detections,
        DataLayerKind::Events,
        DataLayerKind::BurnBans,
        DataLayerKind::WeatherAlerts,
    ];

    /// Stable key for caches and saved preferences.
    pub fn id(self) -> &'static str {
        match self {
            DataLayerKind::Incidents => "incidents",
            DataLayerKind::IncidentShapes => "incident-shapes",
            DataLayerKind::Detections => "detections",
            DataLayerKind::Events => "events",
            DataLayerKind::BurnBans => "burn-bans",
            DataLayerKind::WeatherAlerts => "weather-alerts",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DataLayerKind::Incidents => "Fire incidents",
            DataLayerKind::IncidentShapes => "Incident footprints",
            DataLayerKind::Detections => "Satellite detections",
            DataLayerKind::Events => "Detection events",
            DataLayerKind::BurnBans => "Burn bans",
            DataLayerKind::WeatherAlerts => "Fire weather alerts",
        }
    }

    /// Endpoint path on the Show Me Fire API (public, no authentication).
    pub fn path(self) -> &'static str {
        match self {
            DataLayerKind::Incidents => "/api/fires/incidents.geojson",
            DataLayerKind::IncidentShapes => "/api/fires/incident-shapes.geojson",
            DataLayerKind::Detections => "/api/fires/detections-footprints.geojson",
            DataLayerKind::Events => "/api/fires/events.geojson",
            DataLayerKind::BurnBans => "/api/burn-bans/active.geojson",
            DataLayerKind::WeatherAlerts => "/api/fire-weather-alerts/active.geojson",
        }
    }

    /// How often to re-fetch while the layer is on. The API caches fire data for 60 s.
    pub fn refresh_secs(self) -> u64 {
        match self {
            DataLayerKind::Incidents
            | DataLayerKind::IncidentShapes
            | DataLayerKind::Detections
            | DataLayerKind::Events => 120,
            DataLayerKind::BurnBans | DataLayerKind::WeatherAlerts => 600,
        }
    }

    /// Shown the first time the app runs: the incident picture a responder wants immediately.
    pub fn default_on(self) -> bool {
        matches!(
            self,
            DataLayerKind::Incidents | DataLayerKind::IncidentShapes | DataLayerKind::BurnBans
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DataStyle {
    pub color: [u8; 3],
    pub fill_alpha: u8,
    pub width: f32,
    /// Marker radius in points (points only).
    pub radius: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataFeature {
    pub kind: DataLayerKind,
    pub geometry: Geometry,
    pub props: Map<String, Value>,
    pub style: DataStyle,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("network error: {0}")]
    Network(String),
    #[error("the server answered {0}")]
    Status(u16),
    #[error("could not read the response: {0}")]
    Parse(String),
    #[error("offline")]
    Offline,
}

#[derive(Debug)]
pub struct ParsedLayer {
    pub features: Vec<DataFeature>,
    /// The server's own timestamp (`metadata.fetched_at` / `generated_at`), if present.
    pub generated_at: Option<String>,
    pub skipped: usize,
}

const RED: [u8; 3] = [220, 38, 38];
const ORANGE: [u8; 3] = [245, 124, 11];
const AMBER: [u8; 3] = [234, 179, 8];

fn num(p: &Map<String, Value>, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64)
}

fn text<'a>(p: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Colour by how sure the system is that this is a real fire (never by land use: agricultural and
/// prescribed burns are real fires and must not look less important).
fn confidence_color(pct: Option<f64>, label: Option<&str>) -> [u8; 3] {
    match (pct, label) {
        (Some(p), _) if p >= 70.0 => RED,
        (Some(p), _) if p >= 40.0 => ORANGE,
        (Some(_), _) => AMBER,
        (None, Some("high")) => RED,
        (None, Some("medium")) => ORANGE,
        _ => AMBER,
    }
}

pub fn style_for(kind: DataLayerKind, p: &Map<String, Value>) -> DataStyle {
    match kind {
        DataLayerKind::Incidents => DataStyle {
            color: confidence_color(
                num(p, "INCIDENT_CONFIDENCE_PCT"),
                text(p, "INCIDENT_CONFIDENCE_LABEL"),
            ),
            fill_alpha: 255,
            width: 2.0,
            radius: 7.0,
        },
        DataLayerKind::IncidentShapes => DataStyle {
            color: confidence_color(
                num(p, "INCIDENT_CONFIDENCE_PCT"),
                text(p, "INCIDENT_CONFIDENCE_LABEL"),
            ),
            fill_alpha: 70,
            width: 2.0,
            radius: 0.0,
        },
        DataLayerKind::Detections => DataStyle {
            color: confidence_color(num(p, "DETECTION_CONFIDENCE_PCT"), None),
            fill_alpha: 95,
            width: 1.0,
            radius: 0.0,
        },
        DataLayerKind::Events => DataStyle {
            color: confidence_color(num(p, "DETECTION_CONFIDENCE_PCT"), None),
            fill_alpha: 255,
            width: 1.0,
            radius: 4.0,
        },
        DataLayerKind::BurnBans => DataStyle {
            color: [126, 34, 206],
            fill_alpha: 45,
            width: 1.6,
            radius: 0.0,
        },
        DataLayerKind::WeatherAlerts => DataStyle {
            color: [249, 115, 22],
            fill_alpha: 55,
            width: 1.6,
            radius: 0.0,
        },
    }
}

pub fn title_for(kind: DataLayerKind, p: &Map<String, Value>) -> String {
    match kind {
        DataLayerKind::Incidents | DataLayerKind::IncidentShapes => {
            let county = text(p, "COUNTY")
                .map(|c| format!("{c} County"))
                .unwrap_or_else(|| "Unknown county".into());
            let pct = num(p, "INCIDENT_CONFIDENCE_PCT")
                .map(|v| format!(", {v:.0}% confidence"))
                .unwrap_or_default();
            format!("Fire incident, {county}{pct}")
        }
        DataLayerKind::Detections => {
            let sat = text(p, "SATELLITE").unwrap_or("satellite");
            format!("Detection ({sat})")
        }
        DataLayerKind::Events => {
            let county = text(p, "COUNTY")
                .map(|c| format!(", {c} County"))
                .unwrap_or_default();
            format!("Fire event{county}")
        }
        DataLayerKind::BurnBans => format!(
            "Burn ban: {} County",
            text(p, "county_name").unwrap_or("unknown")
        ),
        DataLayerKind::WeatherAlerts => text(p, "event")
            .or_else(|| text(p, "headline"))
            .or_else(|| text(p, "NAME"))
            .unwrap_or("Fire weather alert")
            .to_owned(),
    }
}

fn pt(v: &Value) -> Option<LngLat> {
    let a = v.as_array()?;
    let (lng, lat) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    ((-180.0..=180.0).contains(&lng) && (-90.0..=90.0).contains(&lat))
        .then(|| LngLat::new(lng, lat))
}

fn ring(v: &Value) -> Option<Vec<LngLat>> {
    let mut r: Vec<LngLat> = v.as_array()?.iter().map(pt).collect::<Option<_>>()?;
    if r.len() > 1 && r.first() == r.last() {
        r.pop();
    }
    (r.len() >= 3).then_some(r)
}

/// Geometries for one GeoJSON feature: a MultiPolygon becomes several polygons.
fn geometries(g: &Value) -> Vec<Geometry> {
    let coords = g.get("coordinates");
    match (g.get("type").and_then(Value::as_str), coords) {
        (Some("Point"), Some(c)) => pt(c).map(Geometry::Marker).into_iter().collect(),
        (Some("MultiPoint"), Some(c)) => c
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(pt)
            .map(Geometry::Marker)
            .collect(),
        // Outer ring only; the layers drawn here do not carry meaningful holes.
        (Some("Polygon"), Some(c)) => c
            .as_array()
            .and_then(|a| a.first())
            .and_then(ring)
            .map(Geometry::Polygon)
            .into_iter()
            .collect(),
        (Some("MultiPolygon"), Some(c)) => c
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|poly| poly.as_array().and_then(|a| a.first()).and_then(ring))
            .map(Geometry::Polygon)
            .collect(),
        _ => vec![],
    }
}

/// Parse a Show Me Fire GeoJSON response. Features it can't draw are counted, not fatal.
pub fn parse_layer(kind: DataLayerKind, text_in: &str) -> Result<ParsedLayer, DataError> {
    let root: Value = serde_json::from_str(text_in).map_err(|e| DataError::Parse(e.to_string()))?;
    let items = root
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| DataError::Parse("not a GeoJSON FeatureCollection".into()))?;
    let generated_at = root
        .get("metadata")
        .and_then(|m| m.get("fetched_at").or_else(|| m.get("generated_at")))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (mut features, mut skipped) = (Vec::with_capacity(items.len()), 0);
    for it in items {
        let props = it
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let geoms = it.get("geometry").map(geometries).unwrap_or_default();
        if geoms.is_empty() {
            skipped += 1;
            continue;
        }
        let (style, title) = (style_for(kind, &props), title_for(kind, &props));
        for geometry in geoms {
            features.push(DataFeature {
                kind,
                geometry,
                props: props.clone(),
                style,
                title: title.clone(),
            });
        }
    }
    Ok(ParsedLayer {
        features,
        generated_at,
        skipped,
    })
}

fn fmt_value(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "yes".into() } else { "no".into() }),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(_) => None,
    }
}

/// "2026-10-10T01:41:18.000000Z" -> "2026-10-10 01:41 UTC". Anything else is shown unchanged.
pub fn short_time(s: &str) -> String {
    let b = s.as_bytes();
    let ok = b.len() >= 16
        && (b[10] == b'T' || b[10] == b' ')
        && b[4] == b'-'
        && b[7] == b'-'
        && b[13] == b':'
        && s.is_char_boundary(16);
    if ok {
        format!("{} {} UTC", &s[..10], &s[11..16])
    } else {
        s.to_owned()
    }
}

/// Labelled rows for the info popup, most useful first.
pub fn summary(f: &DataFeature) -> Vec<(String, String)> {
    let p = &f.props;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut add = |label: &str, v: Option<String>| {
        if let Some(v) = v {
            rows.push((label.to_owned(), v));
        }
    };
    let s = |k: &str| p.get(k).and_then(fmt_value);
    let t = |k: &str| p.get(k).and_then(fmt_value).map(|v| short_time(&v));
    match f.kind {
        DataLayerKind::Incidents | DataLayerKind::IncidentShapes => {
            add("County", s("COUNTY"));
            let conf = match (
                num(p, "INCIDENT_CONFIDENCE_PCT"),
                text(p, "INCIDENT_CONFIDENCE_LABEL"),
            ) {
                (Some(pct), Some(l)) => Some(format!("{pct:.0}% ({l})")),
                (Some(pct), None) => Some(format!("{pct:.0}%")),
                (None, Some(l)) => Some(l.to_owned()),
                _ => None,
            };
            add("Confidence this is a fire", conf);
            add("Why", s("INCIDENT_CONFIDENCE_REASONS"));
            add("Detections", s("DETECTION_COUNT"));
            add("First detected", t("FIRST_DETECTED_AT"));
            add("Last detected", t("LAST_DETECTED_AT"));
            add("Satellite", s("SATELLITE"));
            add(
                "Fire radiative power",
                num(p, "FRP").map(|v| format!("{v:.2} MW")),
            );
            add("Land cover", s("LAND_COVER"));
            add("Confirmed", s("CONFIRMED"));
            add("Feedback reports", s("FEEDBACK_COUNT"));
        }
        DataLayerKind::Detections => {
            add("Acquired", t("ACQ_DATE_TIME"));
            add("Satellite", s("SATELLITE"));
            add(
                "Confidence",
                num(p, "DETECTION_CONFIDENCE_PCT").map(|v| format!("{v:.0}%")),
            );
            add(
                "Fire radiative power",
                num(p, "FRP").map(|v| format!("{v:.2} MW")),
            );
            add("Incident", s("INCIDENT_ID"));
        }
        DataLayerKind::Events => {
            add("County", s("COUNTY"));
            add("State", s("STATE"));
            add("Acquired", t("ACQ_DATE_TIME"));
            add("Satellite", s("SATELLITE"));
            add(
                "Confidence",
                num(p, "DETECTION_CONFIDENCE_PCT").map(|v| format!("{v:.0}%")),
            );
            add(
                "Fire radiative power",
                num(p, "FRP").map(|v| format!("{v:.2} MW")),
            );
            add("Land cover", s("LAND_COVER"));
            add("Verification", s("VERIFICATION_TIER"));
            add("Acres", s("ACRES"));
            add("Notes", s("DESCRIPTION"));
        }
        DataLayerKind::BurnBans => {
            add("County", s("county_name"));
            add("Status", s("status"));
            add("Effective", t("effective_at"));
            add("Expires", t("expires_at").or(Some("no end date".into())));
            add("Published", t("published_at"));
        }
        DataLayerKind::WeatherAlerts => {
            // Not yet seen in live data, so show whatever scalar properties the service provides.
            for (k, v) in p {
                if let Some(v) = fmt_value(v) {
                    rows.push((k.clone(), v));
                }
            }
        }
    }
    rows
}

/// Links worth offering from the popup. Only absolute http(s) URLs and site-relative paths ("/x", not
/// "//host") are accepted, so data from the server can never smuggle in other schemes.
pub fn links(f: &DataFeature, base: &str) -> Vec<(String, String)> {
    let abs = |u: &str| -> Option<String> {
        if u.starts_with("http://") || u.starts_with("https://") {
            Some(u.to_owned())
        } else if u.starts_with('/') && !u.starts_with("//") {
            Some(format!("{}{}", base.trim_end_matches('/'), u))
        } else {
            None
        }
    };
    let mut out = Vec::new();
    for (label, key) in [
        ("Incident graphic", "GRAPHIC_URL"),
        ("Report / give feedback", "FEEDBACK_URL"),
        ("Burn ban proof", "proof_url"),
    ] {
        if let Some(u) = text(&f.props, key).and_then(abs) {
            out.push((label.to_owned(), u));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn parses_real_incident_points() {
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        assert_eq!(p.features.len(), 3);
        assert_eq!(p.skipped, 0);
        assert!(p.generated_at.as_deref().unwrap().starts_with("2026-10-10"));
        let f = &p.features[0];
        assert!(matches!(f.geometry, Geometry::Marker(_)));
        // Live data: 76% confidence ("high") -> red, and the title names the county.
        assert_eq!(f.style.color, RED);
        assert!(
            f.title.contains("County") && f.title.contains("76% confidence"),
            "{}",
            f.title
        );
    }

    #[test]
    fn parses_real_footprints_as_closed_rings_without_the_duplicate_point() {
        let p = parse_layer(
            DataLayerKind::IncidentShapes,
            &fixture("incident_shapes.geojson"),
        )
        .unwrap();
        assert_eq!(p.features.len(), 2);
        let raw: Value = serde_json::from_str(&fixture("incident_shapes.geojson")).unwrap();
        let first_ring = raw["features"][0]["geometry"]["coordinates"][0]
            .as_array()
            .unwrap()
            .len();
        match &p.features[0].geometry {
            Geometry::Polygon(r) => assert_eq!(r.len(), first_ring - 1, "closing vertex dropped"),
            g => panic!("{g:?}"),
        }
        assert_eq!(p.features[0].style.fill_alpha, 70);
    }

    #[test]
    fn parses_detections_events_and_burn_bans() {
        let d = parse_layer(DataLayerKind::Detections, &fixture("detections.geojson")).unwrap();
        assert_eq!(d.features.len(), 3);
        let e = parse_layer(DataLayerKind::Events, &fixture("events.geojson")).unwrap();
        assert!(
            e.features
                .iter()
                .all(|f| matches!(f.geometry, Geometry::Marker(_)))
        );
        assert_eq!(e.features[0].style.radius, 4.0);
        let b = parse_layer(DataLayerKind::BurnBans, &fixture("burn_bans.geojson")).unwrap();
        assert_eq!(b.features.len(), 2);
        assert_eq!(b.features[0].title, "Burn ban: Dent County");
        assert_eq!(b.features[0].style.color, [126, 34, 206]);
    }

    #[test]
    fn empty_alert_collection_is_fine() {
        let a = parse_layer(
            DataLayerKind::WeatherAlerts,
            &fixture("alerts_empty.geojson"),
        )
        .unwrap();
        assert!(a.features.is_empty() && a.skipped == 0);
    }

    #[test]
    fn confidence_colours_never_depend_on_land_use() {
        // An agricultural burn is a real fire: same colour as a forest fire at the same confidence.
        let mk = |land: &str| {
            let mut p = Map::new();
            p.insert("INCIDENT_CONFIDENCE_PCT".into(), Value::from(80));
            p.insert("LAND_COVER".into(), Value::from(land));
            style_for(DataLayerKind::Incidents, &p).color
        };
        assert_eq!(mk("Cropland:90,Trees:10"), mk("Trees:90"));
        let mut p = Map::new();
        for (pct, want) in [
            (95.0, RED),
            (70.0, RED),
            (55.0, ORANGE),
            (40.0, ORANGE),
            (10.0, AMBER),
        ] {
            p.insert("INCIDENT_CONFIDENCE_PCT".into(), Value::from(pct));
            assert_eq!(style_for(DataLayerKind::Incidents, &p).color, want, "{pct}");
        }
    }

    #[test]
    fn bad_input_is_reported_and_undrawable_features_are_counted() {
        assert!(parse_layer(DataLayerKind::Incidents, "nope").is_err());
        assert!(parse_layer(DataLayerKind::Incidents, r#"{"type":"Feature"}"#).is_err());
        let p = parse_layer(
            DataLayerKind::Events,
            r#"{"features":[{"properties":{},"geometry":null},
                {"properties":{},"geometry":{"type":"Point","coordinates":[999,5]}},
                {"properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}},
                {"properties":{},"geometry":{"type":"Point","coordinates":[-92.1,38.5]}}]}"#,
        )
        .unwrap();
        assert_eq!((p.features.len(), p.skipped), (1, 3));
    }

    #[test]
    fn multipolygons_become_several_features() {
        let p = parse_layer(
            DataLayerKind::BurnBans,
            r#"{"features":[{"properties":{"county_name":"X"},"geometry":{"type":"MultiPolygon","coordinates":[
                [[[0,0],[1,0],[1,1],[0,0]]],[[[5,5],[6,5],[6,6],[5,5]]]]}}]}"#,
        )
        .unwrap();
        assert_eq!(p.features.len(), 2);
    }

    #[test]
    fn summaries_use_human_labels_and_skip_empty_values() {
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        let rows = summary(&p.features[0]);
        let get = |k: &str| rows.iter().find(|(l, _)| l == k).map(|(_, v)| v.clone());
        assert_eq!(get("County").as_deref(), Some("St Clair"));
        assert_eq!(
            get("Confidence this is a fire").as_deref(),
            Some("76% (high)")
        );
        assert!(get("Why").unwrap().contains("satellite scans"));
        assert_eq!(get("Fire radiative power").as_deref(), Some("0.39 MW"));
        assert!(rows.iter().all(|(_, v)| !v.is_empty()));
        // Burn bans with no end date say so instead of hiding the row.
        let b = parse_layer(DataLayerKind::BurnBans, &fixture("burn_bans.geojson")).unwrap();
        assert_eq!(
            summary(&b.features[0])
                .iter()
                .find(|(l, _)| l == "Expires")
                .unwrap()
                .1,
            "no end date"
        );
    }

    #[test]
    fn timestamps_are_shortened_for_display() {
        assert_eq!(
            short_time("2026-10-10T01:41:18.000000Z"),
            "2026-10-10 01:41 UTC"
        );
        assert_eq!(short_time("2026-09-23 01:44:02"), "2026-09-23 01:44 UTC");
        assert_eq!(short_time("yesterday"), "yesterday");
        assert_eq!(short_time(""), "");
        assert_eq!(
            short_time("2026-10-10T01"),
            "2026-10-10T01",
            "too short to reformat"
        );
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        let rows = summary(&p.features[0]);
        assert_eq!(
            rows.iter().find(|(l, _)| l == "Last detected").unwrap().1,
            "2026-10-10 01:41 UTC"
        );
    }

    #[test]
    fn links_are_absolute_and_http_only() {
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        let l = links(&p.features[0], "https://api.showmefire.org/");
        assert!(
            l.iter().any(|(n, u)| n == "Incident graphic"
                && u.starts_with("https://api.showmefire.org/images/"))
        );
        assert!(l.iter().any(|(n, u)| n.starts_with("Report")
            && u == "https://api.showmefire.org/fires/incident/IiZmIlcrHGqx"));
        let mut f = p.features[0].clone();
        f.props
            .insert("GRAPHIC_URL".into(), Value::from("javascript:alert(1)"));
        f.props
            .insert("FEEDBACK_URL".into(), Value::from("file:///etc/passwd"));
        let l = links(&f, "https://x.org");
        assert!(l.is_empty(), "other schemes are dropped entirely: {l:?}");
        f.props
            .insert("FEEDBACK_URL".into(), Value::from("//evil.example/x"));
        assert!(
            links(&f, "https://x.org").is_empty(),
            "protocol-relative URLs are refused"
        );
    }

    #[test]
    fn ids_and_defaults_are_unique_and_sensible() {
        let mut ids: Vec<_> = DataLayerKind::ALL.iter().map(|k| k.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 6);
        assert!(DataLayerKind::Incidents.default_on() && !DataLayerKind::Detections.default_on());
        assert!(
            DataLayerKind::ALL
                .iter()
                .all(|k| k.path().starts_with("/api/"))
        );
    }
}
