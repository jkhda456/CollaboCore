//! XML property lists: CFPropertyList.c's parser (its tag handling, entities, base64, dates,
//! integers and reals, and its error messages, line numbers included) and its writer (the
//! same bytes: tabs, sorted keys, base64 lines that shorten with the indentation).

use crate::date;
use crate::num;
use crate::value::{cfstring_compare, Dict, Value};
use std::rc::Rc;

/// The deepest nesting CF parses (_CFPropertyListMaxRecursionDepth).
pub const MAX_DEPTH: usize = 512;

pub struct Parser<'a> {
    b: &'a [u8],
    cur: usize,
    pub error: Option<String>,
}

const PLIST: &[u8] = b"plist";
const ARRAY: &[u8] = b"array";
const DICT: &[u8] = b"dict";
const KEY: &[u8] = b"key";
const STRING: &[u8] = b"string";
const DATA: &[u8] = b"data";
const DATE: &[u8] = b"date";
const REAL: &[u8] = b"real";
const INTEGER: &[u8] = b"integer";
const TRUE: &[u8] = b"true";
const FALSE: &[u8] = b"false";
const DOCTYPE: &[u8] = b"DOCTYPE";
const CDSECT: &[u8] = b"<![CDATA[";

/// A byte as CF's "%c" shows it.
fn ch(b: u8) -> char {
    b as char
}

impl<'a> Parser<'a> {
    pub fn new(b: &'a [u8]) -> Parser<'a> {
        Parser { b, cur: 0, error: None }
    }

    fn end(&self) -> usize {
        self.b.len()
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message);
    }

