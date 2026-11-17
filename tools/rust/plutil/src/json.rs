//! JSON as Foundation's JSONSerialization reads and writes it (swift-corelibs-foundation): its
//! parser (trailing commas allowed, depth 512, its error messages and character indices) and its
//! writer (`\/` for slashes, the pretty form's two spaces and " : ", Swift's number texts).

use crate::num;
use crate::value::{natural_compare, Dict, Value};

#[derive(Debug)]
pub struct Error(pub String);

enum E {
    CannotConvert,
    Eof,
    Unexpected(usize),
    TooDeep(usize),
    InvalidHex(String, usize),
    UnexpectedEscape(usize),
    LowSurrogate,
    NoScalar,
    Unescaped(u8, usize),
    LeadingZero(usize),
    NotRepresentable(String),
    Fragment,
    InvalidUtf8(Vec<u8>, usize),
}

impl E {
    fn message(self) -> String {
        match self {
            E::CannotConvert => "Cannot convert input string to valid utf8 input.".into(),
            E::Eof => "Unexpected end of file during JSON parse.".into(),
            E::Unexpected(i) => format!("Invalid value around character {i}."),
            E::LowSurrogate => "Unexpected end of file during string parse (expected low-surrogate code point but did not find one).".into(),
            E::NoScalar => "Unable to convert hex escape sequence (no high character) to UTF8-encoded character.".into(),
            E::UnexpectedEscape(i) => format!("Invalid escape sequence around character {}.", i - 1),
            E::Fragment => "JSON text did not start with array or object and option to allow fragments not set.".into(),
            E::TooDeep(i) => format!("Too many nested arrays or dictionaries around character {}.", i + 1),
            E::InvalidHex(s, i) => format!("Invalid hex encoded sequence in \"{s}\" at {i}."),
            E::Unescaped(b'\\', i) => format!("Invalid escape sequence around character {i}."),
            E::Unescaped(_, i) => format!("Unescaped control character around character {i}."),
            E::LeadingZero(i) => format!("Number with leading zero around character {i}."),
            E::NotRepresentable(s) => format!("Number {s} is not representable in Swift."),
            E::InvalidUtf8(data, i) => format!("Invalid UTF-8 sequence {} bytes starting from character {i}.", data.len()),
        }
    }
}

/// The parsed value before it becomes Foundation objects (JSONValue).
enum J {
    Str(String),
    Num(String),
    Bool(bool),
    Null,
    Array(Vec<J>),
    Object(Dict2),
}

/// A JSON object: Swift's [String: JSONValue], the last of equal keys winning; kept in the order
/// keys first appear.
type Dict2 = Vec<(String, J)>;

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

fn ws(c: u8) -> bool {
    matches!(c, b' ' | b'\r' | b'\n' | b'\t')
}

