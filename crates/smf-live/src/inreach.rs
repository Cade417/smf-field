//! Garmin inReach MapShare KML feed parsing. Fields are read by their ExtendedData names
//! (Latitude, Longitude, Time UTC, ...) rather than by Point geometry, because that is what
//! the feed documents.

use crate::{Network, Report};

const SHARE: &str = "https://inreach.garmin.com/feed/share/";

/// Feed URL for a MapShare identifier, asking for points since `since_unix` (the feed shows
/// only the latest point without a start date).
pub fn mapshare_url(id: &str, since_unix: Option<u64>) -> Result<String, String> {
    let id = id.trim();
    // Only the MapShare name is accepted, so a pasted full URL can't point the app at another host.
    let id = id.rsplit('/').next().unwrap_or(id);
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("A MapShare name contains only letters, digits, '.', '-' and '_'.".into());
    }
    let mut u = format!("{SHARE}{id}");
    if let Some(t) = since_unix {
        let d = smf_ext::iso_date(t);
        let secs = t % 86_400;
        u.push_str(&format!(
            "?d1={d}T{:02}:{:02}z",
            secs / 3600,
            secs % 3600 / 60
        ));
    }
    Ok(u)
}

/// "5/2/2020 6:01:30 AM" (month/day/year, 12-hour clock) to unix seconds, UTC.
pub fn parse_us_datetime(s: &str) -> Option<u64> {
    let mut it = s.split_whitespace();
    let (date, time) = (it.next()?, it.next()?);
    let ampm = it.next();
    let d: Vec<i64> = date.split('/').filter_map(|x| x.parse().ok()).collect();
    let t: Vec<i64> = time.split(':').filter_map(|x| x.parse().ok()).collect();
    let [mo, day, y] = d[..] else { return None };
    let [mut h, mi, sec] = t[..] else { return None };
    match ampm {
        Some("PM") if h < 12 => h += 12,
        Some("AM") if h == 12 => h = 0,
        Some("AM") | Some("PM") | None => {}
        _ => return None,
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from civil (proleptic Gregorian).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy.rem_euclid(400);
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec).ok()
}

/// First number in text like "1.0 km/h", "292.50° True" or "120.39 m from MSL".
fn leading_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let end = t
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || *c == '.' || (*c == '-' && *i == 0)))
        .map_or(t.len(), |(i, _)| i);
    t[..end].parse().ok()
}