    fn fail_if_none(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    /// lineNumber(): 1 + the line breaks (\r, \n, \r\n) before the current position.
    fn line(&self) -> u32 {
        let mut count = 1;
        let mut p = 0;
        while p < self.end() && p < self.cur {
            match self.b[p] {
                b'\r' => {
                    count += 1;
                    if p + 1 < self.end() && p + 1 < self.cur && self.b[p + 1] == b'\n' {
                        p += 1;
                    }
                }
                b'\n' => count += 1,
                _ => {}
            }
            p += 1;
        }
        count
    }

    fn skip_whitespace(&mut self) {
        while self.cur < self.end() && matches!(self.b[self.cur], b' ' | b'\t' | b'\n' | b'\r') {
            self.cur += 1;
        }
    }

    /// Just past "<!--".
    fn skip_comment(&mut self) {
        let mut p = self.cur;
        let end = self.end().saturating_sub(3);
        while p < end {
            if self.b[p] == b'-' && self.b[p + 1] == b'-' && self.b[p + 2] == b'>' {
                self.cur = p + 3;
                return;
            }
            p += 1;
        }
        let line = self.line();
        self.fail(format!("Unterminated comment started on line {line}"));
    }

    /// Just past "<?".
    fn skip_processing_instruction(&mut self) {
        let begin = self.cur;
        let end = self.end().saturating_sub(2);
        while self.cur < end {
            if self.b[self.cur] == b'?' && self.b[self.cur + 1] == b'>' {
                self.cur += 2;
                return;
            }
            self.cur += 1;
        }
        self.cur = begin;
        let line = self.line();
        self.fail(format!("Encountered unexpected EOF while parsing the processing instruction begun on line {line}"));
    }

    /// Just past "<!".
    fn skip_dtd(&mut self) {
        if self.end() - self.cur < DOCTYPE.len() || &self.b[self.cur..self.cur + DOCTYPE.len()] != DOCTYPE {
            let line = self.line();
            self.fail(format!("Malformed DTD on line {line}"));
            return;
        }
        self.cur += DOCTYPE.len();
        self.skip_whitespace();
        while self.cur < self.end() {
            let c = self.b[self.cur];
            if c == b'[' {
                break;
            }
            if c == b'>' {
                self.cur += 1;
                return;
            }
            self.cur += 1;
        }
        if self.cur == self.end() {
            self.fail("Encountered unexpected EOF while parsing DTD".into());
            return;
        }
        self.skip_whitespace();
        if self.cur < self.end() {
            if self.b[self.cur] == b'>' {
                self.cur += 1;
            } else {
                let (c, line) = (ch(self.b[self.cur]), self.line());
                self.fail(format!("Encountered unexpected character {c} on line {line} while parsing DTD"));
            }
        } else {
            self.fail("Encountered unexpected EOF while parsing DTD".into());
        }
    }

    /// getContentObject: the next element's object, or None at a close tag, the end or an error.
    fn content_object(&mut self, depth: usize) -> Option<(Value, bool)> {
        if depth > MAX_DEPTH {
            let line = self.line();
            self.fail(format!("Too many nested arrays or dictionaries, failing on line {line}"));
            return None;
        }
        while self.error.is_none() && self.cur < self.end() {
            self.skip_whitespace();
            if self.cur >= self.end() {
                self.fail("Encountered unexpected EOF".into());
                return None;
            }
            if self.b[self.cur] != b'<' {
                let (c, line) = (ch(self.b[self.cur]), self.line());
                self.fail(format!("Encountered unexpected character {c} on line {line} while looking for open tag"));
                return None;
            }
            self.cur += 1;
            if self.cur >= self.end() {
                self.fail("Encountered unexpected EOF".into());
                return None;
            }
            match self.b[self.cur] {
                b'?' => self.skip_processing_instruction(),
                b'!' => {
                    if self.cur + 2 >= self.end() {
                        self.fail("Encountered unexpected EOF".into());
                        return None;
                    }
                    if self.b[self.cur + 1] == b'-' && self.b[self.cur + 2] == b'-' {
                        self.cur += 3;
                        self.skip_comment();
                    } else {
                        self.fail("Encountered unexpected EOF".into());
                        return None;
                    }
                }
                b'/' => {
                    self.cur -= 1;
                    return None;
                }
                _ => return self.element(depth),
            }
        }
        None
    }

    fn cdata(&mut self, out: &mut Vec<u8>) {
        if self.end() - self.cur < CDSECT.len() {
            self.fail("Encountered unexpected EOF".into());
            return;
        }
        if &self.b[self.cur..self.cur + CDSECT.len()] != CDSECT {
            let line = self.line();
            self.fail(format!("Encountered improper CDATA opening at line {line}"));
            return;
        }
        self.cur += CDSECT.len();
        let begin = self.cur;
        let end = self.end().saturating_sub(2);
        while self.cur < end {
            if self.b[self.cur] == b']' && self.b[self.cur + 1] == b']' && self.b[self.cur + 2] == b'>' {
                out.extend_from_slice(&self.b[begin..self.cur]);
                self.cur += 3;
                return;
            }
            self.cur += 1;
        }
        self.cur = begin;
        let line = self.line();
        self.fail(format!("Could not find end of CDATA started on line {line}"));
    }

    /// &lt; &gt; &amp; &apos; &quot; &#ddd; &#xhhh;
    fn entity(&mut self, out: &mut Vec<u8>) {
        self.cur += 1;
        let len = self.end() as isize - self.cur as isize;
        if len < 1 {
            self.fail("Encountered unexpected EOF".into());
            return;
        }
        let len = len as usize;
        let at = |p: &Self, k: usize| p.b.get(p.cur + k).copied().unwrap_or(0);
        let unknown = |p: &mut Self| {
            let line = p.line();
            p.fail(format!("Encountered unknown ampersand-escape sequence at line {line}"));
        };
        let c = match self.b[self.cur] {
            b'l' | b'g' => {
                if len >= 3 && at(self, 1) == b't' && at(self, 2) == b';' {
                    let c = if self.b[self.cur] == b'l' { b'<' } else { b'>' };
                    self.cur += 3;
                    c
                } else {
                    return unknown(self);
                }
            }
            b'a' => {
                if len < 4 {
                    self.fail("Encountered unexpected EOF".into());
                    return;
                }
                if at(self, 1) == b'm' && at(self, 2) == b'p' && at(self, 3) == b';' {
                    self.cur += 4;
                    b'&'
                } else if at(self, 1) == b'p' && len > 4 && at(self, 2) == b'o' && at(self, 3) == b's' && at(self, 4) == b';' {
                    self.cur += 5;
                    b'\''
                } else {
                    return unknown(self);
                }
            }
            b'q' => {
                if len >= 5 && at(self, 1) == b'u' && at(self, 2) == b'o' && at(self, 3) == b't' && at(self, 4) == b';' {
                    self.cur += 5;
                    b'"'
                } else {
                    return unknown(self);
                }
            }
            b'#' => {
                if len < 4 {
                    self.fail("Encountered unexpected EOF".into());
                    return;
                }
                self.cur += 1;
                let mut hex = false;
                if self.b[self.cur] == b'x' {
                    hex = true;
                    self.cur += 1;
                }
                let mut num: u32 = 0;
                let mut count = 0;
                while self.cur < self.end() {
                    let c = self.b[self.cur];
                    self.cur += 1;
                    count += 1;
                    if c == b';' {
                        match char::from_u32(num) {
                            Some(c) => {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            }
                            None => {
                                let line = self.line();
                                self.fail(format!("Encountered unparseable Unicode sequence at line {line} while parsing data (input did not result in a real string)"));
                            }
                        }
                        return;
                    } else if count > 8 {
                        let line = self.line();
                        self.fail(format!("Encountered unparseable unicode sequence at line {line} while parsing data (too large of a value for a Unicode sequence)"));
                        return;
                    }
                    num = if hex { num << 4 } else { num.wrapping_mul(10) };
                    let digit = match c {
                        b'0'..=b'9' => (c - b'0') as u32,
                        b'a'..=b'f' if hex => (c - b'a' + 10) as u32,
                        b'A'..=b'F' if hex => (c - b'A' + 10) as u32,
                        _ => {
                            let line = self.line();
                            self.fail(format!("Encountered unexpected character {} at line {line} while parsing data", ch(c)));
                            return;
                        }
                    };
                    num = num.wrapping_add(digit);
                }
                self.fail("Encountered unexpected EOF".into());
                return;
            }
            _ => return unknown(self),
        };
        out.push(c);
    }

    /// parseStringTag: characters, CDATA sections and entities up to the next tag.
    fn string(&mut self) -> Option<String> {
        let mut mark = self.cur;
        let mut data: Option<Vec<u8>> = None;
        while self.error.is_none() && self.cur < self.end() {
            let c = self.b[self.cur];
            if c == b'<' {
                if self.cur + 1 >= self.end() {
                    break;
                }
                if self.b[self.cur + 1] != b'!' {
                    break;
                }
                let d = data.get_or_insert_with(Vec::new);
                d.extend_from_slice(&self.b[mark..self.cur]);
                let mut d = data.take().unwrap();
                self.cdata(&mut d);
                data = Some(d);
                mark = self.cur;
            } else if c == b'&' {
                let mut d = data.take().unwrap_or_default();
                d.extend_from_slice(&self.b[mark..self.cur]);
                self.entity(&mut d);
                data = Some(d);
                mark = self.cur;
            } else {
                self.cur += 1;
            }
        }
        if self.error.is_some() {
            return None;
        }
        let bytes = match data {
            None => self.b[mark..self.cur].to_vec(),
            Some(mut d) => {
                d.extend_from_slice(&self.b[mark..self.cur]);
                d
            }
        };
        match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(_) => {
                self.fail("Unable to convert string to correct encoding".into());
                None
            }
        }
    }