impl Reader<'_> {
    fn peek(&self, off: usize) -> Option<u8> {
        self.b.get(self.i + off).copied()
    }

    fn read(&mut self) -> Option<u8> {
        if self.i >= self.b.len() {
            self.i = self.b.len();
            return None;
        }
        self.i += 1;
        Some(self.b[self.i - 1])
    }

    fn eof(&self) -> bool {
        self.i >= self.b.len()
    }

    fn consume_ws(&mut self) -> Result<u8, E> {
        let mut n = 0;
        while let Some(c) = self.peek(n) {
            if ws(c) {
                n += 1;
                continue;
            }
            self.i += n;
            return Ok(c);
        }
        Err(E::Eof)
    }

    fn parse(&mut self) -> Result<J, E> {
        self.consume_ws()?;
        let v = self.value()?;
        let mut n = 0;
        while let Some(c) = self.peek(n) {
            if ws(c) {
                n += 1;
                continue;
            }
            return Err(E::Unexpected(self.i + n));
        }
        Ok(v)
    }

    fn value(&mut self) -> Result<J, E> {
        let mut n = 0;
        while let Some(c) = self.peek(n) {
            match c {
                b'"' => {
                    self.i += n;
                    return Ok(J::Str(self.string()?));
                }
                b'{' => {
                    self.i += n;
                    return self.object();
                }
                b'[' => {
                    self.i += n;
                    return self.array();
                }
                b'f' | b't' => {
                    self.i += n;
                    return self.boolean();
                }
                b'n' => {
                    self.i += n;
                    self.literal(b"null")?;
                    return Ok(J::Null);
                }
                b'-' | b'0'..=b'9' => {
                    self.i += n;
                    return Ok(J::Num(self.number()?));
                }
                c if ws(c) => n += 1,
                _ => return Err(E::Unexpected(self.i)),
            }
        }
        Err(E::Eof)
    }

    fn array(&mut self) -> Result<J, E> {
        self.read();
        if self.depth >= 512 {
            return Err(E::TooDeep(self.i - 1));
        }
        self.depth += 1;
        let result = (|| {
            if self.consume_ws()? == b']' {
                self.i += 1;
                return Ok(J::Array(Vec::new()));
            }
            let mut a = Vec::new();
            loop {
                a.push(self.value()?);
                match self.consume_ws()? {
                    b']' => {
                        self.i += 1;
                        return Ok(J::Array(a));
                    }
                    b',' => {
                        self.i += 1;
                        if self.consume_ws()? == b']' {
                            self.i += 1;
                            return Ok(J::Array(a));
                        }
                    }
                    _ => return Err(E::Unexpected(self.i)),
                }
            }
        })();
        self.depth -= 1;
        result
    }

    fn object(&mut self) -> Result<J, E> {
        self.read();
        if self.depth >= 512 {
            return Err(E::TooDeep(self.i - 1));
        }
        self.depth += 1;
        let result = (|| {
            if self.consume_ws()? == b'}' {
                self.i += 1;
                return Ok(J::Object(Vec::new()));
            }
            let mut o: Dict2 = Vec::new();
            loop {
                let key = self.string()?;
                if self.consume_ws()? != b':' {
                    return Err(E::Unexpected(self.i));
                }
                self.i += 1;
                self.consume_ws()?;
                let v = self.value()?;
                match o.iter_mut().find(|(k, _)| *k == key) {
                    Some(entry) => entry.1 = v,
                    None => o.push((key, v)),
                }
                match self.consume_ws()? {
                    b'}' => {
                        self.i += 1;
                        return Ok(J::Object(o));
                    }
                    b',' => {
                        self.i += 1;
                        if self.consume_ws()? == b'}' {
                            self.i += 1;
                            return Ok(J::Object(o));
                        }
                    }
                    _ => return Err(E::Unexpected(self.i)),
                }
            }
        })();
        self.depth -= 1;
        result
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), E> {
        for &w in word {
            if self.read() != Some(w) {
                if self.eof() {
                    return Err(E::Eof);
                }
                return Err(E::Unexpected(self.i - 1));
            }
        }
        Ok(())
    }

    fn boolean(&mut self) -> Result<J, E> {
        if self.peek(0) == Some(b't') {
            self.literal(b"true")?;
            Ok(J::Bool(true))
        } else {
            self.literal(b"false")?;
            Ok(J::Bool(false))
        }
    }

    fn make_string(&self, start: usize, end: usize) -> Result<String, E> {
        String::from_utf8(self.b[start..end].to_vec()).map_err(|_| E::InvalidUtf8(self.b[start..end].to_vec(), start))
    }

    fn string(&mut self) -> Result<String, E> {
        if self.read() != Some(b'"') {
            return Err(E::Unexpected(self.i - 1));
        }
        let mut start = self.i;
        let mut copy = 0;
        let mut output: Option<String> = None;
        while let Some(c) = self.peek(copy) {
            match c {
                b'"' => {
                    self.i += copy + 1;
                    let tail = self.make_string(start, start + copy)?;
                    return Ok(match output {
                        Some(mut s) => {
                            s.push_str(&tail);
                            s
                        }
                        None => tail,
                    });
                }
                0..=31 => {
                    let at = self.i + copy;
                    self.make_string(start, at + 1)?;
                    return Err(E::Unescaped(c, at));
                }
                b'\\' => {
                    self.i += copy;
                    let part = self.make_string(start, start + copy)?;
                    output.get_or_insert_with(String::new).push_str(&part);
                    let escaped = self.escape()?;
                    output.as_mut().unwrap().push_str(&escaped);
                    start = self.i;
                    copy = 0;
                }
                _ => copy += 1,
            }
        }
        Err(E::Eof)
    }

    fn escape(&mut self) -> Result<String, E> {
        self.read();
        let Some(c) = self.read() else { return Err(E::Eof) };
        Ok(match c {
            b'"' => "\"".into(),
            b'\\' => "\\".into(),
            b'/' => "/".into(),
            b'b' => "\u{8}".into(),
            b'f' => "\u{c}".into(),
            b'n' => "\n".into(),
            b'r' => "\r".into(),
            b't' => "\t".into(),
            b'u' => self.unicode()?.to_string(),
            _ => return Err(E::UnexpectedEscape(self.i - 1)),
        })
    }

    fn hex4(&mut self) -> Result<u16, E> {
        let start = self.i;
        let mut digits = [0u8; 4];
        for d in digits.iter_mut() {
            *d = self.read().ok_or(E::Eof)?;
        }
        let mut v = 0u16;
        for &d in &digits {
            match (d as char).to_digit(16) {
                Some(x) => v = v << 4 | x as u16,
                None => return Err(E::InvalidHex(String::from_utf8_lossy(&digits).into_owned(), start)),
            }
        }
        Ok(v)
    }

    fn unicode(&mut self) -> Result<char, E> {
        let high = self.hex4()?;
        if high & 0xfc00 == 0xd800 {
            let (Some(slash), Some(u)) = (self.read(), self.read()) else { return Err(E::Eof) };
            if slash != b'\\' || u != b'u' {
                return Err(E::LowSurrogate);
            }
            let low = self.hex4()?;
            if low & 0xfc00 != 0xdc00 {
                return Err(E::LowSurrogate);
            }
            let value = (high as u32 - 0xd800) * 0x400 + (low as u32 - 0xdc00) + 0x10000;
            return char::from_u32(value).ok_or(E::NoScalar);
        }
        char::from_u32(high as u32).ok_or(E::NoScalar)
    }

    fn number(&mut self) -> Result<String, E> {
        #[derive(PartialEq)]
        enum Past {
            Operand,
            Point,
            Exp,
            ExpSign,
        }
        let mut past = Past::Operand;
        let first = self.peek(0).unwrap();
        let (mut since, mut leading_zero) = match first {
            b'0' => (1u32, true),
            b'-' => (0, false),
            _ => (1, false),
        };
        let mut n = 1;
        while let Some(c) = self.peek(n) {
            let at = self.i + n;
            match c {
                b'0' => {
                    if leading_zero {
                        return Err(E::LeadingZero(at));
                    }
                    if since == 0 && past == Past::Operand {
                        leading_zero = true;
                    }
                    n += 1;
                    since += 1;
                }
                b'1'..=b'9' => {
                    if leading_zero {
                        return Err(E::LeadingZero(at));
                    }
                    n += 1;
                    since += 1;
                }
                b'.' => {
                    if since == 0 || past != Past::Operand {
                        return Err(E::Unexpected(at));
                    }
                    n += 1;
                    leading_zero = false;
                    past = Past::Point;
                    since = 0;
                }
                b'e' | b'E' => {
                    if since == 0 || !(past == Past::Operand || past == Past::Point) {
                        return Err(E::Unexpected(at));
                    }
                    n += 1;
                    leading_zero = false;
                    past = Past::Exp;
                    since = 0;
                }
                b'+' | b'-' => {
                    if since != 0 || past != Past::Exp {
                        return Err(E::Unexpected(at));
                    }
                    n += 1;
                    past = Past::ExpSign;
                    since = 0;
                }
                b' ' | b'\r' | b'\n' | b'\t' | b',' | b']' | b'}' => {
                    if since == 0 {
                        return Err(E::Unexpected(at));
                    }
                    let s = String::from_utf8(self.b[self.i..self.i + n].to_vec()).unwrap();
                    self.i += n;
                    return Ok(s);
                }
                _ => return Err(E::Unexpected(at)),
            }
        }
        if since == 0 {
            return Err(E::Eof);
        }
        let s = String::from_utf8(self.b[self.i..].to_vec()).unwrap();
        self.i = self.b.len();
        Ok(s)
    }
}

