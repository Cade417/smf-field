//! Raster layers (fire danger, weather, fuel moisture...), forecast runs, and point conditions.

use crate::layers::DataError;
use serde_json::Value;
use smf_tiles::{Backend, SourceKind, TileSource};

/// RTMA hourly layers update about once an hour; refetch cached tiles a bit before that.
const RASTER_MAX_AGE_SECS: u64 = 15 * 60;
/// The raster server has data to about this zoom; deeper levels are overzoomed.
const RASTER_MAX_ZOOM: u8 = 11;

#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub value: f64,
    pub color: [u8; 3],
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RasterLayer {
    pub id: String,
    pub label: String,
    pub units: String,
    pub filename: String,
    pub colormap: String,
    pub rescale: [f64; 2],
    pub group: String,
    pub available: bool,
    pub observed_at: Option<String>,
    pub legend: Vec<LegendEntry>,
}

fn hex(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    (h.len() == 6)
        .then(|| u32::from_str_radix(h, 16).ok())
        .flatten()
        .map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

pub fn parse_catalog(text: &str) -> Result<Vec<RasterLayer>, DataError> {
    let root: Value = serde_json::from_str(text).map_err(|e| DataError::Parse(e.to_string()))?;
    let items = root
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| DataError::Parse("no \"layers\" list".into()))?;
    let mut out = Vec::new();
    for l in items {
        let s = |k: &str| l.get(k).and_then(Value::as_str).map(str::to_owned);
        let (Some(id), Some(filename)) = (s("id"), s("filename")) else {
            continue;
        };
        let rescale = l
            .get("rescale")
            .and_then(Value::as_array)
            .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
            .unwrap_or([0.0, 1.0]);
        let legend = l
            .get("legend")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some(LegendEntry {
                            value: e.get("value")?.as_f64()?,
                            color: hex(e.get("color")?.as_str()?)?,
                            label: e.get("label").and_then(Value::as_str).map(str::to_owned),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(RasterLayer {
            label: s("label").unwrap_or_else(|| id.clone()),
            units: s("units").unwrap_or_default(),
            colormap: s("colormap").unwrap_or_else(|| "viridis".into()),
            group: s("group").unwrap_or_default(),
            available: l.get("available").and_then(Value::as_bool).unwrap_or(true),
            observed_at: s("observed_at"),
            id,
            filename,
            rescale,
            legend,
        });
    }
    Ok(out)
}

/// Percent-encode a query value the way URLSearchParams does (so `/` becomes %2F), keeping `{}` out of it.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn rescale_text(r: [f64; 2]) -> String {
    format!("{},{}", r[0], r[1])
}

impl RasterLayer {
    /// The layer as a drawable overlay. Tiles expire after 15 minutes so new observations show up.
    pub fn tile_source(&self, base: &str) -> TileSource {
        let mut q = format!(
            "filename={}&colormap={}&rescale={}&mask=missouri",
            enc(&self.filename),
            enc(&self.colormap),
            enc(&rescale_text(self.rescale))
        );
        if let Some(v) = &self.observed_at {
            q.push_str(&format!("&v={}", enc(v)));
        }
        TileSource {
            kind: SourceKind::Raster,
            backend: Backend::Http,
            overlay: true,
            id: format!("smf-raster-{}", self.id),
            name: format!("{} (Show Me Fire)", self.label),
            url: format!(
                "{}/tiles/cog/tiles/{{z}}/{{x}}/{{y}}.png?{q}",
                base.trim_end_matches('/')
            ),
            subdomains: vec![],
            max_zoom: RASTER_MAX_ZOOM,
            attribution: "Show Me Fire (NOAA RTMA and station analysis)".into(),
            // Derived live products: not for bulk download.
            offline_allowed: false,
            max_age_secs: Some(RASTER_MAX_AGE_SECS),
        }
    }
}

// ------------------------------------------------------------------ forecast

#[derive(Debug, Clone, PartialEq)]
pub struct ForecastLayer {
    pub variable: String,
    pub unit: String,
    pub colormap: String,
    pub rescale: [f64; 2],
    /// e.g. /tiles/forecast/<run>/<variable>/{lead_hour}/{z}/{x}/{y}.png
    pub tile_template: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForecastRun {
    pub run_id: String,
    pub cycle_time: String,
    pub horizon_hours: u32,
    pub layers: Vec<ForecastLayer>,
}

pub fn parse_forecast_run(text: &str) -> Result<ForecastRun, DataError> {
    let root: Value = serde_json::from_str(text).map_err(|e| DataError::Parse(e.to_string()))?;
    let get = |k: &str| root.get(k).and_then(Value::as_str).map(str::to_owned);
    let (Some(run_id), Some(cycle_time)) = (get("runId"), get("cycleTime")) else {
        return Err(DataError::Parse(
            "forecast manifest has no runId/cycleTime".into(),
        ));
    };
    let horizon = root
        .get("horizonHours")
        .and_then(Value::as_u64)
        .unwrap_or(72)
        .min(240) as u32;
    let layers = root
        .get("layers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    // Only hourly layers with a per-hour tile URL can be stepped through with a slider.
                    let tpl = l.get("tileUrl")?.as_str()?.to_owned();
                    if !tpl.contains("{lead_hour}") || !tpl.starts_with('/') {
                        return None;
                    }
                    let style = l.get("style")?;
                    Some(ForecastLayer {
                        variable: l.get("variable")?.as_str()?.to_owned(),
                        unit: l
                            .get("unit")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        colormap: style.get("colormap")?.as_str()?.to_owned(),
                        rescale: {
                            let r = style.get("rescale")?.as_array()?;
                            [r.first()?.as_f64()?, r.get(1)?.as_f64()?]
                        },
                        tile_template: tpl,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ForecastRun {
        run_id,
        cycle_time,
        horizon_hours: horizon,
        layers,
    })
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// "2026-10-10T06:00:00Z" -> unix seconds.
fn parse_utc(s: &str) -> Option<i64> {
    let (date, time) = s.trim().trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time
        .split(':')
        .map(|x| x.split('.').next().and_then(|v| v.parse::<i64>().ok()));
    let (hh, mm, ss) = (
        t.next()??,
        t.next().flatten().unwrap_or(0),
        t.next().flatten().unwrap_or(0),
    );
    ((1..=12).contains(&m) && (1..=31).contains(&day) && hh < 24)
        .then(|| days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

impl ForecastRun {
    pub fn unix_valid_time(&self, lead_hour: u32) -> Option<i64> {
        parse_utc(&self.cycle_time).map(|t| t + lead_hour as i64 * 3600)
    }

    /// "Oct 10 14:00 UTC (+8 h)" for the slider.
    pub fn valid_time_label(&self, lead_hour: u32) -> String {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        match self.unix_valid_time(lead_hour) {
            Some(t) => {
                let (_, m, d) = civil_from_days(t.div_euclid(86_400));
                let secs = t.rem_euclid(86_400);
                format!(
                    "{} {d} {:02}:{:02} UTC (+{lead_hour} h)",
                    MONTHS[(m - 1) as usize],
                    secs / 3600,
                    secs % 3600 / 60
                )
            }
            None => format!("+{lead_hour} h"),
        }
    }

    /// A tile source for one variable at one lead hour. Forecast rasters are immutable, so no expiry;
    /// the id includes run, variable and hour so every step has its own cache.
    pub fn tile_source(&self, layer: &ForecastLayer, lead_hour: u32, base: &str) -> TileSource {
        let lead = lead_hour.min(self.horizon_hours);
        TileSource {
            kind: SourceKind::Raster,
            backend: Backend::Http,
            overlay: true,
            id: format!("smf-fc-{}-{}-{lead}", self.run_id, layer.variable),
            name: format!(
                "Forecast: {} ({})",
                layer.variable.replace('_', " "),
                self.valid_time_label(lead)
            ),
            url: format!(
                "{}{}?mask=missouri",
                base.trim_end_matches('/'),
                layer
                    .tile_template
                    .replace("{lead_hour}", &lead.to_string())
            ),
            subdomains: vec![],
            max_zoom: RASTER_MAX_ZOOM,
            attribution: "Show Me Fire 72-hour forecast".into(),
            offline_allowed: false,
            max_age_secs: None,
        }
    }
}

// ---------------------------------------------------------------- conditions

#[derive(Debug, Clone, PartialEq)]
pub struct Conditions {
    pub observed_at: Option<String>,
    pub fire_danger_label: Option<String>,
    /// (key, value) in display order: fire_danger, temperature, rh, wind, fuel_moisture, precipitation, spread_rate.
    pub values: Vec<(String, f64)>,
}

const ORDER: [&str; 7] = [
    "fire_danger",
    "temperature",
    "rh",
    "wind",
    "fuel_moisture",
    "precipitation",
    "spread_rate",
];

pub fn parse_conditions(text: &str) -> Result<Conditions, DataError> {
    let root: Value = serde_json::from_str(text).map_err(|e| DataError::Parse(e.to_string()))?;
    if root.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(DataError::Parse(
            "the server reported no data for this point".into(),
        ));
    }
    let vals = root
        .get("values")
        .and_then(Value::as_object)
        .ok_or_else(|| DataError::Parse("no values".into()))?;
    let mut values: Vec<(String, f64)> = ORDER
        .iter()
        .filter_map(|k| {
            vals.get(*k)
                .and_then(Value::as_f64)
                .map(|v| ((*k).to_owned(), v))
        })
        .collect();
    // Anything the server adds later still shows up, after the known ones.
    for (k, v) in vals {
        if !ORDER.contains(&k.as_str())
            && let Some(f) = v.as_f64()
        {
            values.push((k.clone(), f));
        }
    }
    Ok(Conditions {
        observed_at: root
            .get("observed_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        fire_danger_label: root
            .get("fire_danger_label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        values,
    })
}

impl Conditions {
    /// One-line summary using the catalog's labels and units. The point-query endpoint's own `units`
    /// field is not used: it comes back double-encoded ("Â°F").
    pub fn summary(&self, catalog: &[RasterLayer]) -> String {
        let mut parts = Vec::new();
        for (k, v) in &self.values {
            if k == "fire_danger" {
                parts.push(format!(
                    "Fire danger: {}",
                    self.fire_danger_label
                        .clone()
                        .unwrap_or_else(|| format!("{v:.0}"))
                ));
                continue;
            }
            let layer = catalog.iter().find(|l| l.id == *k);
            let label = layer
                .map(|l| l.label.clone())
                .unwrap_or_else(|| k.replace('_', " "));
            let units = layer.map(|l| l.units.as_str()).unwrap_or("");
            parts.push(format!(
                "{label}: {v:.0}{}{units}",
                if units.chars().next().is_some_and(char::is_alphabetic) {
                    " "
                } else {
                    ""
                }
            ));
        }
        parts.join("  |  ")
    }
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
    fn parses_the_live_catalog() {
        let c = parse_catalog(&fixture("catalog.json")).unwrap();
        assert_eq!(c.len(), 11);
        let fd = c.iter().find(|l| l.id == "fire_danger").unwrap();
        assert_eq!(
            (fd.label.as_str(), fd.colormap.as_str(), fd.rescale),
            ("Fire danger", "fire_danger", [0.0, 4.0])
        );
        assert_eq!(fd.legend.len(), 5);
        assert_eq!(fd.legend[0].color, [0x90, 0xee, 0x90]);
        assert_eq!(fd.legend[4].label.as_deref(), Some("Extreme"));
        assert!(fd.available && fd.observed_at.is_some());
        let temp = c.iter().find(|l| l.id == "temperature").unwrap();
        assert_eq!(temp.units, "\u{b0}F");
        assert_eq!(temp.legend.len(), 6);
        assert!(temp.legend.iter().all(|e| e.label.is_none()));
    }

    #[test]
    fn raster_tile_urls_match_what_the_website_sends() {
        let c = parse_catalog(&fixture("catalog.json")).unwrap();
        let s = c
            .iter()
            .find(|l| l.id == "fire_danger")
            .unwrap()
            .tile_source("https://api.showmefire.org/");
        assert_eq!(s.id, "smf-raster-fire_danger");
        assert!(s.overlay && !s.offline_allowed);
        assert_eq!(s.max_age_secs, Some(900));
        let u = s.url.clone();
        assert!(u.starts_with("https://api.showmefire.org/tiles/cog/tiles/{z}/{x}/{y}.png?filename=latest%2Frealtime_fire_danger.tif&colormap=fire_danger&rescale=0%2C4&mask=missouri&v="), "{u}");
        // The cache-busting version carries the observation time.
        assert!(u.contains("&v=2026-10-10T01%3A00%3A00Z"), "{u}");
        let t = smf_geo::TileId::new(8, 62, 98);
        assert!(s.tile_url(t).contains("/tiles/cog/tiles/8/62/98.png?"));
    }

    #[test]
    fn catalog_errors_and_partial_entries() {
        assert!(parse_catalog("nope").is_err());
        assert!(parse_catalog("{}").is_err());
        let c = parse_catalog(r#"{"layers":[{"id":"a"},{"id":"b","filename":"x.tif"}]}"#).unwrap();
        assert_eq!(c.len(), 1, "entries without a filename are skipped");
        assert_eq!(c[0].colormap, "viridis");
    }

    #[test]
    fn forecast_manifest_gives_stepping_layers_only() {
        let r = parse_forecast_run(&fixture("forecast_manifest.json")).unwrap();
        assert_eq!(
            (r.run_id.as_str(), r.horizon_hours),
            ("20261010T0600Z-v1", 72)
        );
        // The accumulated-precipitation layer has no per-hour tiles, so it isn't offered.
        assert_eq!(r.layers.len(), 2);
        assert_eq!(r.layers[0].variable, "fire_danger");
        assert!(parse_forecast_run("{}").is_err());
    }

    #[test]
    fn forecast_valid_times_and_tile_sources() {
        let r = parse_forecast_run(&fixture("forecast_manifest.json")).unwrap();
        assert_eq!(r.valid_time_label(0), "Oct 10 06:00 UTC (+0 h)");
        assert_eq!(r.valid_time_label(8), "Oct 10 14:00 UTC (+8 h)");
        assert_eq!(r.valid_time_label(24), "Oct 11 06:00 UTC (+24 h)");
        assert_eq!(r.valid_time_label(72), "Oct 13 06:00 UTC (+72 h)");
        // Crossing a month and a year boundary.
        let nye = ForecastRun {
            cycle_time: "2026-12-31T18:00:00Z".into(),
            ..r.clone()
        };
        assert_eq!(nye.valid_time_label(12), "Jan 1 06:00 UTC (+12 h)");
        let leap = ForecastRun {
            cycle_time: "2028-02-28T12:00:00Z".into(),
            ..r.clone()
        };
        assert_eq!(leap.valid_time_label(24), "Feb 29 12:00 UTC (+24 h)");
        let s = r.tile_source(&r.layers[0], 8, "https://api.showmefire.org");
        assert_eq!(s.id, "smf-fc-20261010T0600Z-v1-fire_danger-8");
        assert_eq!(
            s.url,
            "https://api.showmefire.org/tiles/forecast/20261010T0600Z-v1/fire_danger/8/{z}/{x}/{y}.png?mask=missouri"
        );
        assert_eq!(s.max_age_secs, None, "forecast rasters are immutable");
        // Lead hours beyond the horizon are clamped (the server rejects > 72).
        assert!(
            r.tile_source(&r.layers[0], 500, "https://x")
                .url
                .contains("/fire_danger/72/")
        );
        assert_ne!(
            s.id,
            r.tile_source(&r.layers[0], 9, "https://x").id,
            "each hour caches separately"
        );
    }

    #[test]
    fn conditions_parse_and_summarize_with_catalog_units() {
        let c = parse_conditions(&fixture("conditions_at.json")).unwrap();
        assert_eq!(c.fire_danger_label.as_deref(), Some("Low"));
        assert_eq!(
            c.values.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ORDER
        );
        let cat = parse_catalog(&fixture("catalog.json")).unwrap();
        let s = c.summary(&cat);
        assert!(
            s.starts_with("Fire danger: Low  |  Temperature: 74\u{b0}F"),
            "{s}"
        );
        assert!(
            s.contains("Relative humidity: 59%") && s.contains("Wind speed: 4 kt"),
            "{s}"
        );
        assert!(
            !s.contains('\u{c2}'),
            "no mojibake from the endpoint's own units: {s}"
        );
        assert!(parse_conditions(r#"{"success":false}"#).is_err());
    }

    #[test]
    fn parse_utc_rejects_garbage() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2000-02-29T12:30:00Z"), Some(951_827_400));
        for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-10-10"] {
            assert_eq!(parse_utc(bad), None, "{bad:?}");
        }
    }
}
