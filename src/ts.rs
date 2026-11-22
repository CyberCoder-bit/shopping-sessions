//! Timestamps (rules.md "Timestamps"): F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 with `±HH:MM`,
//! F3 ASCII digits = epoch ms. Returns epoch milliseconds, or None for a bad form.
//! Window checks are the caller's job.

pub const DAY_MS: i64 = 86_400_000;

#[inline]
fn d2(b: &[u8], i: usize) -> Option<u32> {
    let hi = b[i].wrapping_sub(b'0');
    let lo = b[i + 1].wrapping_sub(b'0');
    if hi < 10 && lo < 10 { Some(hi as u32 * 10 + lo as u32) } else { None }
}

#[inline]
fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[inline]
fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM:SS` (exactly 19 bytes) as seconds since the epoch, validated.
#[inline]
fn parse_datetime(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y as i64, mo, d) * 86_400 + (h * 3600 + mi * 60 + s) as i64)
}

#[inline]
pub fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    if b[0].is_ascii_digit() && b.iter().all(u8::is_ascii_digit) {
        // F3: epoch ms. Leading zeros are fine; a value that doesn't fit is out of any window.
        let zeros = b.iter().take_while(|&&c| c == b'0').count();
        let digits = &b[zeros..];
        if digits.len() > 18 {
            return None;
        }
        let mut v: i64 = 0;
        for &c in digits {
            v = v * 10 + (c - b'0') as i64;
        }
        return Some(v);
    }
    if n == 20 && b[19] == b'Z' {
        return parse_datetime(&b[..19]).map(|s| s * 1000);
    }
    if n == 25 && (b[19] == b'+' || b[19] == b'-') && b[22] == b':' {
        let local = parse_datetime(&b[..19])?;
        let oh = d2(b, 20)?;
        let om = d2(b, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let offset = (oh * 3600 + om * 60) as i64;
        let utc = if b[19] == b'+' { local - offset } else { local + offset };
        return Some(utc * 1000);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms() {
        let f1 = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(f1, 1_791_071_890_000);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse_ts(b"1791071890000"), Some(f1));
        assert_eq!(parse_ts(b"0001791071890000"), Some(f1));
        assert_eq!(parse_ts(b"2026-10-04T01:30:00+02:00"), parse_ts(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse_ts(b"2026-10-03T10:00:00-00:00"), parse_ts(b"2026-10-03T10:00:00Z"));
        assert_eq!(parse_ts(b"2026-10-03T10:00:00+23:59").map(|_| ()), Some(()));
        for bad in [
            &b""[..], b"2026-10-03 10:00:00Z", b"2026-10-03t10:00:00Z", b"2026-10-03T10:00:00z",
            b"2026-10-03T10:00:00", b"2026-10-03T10:00:00.5Z", b"2026-13-03T10:00:00Z",
            b"2026-02-30T10:00:00Z", b"2027-02-29T10:00:00Z", b"2026-10-03T24:00:00Z",
            b"2026-10-03T10:00:60Z", b"2026-10-03T10:00:00+24:00", b"2026-10-03T10:00:00+05:60",
            b"2026-10-03T10:00:00+0200", b"1791071890000.0", b"1.79e12", b"-5", b"2026-10-03T10:00:00Z\r",
            "٢٠٢٦".as_bytes(), b"123456789012345678901",
        ] {
            assert_eq!(parse_ts(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse_ts(b"2028-02-29T10:00:00Z").is_some());
        assert!(parse_ts(b"2000-02-29T10:00:00Z").is_some());
        assert!(parse_ts(b"2100-02-29T10:00:00Z").is_none());
        assert_eq!(parse_ts(b"86400000"), Some(86_400_000));
    }
}
