//! APRS packet parsing for position reports: plain (uncompressed) and compressed formats,
//! with or without a timestamp. Mic-E packets (used by many mobile trackers) and objects are
//! recognised but not decoded; they return None.

use crate::{Network, Report};

fn base91(s: &[u8]) -> Option<f64> {
    let mut v = 0f64;
    for &c in s {
        if !(33..=124).contains(&c) {
            return None;
        }
        v = v * 91.0 + (c - 33) as f64;
    }
    Some(v)
}

/// "DDMM.mm" + hemisphere letter as signed degrees. APRS allows spaces for position ambiguity.
fn ddm(field: &str, deg_digits: usize) -> Option<f64> {
    let (num, hemi) = field.split_at(field.len().checked_sub(1)?);
    if num.len() < deg_digits + 3 || !num.is_char_boundary(deg_digits) {
        return None;
    }
    let num = num.replace(' ', "0");
    let d: f64 = num[..deg_digits].parse().ok()?;
    let m: f64 = num[deg_digits..].parse().ok()?;
    if m >= 60.0 {
        return None;
    }
    let v = d + m / 60.0;
    match hemi {
        "N" | "E" => Some(v),
        "S" | "W" => Some(-v),
        _ => None,
    }
}

/// "CCC/SSS" (degrees / knots) at the start of a comment, as sent by moving stations.
fn course_speed(comment: &str) -> (Option<f64>, Option<f64>, &str) {
    let b = comment.as_bytes();
    if b.len() >= 7 && b[3] == b'/' && b[..3].iter().chain(&b[4..7]).all(u8::is_ascii_digit) {
        let c: f64 = comment[..3].parse().unwrap_or(0.0);
        let s: f64 = comment[4..7].parse().unwrap_or(0.0);
        // Course 000 means "not known"; 360 is north.
        let course = (c > 0.0).then_some(if c >= 360.0 { 0.0 } else { c });
        return (course, Some(s * 0.514_444), &comment[7..]);
    }
    (None, None, comment)
}

/// Parse one APRS-IS line like `N0CALL>APRS,TCPIP*:!4903.50N/07201.75W-Test`.
/// `now_unix` stamps reports because APRS timestamps are only day/hour/minute.
pub fn parse_aprs(line: &str, now_unix: u64) -> Option<Report> {
    let line = line.trim_end();
    if line.starts_with('#') {
        return None;
    }
    let (head, payload) = line.split_once(':')?;
    let src = head.split('>').next()?.trim();
    if src.is_empty() || src.len() > 9 {
        return None;
    }
    let mut chars = payload.chars();
    let kind = chars.next()?;
    let body = match kind {
        '!' | '=' => &payload[1..],
        // Timestamped: 7 characters (DDHHMMz, HHMMSSh or DDHHMM/) before the position.
        '/' | '@' if payload.len() > 8 && payload.is_char_boundary(8) => &payload[8..],
        _ => return None,
    };
    let b = body.as_bytes();
    let (lat, lng, comment): (f64, f64, &str);
    let (mut course, mut speed) = (None, None);
    if b.first().is_some_and(u8::is_ascii_digit) || b.first() == Some(&b' ') {
        // Uncompressed: lat(8) table(1) lon(9) symbol(1) comment
        if body.len() < 19
            || !body.is_char_boundary(8)
            || !body.is_char_boundary(18)
            || !body.is_char_boundary(19)
        {
            return None;
        }
        lat = ddm(&body[..8], 2)?;
        lng = ddm(&body[9..18], 3)?;
        let (c, s, rest) = course_speed(&body[19..]);
        (course, speed, comment) = (c, s, rest);
    } else {
        // Compressed: table(1) lat(4) lon(4) symbol(1) cs(2) type(1)
        if b.len() < 13 {
            return None;
        }
        lat = 90.0 - base91(&b[1..5])? / 380_926.0;
        lng = -180.0 + base91(&b[5..9])? / 190_463.0;
        let (c, s) = (b[10], b[11]);
        // The type byte says whether the two cs bytes hold course/speed or an altitude.
        let is_altitude = (b[12].saturating_sub(33) & 0x18) == 0x10;
        if c != b' ' && !is_altitude && (b'!'..=b'z').contains(&c) && s >= 33 {
            let deg = (c - 33) as f64 * 4.0;
            // Course 0 means "not known" here too.
            course = (deg > 0.0).then_some(deg);
            speed = Some((1.08f64.powf((s - 33) as f64) - 1.0) * 0.514_444);
        }
        comment = if body.is_char_boundary(13) {
            &body[13..]
        } else {
            ""
        };
    }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return None;
    }
    Some(Report {
        network: Network::Aprs,
        id: src.to_string(),
        name: src.to_string(),
        lat,
        lng,
        time_unix: Some(now_unix),
        speed_mps: speed,
        course_deg: course,
        alt_m: None,
        text: comment.trim().to_string(),
        emergency: false,
    })
}

