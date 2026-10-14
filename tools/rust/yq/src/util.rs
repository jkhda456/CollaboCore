//! Small pieces of yq's lib.go and of Go's standard library that yq's output depends on.

use crate::gostr::{parse_int_bits, NumErr};

/// The format yq keeps an integer in after arithmetic: decimal, 0x hex or 0o octal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntFmt {
    Dec,
    Hex,
    Oct,
}

impl IntFmt {
    pub fn format(&self, v: i64) -> String {
        match self {
            IntFmt::Dec => v.to_string(),
            IntFmt::Hex => {
                if v < 0 {
                    format!("0x-{:X}", (v as i128).unsigned_abs())
                } else {
                    format!("0x{:X}", v)
                }
            }
            IntFmt::Oct => {
                if v < 0 {
                    format!("0o-{:o}", (v as i128).unsigned_abs())
                } else {
                    format!("0o{:o}", v)
                }
            }
        }
    }
}

fn num_err(func: &str, s: &str, e: NumErr) -> String {
    match e {
        NumErr::Syntax => format!("strconv.{func}: parsing {}: invalid syntax", go_quote(s)),
        NumErr::Range => format!("strconv.{func}: parsing {}: value out of range", go_quote(s)),
    }
}

/// yq's parseInt64: (format, value).
pub fn parse_int64(number: &str) -> Result<(IntFmt, i64), String> {
    let s: String = if number.contains('_') { number.replace('_', "") } else { number.to_string() };
    let (sign, digits) = if !s.is_empty() && (s.starts_with('+') || s.starts_with('-')) { (&s[..1], &s[1..]) } else { ("", &s[..]) };
    if digits.starts_with("0x") || digits.starts_with("0X") {
        let t = format!("{sign}{}", &digits[2..]);
        return parse_int_bits(&t, 16, 64).map(|v| (IntFmt::Hex, v)).map_err(|e| num_err("ParseInt", &t, e));
    } else if digits.starts_with("0o") {
        let t = format!("{sign}{}", &digits[2..]);
        return parse_int_bits(&t, 8, 64).map(|v| (IntFmt::Oct, v)).map_err(|e| num_err("ParseInt", &t, e));
    }
    parse_int_bits(&s, 10, 64).map(|v| (IntFmt::Dec, v)).map_err(|e| num_err("ParseInt", &s, e))
}

/// yq's parseInt.
pub fn parse_int(number: &str) -> Result<i64, String> {
    parse_int64(number).map(|(_, v)| v)
}

/// strconv.ParseFloat with Go's error message.
pub fn parse_float(s: &str) -> Result<f64, String> {
    crate::gostr::parse_float(s).map_err(|e| num_err("ParseFloat", s, e))
}

pub fn checked_arith(op: char, l: i64, r: i64) -> Result<i64, String> {
    let res = match op {
        '+' => l.checked_add(r),
        '-' => l.checked_sub(r),
        '*' => l.checked_mul(r),
        _ => return Err(format!("unknown arithmetic operator '{op}'")),
    };
    res.ok_or_else(|| format!("{l} {op} {r} overflows int64"))
}

/// strconv.Quote.
pub fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if is_go_printable(c) => out.push(c),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// unicode.IsPrint (approximately: letters, marks, numbers, punctuation, symbols and ASCII space).
pub fn is_go_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() {
        return false;
    }
    let u = c as u32;
    // separators other than space, private use, surrogates, unassigned noncharacters
    !(matches!(u, 0x00A0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
        || (0xE000..=0xF8FF).contains(&u)
        || (0xF0000..=0x10FFFF).contains(&u)
        || matches!(u, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0xFEFF | 0xFFF9..=0xFFFB | 0x00AD)
        || u == 0xFFFE
        || u == 0xFFFF)
}

/// yq's processEscapeCharacters (for "..." literals in expressions).
pub fn process_escape_characters(original: &str) -> String {
    if original.is_empty() {
        return String::new();
    }
    let runes: Vec<char> = original.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < runes.len() {
        if runes[i] == '\\' && i < runes.len() - 1 {
            let next = runes[i + 1];
            let rep = match next {
                '\\' => {
                    if i + 2 < runes.len() && runes[i + 2] == '(' {
                        result.push_str("\\\\");
                        i += 2;
                        continue;
                    }
                    Some('\\')
                }
                '"' => Some('"'),
                'n' => Some('\n'),
                't' => Some('\t'),
                'r' => Some('\r'),
                'f' => Some('\x0c'),
                'v' => Some('\x0b'),
                'b' => Some('\x08'),
                'a' => Some('\x07'),
                _ => None,
            };
            if let Some(c) = rep {
                result.push(c);
                i += 2;
                continue;
            }
        }
        result.push(runes[i]);
        i += 1;
    }
    result
}

