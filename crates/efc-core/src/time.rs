//! Время ISO 8601 UTC без внешних зависимостей: формат `ГГГГ-ММ-ДДTчч:мм:ссZ` и разбор того же формата
//! (допускаются доли секунды и смещение `+00:00`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Текущее время строкой ISO 8601 UTC.
pub fn now_iso() -> String {
    format(SystemTime::now())
}

/// Время строкой ISO 8601 UTC (с точностью до секунды).
pub fn format(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d, h, mi, s) = parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Части даты-времени UTC из секунд от эпохи.
pub fn parts(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

/// Разбор ISO 8601 UTC. `None` — не наш формат.
pub fn parse(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos: u32 = 0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let mut frac = digits.clone();
        frac.truncate(9);
        while frac.len() < 9 {
            frac.push('0');
        }
        nanos = frac.parse().ok()?;
        rest = &r[digits.len()..];
    }
    let offset: i64 = match rest {
        "Z" | "z" | "" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se - offset;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(secs as u64, nanos))
}

/// Сколько секунд прошло с `t` (отрицательное — `t` в будущем).
pub fn age_secs(t: SystemTime) -> f64 {
    match SystemTime::now().duration_since(t) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

// Алгоритмы Говарда Хиннанта (дни от 1970-01-01 ↔ григорианская дата).
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

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for s in ["1970-01-01T00:00:00Z", "2026-01-15T09:03:01Z", "2000-02-29T23:59:59Z", "2100-12-31T12:00:00Z"] {
            assert_eq!(format(parse(s).unwrap()), s);
        }
        assert_eq!(format(parse("2026-01-15T12:03:01+03:00").unwrap()), "2026-01-15T09:03:01Z");
        assert_eq!(format(parse("2026-01-15T09:03:01.123456789Z").unwrap()), "2026-01-15T09:03:01Z");
        assert!(parse("вчера").is_none());
        assert!(parse("2026-13-01T00:00:00Z").is_none());
    }
}
