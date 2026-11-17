//! Numbers as text, the ways CF and Swift write them.

/// CF's __CFNumberCreateFormattingDescriptionAsFloat64: what an XML <real> holds.
pub fn xml_real(d: f64) -> String {
    if d.is_nan() {
        return "nan".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "+infinity" } else { "-infinity" }.into();
    }
    if d == 0.0 {
        return "0.0".into();
    }
    c_g(d, 17) // "%.*g", DBL_DIG + 2
}

/// C's "%.<precision>g" for a finite value.
pub fn c_g(d: f64, precision: usize) -> String {
    let p = precision.max(1);
    let e = format!("{:.*e}", p - 1, d);
    let (mantissa, exp) = e.split_once('e').unwrap();
    let x: i32 = exp.parse().unwrap();
    if x < -4 || x >= p as i32 {
        let m = strip_zeros(mantissa);
        format!("{m}e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs())
    } else {
        let decimals = (p as i32 - 1 - x).max(0) as usize;
        strip_zeros(&format!("{:.*}", decimals, d))
    }
}

fn strip_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// C's "%f" (what plutil's raw, Swift and Obj-C literal output use for reals).
pub fn c_f(d: f64) -> String {
    if d.is_nan() {
        return if d.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "inf" } else { "-inf" }.into();
    }
    format!("{d:.6}")
}

/// Swift's Double.description (SwiftDtoa's swift_dtoa_optimal_double): the shortest digits that
/// read back the same; exponential below 1e-4 and above 2^53, "e" with two digits at least.
pub fn swift_double(d: f64) -> String {
    swift_format(d.is_nan(), d.is_infinite(), d.is_sign_negative(), d == 0.0, || format!("{:e}", d.abs()), d.abs() > 9007199254740992.0)
}

/// Swift's Float.description: the same for Float32, exponential above 2^24.
pub fn swift_float(f: f32) -> String {
    swift_format(f.is_nan(), f.is_infinite(), f.is_sign_negative(), f == 0.0, || format!("{:e}", f.abs()), f.abs() > 16777216.0)
}

fn swift_format(nan: bool, inf: bool, negative: bool, zero: bool, digits: impl Fn() -> String, big: bool) -> String {
    let sign = if negative { "-" } else { "" };
    if nan {
        return "nan".into();
    }
    if inf {
        return format!("{sign}inf");
    }
    if zero {
        return format!("{sign}0.0");
    }
    // Rust's {:e} gives the shortest round-tripping digits: "d.ddde<x>".
    let e = digits();
    let (mantissa, exp) = e.split_once('e').unwrap();
    let x: i32 = exp.parse().unwrap();
    let ds: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = ds.len() as i32;
    if x < -4 || big {
        let m = if ds.len() > 1 { format!("{}.{}", &ds[..1], &ds[1..]) } else { ds.clone() };
        return format!("{sign}{m}e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs());
    }
    if x < 0 {
        return format!("{sign}0.{}{ds}", "0".repeat((-x - 1) as usize));
    }
    if x + 1 < n {
        let at = (x + 1) as usize;
        return format!("{sign}{}.{}", &ds[..at], &ds[at..]);
    }
    format!("{sign}{ds}{}.0", "0".repeat((x + 1 - n) as usize))
}

/// Foundation's `(string as NSString).integerValue`: optional whitespace, a sign, digits; stops at
/// anything else; saturates at Int's range; 0 when there are no digits.
pub fn ns_integer_value(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r' || b[i] == 0x0b || b[i] == 0x0c) {
        i += 1;
    }
    let mut negative = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        negative = b[i] == b'-';
        i += 1;
    }
    let mut value: i128 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        value = (value * 10 + (b[i] - b'0') as i128).min(i64::MAX as i128 + 1);
        i += 1;
    }
    let v = if negative { -value } else { value };
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Foundation's `(string as NSString).doubleValue`: leading whitespace, then the longest prefix
/// strtod reads (0 when there is none).
pub fn ns_double_value(s: &str) -> f64 {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\u{b}', '\u{c}']);
    let b = t.as_bytes();
    let mut end = 0;
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let lower = t[i..].to_ascii_lowercase();
    for word in ["infinity", "inf", "nan"] {
        if lower.starts_with(word) {
            let value = if word == "nan" { f64::NAN } else { f64::INFINITY };
            return if b.first() == Some(&b'-') { -value } else { value };
        }
    }
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits = true;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits = true;
        }
    }
    if digits {
        end = i;
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            let mut k = i + 1;
            if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                k += 1;
            }
            let start = k;
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            if k > start {
                end = k;
            }
        }
    }
    t[..end].parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_xml_reals() {
        assert_eq!(xml_real(3.14), "3.1400000000000001");
        assert_eq!(xml_real(1.0), "1");
        assert_eq!(xml_real(0.0), "0.0");
        assert_eq!(xml_real(-0.5), "-0.5");
        assert_eq!(xml_real(1e20), "1e+20");
        assert_eq!(xml_real(1.5e-7), "1.4999999999999999e-07");
        assert_eq!(xml_real(123456789012345678.0), "1.2345678901234568e+17");
        assert_eq!(xml_real(f64::INFINITY), "+infinity");
        assert_eq!(xml_real(f64::NAN), "nan");
    }

    #[test]
    fn swift_descriptions() {
        assert_eq!(swift_double(3.14), "3.14");
        assert_eq!(swift_double(1.0), "1.0");
        assert_eq!(swift_double(100.0), "100.0");
        assert_eq!(swift_double(0.0001), "0.0001");
        assert_eq!(swift_double(0.00001), "1e-05");
        assert_eq!(swift_double(1.5e-7), "1.5e-07");
        assert_eq!(swift_double(9007199254740992.0), "9007199254740992.0");
        assert_eq!(swift_double(1e16), "1e+16");
        assert_eq!(swift_double(1.2345e300), "1.2345e+300");
        assert_eq!(swift_double(-2.5), "-2.5");
        assert_eq!(swift_double(-0.0), "-0.0");
        assert_eq!(swift_double(f64::INFINITY), "inf");
        assert_eq!(swift_float(0.1), "0.1");
        assert_eq!(swift_float(16777216.0), "16777216.0");
        assert_eq!(swift_float(1e10), "1e+10");
    }

    #[test]
    fn foundation_string_values() {
        assert_eq!(ns_integer_value(" 42abc"), 42);
        assert_eq!(ns_integer_value("-7"), -7);
        assert_eq!(ns_integer_value("x"), 0);
        assert_eq!(ns_integer_value("99999999999999999999"), i64::MAX);
        assert_eq!(ns_double_value("2.5e3x"), 2500.0);
        assert_eq!(ns_double_value("1e"), 1.0);
        assert_eq!(ns_double_value("abc"), 0.0);
        assert_eq!(c_f(2.5), "2.500000");
    }
}
