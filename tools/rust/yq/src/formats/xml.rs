//! XML: a port of the parts of Go's encoding/xml yq uses (Decoder.RawToken/Token in strict and
//! non-strict mode, Encoder.EncodeToken with indentation) and yq's XML decoder and encoder.

use super::{Decoder, Encoder};
use crate::eval::Res;
use crate::node::*;
use crate::prefs::XmlPrefs;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Name {
    pub space: String,
    pub local: String,
}

#[derive(Clone, Debug)]
pub struct Attr {
    pub name: Name,
    pub value: String,
}

#[derive(Clone, Debug)]
pub enum Token {
    Start(Name, Vec<Attr>),
    End(Name),
    CharData(Vec<u8>),
    Comment(Vec<u8>),
    ProcInst(String, Vec<u8>),
    Directive(Vec<u8>),
}

pub struct XmlDecoder {
    data: Vec<u8>,
    pos: usize,
    next_byte: i32,
    line: usize,
    err: Option<String>,
    eof: bool,
    pub strict: bool,
    need_close: bool,
    to_close: Name,
    buf: Vec<u8>,
    // Token mode
    stk: Vec<StackItem>,
    ns: std::collections::HashMap<String, String>,
}

#[derive(Clone)]
enum StackItem {
    Start(Name),
    Ns(String, String, bool),
}

fn syntax(line: usize, msg: &str) -> String {
    format!("XML syntax error on line {line}: {msg}")
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b':' || c == b'.' || c == b'-'
}

fn is_name(s: &[u8]) -> bool {
    let Ok(st) = std::str::from_utf8(s) else { return false };
    let mut chars = st.chars();
    let Some(first) = chars.next() else { return false };
    if !(first == '_' || first == ':' || first.is_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c == ':' || c == '.' || c == '-' || c.is_alphanumeric() || c == '\u{B7}' || is_combining(c))
}

fn is_combining(c: char) -> bool {
    let u = c as u32;
    (0x0300..=0x036F).contains(&u) || (0x0483..=0x0486).contains(&u) || (0x0591..=0x05C4).contains(&u)
}

fn in_char_range(r: u32) -> bool {
    r == 0x09 || r == 0x0A || r == 0x0D || (0x20..=0xD7FF).contains(&r) || (0xE000..=0xFFFD).contains(&r) || (0x10000..=0x10FFFF).contains(&r)
}

impl XmlDecoder {
    pub fn new(data: Vec<u8>, strict: bool) -> XmlDecoder {
        XmlDecoder {
            data,
            pos: 0,
            next_byte: -1,
            line: 1,
            err: None,
            eof: false,
            strict,
            need_close: false,
            to_close: Name::default(),
            buf: Vec::new(),
            stk: Vec::new(),
            ns: Default::default(),
        }
    }

    fn getc(&mut self) -> Option<u8> {
        if self.err.is_some() || self.eof {
            return None;
        }
        let b;
        if self.next_byte >= 0 {
            b = self.next_byte as u8;
            self.next_byte = -1;
        } else {
            if self.pos >= self.data.len() {
                self.eof = true;
                return None;
            }
            b = self.data[self.pos];
            self.pos += 1;
        }
        if b == b'\n' {
            self.line += 1;
        }
        Some(b)
    }

    fn mustgetc(&mut self) -> Option<u8> {
        let b = self.getc();
        if b.is_none() && self.eof && self.err.is_none() {
            self.err = Some(syntax(self.line, "unexpected EOF"));
        }
        b
    }

    fn ungetc(&mut self, b: u8) {
        if b == b'\n' {
            self.line -= 1;
        }
        self.next_byte = b as i32;
    }

    fn set_err(&mut self, msg: &str) {
        self.err = Some(syntax(self.line, msg));
    }

    fn space(&mut self) {
        while let Some(b) = self.getc() {
            if !matches!(b, b' ' | b'\r' | b'\n' | b'\t') {
                self.ungetc(b);
                return;
            }
        }
    }

    fn read_name(&mut self) -> bool {
        let Some(b) = self.mustgetc() else { return false };
        if b < 0x80 && !is_name_byte(b) {
            self.ungetc(b);
            return false;
        }
        self.buf.push(b);
        loop {
            let Some(b) = self.mustgetc() else { return false };
            if b < 0x80 && !is_name_byte(b) {
                self.ungetc(b);
                break;
            }
            self.buf.push(b);
        }
        true
    }

    fn name(&mut self) -> Option<String> {
        self.buf.clear();
        if !self.read_name() {
            return None;
        }
        if !is_name(&self.buf) {
            let s = String::from_utf8_lossy(&self.buf).into_owned();
            self.set_err(&format!("invalid XML name: {s}"));
            return None;
        }
        Some(String::from_utf8_lossy(&self.buf).into_owned())
    }

    fn nsname(&mut self) -> Option<Name> {
        let s = self.name()?;
        if s.matches(':').count() > 1 {
            return None;
        }
        match s.split_once(':') {
            Some((sp, l)) if !sp.is_empty() && !l.is_empty() => Some(Name { space: sp.into(), local: l.into() }),
            _ => Some(Name { space: String::new(), local: s }),
        }
    }

