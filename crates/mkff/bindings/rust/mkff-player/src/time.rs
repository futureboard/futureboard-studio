//! Time display helpers.

/// `m:ss` below an hour, `h:mm:ss` from an hour; with `millis`, `.mmm` appended.
pub fn format_time(seconds: f64, millis: bool) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total_ms = (seconds * 1000.0).round() as u64;
    let (h, m, s, ms) = (
        total_ms / 3_600_000,
        total_ms / 60_000 % 60,
        total_ms / 1000 % 60,
        total_ms % 1000,
    );
    let mut out = if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    };
    if millis {
        out.push_str(&format!(".{ms:03}"));
    }
    out
}

/// Parse `SS[.fff]`, `MM:SS[.fff]` or `HH:MM:SS[.fff]` into seconds.
pub fn parse_time(text: &str) -> Option<f64> {
    let mut total = 0.0;
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        let v: f64 = part.parse().ok()?;
        if !v.is_finite() || v < 0.0 || (i > 0 && v >= 60.0) {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(format_time(0.0, false), "0:00");
        assert_eq!(format_time(61.5, false), "1:01");
        assert_eq!(format_time(3725.25, true), "1:02:05.250");
        assert_eq!(format_time(f64::NAN, false), "0:00");
        assert_eq!(format_time(-3.0, false), "0:00");
    }

    #[test]
    fn parses() {
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1:30"), Some(90.0));
        assert_eq!(parse_time("1:00:00.5"), Some(3600.5));
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time("x"), None);
        assert_eq!(parse_time("1:2:3:4"), None);
    }
}
