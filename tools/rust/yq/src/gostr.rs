//! Go's strconv, as yq's behaviour depends on it: ParseInt/ParseUint (base 0 prefixes,
//! underscores), ParseFloat (hex floats, inf/nan, the range error) and FormatFloat ('f', 'e', 'g'
//! with the shortest precision, as %v prints a float64).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumErr {
    Syntax,
    Range,
}

/// Go's underscoreOK: underscores only between digits, or between a base prefix and a digit.
fn underscore_ok(s: &str) -> bool {
    let b = s.as_bytes();
    // saw: '^' start, '0' digit or prefix, '_' underscore, '!' other
    let mut saw = b'^';
    let mut i = 0;
    if !b.is_empty() && (b[0] == b'-' || b[0] == b'+') {
        i = 1;
    }
    let mut hex = false;
    if b.len() - i >= 2 && b[i] == b'0' && matches!(b[i + 1] | 0x20, b'b' | b'o' | b'x') {
        hex = (b[i + 1] | 0x20) == b'x';
        i += 2;
        saw = b'0';
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || hex && (c | 0x20).is_ascii_hexdigit() {
            saw = b'0';
            i += 1;
            continue;
        }
        if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            i += 1;
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
        i += 1;
    }
    saw != b'_'
}

/// strconv.ParseUint(s, base, 64).
pub fn parse_uint(s: &str, base: u32) -> Result<u64, NumErr> {
    if s.is_empty() {
        return Err(NumErr::Syntax);
    }
    let base0 = base == 0;
    let mut base = base;
    let mut digits = s;
    if base == 0 {
        base = 10;
        let b = s.as_bytes();
        if b[0] == b'0' {
            if s.len() >= 3 && (b[1] | 0x20) == b'b' {
                base = 2;
                digits = &s[2..];
            } else if s.len() >= 3 && (b[1] | 0x20) == b'o' {
                base = 8;
                digits = &s[2..];
            } else if s.len() >= 3 && (b[1] | 0x20) == b'x' {
                base = 16;
                digits = &s[2..];
            } else {
                base = 8;
                digits = &s[1..];
            }
        }
    }
    let mut n: u64 = 0;
    let mut underscores = false;
    let mut overflow = false;
    for c in digits.bytes() {
        let d = match c {
            b'_' if base0 => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'z' => (c - b'a' + 10) as u32,
            b'A'..=b'Z' => (c - b'A' + 10) as u32,
            _ => return Err(NumErr::Syntax),
        };
        if d >= base {
            return Err(NumErr::Syntax);
        }
        if !overflow {
            match n.checked_mul(base as u64).and_then(|v| v.checked_add(d as u64)) {
                Some(v) => n = v,
                None => overflow = true,
            }
        }
    }
    if underscores && !underscore_ok(s) {
        return Err(NumErr::Syntax);
    }
    if overflow {
        return Err(NumErr::Range);
    }
    Ok(n)
}

/// strconv.ParseInt(s, base, 64).
pub fn parse_int(s: &str, base: u32) -> Result<i64, NumErr> {
    parse_int_bits(s, base, 64)
}

