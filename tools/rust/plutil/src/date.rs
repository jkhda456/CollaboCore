//! Dates: CFAbsoluteTime (seconds since 2001-01-01 00:00:00 UTC) to and from the Gregorian
//! calendar as CFDate.c does it — proleptic, in UTC, year by year within 400-year cycles.

const DAYS_BEFORE_MONTH: [u16; 16] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365, 0, 0];
const INVALID_MONTH: u16 = 0xffff;

/// CF's isleap, for an "absolute" year (2001 == 0).
fn isleap(year: i64) -> bool {
    let mut y = (year + 1) % 400;
    if y < 0 {
        y = -y;
    }
    y & 3 == 0 && y != 100 && y != 200 && y != 300
}

fn days_in_year(year: i64) -> i64 {
    365 + isleap(year) as i64
}

fn days_before_month(month: i64, leap: bool) -> u16 {
    if (1..=13).contains(&month) {
        DAYS_BEFORE_MONTH[month as usize] + (month > 2 && leap) as u16
    } else {
        INVALID_MONTH
    }
}

/// __CFYMDFromAbsolute: days since 2001-01-01 to (absolute year, month, day).
fn ymd_from_absolute(mut absolute: i64) -> (i64, i64, i64) {
    let b = absolute / 146097;
    let mut y = b * 400;
    absolute -= b * 146097;
    while absolute < 0 {
        y -= 1;
        absolute += days_in_year(y);
    }
    let mut ydays = days_in_year(y);
    while ydays <= absolute {
        y += 1;
        absolute -= ydays;
        ydays = days_in_year(y);
    }
    let leap = isleap(y);
    let mut m = absolute / 33 + 1;
    while m + 1 <= 12 && days_before_month(m + 1, leap) as i64 <= absolute {
        m += 1;
    }
    (y, m, absolute - days_before_month(m, leap) as i64 + 1)
}

/// __CFAbsoluteFromYMD: (absolute year, month, day) to days since 2001-01-01. An out of range
/// month adds nothing for itself, as CF's does.
fn absolute_from_ymd(mut year: i64, month: i64, day: i64) -> f64 {
    let mut absolute = 0.0;
    let b = year / 400;
    absolute += b as f64 * 146097.0;
    year -= b * 400;
    if year < 0 {
        for idx in year..0 {
            absolute -= days_in_year(idx) as f64;
        }
    } else {
        for idx in 0..year {
            absolute += days_in_year(idx) as f64;
        }
    }
    let before = days_before_month(month, isleap(year));
    if before != INVALID_MONTH {
        absolute += before as f64;
    }
    absolute + (day - 1) as f64
}

/// CFGregorianDateGetAbsoluteTime in UTC.
pub fn absolute_time(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: f64) -> f64 {
    let mut at = 86400.0 * absolute_from_ymd(year - 2001, month, day);
    at += 3600.0 * hour as f64 + 60.0 * minute as f64 + second;
    at
}

fn double_mod(d: f64, modulus: f64) -> f64 {
    let r = d - (d / modulus).floor() * modulus;
    if r < 0.0 {
        r + modulus
    } else {
        r
    }
}

/// CFAbsoluteTimeGetGregorianDate in UTC: (year, month, day, hour, minute, second).
pub fn gregorian(at: f64) -> (i64, i64, i64, i64, i64, f64) {
    let absolute = (at / 86400.0).floor() as i64;
    let (mut y, m, d) = ymd_from_absolute(absolute);
    if i32::MAX as i64 - 2001 < y {
        y = i32::MAX as i64 - 2001;
    }
    let hour = double_mod((at / 3600.0).floor(), 24.0) as i64;
    let minute = double_mod((at / 60.0).floor(), 60.0) as i64;
    let second = double_mod(at, 60.0);
    (y + 2001, m, d, hour, minute, if second == 0.0 { 0.0 } else { second })
}

/// The XML form, as CF writes it: YYYY-MM-DDThh:mm:ssZ, the seconds cut to a whole number.
pub fn xml(at: f64) -> String {
    let (y, mo, d, h, mi, s) = gregorian(at);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y as i32, mo, d, h, mi, s as i32)
}

/// Date.description: "2001-01-01 00:00:00 +0000".
pub fn description(at: f64) -> String {
    let (y, mo, d, h, mi, s) = gregorian(at);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} +0000", y as i32, mo, d, h, mi, s as i32)
}

/// `date.formatted(.iso8601)`: "2001-01-01T00:00:00Z" (what `-extract … raw` prints).
pub fn iso8601(at: f64) -> String {
    xml(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_calendar() {
        assert_eq!(xml(0.0), "2001-01-01T00:00:00Z");
        assert_eq!(xml(-978307200.0), "1970-01-01T00:00:00Z");
        assert_eq!(xml(694224000.0), "2023-01-01T00:00:00Z");
        assert_eq!(xml(1.9), "2001-01-01T00:00:01Z");
        assert_eq!(xml(-0.5), "2000-12-31T23:59:59Z");
        assert_eq!(description(86400.0 * 59.0), "2001-03-01 00:00:00 +0000");
        for (y, m, d) in [(2000, 2, 29), (1600, 3, 1), (2024, 12, 31), (1, 1, 1), (2400, 2, 29)] {
            let at = absolute_time(y, m, d, 13, 14, 15.0);
            assert_eq!(xml(at), format!("{y:04}-{m:02}-{d:02}T13:14:15Z"));
        }
    }
}
