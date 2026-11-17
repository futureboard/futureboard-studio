//! Time, bitrate and size parsing / formatting in FFmpeg's notation.

/// `[-][HH:]MM:SS[.m…]` or `S[.m…][s|ms|us]`.
pub fn parse_time(s: &str) -> Result<f64, String> {
    let bad = || format!("invalid time '{s}'");
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let secs = if body.contains(':') {
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() > 3 {
            return Err(bad());
        }
        let mut t = 0.0;
        for (i, p) in parts.iter().enumerate() {
            let v: f64 = p.parse().map_err(|_| bad())?;
            if v < 0.0 || (i > 0 && v >= 60.0) {
                return Err(bad());
            }
            t = t * 60.0 + v;
        }
        t
    } else if let Some(v) = body.strip_suffix("ms") {
        v.parse::<f64>().map_err(|_| bad())? / 1e3
    } else if let Some(v) = body.strip_suffix("us") {
        v.parse::<f64>().map_err(|_| bad())? / 1e6
    } else {
        body.strip_suffix('s')
            .unwrap_or(body)
            .parse::<f64>()
            .map_err(|_| bad())?
    };
    if !secs.is_finite() {
        return Err(bad());
    }
    Ok(if neg { -secs } else { secs })
}

/// `128000`, `128k`, `1.5M`.
pub fn parse_bitrate(s: &str) -> Result<u32, String> {
    let bad = || format!("invalid bitrate '{s}'");
    let (num, mul) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1e3),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1e6),
        _ => (s, 1.0),
    };
    let v = num.parse::<f64>().map_err(|_| bad())? * mul;
    if !(1000.0..=2e9).contains(&v) {
        return Err(bad());
    }
    Ok(v.round() as u32)
}

/// `HH:MM:SS.cc`.
pub fn fmt_time(secs: f64) -> String {
    let neg = secs < 0.0;
    let cs = (secs.abs() * 100.0).round() as u64;
    format!(
        "{}{:02}:{:02}:{:02}.{:02}",
        if neg { "-" } else { "" },
        cs / 360_000,
        cs / 6000 % 60,
        cs / 100 % 60,
        cs % 100
    )
}

/// `1234kB`-style size, as FFmpeg's progress line prints it.
pub fn fmt_kb(bytes: u64) -> String {
    format!("{}kB", bytes.div_ceil(1024))
}

/// Channel layout name for a channel count.
pub fn layout(channels: u32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        3 => "2.1".into(),
        4 => "quad".into(),
        5 => "5.0".into(),
        6 => "5.1".into(),
        7 => "6.1".into(),
        8 => "7.1".into(),
        n => format!("{n} channels"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("90").unwrap(), 90.0);
        assert_eq!(parse_time("1:30").unwrap(), 90.0);
        assert_eq!(parse_time("01:01:30.5").unwrap(), 3690.5);
        assert_eq!(parse_time("250ms").unwrap(), 0.25);
        assert_eq!(parse_time("2.5s").unwrap(), 2.5);
        assert!(parse_time("1:75").is_err());
        assert!(parse_time("abc").is_err());
        assert_eq!(fmt_time(3690.5), "01:01:30.50");
        assert_eq!(fmt_time(0.256), "00:00:00.26");
    }

    #[test]
    fn bitrates() {
        assert_eq!(parse_bitrate("192k").unwrap(), 192_000);
        assert_eq!(parse_bitrate("1.5M").unwrap(), 1_500_000);
        assert_eq!(parse_bitrate("64000").unwrap(), 64_000);
        assert!(parse_bitrate("12").is_err());
        assert!(parse_bitrate("fast").is_err());
    }
}