    fn text(&mut self, quote: i32, cdata: bool) -> Option<Vec<u8>> {
        let (mut b0, mut b1) = (0u8, 0u8);
        let mut trunc = 0;
        self.buf.clear();
        loop {
            let Some(b) = self.getc() else {
                if cdata {
                    if self.eof {
                        self.set_err("unexpected EOF in CDATA section");
                    }
                    return None;
                }
                break;
            };
            if quote < 0 && b0 == b']' && b1 == b']' && b == b'>' {
                if cdata {
                    trunc = 2;
                    break;
                }
                self.set_err("unescaped ]]> not in CDATA section");
                return None;
            }
            if b == b'<' && !cdata {
                if quote >= 0 {
                    self.set_err("unescaped < inside quoted string");
                    return None;
                }
                self.ungetc(b'<');
                break;
            }
            if quote >= 0 && b == quote as u8 {
                break;
            }
            if b == b'&' && !cdata {
                let before = self.buf.len();
                self.buf.push(b'&');
                let mut text: Option<String> = None;
                let mut b = self.mustgetc()?;
                if b == b'#' {
                    self.buf.push(b);
                    b = self.mustgetc()?;
                    let mut base = 10;
                    if b == b'x' {
                        base = 16;
                        self.buf.push(b);
                        b = self.mustgetc()?;
                    }
                    let start = self.buf.len();
                    while b.is_ascii_digit() || base == 16 && b.is_ascii_hexdigit() {
                        self.buf.push(b);
                        b = self.mustgetc()?;
                    }
                    if b != b';' {
                        self.ungetc(b);
                    } else {
                        let s = String::from_utf8_lossy(&self.buf[start..]).into_owned();
                        self.buf.push(b';');
                        if let Ok(n) = u64::from_str_radix(&s, base) {
                            if n <= 0x10FFFF {
                                text = Some(char::from_u32(n as u32).map(|c| c.to_string()).unwrap_or_else(|| "\u{fffd}".into()));
                            }
                        }
                    }
                } else {
                    self.ungetc(b);
                    if !self.read_name() && self.err.is_some() {
                        return None;
                    }
                    let b = self.mustgetc()?;
                    if b != b';' {
                        self.ungetc(b);
                    } else {
                        let name = self.buf[before + 1..].to_vec();
                        self.buf.push(b';');
                        if is_name(&name) {
                            text = match &name[..] {
                                b"lt" => Some("<".into()),
                                b"gt" => Some(">".into()),
                                b"amp" => Some("&".into()),
                                b"apos" => Some("'".into()),
                                b"quot" => Some("\"".into()),
                                _ => None,
                            };
                        }
                    }
                }
                if let Some(t) = text {
                    self.buf.truncate(before);
                    self.buf.extend_from_slice(t.as_bytes());
                    b0 = 0;
                    b1 = 0;
                    continue;
                }
                if !self.strict {
                    b0 = 0;
                    b1 = 0;
                    continue;
                }
                let mut ent = String::from_utf8_lossy(&self.buf[before..]).into_owned();
                if !ent.ends_with(';') {
                    ent.push_str(" (no semicolon)");
                }
                self.set_err(&format!("invalid character entity {ent}"));
                return None;
            }
            if b == b'\r' {
                self.buf.push(b'\n');
            } else if b1 == b'\r' && b == b'\n' {
            } else {
                self.buf.push(b);
            }
            b0 = b1;
            b1 = b;
        }
        let mut data = self.buf.clone();
        data.truncate(data.len() - trunc);
        match std::str::from_utf8(&data) {
            Err(_) => {
                self.set_err("invalid UTF-8");
                return None;
            }
            Ok(s) => {
                for c in s.chars() {
                    if !in_char_range(c as u32) {
                        self.set_err(&format!("illegal character code U+{:04X}", c as u32));
                        return None;
                    }
                }
            }
        }
        Some(data)
    }

    fn attrval(&mut self) -> Option<Vec<u8>> {
        let b = self.mustgetc()?;
        if b == b'"' || b == b'\'' {
            return self.text(b as i32, false);
        }
        if self.strict {
            self.set_err("unquoted or missing attribute value in element");
            return None;
        }
        self.ungetc(b);
        self.buf.clear();
        loop {
            let b = self.mustgetc()?;
            if b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'-' {
                self.buf.push(b);
            } else {
                self.ungetc(b);
                break;
            }
        }
        Some(self.buf.clone())
    }

