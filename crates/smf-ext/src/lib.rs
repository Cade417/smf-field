//! Public third-party data layers that CalTopo users expect next to the basemap:
//! historical fire perimeters (NIFC) and snow gauges (NRCS SNOTEL). Both services are free
//! and need no key. This crate only fetches and parses; drawing lives in the app.

use serde_json::Value;
use smf_data::{DataError, DataStyle};
use smf_features::Geometry;
use smf_geo::LngLat;
use std::path::PathBuf;
use std::time::Duration;

const NIFC_QUERY: &str = "https://services3.arcgis.com/T4QMspbfLg3qTGWY/arcgis/rest/services/InterAgencyFirePerimeterHistory_All_Years_View/FeatureServer/0/query";
const AWDB: &str = "https://wcc.sc.egov.usda.gov/awdbRestApi/services/v1";
/// The service returns at most 2000 records; asking for fewer keeps the reply small.
const MAX_PERIMETERS: u32 = 600;
/// A refreshed station list is only needed occasionally; stations rarely move.
const STATION_MAX_AGE_SECS: u64 = 14 * 24 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct ExtFeature {
    pub geometry: Geometry,
    pub style: DataStyle,
    pub title: String,
    /// Extra lines for a details readout.
    pub lines: Vec<String>,
}

fn net(e: reqwest::Error) -> DataError {
    DataError::Network(e.without_url().to_string())
}

fn parse_err(e: impl std::fmt::Display) -> DataError {
    DataError::Parse(e.to_string())
}

// ---------------------------------------------------------------- dates

/// (year, month, day) for a day count since 1970-01-01 (proleptic Gregorian).
pub fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400) as i32 + (m <= 2) as i32;
    (y, m, d)
}

/// "YYYY-MM-DD" for a unix time in seconds.
pub fn iso_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn year_of(unix: u64) -> i32 {
    civil_from_days((unix / 86_400) as i64).0
}

// ---------------------------------------------------------------- fire history

/// Fire perimeters age from vivid red (this year) to brown-grey (decades ago) so recent
/// burns stand out and old scars stay visible without dominating.
pub fn history_style(age_years: i32) -> DataStyle {
    let (color, alpha) = match age_years {
        i32::MIN..=1 => ([200, 30, 30], 90),
        2..=4 => ([225, 110, 30], 80),
        5..=9 => ([190, 150, 70], 70),
        10..=24 => ([150, 120, 90], 60),
        _ => ([120, 110, 105], 50),
    };
    DataStyle {
        color,
        fill_alpha: alpha,
        width: 1.2,
        radius: 0.0,
    }
}

pub fn fire_history_query(bbox: [f64; 4], tolerance_deg: f64) -> Vec<(&'static str, String)> {
    vec![
        ("where", "1=1".into()),
        (
            "geometry",
            format!("{},{},{},{}", bbox[0], bbox[1], bbox[2], bbox[3]),
        ),
        ("geometryType", "esriGeometryEnvelope".into()),
        ("inSR", "4326".into()),
        ("spatialRel", "esriSpatialRelIntersects".into()),
        ("outFields", "INCIDENT,FIRE_YEAR,GIS_ACRES,AGENCY".into()),
        ("outSR", "4326".into()),
        // Generalize server-side so a continent-sized view doesn't download every vertex.
        ("maxAllowableOffset", format!("{tolerance_deg}")),
        ("resultRecordCount", MAX_PERIMETERS.to_string()),
        ("f", "geojson".into()),
    ]
}

