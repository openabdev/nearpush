//! RFC 3339 / ISO 8601 timestamps as Unix milliseconds. Shared by the CLI's
//! `--started-at` and the daemon's relay adapter.

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM[:SS[.fff]](Z|±HH:MM|±HHMM)`; `T` may be `t` or a space. A time
/// with no offset is rejected rather than guessed.
pub fn parse_rfc3339_ms(s: &str) -> Result<u64, String> {
    let bad = || format!("invalid time {s:?}; use e.g. 2026-11-04T18:49:33Z or 2026-11-05T02:49:33+08:00");
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Result<i64, String> {
        let t = s.get(r).ok_or_else(bad)?;
        if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        t.parse::<i64>().map_err(|_| bad())
    };
    if b.len() < 16 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' {
        return Err(bad());
    }
    let (y, mo, d, h, mi) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?);
    let mut i = 16;
    let mut sec = 0;
    let mut ms = 0;
    if b.get(i) == Some(&b':') {
        sec = num(i + 1..i + 3)?;
        i += 3;
        if matches!(b.get(i), Some(b'.') | Some(b',')) {
            let start = i + 1;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                return Err(bad());
            }
            let frac = &s[start..end.min(start + 3)];
            ms = frac.parse::<i64>().map_err(|_| bad())? * 10i64.pow(3 - frac.len() as u32);
            i = end;
        }
    }
    let offset_min = match b.get(i) {
        Some(b'Z') | Some(b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let rest = &s[i + 1..];
            let (oh, om) = match rest.len() {
                5 if rest.as_bytes()[2] == b':' => (num(i + 1..i + 3)?, num(i + 4..i + 6)?),
                4 => (num(i + 1..i + 3)?, num(i + 3..i + 5)?),
                _ => return Err(bad()),
            };
            if oh > 23 || om > 59 {
                return Err(bad());
            }
            let m = oh * 60 + om;
            if sign == b'+' { m } else { -m }
        }
        _ => return Err(format!("{} (a UTC offset such as Z or +08:00 is required)", bad())),
    };
    let dim = [31, if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&mo) || d < 1 || d > dim[(mo - 1) as usize] || h > 23 || mi > 59 || sec > 60 {
        return Err(bad());
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec.min(59) - offset_min * 60;
    if secs <= 0 {
        return Err("--started-at is before 1970".into());
    }
    Ok(secs as u64 * 1000 + ms as u64)
}
