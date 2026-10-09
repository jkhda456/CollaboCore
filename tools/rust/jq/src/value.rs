//! jq's values: JSON with objects in insertion order, numbers as doubles that remember the text
//! they were written with (jq 1.7+'s literal numbers: `100000000000000000001` and `1.000` come
//! out as they went in until arithmetic touches them), jq's total order, and its printer.

use indexmap::IndexMap;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::rc::Rc;

pub type Map = IndexMap<Rc<str>, Value>;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Num(Num),
    Str(Rc<str>),
    Arr(Rc<Vec<Value>>),
    Obj(Rc<Map>),
}

/// A number: its double, and the decimal it was written as (canonical decNumber text).
#[derive(Clone, Debug)]
pub struct Num {
    pub f: f64,
    pub lit: Option<Rc<Dec>>,
}

/// A decimal literal: sign, coefficient digits (no leading zeros, "0" for zero), exponent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec {
    pub neg: bool,
    pub digits: String,
    pub exp: i64,
}

impl Dec {
    /// Parses JSON/jq number text ([-]digits[.digits][e[+-]digits]).
    pub fn parse(s: &str) -> Option<Dec> {
        let b = s.as_bytes();
        let mut i = 0;
        let neg = b.first() == Some(&b'-');
        if neg {
            i += 1;
        }
        let mut digits = String::new();
        let mut exp: i64 = 0;
        let mut seen = false;
        while i < b.len() && b[i].is_ascii_digit() {
            digits.push(b[i] as char);
            i += 1;
            seen = true;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                digits.push(b[i] as char);
                exp -= 1;
                i += 1;
                seen = true;
            }
        }
        if !seen {
            return None;
        }
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            i += 1;
            let eneg = match b.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let start = i;
            let mut e: i64 = 0;
            while i < b.len() && b[i].is_ascii_digit() {
                e = (e * 10 + (b[i] - b'0') as i64).min(1 << 40);
                i += 1;
            }
            if i == start {
                return None;
            }
            exp += if eneg { -e } else { e };
        }
        if i != b.len() {
            return None;
        }
        let trimmed = digits.trim_start_matches('0');
        let digits = if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() };
        Some(Dec { neg, digits, exp })
    }

    /// decNumber's to-scientific-string.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        let n = self.digits.len() as i64;
        let adj = self.exp + n - 1;
        if self.exp <= 0 && adj >= -6 {
            if self.exp == 0 {
                s += &self.digits;
            } else {
                let point = n + self.exp;
                if point > 0 {
                    s += &self.digits[..point as usize];
                    s.push('.');
                    s += &self.digits[point as usize..];
                } else {
                    s += "0.";
                    for _ in 0..(-point) {
                        s.push('0');
                    }
                    s += &self.digits;
                }
            }
        } else {
            s += &self.digits[..1];
            if n > 1 {
                s.push('.');
                s += &self.digits[1..];
            }
            s.push('E');
            s.push(if adj < 0 { '-' } else { '+' });
            s += &adj.abs().to_string();
        }
        s
    }

    pub fn is_zero(&self) -> bool {
        self.digits == "0"
    }

    /// Too large or small for a double: printed through its double instead (as jq does for
    /// decNumber infinities).
    pub fn out_of_range(&self) -> bool {
        let adj = self.exp + self.digits.len() as i64 - 1;
        !self.is_zero() && (adj > 999_999_999 || adj < -999_999_999)
    }

    /// Exact comparison (decNumberCompare).
    pub fn cmp(&self, o: &Dec) -> Ordering {
        let za = self.is_zero();
        let zb = o.is_zero();
        if za && zb {
            return Ordering::Equal;
        }
        let sa = if za { 0 } else if self.neg { -1 } else { 1 };
        let sb = if zb { 0 } else if o.neg { -1 } else { 1 };
        if sa != sb {
            return sa.cmp(&sb);
        }
        let mag = {
            let (da, db) = (self.digits.trim_end_matches('0'), o.digits.trim_end_matches('0'));
            let ea = self.exp + (self.digits.len() - da.len()) as i64;
            let eb = o.exp + (o.digits.len() - db.len()) as i64;
            let aa = ea + da.len() as i64;
            let ab = eb + db.len() as i64;
            if aa != ab {
                aa.cmp(&ab)
            } else {
                // Same magnitude: digits compared left to right.
                let l = da.len().max(db.len());
                let pa = format!("{da:0<l$}");
                let pb = format!("{db:0<l$}");
                pa.cmp(&pb)
            }
        };
        if sa < 0 {
            mag.reverse()
        } else {
            mag
        }
    }
}