    fn close_tag(&mut self, tag: &[u8]) -> bool {
        if self.end() - self.cur < tag.len() + 3 {
            self.fail_if_none("Encountered unexpected EOF".into());
            return false;
        }
        if self.b[self.cur] != b'<' || {
            self.cur += 1;
            self.b[self.cur] != b'/'
        } {
            let (c, line) = (ch(self.b[self.cur]), self.line());
            self.fail_if_none(format!("Encountered unexpected character {c} on line {line} while looking for close tag"));
            return false;
        }
        self.cur += 1;
        if &self.b[self.cur..self.cur + tag.len()] != tag {
            let line = self.line();
            self.fail_if_none(format!("Close tag on line {line} does not match open tag {}", String::from_utf8_lossy(tag)));
            return false;
        }
        self.cur += tag.len();
        self.skip_whitespace();
        if self.cur == self.end() {
            self.fail_if_none("Encountered unexpected EOF".into());
            return false;
        }
        if self.b[self.cur] != b'>' {
            let (c, line) = (ch(self.b[self.cur]), self.line());
            self.fail_if_none(format!("Encountered unexpected character {c} on line {line} while looking for close tag"));
            return false;
        }
        self.cur += 1;
        true
    }

    fn plist_tag(&mut self, depth: usize) -> Option<(Value, bool)> {
        let Some((result, _)) = self.content_object(depth) else {
            self.fail_if_none("Encountered empty plist tag".into());
            return None;
        };
        let save = self.cur;
        if self.content_object(depth).is_some() {
            self.cur = save;
            let line = self.line();
            self.fail(format!("Encountered unexpected element at line {line} (plist can only include one object)"));
            return None;
        }
        if self.error.is_some() {
            return None;
        }
        if !self.close_tag(PLIST) {
            return None;
        }
        Some((result, false))
    }

    fn array_tag(&mut self, depth: usize) -> Option<(Value, bool)> {
        let mut array = Vec::new();
        while let Some((v, _)) = self.content_object(depth) {
            array.push(v);
        }
        if self.error.is_some() || !self.close_tag(ARRAY) {
            return None;
        }
        Some((Value::array(array), false))
    }

