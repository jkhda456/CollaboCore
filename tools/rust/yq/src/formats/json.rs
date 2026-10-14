//! yq's JSON decoder (goccy/go-json into CandidateNodes) and encoder (CandidateNode.MarshalJSON
//! through an indenting json.Encoder).

use super::{Decoder, Encoder};
use crate::eval::{is_truthy, Res};
use crate::node::*;

pub struct JsonDecoder {
    input: Vec<u8>,
    pos: usize,
}

impl JsonDecoder {
    pub fn new() -> JsonDecoder {
        JsonDecoder { input: Vec::new(), pos: 0 }
    }
}

/// encoding/json's quoteChar
fn quote_char(c: char) -> String {
    if c == '\'' {
        return "'\\''".into();
    }
    if c == '"' {
        return "'\"'".into();
    }
    let q = crate::util::go_quote(&c.to_string());
    format!("'{}'", &q[1..q.len() - 1])
}

enum JErr {
    Syntax(String),
    Eof,
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn ch(&self) -> char {
        let s = &self.b[self.i..];
        std::str::from_utf8(&s[..s.len().min(4)])
            .ok()
            .and_then(|t| t.chars().next())
            .or_else(|| String::from_utf8_lossy(&s[..s.len().min(4)]).chars().next())
            .unwrap_or('\u{fffd}')
    }
    fn err(&self, ctx: &str) -> JErr {
        if self.i >= self.b.len() {
            return JErr::Eof;
        }
        JErr::Syntax(format!("invalid character {} {}", quote_char(self.ch()), ctx))
    }

    fn value(&mut self, e: &mut Engine, node: NodeId) -> Result<(), JErr> {
        self.ws();
        if self.i >= self.b.len() {
            return Err(JErr::Eof);
        }
        match self.b[self.i] {
            b'{' => {
                self.i += 1;
                {
                    let n = e.nm(node);
                    n.kind = MAP;
                    n.tag = "!!map".into();
                }
                self.ws();
                if self.i < self.b.len() && self.b[self.i] == b'}' {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if self.i >= self.b.len() {
                        return Err(JErr::Eof);
                    }
                    if self.b[self.i] != b'"' {
                        return Err(self.err("looking for beginning of object key string"));
                    }
                    let key = self.string()?;
                    let k = e.create_child(node);
                    {
                        let kn = e.nm(k);
                        kn.is_map_key = true;
                        kn.value = key;
                        kn.kind = SCALAR;
                        kn.tag = "!!str".into();
                    }
                    self.ws();
                    if self.i >= self.b.len() {
                        return Err(JErr::Eof);
                    }
                    if self.b[self.i] != b':' {
                        return Err(self.err("after object key"));
                    }
                    self.i += 1;
                    let v = e.create_child(node);
                    e.nm(v).key = Some(k);
                    self.value(e, v)?;
                    e.nm(node).content.push(k);
                    e.nm(node).content.push(v);
                    self.ws();
                    if self.i >= self.b.len() {
                        return Err(JErr::Eof);
                    }
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(self.err("after object key:value pair")),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                {
                    let n = e.nm(node);
                    n.kind = SEQ;
                    n.tag = "!!seq".into();
                }
                self.ws();
                if self.i < self.b.len() && self.b[self.i] == b']' {
                    self.i += 1;
                    return Ok(());
                }
                let mut idx = 0;
                loop {
                    let child = e.add(CNode { filename: "".into(), ..Default::default() });
                    self.value(e, child)?;
                    let k = e.create_child(node);
                    {
                        let kn = e.nm(k);
                        kn.kind = SCALAR;
                        kn.tag = "!!int".into();
                        kn.value = idx.to_string();
                        kn.is_map_key = true;
                    }
                    e.nm(child).parent = Some(node);
                    e.nm(child).key = Some(k);
                    e.nm(node).content.push(child);
                    idx += 1;
                    self.ws();
                    if self.i >= self.b.len() {
                        return Err(JErr::Eof);
                    }
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(self.err("after array element")),
                    }
                }
            }
            b'"' => {
                let s = self.string()?;
                let n = e.nm(node);
                n.kind = SCALAR;
                n.tag = "!!str".into();
                n.value = s;
                Ok(())
            }
            b't' => self.literal(e, node, "true", "!!bool"),
            b'f' => self.literal(e, node, "false", "!!bool"),
            b'n' => self.literal(e, node, "null", "!!null"),
            b'-' | b'0'..=b'9' => self.number(e, node),
            _ => Err(self.err("looking for beginning of value")),
        }
    }

    fn literal(&mut self, e: &mut Engine, node: NodeId, word: &str, tag: &str) -> Result<(), JErr> {
        let w = word.as_bytes();
        for (k, c) in w.iter().enumerate() {
            if self.i + k >= self.b.len() {
                self.i += k;
                return Err(JErr::Eof);
            }
            if self.b[self.i + k] != *c {
                self.i += k;
                return Err(self.err(&format!("in literal {word} (expecting {})", quote_char(*c as char))));
            }
        }
        self.i += w.len();
        let n = e.nm(node);
        n.kind = SCALAR;
        n.tag = tag.into();
        n.value = word.into();
        Ok(())
    }