impl Num {
    pub fn f(f: f64) -> Num {
        Num { f, lit: None }
    }
    pub fn from_literal(s: &str) -> Option<Num> {
        let d = Dec::parse(s)?;
        let f = d.to_f64();
        Some(Num { f, lit: Some(Rc::new(d)) })
    }
}

impl Dec {
    /// jvp_literal_number_to_double: the decimal rounded to 17 digits (half even), then read
    /// as a double.
    pub fn to_f64(&self) -> f64 {
        let mut digits = self.digits.clone().into_bytes();
        let mut exp = self.exp;
        if digits.len() > 17 {
            let cut = digits.len() - 17;
            let rest = &digits[17..];
            let first = rest[0];
            let tail_zero = rest[1..].iter().all(|&c| c == b'0');
            let mut keep: Vec<u8> = digits[..17].to_vec();
            let up = first > b'5' || (first == b'5' && (!tail_zero || (keep[16] - b'0') % 2 == 1));
            if up {
                let mut i = keep.len();
                loop {
                    if i == 0 {
                        keep.insert(0, b'1');
                        keep.pop();
                        exp += 1;
                        break;
                    }
                    i -= 1;
                    if keep[i] == b'9' {
                        keep[i] = b'0';
                    } else {
                        keep[i] += 1;
                        break;
                    }
                }
            }
            exp += cut as i64;
            digits = keep;
        }
        let text = format!("{}{}e{}", if self.neg { "-" } else { "" }, String::from_utf8(digits).unwrap(), exp);
        text.parse::<f64>().unwrap_or(f64::NAN)
    }
}

impl Value {
    pub fn num(f: f64) -> Value {
        Value::Num(Num::f(f))
    }
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }
    pub fn string(s: String) -> Value {
        Value::Str(Rc::from(s))
    }
    pub fn arr(v: Vec<Value>) -> Value {
        Value::Arr(Rc::new(v))
    }
    pub fn obj(m: Map) -> Value {
        Value::Obj(Rc::new(m))
    }
    pub fn bool(b: bool) -> Value {
        Value::Bool(b)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Arr(_) => "array",
            Value::Obj(_) => "object",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(false) => 1,
            Value::Bool(true) => 2,
            Value::Num(_) => 3,
            Value::Str(_) => 4,
            Value::Arr(_) => 5,
            Value::Obj(_) => 6,
        }
    }

    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Null | Value::Bool(false))
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(n.f),
            _ => None,
        }
    }

    /// jq's total order: null < false < true < numbers < strings < arrays < objects; objects
    /// by their sorted key lists, then values in that key order.
    pub fn compare(&self, o: &Value) -> Ordering {
        self.compare_d(o, 0)
    }

    fn compare_d(&self, o: &Value, depth: usize) -> Ordering {
        if depth > MAX_CMP_DEPTH {
            TOO_DEEP.with(|t| t.set(true));
            return Ordering::Equal;
        }
        let (ra, rb) = (self.rank(), o.rank());
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, o) {
            // jv_cmp: NaN sorts as if it were null (below every number, NaN included).
            (Value::Num(a), Value::Num(_)) if a.f.is_nan() => Ordering::Less,
            (Value::Num(_), Value::Num(b)) if b.f.is_nan() => Ordering::Greater,
            (Value::Num(a), Value::Num(b)) => num_cmp(a, b),
            (Value::Str(a), Value::Str(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Value::Arr(a), Value::Arr(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let c = x.compare_d(y, depth + 1);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Obj(a), Value::Obj(b)) => {
                let mut ka: Vec<&Rc<str>> = a.keys().collect();
                let mut kb: Vec<&Rc<str>> = b.keys().collect();
                ka.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
                kb.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
                for (x, y) in ka.iter().zip(kb.iter()) {
                    let c = x.as_bytes().cmp(y.as_bytes());
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                let c = ka.len().cmp(&kb.len());
                if c != Ordering::Equal {
                    return c;
                }
                for k in ka {
                    let c = a[k].compare_d(&b[k], depth + 1);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            }
            _ => Ordering::Equal,
        }
    }
}

/// jq's MAX_CMP_DEPTH: comparing values nested deeper fails ("Comparison too deep").
const MAX_CMP_DEPTH: usize = 10000;

thread_local! {
    static TOO_DEEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a comparison since the last call went past MAX_CMP_DEPTH.
pub fn took_too_deep() -> bool {
    TOO_DEEP.with(|t| t.replace(false))
}

pub fn num_cmp(a: &Num, b: &Num) -> Ordering {
    if let (Some(x), Some(y)) = (&a.lit, &b.lit) {
        if !x.out_of_range() && !y.out_of_range() {
            return x.cmp(y);
        }
    }
    if a.f < b.f {
        Ordering::Less
    } else if a.f == b.f {
        Ordering::Equal
    } else {
        Ordering::Greater
    }
}

impl PartialEq for Value {
    /// jv_equal: numbers by value (NaN equals nothing), the rest as compare.
    fn eq(&self, o: &Value) -> bool {
        self.eq_d(o, 0)
    }
}

impl Value {
    fn eq_d(&self, o: &Value, depth: usize) -> bool {
        if depth > MAX_CMP_DEPTH {
            TOO_DEEP.with(|t| t.set(true));
            return false;
        }
        match (self, o) {
            (Value::Num(a), Value::Num(b)) => !a.f.is_nan() && !b.f.is_nan() && num_cmp(a, b) == Ordering::Equal,
            (Value::Arr(a), Value::Arr(b)) => Rc::ptr_eq(a, b) || (a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_d(y, depth + 1))),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b) || (a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).map_or(false, |w| v.eq_d(w, depth + 1)))),
            _ => self.compare(o) == Ordering::Equal,
        }
    }
}