    fn dict_tag(&mut self, depth: usize) -> Option<(Value, bool)> {
        let mut dict = Dict::new();
        while let Some((key, is_key)) = self.content_object(depth) {
            if !is_key {
                let line = self.line();
                self.fail_if_none(format!("Found non-key inside <dict> at line {line}"));
                return None;
            }
            let Some((value, _)) = self.content_object(depth) else {
                let line = self.line();
                self.fail_if_none(format!("Value missing for key inside <dict> at line {line}"));
                return None;
            };
            let Value::String(key) = key else { unreachable!() };
            dict.insert(key, value);
        }
        if self.error.is_some() || !self.close_tag(DICT) {
            return None;
        }
        // A dictionary of CF$UID alone, to an integer, is a keyed archiver's UID.
        if dict.len() == 1 {
            if let Some(v) = dict.get("CF$UID") {
                if let Some(n) = match v {
                    Value::Int(i) => Some(*i as u32 as u64),
                    Value::UInt(u) => Some(*u as u32 as u64),
                    Value::Real(r) => Some(*r as i64 as u32 as u64),
                    Value::Float(f) => Some(*f as i64 as u32 as u64),
                    _ => None,
                } {
                    return Some((Value::Uid(n), false));
                }
            }
        }
        Some((Value::dict(dict), false))
    }

    fn data_tag(&mut self) -> Option<(Value, bool)> {
        let base = self.cur;
        let mut out = Vec::new();
        let (mut numeq, mut acc, mut cntr) = (0, 0u32, 0u32);
        while self.cur < self.end() {
            let c = self.b[self.cur];
            if c == b'<' {
                break;
            }
            if c == b'=' {
                numeq += 1;
            } else if !(c as char).is_ascii_whitespace() && c != 0x0b {
                numeq = 0;
            }
            if c >= 128 {
                let line = self.line();
                self.fail(format!("Could not interpret <data> on line {line} (invalid character 0x{c:X})"));
                return None;
            }
            let Some(d) = base64_digit(c) else {
                self.cur += 1;
                continue;
            };
            cntr += 1;
            acc = (acc << 6).wrapping_add(d);
            if cntr & 3 == 0 {
                out.push((acc >> 16) as u8);
                if numeq < 2 {
                    out.push((acc >> 8) as u8);
                }
                if numeq < 1 {
                    out.push(acc as u8);
                }
            }
            self.cur += 1;
        }
        let _ = base;
        if !self.close_tag(DATA) {
            return None;
        }
        Some((Value::data(out), false))
    }

    fn two_digits(&mut self) -> Option<i64> {
        if self.cur + 2 >= self.end() {
            return None;
        }
        let (a, b) = (self.b[self.cur], self.b[self.cur + 1]);
        self.cur += 2;
        if !a.is_ascii_digit() || !b.is_ascii_digit() {
            return None;
        }
        Some(((a - b'0') * 10 + (b - b'0')) as i64)
    }

    fn date_tag(&mut self) -> Option<(Value, bool)> {
        let (mut year, mut negative) = (0i64, false);
        if self.cur < self.end() && self.b[self.cur] == b'-' {
            negative = true;
            self.cur += 1;
        }
        while self.cur < self.end() && self.b[self.cur].is_ascii_digit() {
            year = (10 * year + (self.b[self.cur] - b'0') as i64) as i32 as i64;
            self.cur += 1;
        }
        let mut bad = false;
        let expect = |p: &mut Self, c: u8, bad: &mut bool| {
            if !*bad && p.cur < p.end() && p.b[p.cur] == c {
                p.cur += 1;
            } else {
                *bad = true;
            }
        };
        expect(self, b'-', &mut bad);
        let mut fields = [0i64; 5];
        for (i, sep) in [b'-', b'T', b':', b':', b'Z'].into_iter().enumerate() {
            if bad {
                break;
            }
            match self.two_digits() {
                Some(n) => fields[i] = n,
                None => {
                    bad = true;
                    break;
                }
            }
            expect(self, sep, &mut bad);
        }
        if bad || !self.close_tag(DATE) {
            let line = self.line();
            self.fail(format!("Could not interpret <date> at line {line}"));
            return None;
        }
        let year = if negative { -year } else { year };
        let [month, day, hour, minute, second] = fields;
        Some((Value::Date(date::absolute_time(year, month, day, hour, minute, second as f64)), false))
    }

    fn real_tag(&mut self) -> Option<(Value, bool)> {
        let Some(s) = self.string() else {
            let line = self.line();
            self.fail_if_none(format!("Encountered empty <real> on line {line}"));
            return None;
        };
        let special = match s.to_ascii_lowercase().as_str() {
            "nan" => Some(f64::NAN),
            "+infinity" | "infinity" | "inf" | "+inf" => Some(f64::INFINITY),
            "-infinity" | "-inf" => Some(f64::NEG_INFINITY),
            _ => None,
        };
        let value = match special {
            Some(v) => v,
            None => match scan_double(&s) {
                Some(v) => v,
                None => {
                    let line = self.line();
                    self.fail(format!("Encountered misformatted real on line {line}"));
                    return None;
                }
            },
        };
        if !self.close_tag(REAL) {
            return None;
        }
        Some((Value::Real(value), false))
    }