    /// RawToken: Ok(None) at EOF.
    pub fn raw_token(&mut self) -> Result<Option<Token>, String> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        if self.need_close {
            self.need_close = false;
            return Ok(Some(Token::End(std::mem::take(&mut self.to_close))));
        }
        let Some(b) = self.getc() else { return self.eof_or_err() };
        if b != b'<' {
            self.ungetc(b);
            return match self.text(-1, false) {
                Some(d) => Ok(Some(Token::CharData(d))),
                None => self.eof_or_err(),
            };
        }
        let Some(b) = self.mustgetc() else { return self.eof_or_err() };
        match b {
            b'/' => {
                let Some(name) = self.nsname() else {
                    if self.err.is_none() {
                        self.set_err("expected element name after </");
                    }
                    return self.eof_or_err();
                };
                self.space();
                let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                if b != b'>' {
                    self.set_err(&format!("invalid characters between </{} and >", name.local));
                    return self.eof_or_err();
                }
                return Ok(Some(Token::End(name)));
            }
            b'?' => {
                let Some(target) = self.name() else {
                    if self.err.is_none() {
                        self.set_err("expected target name after <?");
                    }
                    return self.eof_or_err();
                };
                self.space();
                self.buf.clear();
                let mut b0 = 0u8;
                loop {
                    let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                    self.buf.push(b);
                    if b0 == b'?' && b == b'>' {
                        break;
                    }
                    b0 = b;
                }
                let mut data = self.buf.clone();
                data.truncate(data.len() - 2);
                if target == "xml" {
                    let content = String::from_utf8_lossy(&data).into_owned();
                    let ver = proc_inst("version", &content);
                    if !ver.is_empty() && ver != "1.0" {
                        let e = format!("xml: unsupported version {}; only version 1.0 is supported", crate::util::go_quote(&ver));
                        self.err = Some(e.clone());
                        return Err(e);
                    }
                    let enc = proc_inst("encoding", &content);
                    if !enc.is_empty() && !enc.eq_ignore_ascii_case("utf-8") {
                        match convert_charset(&enc, &self.data[self.pos..]) {
                            Ok(converted) => {
                                self.data.truncate(self.pos);
                                self.data.extend_from_slice(&converted);
                            }
                            Err(err) => {
                                let e = format!("xml: opening charset {}: {err}", crate::util::go_quote(&enc));
                                self.err = Some(e.clone());
                                return Err(e);
                            }
                        }
                    }
                }
                return Ok(Some(Token::ProcInst(target, data)));
            }
            b'!' => {
                let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                match b {
                    b'-' => {
                        let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                        if b != b'-' {
                            self.set_err("invalid sequence <!- not part of <!--");
                            return self.eof_or_err();
                        }
                        self.buf.clear();
                        let (mut b0, mut b1) = (0u8, 0u8);
                        loop {
                            let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                            self.buf.push(b);
                            if b0 == b'-' && b1 == b'-' {
                                if b != b'>' {
                                    self.set_err("invalid sequence \"--\" not allowed in comments");
                                    return self.eof_or_err();
                                }
                                break;
                            }
                            b0 = b1;
                            b1 = b;
                        }
                        let mut data = self.buf.clone();
                        data.truncate(data.len() - 3);
                        return Ok(Some(Token::Comment(data)));
                    }
                    b'[' => {
                        for i in 0..6 {
                            let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                            if b != b"CDATA["[i] {
                                self.set_err("invalid <![ sequence");
                                return self.eof_or_err();
                            }
                        }
                        return match self.text(-1, true) {
                            Some(d) => Ok(Some(Token::CharData(d))),
                            None => self.eof_or_err(),
                        };
                    }
                    _ => {}
                }
                // a directive
                self.buf.clear();
                self.buf.push(b);
                let mut inquote = 0u8;
                let mut depth = 0i32;
                loop {
                    let Some(mut b) = self.mustgetc() else { return self.eof_or_err() };
                    if inquote == 0 && b == b'>' && depth == 0 {
                        break;
                    }
                    'handle: loop {
                        self.buf.push(b);
                        if b == inquote {
                            inquote = 0;
                        } else if inquote != 0 {
                        } else if b == b'\'' || b == b'"' {
                            inquote = b;
                        } else if b == b'>' {
                            depth -= 1;
                        } else if b == b'<' {
                            let s = b"!--";
                            for i in 0..3 {
                                let Some(c) = self.mustgetc() else { return self.eof_or_err() };
                                b = c;
                                if b != s[i] {
                                    for j in 0..i {
                                        self.buf.push(s[j]);
                                    }
                                    depth += 1;
                                    continue 'handle;
                                }
                            }
                            self.buf.pop();
                            let (mut b0, mut b1) = (0u8, 0u8);
                            loop {
                                let Some(c) = self.mustgetc() else { return self.eof_or_err() };
                                if b0 == b'-' && b1 == b'-' && c == b'>' {
                                    break;
                                }
                                b0 = b1;
                                b1 = c;
                            }
                            self.buf.push(b' ');
                        }
                        break;
                    }
                }
                return Ok(Some(Token::Directive(self.buf.clone())));
            }
            _ => {}
        }
        self.ungetc(b);
        let Some(name) = self.nsname() else {
            if self.err.is_none() {
                self.set_err("expected element name after <");
            }
            return self.eof_or_err();
        };
        let mut attrs = Vec::new();
        let mut empty = false;
        loop {
            self.space();
            let Some(b) = self.mustgetc() else { return self.eof_or_err() };
            if b == b'/' {
                empty = true;
                let Some(b) = self.mustgetc() else { return self.eof_or_err() };
                if b != b'>' {
                    self.set_err("expected /> in element");
                    return self.eof_or_err();
                }
                break;
            }
            if b == b'>' {
                break;
            }
            self.ungetc(b);
            let Some(an) = self.nsname() else {
                if self.err.is_none() {
                    self.set_err("expected attribute name in element");
                }
                return self.eof_or_err();
            };
            self.space();
            let Some(b) = self.mustgetc() else { return self.eof_or_err() };
            let value;
            if b != b'=' {
                if self.strict {
                    self.set_err("attribute name without = in element");
                    return self.eof_or_err();
                }
                self.ungetc(b);
                value = an.local.clone();
            } else {
                self.space();
                let Some(d) = self.attrval() else { return self.eof_or_err() };
                value = String::from_utf8_lossy(&d).into_owned();
            }
            attrs.push(Attr { name: an, value });
        }
        if empty {
            self.need_close = true;
            self.to_close = name.clone();
        }
        Ok(Some(Token::Start(name, attrs)))
    }

    fn eof_or_err(&mut self) -> Result<Option<Token>, String> {
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(None),
        }
    }

    /// Decoder.Token: namespace translation and end-tag checks.
    pub fn token(&mut self) -> Result<Option<Token>, String> {
        let t = match self.raw_token()? {
            Some(t) => t,
            None => {
                if self.stk.iter().any(|s| matches!(s, StackItem::Start(_))) {
                    return Err(syntax(self.line, "unexpected EOF"));
                }
                return Ok(None);
            }
        };
        match t {
            Token::Start(mut name, mut attrs) => {
                for a in &attrs {
                    if a.name.space == "xmlns" {
                        let old = self.ns.get(&a.name.local).cloned();
                        self.stk.push(StackItem::Ns(a.name.local.clone(), old.clone().unwrap_or_default(), old.is_some()));
                        self.ns.insert(a.name.local.clone(), a.value.clone());
                    }
                    if a.name.space.is_empty() && a.name.local == "xmlns" {
                        let old = self.ns.get("").cloned();
                        self.stk.push(StackItem::Ns(String::new(), old.clone().unwrap_or_default(), old.is_some()));
                        self.ns.insert(String::new(), a.value.clone());
                    }
                }
                self.stk.push(StackItem::Start(name.clone()));
                self.translate(&mut name, true);
                for a in attrs.iter_mut() {
                    self.translate(&mut a.name, false);
                }
                Ok(Some(Token::Start(name, attrs)))
            }
            Token::End(mut name) => {
                // popElement
                let s = self.stk.pop();
                match s {
                    Some(StackItem::Start(sn)) => {
                        if sn.local != name.local {
                            if !self.strict {
                                self.need_close = true;
                                self.to_close = name.clone();
                                name = sn;
                            } else {
                                let e = syntax(self.line, &format!("element <{}> closed by </{}>", sn.local, name.local));
                                self.err = Some(e.clone());
                                return Err(e);
                            }
                        } else if sn.space != name.space {
                            let ns = if name.space.is_empty() { "\"\"".to_string() } else { name.space.clone() };
                            let e = syntax(
                                self.line,
                                &format!("element <{}> in space {} closed by </{}> in space {}", sn.local, sn.space, name.local, ns),
                            );
                            self.err = Some(e.clone());
                            return Err(e);
                        }
                    }
                    _ => {
                        let e = syntax(self.line, &format!("unexpected end element </{}>", name.local));
                        self.err = Some(e.clone());
                        return Err(e);
                    }
                }
                self.translate(&mut name, true);
                while let Some(StackItem::Ns(..)) = self.stk.last() {
                    if let Some(StackItem::Ns(local, url, ok)) = self.stk.pop() {
                        if ok {
                            self.ns.insert(local, url);
                        } else {
                            self.ns.remove(&local);
                        }
                    }
                }
                Ok(Some(Token::End(name)))
            }
            other => Ok(Some(other)),
        }
    }

    fn translate(&self, n: &mut Name, is_element: bool) {
        if n.space == "xmlns" {
            return;
        }
        if n.space.is_empty() && !is_element {
            return;
        }
        if n.space == "xml" {
            n.space = "http://www.w3.org/XML/1998/namespace".into();
        } else if n.space.is_empty() && n.local == "xmlns" {
            return;
        }
        if let Some(v) = self.ns.get(&n.space) {
            n.space = v.clone();
        }
    }
}