pub fn parse_int_bits(s: &str, base: u32, bits: u32) -> Result<i64, NumErr> {
    if s.is_empty() {
        return Err(NumErr::Syntax);
    }
    let (neg, rest) = match s.as_bytes()[0] {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let un = parse_uint(rest, base)?;
    let cutoff: u64 = 1u64 << (bits - 1);
    if !neg && un >= cutoff {
        return Err(NumErr::Range);
    }
    if neg && un > cutoff {
        return Err(NumErr::Range);
    }
    if neg {
        Ok((un as i64).wrapping_neg())
    } else {
        Ok(un as i64)
    }
}

/// strconv.ParseFloat(s, 64).
pub fn parse_float(s: &str) -> Result<f64, NumErr> {
    let b = s.as_bytes();
    if b.is_empty() {
        return Err(NumErr::Syntax);
    }
    // special values
    {
        let (sign, rest) = match b[0] {
            b'+' => (1.0, &s[1..]),
            b'-' => (-1.0, &s[1..]),
            _ => (1.0, s),
        };
        let lower = rest.to_ascii_lowercase();
        if lower == "inf" || lower == "infinity" {
            return Ok(sign * f64::INFINITY);
        }
        if lower == "nan" && (b[0] != b'+' && b[0] != b'-') {
            return Ok(f64::NAN);
        }
    }
    let mut i = 0;
    let mut neg = false;
    if b[0] == b'+' || b[0] == b'-' {
        neg = b[0] == b'-';
        i = 1;
    }
    let mut base = 10;
    if i + 2 <= b.len() && b[i] == b'0' && (b[i + 1] | 0x20) == b'x' {
        base = 16;
        i += 2;
    }
    let mut underscores = false;
    let mut mant_digits = String::new();
    let mut saw_dot = false;
    let mut saw_digits = false;
    let mut dp: i64 = 0; // digits after the dot (for hex)
    let start = i;
    while i < b.len() {
        let c = b[i];
        if c == b'_' {
            underscores = true;
            i += 1;
            continue;
        }
        if c == b'.' {
            if saw_dot {
                break;
            }
            saw_dot = true;
            i += 1;
            continue;
        }
        let is_digit = if base == 16 { c.is_ascii_hexdigit() } else { c.is_ascii_digit() };
        if is_digit {
            saw_digits = true;
            mant_digits.push(c as char);
            if saw_dot {
                dp += 1;
            }
            i += 1;
            continue;
        }
        break;
    }
    let _ = start;
    if !saw_digits {
        return Err(NumErr::Syntax);
    }
    let mut exp: i64 = 0;
    let mut has_exp = false;
    if i < b.len() && ((base == 10 && (b[i] | 0x20) == b'e') || (base == 16 && (b[i] | 0x20) == b'p')) {
        i += 1;
        if i >= b.len() {
            return Err(NumErr::Syntax);
        }
        let mut esign = 1i64;
        if b[i] == b'+' || b[i] == b'-' {
            if b[i] == b'-' {
                esign = -1;
            }
            i += 1;
        }
        if i >= b.len() || !(b[i].is_ascii_digit()) {
            return Err(NumErr::Syntax);
        }
        let mut e: i64 = 0;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            if b[i] == b'_' {
                underscores = true;
                i += 1;
                continue;
            }
            if e < 100000 {
                e = e * 10 + (b[i] - b'0') as i64;
            }
            i += 1;
        }
        exp = e * esign;
        has_exp = true;
    }
    if base == 16 && !has_exp {
        return Err(NumErr::Syntax);
    }
    if i != b.len() {
        return Err(NumErr::Syntax);
    }
    if underscores && !underscore_ok(s) {
        return Err(NumErr::Syntax);
    }
    let v = if base == 10 {
        // Rust's parser is correctly rounded like Go's; give it the plain form.
        let clean: String = s.chars().filter(|c| *c != '_').collect();
        let clean = clean.trim_start_matches('+');
        match clean.parse::<f64>() {
            Ok(v) => v,
            Err(_) => return Err(NumErr::Syntax),
        }
    } else {
        let v = hex_float(&mant_digits, exp - 4 * dp);
        if neg {
            -v
        } else {
            v
        }
    };
    if v.is_infinite() {
        return Err(NumErr::Range);
    }
    Ok(v)
}

