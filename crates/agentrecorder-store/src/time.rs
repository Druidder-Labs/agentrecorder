use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

pub fn format_rfc3339(unix_ns: i64) -> String {
    let secs = unix_ns.div_euclid(1_000_000_000);
    let nanos = unix_ns.rem_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{nanos:09}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
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
}