/// jvp_dtoa_fmt: the shortest digits that read back as `x`, in plain notation unless the
/// decimal point is far away (then d.ddde±XX).
pub fn fmt_f64(mut x: f64) -> String {
    if x.is_nan() {
        return "null".into();
    }
    if x > f64::MAX {
        x = f64::MAX;
    }
    if x < -f64::MAX {
        x = -f64::MAX;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{:e}", x);
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let nd = digits.len() as i32;
    let decpt = exp + 1;
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if decpt <= -4 || decpt > nd + 15 {
        s.push_str(&digits[..1]);
        if nd > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push('e');
        let d = decpt - 1;
        s.push(if d < 0 { '-' } else { '+' });
        let _ = write!(s, "{:02}", d.abs());
    } else if decpt <= 0 {
        s.push_str("0.");
        for _ in 0..(-decpt) {
            s.push('0');
        }
        s.push_str(&digits);
    } else if decpt >= nd {
        s.push_str(&digits);
        for _ in 0..(decpt - nd) {
            s.push('0');
        }
    } else {
        s.push_str(&digits[..decpt as usize]);
        s.push('.');
        s.push_str(&digits[decpt as usize..]);
    }
    s
}

pub fn fmt_num(n: &Num) -> String {
    if let Some(d) = &n.lit {
        if !d.out_of_range() {
            return d.to_text();
        }
    }
    fmt_f64(n.f)
}

/// Printing options (the CLI's -c, --tab, --indent, -S, -a, -C).
#[derive(Clone)]
pub struct Fmt {
    /// Newlines and indentation (by `indent` spaces, or tabs), and a space after ':'.
    pub pretty: bool,
    pub indent: usize,
    pub tab: bool,
    pub sort: bool,
    pub ascii: bool,
    pub color: Option<Colors>,
}

impl Fmt {
    pub fn compact() -> Fmt {
        Fmt { pretty: false, indent: 0, tab: false, sort: false, ascii: false, color: None }
    }
}

/// JQ_COLORS: null:false:true:numbers:strings:arrays:objects:object keys.
#[derive(Clone)]
pub struct Colors {
    pub c: [String; 8],
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            c: ["0;90", "0;39", "0;39", "0;39", "0;32", "1;39", "1;39", "1;34"].map(|s| format!("\x1b[{s}m")),
        }
    }
}