/// procInst: the value of a param in a processing instruction.
fn proc_inst(param: &str, s: &str) -> String {
    let mut s = s;
    let p = format!("{param}=");
    loop {
        let Some(i) = s.find(&p) else { return String::new() };
        let before_ok = i == 0 || matches!(s.as_bytes()[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        let v = &s[i + p.len()..];
        if !before_ok {
            s = v;
            continue;
        }
        let Some(q) = v.chars().next() else { return String::new() };
        if q != '\'' && q != '"' {
            return String::new();
        }
        let rest = &v[1..];
        return match rest.find(q) {
            Some(j) => rest[..j].to_string(),
            None => String::new(),
        };
    }
}

/// golang.org/x/net/html/charset for the encodings worth handling here.
fn convert_charset(label: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let l = label.trim().to_ascii_lowercase();
    match l.as_str() {
        "iso-8859-1" | "latin1" | "l1" | "iso8859-1" | "iso_8859-1" | "windows-1252" | "cp1252" | "us-ascii" | "ascii" => {
            let mut out = String::new();
            for b in data {
                let c = if l.contains("1252") || l.contains("8859") || l.contains("latin") || l == "l1" || l.contains("ascii") {
                    windows1252(*b)
                } else {
                    *b as char
                };
                out.push(c);
            }
            Ok(out.into_bytes())
        }
        "utf-16" | "utf-16le" | "utf-16be" => {
            let le = l != "utf-16be";
            let units: Vec<u16> = data.chunks(2).map(|c| if c.len() == 2 { if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) } } else { 0xFFFD }).collect();
            Ok(String::from_utf16_lossy(&units).into_bytes())
        }
        _ => Err(format!("unsupported charset: {}", crate::util::go_quote(label))),
    }
}

fn windows1252(b: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, 0x0090,
        0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
    ];
    if (0x80..0xA0).contains(&b) {
        char::from_u32(HIGH[(b - 0x80) as usize] as u32).unwrap()
    } else {
        b as char
    }
}

// ---- yq's XML decoder ----

#[derive(Default, Clone)]
struct XNode {
    children: Vec<ChildKv>,
    head_comment: String,
    foot_comment: String,
    line_comment: String,
    data: Vec<String>,
}

#[derive(Default, Clone)]
struct ChildKv {
    k: String,
    v: Vec<XNode>,
    foot_comment: String,
}

impl XNode {
    fn add_child(&mut self, s: &str, c: XNode) {
        for e in self.children.iter_mut() {
            if e.k == s {
                e.v.push(c);
                return;
            }
        }
        self.children.push(ChildKv { k: s.into(), v: vec![c], foot_comment: String::new() });
    }
}

fn join_comments(parts: &[&str], sep: &str) -> String {
    parts.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(sep)
}

