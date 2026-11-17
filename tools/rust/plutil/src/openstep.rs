//! Old-style (OpenStep, NeXTSTEP) property lists, read as CFOldStylePList.c reads them: strings
//! quoted or not, ( arrays ), { dictionaries; }, <hex data>, // and /* */ comments, and the
//! .strings shortcut ("key" = "value"; with no braces, or "key"; alone). CF tries this only
//! after XML fails, and reports XML's error when both do, so only whether it reads matters to
//! plutil; the messages are CF's all the same.

use crate::value::{Dict, Value};

const MAX_DEPTH: u32 = 512;

/// NeXTSTEP Latin above 0x7F, as Unicode (CF's NSToPrecompUnicodeTable): what \ooo escapes mean.
const NEXTSTEP: [u16; 128] = [
    0x00a0, 0x00c0, 0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb, 0x00cc, 0x00cd, 0x00ce, 0x00cf,
    0x00d0, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x00de, 0x00b5, 0x00d7, 0x00f7,
    0x00a9, 0x00a1, 0x00a2, 0x00a3, 0x2044, 0x00a5, 0x0192, 0x00a7, 0x00a4, 0x2019, 0x201c, 0x00ab, 0x2039, 0x203a, 0xfb01, 0xfb02,
    0x00ae, 0x2013, 0x2020, 0x2021, 0x00b7, 0x00a6, 0x00b6, 0x2022, 0x201a, 0x201e, 0x201d, 0x00bb, 0x2026, 0x2030, 0x00ac, 0x00bf,
    0x00b9, 0x02cb, 0x00b4, 0x02c6, 0x02dc, 0x00af, 0x02d8, 0x02d9, 0x00a8, 0x00b2, 0x02da, 0x00b8, 0x00b3, 0x02dd, 0x02db, 0x02c7,
    0x2014, 0x00b1, 0x00bc, 0x00bd, 0x00be, 0x00e0, 0x00e1, 0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb,
    0x00ec, 0x00c6, 0x00ed, 0x00aa, 0x00ee, 0x00ef, 0x00f0, 0x00f1, 0x0141, 0x00d8, 0x0152, 0x00ba, 0x00f2, 0x00f3, 0x00f4, 0x00f5,
    0x00f6, 0x00e6, 0x00f9, 0x00fa, 0x00fb, 0x0131, 0x00fc, 0x00fd, 0x0142, 0x00f8, 0x0153, 0x00df, 0x00fe, 0x00ff, 0xfffd, 0xfffd,
];

fn unquoted(c: u16) -> bool {
    matches!(c, 0x61..=0x7a | 0x41..=0x5a | 0x30..=0x39) || [b'_', b'$', b'/', b':', b'.', b'-'].iter().any(|&b| c == b as u16)
}

struct Parser {
    s: Vec<u16>,
    cur: usize,
    error: Option<String>,
}

impl Parser {
    fn end(&self) -> usize {
        self.s.len()
    }

    fn line(&self) -> u32 {
        let mut count = 1;
        let mut p = 0;
        while p < self.end() && p < self.cur {
            if self.s[p] == b'\r' as u16 {
                count += 1;
                if p + 1 < self.end() && p + 1 < self.cur && self.s[p + 1] == b'\n' as u16 {
                    p += 1;
                }
            } else if self.s[p] == b'\n' as u16 {
                count += 1;
            }
            p += 1;
        }
        count
    }

    fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    /// advanceToNonSpace: whitespace and comments; false at the end.
    fn advance(&mut self) -> bool {
        while self.cur < self.end() {
            let c = self.s[self.cur];
            self.cur += 1;
            if (9..=0x0d).contains(&c) || c == b' ' as u16 || c == 0x2028 || c == 0x2029 {
                continue;
            }
            if c == b'/' as u16 {
                if self.cur >= self.end() {
                    self.cur -= 1;
                    return true;
                } else if self.s[self.cur] == b'/' as u16 {
                    self.cur += 1;
                    while self.cur < self.end() {
                        let c3 = self.s[self.cur];
                        if c3 == b'\n' as u16 || c3 == b'\r' as u16 || c3 == 0x2028 || c3 == 0x2029 {
                            break;
                        }
                        self.cur += 1;
                    }
                } else if self.s[self.cur] == b'*' as u16 {
                    self.cur += 1;
                    while self.cur < self.end() {
                        let c2 = self.s[self.cur];
                        self.cur += 1;
                        if c2 == b'*' as u16 && self.cur < self.end() && self.s[self.cur] == b'/' as u16 {
                            self.cur += 1;
                            break;
                        }
                    }
                } else {
                    self.cur -= 1;
                    return true;
                }
            } else {
                self.cur -= 1;
                return true;
            }
        }
        false
    }

    /// getSlashedChar: just past the backslash.
    fn slashed(&mut self) -> u16 {
        let c = self.s[self.cur];
        self.cur += 1;
        match c {
            0x30..=0x37 => {
                let mut num: u8 = (c - 0x30) as u8;
                for _ in 0..2 {
                    if self.cur >= self.end() {
                        break;
                    }
                    let d = self.s[self.cur];
                    if (0x30..=0x37).contains(&d) {
                        num = (num << 3).wrapping_add((d - 0x30) as u8);
                        self.cur += 1;
                    } else {
                        break;
                    }
                }
                let u = if num < 0x80 { num as u16 } else { NEXTSTEP[(num - 0x80) as usize] };
                if u == 0xfffd {
                    self.set_error("Unable to convert octet-stream while parsing plist".into());
                    return 0;
                }
                u
            }
            0x55 => {
                // 'U': up to four hex digits; a non-digit still counts as one of the four.
                let (mut num, mut digits) = (0u16, 4);
                while self.cur < self.end() && digits > 0 {
                    digits -= 1;
                    let d = self.s[self.cur];
                    if let Some(v) = char::from_u32(d as u32).and_then(|c| c.to_digit(16)) {
                        self.cur += 1;
                        num = (num << 4).wrapping_add(v as u16);
                    }
                }
                num
            }
            0x61 => 0x07,
            0x62 => 0x08,
            0x66 => 0x0c,
            0x6e => 0x0a,
            0x72 => 0x0d,
            0x74 => 0x09,
            0x76 => 0x0b,
            0x22 => 0x22,
            0x0a => 0x0a,
            other => other,
        }
    }

    fn quoted(&mut self, quote: u16) -> Option<String> {
        let start = self.cur;
        let mut out: Vec<u16> = Vec::new();
        let mut mark = self.cur;
        while self.cur < self.end() {
            let c = self.s[self.cur];
            if c == quote {
                break;
            }
            if c == b'\\' as u16 {
                out.extend_from_slice(&self.s[mark..self.cur]);
                self.cur += 1;
                if self.cur == self.end() {
                    let line = self.line();
                    self.set_error(format!("Unterminated backslash sequence on line {line}"));
                    return None;
                }
                let c = self.slashed();
                out.push(c);
                mark = self.cur;
            } else {
                self.cur += 1;
            }
        }
        if self.end() <= self.cur {
            self.cur = start;
            let line = self.line();
            self.set_error(format!("Unterminated quoted string starting on line {line}"));
            return None;
        }
        out.extend_from_slice(&self.s[mark..self.cur]);
        self.cur += 1;
        self.error = None;
        Some(String::from_utf16_lossy(&out))
    }

    fn unquoted_string(&mut self) -> Option<String> {
        let mark = self.cur;
        while self.cur < self.end() && unquoted(self.s[self.cur]) {
            self.cur += 1;
        }
        if self.cur != mark {
            return Some(String::from_utf16_lossy(&self.s[mark..self.cur]));
        }
        self.set_error("Unexpected EOF".into());
        None
    }