    fn integer_tag(&mut self) -> Option<(Value, bool)> {
        let premature = |p: &mut Self| {
            let line = p.line();
            p.fail(format!("Premature end of file after <integer> on line {line}"));
        };
        while self.cur < self.end() && integer_whitespace(&self.b[self.cur..]) {
            self.cur += 1;
        }
        if self.cur == self.end() {
            premature(self);
            return None;
        }
        let mut c = self.b[self.cur];
        if c == b'<' {
            let line = self.line();
            self.fail(format!("Encountered empty <integer> on line {line}"));
            return None;
        }
        let mut negative = false;
        if c == b'-' || c == b'+' {
            negative = c == b'-';
            self.cur += 1;
            while self.cur < self.end() && integer_whitespace(&self.b[self.cur..]) {
                self.cur += 1;
            }
        }
        if self.cur == self.end() {
            premature(self);
            return None;
        }
        c = self.b[self.cur];
        let (mut hex, mut leading_zero) = (false, false);
        if c == b'0' {
            if self.cur + 1 < self.end() && (self.b[self.cur + 1] == b'x' || self.b[self.cur + 1] == b'X') {
                self.cur += 1;
                hex = true;
            } else {
                leading_zero = true;
            }
            self.cur += 1;
        }
        if self.cur == self.end() {
            premature(self);
            return None;
        }
        c = self.b[self.cur];
        while c == b'0' {
            leading_zero = true;
            self.cur += 1;
            if self.cur == self.end() {
                premature(self);
                return None;
            }
            c = self.b[self.cur];
        }
        if c == b'<' && leading_zero {
            if !self.close_tag(INTEGER) {
                return None;
            }
            return Some((Value::Int(0), false));
        }
        if c == b'<' {
            let line = self.line();
            self.fail(format!("Incomplete <integer> on line {line}"));
            return None;
        }
        let mut value: u64 = 0;
        let multiplier: u64 = if hex { 16 } else { 10 };
        while c != b'<' {
            let digit = match c {
                b'0'..=b'9' => (c - b'0') as u64,
                b'a'..=b'f' => (c - b'a' + 10) as u64,
                b'A'..=b'F' => (c - b'A' + 10) as u64,
                _ => {
                    let line = self.line();
                    self.fail(format!("Unknown character '{}' (0x{:x}) in <integer> on line {line}", ch(c), c as i8 as i32 as u32));
                    return None;
                }
            };
            if !hex && digit > 9 {
                let line = self.line();
                self.fail(format!("Hex digit in non-hex <integer> on line {line}"));
                return None;
            }
            if u64::MAX / multiplier < value {
                let line = self.line();
                self.fail(format!("Integer overflow in <integer> on line {line}"));
                return None;
            }
            value *= multiplier;
            if u64::MAX - digit < value {
                let line = self.line();
                self.fail(format!("Integer overflow in <integer> on line {line}"));
                return None;
            }
            value += digit;
            if negative && (i64::MAX as u64) + 1 < value {
                let line = self.line();
                self.fail(format!("Integer underflow in <integer> on line {line}"));
                return None;
            }
            self.cur += 1;
            if self.cur == self.end() {
                premature(self);
                return None;
            }
            c = self.b[self.cur];
        }
        if !self.close_tag(INTEGER) {
            return None;
        }
        let v = if negative || value <= i64::MAX as u64 {
            let v = value as i64;
            Value::Int(if negative { v.wrapping_neg() } else { v })
        } else {
            Value::UInt(value)
        };
        Some((v, false))
    }