fn trim_non_graphic(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let keep = |c: char| is_graphic(c) && !crate::util::go_is_space(c);
    let first = chars.iter().position(|c| keep(*c));
    let Some(first) = first else { return String::new() };
    let last = chars.iter().rposition(|c| keep(*c)).unwrap();
    chars[first..=last].iter().collect()
}

/// unicode.IsGraphic: letters, marks, numbers, punctuation, symbols and spaces (Zs).
fn is_graphic(c: char) -> bool {
    if c.is_control() {
        return false;
    }
    let u = c as u32;
    !((0xE000..=0xF8FF).contains(&u) || (0xF0000..=0x10FFFF).contains(&u) || (0xD800..=0xDFFF).contains(&u) || u == 0x2028 || u == 0x2029 || matches!(u, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0xFEFF | 0xFFF9..=0xFFFB | 0x00AD))
}

pub struct YqXmlDecoder {
    prefs: XmlPrefs,
    data: Vec<u8>,
    read_anything: bool,
    finished: bool,
}

impl YqXmlDecoder {
    pub fn new(prefs: XmlPrefs) -> YqXmlDecoder {
        YqXmlDecoder { prefs, data: Vec::new(), read_anything: false, finished: false }
    }

    fn process_comment(&self, c: &str) -> String {
        if c.is_empty() {
            return String::new();
        }
        // `(^|\n)([[:alpha:]])` -> `$1 $2`
        let mut out = String::new();
        let mut at_line_start = true;
        for ch in c.chars() {
            if at_line_start && ch.is_ascii_alphabetic() {
                out.push(' ');
            }
            out.push(ch);
            at_line_start = ch == '\n';
        }
        format!("#{}", out.trim_end_matches(' ').replace('\n', "\n#"))
    }

    fn value_from_data(&self, e: &mut Engine, values: &[String]) -> NodeId {
        match values.len() {
            0 => e.scalar("!!null", ""),
            1 => e.scalar("!!str", &values[0]),
            _ => {
                let content: Vec<NodeId> = values.iter().map(|v| e.scalar("!!str", v)).collect();
                let n = e.add(CNode::new(SEQ, "!!seq"));
                e.nm(n).content = content;
                n
            }
        }
    }

    fn convert(&self, e: &mut Engine, n: &mut XNode) -> Res<NodeId> {
        if !n.children.is_empty() {
            return self.create_map(e, n);
        }
        let s = self.value_from_data(e, &n.data);
        e.nm(s).head_comment = self.process_comment(&n.head_comment);
        e.nm(s).line_comment = self.process_comment(&n.line_comment);
        if e.n(s).tag == "!!seq" {
            let lc = std::mem::take(&mut e.nm(s).line_comment);
            let first = e.n(s).content[0];
            e.nm(first).head_comment = lc;
        }
        e.nm(s).foot_comment = self.process_comment(&n.foot_comment);
        Ok(s)
    }

    fn create_map(&self, e: &mut Engine, n: &mut XNode) -> Res<NodeId> {
        let m = e.add(CNode::new(MAP, "!!map"));
        if !n.data.is_empty() {
            let label = e.scalar("!!str", &self.prefs.content_name);
            e.nm(label).head_comment = self.process_comment(&n.head_comment);
            e.nm(label).line_comment = self.process_comment(&n.line_comment);
            e.nm(label).foot_comment = self.process_comment(&n.foot_comment);
            let v = self.value_from_data(e, &n.data);
            e.add_key_value_child(m, label, v);
        }
        let hc = n.head_comment.clone();
        for (i, kv) in n.children.iter_mut().enumerate() {
            let label = e.scalar("!!str", &kv.k);
            if i == 0 {
                e.nm(label).head_comment = self.process_comment(&hc);
            }
            e.nm(label).foot_comment = self.process_comment(&kv.foot_comment);
            let value = if kv.v.len() > 1 {
                let seq = e.add(CNode::new(SEQ, "!!seq"));
                for c in kv.v.iter_mut() {
                    let y = self.convert(e, c)?;
                    e.add_child(seq, y);
                }
                seq
            } else {
                let c0 = &mut kv.v[0];
                if c0.children.is_empty() && !c0.head_comment.is_empty() {
                    if !c0.data.is_empty() {
                        let lh = e.n(label).head_comment.clone();
                        e.nm(label).head_comment = join_comments(&[&lh, c0.head_comment.trim()], "\n");
                        c0.head_comment = String::new();
                    } else {
                        c0.line_comment = std::mem::take(&mut c0.head_comment);
                    }
                }
                self.convert(e, c0)?
            };
            e.add_key_value_child(m, label, value);
        }
        Ok(m)
    }

