//! Wall-clock helpers. The harness compares its own clock (when it fired a
//! fault) with the times the kubelet writes into Pod status; both are the
//! host's and the kind node's clocks, which on one machine agree to well
//! under the seconds the windows here are wide.

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse the RFC 3339 timestamps Kubernetes writes (`2026-11-01T06:36:24Z`,
/// optionally with a fraction or a numeric offset) into unix milliseconds.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut frac_ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &s[start..i];
        let padded = format!("{:0<3}", &digits[..digits.len().min(3)]);
        frac_ms = padded.parse().ok()?;
    }
    let offset_s = match b.get(i)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let oh: i64 = s.get(i + 1..i + 3)?.parse().ok()?;
            let om: i64 = s.get(i + 4..i + 6)?.parse().ok()?;
            let o = oh * 3600 + om * 60;
            if *sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_s;
    Some(secs * 1000 + frac_ms)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_the_kubelet_writes() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        // 2026-11-01T06:36:24Z, checked with `date -u -d @1793514984`.
        assert_eq!(
            parse_rfc3339_ms("2026-11-01T06:36:24Z"),
            Some(1_793_514_984_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-01T06:36:24.5Z"),
            Some(1_793_514_984_500)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-01T12:06:24+05:30"),
            Some(1_793_514_984_000)
        );
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }
}