    fn number(&mut self, e: &mut Engine, node: NodeId) -> Result<(), JErr> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
            if self.i >= self.b.len() {
                return Err(JErr::Eof);
            }
            if !self.b[self.i].is_ascii_digit() {
                return Err(self.err("in numeric literal"));
            }
        }
        if self.b[self.i] == b'0' {
            self.i += 1;
        } else {
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            self.i += 1;
            if self.i >= self.b.len() {
                return Err(JErr::Eof);
            }
            if !self.b[self.i].is_ascii_digit() {
                return Err(self.err("after decimal point in numeric literal"));
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if self.i < self.b.len() && (self.b[self.i] == b'e' || self.b[self.i] == b'E') {
            self.i += 1;
            if self.i < self.b.len() && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
                self.i += 1;
            }
            if self.i >= self.b.len() {
                return Err(JErr::Eof);
            }
            if !self.b[self.i].is_ascii_digit() {
                return Err(self.err("in exponent of numeric literal"));
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap();
        let f: f64 = match text.parse::<f64>() {
            Ok(f) if f.is_finite() => f,
            _ => return Err(JErr::Syntax(format!("json: cannot unmarshal number {text} into Go value of type float64"))),
        };
        // Go's float64 -> int64 conversion as on amd64: out of range is MinInt64
        let as_int: i64 = if f.is_nan() || f >= 9.223372036854775808e18 || f < -9.223372036854775808e18 { i64::MIN } else { f as i64 };
        let n = e.nm(node);
        n.kind = SCALAR;
        if f == as_int as f64 {
            n.tag = "!!int".into();
            n.value = as_int.to_string();
        } else {
            n.tag = "!!float".into();
            n.value = crate::gostr::fmt_v(f);
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String, JErr> {
        // at '"'
        self.i += 1;
        let mut out = String::new();
        loop {
            if self.i >= self.b.len() {
                return Err(JErr::Eof);
            }
            let c = self.b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    if self.i >= self.b.len() {
                        return Err(JErr::Eof);
                    }
                    let esc = self.b[self.i];
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let r = self.hex4()?;
                            if (0xD800..0xDC00).contains(&r) {
                                // a surrogate pair?
                                if self.i + 1 < self.b.len() && self.b[self.i] == b'\\' && self.b[self.i + 1] == b'u' {
                                    let save = self.i;
                                    self.i += 2;
                                    let r2 = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&r2) {
                                        let cp = 0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00);
                                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                        continue;
                                    }
                                    self.i = save;
                                }
                                out.push('\u{fffd}');
                            } else if (0xDC00..0xE000).contains(&r) {
                                out.push('\u{fffd}');
                            } else {
                                out.push(char::from_u32(r).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err("in string escape code"));
                        }
                    }
                }
                c if c < 0x20 => return Err(self.err("in string literal")),
                _ => {
                    // copy one UTF-8 character (invalid bytes become U+FFFD)
                    let w = crate::yaml::width(c).max(1);
                    let end = (self.i + w).min(self.b.len());
                    match std::str::from_utf8(&self.b[self.i..end]) {
                        Ok(s) => {
                            out.push_str(s);
                            self.i = end;
                        }
                        Err(_) => {
                            out.push('\u{fffd}');
                            self.i += 1;
                        }
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JErr> {
        let mut v = 0u32;
        for _ in 0..4 {
            if self.i >= self.b.len() {
                return Err(JErr::Eof);
            }
            let c = self.b[self.i];
            let d = (c as char).to_digit(16).ok_or_else(|| self.err("in \\u hexadecimal character escape"))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }
}

impl Decoder for JsonDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        self.input = input;
        self.pos = 0;
        Ok(())
    }

    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        let mut p = P { b: &self.input, i: self.pos };
        p.ws();
        if p.i >= p.b.len() {
            self.pos = p.i;
            return Ok(None);
        }
        let node = e.add(CNode { filename: "".into(), ..Default::default() });
        match p.value(e, node) {
            Ok(()) => {
                self.pos = p.i;
                Ok(Some(node))
            }
            Err(JErr::Eof) => Err("unexpected EOF".into()),
            Err(JErr::Syntax(s)) => Err(s),
        }
    }
}

// ---- encoding ----

pub struct JsonEncoder {
    pub indent: i64,
    pub unwrap_scalar: bool,
    pub colors: bool,
}

const MARSHAL_PREFIX: &str = "json: error calling MarshalJSON for type *yqlib.CandidateNode: ";

/// encoding/json's string encoding with SetEscapeHTML(false).
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn is_json_number_literal(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    if b[i] == b'-' {
        i += 1;
        if i == b.len() {
            return false;
        }
    }
    if b[i] == b'0' {
        i += 1;
    } else if b[i].is_ascii_digit() {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }
    let mut frac = false;
    if i < b.len() && b[i] == b'.' {
        frac = true;
        i += 1;
        if i == b.len() || !b[i].is_ascii_digit() {
            return false;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    let mut exp = false;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        exp = true;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if i == b.len() || !b[i].is_ascii_digit() {
            return false;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    i == b.len() && (frac || exp)
}

fn json_float_literal(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let f = crate::gostr::parse_float(raw).ok()?;
    if f.is_infinite() || f.is_nan() {
        return None;
    }
    if is_json_number_literal(raw) {
        return Some(raw.to_string());
    }
    let mut s = crate::gostr::format_float(f, b'f', -1);
    if !s.contains(['.', 'e', 'E']) {
        s.push_str(".0");
    }
    Some(s)
}

/// encoding/json's float64 encoding
pub fn json_float(f: f64) -> Result<String, String> {
    if f.is_nan() || f.is_infinite() {
        return Err(format!("json: unsupported value: {}", crate::gostr::fmt_v(f)));
    }
    let abs = f.abs();
    let mut fmt = b'f';
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        fmt = b'e';
    }
    let mut s = crate::gostr::format_float(f, fmt, -1);
    if fmt == b'e' {
        // clean up e-09 to e-9
        let n = s.len();
        if n >= 4 && &s[n - 4..n - 2] == "e-" && &s[n - 2..n - 1] == "0" {
            s.remove(n - 2);
        }
    }
    Ok(s)
}

/// CandidateNode.MarshalJSON into compact JSON; errors carry encoding/json's wrapping.
pub fn marshal(e: &mut Engine, node: NodeId, out: &mut String) -> Result<(), String> {
    let n = e.n(node);
    match n.kind {
        ALIAS => match n.alias {
            Some(a) => marshal(e, a, out).map_err(|err| format!("{MARSHAL_PREFIX}{err}")),
            None => {
                out.push_str("null");
                Ok(())
            }
        },
        SCALAR => {
            if e.guess_tag_from_custom_type(node) == "!!float" {
                if let Some(raw) = json_float_literal(&e.n(node).value) {
                    out.push_str(&raw);
                    return Ok(());
                }
            }
            let tag = e.guess_tag_from_custom_type(node);
            let v = e.n(node).value.clone();
            match tag.as_str() {
                "!!int" => {
                    let (_, i) = crate::util::parse_int64(&v)?;
                    out.push_str(&i.to_string());
                }
                "!!float" => {
                    let f = crate::util::parse_float(&v)?;
                    out.push_str(&json_float(f)?);
                }
                "!!bool" => out.push_str(if is_truthy(e, Some(node)) { "true" } else { "false" }),
                "!!null" => out.push_str("null"),
                _ => write_json_string(out, &v),
            }
            Ok(())
        }
        MAP => {
            out.push('{');
            let content = n.content.clone();
            let mut i = 0;
            while i + 1 < content.len() {
                write_json_string(out, &e.n(content[i]).value);
                out.push(':');
                marshal(e, content[i + 1], out).map_err(|err| format!("{MARSHAL_PREFIX}{err}"))?;
                if i != content.len() - 2 {
                    out.push(',');
                }
                i += 2;
            }
            out.push('}');
            Ok(())
        }
        SEQ => {
            let content = n.content.clone();
            if content.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, c) in content.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                marshal(e, *c, out).map_err(|err| format!("{MARSHAL_PREFIX}{err}"))?;
            }
            out.push(']');
            Ok(())
        }
        _ => {
            out.push_str("null");
            Ok(())
        }
    }
}

/// encoding/json's Indent of compact JSON.
pub fn indent_json(src: &str, indent: &str) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let mut depth = 0usize;
    let mut need_indent = false;
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = src.chars().collect();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    for (idx, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if need_indent && c != ']' && c != '}' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                need_indent = true;
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => {
                out.push(c);
                out.push(' ');
            }
            '}' | ']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
        let _ = idx;
    }
    out
}

impl Encoder for JsonEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        if e.n(node).kind == SCALAR && self.unwrap_scalar {
            out.extend_from_slice(e.n(node).value.as_bytes());
            out.push(b'\n');
            return Ok(());
        }
        let mut compact = String::new();
        marshal(e, node, &mut compact).map_err(|err| format!("{MARSHAL_PREFIX}{err}"))?;
        let mut s = if self.indent > 0 { indent_json(&compact, &" ".repeat(self.indent as usize)) } else { compact };
        s.push('\n');
        if self.colors {
            out.extend_from_slice(&crate::color::colorize_yaml(s.as_bytes()));
        } else {
            out.extend_from_slice(s.as_bytes());
        }
        Ok(())
    }
}