    fn decode_xml(&self) -> Res<XNode> {
        let mut dec = XmlDecoder::new(self.data.clone(), self.prefs.strict_mode);
        // the element stack: (node, label, state)
        let mut stack: Vec<(XNode, String, &'static str)> = vec![(XNode::default(), String::new(), "")];
        let mut started = false;
        loop {
            let t = if self.prefs.use_raw_token { dec.raw_token()? } else { dec.token()? };
            let Some(t) = t else { break };
            match t {
                Token::Start(name, attrs) => {
                    stack.last_mut().unwrap().2 = "started";
                    let mut label = name.local.clone();
                    if self.prefs.keep_namespace && !name.space.is_empty() {
                        label = format!("{}:{}", name.space, name.local);
                    }
                    let mut n = XNode::default();
                    for a in attrs {
                        let mut local = a.name.local.clone();
                        if self.prefs.keep_namespace && !a.name.space.is_empty() {
                            local = format!("{}:{}", a.name.space, a.name.local);
                        }
                        n.add_child(&format!("{}{}", self.prefs.attribute_prefix, local), XNode { data: vec![a.value], ..Default::default() });
                    }
                    stack.push((n, label, ""));
                }
                Token::CharData(d) => {
                    let new_bit = trim_non_graphic(&String::from_utf8_lossy(&d));
                    if !started && !new_bit.is_empty() {
                        return Err(format!("invalid XML: Encountered chardata [{new_bit}] outside of XML node"));
                    }
                    if !new_bit.is_empty() {
                        let top = stack.last_mut().unwrap();
                        top.0.data.push(new_bit);
                        top.2 = "chardata";
                    }
                }
                Token::End(_) => {
                    if stack.len() > 1 {
                        let (n, label, _) = stack.pop().unwrap();
                        stack.last_mut().unwrap().0.add_child(&label, n);
                    } else if stack.len() == 1 {
                        // the root's end: elem becomes nil in yq; further tokens are ignored
                        // (the root stays as it is)
                        stack.last_mut().unwrap().2 = "finished";
                    }
                }
                Token::Comment(c) => {
                    let cs = String::from_utf8_lossy(&c).into_owned();
                    let top = stack.last_mut().unwrap();
                    match top.2 {
                        "started" => apply_foot_comment(&mut top.0, &cs),
                        "chardata" => top.0.line_comment = join_comments(&[&top.0.line_comment, &cs], " "),
                        _ => top.0.head_comment = join_comments(&[&top.0.head_comment, &cs], " "),
                    }
                }
                Token::ProcInst(target, inst) => {
                    if !self.prefs.skip_proc_inst {
                        let top = stack.last_mut().unwrap();
                        top.0.add_child(
                            &format!("{}{}", self.prefs.proc_inst_prefix, target),
                            XNode { data: vec![String::from_utf8_lossy(&inst).into_owned()], ..Default::default() },
                        );
                    }
                }
                Token::Directive(d) => {
                    if !self.prefs.skip_directives {
                        let top = stack.last_mut().unwrap();
                        top.0.add_child(&self.prefs.directive_name, XNode { data: vec![String::from_utf8_lossy(&d).into_owned()], ..Default::default() });
                    }
                }
            }
            started = true;
        }
        // elements left open at EOF are dropped, as yq leaves them unattached
        Ok(stack.swap_remove(0).0)
    }
}

fn apply_foot_comment(n: &mut XNode, c: &str) {
    if let Some(kv) = n.children.last_mut() {
        if !kv.v.is_empty() && kv.v[0].children.is_empty() {
            let last = kv.v.last_mut().unwrap();
            last.foot_comment = join_comments(&[&last.foot_comment, c], " ");
        } else {
            kv.foot_comment = join_comments(&[&n.foot_comment, c], " ");
        }
    } else {
        n.foot_comment = join_comments(&[&n.foot_comment, c], " ");
    }
}

impl Decoder for YqXmlDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        self.data = input;
        self.read_anything = false;
        self.finished = false;
        Ok(())
    }

    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        if self.finished {
            return Ok(None);
        }
        let mut root = self.decode_xml()?;
        let first = self.convert(e, &mut root)?;
        if e.n(first).tag == "!!null" {
            self.finished = true;
            if self.read_anything {
                return Ok(None);
            }
        }
        self.read_anything = true;
        self.finished = true;
        Ok(Some(first))
    }
}

// ---- Go's xml.Encoder ----

pub struct XmlPrinter<'a> {
    pub out: &'a mut Vec<u8>,
    start_len: usize,
    indent: String,
    depth: usize,
    indented_in: bool,
    put_newline: bool,
    tags: Vec<String>,
}