/// NSNumber.fromJSONNumber: Int64 or UInt64 for integers that fit, else Double.
fn number(s: &str) -> Result<Value, E> {
    let integer = !s.contains('.') && !s.contains('e');
    let negative = s.starts_with('-');
    let digits = s.split('e').next().unwrap().chars().count();
    if integer {
        if negative {
            if digits <= 19 {
                if let Ok(i) = s.parse::<i64>() {
                    return Ok(Value::Int(i));
                }
            }
        } else if digits <= 20 {
            if let Ok(u) = s.parse::<u64>() {
                return Ok(if u > i64::MAX as u64 { Value::UInt(u) } else { Value::Int(u as i64) });
            }
        }
    }
    // Foundation keeps more than 17 digits as an NSDecimalNumber; here they become a Double.
    match s.parse::<f64>() {
        Ok(d) if d.is_finite() => Ok(Value::Real(d)),
        _ => Err(E::NotRepresentable(s.to_string())),
    }
}

fn to_value(j: J) -> Result<Value, E> {
    Ok(match j {
        J::Str(s) => Value::String(s),
        J::Num(s) => number(&s)?,
        J::Bool(b) => Value::Bool(b),
        J::Null => Value::Null,
        J::Array(a) => Value::array(a.into_iter().map(to_value).collect::<Result<_, _>>()?),
        J::Object(o) => {
            let mut d = Dict::with_capacity(o.len());
            for (k, v) in o {
                d.insert(k, to_value(v)?);
            }
            Value::dict(d)
        }
    })
}