    /// parseXMLElement: just past the '<'.
    fn element(&mut self, depth: usize) -> Option<(Value, bool)> {
        let marker = self.cur;
        let mut marker_len: isize = -1;
        while self.cur < self.end() {
            let c = self.b[self.cur];
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                if marker_len == -1 {
                    marker_len = (self.cur - marker) as isize;
                }
            } else if c == b'>' {
                break;
            }
            self.cur += 1;
        }
        if self.cur >= self.end() {
            return None;
        }
        let empty = self.b[self.cur - 1] == b'/';
        if marker_len == -1 {
            marker_len = (self.cur - empty as usize - marker) as isize;
        }
        self.cur += 1;
        if marker_len == 0 {
            self.cur = marker;
            let line = self.line();
            self.fail(format!("Malformed tag on line {line}"));
            return None;
        }
        let name = &self.b[marker..marker + marker_len as usize];
        let empty_error = |p: &mut Self, what: &str| {
            let line = p.line();
            p.fail(format!("Encountered empty <{what}> on line {line}"));
            None
        };
        match name {
            PLIST => {
                if empty {
                    self.fail("Encountered empty plist tag".into());
                    return None;
                }
                self.plist_tag(depth)
            }
            ARRAY if empty => Some((Value::array(Vec::new()), false)),
            ARRAY => self.array_tag(depth + 1),
            DICT if empty => Some((Value::dict(Dict::new()), false)),
            DICT => self.dict_tag(depth + 1),
            KEY | STRING => {
                let is_key = name == KEY;
                if empty {
                    return Some((Value::String(String::new()), is_key));
                }
                let s = self.string()?;
                if !self.close_tag(name) {
                    return None;
                }
                Some((Value::String(s), is_key))
            }
            DATA if empty => empty_error(self, "data"),
            DATA => self.data_tag(),
            DATE if empty => empty_error(self, "date"),
            DATE => self.date_tag(),
            TRUE | FALSE => {
                let value = name == TRUE;
                if !empty && !self.close_tag(name) {
                    let line = self.line();
                    self.fail(format!("Encountered non-empty <{}> on line {line}", if value { "true" } else { "false" }));
                    return None;
                }
                Some((Value::Bool(value), false))
            }
            REAL if empty => empty_error(self, "real"),
            REAL => self.real_tag(),
            INTEGER if empty => empty_error(self, "integer"),
            INTEGER => self.integer_tag(),
            _ => {
                let s = String::from_utf8_lossy(name).into_owned();
                self.cur = marker;
                let line = self.line();
                self.fail(format!("Encountered unknown tag {s} on line {line}"));
                None
            }
        }
    }

    /// parseXMLPropertyList: the prolog (whitespace, <?…?>, <!DOCTYPE>, comments), then the
    /// first element. What follows it is not looked at.
    pub fn parse(&mut self) -> Option<Value> {
        while self.error.is_none() && self.cur < self.end() {
            self.skip_whitespace();
            if self.cur + 1 >= self.end() {
                self.fail("No XML content found".into());
                return None;
            }
            if self.b[self.cur] != b'<' {
                let (c, line) = (ch(self.b[self.cur]), self.line());
                self.fail(format!("Unexpected character {c} at line {line}"));
                return None;
            }
            self.cur += 1;
            let c = self.b[self.cur];
            if c == b'!' {
                self.cur += 1;
                if self.cur + 1 < self.end() && self.b[self.cur] == b'-' && self.b[self.cur + 1] == b'-' {
                    self.cur += 3;
                    self.skip_comment();
                } else {
                    self.skip_dtd();
                }
            } else if c == b'?' {
                self.cur += 1;
                self.skip_processing_instruction();
            } else {
                return self.element(0).map(|(v, _)| v);
            }
        }
        if self.error.is_none() {
            self.fail("Encountered unexpected EOF".into());
        }
        None
    }
}

/// The XML parser's base64 table ('=' counts as a digit of value 0).
fn base64_digit(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        b'=' => Some(0),
        _ => None,
    }
}

/// The <integer> parser's whitespace: CFString's, as UTF-8, and then some (all of 0x00–0x20
/// and 0x7F–0xA0, as CF has it for compatibility; the 0x80 range check is CF's own).
fn integer_whitespace(s: &[u8]) -> bool {
    let b1 = s[0];
    if b1 < 0x21 || (b1 > 0x7E && b1 < 0xA1) {
        return true;
    }
    if (b1 == 0xe2 || b1 == 0xe3) && s.len() >= 3 {
        let (b2, b3) = (s[1], s[2]);
        if b1 == 0xe2 && b2 == 0x80 {
            return (80..=0x8b).contains(&b3) || b3 == 0xaf;
        } else if b1 == 0xe2 && b2 == 0x81 {
            return b3 == 0x9f;
        } else if b1 == 0xe3 && b2 == 0x80 && b3 == 0x80 {
            return true;
        }
    }
    false
}

/// CFString's whitespace (what __CFStringScanDouble skips first).
fn cf_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
        || ('\u{2000}'..='\u{200b}').contains(&c)
}

