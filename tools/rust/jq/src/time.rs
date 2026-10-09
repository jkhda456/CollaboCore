//! Dates: jq's ISO 8601 parser (iso8601.c), and strptime/strftime/mktime/gmtime/localtime
//! through the C library, with jq's broken-down time arrays
//! [year, month (0-11), mday, hours, minutes, seconds (with fraction), wday, yday].

use crate::interp::{err, Flow};
use crate::value::{dump_trunc, Value};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long};

#[repr(C)]
#[derive(Clone, Copy)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *const c_char,
}

impl Tm {
    fn zero() -> Tm {
        Tm { tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0, tm_mon: 0, tm_year: 0, tm_wday: 0, tm_yday: 0, tm_isdst: 0, tm_gmtoff: 0, tm_zone: std::ptr::null() }
    }
}

extern "C" {
    #[link_name = "strptime"]
    fn c_strptime(s: *const c_char, fmt: *const c_char, tm: *mut Tm) -> *const c_char;
    #[link_name = "strftime"]
    fn c_strftime(buf: *mut c_char, max: usize, fmt: *const c_char, tm: *const Tm) -> usize;
    #[link_name = "timegm"]
    fn c_timegm(tm: *mut Tm) -> libc::time_t;
    #[link_name = "mktime"]
    fn c_mktime(tm: *mut Tm) -> libc::time_t;
    #[link_name = "gmtime_r"]
    fn c_gmtime_r(t: *const libc::time_t, tm: *mut Tm) -> *mut Tm;
    #[link_name = "localtime_r"]
    fn c_localtime_r(t: *const libc::time_t, tm: *mut Tm) -> *mut Tm;
}

fn num(x: f64) -> Value {
    Value::num(x)
}

fn tm2jv(tm: &Tm, fsecs: f64) -> Value {
    Value::arr(vec![
        num(tm.tm_year as f64 + 1900.0),
        num(tm.tm_mon as f64),
        num(tm.tm_mday as f64),
        num(tm.tm_hour as f64),
        num(tm.tm_min as f64),
        num(tm.tm_sec as f64 + (fsecs - fsecs.floor())),
        num(tm.tm_wday as f64),
        num(tm.tm_yday as f64),
    ])
}

fn jv2tm(a: &Value, local: bool) -> Option<Tm> {
    let Value::Arr(items) = a else { return None };
    let mut f = [0 as c_int; 8];
    for (i, slot) in f.iter_mut().enumerate() {
        let Some(n) = items.get(i) else { break };
        let Value::Num(n) = n else { return None };
        if n.f.is_nan() {
            return None;
        }
        let mut d = n.f;
        if i == 0 {
            d -= 1900.0;
        }
        *slot = if d < c_int::MIN as f64 {
            c_int::MIN
        } else if d > c_int::MAX as f64 {
            c_int::MAX
        } else {
            d as c_int
        };
    }
    let mut tm = Tm::zero();
    tm.tm_year = f[0];
    tm.tm_mon = f[1];
    tm.tm_mday = f[2];
    tm.tm_hour = f[3];
    tm.tm_min = f[4];
    tm.tm_sec = f[5];
    tm.tm_wday = f[6];
    tm.tm_yday = f[7];
    unsafe {
        if local {
            tm.tm_isdst = -1;
            c_mktime(&mut tm);
        } else {
            c_timegm(&mut tm);
        }
    }
    Some(tm)
}

fn set_tm_wday(tm: &mut Tm) {
    let century = (1900 + tm.tm_year) / 100;
    let mut year = (1900 + tm.tm_year) % 100;
    if tm.tm_mon < 2 {
        year -= 1;
    }
    let mut mon = tm.tm_mon - 1;
    if mon < 1 {
        mon += 12;
    }
    let mut wday = (tm.tm_mday + (2.6 * mon as f64 - 0.2).floor() as c_int + year + (year as f64 / 4.0).floor() as c_int + (century as f64 / 4.0).floor() as c_int - 2 * century) % 7;
    if wday < 0 {
        wday += 7;
    }
    tm.tm_wday = wday;
}

