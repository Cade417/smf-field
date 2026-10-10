//! NMEA 0183 parsing: GGA (position, altitude, satellites, HDOP) and RMC (position, speed,
//! course, date and time). Lines with a bad checksum are ignored, never guessed at.

use crate::{Event, Fix};

/// True when the `*HH` checksum matches (sentences without one are accepted: some gpsd
/// relays and loggers strip it, but a present-and-wrong checksum is corruption).
pub fn checksum_ok(line: &str) -> bool {
    let line = line.trim();
    let Some(body) = line.strip_prefix('$') else {
        return false;
    };
    match body.split_once('*') {
        None => true,
        Some((data, sum)) => {
            let calc = data.bytes().fold(0u8, |a, b| a ^ b);
            u8::from_str_radix(sum.trim(), 16).is_ok_and(|s| s == calc)
        }
    }
}

/// "ddmm.mmmm" or "dddmm.mmmm" plus hemisphere to signed degrees.
pub fn parse_latlon(value: &str, hemi: &str) -> Option<f64> {
    let dot = value.find('.').unwrap_or(value.len());
    if dot < 3 {
        return None;
    }
    let (deg, min) = value.split_at(dot - 2);
    let d: f64 = deg.parse().ok()?;
    let m: f64 = min.parse().ok()?;
    if !(0.0..60.0).contains(&m) {
        return None;
    }
    let v = d + m / 60.0;
    match hemi {
        "N" | "E" => Some(v),
        "S" | "W" => Some(-v),
        _ => None,
    }
}

/// Days since 1970-01-01 for a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "hhmmss(.ss)" and "ddmmyy" to unix seconds (two-digit year pivoted at 1980).
fn rmc_time(time: &str, date: &str) -> Option<u64> {
    if time.len() < 6 || date.len() != 6 {
        return None;
    }
    let n = |s: &str| s.parse::<i64>().ok();
    let (h, mi, s) = (n(&time[0..2])?, n(&time[2..4])?, n(&time[4..6])?);
    let (d, mo, yy) = (n(&date[0..2])?, n(&date[2..4])?, n(&date[4..6])?);
    // Two-digit years: GPS began in 1980, so 80-99 are the 1900s and 00-79 the 2000s.
    let y = if yy >= 80 { 1900 + yy } else { 2000 + yy };
    if !(1..=31).contains(&d) || !(1..=12).contains(&mo) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(t).ok()
}

/// Remembers the latest GGA so an RMC fix can carry altitude, satellites and HDOP too.
#[derive(Default)]
pub struct NmeaParser {
    alt_m: Option<f64>,
    sats: Option<u32>,
    hdop: Option<f64>,
}