/// __CFStringScanDouble over the whole string: leading whitespace, then [+-.0-9Ee]* read by
/// strtod; None unless that is the entire string.
pub fn scan_double(s: &str) -> Option<f64> {
    let t = s.trim_start_matches(cf_whitespace);
    let run: String = t.chars().take_while(|c| matches!(c, '+' | '-' | '.' | '0'..='9' | 'e' | 'E')).collect();
    let (value, used) = strtod_prefix(&run)?;
    (used == t.len()).then_some(value)
}

/// strtod's longest decimal prefix of `s`: (value, bytes used), None when there is none.
pub fn strtod_prefix(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
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
    if !digits {
        return None;
    }
    let mut end = i;
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
    let text = &s[..end];
    let text = if text.ends_with('.') { &text[..text.len() - 1] } else { text };
    let text = if let Some(rest) = text.strip_prefix('.') { format!("0.{rest}") } else { text.replace("-.", "-0.").replace("+.", "+0.") };
    text.parse::<f64>().ok().map(|v| (v, end))
}

// ---- writing ---------------------------------------------------------------------------------

fn indent(out: &mut Vec<u8>, n: usize) {
    out.extend(std::iter::repeat_n(b'\t', n));
}

fn escaped(out: &mut Vec<u8>, s: &str) {
    for c in s.bytes() {
        match c {
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            c => out.push(c),
        }
    }
}

/// _XMLPlistAppendDataUsingBase64: lines of 76 characters less 8 per tab of indentation.
fn base64_lines(out: &mut Vec<u8>, data: &[u8], indentation: usize) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let ind = indentation.min(8);
    let limit = 76 - 8 * ind;
    let mut buf: Vec<u8> = Vec::with_capacity(82);
    let mut i = 0;
    while i < data.len() {
        let p = data[i];
        match i % 3 {
            0 => buf.push(TABLE[((p >> 2) & 0x3f) as usize]),
            1 => buf.push(TABLE[((((data[i - 1] as u32) << 8 | p as u32) >> 4) & 0x3f) as usize]),
            _ => {
                buf.push(TABLE[((((data[i - 1] as u32) << 8 | p as u32) >> 6) & 0x3f) as usize]);
                buf.push(TABLE[(p & 0x3f) as usize]);
            }
        }
        if buf.len() >= limit {
            indent(out, ind);
            out.extend_from_slice(&buf);
            out.push(b'\n');
            buf.clear();
        }
        i += 1;
    }
    match data.len() % 3 {
        1 => {
            buf.push(TABLE[((data[data.len() - 1] << 4) & 0x30) as usize]);
            buf.extend_from_slice(b"==");
        }
        2 => {
            buf.push(TABLE[((data[data.len() - 1] << 2) & 0x3c) as usize]);
            buf.push(b'=');
        }
        _ => {}
    }
    if !buf.is_empty() {
        indent(out, ind);
        out.extend_from_slice(&buf);
        out.push(b'\n');
    }
}

fn tag(out: &mut Vec<u8>, name: &str, content: &str) {
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    out.push(b'>');
    out.extend_from_slice(content.as_bytes());
    out.extend_from_slice(b"</");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b">\n");
}

/// _CFAppendXML0.
fn append(v: &Value, level: usize, out: &mut Vec<u8>) {
    indent(out, level);
    match v {
        Value::String(s) => {
            out.extend_from_slice(b"<string>");
            escaped(out, s);
            out.extend_from_slice(b"</string>\n");
        }
        Value::Uid(u) => {
            out.extend_from_slice(b"<dict>\n");
            indent(out, level + 1);
            out.extend_from_slice(b"<key>CF$UID</key>\n");
            indent(out, level + 1);
            tag(out, "integer", &(*u as i64).to_string());
            indent(out, level);
            out.extend_from_slice(b"</dict>\n");
        }
        Value::Array(a) if a.is_empty() => out.extend_from_slice(b"<array/>\n"),
        Value::Array(a) => {
            out.extend_from_slice(b"<array>\n");
            for item in a.iter() {
                append(item, level + 1, out);
            }
            indent(out, level);
            out.extend_from_slice(b"</array>\n");
        }
        Value::Dict(d) if d.is_empty() => out.extend_from_slice(b"<dict/>\n"),
        Value::Dict(d) => {
            out.extend_from_slice(b"<dict>\n");
            let mut keys: Vec<&String> = d.keys().collect();
            keys.sort_by(|a, b| cfstring_compare(a, b));
            for key in keys {
                indent(out, level + 1);
                out.extend_from_slice(b"<key>");
                escaped(out, key);
                out.extend_from_slice(b"</key>\n");
                append(&d[key.as_str()], level + 1, out);
            }
            indent(out, level);
            out.extend_from_slice(b"</dict>\n");
        }
        Value::Data(bytes) => {
            out.extend_from_slice(b"<data>\n");
            base64_lines(out, bytes, level);
            indent(out, level);
            out.extend_from_slice(b"</data>\n");
        }
        Value::Date(at) => {
            out.extend_from_slice(b"<date>");
            out.extend_from_slice(date::xml(*at).as_bytes());
            out.extend_from_slice(b"</date>\n");
        }
        Value::Real(r) => tag(out, "real", &num::xml_real(*r)),
        Value::Float(f) => tag(out, "real", &num::xml_real(*f as f64)),
        Value::Int(i) => tag(out, "integer", &i.to_string()),
        Value::UInt(u) => tag(out, "integer", &u.to_string()),
        Value::Bool(true) => out.extend_from_slice(b"<true/>\n"),
        Value::Bool(false) => out.extend_from_slice(b"<false/>\n"),
        Value::Null | Value::Set(_) | Value::KeyedDict(_) => {}
    }
}