/// time.ParseDuration: nanoseconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let orig = s;
    let err = || format!("time: invalid duration {}", crate::gotime::quote(orig));
    let mut s = s;
    let mut d: u64 = 0;
    let mut neg = false;
    if !s.is_empty() && (s.starts_with('-') || s.starts_with('+')) {
        neg = s.starts_with('-');
        s = &s[1..];
    }
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(err());
    }
    while !s.is_empty() {
        let b = s.as_bytes();
        if !(b[0] == b'.' || b[0].is_ascii_digit()) {
            return Err(err());
        }
        // leading int
        let mut i = 0;
        let mut v: u64 = 0;
        let mut overflow = false;
        while i < b.len() && b[i].is_ascii_digit() {
            if v > (1u64 << 63) / 10 {
                overflow = true;
            }
            v = v.wrapping_mul(10).wrapping_add((b[i] - b'0') as u64);
            if v > 1u64 << 63 {
                overflow = true;
            }
            i += 1;
        }
        if overflow {
            return Err(err());
        }
        let pre = i != 0;
        s = &s[i..];
        let mut f: u64 = 0;
        let mut scale: f64 = 1.0;
        let mut post = false;
        if s.starts_with('.') {
            s = &s[1..];
            let b = s.as_bytes();
            let mut j = 0;
            let mut of = false;
            while j < b.len() && b[j].is_ascii_digit() {
                if of {
                    j += 1;
                    continue;
                }
                if f > (1u64 << 63) / 10 {
                    of = true;
                    j += 1;
                    continue;
                }
                let y = f * 10 + (b[j] - b'0') as u64;
                if y > 1u64 << 63 {
                    of = true;
                    j += 1;
                    continue;
                }
                f = y;
                scale *= 10.0;
                j += 1;
            }
            post = j != 0;
            s = &s[j..];
        }
        if !pre && !post {
            return Err(err());
        }
        let b = s.as_bytes();
        let mut k = 0;
        while k < b.len() && b[k] != b'.' && !b[k].is_ascii_digit() {
            k += 1;
        }
        if k == 0 {
            return Err(format!("time: missing unit in duration {}", crate::gotime::quote(orig)));
        }
        let u = &s[..k];
        s = &s[k..];
        let unit: u64 = match u {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return Err(format!("time: unknown unit {} in duration {}", crate::gotime::quote(u), crate::gotime::quote(orig))),
        };
        if v > (1u64 << 63) / unit {
            return Err(err());
        }
        v *= unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1u64 << 63 {
                return Err(err());
            }
        }
        d += v;
        if d > 1u64 << 63 {
            return Err(err());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > (1u64 << 63) - 1 {
        return Err(err());
    }
    Ok(d as i64)
}

/// Go's strings.ToUpper / ToLower: per character, keeping characters whose case mapping is not a
/// single character.
pub fn go_to_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut u = c.to_uppercase();
            match (u.next(), u.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

pub fn go_to_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut u = c.to_lowercase();
            match (u.next(), u.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

/// unicode.IsSpace
pub fn go_is_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ' | '\u{85}' | '\u{A0}')
        || (c as u32 > 0xFF && c.is_whitespace())
}

pub fn go_trim_space(s: &str) -> &str {
    s.trim_matches(go_is_space)
}

/// yq's matchKey: glob with `*` and `?`.
pub fn match_key(name: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return name == pattern;
    }
    if pattern == "*" {
        return true;
    }
    deep_match(name.as_bytes(), pattern.as_bytes())
}

fn deep_match(name: &[u8], pattern: &[u8]) -> bool {
    let (mut px, mut nx, mut next_px, mut next_nx) = (0usize, 0usize, 0usize, 0usize);
    while px < pattern.len() || nx < name.len() {
        if px < pattern.len() {
            let c = pattern[px];
            match c {
                b'?' => {
                    if nx < name.len() {
                        px += 1;
                        nx += 1;
                        continue;
                    }
                }
                b'*' => {
                    next_px = px;
                    next_nx = nx + 1;
                    px += 1;
                    continue;
                }
                _ => {
                    if nx < name.len() && name[nx] == c {
                        px += 1;
                        nx += 1;
                        continue;
                    }
                }
            }
        }
        if 0 < next_nx && next_nx <= name.len() {
            px = next_px;
            nx = next_nx;
            continue;
        }
        return false;
    }
    true
}

