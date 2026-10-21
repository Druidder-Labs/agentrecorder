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
    let secs = match parts.as_slice() {
        [s] => s.parse::<f64>()?,
        [m, s] => m.parse::<f64>()? * 60.0 + s.parse::<f64>()?,
        [h, m, s] => h.parse::<f64>()? * 3600.0 + m.parse::<f64>()? * 60.0 + s.parse::<f64>()?,
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
    use super::*;

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