fn set_tm_yday(tm: &mut Tm) {
    const D: [c_int; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut mon = tm.tm_mon;
    let year = 1900 + tm.tm_year;
    let leap = tm.tm_mon > 1 && ((year % 4 == 0 && year % 100 != 0) || year % 400 == 0);
    if mon < 0 {
        mon = -mon;
    }
    if mon > 11 {
        mon %= 12;
    }
    tm.tm_yday = D[mon as usize] + leap as c_int + tm.tm_mday - 1;
}

fn err2(a: &Value, b: &Value, msg: String) -> Flow {
    let _ = (a, b);
    err(msg)
}

pub fn strptime(input: &Value, fmt: &Value) -> Result<Value, Flow> {
    let (Value::Str(s), Value::Str(f)) = (input, fmt) else {
        return Err(err2(input, fmt, "strptime/1 requires string inputs and arguments".into()));
    };
    let mut tm = Tm::zero();
    tm.tm_wday = 8;
    tm.tm_yday = 367;
    let cs = CString::new(s.as_bytes()).map_err(|_| err("strptime/1 requires string inputs and arguments"))?;
    let cf = CString::new(f.as_bytes()).map_err(|_| err("strptime/1 requires string inputs and arguments"))?;
    let end = unsafe { c_strptime(cs.as_ptr(), cf.as_ptr(), &mut tm) };
    let rest = if end.is_null() { None } else { Some(unsafe { CStr::from_ptr(end) }.to_bytes()) };
    let ok = match rest {
        None => false,
        Some(r) => r.is_empty() || r[0].is_ascii_whitespace(),
    };
    if !ok {
        return Err(err(format!("date \"{s}\" does not match format \"{f}\"")));
    }
    if tm.tm_wday == 8 && tm.tm_mday != 0 && (0..=11).contains(&tm.tm_mon) {
        set_tm_wday(&mut tm);
    }
    if tm.tm_yday == 367 && tm.tm_mday != 0 && (0..=11).contains(&tm.tm_mon) {
        set_tm_yday(&mut tm);
    }
    let r = tm2jv(&tm, 0.0);
    let rest = rest.unwrap();
    if !rest.is_empty() {
        let Value::Arr(a) = r else { unreachable!() };
        let mut a = (*a).clone();
        a.push(Value::string(crate::value::utf8_lossy(rest)));
        return Ok(Value::arr(a));
    }
    Ok(r)
}


pub fn mktime(input: &Value) -> Result<Value, Flow> {
    if !matches!(input, Value::Arr(_)) {
        return Err(err("mktime requires array inputs"));
    }
    let Some(mut tm) = jv2tm(input, false) else { return Err(err("mktime requires parsed datetime inputs")) };
    let t = unsafe { c_timegm(&mut tm) };
    if t == -1 {
        return Err(err("invalid gmtime representation"));
    }
    Ok(num(t as f64))
}


/// gmtime (local: localtime).
pub fn gmtime(input: &Value, local: bool) -> Result<Value, Flow> {
    let Value::Num(n) = input else {
        return Err(err(if local { "localtime() requires numeric inputs" } else { "gmtime() requires numeric inputs" }));
    };
    let fsecs = n.f;
    let secs = fsecs.floor() as libc::time_t;
    let mut tm = Tm::zero();
    let r = unsafe {
        if local {
            c_localtime_r(&secs, &mut tm)
        } else {
            c_gmtime_r(&secs, &mut tm)
        }
    };
    if r.is_null() {
        return Err(err("error converting number of seconds since epoch to datetime"));
    }
    Ok(tm2jv(&tm, fsecs))
}

/// strftime (local: strflocaltime).
pub fn strftime(input: &Value, fmt: &Value, local: bool) -> Result<Value, Flow> {
    let name = if local { "strflocaltime" } else { "strftime" };
    let a = match input {
        Value::Num(_) => gmtime(input, local)?,
        Value::Arr(_) => input.clone(),
        _ => return Err(err(format!("{name}/1 requires parsed datetime inputs"))),
    };
    let Value::Str(f) = fmt else { return Err(err(format!("{name}/1 requires a string format"))) };
    let Some(tm) = jv2tm(&a, local) else { return Err(err(format!("{name}/1 requires parsed datetime inputs"))) };
    let cf = CString::new(f.as_bytes()).map_err(|_| err(format!("{name}/1 requires a string format")))?;
    let max = f.len() + 100;
    let mut buf = vec![0u8; max];
    let n = unsafe { c_strftime(buf.as_mut_ptr() as *mut c_char, max, cf.as_ptr(), &tm) };
    if (n == 0 && !f.is_empty()) || n > max {
        return Err(err(format!("{name}/1: unknown system failure")));
    }
    buf.truncate(n);
    Ok(Value::string(crate::value::utf8_lossy(&buf)))
}


// iso8601.c

fn days_from_civil(y: i64, m: i32, d: i32) -> i64 {
    let y = y - (m <= 2) as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i32) -> i32 {
    const D: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if m == 2 && is_leap(y) {
        29
    } else {
        D[(m - 1) as usize]
    }
}

fn iso_wday(days: i64) -> i64 {
    let mut w = (days + 3) % 7;
    if w < 0 {
        w += 7;
    }
    w + 1
}

fn iso_weeks_in_year(y: i64) -> i32 {
    let jan1 = iso_wday(days_from_civil(y, 1, 1));
    if jan1 == 4 || (jan1 == 3 && is_leap(y)) {
        53
    } else {
        52
    }
}

struct Scan<'a> {
    s: &'a [u8],
    p: usize,
}