/// Go's slog text handler line for a warning, written to stderr.
pub fn warn(msg: &str) {
    let now = crate::gotime::Time::now();
    let t = now.format("2006-01-02T15:04:05.000Z07:00");
    eprintln!("time={} level=WARN msg={}", t, slog_value(msg));
}

fn slog_value(s: &str) -> String {
    let needs = s.is_empty()
        || s.chars().any(|c| c == ' ' || c == '=' || c == '"' || !is_go_printable(c) || c == '\u{fffd}');
    if needs {
        go_quote(s)
    } else {
        s.to_string()
    }
}

/// Go's sort.Stable: insertion sort on blocks of 20, then symmerge (the order depends on it when
/// the comparison is not a strict weak order).
pub fn go_stable_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut [T], mut less: F) {
    let n = data.len();
    let mut block_size = 20;
    let (mut a, mut b) = (0, block_size);
    while b <= n {
        insertion_sort(data, a, b, &mut less);
        a = b;
        b += block_size;
    }
    insertion_sort(data, a, n, &mut less);
    while block_size < n {
        a = 0;
        b = 2 * block_size;
        while b <= n {
            sym_merge(data, a, a + block_size, b, &mut less);
            a = b;
            b += 2 * block_size;
        }
        let m = a + block_size;
        if m < n {
            sym_merge(data, a, m, n, &mut less);
        }
        block_size *= 2;
    }
}

fn insertion_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut [T], a: usize, b: usize, less: &mut F) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && less(&data[j], &data[j - 1]) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sym_merge<T, F: FnMut(&T, &T) -> bool>(data: &mut [T], a: usize, m: usize, b: usize, less: &mut F) {
    if m - a == 1 {
        let mut i = m;
        let mut j = b;
        while i < j {
            let h = (i + j) / 2;
            if less(&data[h], &data[a]) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        for k in a..i - 1 {
            data.swap(k, k + 1);
        }
        return;
    }
    if b - m == 1 {
        let mut i = a;
        let mut j = m;
        while i < j {
            let h = (i + j) / 2;
            if !less(&data[m], &data[h]) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        let mut k = m;
        while k > i {
            data.swap(k, k - 1);
            k -= 1;
        }
        return;
    }
    let mid = (a + b) / 2;
    let n = mid + m;
    let (mut start, mut r) = if m > mid { (n - b, mid) } else { (a, m) };
    let p = n - 1;
    while start < r {
        let c = (start + r) / 2;
        if !less(&data[p - c], &data[c]) {
            start = c + 1;
        } else {
            r = c;
        }
    }
    let end = n - start;
    if start < m && m < end {
        rotate(data, start, m, end);
    }
    if a < start && start < mid {
        sym_merge(data, a, start, mid, less);
    }
    if mid < end && end < b {
        sym_merge(data, mid, end, b, less);
    }
}

fn swap_range<T>(data: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        data.swap(a + i, b + i);
    }
}

fn rotate<T>(data: &mut [T], a: usize, m: usize, b: usize) {
    let mut i = m - a;
    let mut j = b - m;
    while i != j {
        if i > j {
            swap_range(data, m - i, m, j);
            i -= j;
        } else {
            swap_range(data, m - i, m + j - i, i);
            j -= i;
        }
    }
    swap_range(data, m - i, m, i);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h"), Ok(3_600_000_000_000));
        assert_eq!(parse_duration("1.5s"), Ok(1_500_000_000));
        assert_eq!(parse_duration("-2m3s"), Ok(-123_000_000_000));
        assert!(parse_duration("3").is_err());
        assert_eq!(parse_duration("3").unwrap_err(), "time: missing unit in duration \"3\"");
    }

    #[test]
    fn stable() {
        let mut v: Vec<(i32, i32)> = (0..100).map(|i| ((i * 7919) % 13, i)).collect();
        let mut w = v.clone();
        go_stable_sort(&mut v, |a, b| a.0 < b.0);
        w.sort_by_key(|x| x.0);
        assert_eq!(v, w);
    }

    #[test]
    fn ints() {
        assert_eq!(parse_int64("0x1F"), Ok((IntFmt::Hex, 31)));
        assert_eq!(parse_int64("-0x1F"), Ok((IntFmt::Hex, -31)));
        assert_eq!(parse_int64("017"), Ok((IntFmt::Dec, 17)));
        assert_eq!(parse_int64("1_000"), Ok((IntFmt::Dec, 1000)));
        assert_eq!(IntFmt::Hex.format(-255), "0x-FF");
        assert_eq!(parse_int64("0b101").unwrap_err(), "strconv.ParseInt: parsing \"0b101\": invalid syntax");
    }
}