/// detectEncoding: a BOM, or where the zero bytes are; the text as UTF-8.
fn utf8(b: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, E> {
    use std::borrow::Cow;
    let units16 = |b: &[u8], be: bool| -> Vec<u16> { b.chunks(2).filter(|c| c.len() == 2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect() };
    let from16 = |b: &[u8], be: bool| -> Result<Cow<'_, [u8]>, E> {
        if b.len() % 2 != 0 {
            return Err(E::CannotConvert);
        }
        String::from_utf16(&units16(b, be)).map(|s| Cow::Owned(s.into_bytes())).map_err(|_| E::CannotConvert)
    };
    let from32 = |b: &[u8], be: bool| -> Result<Cow<'_, [u8]>, E> {
        if b.len() % 4 != 0 {
            return Err(E::CannotConvert);
        }
        let mut s = String::new();
        for c in b.chunks(4) {
            let u = if be { u32::from_be_bytes([c[0], c[1], c[2], c[3]]) } else { u32::from_le_bytes([c[0], c[1], c[2], c[3]]) };
            s.push(char::from_u32(u).ok_or(E::CannotConvert)?);
        }
        Ok(Cow::Owned(s.into_bytes()))
    };
    if b.len() >= 4 {
        if b.starts_with(&[0xef, 0xbb, 0xbf]) {
            return Ok(Cow::Borrowed(&b[3..]));
        }
        if b.starts_with(&[0, 0, 0xfe, 0xff]) {
            return from32(&b[4..], true);
        }
        if b.starts_with(&[0xff, 0xfe, 0, 0]) {
            return from32(&b[4..], false);
        }
        if b.starts_with(&[0xff, 0xfe]) {
            return from16(&b[2..], false);
        }
        if b.starts_with(&[0xfe, 0xff]) {
            return from16(&b[2..], true);
        }
        match (b[0], b[1], b[2], b[3]) {
            (0, 0, 0, _) => return from32(b, true),
            (_, 0, 0, 0) => return from32(b, false),
            (0, _, 0, _) => return from16(b, true),
            (_, 0, _, 0) => return from16(b, false),
            _ => {}
        }
    } else if b.len() >= 2 {
        match (b[0], b[1]) {
            (0, _) => return from16(b, true),
            (_, 0) => return from16(b, false),
            _ => {}
        }
    }
    Ok(Cow::Borrowed(b))
}

/// JSONSerialization.jsonObject(with:options:): `fragments` allows a top level that is neither an
/// array nor an object.
pub fn parse(b: &[u8], fragments: bool) -> Result<Value, Error> {
    let run = || -> Result<Value, E> {
        let text = utf8(b)?;
        let mut r = Reader { b: &text, i: 0, depth: 0 };
        let j = r.parse()?;
        if !fragments && !matches!(j, J::Array(_) | J::Object(_)) {
            return Err(E::Fragment);
        }
        to_value(j)
    };
    run().map_err(|e| Error(e.message()))
}

/// JSONSerialization.isValidJSONObject: strings, finite numbers, bools, null, arrays and
/// dictionaries of them.
pub fn is_valid(v: &Value) -> bool {
    fn walk(v: &Value, depth: usize) -> bool {
        if depth >= 512 {
            return false;
        }
        match v {
            Value::String(_) | Value::Int(_) | Value::UInt(_) | Value::Bool(_) | Value::Null => true,
            Value::Real(r) => r.is_finite(),
            Value::Float(f) => f.is_finite(),
            Value::Array(a) => a.iter().all(|x| walk(x, depth + 1)),
            Value::Dict(d) => d.values().all(|x| walk(x, depth + 1)),
            _ => false,
        }
    }
    walk(v, 0)
}