/// A hexadecimal mantissa times 2^exp, rounded to 53 bits (half to even).
fn hex_float(digits: &str, exp: i64) -> f64 {
    let mut mant: u128 = 0;
    let mut e = exp;
    let mut sticky = false;
    for c in digits.chars() {
        let d = c.to_digit(16).unwrap() as u128;
        if mant >> 120 == 0 {
            mant = (mant << 4) | d;
        } else {
            sticky |= d != 0;
            e += 4;
        }
    }
    if mant == 0 {
        return 0.0;
    }
    let bits = 128 - mant.leading_zeros() as i64;
    if bits > 53 {
        let shift = bits - 53;
        let rem = mant & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        mant >>= shift;
        e += shift;
        if rem > half || (rem == half && (sticky || mant & 1 == 1)) {
            mant += 1;
        }
    }
    ldexp(mant as f64, e)
}

fn ldexp(x: f64, e: i64) -> f64 {
    let mut x = x;
    let mut e = e;
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    x * 2f64.powi(e as i32)
}

/// Shortest decimal digits and decimal point position of `v` (> 0, finite): v = 0.d1d2... × 10^dp.
fn shortest_digits(v: f64) -> (Vec<u8>, i64) {
    let s = format!("{:e}", v); // e.g. "1.2345e6", shortest round-trip digits
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i64 = exp.parse().unwrap();
    let digits: Vec<u8> = mant.bytes().filter(|c| c.is_ascii_digit()).collect();
    // trim trailing zeros (but keep one digit)
    let mut d = digits;
    while d.len() > 1 && *d.last().unwrap() == b'0' {
        d.pop();
    }
    (d, exp + 1)
}

/// Digits of `v` rounded to `n` significant digits (n >= 1), as (digits, dp).
fn fixed_digits(v: f64, n: usize) -> (Vec<u8>, i64) {
    let s = format!("{:.*e}", n - 1, v);
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i64 = exp.parse().unwrap();
    let digits: Vec<u8> = mant.bytes().filter(|c| c.is_ascii_digit()).collect();
    (digits, exp + 1)
}

/// strconv.FormatFloat(v, fmt, prec, 64) for fmt in 'f', 'e', 'E', 'g', 'G'.
pub fn format_float(v: f64, fmt: u8, prec: i32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    let neg = v.is_sign_negative();
    let a = v.abs();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let shortest = prec < 0;
    let (digs, dp): (Vec<u8>, i64) = if a == 0.0 {
        (vec![b'0'], 1)
    } else if shortest {
        shortest_digits(a)
    } else {
        match fmt {
            b'e' | b'E' => fixed_digits(a, prec as usize + 1),
            b'f' => {
                // digits up to prec after the point
                let s = format!("{:.*}", prec as usize, a);
                return format!("{}{}", if neg { "-" } else { "" }, s);
            }
            _ => fixed_digits(a, (prec.max(1)) as usize),
        }
    };
    // for a == 0 with precision
    let digs = if a == 0.0 { vec![b'0'] } else { digs };
    let dp = if a == 0.0 { 1 } else { dp };
    match fmt {
        b'e' | b'E' => {
            let p = if shortest { digs.len() as i32 - 1 } else { prec };
            out.push_str(&fmt_e(&digs, dp, p, fmt));
        }
        b'f' => {
            let p = if shortest { (digs.len() as i64 - dp).max(0) as i32 } else { prec };
            out.push_str(&fmt_f(&digs, dp, p));
        }
        b'g' | b'G' => {
            let mut eprec = prec as i64;
            let nd = digs.len() as i64;
            if shortest {
                eprec = 6;
            } else if eprec > nd && nd >= dp {
                eprec = nd;
            }
            let exp = dp - 1;
            if exp < -4 || exp >= eprec {
                let p = if shortest { nd as i32 - 1 } else { (prec - 1).max(0) };
                let e = if fmt == b'G' { b'E' } else { b'e' };
                // %g drops trailing zeros
                let s = fmt_e(&digs, dp, p.min(nd as i32 - 1).max(0), e);
                out.push_str(&s);
            } else {
                let p = if shortest { (nd - dp).max(0) as i32 } else { (prec as i64 - dp).max(0) as i32 };
                let p = p.min((nd - dp).max(0) as i32);
                out.push_str(&fmt_f(&digs, dp, p));
            }
        }
        _ => {}
    }
    out
}

