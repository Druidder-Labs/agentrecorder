use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn now_unix_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[must_use]
pub fn format_rfc3339(unix_ns: i64) -> String {
    let secs = unix_ns.div_euclid(1_000_000_000);
    let nanos = unix_ns.rem_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{nanos:09}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

#[must_use]
pub fn format_rfc3339_seconds(unix_ns: i64) -> String {
    let full = format_rfc3339(unix_ns);
    format!("{}Z", &full[..19])
}

/// Parses a UTC or offset RFC 3339 timestamp to Unix nanoseconds; leap seconds (`:60`) and days a month
/// does not have are rejected.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (clock, zone) = (&rest[..zone_at], &rest[zone_at..]);
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = hms.splitn(3, ':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut nanos: i64 = 0;
    if !frac.is_empty() {
        if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nanos = frac.parse::<i64>().ok()? * 10_i64.pow(9 - u32::try_from(frac.len()).ok()?);
    }
    let offset_s = match zone {
        "Z" | "z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let (oh, om) = z[1..].split_once(':')?;
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60)
        }
    };
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset_s;
    Some(secs * 1_000_000_000 + nanos)
}

fn is_leap_year(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap_year(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("day in 1..=31");
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("month in 1..=12");
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(format_rfc3339(1_792_497_600_000_000_123), "2026-10-20T12:00:00.000000123Z");
        assert_eq!(format_rfc3339(951_782_400_000_000_000), "2000-02-29T00:00:00.000000000Z");
    }

    #[test]
    fn parses_what_it_formats_and_javascript_iso_strings() {
        for ns in [0, 1_792_497_600_000_000_123, 951_782_400_000_000_000, 1_761_050_000_123_000_000] {
            assert_eq!(parse_rfc3339(&format_rfc3339(ns)), Some(ns));
        }
        assert_eq!(parse_rfc3339("2026-10-20T12:00:00.123Z"), Some(1_792_497_600_123_000_000));
        assert_eq!(parse_rfc3339("2026-10-20T12:00:00Z"), Some(1_792_497_600_000_000_000));
        assert_eq!(parse_rfc3339("2026-10-20T09:00:00-03:00"), Some(1_792_497_600_000_000_000));
        for bad in ["", "2026-10-20", "2026-13-01T00:00:00Z", "2026-10-20T12:00:00", "x"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn rejects_days_the_month_does_not_have_and_leap_seconds() {
        for bad in [
            "2026-04-31T00:00:00Z",
            "2026-06-31T00:00:00Z",
            "2026-09-31T00:00:00Z",
            "2026-11-31T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-32T00:00:00Z",
            "2026-10-20T23:59:60Z",
            "2026-10-20T24:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800_000_000_000));
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(951_782_400_000_000_000));
        assert_eq!(parse_rfc3339("2026-01-31T23:59:59Z"), Some(1_769_903_999_000_000_000));
    }

    #[test]
    fn civil_days_round_trip_across_years() {
        let mut days = days_from_civil(1970, 1, 1);
        assert_eq!(days, 0);
        for y in 1970..=2200 {
            for m in 1..=12 {
                for d in 1..=days_in_month(y, m) {
                    assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
                    assert_eq!(civil_from_days(days), (y, m, d), "{days}");
                    days += 1;
                }
            }
        }
        assert_eq!(days, days_from_civil(2201, 1, 1));
    }
}
