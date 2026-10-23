use anyhow::{Result, bail};

pub fn duration_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn offset_ms(ms: i64) -> String {
    let tenths = ms.max(0) / 100;
    let (s, t) = (tenths / 10, tenths % 10);
    format!("{:02}:{:02}:{:02}.{t}", s / 3600, (s % 3600) / 60, s % 60)
}

pub fn ago(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

pub fn abbreviate_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path == h => "~".into(),
        Ok(h) if !h.is_empty() && path.starts_with(&format!("{h}/")) => format!("~{}", &path[h.len()..]),
        _ => path.into(),
    }
}

pub fn truncate_back(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max { s.into() } else { format!("{}...", s.chars().take(max.saturating_sub(3)).collect::<String>()) }
}

pub fn truncate_front(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max { s.into() } else { format!("...{}", s.chars().skip(n + 3 - max).collect::<String>()) }
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn parse_offset(s: &str) -> Result<i64> {
    let parts: Vec<&str> = s.split(':').collect();
    let num =
        |x: &str| x.parse::<f64>().map_err(|_| anyhow::anyhow!("offset {s:?} must be seconds, MM:SS or HH:MM:SS"));
    let secs = match parts.as_slice() {
        [s] => num(s)?,
        [m, s] => num(m)? * 60.0 + num(s)?,
        [h, m, s] => num(h)? * 3600.0 + num(m)? * 60.0 + num(s)?,
        _ => bail!("offset {s:?} must be seconds, MM:SS or HH:MM:SS"),
    };
    if !secs.is_finite() || secs < 0.0 {
        bail!("offset {s:?} must be a non-negative time");
    }
    Ok((secs * 1e9) as i64)
}

pub fn parse_age(s: &str) -> Result<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().map_err(|_| anyhow::anyhow!("age {s:?} must look like 30d, 12h or 90m"))?;
    let secs = match unit {
        "d" => n * 86_400,
        "h" => n * 3600,
        "m" => n * 60,
        "s" => n,
        _ => bail!("age {s:?} must end in d, h, m or s"),
    };
    Ok(secs * 1_000_000_000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn relative_times_and_truncation() {
        assert_eq!(super::ago(5_000), "5s ago");
        assert_eq!(super::ago(90_000), "1m ago");
        assert_eq!(super::ago(7_200_000), "2h ago");
        assert_eq!(super::ago(200_000_000), "2d ago");
        assert_eq!(super::truncate_back("abcdef", 5), "ab...");
        assert_eq!(super::truncate_front("abcdef", 5), "...ef");
        assert_eq!(super::truncate_back("abc", 5), "abc");
    }

    use super::*;

    #[test]
    fn offsets_round_trip_through_parse_offset() {
        assert_eq!(offset_ms(312), "00:00:00.3");
        assert_eq!(offset_ms(3_723_950), "01:02:03.9");
        assert_eq!(parse_offset(&offset_ms(3_723_950)).unwrap(), 3_723_900_000_000);
    }

    #[test]
    fn formats_and_parses() {
        assert_eq!(duration_ms(763_000), "12m 43s");
        assert_eq!(duration_ms(3_723_000), "1h 02m 03s");
        assert_eq!(duration_ms(4_000), "4s");
        assert_eq!(parse_offset("00:02:30").unwrap(), 150_000_000_000);
        assert_eq!(parse_offset("2:30").unwrap(), 150_000_000_000);
        assert_eq!(parse_offset("1.5").unwrap(), 1_500_000_000);
        assert!(parse_offset("-1").is_err());
        assert_eq!(parse_age("7d").unwrap(), 7 * 86_400 * 1_000_000_000);
        assert!(parse_age("7w").is_err());
        assert_eq!(bytes(184 * 1024 * 1024), "184.0 MiB");
    }
}