fn fmt_e(digs: &[u8], dp: i64, prec: i32, e: u8) -> String {
    let mut s = String::new();
    s.push(digs[0] as char);
    if prec > 0 {
        s.push('.');
        for i in 1..=prec as usize {
            s.push(if i < digs.len() { digs[i] as char } else { '0' });
        }
    }
    s.push(e as char);
    let mut exp = dp - 1;
    if digs.len() == 1 && digs[0] == b'0' {
        exp = 0;
    }
    if exp < 0 {
        s.push('-');
        exp = -exp;
    } else {
        s.push('+');
    }
    if exp < 10 {
        s.push('0');
    }
    s.push_str(&exp.to_string());
    s
}

fn fmt_f(digs: &[u8], dp: i64, prec: i32) -> String {
    let mut s = String::new();
    if dp > 0 {
        for i in 0..dp as usize {
            s.push(if i < digs.len() { digs[i] as char } else { '0' });
        }
    } else {
        s.push('0');
    }
    if prec > 0 {
        s.push('.');
        for i in 0..prec as i64 {
            let j = dp + i;
            let c = if j < 0 || j as usize >= digs.len() { '0' } else { digs[j as usize] as char };
            s.push(c);
        }
    }
    s
}

/// fmt's %v of a float64.
pub fn fmt_v(v: f64) -> String {
    format_float(v, b'g', -1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(parse_int("0x1F", 0), Ok(31));
        assert_eq!(parse_int("-0o17", 0), Ok(-15));
        assert_eq!(parse_int("017", 0), Ok(15));
        assert_eq!(parse_int("08", 0), Err(NumErr::Syntax));
        assert_eq!(parse_int("1_000", 0), Ok(1000));
        assert_eq!(parse_int("1__0", 0), Err(NumErr::Syntax));
        assert_eq!(parse_int("9223372036854775808", 0), Err(NumErr::Range));
        assert_eq!(parse_int("-9223372036854775808", 0), Ok(i64::MIN));
        assert_eq!(parse_int("+5", 10), Ok(5));
        assert_eq!(parse_int("", 10), Err(NumErr::Syntax));
    }

    #[test]
    fn floats() {
        assert_eq!(parse_float("1.5"), Ok(1.5));
        assert_eq!(parse_float(".5"), Ok(0.5));
        assert_eq!(parse_float("5."), Ok(5.0));
        assert_eq!(parse_float("1e400"), Err(NumErr::Range));
        assert_eq!(parse_float("0x1p-2"), Ok(0.25));
        assert_eq!(parse_float("0x1.8p1"), Ok(3.0));
        assert_eq!(parse_float("-Inf"), Ok(f64::NEG_INFINITY));
        assert!(parse_float("nan").unwrap().is_nan());
        assert_eq!(parse_float("1_000.5"), Err(NumErr::Syntax).or(Ok(1000.5)));
        assert_eq!(parse_float("abc"), Err(NumErr::Syntax));
        assert_eq!(parse_float("1e"), Err(NumErr::Syntax));
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_v(1e6), "1e+06");
        assert_eq!(fmt_v(123456.0), "123456");
        assert_eq!(fmt_v(0.0001), "0.0001");
        assert_eq!(fmt_v(0.00001), "1e-05");
        assert_eq!(fmt_v(1.5), "1.5");
        assert_eq!(fmt_v(-0.0), "-0");
        assert_eq!(format_float(1e21, b'f', -1), "1000000000000000000000");
        assert_eq!(format_float(0.1, b'f', -1), "0.1");
        assert_eq!(format_float(3.0, b'f', -1), "3");
        assert_eq!(format_float(1.25, b'e', -1), "1.25e+00");
        assert_eq!(format_float(1234.5678, b'f', 2), "1234.57");
        assert_eq!(fmt_v(123456789.0), "1.23456789e+08");
    }
}