impl NmeaParser {
    /// Feed one line; returns an event when it carried a position (or said there is none).
    pub fn feed(&mut self, line: &str) -> Option<Event> {
        let line = line.trim();
        if !line.starts_with('$') || !checksum_ok(line) {
            return None;
        }
        let body = line[1..].split('*').next()?;
        let f: Vec<&str> = body.split(',').collect();
        // Talker id is two letters (GP, GN, GL...); the type is the last three.
        let kind = f.first().filter(|k| k.len() == 5).map(|k| &k[2..])?;
        match kind {
            "GGA" if f.len() >= 10 => {
                let quality: u32 = f[6].parse().unwrap_or(0);
                self.sats = f[7].parse().ok();
                self.hdop = f[8].parse().ok();
                self.alt_m = f[9].parse().ok();
                if quality == 0 {
                    return Some(Event::NoFix);
                }
                Some(Event::Fix(Fix {
                    lat: parse_latlon(f[2], f[3])?,
                    lng: parse_latlon(f[4], f[5])?,
                    alt_m: self.alt_m,
                    satellites: self.sats,
                    hdop: self.hdop,
                    // About 5 m of error per unit of HDOP is the usual consumer-receiver rule of thumb.
                    accuracy_m: self.hdop.map(|h| h * 5.0),
                    ..Fix::default()
                }))
            }
            "RMC" if f.len() >= 10 => {
                if f[2] != "A" {
                    return Some(Event::NoFix);
                }
                Some(Event::Fix(Fix {
                    lat: parse_latlon(f[3], f[4])?,
                    lng: parse_latlon(f[5], f[6])?,
                    speed_mps: f[7].parse::<f64>().ok().map(|kn| kn * 0.514_444),
                    course_deg: f[8].parse().ok(),
                    time_unix: rmc_time(f[1], f[9]),
                    alt_m: self.alt_m,
                    satellites: self.sats,
                    hdop: self.hdop,
                    accuracy_m: self.hdop.map(|h| h * 5.0),
                }))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sample sentences from the NMEA 0183 reference documentation.
    const GGA: &str = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47";
    const RMC: &str = "$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A";

    #[test]
    fn checksums() {
        assert!(checksum_ok(GGA));
        assert!(checksum_ok(RMC));
        assert!(
            !checksum_ok(&GGA.replace("545.4", "545.5")),
            "corruption is caught"
        );
        assert!(!checksum_ok("GPGGA,no,dollar*00"));
        assert!(
            checksum_ok("$GPGLL,4916.45,N,12311.12,W,225444,A"),
            "no checksum accepted"
        );
        assert!(!checksum_ok("$GPGGA,1*ZZ"));
    }

    #[test]
    fn coordinates_with_hemispheres() {
        let lat = parse_latlon("4807.038", "N").unwrap();
        assert!((lat - 48.1173).abs() < 1e-6, "{lat}");
        let lng = parse_latlon("01131.000", "E").unwrap();
        assert!((lng - 11.516_666_7).abs() < 1e-6, "{lng}");
        assert!(parse_latlon("4807.038", "S").unwrap() < 0.0);
        assert!(parse_latlon("09215.000", "W").unwrap() < -92.0);
        for bad in [
            ("", "N"),
            ("48", "N"),
            ("4899.0", "N"),
            ("4807.038", "X"),
            ("abcd.ef", "N"),
        ] {
            assert!(parse_latlon(bad.0, bad.1).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn gga_then_rmc_merge_into_one_fix() {
        let mut p = NmeaParser::default();
        let Some(Event::Fix(g)) = p.feed(GGA) else {
            panic!()
        };
        assert_eq!(g.satellites, Some(8));
        assert_eq!(g.alt_m, Some(545.4));
        assert!((g.accuracy_m.unwrap() - 4.5).abs() < 1e-9);
        let Some(Event::Fix(r)) = p.feed(RMC) else {
            panic!()
        };
        assert!((r.speed_mps.unwrap() - 11.52).abs() < 0.01, "22.4 knots");
        assert_eq!(r.course_deg, Some(84.4));
        // 1994-03-23 12:35:19 UTC
        assert_eq!(r.time_unix, Some(764_426_119));
        // Altitude and satellites carry over from the GGA that came before.
        assert_eq!(r.alt_m, Some(545.4));
        assert_eq!(r.satellites, Some(8));
    }

    #[test]
    fn no_fix_and_junk_are_handled() {
        let mut p = NmeaParser::default();
        // Status V (void) means the receiver has no position yet.
        assert_eq!(p.feed("$GPRMC,123519,V,,,,,,,230394,,"), Some(Event::NoFix));
        assert_eq!(
            p.feed("$GPGGA,123519,,,,,0,00,,,M,,M,,"),
            Some(Event::NoFix)
        );
        assert_eq!(p.feed("garbage"), None);
        assert_eq!(
            p.feed("$GPGSV,3,1,11,03,03,111,00*74"),
            None,
            "other sentence types are skipped"
        );
        assert_eq!(p.feed(&GGA.replace("545.4", "545.9")), None, "bad checksum");
        // GN (multi-constellation) talkers are accepted.
        let gn = "$GNRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W";
        assert!(matches!(p.feed(gn), Some(Event::Fix(_))));
    }
}