impl Scan<'_> {
    fn at_end(&self) -> bool {
        self.p == self.s.len()
    }
    fn peek(&self) -> i32 {
        if self.at_end() {
            -1
        } else {
            self.s[self.p] as i32
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.at_end() || self.s[self.p] != c {
            return false;
        }
        self.p += 1;
        true
    }
    fn designator(&mut self, c: u8) -> bool {
        if self.at_end() || (self.s[self.p] != c && self.s[self.p] != c + (b'a' - b'A')) {
            return false;
        }
        self.p += 1;
        true
    }
    fn count_digits(&self) -> usize {
        self.s[self.p..].iter().take_while(|c| c.is_ascii_digit()).count()
    }
    fn digits(&mut self, n: usize) -> Option<i32> {
        if self.s.len() - self.p < n {
            return None;
        }
        let mut v = 0;
        for i in 0..n {
            let c = self.s[self.p + i];
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i32;
        }
        self.p += n;
        Some(v)
    }
}

fn parse_year(s: &mut Scan) -> Option<i64> {
    let mut sign = 1;
    let mut digits = 4;
    if s.peek() == b'+' as i32 || s.peek() == b'-' as i32 {
        sign = if s.s[s.p] == b'-' { -1 } else { 1 };
        s.p += 1;
        digits = 6;
    }
    if s.s.len() - s.p < digits {
        return None;
    }
    let mut v: i64 = 0;
    for i in 0..digits {
        let c = s.s[s.p + i];
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    s.p += digits;
    Some(sign * v)
}

fn civil(y: i64, mon: i32, mday: i32) -> Option<i64> {
    if !(1..=12).contains(&mon) || mday < 1 || mday > days_in_month(y, mon) {
        return None;
    }
    Some(days_from_civil(y, mon, mday))
}

fn ordinal(y: i64, yday: i32) -> Option<i64> {
    if yday < 1 || yday > if is_leap(y) { 366 } else { 365 } {
        return None;
    }
    Some(days_from_civil(y, 1, 1) + yday as i64 - 1)
}

fn week(y: i64, week: i32, wday: i32) -> Option<i64> {
    if week < 1 || week > iso_weeks_in_year(y) || !(1..=7).contains(&wday) {
        return None;
    }
    let jan4 = days_from_civil(y, 1, 4);
    Some(jan4 - (iso_wday(jan4) - 1) + (week as i64 - 1) * 7 + (wday as i64 - 1))
}

fn parse_date(s: &mut Scan) -> Option<(i64, bool)> {
    let year = parse_year(s)?;
    if s.eat(b'-') {
        if s.designator(b'W') {
            let w = s.digits(2)?;
            if !s.eat(b'-') {
                return None;
            }
            let d = s.digits(1)?;
            return week(year, w, d).map(|d| (d, true));
        }
        return match s.count_digits() {
            3 => {
                let yd = s.digits(3)?;
                ordinal(year, yd).map(|d| (d, true))
            }
            2 => {
                let m = s.digits(2)?;
                if !s.eat(b'-') {
                    return None;
                }
                let d = s.digits(2)?;
                civil(year, m, d).map(|d| (d, true))
            }
            _ => None,
        };
    }
    if s.designator(b'W') {
        let w = s.digits(2)?;
        let d = s.digits(1)?;
        return week(year, w, d).map(|d| (d, false));
    }
    match s.count_digits() {
        4 => {
            let m = s.digits(2)?;
            let d = s.digits(2)?;
            civil(year, m, d).map(|d| (d, false))
        }
        3 => {
            let yd = s.digits(3)?;
            ordinal(year, yd).map(|d| (d, false))
        }
        _ => None,
    }
}

fn parse_time(s: &mut Scan, extended: bool) -> Option<(f64, i64)> {
    let mut hh = s.digits(2)?;
    let (mut mm, mut ss, mut unit) = (0, 0, 3600.0);
    let more = |s: &mut Scan| if extended { s.eat(b':') } else { s.count_digits() >= 2 };
    if more(s) {
        mm = s.digits(2)?;
        unit = 60.0;
        if more(s) {
            ss = s.digits(2)?;
            unit = 1.0;
        }
    }
    let mut frac = 0.0;
    if s.peek() == b'.' as i32 || s.peek() == b',' as i32 {
        s.p += 1;
        if s.count_digits() == 0 {
            return None;
        }
        let mut n: u64 = 0;
        let mut digits = 0;
        while s.peek() >= b'0' as i32 && s.peek() <= b'9' as i32 {
            if digits < 15 {
                n = n * 10 + (s.s[s.p] - b'0') as u64;
                digits += 1;
            }
            s.p += 1;
        }
        frac = n as f64 / 10f64.powi(digits);
    }
    let mut carry = 0;
    if hh == 24 {
        if mm != 0 || ss != 0 || frac != 0.0 {
            return None;
        }
        hh = 0;
        carry = 1;
    } else if hh > 24 || mm > 59 || ss > 60 {
        return None;
    } else if ss == 60 {
        ss = 59;
    }
    Some(((hh * 3600 + mm * 60 + ss) as f64 + frac * unit, carry))
}

fn parse_offset(s: &mut Scan) -> Option<i32> {
    if s.designator(b'Z') {
        return Some(0);
    }
    let sign = s.peek();
    if sign != b'+' as i32 && sign != b'-' as i32 {
        return Some(0);
    }
    s.p += 1;
    let oh = s.digits(2)?;
    let had_colon = s.eat(b':');
    let om = match s.digits(2) {
        Some(m) => m,
        None if had_colon => return None,
        None => 0,
    };
    if oh > 23 || om > 59 {
        return None;
    }
    Some(if sign == b'+' as i32 { 1 } else { -1 } * (oh * 3600 + om * 60))
}

pub fn iso8601_parse(buf: &[u8]) -> Option<f64> {
    let mut s = Scan { s: buf, p: 0 };
    let (days, extended) = parse_date(&mut s)?;
    let (mut secs, mut carry, mut offset) = (0.0, 0, 0);
    let sep = s.peek();
    if sep == b'T' as i32 || sep == b't' as i32 || sep == b' ' as i32 {
        s.p += 1;
        let (t, c) = parse_time(&mut s, extended)?;
        secs = t;
        carry = c;
        offset = parse_offset(&mut s)?;
    }
    if !s.at_end() {
        return None;
    }
    Some((days + carry) as f64 * 86400.0 + secs - offset as f64)
}

#[allow(dead_code)]
fn trunc(v: &Value) -> String {
    dump_trunc(v, 30)
}
