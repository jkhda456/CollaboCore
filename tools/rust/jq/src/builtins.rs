//! jq's native builtins (builtin.c and the bytecoded ones: empty, not, path, last, range),
//! with jq's argument order (a C function's last argument is the outer loop) and messages.
//! The rest of the library is jq code: builtin.jq, unchanged from jq.

use crate::interp::{delpaths, err, getpath, input_free, setpath, Env, Flow, Interp, Pv, R};
use crate::parser::{Ast, BinOp};
use crate::value::{dump, dump_trunc, num_cmp, Fmt, Map, Num, Value};
use std::cmp::Ordering;
use std::rc::Rc;

pub const BUILTIN_JQ: &str = include_str!("builtin.jq");

fn type_error(v: &Value, msg: &str) -> Flow {
    err(format!("{} ({}) {}", v.kind(), dump_trunc(v, 30), msg))
}

fn type_error2(a: &Value, b: &Value, msg: &str) -> Flow {
    err(format!("{} ({}) and {} ({}) {}", a.kind(), dump_trunc(a, 30), b.kind(), dump_trunc(b, 30), msg))
}

fn num(v: f64) -> Value {
    Value::num(v)
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

pub fn negate(v: Value) -> Result<Value, Flow> {
    match v {
        Value::Num(n) => {
            if let Some(d) = &n.lit {
                let mut d2 = (**d).clone();
                if !d2.is_zero() {
                    d2.neg = !d2.neg;
                }
                return Ok(Value::Num(Num { f: -n.f, lit: Some(Rc::new(d2)) }));
            }
            Ok(num(-n.f))
        }
        other => Err(type_error(&other, "cannot be negated")),
    }
}

fn abs_num(n: &Num) -> Value {
    if let Some(d) = &n.lit {
        let mut d2 = (**d).clone();
        d2.neg = false;
        return Value::Num(Num { f: n.f.abs(), lit: Some(Rc::new(d2)) });
    }
    num(n.f.abs())
}

fn merge_recursive(a: &Map, b: &Map, depth: usize) -> Result<Map, Flow> {
    if depth > 10000 {
        return Err(err("Object merge too deep"));
    }
    let mut out = a.clone();
    for (k, bv) in b.iter() {
        let nv = match (out.get(k), bv) {
            (Some(Value::Obj(x)), Value::Obj(y)) => Value::obj(merge_recursive(x, y, depth + 1)?),
            _ => bv.clone(),
        };
        out.insert(k.clone(), nv);
    }
    Ok(out)
}

/// The result of comparisons, or jq's error when they went too deep.
fn cmp_checked<T>(v: T, msg: &str) -> Result<T, Flow> {
    if crate::value::took_too_deep() {
        return Err(err(msg));
    }
    Ok(v)
}

pub fn binop(op: BinOp, a: Value, b: Value) -> Result<Value, Flow> {
    match op {
        BinOp::Add => match (a, b) {
            (Value::Null, b) => Ok(b),
            (a, Value::Null) => Ok(a),
            (Value::Num(x), Value::Num(y)) => Ok(num(x.f + y.f)),
            (Value::Str(x), Value::Str(y)) => Ok(Value::string(format!("{x}{y}"))),
            // In place when the left side is held only here (Rc::make_mut).
            (Value::Arr(mut x), Value::Arr(y)) => {
                Rc::make_mut(&mut x).extend(y.iter().cloned());
                Ok(Value::Arr(x))
            }
            (Value::Obj(mut x), Value::Obj(y)) => {
                let m = Rc::make_mut(&mut x);
                for (k, v) in y.iter() {
                    m.insert(k.clone(), v.clone());
                }
                Ok(Value::Obj(x))
            }
            (a, b) => Err(type_error2(&a, &b, "cannot be added")),
        },
        BinOp::Sub => match (a, b) {
            (Value::Num(x), Value::Num(y)) => Ok(num(x.f - y.f)),
            (Value::Arr(x), Value::Arr(y)) => Ok(Value::arr(x.iter().filter(|e| !y.iter().any(|z| z == *e)).cloned().collect())),
            (a, b) => Err(type_error2(&a, &b, "cannot be subtracted")),
        },
        BinOp::Mul => match (a, b) {
            (Value::Num(x), Value::Num(y)) => Ok(num(x.f * y.f)),
            (Value::Str(s), Value::Num(n)) | (Value::Num(n), Value::Str(s)) => {
                let d = n.f;
                let k = if d < 0.0 || d.is_nan() { -1 } else if d > i32::MAX as f64 { i32::MAX as i64 } else { d as i64 };
                if k < 0 {
                    return Ok(Value::Null);
                }
                if (s.len() as i64).saturating_mul(k) >= (i32::MAX as i64) - 8 {
                    return Err(err("Repeat string result too long"));
                }
                Ok(Value::string(s.repeat(k as usize)))
            }
            (Value::Obj(x), Value::Obj(y)) => Ok(Value::obj(merge_recursive(&x, &y, 0)?)),
            (a, b) => Err(type_error2(&a, &b, "cannot be multiplied")),
        },
        BinOp::Div => match (a, b) {
            (Value::Num(x), Value::Num(y)) => {
                if y.f == 0.0 {
                    return Err(type_error2(&Value::Num(x), &Value::Num(y), "cannot be divided because the divisor is zero"));
                }
                Ok(num(x.f / y.f))
            }
            (Value::Str(x), Value::Str(y)) => Ok(string_split(&x, &y)),
            (a, b) => Err(type_error2(&a, &b, "cannot be divided")),
        },
        BinOp::Mod => match (a, b) {
            (Value::Num(x), Value::Num(y)) => {
                if x.f.is_nan() || y.f.is_nan() {
                    return Ok(num(f64::NAN));
                }
                let dtoi = |n: f64| -> i64 {
                    if n < i64::MIN as f64 {
                        i64::MIN
                    } else if -n <= i64::MIN as f64 {
                        i64::MAX
                    } else {
                        n as i64
                    }
                };
                let bi = dtoi(y.f);
                if bi == 0 {
                    return Err(type_error2(&Value::Num(x), &Value::Num(y), "cannot be divided (remainder) because the divisor is zero"));
                }
                Ok(num(if bi == -1 { 0.0 } else { (dtoi(x.f) % bi) as f64 }))
            }
            (a, b) => Err(type_error2(&a, &b, "cannot be divided (remainder)")),
        },
        BinOp::Eq => cmp_checked(Value::Bool(a == b), "Equality check too deep"),
        BinOp::Ne => cmp_checked(Value::Bool(a != b), "Equality check too deep"),
        BinOp::Lt => cmp_checked(Value::Bool(a.compare(&b) == Ordering::Less), "Comparison too deep"),
        BinOp::Le => cmp_checked(Value::Bool(a.compare(&b) != Ordering::Greater), "Comparison too deep"),
        BinOp::Gt => cmp_checked(Value::Bool(a.compare(&b) == Ordering::Greater), "Comparison too deep"),
        BinOp::Ge => cmp_checked(Value::Bool(a.compare(&b) != Ordering::Less), "Comparison too deep"),
    }
}

pub fn string_split(s: &str, sep: &str) -> Value {
    if sep.is_empty() {
        return Value::arr(s.chars().map(|c| Value::string(c.to_string())).collect());
    }
    if s.is_empty() {
        return Value::arr(vec![]);
    }
    Value::arr(s.split(sep).map(Value::str).collect())
}

pub fn array_indexes(a: &[Value], b: &[Value]) -> Value {
    let mut res = vec![];
    if b.is_empty() {
        return Value::arr(res);
    }
    for ai in 0..a.len() {
        if ai + b.len() <= a.len() && b.iter().enumerate().all(|(bi, x)| a[ai + bi] == *x) {
            res.push(num(ai as f64));
        }
    }
    Value::arr(res)
}

/// jvp_contains; Err when nested deeper than MAX_CONTAINS_DEPTH.
fn contains(a: &Value, b: &Value, depth: usize) -> Result<bool, ()> {
    if depth > 10000 {
        return Err(());
    }
    Ok(match (a, b) {
        (Value::Obj(x), Value::Obj(y)) => {
            for (k, bv) in y.iter() {
                match x.get(k) {
                    Some(av) if contains(av, bv, depth + 1)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        (Value::Arr(x), Value::Arr(y)) => {
            for bv in y.iter() {
                let mut found = false;
                for av in x.iter() {
                    if contains(av, bv, depth + 1)? {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Str(x), Value::Str(y)) => x.contains(&**y),
        _ => a == b,
    })
}

fn to_string(v: &Value) -> Value {
    match v {
        Value::Str(_) => v.clone(),
        other => Value::string(dump(other, &Fmt::compact())),
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn escape(s: &str, table: &[(char, &str)]) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c == '\0' {
            out.push_str("\\0");
            continue;
        }
        match table.iter().find(|(x, _)| *x == c) {
            Some((_, r)) => out.push_str(r),
            None => out.push(c),
        }
    }
    out
}

pub fn format(v: &Value, fmt: &str) -> Result<Value, Flow> {
    match fmt {
        "json" => Ok(Value::string(dump(v, &Fmt::compact()))),
        "text" => Ok(to_string(v)),
        "csv" | "tsv" => {
            let (quotes, sep, table, msg): (&str, &str, &[(char, &str)], &str) = if fmt == "csv" {
                ("\"", ",", &[('"', "\"\"")], "cannot be csv-formatted, only array")
            } else {
                ("", "\t", &[('\t', "\\t"), ('\r', "\\r"), ('\n', "\\n"), ('\\', "\\\\")], "cannot be tsv-formatted, only array")
            };
            let Value::Arr(a) = v else { return Err(type_error(v, msg)) };
            let mut line = String::new();
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    line.push_str(sep);
                }
                match x {
                    Value::Null => {}
                    Value::Bool(_) => line.push_str(&dump(x, &Fmt::compact())),
                    Value::Num(n) => {
                        if !n.f.is_nan() {
                            line.push_str(&dump(x, &Fmt::compact()));
                        }
                    }
                    Value::Str(s) => {
                        line.push_str(quotes);
                        line.push_str(&escape(s, table));
                        line.push_str(quotes);
                    }
                    other => return Err(type_error(other, "is not valid in a csv row")),
                }
            }
            Ok(Value::string(line))
        }
        "html" => {
            let s = to_string(v);
            let Value::Str(s) = s else { unreachable!() };
            Ok(Value::string(escape(&s, &[('&', "&amp;"), ('<', "&lt;"), ('>', "&gt;"), ('\'', "&apos;"), ('"', "&quot;")])))
        }
        "uri" => {
            let Value::Str(s) = to_string(v) else { unreachable!() };
            let mut out = String::new();
            for b in s.bytes() {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    out.push(b as char);
                } else {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            Ok(Value::string(out))
        }
        "urid" => {
            let Value::Str(s) = to_string(v) else { unreachable!() };
            let bad = || type_error(&Value::Str(s.clone()), "is not a valid uri encoding");
            let b = s.as_bytes();
            let mut out = vec![];
            let mut i = 0;
            while i < b.len() {
                if b[i] != b'%' {
                    out.push(b[i]);
                    i += 1;
                    continue;
                }
                if i + 2 >= b.len() + 0 && i + 2 > b.len() - 1 + 1 {
                    return Err(bad());
                }
                let h = |c: u8| (c as char).to_digit(16);
                match (b.get(i + 1).and_then(|c| h(*c)), b.get(i + 2).and_then(|c| h(*c))) {
                    (Some(hi), Some(lo)) => out.push((hi * 16 + lo) as u8),
                    _ => return Err(bad()),
                }
                i += 3;
            }
            match String::from_utf8(out) {
                Ok(s) => Ok(Value::string(s)),
                Err(_) => Err(bad()),
            }
        }
        "sh" => {
            let items: Vec<Value> = match v {
                Value::Arr(a) => (**a).clone(),
                other => vec![other.clone()],
            };
            let mut line = String::new();
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                match x {
                    Value::Null | Value::Bool(_) | Value::Num(_) => line.push_str(&dump(x, &Fmt::compact())),
                    Value::Str(s) => {
                        line.push('\'');
                        line.push_str(&escape(s, &[('\'', "'\\''")]));
                        line.push('\'');
                    }
                    other => return Err(type_error(other, "can not be escaped for shell")),
                }
            }
            Ok(Value::string(line))
        }
        "base64" => {
            let Value::Str(s) = to_string(v) else { unreachable!() };
            let d = s.as_bytes();
            let mut out = String::new();
            for chunk in d.chunks(3) {
                let mut code = 0u32;
                for j in 0..3 {
                    code <<= 8;
                    code |= *chunk.get(j).unwrap_or(&0) as u32;
                }
                let mut buf = [0u8; 4];
                for (j, b) in buf.iter_mut().enumerate() {
                    *b = BASE64[((code >> (18 - j * 6)) & 0x3f) as usize];
                }
                if chunk.len() < 3 {
                    buf[3] = b'=';
                }
                if chunk.len() < 2 {
                    buf[2] = b'=';
                }
                out.push_str(std::str::from_utf8(&buf).unwrap());
            }
            Ok(Value::string(out))
        }
        "base64d" => {
            let Value::Str(s) = to_string(v) else { unreachable!() };
            let mut out = vec![];
            let mut code = 0u32;
            let mut n = 0;
            for &c in s.as_bytes() {
                if c == b'=' {
                    break;
                }
                let Some(d) = BASE64.iter().position(|&x| x == c) else {
                    return Err(type_error(&Value::Str(s.clone()), "is not valid base64 data"));
                };
                code = (code << 6) | d as u32;
                n += 1;
                if n == 4 {
                    out.push((code >> 16) as u8);
                    out.push((code >> 8) as u8);
                    out.push(code as u8);
                    n = 0;
                    code = 0;
                }
            }
            match n {
                3 => {
                    out.push((code >> 10) as u8);
                    out.push((code >> 2) as u8);
                }
                2 => out.push((code >> 4) as u8),
                1 => return Err(type_error(&Value::Str(s.clone()), "trailing base64 byte found")),
                _ => {}
            }
            Ok(Value::string(crate::value::utf8_lossy(&out)))
        }
        other => Err(err(format!("{other} is not a valid format"))),
    }
}

fn is_ws(c: char) -> bool {
    let c = c as u32;
    (0x9..=0xD).contains(&c) || c == 0x20 || c == 0x85 || c == 0xA0 || c == 0x1680 || (0x2000..=0x200A).contains(&c) || c == 0x2028 || c == 0x2029 || c == 0x202F || c == 0x205F || c == 0x3000
}

fn sort_items(objects: &[Value], keys: &[Value]) -> Vec<(Value, Value)> {
    let mut v: Vec<(usize, Value, Value)> = objects.iter().cloned().zip(keys.iter().cloned()).enumerate().map(|(i, (o, k))| (i, o, k)).collect();
    v.sort_by(|a, b| a.2.compare(&b.2).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(_, o, k)| (o, k)).collect()
}

fn keys_of(v: &Value) -> Result<Value, Flow> {
    match v {
        Value::Obj(m) => {
            let mut ks: Vec<&Rc<str>> = m.keys().collect();
            ks.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            Ok(Value::arr(ks.into_iter().map(|k| Value::Str(k.clone())).collect()))
        }
        Value::Arr(a) => Ok(Value::arr((0..a.len()).map(|i| num(i as f64)).collect())),
        other => Err(type_error(other, "has no keys")),
    }
}

fn minmax(values: &Value, keys: &Value, is_min: bool) -> Result<Value, Flow> {
    let (Value::Arr(vs), Value::Arr(ks)) = (values, keys) else { return Err(type_error2(values, keys, "cannot be iterated over")) };
    if vs.len() != ks.len() {
        return Err(type_error2(values, keys, "have wrong length"));
    }
    if vs.is_empty() {
        return Ok(Value::Null);
    }
    let mut ret = 0;
    for i in 1..vs.len() {
        let c = ks[i].compare(&ks[ret]);
        if (c == Ordering::Less) == is_min {
            ret = i;
        }
    }
    Ok(vs[ret].clone())
}

extern "C" {
    fn tgamma(x: f64) -> f64;
    fn lgamma(x: f64) -> f64;
    fn lgamma_r(x: f64, sign: *mut i32) -> f64;
    fn erf(x: f64) -> f64;
    fn erfc(x: f64) -> f64;
    fn j0(x: f64) -> f64;
    fn j1(x: f64) -> f64;
    fn y0(x: f64) -> f64;
    fn y1(x: f64) -> f64;
    fn jn(n: i32, x: f64) -> f64;
    fn yn(n: i32, x: f64) -> f64;
    fn logb(x: f64) -> f64;
    fn nearbyint(x: f64) -> f64;
    fn rint(x: f64) -> f64;
    fn remainder(x: f64, y: f64) -> f64;
    fn fdim(x: f64, y: f64) -> f64;
    fn fmod(x: f64, y: f64) -> f64;
    fn nextafter(x: f64, y: f64) -> f64;
    fn ldexp(x: f64, e: i32) -> f64;
    fn scalbln(x: f64, e: libc::c_long) -> f64;
    fn frexp(x: f64, e: *mut i32) -> f64;
    fn modf(x: f64, i: *mut f64) -> f64;
    fn significand(x: f64) -> f64;
    fn exp10(x: f64) -> f64;
}

fn math1(name: &str, x: f64) -> Option<f64> {
    Some(unsafe {
        match name {
            "acos" => x.acos(),
            "acosh" => x.acosh(),
            "asin" => x.asin(),
            "asinh" => x.asinh(),
            "atan" => x.atan(),
            "atanh" => x.atanh(),
            "cbrt" => x.cbrt(),
            "cos" => x.cos(),
            "cosh" => x.cosh(),
            "exp" => x.exp(),
            "exp2" => x.exp2(),
            "floor" => x.floor(),
            "j0" => j0(x),
            "j1" => j1(x),
            "log" => x.ln(),
            "log10" => x.log10(),
            "log2" => x.log2(),
            "sin" => x.sin(),
            "sinh" => x.sinh(),
            "sqrt" => x.sqrt(),
            "tan" => x.tan(),
            "tanh" => x.tanh(),
            "tgamma" => tgamma(x),
            "y0" => y0(x),
            "y1" => y1(x),
            "ceil" => x.ceil(),
            "erf" => erf(x),
            "erfc" => erfc(x),
            "exp10" => exp10(x),
            "expm1" => x.exp_m1(),
            "fabs" => x.abs(),
            "gamma" | "lgamma" => lgamma(x),
            "log1p" => x.ln_1p(),
            "logb" => logb(x),
            "nearbyint" => nearbyint(x),
            "rint" => rint(x),
            "round" => x.round(),
            "significand" => significand(x),
            "trunc" => x.trunc(),
            _ => return None,
        }
    })
}

fn math2(name: &str, a: f64, b: f64) -> Option<f64> {
    Some(unsafe {
        match name {
            "atan2" => a.atan2(b),
            "hypot" => a.hypot(b),
            "pow" => a.powf(b),
            "remainder" | "drem" => remainder(a, b),
            "copysign" => a.copysign(b),
            "fdim" => fdim(a, b),
            "fmax" => a.max(b),
            "fmin" => a.min(b),
            "fmod" => fmod(a, b),
            "nextafter" | "nexttoward" => nextafter(a, b),
            "scalb" => a * b.exp2(),
            "scalbln" => scalbln(a, b as libc::c_long),
            "ldexp" => ldexp(a, b as i32),
            "jn" => jn(a as i32, b),
            "yn" => yn(a as i32, b),
            _ => return None,
        }
    })
}

/// The functions the CLI's `builtins` lists (the natives; builtin.jq's come from its parse).
pub const NATIVE_NAMES: &[(&str, usize)] = &[
    ("empty", 0), ("not", 0), ("path", 1), ("last", 1), ("range", 3), ("_negate", 0), ("tojson", 0), ("fromjson", 0), ("tonumber", 0), ("toboolean", 0),
    ("tostring", 0), ("keys", 0), ("keys_unsorted", 0), ("startswith", 1), ("endswith", 1), ("split", 1), ("explode", 0), ("implode", 0),
    ("_strindices", 1), ("trim", 0), ("ltrim", 0), ("rtrim", 0), ("setpath", 2), ("getpath", 1), ("delpaths", 1), ("has", 1), ("contains", 1),
    ("length", 0), ("utf8bytelength", 0), ("type", 0), ("isinfinite", 0), ("isnan", 0), ("isnormal", 0), ("infinite", 0), ("nan", 0), ("sort", 0),
    ("_sort_by_impl", 1), ("_group_by_impl", 1), ("unique", 0), ("_unique_by_impl", 1), ("bsearch", 1), ("min", 0), ("max", 0),
    ("_min_by_impl", 1), ("_max_by_impl", 1), ("error", 0), ("format", 1), ("env", 0), ("halt", 0), ("halt_error", 1), ("get_search_list", 0),
    ("get_prog_origin", 0), ("get_jq_origin", 0), ("_match_impl", 3), ("modulemeta", 0), ("input", 0), ("debug", 0), ("stderr", 0),
    ("fromdateiso8601", 0), ("strptime", 1), ("strftime", 1), ("strflocaltime", 1), ("mktime", 0), ("gmtime", 0), ("localtime", 0), ("now", 0),
    ("input_filename", 0), ("input_line_number", 0), ("have_decnum", 0), ("have_literal_numbers", 0), ("_plus", 2), ("_minus", 2),
    ("_multiply", 2), ("_divide", 2), ("_mod", 2), ("_equal", 2), ("_notequal", 2), ("_less", 2), ("_greater", 2), ("_lesseq", 2),
    ("_greatereq", 2), ("acos", 0), ("acosh", 0), ("asin", 0), ("asinh", 0), ("atan", 0), ("atanh", 0), ("cbrt", 0), ("cos", 0), ("cosh", 0),
    ("exp", 0), ("exp2", 0), ("floor", 0), ("j0", 0), ("j1", 0), ("log", 0), ("log10", 0), ("log2", 0), ("sin", 0), ("sinh", 0), ("sqrt", 0),
    ("tan", 0), ("tanh", 0), ("tgamma", 0), ("y0", 0), ("y1", 0), ("ceil", 0), ("erf", 0), ("erfc", 0), ("exp10", 0), ("expm1", 0), ("fabs", 0),
    ("gamma", 0), ("lgamma", 0), ("log1p", 0), ("logb", 0), ("nearbyint", 0), ("rint", 0), ("round", 0), ("significand", 0), ("trunc", 0),
    ("atan2", 2), ("hypot", 2), ("pow", 2), ("remainder", 2), ("copysign", 2), ("drem", 2), ("fdim", 2), ("fmax", 2), ("fmin", 2), ("fmod", 2),
    ("nextafter", 2), ("nexttoward", 2), ("scalb", 2), ("scalbln", 2), ("ldexp", 2), ("jn", 2), ("yn", 2), ("fma", 3), ("modf", 0), ("frexp", 0),
    ("lgamma_r", 0), ("builtins", 0), ("while", 2), ("until", 2), ("repeat", 1), ("add", 0), ("add", 1), ("join", 1), ("_flatten", 1),
];

/// builtin.jq definitions replaced by native loops.
pub const NATIVE_OVERRIDES: &[(&str, usize)] = &[("while", 2), ("until", 2), ("repeat", 1), ("add", 0), ("add", 1), ("join", 1), ("_flatten", 1)];

/// builtin.jq's `_flatten($x)`: `reduce .[] as $i ([]; if $i | type == "array" and $x != 0 then
/// . + ($i | _flatten($x - 1)) else . + [$i] end)`, recursing in Rust (data depth, not jq's).
fn flatten(v: &Value, x: &Value, out: &mut Vec<Value>) -> Result<(), Flow> {
    for i in iterate(v)? {
        if matches!(i, Value::Arr(_)) && *x != Value::num(0.0) {
            let x1 = binop(BinOp::Sub, x.clone(), Value::num(1.0))?;
            flatten(&i, &x1, out)?;
        } else {
            out.push(i);
        }
    }
    Ok(())
}

/// `reduce f as $x (null; . + $x)` with the sum held here: strings are appended to one
/// buffer, arrays and objects added to in place.
enum Accum {
    Val(Value),
    Str(String),
}

impl Accum {
    fn add(&mut self, x: Value) -> Result<(), Flow> {
        match (&mut *self, x) {
            (Accum::Str(_), Value::Null) => {}
            (Accum::Str(b), Value::Str(s)) => b.push_str(&s),
            (Accum::Val(Value::Str(a)), Value::Str(s)) => {
                let mut b = String::with_capacity(a.len() + s.len());
                b.push_str(a);
                b.push_str(&s);
                *self = Accum::Str(b);
            }
            (_, x) => {
                let cur = std::mem::replace(self, Accum::Val(Value::Null)).finish();
                *self = Accum::Val(binop(BinOp::Add, cur, x)?);
            }
        }
        Ok(())
    }
    fn finish(self) -> Value {
        match self {
            Accum::Val(v) => v,
            Accum::Str(s) => Value::string(s),
        }
    }
}

fn iterate(v: &Value) -> Result<Vec<Value>, Flow> {
    match v {
        Value::Arr(a) => Ok((**a).clone()),
        Value::Obj(m) => Ok(m.values().cloned().collect()),
        other => Err(err(format!("Cannot iterate over {} ({})", other.kind(), dump_trunc(other, 30)))),
    }
}

/// builtin.jq's join/1: `reduce .[] as $i (null; (if .==null then "" else .+$x end) | . + ($i |
/// if type=="boolean" or type=="number" then tostring end)) // ""`, on one buffer.
fn join(input: &Value, sep: &Value) -> Result<Value, Flow> {
    let mut acc: Option<String> = None;
    for i in iterate(input)? {
        let mut s = match acc.take() {
            None => String::new(),
            Some(mut s) => {
                match sep {
                    Value::Str(x) => s.push_str(x),
                    Value::Null => {}
                    other => return Err(type_error2(&Value::string(s), other, "cannot be added")),
                }
                s
            }
        };
        let r = match i {
            Value::Bool(_) | Value::Num(_) => to_string(&i),
            other => other,
        };
        match r {
            Value::Str(x) => s.push_str(&x),
            Value::Null => {}
            other => return Err(type_error2(&Value::string(s), &other, "cannot be added")),
        }
        acc = Some(s);
    }
    Ok(Value::string(acc.unwrap_or_default()))
}

/// A generator's outputs up to its first error (the error comes after them).
fn collect(it: &mut Interp, f: &'static Ast, env: &Env, input: Pv, paths: bool) -> (Vec<Pv>, Option<Flow>) {
    let mut out = vec![];
    let r = it.eval(f, env, input, paths, &mut |_, v| {
        out.push(v);
        Ok(())
    });
    (out, r.err())
}

/// while(cond; update) and until(cond; next), depth first without recursing: a stack of
/// pending values (each level's outputs), and of a value's pending condition outputs.
fn loop_native(it: &mut Interp, env: &Env, is_while: bool, cond: &'static Ast, step: &'static Ast, input: Pv, paths: bool, cb: &mut dyn FnMut(&mut Interp, Pv) -> R) -> R {
    enum Frame {
        Vals(std::collections::VecDeque<Pv>, Option<Flow>),
        Conds(Pv, std::collections::VecDeque<Pv>, Option<Flow>),
    }
    let mut stack = vec![Frame::Vals(std::collections::VecDeque::from([input]), None)];
    while let Some(top) = stack.last_mut() {
        match top {
            Frame::Vals(vals, e) => match vals.pop_front() {
                Some(v) => {
                    let (cs, ce) = collect(it, cond, env, Pv::val(v.v.clone()), false);
                    stack.push(Frame::Conds(v, cs.into(), ce));
                }
                None => {
                    if let Some(e) = e.take() {
                        return Err(e);
                    }
                    stack.pop();
                }
            },
            Frame::Conds(v, cs, e) => match cs.pop_front() {
                Some(c) => {
                    let v = v.clone();
                    if is_while {
                        if c.v.truthy() {
                            cb(it, v.clone())?;
                            let (ups, ue) = collect(it, step, env, v, paths);
                            stack.push(Frame::Vals(ups.into(), ue));
                        }
                    } else if c.v.truthy() {
                        cb(it, v)?;
                    } else {
                        let (ups, ue) = collect(it, step, env, v, paths);
                        stack.push(Frame::Vals(ups.into(), ue));
                    }
                }
                None => {
                    if let Some(e) = e.take() {
                        return Err(e);
                    }
                    stack.pop();
                }
            },
        }
    }
    Ok(())
}

/// A native called with its arguments' values.
fn simple(it: &mut Interp, name: &str, input: &Value, a: &[Value]) -> Option<Result<Value, Flow>> {
    crate::value::took_too_deep();
    let r = (|| -> Result<Value, Flow> {
        if a.is_empty() {
            if let Some(op) = math1_names(name) {
                let Value::Num(n) = input else { return Err(type_error(input, "number required")) };
                return Ok(num(math1(op, n.f).unwrap()));
            }
        }
        if a.len() == 2 && math2(name, 0.0, 0.0).is_some() {
            if !matches!(a[0], Value::Num(_)) {
                return Err(type_error(&a[0], "number required"));
            }
            if !matches!(a[1], Value::Num(_)) {
                return Err(type_error(&a[1], "number required"));
            }
            return Ok(num(math2(name, f(&a[0]), f(&a[1])).unwrap()));
        }
        Ok(match (name, a.len()) {
            ("fma", 3) => {
                for x in a {
                    if !matches!(x, Value::Num(_)) {
                        return Err(type_error(x, "number required"));
                    }
                }
                num(f(&a[0]).mul_add(f(&a[1]), f(&a[2])))
            }
            ("modf", 0) | ("frexp", 0) | ("lgamma_r", 0) => {
                let Value::Num(n) = input else { return Err(type_error(input, "number required")) };
                unsafe {
                    match name {
                        "modf" => {
                            let mut i = 0.0;
                            let fr = modf(n.f, &mut i);
                            Value::arr(vec![num(fr), num(i)])
                        }
                        "frexp" => {
                            let mut e = 0;
                            let m = frexp(n.f, &mut e);
                            Value::arr(vec![num(m), num(e as f64)])
                        }
                        _ => {
                            let mut s = 0;
                            let g = lgamma_r(n.f, &mut s);
                            Value::arr(vec![num(g), num(s as f64)])
                        }
                    }
                }
            }
            ("_negate", 0) => negate(input.clone())?,
            ("tojson", 0) => Value::string(dump(input, &Fmt::compact())),
            ("fromjson", 0) => {
                let Value::Str(s) = input else { return Err(type_error(input, "only strings can be parsed")) };
                match crate::json::parse_one(s) {
                    Ok(v) => v,
                    Err(e) => return Err(err(format!("{} (while parsing '{}')", e, s))),
                }
            }
            ("tonumber", 0) => match input {
                Value::Num(_) => input.clone(),
                Value::Str(s) => {
                    if s.contains('\0') {
                        return Err(type_error(input, "cannot be parsed as a number"));
                    }
                    match crate::json::parse_number(s.as_bytes()) {
                        Some(n) => Value::Num(n),
                        None => return Err(type_error(input, "cannot be parsed as a number")),
                    }
                }
                _ => return Err(type_error(input, "cannot be parsed as a number")),
            },
            ("toboolean", 0) => match input {
                Value::Bool(_) => input.clone(),
                Value::Str(s) if &**s == "true" => Value::Bool(true),
                Value::Str(s) if &**s == "false" => Value::Bool(false),
                _ => return Err(type_error(input, "cannot be parsed as a boolean")),
            },
            ("tostring", 0) => to_string(input),
            ("keys", 0) => keys_of(input)?,
            ("keys_unsorted", 0) => match input {
                Value::Obj(m) => Value::arr(m.keys().map(|k| Value::Str(k.clone())).collect()),
                other => keys_of(other)?,
            },
            ("startswith", 1) => match (input, &a[0]) {
                (Value::Str(x), Value::Str(y)) => Value::Bool(x.as_bytes().starts_with(y.as_bytes())),
                _ => return Err(err("startswith() requires string inputs")),
            },
            ("endswith", 1) => match (input, &a[0]) {
                (Value::Str(x), Value::Str(y)) => Value::Bool(x.as_bytes().ends_with(y.as_bytes())),
                _ => return Err(err("endswith() requires string inputs")),
            },
            ("split", 1) => match (input, &a[0]) {
                (Value::Str(x), Value::Str(y)) => string_split(x, y),
                _ => return Err(err("split input and separator must be strings")),
            },
            ("explode", 0) => match input {
                Value::Str(s) => Value::arr(s.chars().map(|c| num(c as u32 as f64)).collect()),
                _ => return Err(err("explode input must be a string")),
            },
            ("implode", 0) => {
                let Value::Arr(arr) = input else { return Err(err("implode input must be an array")) };
                let mut s = String::new();
                for n in arr.iter() {
                    let Value::Num(x) = n else { return Err(type_error(n, "can't be imploded, unicode codepoint needs to be numeric")) };
                    if x.f.is_nan() {
                        return Err(type_error(n, "can't be imploded, unicode codepoint needs to be numeric"));
                    }
                    let mut c = x.f as i64;
                    if !(0..=0x10FFFF).contains(&c) || (0xD800..=0xDFFF).contains(&c) {
                        c = 0xFFFD;
                    }
                    s.push(char::from_u32(c as u32).unwrap_or('\u{FFFD}'));
                }
                Value::string(s)
            }
            ("_strindices", 1) => {
                let Value::Str(s) = input else { return Err(type_error(input, "cannot be searched, as it is not a string")) };
                let Value::Str(k) = &a[0] else { return Err(type_error(&a[0], "is not a string")) };
                let mut out = vec![];
                if !k.is_empty() {
                    let sb = s.as_bytes();
                    let kb = k.as_bytes();
                    let mut cp = 0usize;
                    let mut last = 0usize;
                    let mut i = 0usize;
                    while i + kb.len() <= sb.len() {
                        if &sb[i..i + kb.len()] == kb {
                            cp += s[last..i].chars().count();
                            last = i;
                            out.push(num(cp as f64));
                        }
                        i += 1;
                    }
                }
                Value::arr(out)
            }
            ("trim", 0) | ("ltrim", 0) | ("rtrim", 0) => {
                let Value::Str(s) = input else { return Err(err("trim input must be a string")) };
                let t = match name {
                    "trim" => s.trim_matches(is_ws),
                    "ltrim" => s.trim_start_matches(is_ws),
                    _ => s.trim_end_matches(is_ws),
                };
                Value::str(t)
            }
            ("setpath", 2) => {
                let Value::Arr(p) = &a[0] else { return Err(err("Path must be specified as an array")) };
                if p.len() > 10000 {
                    return Err(err("Path too deep"));
                }
                setpath(input.clone(), p, a[1].clone())?
            }
            ("delpaths", 1) => {
                let Value::Arr(ps) = &a[0] else { return Err(err("Paths must be specified as an array")) };
                delpaths(input.clone(), ps)?
            }
            ("has", 1) => match (input, &a[0]) {
                (Value::Null, _) => Value::Bool(false),
                (Value::Obj(m), Value::Str(k)) => Value::Bool(m.contains_key(k)),
                (Value::Arr(arr), Value::Num(n)) => Value::Bool(!n.f.is_nan() && n.f >= 0.0 && (n.f as i64) < arr.len() as i64),
                (t, k) => return Err(err(format!("Cannot check whether {} has a {} key", t.kind(), k.kind()))),
            },
            ("contains", 1) => {
                if input.kind() == a[0].kind() {
                    match contains(input, &a[0], 0) {
                        Ok(b) => Value::Bool(b),
                        Err(()) => return Err(err("Containment check too deep")),
                    }
                } else {
                    return Err(type_error2(input, &a[0], "cannot have their containment checked"));
                }
            }
            ("length", 0) => match input {
                Value::Arr(x) => num(x.len() as f64),
                Value::Obj(m) => num(m.len() as f64),
                Value::Str(s) => num(s.chars().count() as f64),
                Value::Num(n) => abs_num(n),
                Value::Null => num(0.0),
                other => return Err(type_error(other, "has no length")),
            },
            ("utf8bytelength", 0) => match input {
                Value::Str(s) => num(s.len() as f64),
                other => return Err(type_error(other, "only strings have UTF-8 byte length")),
            },
            ("type", 0) => Value::str(input.kind()),
            ("isinfinite", 0) => Value::Bool(input.as_f64().map_or(false, |x| x.is_infinite())),
            ("isnan", 0) => Value::Bool(input.as_f64().map_or(false, |x| x.is_nan())),
            ("isnormal", 0) => Value::Bool(input.as_f64().map_or(false, |x| x.is_normal())),
            ("infinite", 0) => num(f64::INFINITY),
            ("nan", 0) => num(f64::NAN),
            ("sort", 0) => match input {
                Value::Arr(x) => Value::arr(sort_items(x, x).into_iter().map(|(o, _)| o).collect()),
                other => return Err(type_error(other, "cannot be sorted, as it is not an array")),
            },
            ("_sort_by_impl", 1) | ("_group_by_impl", 1) | ("_unique_by_impl", 1) => {
                let (Value::Arr(x), Value::Arr(k)) = (input, &a[0]) else {
                    return Err(type_error2(input, &a[0], "cannot be sorted, as they are not both arrays"));
                };
                if x.len() != k.len() {
                    return Err(type_error2(input, &a[0], "cannot be sorted, as they are not both arrays"));
                }
                let sorted = sort_items(x, k);
                match name {
                    "_sort_by_impl" => Value::arr(sorted.into_iter().map(|(o, _)| o).collect()),
                    "_group_by_impl" => {
                        let mut groups: Vec<Value> = vec![];
                        let mut cur: Vec<Value> = vec![];
                        let mut ck: Option<Value> = None;
                        for (o, k) in sorted {
                            if ck.as_ref().map_or(false, |c| *c == k) {
                                cur.push(o);
                            } else {
                                if ck.is_some() {
                                    groups.push(Value::arr(std::mem::take(&mut cur)));
                                }
                                ck = Some(k);
                                cur.push(o);
                            }
                        }
                        if ck.is_some() {
                            groups.push(Value::arr(cur));
                        }
                        Value::arr(groups)
                    }
                    _ => {
                        let mut out = vec![];
                        let mut ck: Option<Value> = None;
                        for (o, k) in sorted {
                            if ck.as_ref().map_or(true, |c| *c != k) {
                                out.push(o);
                                ck = Some(k);
                            }
                        }
                        Value::arr(out)
                    }
                }
            }
            ("unique", 0) => match input {
                Value::Arr(x) => {
                    let mut out: Vec<Value> = vec![];
                    for (o, _) in sort_items(x, x) {
                        if out.last().map_or(true, |l| *l != o) {
                            out.push(o);
                        }
                    }
                    Value::arr(out)
                }
                other => return Err(type_error(other, "cannot be sorted, as it is not an array")),
            },
            ("bsearch", 1) => {
                let Value::Arr(x) = input else { return Err(type_error(input, "cannot be searched from")) };
                let (mut lo, mut hi) = (0usize, x.len());
                let mut ans = None;
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    match a[0].compare(&x[mid]) {
                        Ordering::Equal => {
                            ans = Some(mid as f64);
                            break;
                        }
                        Ordering::Less => hi = mid,
                        Ordering::Greater => lo = mid + 1,
                    }
                }
                num(ans.unwrap_or(-1.0 - lo as f64))
            }
            ("min", 0) => minmax(input, input, true)?,
            ("max", 0) => minmax(input, input, false)?,
            ("_min_by_impl", 1) => minmax(input, &a[0], true)?,
            ("_max_by_impl", 1) => minmax(input, &a[0], false)?,
            ("format", 1) => {
                let Value::Str(fmt) = &a[0] else { return Err(type_error(&a[0], "is not a valid format")) };
                format(input, fmt)?
            }
            ("env", 0) => it.env_value.clone(),
            ("get_search_list", 0) => Value::arr(it.search_list.iter().map(|s| Value::str(s)).collect()),
            ("get_prog_origin", 0) => it.prog_origin.clone(),
            ("get_jq_origin", 0) => std::env::current_exe().ok().and_then(|p| p.parent().map(|d| Value::string(d.to_string_lossy().into_owned()))).unwrap_or(Value::Null),
            ("fromdateiso8601", 0) => {
                let Value::Str(s) = input else { return Err(err("fromdateiso8601 requires string inputs")) };
                match crate::time::iso8601_parse(s.as_bytes()) {
                    Some(t) => num(t),
                    None => return Err(err(format!("date \"{s}\" is not a valid ISO 8601 datetime"))),
                }
            }
            ("strptime", 1) => crate::time::strptime(input, &a[0])?,
            ("strftime", 1) => crate::time::strftime(input, &a[0], false)?,
            ("strflocaltime", 1) => crate::time::strftime(input, &a[0], true)?,
            ("mktime", 0) => crate::time::mktime(input)?,
            ("gmtime", 0) => crate::time::gmtime(input, false)?,
            ("localtime", 0) => crate::time::gmtime(input, true)?,
            ("now", 0) => num(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)),
            ("input_filename", 0) => it.inputs.as_ref().map(|i| i.filename()).unwrap_or(Value::Null),
            ("input_line_number", 0) => num(it.inputs.as_ref().map(|i| i.line()).unwrap_or(0) as f64),
            ("have_decnum", 0) | ("have_literal_numbers", 0) => Value::Bool(true),
            ("_match_impl", 3) => crate::regex::match_impl(input, &a[0], &a[1], &a[2])?,
            ("modulemeta", 0) => match input {
                Value::Str(name) => crate::modules::modulemeta(it, name)?,
                _ => return Err(err("modulemeta input module name must be a string")),
            },
            ("getpath", 1) => {
                let Value::Arr(p) = &a[0] else { return Err(err("Path must be specified as an array")) };
                getpath(input, p)?
            }
            (bin, 2) if bin.starts_with('_') => {
                let op = match bin {
                    "_plus" => BinOp::Add,
                    "_minus" => BinOp::Sub,
                    "_multiply" => BinOp::Mul,
                    "_divide" => BinOp::Div,
                    "_mod" => BinOp::Mod,
                    "_equal" => BinOp::Eq,
                    "_notequal" => BinOp::Ne,
                    "_less" => BinOp::Lt,
                    "_greater" => BinOp::Gt,
                    "_lesseq" => BinOp::Le,
                    "_greatereq" => BinOp::Ge,
                    _ => return Err(err("unknown")),
                };
                binop(op, a[0].clone(), a[1].clone())?
            }
            _ => return Err(err("\u{0}")),
        })
    })();
    match r {
        Err(Flow::Err(Value::Str(s))) if &*s == "\u{0}" => None,
        Ok(v) if crate::value::took_too_deep() => {
            let _ = v;
            Some(Err(err(if name == "_strindices" || name == "has" { "Equality check too deep" } else { "Comparison too deep" })))
        }
        r => Some(r),
    }
}

fn math1_names(name: &str) -> Option<&str> {
    math1(name, 0.0).map(|_| name)
}

/// Calls a builtin that is not jq code: the bytecoded ones and the C ones.
#[allow(clippy::too_many_arguments)]
pub fn call_native(it: &mut Interp, env: &Env, name: &str, args: &'static [Ast], input: Pv, paths: bool, span: (usize, usize), cb: &mut dyn FnMut(&mut Interp, Pv) -> R) -> R {
    let _ = span;
    match (name, args.len()) {
        ("empty", 0) => return Ok(()),
        ("not", 0) => return cb(it, Pv::val(Value::Bool(!input.v.truthy()))),
        ("error", 0) => return Err(Flow::Err(input.v)),
        ("path", 1) => {
            // Each path as it comes (what follows runs before the rest are looked for).
            let start = Pv { v: input.v, p: Some(Rc::new(vec![])) };
            return it.eval(&args[0], env, start, true, &mut |it, pv| match pv.p {
                Some(p) => cb(it, Pv::val(Value::arr((*p).clone()))),
                None => Err(err(format!("Invalid path expression with result {}", dump_trunc(&pv.v, 30)))),
            });
        }
        ("last", 1) => {
            let mut last = None;
            it.eval(&args[0], env, input, paths, &mut |_, v| {
                last = Some(v);
                Ok(())
            })?;
            // jq keeps it in a variable: in a path expression it has no path.
            return match last {
                Some(v) => cb(it, Pv::val(v.v)),
                None => Ok(()),
            };
        }
        ("range", 3) => {
            // $start, $end, $step: the first outermost (they are $-parameters).
            let inp = input.v.clone();
            return it.eval(&args[0], env, Pv::val(input.v.clone()), false, &mut |it, start| {
                let inp2 = inp.clone();
                it.eval(&args[1], env, Pv::val(inp.clone()), false, &mut |it, end| {
                    let start = start.v.clone();
                    it.eval(&args[2], env, Pv::val(inp2.clone()), false, &mut |it, step| {
                        let (Value::Num(_), Value::Num(_), Value::Num(st)) = (&start, &end.v, &step.v) else {
                            return Err(err("Range bounds and step must be numeric"));
                        };
                        let zero = Num::f(0.0);
                        let sign = num_cmp(st, &zero);
                        let mut cur = start.clone();
                        loop {
                            let Value::Num(c) = &cur else { unreachable!() };
                            let Value::Num(e) = &end.v else { unreachable!() };
                            let c1 = num_cmp(c, e);
                            let prod = (c1 as i32) * (sign as i32);
                            if prod >= 0 {
                                return Ok(());
                            }
                            cb(it, Pv::val(cur.clone()))?;
                            cur = num(c.f + st.f);
                        }
                    })
                })
            });
        }
        ("getpath", 1) => {
            return it.eval_args(env, args, &input.v.clone(), &mut |it, a| {
                let Value::Arr(p) = &a[0] else { return Err(err("Path must be specified as an array")) };
                if p.len() > 10000 {
                    return Err(err("Path too deep"));
                }
                let r = match getpath(&input.v, p) {
                    Ok(r) => r,
                    Err(e) => {
                        if paths {
                            return Err(e);
                        }
                        return Err(e);
                    }
                };
                let path = if paths {
                    match &input.p {
                        Some(base) => {
                            let mut v = (**base).clone();
                            v.extend(p.iter().cloned());
                            Some(Rc::new(v))
                        }
                        None => return Err(err(format!("Invalid path expression with result {}", dump_trunc(&input.v, 30)))),
                    }
                } else {
                    None
                };
                cb(it, Pv { v: r, p: path })
            });
        }
        ("input", 0) => {
            let next = it.inputs.as_mut().and_then(|i| i.next());
            return match next {
                Some(Ok(v)) => cb(it, Pv::val(v)),
                Some(Err(e)) => Err(err(e)),
                None => Err(err("break")),
            };
        }
        ("debug", 0) => {
            (it.debug_out)(&input.v);
            return cb(it, input);
        }
        ("stderr", 0) => {
            (it.stderr_out)(&input.v);
            return cb(it, input);
        }
        ("halt", 0) => return Err(Flow::Halt(0, None)),
        ("add", 0) => {
            let mut acc = Accum::Val(Value::Null);
            for x in iterate(&input.v)? {
                acc.add(x)?;
            }
            return cb(it, Pv::val(acc.finish()));
        }
        ("add", 1) => {
            let mut acc = Accum::Val(Value::Null);
            it.eval(&args[0], env, Pv::val(input.v.clone()), false, &mut |_, x| acc.add(x.v))?;
            return cb(it, Pv::val(acc.finish()));
        }
        ("_flatten", 1) => {
            let inp = input.v.clone();
            return it.eval_args(env, args, &input.v, &mut |it, a| {
                let mut out = vec![];
                flatten(&inp, &a[0], &mut out)?;
                cb(it, Pv::val(Value::arr(out)))
            });
        }
        ("join", 1) => {
            let inp = input.v.clone();
            return it.eval_args(env, args, &input.v, &mut |it, a| {
                let r = join(&inp, &a[0])?;
                cb(it, Pv::val(r))
            });
        }
        ("setpath", 2) | ("delpaths", 1) if !paths && args.iter().all(input_free) => {
            // The arguments first, so that the input itself (held only here) is changed in place.
            let mut combos: Vec<Vec<Value>> = vec![];
            let r = it.eval_args(env, args, &input.v, &mut |_, a| {
                combos.push(a.to_vec());
                Ok(())
            });
            let n = combos.len();
            let mut inp = Some(input.v);
            for (i, a) in combos.into_iter().enumerate() {
                let v = if i + 1 == n && r.is_ok() { inp.take().unwrap() } else { inp.clone().unwrap() };
                let out = if name == "setpath" {
                    let Value::Arr(p) = &a[0] else { return Err(err("Path must be specified as an array")) };
                    if p.len() > 10000 {
                        return Err(err("Path too deep"));
                    }
                    setpath(v, p, a[1].clone())?
                } else {
                    let Value::Arr(ps) = &a[0] else { return Err(err("Paths must be specified as an array")) };
                    delpaths(v, ps)?
                };
                cb(it, Pv::val(out))?;
            }
            return r;
        }
        ("while", 2) => return loop_native(it, env, true, &args[0], &args[1], input, paths, cb),
        ("until", 2) => return loop_native(it, env, false, &args[0], &args[1], input, paths, cb),
        ("repeat", 1) => loop {
            it.eval(&args[0], env, input.clone(), paths, cb)?;
        },
        ("halt_error", 1) => {
            return it.eval_args(env, args, &input.v.clone(), &mut |_, a| {
                let Value::Num(n) = &a[0] else { return Err(type_error(&input.v, "halt_error/1: number required")) };
                Err(Flow::Halt(n.f as i32, Some(input.v.clone())))
            });
        }
        ("builtins", 0) => {
            let mut names: Vec<String> = NATIVE_NAMES.iter().filter(|(n, _)| !n.starts_with('_')).map(|(n, a)| format!("{n}/{a}")).collect();
            for (n, a) in &it.builtin_order {
                if !n.starts_with('_') {
                    names.push(format!("{n}/{a}"));
                }
            }
            names.sort();
            names.dedup();
            return cb(it, Pv::val(Value::arr(names.into_iter().map(Value::string).collect())));
        }
        _ => {}
    }
    let inp = input.v.clone();
    let name_s = name.to_string();
    it.eval_args(env, args, &inp, &mut |it, a| match simple(it, &name_s, &inp, a) {
        Some(Ok(v)) => cb(it, Pv::val(v)),
        Some(Err(e)) => Err(e),
        None => Err(err(format!("{}/{} is not defined", name_s, a.len()))),
    })
}
