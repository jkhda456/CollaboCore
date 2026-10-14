//! Go's time package, as far as yq uses it: Parse and Format with Go layouts ("2006-01-02T15:04:05Z07:00"),
//! locations (UTC, fixed offsets, zoneinfo files read like Go's LoadLocation, and Local from TZ or
//! /etc/localtime) and Unix conversions.

use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct Zone {
    pub name: String,
    pub offset: i64,
    pub is_dst: bool,
}

#[derive(Clone, Debug)]
pub struct Location {
    pub name: String,
    pub zones: Vec<Zone>,
    /// (unix time, zone index), sorted
    pub tx: Vec<(i64, usize)>,
    /// POSIX TZ rule for times after the last transition
    pub extend: String,
}

impl Location {
    pub fn utc() -> Location {
        Location { name: "UTC".into(), zones: vec![], tx: vec![], extend: String::new() }
    }
    pub fn fixed(name: &str, offset: i64) -> Location {
        Location {
            name: name.into(),
            zones: vec![Zone { name: name.into(), offset, is_dst: false }],
            tx: vec![(i64::MIN, 0)],
            extend: String::new(),
        }
    }
    pub fn is_utc(&self) -> bool {
        self.zones.is_empty()
    }

    /// The zone in effect at `sec`: (name, offset, start, end, isDST).
    pub fn lookup(&self, sec: i64) -> (String, i64, i64, i64, bool) {
        if self.zones.is_empty() {
            return ("UTC".into(), 0, i64::MIN, i64::MAX, false);
        }
        if self.tx.is_empty() || sec < self.tx[0].0 {
            let z = &self.zones[self.lookup_first_zone()];
            let end = if self.tx.is_empty() { i64::MAX } else { self.tx[0].0 };
            return (z.name.clone(), z.offset, i64::MIN, end, z.is_dst);
        }
        // binary search for the last transition <= sec
        let mut lo = 0;
        let mut hi = self.tx.len();
        let mut end = i64::MAX;
        while hi - lo > 1 {
            let m = lo + (hi - lo) / 2;
            let lim = self.tx[m].0;
            if sec < lim {
                end = lim;
                hi = m;
            } else {
                lo = m;
            }
        }
        let z = &self.zones[self.tx[lo].1];
        let start = self.tx[lo].0;
        if lo == self.tx.len() - 1 && !self.extend.is_empty() {
            if let Some(r) = tzset(&self.extend, start, sec) {
                return r;
            }
        }
        (z.name.clone(), z.offset, start, end, z.is_dst)
    }

    fn lookup_first_zone(&self) -> usize {
        // Case 1: the first zone is unused by transitions, it's the zone before them.
        if !self.first_zone_used() {
            return 0;
        }
        // Case 2: the first transition is to a DST zone; find the first non-DST zone before it.
        if !self.tx.is_empty() && self.zones[self.tx[0].1].is_dst {
            let mut zi = self.tx[0].1 as i64 - 1;
            while zi >= 0 {
                if !self.zones[zi as usize].is_dst {
                    return zi as usize;
                }
                zi -= 1;
            }
        }
        // Case 3: the first non-DST zone.
        for (i, z) in self.zones.iter().enumerate() {
            if !z.is_dst {
                return i;
            }
        }
        0
    }

    fn first_zone_used(&self) -> bool {
        self.tx.iter().any(|t| t.1 == 0)
    }

    /// The offset of the zone named `name` in effect at `unix` (Go's lookupName).
    pub fn lookup_name(&self, name: &str, unix: i64) -> Option<i64> {
        for z in &self.zones {
            if z.name == name {
                let (nam, offset, ..) = self.lookup(unix - z.offset);
                if nam == z.name {
                    return Some(offset);
                }
            }
        }
        for z in &self.zones {
            if z.name == name {
                return Some(z.offset);
            }
        }
        None
    }
}

static LOCAL: OnceLock<Location> = OnceLock::new();

/// The local location, as Go sets it up: $TZ ("" or unset → /etc/localtime), UTC when unreadable.
pub fn local() -> &'static Location {
    LOCAL.get_or_init(|| {
        let mut loc = match std::env::var("TZ") {
            Err(_) => load_tzif_file("/etc/localtime").unwrap_or_else(Location::utc),
            Ok(tz) if tz.is_empty() => Location::utc(),
            Ok(tz) => {
                let name = tz.strip_prefix(':').unwrap_or(&tz).to_string();
                if name == "UTC" {
                    Location::utc()
                } else if name.starts_with('/') {
                    load_tzif_file(&name).unwrap_or_else(Location::utc)
                } else {
                    load_location(&name).unwrap_or_else(|_| Location::utc())
                }
            }
        };
        loc.name = "Local".into();
        loc
    })
}

const ZONE_DIRS: &[&str] = &["/usr/share/zoneinfo/", "/usr/share/lib/zoneinfo/", "/usr/lib/locale/TZ/", "/etc/zoneinfo/"];

/// time.LoadLocation.
pub fn load_location(name: &str) -> Result<Location, String> {
    if name.is_empty() || name == "UTC" {
        return Ok(Location::utc());
    }
    if name == "Local" {
        return Ok(local().clone());
    }
    if contains_dot_dot(name) || name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("time: invalid location name"));
    }
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(z) = std::env::var("ZONEINFO") {
        if !z.is_empty() {
            dirs.push(format!("{z}/"));
        }
    }
    dirs.extend(ZONE_DIRS.iter().map(|s| s.to_string()));
    for d in &dirs {
        if let Some(mut loc) = load_tzif_file(&format!("{d}{name}")) {
            loc.name = name.to_string();
            return Ok(loc);
        }
    }
    Err(format!("unknown time zone {name}"))
}

fn contains_dot_dot(s: &str) -> bool {
    s.split(['/', '\\']).any(|p| p == "..")
}

fn load_tzif_file(path: &str) -> Option<Location> {
    let data = std::fs::read(path).ok()?;
    parse_tzif(&data, path)
}