    fn string(&mut self, require: bool) -> Option<String> {
        if !self.advance() {
            if require {
                self.set_error("Unexpected EOF while parsing string".into());
            }
            return None;
        }
        let c = self.s[self.cur];
        if c == b'\'' as u16 || c == b'"' as u16 {
            self.cur += 1;
            self.quoted(c)
        } else if unquoted(c) {
            self.unquoted_string()
        } else {
            if require {
                let line = self.line();
                self.set_error(format!("Invalid string character at line {line}"));
            }
            None
        }
    }

    fn array(&mut self, depth: u32) -> Option<Value> {
        let mut array = Vec::new();
        let mut item = self.object(false, depth + 1);
        while let Some(v) = item {
            array.push(v);
            if !self.advance() {
                let line = self.line();
                self.set_error(format!("Expected ',' for array at line {line}"));
                return None;
            }
            if self.s[self.cur] != b',' as u16 {
                item = None;
            } else {
                self.cur += 1;
                item = self.object(false, depth + 1);
            }
        }
        if !self.advance() || self.s[self.cur] != b')' as u16 {
            let line = self.line();
            self.set_error(format!("Expected terminating ')' for array at line {line}"));
            return None;
        }
        self.error = None;
        self.cur += 1;
        Some(Value::array(array))
    }

    fn dict_content(&mut self, depth: u32) -> Option<Dict> {
        let mut dict = Dict::new();
        let mut key = self.string(false);
        while let Some(k) = key {
            if !self.advance() {
                let line = self.line();
                self.set_error(format!("Missing ';' on line {line}"));
                return None;
            }
            let c = self.s[self.cur];
            let value = if c == b';' as u16 {
                Value::String(k.clone())
            } else if c == b'=' as u16 {
                self.cur += 1;
                self.object(true, depth + 1)?
            } else {
                let line = self.line();
                self.set_error(format!("Expected ';' or '=' after key at line {line}"));
                return None;
            };
            dict.insert(k, value);
            if self.advance() && self.s[self.cur] == b';' as u16 {
                self.cur += 1;
                key = self.string(false);
            } else {
                let line = self.line();
                self.set_error(format!("Missing ';' on line {line}"));
                return None;
            }
        }
        self.error = None;
        Some(dict)
    }

    fn dict(&mut self, depth: u32) -> Option<Value> {
        let dict = self.dict_content(depth)?;
        if !self.advance() || self.s[self.cur] != b'}' as u16 {
            let line = self.line();
            self.set_error(format!("Expected terminating '}}' for dictionary at line {line}"));
            return None;
        }
        self.cur += 1;
        Some(Value::dict(dict))
    }

    fn data(&mut self) -> Option<Value> {
        let mut out = Vec::new();
        let hex = |c: u16| -> Option<u8> {
            let c = (c & 0xff) as u8;
            (c as char).to_digit(16).map(|d| d as u8)
        };
        loop {
            // getDataBytes, without its 400-byte chunks (they change nothing).
            let mut read = 0;
            let mut failure = 0;
            while self.cur < self.end() {
                let c1 = self.s[self.cur];
                if c1 == b'>' as u16 {
                    break;
                }
                if let Some(first) = hex(c1) {
                    self.cur += 1;
                    if self.cur >= self.end() {
                        failure = -2;
                        break;
                    }
                    let Some(second) = hex(self.s[self.cur]) else {
                        failure = -2;
                        break;
                    };
                    out.push((first << 4) + second);
                    read += 1;
                    self.cur += 1;
                } else if c1 == b' ' as u16 || c1 == b'\n' as u16 || c1 == b'\t' as u16 || c1 == b'\r' as u16 || c1 == 0x2028 || c1 == 0x2029 {
                    self.cur += 1;
                } else {
                    failure = -1;
                    break;
                }
            }
            if failure != 0 {
                let line = self.line();
                let why = if failure == -2 { "uneven length" } else { "invalid hex" };
                self.set_error(format!("Malformed data byte group at line {line}; {why}"));
                return None;
            }
            if read == 0 {
                break;
            }
        }
        self.error = None;
        if self.cur < self.end() && self.s[self.cur] == b'>' as u16 {
            self.cur += 1;
            Some(Value::data(out))
        } else {
            let line = self.line();
            self.set_error(format!("Expected terminating '>' for data at line {line}"));
            None
        }
    }