impl Colors {
    /// jq_set_colors: up to 8 colors separated by ':' (digits and ';'); an empty last one
    /// is ignored; the ones not given keep their defaults. None when the text is invalid.
    pub fn from_env(spec: &str) -> Option<Colors> {
        let mut c = Colors::default();
        let mut parts: Vec<&str> = vec![];
        let mut rest = spec;
        loop {
            let n = rest.find(|ch: char| !(ch.is_ascii_digit() || ch == ';')).unwrap_or(rest.len());
            parts.push(&rest[..n]);
            rest = &rest[n..];
            if rest.is_empty() || parts.len() >= 8 {
                break;
            }
            if !rest.starts_with(':') {
                return None;
            }
            rest = &rest[1..];
        }
        if parts.last().map_or(false, |p| p.is_empty()) {
            parts.pop();
        }
        for (i, p) in parts.iter().enumerate() {
            c.c[i] = format!("\x1b[{p}m");
        }
        Some(c)
    }
}

pub fn dump_string(s: &str, ascii: bool, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ascii && (c as u32) > 126 => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", u);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn dump(v: &Value, f: &Fmt) -> String {
    let mut s = String::new();
    dump_into(v, f, 0, &mut s);
    s
}

fn newline(f: &Fmt, level: usize, out: &mut String) {
    if !f.pretty {
        return;
    }
    out.push('\n');
    if f.tab {
        for _ in 0..level {
            out.push('\t');
        }
    } else {
        for _ in 0..level * f.indent {
            out.push(' ');
        }
    }
}

pub fn dump_into(v: &Value, f: &Fmt, level: usize, out: &mut String) {
    let pretty = f.pretty;
    let col = |i: usize| f.color.as_ref().map(|c| c.c[i].clone());
    let reset = "\x1b[0m";
    if level > 10000 {
        // jq's MAX_PRINT_DEPTH
        out.push_str("<skipped: too deep>");
        return;
    }
    match v {
        Value::Null | Value::Bool(_) | Value::Num(_) | Value::Str(_) => {
            let ci = match v {
                Value::Null => 0,
                Value::Bool(false) => 1,
                Value::Bool(true) => 2,
                Value::Num(_) => 3,
                _ => 4,
            };
            if let Some(c) = col(ci) {
                out.push_str(&c);
            }
            match v {
                Value::Null => out.push_str("null"),
                Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Value::Num(n) => out.push_str(&fmt_num(n)),
                Value::Str(s) => dump_string(s, f.ascii, out),
                _ => {}
            }
            if col(ci).is_some() {
                out.push_str(reset);
            }
        }
        Value::Arr(a) => {
            let c = col(5);
            if let Some(c) = &c {
                out.push_str(c);
            }
            if a.is_empty() {
                out.push_str("[]");
                if c.is_some() {
                    out.push_str(reset);
                }
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    if let Some(c) = &c {
                        out.push_str(c);
                    }
                    out.push(',');
                }
                if c.is_some() {
                    out.push_str(reset);
                }
                newline(f, level + 1, out);
                dump_into(x, f, level + 1, out);
            }
            newline(f, level, out);
            if let Some(c) = &c {
                out.push_str(c);
            }
            out.push(']');
            if c.is_some() {
                out.push_str(reset);
            }
        }
        Value::Obj(m) => {
            let c = col(6);
            if let Some(c) = &c {
                out.push_str(c);
            }
            if m.is_empty() {
                out.push_str("{}");
                if c.is_some() {
                    out.push_str(reset);
                }
                return;
            }
            out.push('{');
            let mut keys: Vec<&Rc<str>> = m.keys().collect();
            if f.sort {
                keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            }
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    if let Some(c) = &c {
                        out.push_str(c);
                    }
                    out.push(',');
                }
                if c.is_some() {
                    out.push_str(reset);
                }
                newline(f, level + 1, out);
                if let Some(kc) = col(7) {
                    out.push_str(&kc);
                    dump_string(k, f.ascii, out);
                    out.push_str(reset);
                } else {
                    dump_string(k, f.ascii, out);
                }
                if let Some(c) = &c {
                    out.push_str(c);
                }
                out.push(':');
                if c.is_some() {
                    out.push_str(reset);
                }
                if pretty {
                    out.push(' ');
                }
                dump_into(&m[*k], f, level + 1, out);
            }
            newline(f, level, out);
            if let Some(c) = &c {
                out.push_str(c);
            }
            out.push('}');
            if c.is_some() {
                out.push_str(reset);
            }
        }
    }
}