/// A TZif file (as Go's LoadLocationFromTZData).
pub fn parse_tzif(data: &[u8], name: &str) -> Option<Location> {
    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }
    if data.len() < 44 || &data[0..4] != b"TZif" {
        return None;
    }
    let version = data[4];
    let mut p = 20usize;
    let n = |p: usize| be32(&data[p..p + 4]) as usize;
    let (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt) = (n(p), n(p + 4), n(p + 8), n(p + 12), n(p + 16), n(p + 20));
    p += 24;
    let mut is64 = false;
    let (mut isutcnt, mut isstdcnt, mut leapcnt, mut timecnt, mut typecnt, mut charcnt) =
        (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt);
    if version >= b'2' {
        // skip the v1 data
        let skip = timecnt * 4 + timecnt + typecnt * 6 + charcnt + leapcnt * 8 + isstdcnt + isutcnt;
        p += skip;
        if data.len() < p + 44 || &data[p..p + 4] != b"TZif" {
            return None;
        }
        p += 20;
        isutcnt = n(p);
        isstdcnt = n(p + 4);
        leapcnt = n(p + 8);
        timecnt = n(p + 12);
        typecnt = n(p + 16);
        charcnt = n(p + 20);
        p += 24;
        is64 = true;
    }
    let tsize = if is64 { 8 } else { 4 };
    let need = timecnt * tsize + timecnt + typecnt * 6 + charcnt + leapcnt * (tsize + 4) + isstdcnt + isutcnt;
    if data.len() < p + need {
        return None;
    }
    let mut times = Vec::with_capacity(timecnt);
    for i in 0..timecnt {
        let o = p + i * tsize;
        let t = if is64 {
            i64::from_be_bytes(data[o..o + 8].try_into().unwrap())
        } else {
            be32(&data[o..o + 4]) as i32 as i64
        };
        times.push(t);
    }
    p += timecnt * tsize;
    let idx: Vec<usize> = data[p..p + timecnt].iter().map(|b| *b as usize).collect();
    p += timecnt;
    let mut raw_zones = Vec::new();
    for i in 0..typecnt {
        let o = p + i * 6;
        let off = be32(&data[o..o + 4]) as i32 as i64;
        let isdst = data[o + 4] != 0;
        let abbr = data[o + 5] as usize;
        raw_zones.push((off, isdst, abbr));
    }
    p += typecnt * 6;
    let chars = &data[p..p + charcnt];
    p += charcnt;
    p += leapcnt * (tsize + 4) + isstdcnt + isutcnt;
    let zones: Vec<Zone> = raw_zones
        .iter()
        .map(|(off, dst, abbr)| {
            let end = chars[*abbr.min(&chars.len())..].iter().position(|c| *c == 0).map(|e| abbr + e).unwrap_or(chars.len());
            Zone { name: String::from_utf8_lossy(&chars[(*abbr).min(chars.len())..end]).into_owned(), offset: *off, is_dst: *dst }
        })
        .collect();
    if zones.is_empty() {
        return None;
    }
    let mut extend = String::new();
    if is64 && p < data.len() && data[p] == b'\n' {
        if let Some(e) = data[p + 1..].iter().position(|c| *c == b'\n') {
            extend = String::from_utf8_lossy(&data[p + 1..p + 1 + e]).into_owned();
        }
    }
    let mut tx: Vec<(i64, usize)> = times.into_iter().zip(idx).collect();
    if tx.is_empty() {
        // Build fake transition to cover all time (as Go does).
        tx.push((i64::MIN, 0));
    }
    Some(Location { name: name.to_string(), zones, tx, extend })
}

// ---- POSIX TZ rules (Go's tzset) ----

fn tzset(s: &str, last_tx_sec: i64, sec: i64) -> Option<(String, i64, i64, i64, bool)> {
    let (std_name, s) = tzset_name(s)?;
    let (std_off, mut s) = tzset_offset(s)?;
    let std_offset = -std_off;
    if s.is_empty() || s.starts_with(',') {
        return Some((std_name.to_string(), std_offset, last_tx_sec, i64::MAX, false));
    }
    let (dst_name, rest) = tzset_name(s)?;
    s = rest;
    let dst_offset;
    if s.is_empty() || s.starts_with(',') {
        dst_offset = std_offset + 3600;
    } else {
        let (o, rest) = tzset_offset(s)?;
        dst_offset = -o;
        s = rest;
    }
    if s.is_empty() {
        s = ",M3.2.0,M11.1.0";
    }
    if !s.starts_with(',') && !s.starts_with(';') {
        return None;
    }
    s = &s[1..];
    let (start_rule, rest) = tzset_rule(s)?;
    if rest.is_empty() || !rest.starts_with(',') {
        return None;
    }
    let (end_rule, rest) = tzset_rule(&rest[1..])?;
    if !rest.is_empty() {
        return None;
    }
    let (year, _, _, yday) = absdate(sec + 0, true);
    let ysec = (yday as i64) * 86400 + (sec.rem_euclid(86400));
    let year_start = sec - ysec;
    let mut start_sec = tzrule_time(year, &start_rule, std_offset);
    let mut end_sec = tzrule_time(year, &end_rule, dst_offset);
    let mut dst_is_dst = true;
    let mut std_is_dst = false;
    let (mut sn, mut so, mut dn, mut dof) = (std_name.to_string(), std_offset, dst_name.to_string(), dst_offset);
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut sn, &mut dn);
        std::mem::swap(&mut so, &mut dof);
        std::mem::swap(&mut std_is_dst, &mut dst_is_dst);
    }
    if ysec < start_sec {
        return Some((sn, so, year_start, start_sec + year_start, std_is_dst));
    } else if ysec >= end_sec {
        return Some((sn, so, end_sec + year_start, year_start + 365 * 86400, std_is_dst));
    }
    Some((dn, dof, start_sec + year_start, end_sec + year_start, dst_is_dst))
}

fn tzset_name(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    if !s.starts_with('<') {
        for (i, c) in s.char_indices() {
            if matches!(c, '0'..='9' | ',' | '-' | '+') {
                if i < 3 {
                    return None;
                }
                return Some((&s[..i], &s[i..]));
            }
        }
        if s.len() < 3 {
            return None;
        }
        return Some((s, ""));
    }
    for (i, c) in s.char_indices() {
        if c == '>' {
            return Some((&s[1..i], &s[i + 1..]));
        }
    }
    None
}

fn tzset_offset(s: &str) -> Option<(i64, &str)> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut neg = false;
    let mut s = s;
    if b[0] == b'+' {
        s = &s[1..];
    } else if b[0] == b'-' {
        s = &s[1..];
        neg = true;
    }
    let (hours, rest) = tzset_num(s, 0, 24 * 7)?;
    let mut off = hours * 3600;
    s = rest;
    if s.is_empty() || !s.starts_with(':') {
        return Some((if neg { -off } else { off }, s));
    }
    let (mins, rest) = tzset_num(&s[1..], 0, 59)?;
    off += mins * 60;
    s = rest;
    if s.is_empty() || !s.starts_with(':') {
        return Some((if neg { -off } else { off }, s));
    }
    let (secs, rest) = tzset_num(&s[1..], 0, 59)?;
    off += secs;
    Some((if neg { -off } else { off }, rest))
}

