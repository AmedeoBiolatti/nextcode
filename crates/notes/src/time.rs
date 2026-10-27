//! RFC 3339 UTC times (`2026-10-18T12:34:56.789Z`) without a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Now, as stored in notes.
pub fn now() -> String {
    format_time(now_ms())
}

/// Unix milliseconds as RFC 3339 UTC with milliseconds; these sort as text.
pub fn format_time(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms % 1000
    )
}

/// Unix milliseconds from RFC 3339 (any offset, optional fraction), or
/// from a plain number of milliseconds.
pub fn parse_time(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Ok(ms) = text.parse::<u64>() {
        return Some(ms);
    }
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes[16] != b':' {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        millis = padded.parse::<i64>().ok()?;
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours = rest.get(1..3)?.parse::<i64>().ok()?;
            let minutes = rest.get(4..6)?.parse::<i64>().ok()?;
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            sign * (hours * 60 + minutes) * 60
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(secs * 1000 + millis).ok()
}

// Howard Hinnant's civil calendar algorithms.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for ms in [0, 951_782_400_123, 1_792_330_496_789, 4_102_444_800_000] {
            assert_eq!(parse_time(&format_time(ms)), Some(ms));
        }
        assert_eq!(format_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_time(951_782_400_123), "2000-02-29T00:00:00.123Z");
    }

    #[test]
    fn parses_offsets_and_numbers() {
        assert_eq!(
            parse_time("2026-10-18T14:00:00+02:00"),
            parse_time("2026-10-18T12:00:00Z")
        );
        assert_eq!(parse_time("2026-10-18T12:00:00.5Z").unwrap() % 1000, 500);
        assert_eq!(parse_time("1700000000000"), Some(1_700_000_000_000));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(parse_time("2026-13-01T00:00:00Z"), None);
    }
}