/// Build an APRS-IS server filter from what a person types: callsigns separated by commas
/// or spaces (`W0ABC-9, k0xyz`, `*` wildcards allowed) become a buddy list; `area:lat,lng,km`
/// follows everything within a radius. Anything else is refused rather than passed through,
/// so typed text can never inject extra login parameters.
pub fn aprs_filter(input: &str) -> Result<String, String> {
    let t = input.trim();
    if let Some(a) = t.strip_prefix("area:") {
        let n: Vec<f64> = a.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        return match n[..] {
            [lat, lng, km]
                if (-90.0..=90.0).contains(&lat)
                    && (-180.0..=180.0).contains(&lng)
                    && km > 0.0
                    && km <= 500.0 =>
            {
                Ok(format!("r/{lat:.4}/{lng:.4}/{km:.0}"))
            }
            _ => Err("Use area:lat,lng,km with a radius up to 500 km.".into()),
        };
    }
    let calls: Vec<String> = t
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|c| !c.is_empty())
        .map(str::to_uppercase)
        .collect();
    if calls.is_empty() || calls.len() > 30 {
        return Err("Enter 1 to 30 callsigns, like W0ABC-9, K0XYZ".into());
    }
    for c in &calls {
        if c.len() > 9
            || !c
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '*')
        {
            return Err(format!("\"{c}\" is not a valid callsign."));
        }
    }
    Ok(format!("b/{}", calls.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_position_from_the_spec_example() {
        let r = parse_aprs("N0CALL>APRS,TCPIP*:!4903.50N/07201.75W-Test 001234", 1000).unwrap();
        assert_eq!(r.id, "N0CALL");
        assert!((r.lat - 49.058_333).abs() < 1e-5, "{}", r.lat);
        assert!((r.lng + 72.029_166).abs() < 1e-5, "{}", r.lng);
        assert_eq!(r.text, "Test 001234");
        assert_eq!(r.time_unix, Some(1000));
        assert_eq!((r.speed_mps, r.course_deg), (None, None));
    }

    #[test]
    fn course_and_speed_in_the_comment() {
        let r = parse_aprs("W0ABC-9>APRS:=3834.60N/09210.41W>088/036 on the road", 5).unwrap();
        assert_eq!(r.course_deg, Some(88.0));
        assert!((r.speed_mps.unwrap() - 36.0 * 0.514_444).abs() < 1e-9);
        assert_eq!(r.text, "on the road");
        assert_eq!(r.id, "W0ABC-9");
        // Southern and western hemispheres are negative.
        let s = parse_aprs("A>B:!3351.00S/15112.00W-", 1).unwrap();
        assert!(s.lat < 0.0 && s.lng < 0.0);
    }

    #[test]
    fn timestamped_positions() {
        let r = parse_aprs("K0XYZ>APRS:@092345z4903.50N/07201.75W-hello", 1).unwrap();
        assert!((r.lat - 49.058_333).abs() < 1e-5);
        assert_eq!(r.text, "hello");
        let r = parse_aprs("K0XYZ>APRS:/092345h4903.50N/07201.75W-", 1).unwrap();
        assert!((r.lng + 72.029_166).abs() < 1e-5);
    }

    #[test]
    fn compressed_position_from_the_spec_example() {
        // The APRS spec's compressed example: 49.5 N, 72.75 W, course 88, speed 36.2 knots.
        let r = parse_aprs("N0CALL>APRS:!/5L!!<*e7>7P[", 1).unwrap();
        assert!((r.lat - 49.5).abs() < 1e-3, "{}", r.lat);
        assert!((r.lng + 72.75).abs() < 1e-3, "{}", r.lng);
        assert_eq!(r.course_deg, Some(88.0));
        assert!(
            (r.speed_mps.unwrap() / 0.514_444 - 36.2).abs() < 0.2,
            "{:?}",
            r.speed_mps
        );
    }

    #[test]
    fn filters_from_what_people_type() {
        assert_eq!(aprs_filter("W0ABC-9, k0xyz").unwrap(), "b/W0ABC-9/K0XYZ");
        assert_eq!(aprs_filter("w0*").unwrap(), "b/W0*");
        assert_eq!(
            aprs_filter("area:38.5767,-92.1735,150").unwrap(),
            "r/38.5767/-92.1735/150"
        );
        for bad in [
            "",
            "   ",
            "area:1,2",
            "area:95,0,10",
            "area:38,-92,9000",
            "A B; filter *",
            "W0ABC;x",
            "TOOLONGCALL1",
        ] {
            assert!(aprs_filter(bad).is_err(), "{bad:?}");
        }
        let many = (0..31)
            .map(|i| format!("A{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(aprs_filter(&many).is_err());
    }

    #[test]
    fn everything_else_is_ignored_safely() {
        for l in [
            "# aprsc 2.1.15 server banner",
            "N0CALL>APRS:>status text only",
            "N0CALL>APRS:`mic-e data here",
            "N0CALL>APRS::MESSAGE  :hello{1",
            "N0CALL>APRS:!4903.50N",
            "N0CALL>APRS:!9999.99N/07201.75W-",
            "N0CALL>APRS:!4903.50X/07201.75W-",
            ":nocall",
            "",
            "TOOLONGCALLSIGN>APRS:!4903.50N/07201.75W-",
        ] {
            assert!(parse_aprs(l, 1).is_none(), "{l:?}");
        }
        // Non-ASCII junk must not panic on a char boundary.
        assert!(parse_aprs("A>B:!49é3.50N/07201.75W-", 1).is_none());
    }
}