/// jq's error-message rendering of a value: its JSON, cut to `max` bytes with "...".
/// jv_dump_string_trunc with a buffer of `bufsize` bytes: the compact text, cut to fit with
/// "..." and the closing delimiter (long strings are first cut to `bufsize` codepoints).
pub fn dump_trunc(v: &Value, bufsize: usize) -> String {
    let s = match v {
        Value::Str(x) if x.len() > bufsize => dump(&Value::string(x.chars().take(bufsize).collect()), &Fmt::compact()),
        _ => dump(v, &Fmt::compact()),
    };
    if s.len() > bufsize - 1 && bufsize >= 8 {
        let delim = match s.as_bytes()[0] {
            b'"' => "\"",
            b'[' => "]",
            b'{' => "}",
            _ => "",
        };
        let mut l = bufsize - if delim.is_empty() { 4 } else { 5 };
        while !s.is_char_boundary(l) {
            l -= 1;
        }
        return format!("{}...{}", &s[..l], delim);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers() {
        assert_eq!(fmt_f64(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(fmt_f64(1e-5), "1e-05");
        assert_eq!(fmt_f64(1e23), "1e+23");
        assert_eq!(fmt_f64(1e17), "1e+17");
        assert_eq!(fmt_f64(1e15), "1000000000000000");
        assert_eq!(fmt_f64(100.0), "100");
        assert_eq!(fmt_f64(f64::INFINITY), "1.7976931348623157e+308");
        assert_eq!(Dec::parse("1.000").unwrap().to_text(), "1.000");
        assert_eq!(Dec::parse("1E2").unwrap().to_text(), "1E+2");
        assert_eq!(Dec::parse("1e-5").unwrap().to_text(), "0.00001");
        assert_eq!(Dec::parse("1e-7").unwrap().to_text(), "1E-7");
        assert_eq!(Dec::parse("1e17").unwrap().to_text(), "1E+17");
        assert_eq!(Dec::parse("0.00").unwrap().to_text(), "0.00");
        let a = Dec::parse("13911860366432393").unwrap();
        let b = Dec::parse("13911860366432392").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Greater);
        assert_eq!(Dec::parse("1.0").unwrap().cmp(&Dec::parse("1").unwrap()), Ordering::Equal);
    }
}

/// jvp_string_copy_replace_bad: bytes as text, each bad sequence one U+FFFD as jq's decoder
/// (jvp_utf8_next) delimits them: a truncated sequence at the end takes the rest; a sequence
/// cut short by a non-continuation byte ends before it.
pub fn utf8_lossy(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    let coding_length = |c: u8| -> u8 {
        match c {
            0x00..=0x7F => 1,
            0x80..=0xBF => 0xFF,
            0xC0 | 0xC1 => 0,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        }
    };
    let mut out = String::with_capacity(b.len() + 8);
    let mut i = 0;
    while i < b.len() {
        let first = b[i];
        let len = coding_length(first);
        let (cp, n): (i64, usize) = if first < 0x80 {
            (first as i64, 1)
        } else if len == 0 || len == 0xFF {
            (-1, 1)
        } else if len as usize > b.len() - i {
            (-1, b.len() - i)
        } else {
            let bits = match len {
                2 => 0x1F,
                3 => 0x0F,
                _ => 0x07,
            };
            let mut cp = (first & bits) as i64;
            let mut n = len as usize;
            for k in 1..len as usize {
                let ch = b[i + k];
                if coding_length(ch) != 0xFF {
                    cp = -1;
                    n = k;
                    break;
                }
                cp = (cp << 6) | (ch & 0x3F) as i64;
            }
            let min = [0, 0, 0x80, 0x800, 0x10000][n.min(4)];
            if cp != -1 && (cp < min || (0xD800..=0xDFFF).contains(&cp) || cp > 0x10FFFF) {
                cp = -1;
            }
            (cp, n)
        };
        out.push(if cp < 0 { '\u{FFFD}' } else { char::from_u32(cp as u32).unwrap_or('\u{FFFD}') });
        i += n;
    }
    out
}