pub fn parse_mapshare(kml: &str) -> Result<Vec<Report>, String> {
    let doc = roxmltree::Document::parse(kml).map_err(|e| format!("not a KML feed ({e})"))?;
    let mut out = vec![];
    for pm in doc.descendants().filter(|n| n.has_tag_name("Placemark")) {
        let data = |name: &str| -> Option<String> {
            pm.descendants()
                .filter(|n| n.has_tag_name("Data"))
                .find(|n| n.attribute("name") == Some(name))
                .and_then(|n| n.children().find(|c| c.has_tag_name("value")))
                .and_then(|v| v.text())
                .map(|t| t.trim().to_string())
        };
        let (Some(lat), Some(lng)) = (
            data("Latitude").and_then(|v| v.parse::<f64>().ok()),
            data("Longitude").and_then(|v| v.parse::<f64>().ok()),
        ) else {
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
            continue;
        }
        // Without a valid GPS fix the position is the last known one, not where the person is.
        if data("Valid GPS Fix").is_some_and(|v| v.eq_ignore_ascii_case("false")) {
            continue;
        }
        let name = data("Map Display Name")
            .filter(|n| !n.is_empty())
            .or_else(|| data("Name").filter(|n| !n.is_empty()))
            .unwrap_or_else(|| "inReach".into());
        let id = data("IMEI")
            .or_else(|| data("Name"))
            .unwrap_or_else(|| name.clone());
        out.push(Report {
            network: Network::InReach,
            id,
            name,
            lat,
            lng,
            time_unix: data("Time UTC").and_then(|t| parse_us_datetime(&t)),
            speed_mps: data("Velocity")
                .and_then(|v| leading_number(&v))
                .map(|kmh| kmh / 3.6),
            course_deg: data("Course").and_then(|v| leading_number(&v)),
            alt_m: data("Elevation").and_then(|v| leading_number(&v)),
            text: data("Text").unwrap_or_default(),
            emergency: data("In Emergency").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built from the documented ExtendedData field names and example values; not a live capture.
    fn placemark(
        time: &str,
        lat: &str,
        lng: &str,
        fix: &str,
        emergency: &str,
        text: &str,
    ) -> String {
        format!(
            r#"<Placemark><name>Joe the inReach User</name><TimeStamp><when>2020-05-02T06:01:30Z</when></TimeStamp>
<ExtendedData xmlns:gx="x">
<Data name="Id"><value>868926</value></Data>
<Data name="Time UTC"><value>{time}</value></Data>
<Data name="Name"><value>Joe User</value></Data>
<Data name="Map Display Name"><value>Joe the inReach User</value></Data>
<Data name="Device Type"><value>inReach 2.5</value></Data>
<Data name="IMEI"><value>300000000000000</value></Data>
<Data name="Latitude"><value>{lat}</value></Data>
<Data name="Longitude"><value>{lng}</value></Data>
<Data name="Elevation"><value>120.39 m from MSL</value></Data>
<Data name="Velocity"><value>36.0 km/h</value></Data>
<Data name="Course"><value>292.50&#176; True</value></Data>
<Data name="Valid GPS Fix"><value>{fix}</value></Data>
<Data name="In Emergency"><value>{emergency}</value></Data>
<Data name="Text"><value>{text}</value></Data>
<Data name="Event"><value>Tracking message received</value></Data>
</ExtendedData><Point><coordinates>{lng},{lat},120.39</coordinates></Point></Placemark>"#
        )
    }

    fn feed(parts: &[String]) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><kml xmlns="http://www.opengis.net/kml/2.2"><Document>{}</Document></kml>"#,
            parts.join("")
        )
    }

    #[test]
    fn parses_documented_fields_and_units() {
        let kml = feed(&[placemark(
            "5/2/2020 6:01:30 AM",
            "43.790485",
            "-70.192015",
            "True",
            "False",
            "I am doing good!",
        )]);
        let r = parse_mapshare(&kml).unwrap();
        assert_eq!(r.len(), 1);
        let p = &r[0];
        assert_eq!((p.lat, p.lng), (43.790485, -70.192015));
        assert_eq!(p.name, "Joe the inReach User");
        assert_eq!(p.id, "300000000000000");
        assert_eq!(p.text, "I am doing good!");
        assert!(
            (p.speed_mps.unwrap() - 10.0).abs() < 1e-9,
            "36 km/h is 10 m/s"
        );
        assert_eq!(p.course_deg, Some(292.5));
        assert_eq!(p.alt_m, Some(120.39));
        assert_eq!(p.time_unix, Some(1_588_399_290)); // 2020-05-02 06:01:30 UTC
        assert!(!p.emergency);
    }

    #[test]
    fn skips_bad_fixes_and_flags_emergencies() {
        let kml = feed(&[
            placemark(
                "5/2/2020 6:01:30 AM",
                "43.0",
                "-70.0",
                "False",
                "False",
                "stale position",
            ),
            placemark(
                "5/2/2020 6:11:30 AM",
                "95.0",
                "-70.0",
                "True",
                "False",
                "impossible latitude",
            ),
            placemark(
                "5/2/2020 6:21:30 PM",
                "43.5",
                "-70.5",
                "True",
                "True",
                "SOS",
            ),
        ]);
        let r = parse_mapshare(&kml).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].emergency);
        assert_eq!(r[0].time_unix, Some(1_588_399_290 + 12 * 3600 + 20 * 60)); // 6:21:30 PM
        // A login page that happens to be XML has no placemarks: empty, not an error and not junk.
        assert!(parse_mapshare("<html>sign in</html>").unwrap().is_empty());
        assert!(parse_mapshare("not xml <").is_err());
    }

    #[test]
    fn us_datetimes() {
        assert_eq!(
            parse_us_datetime("5/2/2020 6:01:30 AM"),
            Some(1_588_399_290)
        );
        assert_eq!(
            parse_us_datetime("5/2/2020 12:00:00 AM"),
            Some(1_588_377_600)
        );
        assert_eq!(
            parse_us_datetime("5/2/2020 12:00:00 PM"),
            Some(1_588_377_600 + 12 * 3600)
        );
        assert_eq!(parse_us_datetime("13/2/2020 6:01:30 AM"), None);
        assert_eq!(parse_us_datetime("garbage"), None);
    }

    #[test]
    fn urls_are_built_only_from_the_mapshare_name() {
        assert_eq!(
            mapshare_url("BritinusaG1", None).unwrap(),
            "https://inreach.garmin.com/feed/share/BritinusaG1"
        );
        // A pasted link keeps only its last path segment and still goes to Garmin.
        assert_eq!(
            mapshare_url("https://share.garmin.com/BritinusaG1", None).unwrap(),
            "https://inreach.garmin.com/feed/share/BritinusaG1"
        );
        assert_eq!(
            mapshare_url("abc", Some(1_588_399_290)).unwrap(),
            "https://inreach.garmin.com/feed/share/abc?d1=2020-05-02T06:01z"
        );
        for bad in ["", "a b", "x?evil=1", "a@evil.com"] {
            assert!(mapshare_url(bad, None).is_err(), "{bad:?}");
        }
    }
}