#[derive(Clone, Copy)]
enum RuleKind {
    Julian,
    Doy,
    MonthWeekDay,
}

#[derive(Clone, Copy)]
struct Rule {
    kind: RuleKind,
    day: i64,
    week: i64,
    mon: i64,
    time: i64,
}

fn tzset_rule(s: &str) -> Option<(Rule, &str)> {
    let mut r = Rule { kind: RuleKind::Julian, day: 0, week: 0, mon: 0, time: 0 };
    let mut s = s;
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('J') {
        let (jday, rest) = tzset_num(rest, 1, 365)?;
        r.kind = RuleKind::Julian;
        r.day = jday;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('M') {
        let (mon, rest) = tzset_num(rest, 1, 12)?;
        let rest = rest.strip_prefix('.')?;
        let (week, rest) = tzset_num(rest, 1, 5)?;
        let rest = rest.strip_prefix('.')?;
        let (day, rest) = tzset_num(rest, 0, 6)?;
        r.kind = RuleKind::MonthWeekDay;
        r.day = day;
        r.week = week;
        r.mon = mon;
        s = rest;
    } else {
        let (day, rest) = tzset_num(s, 0, 365)?;
        r.kind = RuleKind::Doy;
        r.day = day;
        s = rest;
    }
    if s.is_empty() || !s.starts_with('/') {
        r.time = 2 * 3600;
        return Some((r, s));
    }
    let (off, rest) = tzset_offset(&s[1..])?;
    r.time = off;
    Some((r, rest))
}

fn tzset_num(s: &str, min: i64, max: i64) -> Option<(i64, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut num: i64 = 0;
    for (i, c) in s.char_indices() {
        if !c.is_ascii_digit() {
            if i == 0 || num < min {
                return None;
            }
            return Some((num, &s[i..]));
        }
        num = num * 10 + (c as i64 - '0' as i64);
        if num > max {
            return None;
        }
    }
    if num < min {
        return None;
    }
    Some((num, ""))
}

fn tzrule_time(year: i64, r: &Rule, off: i64) -> i64 {
    let s = match r.kind {
        RuleKind::Julian => {
            let mut s = (r.day - 1) * 86400;
            if is_leap(year) && r.day >= 60 {
                s += 86400;
            }
            s
        }
        RuleKind::Doy => r.day * 86400,
        RuleKind::MonthWeekDay => {
            let m1 = (r.mon + 9) % 12 + 1;
            let mut yy0 = year;
            if r.mon <= 2 {
                yy0 -= 1;
            }
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = r.day - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..r.week {
                if d + 7 >= days_in(r.mon, year) {
                    break;
                }
                d += 7;
            }
            d += days_before(r.mon) as i64;
            if is_leap(year) && r.mon > 2 {
                d += 1;
            }
            d * 86400
        }
    };
    s + r.time - off
}

// ---- calendar ----

pub fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const DAYS_BEFORE: [i32; 13] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365];

fn days_before(month: i64) -> i32 {
    DAYS_BEFORE[(month - 1) as usize]
}

pub fn days_in(month: i64, year: i64) -> i64 {
    if month == 2 && is_leap(year) {
        return 29;
    }
    (DAYS_BEFORE[month as usize] - DAYS_BEFORE[(month - 1) as usize]) as i64
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day, yday 0-based) of a unix second (already shifted by the zone offset).
fn absdate(sec: i64, _full: bool) -> (i64, i64, i64, i64) {
    let days = sec.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let yday = days - days_from_civil(y, 1, 1);
    (y, m, d, yday)
}

// ---- Time ----

#[derive(Clone, Debug)]
pub struct Time {
    pub sec: i64,
    pub nsec: i64,
    pub loc: Location,
}

pub struct Civil {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    pub nsec: i64,
    pub yday: i64, // 1-based
    pub wday: i64, // 0 = Sunday
    pub zone: String,
    pub offset: i64,
}

impl Time {
    pub fn unix(sec: i64, nsec: i64, loc: Location) -> Time {
        Time { sec: sec + nsec.div_euclid(1_000_000_000), nsec: nsec.rem_euclid(1_000_000_000), loc }
    }

    pub fn now() -> Time {
        if std::env::var("COLLABO_YQ_TEST_NOW").is_ok() {
            // yq's tests fix Now at 2021-05-19T01:02:03.000000004Z
            return Time { sec: 1621386123, nsec: 4, loc: Location::utc() };
        }
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Time { sec: d.as_secs() as i64, nsec: d.subsec_nanos() as i64, loc: local().clone() }
    }

    /// time.Date: normalises out-of-range fields; resolves the offset in loc.
    pub fn date(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64, nsec: i64, loc: Location) -> Time {
        // normalise month
        let m0 = month - 1;
        let year = year + m0.div_euclid(12);
        let month = m0.rem_euclid(12) + 1;
        let mut s = sec + nsec.div_euclid(1_000_000_000);
        let ns = nsec.rem_euclid(1_000_000_000);
        s += min * 60 + hour * 3600;
        let days = days_from_civil(year, month, 1) + day - 1;
        let unix = days * 86400 + s;
        // adjust for the zone: look up the offset at unix - offset
        let (_, offset, start, end, _) = loc.lookup(unix);
        let mut utc = unix;
        if offset != 0 {
            utc = unix - offset;
            if utc < start || utc >= end {
                let (_, off2, ..) = loc.lookup(utc);
                utc = unix - off2;
            }
        }
        Time { sec: utc, nsec: ns, loc }
    }

    pub fn civil(&self) -> Civil {
        let (zone, offset, ..) = self.loc.lookup(self.sec);
        let local = self.sec + offset;
        let days = local.div_euclid(86400);
        let secs = local.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        let yday = days - days_from_civil(y, 1, 1) + 1;
        let wday = (days + 4).rem_euclid(7);
        Civil { year: y, month: m, day: d, hour: secs / 3600, min: secs % 3600 / 60, sec: secs % 60, nsec: self.nsec, yday, wday, zone, offset }
    }

    pub fn in_loc(&self, loc: &Location) -> Time {
        Time { sec: self.sec, nsec: self.nsec, loc: loc.clone() }
    }