fn escape_text(out: &mut Vec<u8>, s: &str, escape_newline: bool) {
    for c in s.chars() {
        let esc: Option<&str> = match c {
            '"' => Some("&#34;"),
            '\'' => Some("&#39;"),
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '\t' => Some("&#x9;"),
            '\n' => {
                if escape_newline {
                    Some("&#xA;")
                } else {
                    None
                }
            }
            '\r' => Some("&#xD;"),
            c if !in_char_range(c as u32) => Some("\u{FFFD}"),
            _ => None,
        };
        match esc {
            Some(e) => out.extend_from_slice(e.as_bytes()),
            None => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
}

fn is_valid_directive(dir: &[u8]) -> bool {
    let (mut depth, mut inquote, mut incomment) = (0i32, 0u8, false);
    for (i, &c) in dir.iter().enumerate() {
        if incomment {
            if c == b'>' {
                let n = 1 + i as i64 - 3;
                if n >= 0 && &dir[n as usize..=i] == b"-->" {
                    incomment = false;
                }
            }
        } else if inquote != 0 {
            if c == inquote {
                inquote = 0;
            }
        } else if c == b'\'' || c == b'"' {
            inquote = c;
        } else if c == b'<' {
            if i + 4 < dir.len() && &dir[i..i + 4] == b"<!--" {
                incomment = true;
            } else {
                depth += 1;
            }
        } else if c == b'>' {
            if depth == 0 {
                return false;
            }
            depth -= 1;
        }
    }
    depth == 0 && inquote == 0 && !incomment
}

impl<'a> XmlPrinter<'a> {
    pub fn new(out: &'a mut Vec<u8>, indent: &str) -> XmlPrinter<'a> {
        let start_len = out.len();
        XmlPrinter { out, start_len, indent: indent.to_string(), depth: 0, indented_in: false, put_newline: false, tags: Vec::new() }
    }

    fn write_indent(&mut self, delta: i32) {
        if self.indent.is_empty() {
            return;
        }
        if delta < 0 {
            self.depth = self.depth.saturating_sub(1);
            if self.indented_in {
                self.indented_in = false;
                return;
            }
            self.indented_in = false;
        }
        if self.put_newline {
            self.out.push(b'\n');
        } else {
            self.put_newline = true;
        }
        for _ in 0..self.depth {
            self.out.extend_from_slice(self.indent.as_bytes());
        }
        if delta > 0 {
            self.depth += 1;
            self.indented_in = true;
        }
    }

    pub fn start(&mut self, name: &str, attrs: &[(String, String)]) -> Res<()> {
        if name.is_empty() {
            return Err("xml: start tag with no name".into());
        }
        self.tags.push(name.to_string());
        self.write_indent(1);
        self.out.push(b'<');
        self.out.extend_from_slice(name.as_bytes());
        for (n, v) in attrs {
            if n.is_empty() {
                continue;
            }
            self.out.push(b' ');
            self.out.extend_from_slice(n.as_bytes());
            self.out.extend_from_slice(b"=\"");
            escape_text(self.out, v, true);
            self.out.push(b'"');
        }
        self.out.push(b'>');
        Ok(())
    }

    pub fn end(&mut self, name: &str) -> Res<()> {
        if name.is_empty() {
            return Err("xml: end tag with no name".into());
        }
        match self.tags.last() {
            None => return Err(format!("xml: end tag </{name}> without start tag")),
            Some(t) if t != name => return Err(format!("xml: end tag </{name}> does not match start tag <{t}>")),
            _ => {}
        }
        self.tags.pop();
        self.write_indent(-1);
        self.out.extend_from_slice(b"</");
        self.out.extend_from_slice(name.as_bytes());
        self.out.push(b'>');
        Ok(())
    }

    pub fn chardata(&mut self, s: &str) {
        escape_text(self.out, s, false);
    }

    pub fn comment(&mut self, c: &str) -> Res<()> {
        if c.contains("-->") {
            return Err("xml: EncodeToken of Comment containing --> marker".into());
        }
        self.out.extend_from_slice(b"<!--");
        self.out.extend_from_slice(c.as_bytes());
        self.out.extend_from_slice(b"-->");
        Ok(())
    }

    pub fn proc_inst(&mut self, target: &str, inst: &str) -> Res<()> {
        if target == "xml" && self.out.len() != self.start_len {
            return Err("xml: EncodeToken of ProcInst xml target only valid for xml declaration, first token encoded".into());
        }
        if !is_name(target.as_bytes()) {
            return Err("xml: EncodeToken of ProcInst with invalid Target".into());
        }
        if inst.contains("?>") {
            return Err("xml: EncodeToken of ProcInst containing ?> marker".into());
        }
        self.out.extend_from_slice(b"<?");
        self.out.extend_from_slice(target.as_bytes());
        if !inst.is_empty() {
            self.out.push(b' ');
            self.out.extend_from_slice(inst.as_bytes());
        }
        self.out.extend_from_slice(b"?>");
        Ok(())
    }

    pub fn directive(&mut self, d: &str) -> Res<()> {
        if !is_valid_directive(d.as_bytes()) {
            return Err("xml: EncodeToken of Directive containing wrong < or > markers".into());
        }
        self.out.extend_from_slice(b"<!");
        self.out.extend_from_slice(d.as_bytes());
        self.out.push(b'>');
        Ok(())
    }
}

// ---- yq's XML encoder ----

pub struct YqXmlEncoder {
    pub prefs: XmlPrefs,
    pub indent: String,
    leading_content: String,
}

impl YqXmlEncoder {
    pub fn new(prefs: XmlPrefs) -> YqXmlEncoder {
        let indent = " ".repeat(prefs.indent.max(0) as usize);
        YqXmlEncoder { prefs, indent, leading_content: String::new() }
    }

    fn is_attribute(&self, name: &str) -> bool {
        name.starts_with(&self.prefs.attribute_prefix)
            && name != self.prefs.content_name
            && name != self.prefs.directive_name
            && !name.starts_with(&self.prefs.proc_inst_prefix)
    }

    fn encode_comment(&self, p: &mut XmlPrinter, comment: &str) -> Res<()> {
        if comment.is_empty() {
            return Ok(());
        }
        let mut c = comment.to_string();
        if c.len() > 2 && c[1..c.len() - 1].contains('\n') {
            if c.ends_with('\n') {
                c.pop();
            }
            // `(^|\n) *# ?(.*)` -> `$1$2`
            let lines: Vec<String> = c
                .split('\n')
                .map(|l| {
                    let t = l.trim_start_matches(' ');
                    if let Some(r) = t.strip_prefix('#') {
                        r.strip_prefix(' ').unwrap_or(r).to_string()
                    } else {
                        l.to_string()
                    }
                })
                .collect();
            c = lines.join("\n");
            if !c.starts_with('\n') && !c.starts_with(' ') {
                c = format!(" {c}");
            }
        } else {
            // `^\s*#(.*)\n?` -> `$1`
            let t = c.trim_start_matches(|ch: char| matches!(ch, ' ' | '\t' | '\n' | '\x0c' | '\r'));
            if let Some(rest) = t.strip_prefix('#') {
                let (line, tail) = match rest.find('\n') {
                    Some(i) => (&rest[..i], &rest[i + 1..]),
                    None => (rest, ""),
                };
                c = format!("{line}{tail}");
            }
        }
        if !c.ends_with(' ') && !c.ends_with('\n') {
            c.push(' ');
        }
        p.comment(&c)
    }

    fn do_encode(&self, e: &Engine, p: &mut XmlPrinter, node: NodeId, name: &str) -> Res<()> {
        let n = e.n(node);
        match n.kind {
            MAP => self.encode_map(e, p, node, name),
            SEQ => {
                self.encode_comment(p, &head_and_line(e, node))?;
                for c in &n.content {
                    self.do_encode(e, p, *c, name)?;
                }
                self.encode_comment(p, &foot(e, node))
            }
            SCALAR => {
                p.start(name, &[])?;
                self.encode_comment(p, &head(e, node))?;
                p.chardata(&n.value);
                self.encode_comment(p, &line(e, node))?;
                p.end(name)?;
                self.encode_comment(p, &foot(e, node))
            }
            _ => Err(format!("unsupported type {}", n.tag)),
        }
    }

    fn encode_map(&self, e: &Engine, p: &mut XmlPrinter, node: NodeId, name: &str) -> Res<()> {
        let c = e.n(node).content.clone();
        let mut attrs = Vec::new();
        let mut i = 0;
        while i + 1 < c.len() {
            let (k, v) = (c[i], c[i + 1]);
            if self.is_attribute(&e.n(k).value) {
                if e.n(v).kind == SCALAR {
                    let an = e.n(k).value.replacen(&self.prefs.attribute_prefix, "", 1);
                    attrs.push((an, e.n(v).value.clone()));
                } else {
                    return Err(format!("cannot use {} as attribute, only scalars are supported", e.n(v).tag));
                }
            }
            i += 2;
        }
        p.start(name, &attrs)?;
        self.encode_comment(p, &head(e, node))?;
        let mut i = 0;
        while i + 1 < c.len() {
            let (k, v) = (c[i], c[i + 1]);
            let kv = e.n(k).value.clone();
            self.encode_comment(p, &head_and_line(e, k))?;
            if kv.starts_with(&self.prefs.proc_inst_prefix) {
                let pn = kv.replacen(&self.prefs.proc_inst_prefix, "", 1);
                p.proc_inst(&pn, &e.n(v).value)?;
            } else if kv == self.prefs.directive_name {
                p.directive(&e.n(v).value)?;
            } else if kv == self.prefs.content_name {
                self.encode_comment(p, &head_and_line(e, v))?;
                p.chardata(&e.n(v).value);
                self.encode_comment(p, &foot(e, v))?;
            } else if !self.is_attribute(&kv) {
                self.do_encode(e, p, v, &kv)?;
            }
            self.encode_comment(p, &foot(e, k))?;
            i += 2;
        }
        p.end(name)?;
        self.encode_comment(p, &foot(e, node))
    }

    fn encode_top_level_map(&self, e: &Engine, p: &mut XmlPrinter, node: NodeId) -> Res<()> {
        self.encode_comment(p, &head_and_line(e, node))?;
        let c = e.n(node).content.clone();
        let mut i = 0;
        while i + 1 < c.len() {
            let (k, v) = (c[i], c[i + 1]);
            let kv = e.n(k).value.clone();
            self.encode_comment(p, &head_and_line(e, k))?;
            if !head_and_line(e, k).is_empty() {
                p.chardata("\n");
            }
            if kv == format!("{}xml", self.prefs.proc_inst_prefix) {
            } else if kv.starts_with(&self.prefs.proc_inst_prefix) {
                let pn = kv.replacen(&self.prefs.proc_inst_prefix, "", 1);
                p.proc_inst(&pn, &e.n(v).value)?;
                p.out.push(b'\n');
            } else if kv == self.prefs.directive_name {
                p.directive(&e.n(v).value)?;
                p.out.push(b'\n');
            } else {
                self.do_encode(e, p, v, &kv)?;
            }
            self.encode_comment(p, &foot(e, k))?;
            i += 2;
        }
        self.encode_comment(p, &foot(e, node))
    }
}

fn head(e: &Engine, n: NodeId) -> String {
    e.n(n).head_comment.replacen('#', "", 1)
}
fn line(e: &Engine, n: NodeId) -> String {
    e.n(n).line_comment.replacen('#', "", 1)
}
fn foot(e: &Engine, n: NodeId) -> String {
    e.n(n).foot_comment.replacen('#', "", 1)
}
fn head_and_line(e: &Engine, n: NodeId) -> String {
    head(e, n) + &line(e, n)
}

impl Encoder for YqXmlEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, content: &str) -> Res<()> {
        self.leading_content = content.to_string();
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        let mut p = XmlPrinter::new(out, &self.indent);
        if e.n(node).tag == "!!map" {
            let c = e.n(node).content.clone();
            let mut i = 0;
            while i + 1 < c.len() {
                if e.n(c[i]).value == format!("{}xml", self.prefs.proc_inst_prefix) {
                    let inst = e.n(c[i + 1]).value.clone();
                    p.proc_inst("xml", &inst)?;
                    p.out.push(b'\n');
                }
                i += 2;
            }
        }
        if !self.leading_content.is_empty() {
            let lc = self.leading_content.clone();
            self.encode_comment(&mut p, &lc)?;
            p.chardata("\n");
        }
        match e.n(node).kind {
            MAP => self.encode_top_level_map(e, &mut p, node)?,
            SCALAR => {
                p.chardata(&e.n(node).value);
                return Ok(());
            }
            _ => return Err(format!("cannot encode {} to XML - only maps can be encoded", e.n(node).tag)),
        }
        p.chardata("\n");
        Ok(())
    }
}