struct Writer {
    out: String,
    indent: usize,
    pretty: bool,
    sorted: bool,
}

impl Writer {
    fn string(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '/' => self.out.push_str("\\/"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c if (c as u32) < 0x10 => self.out.push_str(&format!("\\u000{:x}", c as u32)),
                c if (c as u32) < 0x20 => self.out.push_str(&format!("\\u00{:x}", c as u32)),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    fn write_indent(&mut self) {
        self.out.extend(std::iter::repeat_n(' ', self.indent));
    }

    fn float(&mut self, s: String) {
        self.out.push_str(s.strip_suffix(".0").unwrap_or(&s));
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::String(s) => self.string(s),
            Value::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => self.out.push_str(&i.to_string()),
            Value::UInt(u) => self.out.push_str(&u.to_string()),
            Value::Real(r) => self.float(num::swift_double(*r)),
            Value::Float(f) => self.float(num::swift_float(*f)),
            Value::Null => self.out.push_str("null"),
            Value::Array(a) => {
                self.out.push('[');
                if self.pretty {
                    self.out.push('\n');
                    self.indent += 2;
                }
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(if self.pretty { ",\n" } else { "," });
                    }
                    if self.pretty {
                        self.write_indent();
                    }
                    self.value(item);
                }
                if self.pretty {
                    self.out.push('\n');
                    self.indent -= 2;
                    self.write_indent();
                }
                self.out.push(']');
            }
            Value::Dict(d) => {
                self.out.push('{');
                if self.pretty {
                    self.out.push('\n');
                    self.indent += 2;
                    if !d.is_empty() {
                        self.write_indent();
                    }
                }
                let mut keys: Vec<&String> = d.keys().collect();
                if self.sorted {
                    keys.sort_by(|a, b| natural_compare(a, b));
                }
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        if self.pretty {
                            self.out.push_str(",\n");
                            self.write_indent();
                        } else {
                            self.out.push(',');
                        }
                    }
                    self.string(k);
                    self.out.push_str(if self.pretty { " : " } else { ":" });
                    self.value(&d[k.as_str()]);
                }
                if self.pretty {
                    self.out.push('\n');
                    self.indent -= 2;
                    self.write_indent();
                }
                self.out.push('}');
            }
            _ => {}
        }
    }
}

/// JSONSerialization.data(withJSONObject:options:): compact, or with `readable` (plutil's -r)
/// [.prettyPrinted, .sortedKeys]. A top level that is no container is written as the fragment it
/// is (Foundation would stop the program).
pub fn write(v: &Value, readable: bool) -> Vec<u8> {
    let mut w = Writer { out: String::new(), indent: 0, pretty: readable, sorted: readable };
    w.value(v);
    w.out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(s: &str) -> String {
        match parse(s.as_bytes(), false) {
            Err(Error(m)) => m,
            Ok(_) => panic!("{s} parsed"),
        }
    }

    #[test]
    fn foundation_json() {
        let v = parse(r#" {"b": [1, -2, 3.5, 1e3, true, null, "x\/yé"], "a": {}, "big": 18446744073709551615,} "#.as_bytes(), false).unwrap();
        let d = v.as_dict().unwrap();
        assert!(matches!(d["big"], Value::UInt(u64::MAX)));
        assert_eq!(String::from_utf8(write(&Value::dict(d.clone()), false)).unwrap(), r#"{"b":[1,-2,3.5,1000,true,null,"x\/yé"],"a":{},"big":18446744073709551615}"#);
        let mut d2 = d.clone();
        d2.shift_remove("b");
        assert_eq!(String::from_utf8(write(&Value::dict(d2), true)).unwrap(), "{\n  \"a\" : {\n\n  },\n  \"big\" : 18446744073709551615\n}");
        assert_eq!(err("[1, 2"), "Unexpected end of file during JSON parse.");
        assert_eq!(err("[1 2]"), "Invalid value around character 3.");
        assert_eq!(err("[01]"), "Number with leading zero around character 2.");
        assert_eq!(err("\"x\""), "JSON text did not start with array or object and option to allow fragments not set.");
        assert_eq!(err("[\"a\\qb\"]"), "Invalid escape sequence around character 3.");
        assert_eq!(err("[\"a\nb\"]"), "Unescaped control character around character 3.");
        assert_eq!(err("{\"a\" 1}"), "Invalid value around character 5.");
        assert_eq!(err("<plist>"), "Invalid value around character 0.");
        assert!(parse(b"\"x\"", true).is_ok());
    }
}