/// An XML property list, as CFPropertyListCreateData writes kCFPropertyListXMLFormat_v1_0.
pub fn write(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n",
    );
    append(v, 0, &mut out);
    out.extend_from_slice(b"</plist>\n");
    out
}

/// Shared collections met more than this many times make XML refuse (CF's
/// _CFPropertyListMaxRecursionWidth): written out, they would grow without bound.
pub const MAX_WIDTH: usize = MAX_DEPTH * 3;

/// CFPropertyListIsValid for XML (and, with `limit_width` false, binary): property list types
/// only, string keys, no collection inside itself, and for XML no collection met more than
/// MAX_WIDTH times.
pub fn is_valid(v: &Value, limit_width: bool) -> bool {
    fn walk(v: &Value, limit: bool, counts: &mut std::collections::HashMap<usize, usize>) -> bool {
        let (ptr, children): (usize, Vec<&Value>) = match v {
            Value::Array(a) => (Rc::as_ptr(a) as usize, a.iter().collect()),
            Value::Dict(d) => (Rc::as_ptr(d) as usize, d.values().collect()),
            Value::Null | Value::Set(_) | Value::KeyedDict(_) => return false,
            _ => return true,
        };
        if limit {
            let count = counts.entry(ptr).or_insert(0);
            if *count > MAX_WIDTH {
                return false;
            }
            if !children.is_empty() {
                *count += 1;
            }
        }
        children.into_iter().all(|c| walk(c, limit, counts))
    }
    walk(v, limit_width, &mut std::collections::HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Value, String> {
        let mut p = Parser::new(s.as_bytes());
        p.parse().ok_or_else(|| p.error.unwrap_or_default())
    }

    #[test]
    fn reads_and_writes_cf_xml() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>a &amp; b</key>\n\t<array>\n\t\t<integer>-12</integer>\n\t\t<real>2.5</real>\n\t\t<true/>\n\t\t<date>2024-02-29T12:00:00Z</date>\n\t\t<data>\n\t\tAAECAw==\n\t\t</data>\n\t</array>\n\t<key>z</key>\n\t<string>x&lt;y</string>\n</dict>\n</plist>\n";
        let v = parse(text).unwrap();
        assert_eq!(String::from_utf8(write(&v)).unwrap(), text);
    }

    #[test]
    fn cf_error_messages() {
        assert_eq!(parse("{\"a\":1}").unwrap_err(), "Unexpected character { at line 1");
        assert_eq!(parse("<plist><dict><string>x</string></dict></plist>").unwrap_err(), "Found non-key inside <dict> at line 1");
        assert_eq!(parse("<plist>\n<integer>12a</integer></plist>").unwrap_err(), "Hex digit in non-hex <integer> on line 2");
        assert_eq!(parse("<plist><foo/></plist>").unwrap_err(), "Encountered unknown tag foo on line 1");
        assert_eq!(parse("<plist><array><string>x</array></plist>").unwrap_err(), "Close tag on line 1 does not match open tag string");
        assert_eq!(parse("<plist><real>1.5x</real></plist>").unwrap_err(), "Encountered misformatted real on line 1");
        assert_eq!(parse("<plist><string>a</string><string>b</string></plist>").unwrap_err(), "Encountered unexpected element at line 1 (plist can only include one object)");
        assert_eq!(parse("  ").unwrap_err(), "No XML content found");
    }

    #[test]
    fn base64_lines_shorten_with_depth() {
        let data: Vec<u8> = (0..100).collect();
        let mut out = Vec::new();
        base64_lines(&mut out, &data, 2);
        let text = String::from_utf8(out).unwrap();
        assert!(text.lines().all(|l| l.len() <= 2 + 60));
        assert_eq!(text.lines().next().unwrap().len(), 2 + 60);
    }
}