    pub fn format(&self, layout: &str) -> String {
        format(self, layout)
    }
}

// ---- layouts ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Std {
    None,
    LongMonth,
    Month,
    NumMonth,
    ZeroMonth,
    LongWeekDay,
    WeekDay,
    Day,
    UnderDay,
    ZeroDay,
    UnderYearDay,
    ZeroYearDay,
    Hour,
    Hour12,
    ZeroHour12,
    Minute,
    ZeroMinute,
    Second,
    ZeroSecond,
    LongYear,
    Year,
    PM,
    Pm,
    TZ,
    ISO8601TZ,
    ISO8601SecondsTZ,
    ISO8601ShortTZ,
    ISO8601ColonTZ,
    ISO8601ColonSecondsTZ,
    NumTZ,
    NumSecondsTz,
    NumShortTZ,
    NumColonTZ,
    NumColonSecondsTZ,
    /// (digits, '.' or ',')
    FracSecond0(usize, u8),
    FracSecond9(usize, u8),
}

const LONG_MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const SHORT_MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const LONG_DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const SHORT_DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn starts_with_lower(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|c| c.is_ascii_lowercase())
}

fn next_std_chunk(layout: &str) -> (&str, Std, &str) {
    let l = layout.as_bytes();
    let n = l.len();
    let mut i = 0;
    while i < n {
        let c = l[i];
        match c {
            b'J' => {
                if n >= i + 3 && &layout[i..i + 3] == "Jan" {
                    if n >= i + 7 && &layout[i..i + 7] == "January" {
                        return (&layout[..i], Std::LongMonth, &layout[i + 7..]);
                    }
                    if !starts_with_lower(&layout[i + 3..]) {
                        return (&layout[..i], Std::Month, &layout[i + 3..]);
                    }
                }
            }
            b'M' => {
                if n >= i + 3 {
                    if &layout[i..i + 3] == "Mon" {
                        if n >= i + 6 && &layout[i..i + 6] == "Monday" {
                            return (&layout[..i], Std::LongWeekDay, &layout[i + 6..]);
                        }
                        if !starts_with_lower(&layout[i + 3..]) {
                            return (&layout[..i], Std::WeekDay, &layout[i + 3..]);
                        }
                    }
                    if &layout[i..i + 3] == "MST" {
                        return (&layout[..i], Std::TZ, &layout[i + 3..]);
                    }
                }
            }
            b'0' => {
                if n >= i + 2 && (b'1'..=b'6').contains(&l[i + 1]) {
                    let std = [Std::ZeroMonth, Std::ZeroDay, Std::ZeroHour12, Std::ZeroMinute, Std::ZeroSecond, Std::Year]
                        [(l[i + 1] - b'1') as usize];
                    return (&layout[..i], std, &layout[i + 2..]);
                }
                if n >= i + 3 && l[i + 1] == b'0' && l[i + 2] == b'2' {
                    return (&layout[..i], Std::ZeroYearDay, &layout[i + 3..]);
                }
            }
            b'1' => {
                if n >= i + 2 && l[i + 1] == b'5' {
                    return (&layout[..i], Std::Hour, &layout[i + 2..]);
                }
                return (&layout[..i], Std::NumMonth, &layout[i + 1..]);
            }
            b'2' => {
                if n >= i + 4 && &layout[i..i + 4] == "2006" {
                    return (&layout[..i], Std::LongYear, &layout[i + 4..]);
                }
                return (&layout[..i], Std::Day, &layout[i + 1..]);
            }
            b'_' => {
                if n >= i + 2 && l[i + 1] == b'2' {
                    if n >= i + 5 && &layout[i + 1..i + 5] == "2006" {
                        return (&layout[..i + 1], Std::LongYear, &layout[i + 5..]);
                    }
                    return (&layout[..i], Std::UnderDay, &layout[i + 2..]);
                }
                if n >= i + 3 && l[i + 1] == b'_' && l[i + 2] == b'2' {
                    return (&layout[..i], Std::UnderYearDay, &layout[i + 3..]);
                }
            }
            b'3' => return (&layout[..i], Std::Hour12, &layout[i + 1..]),
            b'4' => return (&layout[..i], Std::Minute, &layout[i + 1..]),
            b'5' => return (&layout[..i], Std::Second, &layout[i + 1..]),
            b'P' => {
                if n >= i + 2 && l[i + 1] == b'M' {
                    return (&layout[..i], Std::PM, &layout[i + 2..]);
                }
            }
            b'p' => {
                if n >= i + 2 && l[i + 1] == b'm' {
                    return (&layout[..i], Std::Pm, &layout[i + 2..]);
                }
            }
            b'-' => {
                for (pat, std) in [
                    ("-070000", Std::NumSecondsTz),
                    ("-07:00:00", Std::NumColonSecondsTZ),
                    ("-0700", Std::NumTZ),
                    ("-07:00", Std::NumColonTZ),
                    ("-07", Std::NumShortTZ),
                ] {
                    if layout[i..].starts_with(pat) {
                        return (&layout[..i], std, &layout[i + pat.len()..]);
                    }
                }
            }
            b'Z' => {
                for (pat, std) in [
                    ("Z070000", Std::ISO8601SecondsTZ),
                    ("Z07:00:00", Std::ISO8601ColonSecondsTZ),
                    ("Z0700", Std::ISO8601TZ),
                    ("Z07:00", Std::ISO8601ColonTZ),
                    ("Z07", Std::ISO8601ShortTZ),
                ] {
                    if layout[i..].starts_with(pat) {
                        return (&layout[..i], std, &layout[i + pat.len()..]);
                    }
                }
            }
            b'.' | b',' => {
                if i + 1 < n && (l[i + 1] == b'0' || l[i + 1] == b'9') {
                    let ch = l[i + 1];
                    let mut j = i + 1;
                    while j < n && l[j] == ch {
                        j += 1;
                    }
                    if !(j < n && l[j].is_ascii_digit()) {
                        let digits = j - (i + 1);
                        let std = if ch == b'9' { Std::FracSecond9(digits, c) } else { Std::FracSecond0(digits, c) };
                        return (&layout[..i], std, &layout[j..]);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    (layout, Std::None, "")
}

fn append_int(b: &mut String, x: i64, width: usize) {
    let mut u = x;
    if x < 0 {
        b.push('-');
        u = -x;
    }
    let s = u.to_string();
    for _ in s.len()..width {
        b.push('0');
    }
    b.push_str(&s);
}

pub fn format(t: &Time, layout: &str) -> String {
    let c = t.civil();
    let mut b = String::new();
    let mut layout = layout;
    loop {
        let (prefix, std, suffix) = next_std_chunk(layout);
        b.push_str(prefix);
        if std == Std::None {
            break;
        }
        layout = suffix;
        match std {
            Std::Year => {
                let mut y = c.year;
                if y < 0 {
                    y = -y;
                }
                append_int(&mut b, y % 100, 2);
            }
            Std::LongYear => append_int(&mut b, c.year, 4),
            Std::Month => b.push_str(SHORT_MONTHS[(c.month - 1) as usize]),
            Std::LongMonth => b.push_str(LONG_MONTHS[(c.month - 1) as usize]),
            Std::NumMonth => append_int(&mut b, c.month, 0),
            Std::ZeroMonth => append_int(&mut b, c.month, 2),
            Std::WeekDay => b.push_str(SHORT_DAYS[c.wday as usize]),
            Std::LongWeekDay => b.push_str(LONG_DAYS[c.wday as usize]),
            Std::Day => append_int(&mut b, c.day, 0),
            Std::UnderDay => {
                if c.day < 10 {
                    b.push(' ');
                }
                append_int(&mut b, c.day, 0);
            }
            Std::ZeroDay => append_int(&mut b, c.day, 2),
            Std::UnderYearDay => {
                if c.yday < 100 {
                    b.push(' ');
                    if c.yday < 10 {
                        b.push(' ');
                    }
                }
                append_int(&mut b, c.yday, 0);
            }
            Std::ZeroYearDay => append_int(&mut b, c.yday, 3),
            Std::Hour => append_int(&mut b, c.hour, 2),
            Std::Hour12 | Std::ZeroHour12 => {
                let mut hr = c.hour % 12;
                if hr == 0 {
                    hr = 12;
                }
                append_int(&mut b, hr, if std == Std::ZeroHour12 { 2 } else { 0 });
            }
            Std::Minute => append_int(&mut b, c.min, 0),
            Std::ZeroMinute => append_int(&mut b, c.min, 2),
            Std::Second => append_int(&mut b, c.sec, 0),
            Std::ZeroSecond => append_int(&mut b, c.sec, 2),
            Std::PM => b.push_str(if c.hour >= 12 { "PM" } else { "AM" }),
            Std::Pm => b.push_str(if c.hour >= 12 { "pm" } else { "am" }),
            Std::ISO8601TZ
            | Std::ISO8601ColonTZ
            | Std::ISO8601SecondsTZ
            | Std::ISO8601ShortTZ
            | Std::ISO8601ColonSecondsTZ
            | Std::NumTZ
            | Std::NumColonTZ
            | Std::NumSecondsTz
            | Std::NumShortTZ
            | Std::NumColonSecondsTZ => {
                let iso = matches!(
                    std,
                    Std::ISO8601TZ | Std::ISO8601ColonTZ | Std::ISO8601SecondsTZ | Std::ISO8601ShortTZ | Std::ISO8601ColonSecondsTZ
                );
                if c.offset == 0 && iso {
                    b.push('Z');
                    continue;
                }
                let mut zone = c.offset / 60;
                let mut absoffset = c.offset;
                if zone < 0 {
                    b.push('-');
                    zone = -zone;
                    absoffset = -absoffset;
                } else {
                    b.push('+');
                }
                append_int(&mut b, zone / 60, 2);
                if matches!(std, Std::ISO8601ColonTZ | Std::NumColonTZ | Std::ISO8601ColonSecondsTZ | Std::NumColonSecondsTZ) {
                    b.push(':');
                }
                if std != Std::NumShortTZ && std != Std::ISO8601ShortTZ {
                    append_int(&mut b, zone % 60, 2);
                }
                if matches!(std, Std::ISO8601SecondsTZ | Std::NumSecondsTz | Std::NumColonSecondsTZ | Std::ISO8601ColonSecondsTZ) {
                    if std == Std::NumColonSecondsTZ || std == Std::ISO8601ColonSecondsTZ {
                        b.push(':');
                    }
                    append_int(&mut b, absoffset % 60, 2);
                }
            }
            Std::TZ => {
                if !c.zone.is_empty() {
                    b.push_str(&c.zone);
                } else {
                    let mut zone = c.offset / 60;
                    if zone < 0 {
                        b.push('-');
                        zone = -zone;
                    } else {
                        b.push('+');
                    }
                    append_int(&mut b, zone / 60, 2);
                    append_int(&mut b, zone % 60, 2);
                }
            }
            Std::FracSecond0(n, sep) | Std::FracSecond9(n, sep) => {
                let trim = matches!(std, Std::FracSecond9(..));
                if trim && (n == 0 || c.nsec == 0) {
                    continue;
                }
                b.push(sep as char);
                let mut digits = format!("{:09}", c.nsec);
                if n < 9 {
                    digits.truncate(n);
                }
                b.push_str(&digits);
                if trim {
                    while b.ends_with('0') {
                        b.pop();
                    }
                    if b.ends_with(sep as char) {
                        b.pop();
                    }
                }
            }
            Std::None => {}
        }
    }
    b
}

// ---- parsing ----

pub fn quote(s: &str) -> String {
    let mut buf = String::from("\"");
    for c in s.chars() {
        if c as u32 >= 0x80 || (c as u32) < 0x20 {
            let mut tmp = [0u8; 4];
            for byte in c.encode_utf8(&mut tmp).bytes() {
                buf.push_str(&format!("\\x{:02x}", byte));
            }
        } else {
            if c == '"' || c == '\\' {
                buf.push('\\');
            }
            buf.push(c);
        }
    }
    buf.push('"');
    buf
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub layout: String,
    pub value: String,
    pub layout_elem: String,
    pub value_elem: String,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(
                f,
                "parsing time {} as {}: cannot parse {} as {}",
                quote(&self.value),
                quote(&self.layout),
                quote(&self.value_elem),
                quote(&self.layout_elem)
            )
        } else {
            write!(f, "parsing time {}{}", quote(&self.value), self.message)
        }
    }
}

fn is_digit_at(s: &str, i: usize) -> bool {
    s.as_bytes().get(i).is_some_and(|c| c.is_ascii_digit())
}

fn getnum(s: &str, fixed: bool) -> Result<(i64, &str), ()> {
    if !is_digit_at(s, 0) {
        return Err(());
    }
    let b = s.as_bytes();
    if !is_digit_at(s, 1) {
        if fixed {
            return Err(());
        }
        return Ok(((b[0] - b'0') as i64, &s[1..]));
    }
    Ok((((b[0] - b'0') as i64) * 10 + (b[1] - b'0') as i64, &s[2..]))
}

fn getnum3(s: &str, fixed: bool) -> Result<(i64, &str), ()> {
    let mut n = 0i64;
    let mut i = 0;
    while i < 3 && is_digit_at(s, i) {
        n = n * 10 + (s.as_bytes()[i] - b'0') as i64;
        i += 1;
    }
    if i == 0 || fixed && i != 3 {
        return Err(());
    }
    Ok((n, &s[i..]))
}

fn cutspace(s: &str) -> &str {
    s.trim_start_matches(' ')
}

fn skip<'a>(value: &'a str, prefix: &str) -> Result<&'a str, &'a str> {
    let mut value = value;
    let mut prefix = prefix;
    while !prefix.is_empty() {
        if prefix.as_bytes()[0] == b' ' {
            if !value.is_empty() && value.as_bytes()[0] != b' ' {
                return Err(value);
            }
            prefix = cutspace(prefix);
            value = cutspace(value);
            continue;
        }
        if value.is_empty() || value.as_bytes()[0] != prefix.as_bytes()[0] {
            return Err(value);
        }
        // advance one byte (prefix bytes are matched bytewise as Go does)
        prefix = &prefix[1..];
        let mut k = 1;
        while !value.is_char_boundary(k) {
            k += 1;
        }
        value = &value[k..];
        // if the prefix was multi-byte, its next bytes must match too; char boundaries
        // line up for valid UTF-8 inputs
        while !prefix.is_char_boundary(0) {
            prefix = &prefix[1..];
        }
    }
    Ok(value)
}

fn lookup<'a>(tab: &[&str], val: &'a str) -> Result<(usize, &'a str), ()> {
    for (i, v) in tab.iter().enumerate() {
        if val.len() >= v.len() && val.as_bytes()[..v.len()].eq_ignore_ascii_case(v.as_bytes()) {
            return Ok((i, &val[v.len()..]));
        }
    }
    Err(())
}

fn leading_int(s: &str) -> Result<(i64, &str), ()> {
    let mut x: i64 = 0;
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() && b[i].is_ascii_digit() {
        if x > (i64::MAX - 10) / 10 {
            return Err(());
        }
        x = x * 10 + (b[i] - b'0') as i64;
        i += 1;
    }
    Ok((x, &s[i..]))
}

fn atoi(s: &str) -> Result<i64, ()> {
    let mut neg = false;
    let mut s = s;
    if !s.is_empty() && (s.as_bytes()[0] == b'-' || s.as_bytes()[0] == b'+') {
        neg = s.as_bytes()[0] == b'-';
        s = &s[1..];
    }
    let (q, rem) = leading_int(s)?;
    if !rem.is_empty() {
        return Err(());
    }
    Ok(if neg { -q } else { q })
}

fn parse_nanoseconds(value: &str, nbytes: usize) -> Result<(i64, String), ()> {
    let b = value.as_bytes();
    if b[0] != b'.' && b[0] != b',' {
        return Err(());
    }
    let mut nbytes = nbytes;
    let mut v = value;
    if nbytes > 10 {
        v = &value[..10];
        nbytes = 10;
    }
    let mut ns = atoi(&v[1..nbytes])?;
    if ns < 0 {
        return Ok((0, "fractional second".into()));
    }
    for _ in 0..(10 - nbytes) {
        ns *= 10;
    }
    Ok((ns, String::new()))
}

fn parse_signed_offset(value: &str) -> usize {
    let b = value.as_bytes();
    if b.is_empty() || (b[0] != b'-' && b[0] != b'+') {
        return 0;
    }
    match leading_int(&value[1..]) {
        Ok((x, rem)) => {
            if value[1..].len() == rem.len() || x > 23 {
                return 0;
            }
            value.len() - rem.len()
        }
        Err(_) => 0,
    }
}

fn parse_gmt(value: &str) -> usize {
    let v = &value[3..];
    if v.is_empty() {
        return 3;
    }
    3 + parse_signed_offset(v)
}

fn parse_time_zone(value: &str) -> Option<usize> {
    let b = value.as_bytes();
    if b.len() < 3 {
        return None;
    }
    if b.len() >= 4 && (&value[..4] == "ChST" || &value[..4] == "MeST") {
        return Some(4);
    }
    if &value[..3] == "GMT" {
        return Some(parse_gmt(value));
    }
    if b[0] == b'+' || b[0] == b'-' {
        let l = parse_signed_offset(value);
        return if l > 0 { Some(l) } else { None };
    }
    let mut n_upper = 0;
    while n_upper < 6 {
        if n_upper >= b.len() || !b[n_upper].is_ascii_uppercase() {
            break;
        }
        n_upper += 1;
    }
    match n_upper {
        5 => {
            if b[4] == b'T' {
                return Some(5);
            }
        }
        4 => {
            if b[3] == b'T' || &value[..4] == "WITA" {
                return Some(4);
            }
        }
        3 => return Some(3),
        _ => {}
    }
    None
}

/// time.Parse(layout, value): UTC unless the value names a zone.
pub fn parse(layout: &str, value: &str) -> Result<Time, ParseError> {
    parse_in(layout, value, &Location::utc(), local())
}

fn perr(layout: &str, value: &str, le: &str, ve: &str, msg: &str) -> ParseError {
    ParseError { layout: layout.into(), value: value.into(), layout_elem: le.into(), value_elem: ve.into(), message: msg.into() }
}

pub fn parse_in(alayout: &str, avalue: &str, default_loc: &Location, local: &Location) -> Result<Time, ParseError> {
    let mut layout = alayout;
    let mut value = avalue;
    let mut am_set = false;
    let mut pm_set = false;
    let (mut year, mut month, mut day, mut yday) = (0i64, -1i64, -1i64, -1i64);
    let (mut hour, mut min, mut sec, mut nsec) = (0i64, 0i64, 0i64, 0i64);
    let mut z: Option<Location> = None;
    let mut zone_offset: i64 = -1;
    let mut zone_name = String::new();
    loop {
        let (prefix, std, suffix) = next_std_chunk(layout);
        let stdstr = &layout[prefix.len()..layout.len() - suffix.len()];
        value = match skip(value, prefix) {
            Ok(v) => v,
            Err(v) => return Err(perr(alayout, avalue, prefix, v, "")),
        };
        if std == Std::None {
            if !value.is_empty() {
                return Err(perr(alayout, avalue, "", value, &format!(": extra text: {}", quote(value))));
            }
            break;
        }
        layout = suffix;
        let hold = value;
        let mut range_err = String::new();
        let mut bad = false;
        macro_rules! num {
            ($e:expr) => {
                match $e {
                    Ok((n, rest)) => {
                        value = rest;
                        n
                    }
                    Err(_) => {
                        bad = true;
                        0
                    }
                }
            };
        }
        match std {
            Std::Year => {
                if value.len() < 2 {
                    bad = true;
                } else {
                    let p = &value[..2];
                    value = &value[2..];
                    match atoi(p) {
                        Ok(y) => year = if y >= 69 { y + 1900 } else { y + 2000 },
                        Err(_) => bad = true,
                    }
                }
            }
            Std::LongYear => {
                if value.len() < 4 || !is_digit_at(value, 0) || !value.is_char_boundary(4) {
                    bad = true;
                } else {
                    let p = &value[..4];
                    value = &value[4..];
                    match atoi(p) {
                        Ok(y) => year = y,
                        Err(_) => bad = true,
                    }
                }
            }
            Std::Month | Std::LongMonth => {
                let tab: &[&str] = if std == Std::Month { &SHORT_MONTHS } else { &LONG_MONTHS };
                match lookup(tab, value) {
                    Ok((i, rest)) => {
                        month = i as i64 + 1;
                        value = rest;
                    }
                    Err(_) => {
                        bad = true;
                        month = 0;
                    }
                }
            }
            Std::NumMonth | Std::ZeroMonth => {
                month = num!(getnum(value, std == Std::ZeroMonth));
                if !bad && (month <= 0 || 12 < month) {
                    range_err = "month".into();
                }
            }
            Std::WeekDay | Std::LongWeekDay => {
                let tab: &[&str] = if std == Std::WeekDay { &SHORT_DAYS } else { &LONG_DAYS };
                match lookup(tab, value) {
                    Ok((_, rest)) => value = rest,
                    Err(_) => bad = true,
                }
            }
            Std::Day | Std::UnderDay | Std::ZeroDay => {
                if std == Std::UnderDay && value.starts_with(' ') {
                    value = &value[1..];
                }
                day = num!(getnum(value, std == Std::ZeroDay));
            }
            Std::UnderYearDay | Std::ZeroYearDay => {
                for _ in 0..2 {
                    if std == Std::UnderYearDay && value.starts_with(' ') {
                        value = &value[1..];
                    }
                }
                yday = num!(getnum3(value, std == Std::ZeroYearDay));
            }
            Std::Hour => {
                hour = num!(getnum(value, false));
                if !(0..24).contains(&hour) {
                    range_err = "hour".into();
                }
            }
            Std::Hour12 | Std::ZeroHour12 => {
                hour = num!(getnum(value, std == Std::ZeroHour12));
                if !(0..=12).contains(&hour) {
                    range_err = "hour".into();
                }
            }
            Std::Minute | Std::ZeroMinute => {
                min = num!(getnum(value, std == Std::ZeroMinute));
                if !(0..60).contains(&min) {
                    range_err = "minute".into();
                }
            }
            Std::Second | Std::ZeroSecond => {
                sec = num!(getnum(value, std == Std::ZeroSecond));
                if !bad {
                    if !(0..60).contains(&sec) {
                        range_err = "second".into();
                    } else if value.len() >= 2 && (value.as_bytes()[0] == b'.' || value.as_bytes()[0] == b',') && is_digit_at(value, 1) {
                        let (_, next, _) = next_std_chunk(layout);
                        if !matches!(next, Std::FracSecond0(..) | Std::FracSecond9(..)) {
                            let mut n = 2;
                            while n < value.len() && is_digit_at(value, n) {
                                n += 1;
                            }
                            match parse_nanoseconds(value, n) {
                                Ok((ns, re)) => {
                                    nsec = ns;
                                    range_err = re;
                                }
                                Err(_) => bad = true,
                            }
                            value = &value[n..];
                        }
                    }
                }
            }
            Std::PM | Std::Pm => {
                if value.len() < 2 || !value.is_char_boundary(2) {
                    bad = true;
                } else {
                    let p = &value[..2];
                    value = &value[2..];
                    let (pm, am) = if std == Std::PM { ("PM", "AM") } else { ("pm", "am") };
                    if p == pm {
                        pm_set = true;
                    } else if p == am {
                        am_set = true;
                    } else {
                        bad = true;
                    }
                }
            }
            Std::ISO8601TZ
            | Std::ISO8601ShortTZ
            | Std::ISO8601ColonTZ
            | Std::ISO8601SecondsTZ
            | Std::ISO8601ColonSecondsTZ
            | Std::NumTZ
            | Std::NumShortTZ
            | Std::NumColonTZ
            | Std::NumSecondsTz
            | Std::NumColonSecondsTZ => {
                let iso = matches!(
                    std,
                    Std::ISO8601TZ | Std::ISO8601ShortTZ | Std::ISO8601ColonTZ | Std::ISO8601SecondsTZ | Std::ISO8601ColonSecondsTZ
                );
                if iso && value.starts_with('Z') {
                    value = &value[1..];
                    z = Some(Location::utc());
                } else {
                    let vb = value.as_bytes();
                    let parts: Option<(&str, &str, &str, &str, &str)> = if std == Std::ISO8601ColonTZ || std == Std::NumColonTZ {
                        if vb.len() < 6 || vb[3] != b':' || !value.is_char_boundary(6) {
                            None
                        } else {
                            Some((&value[0..1], &value[1..3], &value[4..6], "00", &value[6..]))
                        }
                    } else if std == Std::NumShortTZ || std == Std::ISO8601ShortTZ {
                        if vb.len() < 3 || !value.is_char_boundary(3) {
                            None
                        } else {
                            Some((&value[0..1], &value[1..3], "00", "00", &value[3..]))
                        }
                    } else if std == Std::ISO8601ColonSecondsTZ || std == Std::NumColonSecondsTZ {
                        if vb.len() < 9 || vb[3] != b':' || vb[6] != b':' || !value.is_char_boundary(9) {
                            None
                        } else {
                            Some((&value[0..1], &value[1..3], &value[4..6], &value[7..9], &value[9..]))
                        }
                    } else if std == Std::ISO8601SecondsTZ || std == Std::NumSecondsTz {
                        if vb.len() < 7 || !value.is_char_boundary(7) {
                            None
                        } else {
                            Some((&value[0..1], &value[1..3], &value[3..5], &value[5..7], &value[7..]))
                        }
                    } else if vb.len() < 5 || !value.is_char_boundary(5) {
                        None
                    } else {
                        Some((&value[0..1], &value[1..3], &value[3..5], "00", &value[5..]))
                    };
                    match parts {
                        None => bad = true,
                        Some((sign, h, m, s, rest)) => {
                            value = rest;
                            let hr = getnum(h, true);
                            let mm = getnum(m, true);
                            let ss = getnum(s, true);
                            match (hr, mm, ss) {
                                (Ok((hr, _)), Ok((mm, _)), Ok((ss, _))) => {
                                    if hr > 24 {
                                        range_err = "time zone offset hour".into();
                                    }
                                    if mm > 60 {
                                        range_err = "time zone offset minute".into();
                                    }
                                    if ss > 60 {
                                        range_err = "time zone offset second".into();
                                    }
                                    zone_offset = (hr * 60 + mm) * 60 + ss;
                                    match sign {
                                        "+" => {}
                                        "-" => zone_offset = -zone_offset,
                                        _ => bad = true,
                                    }
                                }
                                _ => bad = true,
                            }
                        }
                    }
                }
            }
            Std::TZ => {
                if value.len() >= 3 && &value.as_bytes()[..3] == b"UTC" {
                    z = Some(Location::utc());
                    value = &value[3..];
                } else {
                    match parse_time_zone(value) {
                        Some(n) => {
                            zone_name = value[..n].to_string();
                            value = &value[n..];
                        }
                        None => bad = true,
                    }
                }
            }
            Std::FracSecond0(digits, _) => {
                let ndigit = 1 + digits;
                if value.len() < ndigit || !value.is_char_boundary(ndigit) {
                    bad = true;
                } else {
                    match parse_nanoseconds(value, ndigit) {
                        Ok((ns, re)) => {
                            nsec = ns;
                            range_err = re;
                        }
                        Err(_) => bad = true,
                    }
                    value = &value[ndigit..];
                }
            }
            Std::FracSecond9(..) => {
                let vb = value.as_bytes();
                if vb.len() < 2 || !(vb[0] == b'.' || vb[0] == b',') || !vb[1].is_ascii_digit() {
                    // omitted
                } else {
                    let mut i = 0;
                    while i + 1 < vb.len() && vb[i + 1].is_ascii_digit() {
                        i += 1;
                    }
                    match parse_nanoseconds(value, 1 + i) {
                        Ok((ns, re)) => {
                            nsec = ns;
                            range_err = re;
                        }
                        Err(_) => bad = true,
                    }
                    value = &value[1 + i..];
                }
            }
            Std::None => {}
        }
        if !range_err.is_empty() {
            return Err(perr(alayout, avalue, stdstr, value, &format!(": {range_err} out of range")));
        }
        if bad {
            return Err(perr(alayout, avalue, stdstr, hold, ""));
        }
    }
    if pm_set && hour < 12 {
        hour += 12;
    } else if am_set && hour == 12 {
        hour = 0;
    }
    if yday >= 0 {
        let mut d = 0;
        let mut m = 0;
        let mut yd = yday;
        if is_leap(year) {
            if yd == 31 + 29 {
                m = 2;
                d = 29;
            } else if yd > 31 + 29 {
                yd -= 1;
            }
        }
        if !(1..=365).contains(&yd) {
            return Err(perr(alayout, avalue, "", value, ": day-of-year out of range"));
        }
        if m == 0 {
            m = (yd - 1) / 31 + 1;
            if (days_before(m + 1) as i64) < yd {
                m += 1;
            }
            d = yd - days_before(m) as i64;
        }
        if month >= 0 && month != m {
            return Err(perr(alayout, avalue, "", value, ": day-of-year does not match month"));
        }
        month = m;
        if day >= 0 && day != d {
            return Err(perr(alayout, avalue, "", value, ": day-of-year does not match day"));
        }
        day = d;
    } else {
        if month < 0 {
            month = 1;
        }
        if day < 0 {
            day = 1;
        }
    }
    if day < 1 || day > days_in(month, year) {
        return Err(perr(alayout, avalue, "", value, ": day out of range"));
    }
    if let Some(z) = z {
        return Ok(Time::date(year, month, day, hour, min, sec, nsec, z));
    }
    if zone_offset != -1 {
        let mut t = Time::date(year, month, day, hour, min, sec, nsec, Location::utc());
        t.sec -= zone_offset;
        let (name, offset, ..) = local.lookup(t.sec);
        if offset == zone_offset && (zone_name.is_empty() || name == zone_name) {
            t.loc = local.clone();
            return Ok(t);
        }
        t.loc = Location::fixed(&zone_name, zone_offset);
        return Ok(t);
    }
    if !zone_name.is_empty() {
        let mut t = Time::date(year, month, day, hour, min, sec, nsec, Location::utc());
        if let Some(offset) = local.lookup_name(&zone_name, t.sec) {
            t.sec -= offset;
            t.loc = local.clone();
            return Ok(t);
        }
        let mut offset = 0;
        if zone_name.len() > 3 && zone_name.starts_with("GMT") {
            offset = atoi(&zone_name[3..]).unwrap_or(0) * 3600;
        }
        t.loc = Location::fixed(&zone_name, offset);
        return Ok(t);
    }
    Ok(Time::date(year, month, day, hour, min, sec, nsec, default_loc.clone()))
}

pub const RFC3339: &str = "2006-01-02T15:04:05Z07:00";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = parse(RFC3339, "2001-12-14T21:59:43.1-05:00").unwrap();
        assert_eq!(t.format(RFC3339), "2001-12-14T21:59:43-05:00");
        assert_eq!(t.format("2006-01-02T15:04:05.999999999Z07:00"), "2001-12-14T21:59:43.1-05:00");
        assert_eq!(t.format("Monday, 02-Jan-06 at 3:04PM MST"), "Friday, 14-Dec-01 at 9:59PM -0500");
        let u = parse("2006-1-2", "2001-12-14").unwrap();
        assert_eq!(u.format(RFC3339), "2001-12-14T00:00:00Z");
        assert!(parse("2006-1-2", "2001-02-30").is_err());
        assert_eq!(
            parse(RFC3339, "2001-12-14").unwrap_err().to_string(),
            "parsing time \"2001-12-14\" as \"2006-01-02T15:04:05Z07:00\": cannot parse \"\" as \"T\""
        );
    }
}