    fn object(&mut self, require: bool, depth: u32) -> Option<Value> {
        if depth > MAX_DEPTH {
            let line = self.line();
            self.set_error(format!("Too many nested arrays or dictionaries at line {line}"));
            return None;
        }
        if !self.advance() {
            if require {
                self.set_error("Unexpected EOF while parsing plist".into());
            }
            return None;
        }
        let c = self.s[self.cur];
        self.cur += 1;
        match c {
            0x7b => self.dict(depth),
            0x28 => self.array(depth),
            0x3c => self.data(),
            0x27 | 0x22 => self.quoted(c).map(Value::String),
            c if unquoted(c) => {
                self.cur -= 1;
                self.unquoted_string().map(Value::String)
            }
            c => {
                self.cur -= 1;
                if require {
                    let line = self.line();
                    self.set_error(format!("Unexpected character '0x{c:x}' at line {line}"));
                }
                None
            }
        }
    }
}

/// __CFCreateOldStylePropertyListOrStringsFile on text: the property list, or the error.
pub fn parse(text: &str) -> Result<Value, String> {
    let s: Vec<u16> = text.encode_utf16().collect();
    if s.is_empty() {
        return Err("Conversion of string failed. The string is empty.".into());
    }
    let mut p = Parser { s, cur: 0, error: None };
    let result = if !p.advance() {
        Some(Value::dict(Dict::new()))
    } else {
        match p.object(true, 0) {
            Some(v) if p.advance() => {
                if !matches!(v, Value::String(_)) {
                    let line = p.line();
                    p.error = Some(format!("Junk after plist at line {line}"));
                    None
                } else {
                    // More after a lone string: a .strings file.
                    p.error = None;
                    p.cur = 0;
                    p.dict_content(0).map(Value::dict)
                }
            }
            other => other,
        }
    };
    result.ok_or_else(|| {
        let line = p.line();
        p.error.unwrap_or_else(|| format!("Unknown error parsing property list around line {line}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_old_style_plists() {
        let v = parse("{ name = \"a b\"; list = (1, two, <0a0B>); /* c */ nested = { x = y; }; // end\n}").unwrap();
        let d = v.as_dict().unwrap();
        assert!(matches!(&d["name"], Value::String(s) if s == "a b"));
        let list = d["list"].as_array().unwrap();
        assert!(matches!(&list[2], Value::Data(b) if b.as_slice() == [0x0a, 0x0b]));
        assert!(parse("  ").unwrap().as_dict().unwrap().is_empty());
        let strings = parse("\"greeting\" = \"hi\\n\\101\\U00e9\";\n\"alone\";").unwrap();
        let d = strings.as_dict().unwrap();
        assert!(matches!(&d["greeting"], Value::String(s) if s == "hi\nAé"));
        assert!(matches!(&d["alone"], Value::String(s) if s == "alone"));
        assert!(matches!(parse("42").unwrap(), Value::String(s) if s == "42"));
        assert_eq!(parse("{\"a\": 1}").unwrap_err(), "Expected ';' or '=' after key at line 1");
        assert_eq!(parse("(1, 2").unwrap_err(), "Expected ',' for array at line 1");
        assert_eq!(parse("(1, 2 x)").unwrap_err(), "Expected terminating ')' for array at line 1");
        assert_eq!(parse("(1) x").unwrap_err(), "Junk after plist at line 1");
    }
}