fn ring(v: &Value) -> Option<Vec<LngLat>> {
    let pts: Vec<LngLat> = v
        .as_array()?
        .iter()
        .filter_map(|p| Some(LngLat::new(p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    // Stored rings repeat the first point; the editor's polygons are open.
    let n = if pts.len() > 1 && pts.first() == pts.last() {
        pts.len() - 1
    } else {
        pts.len()
    };
    (n >= 3).then(|| pts[..n].to_vec())
}

/// Parse a NIFC perimeter query. Each outer ring becomes one feature (holes are ignored).
pub fn parse_fire_history(text: &str, current_year: i32) -> Result<Vec<ExtFeature>, DataError> {
    let v: Value = serde_json::from_str(text).map_err(parse_err)?;
    if let Some(e) = v.get("error") {
        return Err(DataError::Parse(format!("service error: {e}")));
    }
    let mut out = vec![];
    for f in v
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| DataError::Parse("no features array".into()))?
    {
        let props = f.get("properties").cloned().unwrap_or(Value::Null);
        let s = |k: &str| {
            props
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let year: Option<i32> = s("FIRE_YEAR").trim().parse().ok().or_else(|| {
            props
                .get("FIRE_YEAR")
                .and_then(Value::as_i64)
                .map(|y| y as i32)
        });
        let acres = props.get("GIS_ACRES").and_then(Value::as_f64);
        let name = {
            let n = s("INCIDENT");
            if n.trim().is_empty() {
                "Unnamed fire".to_string()
            } else {
                n
            }
        };
        let title = match year {
            Some(y) => format!("{name} ({y})"),
            None => name,
        };
        let mut lines = vec![];
        if let Some(a) = acres {
            lines.push(format!("{a:.0} acres"));
        }
        if !s("AGENCY").is_empty() {
            lines.push(format!("Agency: {}", s("AGENCY")));
        }
        let style = history_style(year.map_or(30, |y| current_year - y));
        let Some(g) = f.get("geometry") else { continue };
        let rings: Vec<&Value> = match g.get("type").and_then(Value::as_str) {
            Some("Polygon") => g
                .get("coordinates")
                .and_then(|c| c.get(0))
                .into_iter()
                .collect(),
            Some("MultiPolygon") => g
                .get("coordinates")
                .and_then(Value::as_array)
                .map(|ps| ps.iter().filter_map(|p| p.get(0)).collect())
                .unwrap_or_default(),
            _ => continue,
        };
        for r in rings {
            if let Some(pts) = ring(r) {
                out.push(ExtFeature {
                    geometry: Geometry::Polygon(pts),
                    style,
                    title: title.clone(),
                    lines: lines.clone(),
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- snow gauges

#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub triplet: String,
    pub name: String,
    pub pos: LngLat,
    pub elev_ft: f64,
    pub county: String,
    pub state: String,
}

/// Only automated SNOTEL sites: the list also holds snow courses and unrelated networks
/// that report no daily snow depth.
pub fn parse_stations(text: &str) -> Result<Vec<Station>, DataError> {
    let v: Value = serde_json::from_str(text).map_err(parse_err)?;
    let arr = v
        .as_array()
        .ok_or_else(|| DataError::Parse("station list is not an array".into()))?;
    Ok(arr
        .iter()
        .filter(|s| s.get("networkCode").and_then(Value::as_str) == Some("SNTL"))
        .filter_map(|s| {
            Some(Station {
                triplet: s.get("stationTriplet")?.as_str()?.to_string(),
                name: s.get("name")?.as_str()?.to_string(),
                pos: LngLat::new(s.get("longitude")?.as_f64()?, s.get("latitude")?.as_f64()?),
                elev_ft: s.get("elevation").and_then(Value::as_f64).unwrap_or(0.0),
                county: s
                    .get("countyName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                state: s
                    .get("stateCode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

pub fn stations_in(all: &[Station], bbox: [f64; 4]) -> Vec<&Station> {
    all.iter()
        .filter(|s| {
            s.pos.lng >= bbox[0]
                && s.pos.lng <= bbox[2]
                && s.pos.lat >= bbox[1]
                && s.pos.lat <= bbox[3]
        })
        .collect()
}

/// Marker colour by snow water equivalent in inches: grey (bare) through blue to purple (deep).
pub fn swe_style(swe_in: Option<f64>) -> DataStyle {
    let color = match swe_in {
        None => [140, 140, 140],
        Some(s) if s < 0.1 => [170, 170, 170],
        Some(s) if s < 3.0 => [150, 200, 240],
        Some(s) if s < 10.0 => [70, 140, 225],
        Some(s) if s < 20.0 => [40, 70, 190],
        Some(_) => [120, 50, 170],
    };
    DataStyle {
        color,
        fill_alpha: 255,
        width: 1.5,
        radius: 6.5,
    }
}

/// Latest reported SWE (WTEQ) and snow depth (SNWD), in inches, per station triplet.
pub fn parse_snotel_data(text: &str, stations: &[&Station]) -> Result<Vec<ExtFeature>, DataError> {
    let v: Value = serde_json::from_str(text).map_err(parse_err)?;
    let arr = v
        .as_array()
        .ok_or_else(|| DataError::Parse("snow data is not an array".into()))?;
    let mut out = vec![];
    for st in stations {
        let entry = arr
            .iter()
            .find(|e| e.get("stationTriplet").and_then(Value::as_str) == Some(&st.triplet));
        let latest = |code: &str| -> Option<(f64, String)> {
            let data = entry?.get("data")?.as_array()?;
            let el = data.iter().find(|d| {
                d.pointer("/stationElement/elementCode")
                    .and_then(Value::as_str)
                    == Some(code)
            })?;
            el.get("values")?.as_array()?.iter().rev().find_map(|x| {
                Some((
                    x.get("value")?.as_f64()?,
                    x.get("date")?.as_str()?.to_string(),
                ))
            })
        };
        let swe = latest("WTEQ");
        let depth = latest("SNWD");
        let mut lines = vec![];
        if let Some((v, d)) = &swe {
            lines.push(format!("Snow water equivalent {v:.1} in ({d})"));
        }
        if let Some((v, _)) = &depth {
            lines.push(format!("Snow depth {v:.0} in"));
        }
        if swe.is_none() && depth.is_none() {
            lines.push("No recent snow report".into());
        }
        lines.push(format!("Elevation {:.0} ft", st.elev_ft));
        out.push(ExtFeature {
            geometry: Geometry::Marker(st.pos),
            style: swe_style(swe.as_ref().map(|s| s.0)),
            title: format!("{} SNOTEL, {}", st.name, st.state),
            lines,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- client

pub struct ExtClient {
    http: reqwest::blocking::Client,
    cache_dir: PathBuf,
    stations: Option<Vec<Station>>,
}

impl ExtClient {
    pub fn new(cache_dir: PathBuf) -> Result<Self, DataError> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(40))
            .build()
            .map_err(net)?;
        Ok(Self {
            http,
            cache_dir,
            stations: None,
        })
    }

    fn get_text(&self, url: &str, query: &[(&str, String)]) -> Result<String, DataError> {
        let r = self.http.get(url).query(query).send().map_err(net)?;
        if !r.status().is_success() {
            return Err(DataError::Status(r.status().as_u16()));
        }
        r.text().map_err(net)
    }

    /// Fire perimeters intersecting `bbox` ([west, south, east, north]).
    pub fn fire_history(
        &self,
        bbox: [f64; 4],
        now_unix: u64,
    ) -> Result<Vec<ExtFeature>, DataError> {
        // ~1/800 of the view width keeps shapes recognisable without megabytes of vertices.
        let tol = ((bbox[2] - bbox[0]).abs() / 800.0).max(0.00005);
        let text = self.get_text(NIFC_QUERY, &fire_history_query(bbox, tol))?;
        parse_fire_history(&text, year_of(now_unix))
    }

    fn load_stations(&mut self) -> Result<&[Station], DataError> {
        if self.stations.is_none() {
            let path = self.cache_dir.join("snotel_stations.json");
            let fresh = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < STATION_MAX_AGE_SECS);
            let cached = if fresh {
                std::fs::read_to_string(&path).ok()
            } else {
                None
            };
            let text = match cached {
                Some(t) => t,
                None => match self.get_text(
                    &format!("{AWDB}/stations"),
                    &[("activeOnly", "true".into())],
                ) {
                    Ok(t) => {
                        let _ = std::fs::create_dir_all(&self.cache_dir);
                        let _ = std::fs::write(&path, &t);
                        t
                    }
                    // Offline or the service is down: an old copy beats nothing.
                    Err(e) => std::fs::read_to_string(&path).map_err(|_| e)?,
                },
            };
            self.stations = Some(parse_stations(&text)?);
        }
        Ok(self.stations.as_deref().unwrap_or(&[]))
    }

    /// Latest readings for the SNOTEL sites inside `bbox`. Capped so a continent-wide view
    /// doesn't send an enormous request.
    pub fn snow_gauges(
        &mut self,
        bbox: [f64; 4],
        now_unix: u64,
    ) -> Result<Vec<ExtFeature>, DataError> {
        const MAX_STATIONS: usize = 150;
        let all = self.load_stations()?.to_vec();
        let mut inside: Vec<&Station> = stations_in(&all, bbox);
        inside.truncate(MAX_STATIONS);
        if inside.is_empty() {
            return Ok(vec![]);
        }
        let triplets = inside
            .iter()
            .map(|s| s.triplet.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let query = [
            ("stationTriplets", triplets),
            ("elements", "WTEQ,SNWD".into()),
            ("duration", "DAILY".into()),
            ("beginDate", iso_date(now_unix.saturating_sub(4 * 86_400))),
            ("endDate", iso_date(now_unix)),
            ("periodRef", "END".into()),
            ("returnFlags", "false".into()),
        ];
        let text = self.get_text(&format!("{AWDB}/data"), &query)?;
        parse_snotel_data(&text, &inside)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIFC: &str = include_str!("../tests/fixtures/nifc_history.json");
    const STATIONS: &str = include_str!("../tests/fixtures/snotel_stations.json");
    const DATA: &str = include_str!("../tests/fixtures/snotel_data.json");

    #[test]
    fn dates_match_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(iso_date(1_770_076_800), "2026-02-03");
        assert_eq!(year_of(1_770_076_800), 2026);
        // The day before a leap day and the end of a year.
        assert_eq!(iso_date(951_782_400), "2000-02-29");
        assert_eq!(iso_date(1_767_139_200), "2025-12-31");
    }

    #[test]
    fn parses_real_nifc_perimeters() {
        let f = parse_fire_history(NIFC, 2026).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].title, "1745 (2009)");
        assert!(
            f[0].lines
                .iter()
                .any(|l| l == "0 acres" || l.ends_with("acres"))
        );
        assert!(f[0].lines.iter().any(|l| l == "Agency: USFS"));
        let Geometry::Polygon(p) = &f[0].geometry else {
            panic!()
        };
        // The service closes rings; ours are open.
        assert_ne!(p.first(), p.last());
        assert_eq!(p.len(), 3);
        // A 17-year-old burn is toned down compared with last year's.
        assert!(f[0].style.color != history_style(1).color);
    }

    #[test]
    fn multipolygons_and_errors() {
        let mp = r#"{"features":[{"properties":{"INCIDENT":"","FIRE_YEAR":"2025","GIS_ACRES":5.0,"AGENCY":""},
            "geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]],[[[5,5],[6,5],[6,6],[5,5]]]]}}]}"#;
        let f = parse_fire_history(mp, 2026).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].title, "Unnamed fire (2025)");
        assert!(parse_fire_history(r#"{"error":{"code":400}}"#, 2026).is_err());
        assert!(parse_fire_history("nope", 2026).is_err());
        // Degenerate rings are dropped rather than drawn.
        let bad = r#"{"features":[{"properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,1]]]}}]}"#;
        assert!(parse_fire_history(bad, 2026).unwrap().is_empty());
    }

    #[test]
    fn query_asks_for_the_view_box_in_degrees() {
        let q = fire_history_query([-92.5, 37.0, -91.0, 38.0], 0.002);
        let get = |k: &str| q.iter().find(|(n, _)| *n == k).unwrap().1.clone();
        assert_eq!(get("geometry"), "-92.5,37,-91,38");
        assert_eq!(get("inSR"), "4326");
        assert_eq!(get("f"), "geojson");
    }

    #[test]
    fn stations_filter_to_automated_sntl_only() {
        let st = parse_stations(STATIONS).unwrap();
        // The fixture holds two SNTL sites and one snow course (SNOW), which is excluded.
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|s| s.triplet.ends_with(":SNTL")));
        let co = stations_in(&st, [-108.0, 38.5, -106.5, 39.5]);
        assert_eq!(co.len(), 1);
        assert_eq!(co[0].name, "North Lost Trail");
        assert!(stations_in(&st, [-92.5, 37.0, -91.0, 38.0]).is_empty());
    }

    #[test]
    fn latest_snow_reading_is_used() {
        let st = parse_stations(STATIONS).unwrap();
        let refs: Vec<&Station> = st.iter().collect();
        let f = parse_snotel_data(DATA, &refs).unwrap();
        assert_eq!(f.len(), 2);
        let nlt = f
            .iter()
            .find(|x| x.title.starts_with("North Lost Trail"))
            .unwrap();
        assert!(
            nlt.lines[0].contains("6.0 in (2026-02-03)"),
            "{:?}",
            nlt.lines
        );
        assert!(nlt.lines.iter().any(|l| l == "Snow depth 24 in"));
        // 6 in of water falls in the 3-10 band.
        assert_eq!(nlt.style, swe_style(Some(6.0)));
        // A station missing from the reply still appears, marked as having no report.
        let only = parse_snotel_data("[]", &refs[..1]).unwrap();
        assert!(
            only[0]
                .lines
                .iter()
                .any(|l| l.contains("No recent snow report"))
        );
        assert!(parse_snotel_data("{}", &refs).is_err());
    }

    #[test]
    fn swe_bands_are_monotonic_in_depth() {
        let c = |v| swe_style(Some(v)).color;
        assert_ne!(c(0.0), c(1.0));
        assert_ne!(c(1.0), c(5.0));
        assert_ne!(c(5.0), c(15.0));
        assert_ne!(c(15.0), c(30.0));
        assert_eq!(swe_style(None).fill_alpha, 255);
    }
}
