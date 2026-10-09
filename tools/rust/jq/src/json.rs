//! jq's JSON reader (jv_parse.c's state machine, carried over): bytes classified as
//! whitespace, quote, structure or literal; literals checked when they end; values given out
//! one by one as the input comes in; errors worded and located (line, column in bytes) as jq
//! words them. Also the RS-separated sequence mode (--seq).

use crate::value::{Map, Num, Value};
use std::rc::Rc;

const MAX_DEPTH: usize = 10000;

#[derive(PartialEq, Eq, Clone, Copy)]
enum St {
    Normal,
    Str,
    StrEscape,
    WaitRs,
}

enum Frame {
    Arr(Vec<Value>),
    Obj(Map),
    Key(Rc<str>),
}

pub struct Parser {
    stack: Vec<Frame>,
    next: Option<Value>,
    token: Vec<u8>,
    st: St,
    line: u64,
    column: u64,
    seq: bool,
    eof: bool,
    last_ch_was_ws: bool,
    bom: usize,
    buf: Vec<u8>,
    pos: usize,
    partial: bool,
    dead: bool,
    /// --stream: values given out as [path, leaf] events (stream_token), as they are read.
    streaming: bool,
    /// --stream-errors: errors given out as [message, path] values.
    stream_errors: bool,
    path: Vec<Value>,
    last_seen: Last,
    output: Option<Vec<Value>>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Last {
    None,
    OpenArray,
    OpenObject,
    Colon,
    Comma,
    Value,
}

pub enum Next {
    Value(Value),
    Error(String),
    /// Needs more input (or, after finish(), nothing more).
    None,
}

type R = Result<(), &'static str>;

impl Parser {
    pub fn new(seq: bool) -> Parser {
        Parser {
            stack: vec![],
            next: None,
            token: vec![],
            // A sequence starts with an RS: text before the first one is abandoned.
            st: if seq { St::WaitRs } else { St::Normal },
            line: 1,
            column: 0,
            seq,
            eof: false,
            last_ch_was_ws: false,
            bom: 0,
            buf: vec![],
            pos: 0,
            partial: true,
            dead: false,
            streaming: false,
            stream_errors: false,
            path: vec![],
            last_seen: Last::None,
            output: None,
        }
    }

    pub fn new_streaming(seq: bool, errors: bool) -> Parser {
        let mut p = Parser::new(seq);
        p.streaming = true;
        p.stream_errors = errors;
        p
    }

    fn make_error(&self, msg: String) -> Next {
        if self.stream_errors {
            return Next::Value(Value::arr(vec![Value::string(msg), Value::arr(self.path.clone())]));
        }
        Next::Error(msg)
    }

    /// Adds input; `last` when it is the end of it.
    pub fn feed(&mut self, data: &[u8], last: bool) {
        let mut d = data;
        const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
        while !d.is_empty() && self.bom < 3 {
            if d[0] == BOM[self.bom] {
                d = &d[1..];
                self.bom += 1;
            } else if self.bom == 0 {
                self.bom = 3;
            } else {
                self.bom = 0xff;
                break;
            }
        }
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        self.buf.extend_from_slice(d);
        self.partial = !last;
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.next = None;
        self.token.clear();
        self.st = St::Normal;
        self.path.clear();
        self.last_seen = Last::None;
        self.output = None;
    }

    fn value(&mut self, v: Value) -> R {
        if self.streaming {
            if self.next.is_some() || self.last_seen == Last::Value {
                return Err("Expected separator between values");
            }
            self.last_seen = if self.path.is_empty() { Last::None } else { Last::Value };
        } else if self.next.is_some() {
            return Err("Expected separator between values");
        }
        self.next = Some(v);
        Ok(())
    }

    fn stream_token(&mut self, ch: u8) -> R {
        let top_is_num = |p: &Parser| matches!(p.path.last(), Some(Value::Num(_)));
        match ch {
            b'[' => {
                if self.next.is_some() {
                    return Err("Expected a separator between values");
                }
                if self.last_seen == Last::OpenObject {
                    return Err("Expected string key after '{', not '['");
                }
                if self.last_seen == Last::Comma && !top_is_num(self) {
                    return Err("Expected string key after ',' in object, not '['");
                }
                self.path.push(Value::num(0.0));
                self.last_seen = Last::OpenArray;
            }
            b'{' => {
                if self.last_seen == Last::Value {
                    return Err("Expected a separator between values");
                }
                if self.last_seen == Last::OpenObject {
                    return Err("Expected string key after '{', not '{'");
                }
                if self.last_seen == Last::Comma && !top_is_num(self) {
                    return Err("Expected string key after ',' in object, not '{'");
                }
                self.path.push(Value::Null);
                self.last_seen = Last::OpenObject;
            }
            b':' => {
                if self.path.is_empty() || top_is_num(self) {
                    return Err("':' not as part of an object");
                }
                if self.next.is_none() || self.last_seen == Last::None {
                    return Err("Expected string key before ':'");
                }
                if !matches!(self.next, Some(Value::Str(_))) {
                    return Err("Object keys must be strings");
                }
                if self.last_seen != Last::Value {
                    return Err("':' should follow a key");
                }
                self.last_seen = Last::Colon;
                let k = self.next.take().unwrap();
                *self.path.last_mut().unwrap() = k;
            }
            b',' => {
                if self.last_seen != Last::Value {
                    return Err("Expected value before ','");
                }
                let Some(last) = self.path.last().cloned() else { return Err("',' not as part of an object or array") };
                match last {
                    Value::Num(n) => {
                        if let Some(v) = self.next.take() {
                            self.output = Some(vec![Value::arr(self.path.clone()), v]);
                        }
                        *self.path.last_mut().unwrap() = Value::num(n.f + 1.0);
                        self.last_seen = Last::Comma;
                    }
                    Value::Str(_) => {
                        if let Some(v) = self.next.take() {
                            self.output = Some(vec![Value::arr(self.path.clone()), v]);
                        }
                        *self.path.last_mut().unwrap() = Value::Null;
                        self.last_seen = Last::Comma;
                    }
                    _ => return Err("Objects must consist of key:value pairs"),
                }
            }
            b']' => {
                if self.path.is_empty() {
                    return Err("Unmatched ']' at the top-level");
                }
                if self.last_seen == Last::Comma {
                    return Err("Expected another array element");
                }
                if !top_is_num(self) {
                    return Err("Unmatched ']' in the middle of an object");
                }
                if let Some(v) = self.next.take() {
                    self.output = Some(vec![Value::arr(self.path.clone()), v, Value::Bool(true)]);
                } else if self.last_seen != Last::OpenArray {
                    self.output = Some(vec![Value::arr(self.path.clone())]);
                }
                self.path.pop();
                if self.last_seen == Last::OpenArray {
                    self.output = Some(vec![Value::arr(self.path.clone()), Value::arr(vec![])]);
                }
                self.last_seen = if self.path.is_empty() { Last::None } else { Last::Value };
            }
            b'}' => {
                if self.path.is_empty() {
                    return Err("Unmatched '}' at the top-level");
                }
                if self.last_seen == Last::Comma {
                    return Err("Expected another key:value pair");
                }
                let last = self.path.last().unwrap().clone();
                if matches!(last, Value::Num(_)) {
                    return Err("Unmatched '}' in the middle of an array");
                }
                if let Some(v) = self.next.take() {
                    if !matches!(last, Value::Str(_)) {
                        self.next = Some(v);
                        return Err("Objects must consist of key:value pairs");
                    }
                    self.output = Some(vec![Value::arr(self.path.clone()), v, Value::Bool(true)]);
                } else {
                    match self.last_seen {
                        Last::Colon => return Err("Missing value in key:value pair"),
                        Last::Comma => return Err("Expected another key-value pair"),
                        Last::OpenArray => return Err("Unmatched '}' in the middle of an array"),
                        Last::Value | Last::OpenObject => {}
                        _ => return Err("Unmatched '}'"),
                    }
                    if self.last_seen != Last::OpenObject {
                        self.output = Some(vec![Value::arr(self.path.clone())]);
                    }
                }
                self.path.pop();
                if self.last_seen == Last::OpenObject {
                    self.output = Some(vec![Value::arr(self.path.clone()), Value::obj(Map::new())]);
                }
                self.last_seen = if self.path.is_empty() { Last::None } else { Last::Value };
            }
            _ => {}
        }
        Ok(())
    }

    fn stream_done(&mut self) -> Option<Value> {
        if self.path.is_empty() && self.next.is_some() {
            let v = self.next.take().unwrap();
            return Some(Value::arr(vec![Value::arr(vec![]), v]));
        }
        let out = self.output.take()?;
        if out.len() > 2 {
            self.output = Some(vec![out[0].clone()]);
            return Some(Value::arr(out[..2].to_vec()));
        }
        Some(Value::arr(out))
    }

    fn token_ch(&mut self, ch: u8) -> R {
        if self.streaming {
            return self.stream_token(ch);
        }
        match ch {
            b'[' | b'{' => {
                if self.stack.len() >= MAX_DEPTH {
                    return Err("Exceeds depth limit for parsing");
                }
                if self.next.is_some() {
                    return Err("Expected separator between values");
                }
                self.stack.push(if ch == b'[' { Frame::Arr(vec![]) } else { Frame::Obj(Map::new()) });
            }
            b':' => {
                let Some(n) = &self.next else { return Err("Expected string key before ':'") };
                if !matches!(self.stack.last(), Some(Frame::Obj(_))) {
                    return Err("':' not as part of an object");
                }
                let Value::Str(k) = n else { return Err("Object keys must be strings") };
                let k = k.clone();
                self.next = None;
                self.stack.push(Frame::Key(k));
            }
            b',' => {
                let Some(n) = self.next.take() else { return Err("Expected value before ','") };
                match self.stack.last_mut() {
                    None => {
                        self.next = Some(n);
                        return Err("',' not as part of an object or array");
                    }
                    Some(Frame::Arr(a)) => a.push(n),
                    Some(Frame::Key(_)) => {
                        let Some(Frame::Key(k)) = self.stack.pop() else { unreachable!() };
                        if let Some(Frame::Obj(m)) = self.stack.last_mut() {
                            m.insert(k, n);
                        }
                    }
                    Some(Frame::Obj(_)) => {
                        self.next = Some(n);
                        return Err("Objects must consist of key:value pairs");
                    }
                }
            }
            b']' => {
                let Some(Frame::Arr(_)) = self.stack.last() else { return Err("Unmatched ']'") };
                let Some(Frame::Arr(mut a)) = self.stack.pop() else { unreachable!() };
                match self.next.take() {
                    Some(n) => a.push(n),
                    None => {
                        if !a.is_empty() {
                            self.stack.push(Frame::Arr(a));
                            return Err("Expected another array element");
                        }
                    }
                }
                self.next = Some(Value::arr(a));
            }
            b'}' => {
                if self.stack.is_empty() {
                    return Err("Unmatched '}'");
                }
                match self.next.take() {
                    Some(n) => {
                        let Some(Frame::Key(_)) = self.stack.last() else {
                            self.next = Some(n);
                            return Err("Objects must consist of key:value pairs");
                        };
                        let Some(Frame::Key(k)) = self.stack.pop() else { unreachable!() };
                        if let Some(Frame::Obj(m)) = self.stack.last_mut() {
                            m.insert(k, n);
                        }
                    }
                    None => match self.stack.last() {
                        Some(Frame::Obj(m)) => {
                            if !m.is_empty() {
                                return Err("Expected another key-value pair");
                            }
                        }
                        _ => return Err("Unmatched '}'"),
                    },
                }
                let Some(Frame::Obj(m)) = self.stack.pop() else { return Err("Unmatched '}'") };
                self.next = Some(Value::obj(m));
            }
            _ => {}
        }
        Ok(())
    }

    fn check_literal(&mut self) -> R {
        if self.token.is_empty() {
            return Ok(());
        }
        let t = std::mem::take(&mut self.token);
        let pattern: Option<(&[u8], Value)> = match t[0] {
            b't' => Some((b"true", Value::Bool(true))),
            b'f' => Some((b"false", Value::Bool(false))),
            b'\'' => return Err("Invalid string literal; expected \", but got '"),
            b'n' if t.len() > 1 && t[1] == b'u' => Some((b"null", Value::Null)),
            _ => None,
        };
        if let Some((p, v)) = pattern {
            if t != p {
                return Err("Invalid literal");
            }
            return self.value(v);
        }
        match parse_number(&t) {
            Some(n) => self.value(Value::Num(n)),
            None => Err("Invalid numeric literal"),
        }
    }

    fn found_string(&mut self) -> R {
        let t = std::mem::take(&mut self.token);
        let mut out: Vec<u8> = Vec::with_capacity(t.len());
        let mut i = 0;
        while i < t.len() {
            let c = t[i];
            i += 1;
            if c == b'\\' {
                if i >= t.len() {
                    return Err("Expected escape character at end of string");
                }
                let e = t[i];
                i += 1;
                match e {
                    b'\\' | b'"' | b'/' => out.push(e),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b'u' => {
                        if i + 4 > t.len() {
                            return Err("Invalid \\uXXXX escape");
                        }
                        let Some(mut cp) = unhex4(&t[i..i + 4]) else { return Err("Invalid characters in \\uXXXX escape") };
                        i += 4;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if i + 6 > t.len() || t[i] != b'\\' || t[i + 1] != b'u' {
                                return Err("Invalid \\uXXXX\\uXXXX surrogate pair escape");
                            }
                            let s = unhex4(&t[i + 2..i + 6]).unwrap_or(0);
                            if !(0xDC00..=0xDFFF).contains(&s) {
                                return Err("Invalid \\uXXXX\\uXXXX surrogate pair escape");
                            }
                            i += 6;
                            cp = 0x10000 + (((cp - 0xD800) << 10) | (s - 0xDC00));
                        }
                        let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                        let mut b = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                    }
                    _ => return Err("Invalid escape"),
                }
            } else {
                if c < 0x20 {
                    return Err("Invalid string: control characters from U+0000 through U+001F must be escaped");
                }
                out.push(c);
            }
        }
        self.value(Value::Str(Rc::from(crate::value::utf8_lossy(&out).as_str())))
    }

    fn done(&mut self) -> Option<Value> {
        if self.streaming {
            return self.stream_done();
        }
        if self.stack.is_empty() {
            self.next.take()
        } else {
            None
        }
    }

    fn scan(&mut self, ch: u8) -> Result<Option<Value>, &'static str> {
        self.column += 1;
        if ch == b'\n' {
            self.line += 1;
            self.column = 0;
        }
        if self.seq && ch == 0x1e {
            let truncated = if self.streaming {
                !self.path.is_empty() || matches!(self.next, Some(Value::Num(_) | Value::Bool(_) | Value::Null))
            } else {
                !self.last_ch_was_ws && (!self.stack.is_empty() || !self.token.is_empty() || matches!(self.next, Some(Value::Num(_))))
            };
            if truncated {
                let top_num = self.check_literal().is_ok() && self.stack.is_empty() && self.path.is_empty() && matches!(self.next, Some(Value::Num(_)));
                return Err(if top_num { "Potentially truncated top-level numeric value" } else { "Truncated value" });
            }
            self.check_literal()?;
            if self.st == St::Normal {
                if let Some(v) = self.done() {
                    return Ok(Some(v));
                }
            }
            self.reset();
            return Ok(None);
        }
        let mut out = None;
        self.last_ch_was_ws = false;
        if self.st == St::Normal {
            let cls = match ch {
                0 => 4,
                b' ' | b'\t' | b'\r' | b'\n' => 1,
                b'"' => 3,
                b'[' | b',' | b']' | b'{' | b':' | b'}' => 2,
                _ => 0,
            };
            if cls == 1 {
                self.last_ch_was_ws = true;
            }
            if cls != 0 {
                self.check_literal()?;
                if let Some(v) = self.done() {
                    out = Some(v);
                }
            }
            match cls {
                0 => self.token.push(ch),
                1 => {}
                3 => self.st = St::Str,
                2 => self.token_ch(ch)?,
                _ => return Err("Invalid character"),
            }
            if out.is_none() {
                if let Some(v) = self.done() {
                    out = Some(v);
                }
            }
        } else if ch == b'"' && self.st == St::Str {
            self.found_string()?;
            self.st = St::Normal;
            if let Some(v) = self.done() {
                out = Some(v);
            }
        } else {
            self.token.push(ch);
            self.st = if ch == b'\\' && self.st == St::Str { St::StrEscape } else { St::Str };
        }
        Ok(out)
    }

    pub fn next(&mut self) -> Next {
        if self.eof || self.dead {
            return Next::None;
        }
        if self.bom == 0xff {
            if !self.seq {
                self.dead = true;
                return Next::Error("Malformed BOM".into());
            }
            self.st = St::WaitRs;
            self.reset();
            self.bom = 3;
        }
        if self.streaming {
            if let Some(v) = self.stream_done() {
                return Next::Value(v);
            }
        }
        let mut ch = 0u8;
        while self.pos < self.buf.len() {
            ch = self.buf[self.pos];
            self.pos += 1;
            if self.st == St::WaitRs {
                if ch == b'\n' {
                    self.line += 1;
                    self.column = 0;
                } else {
                    self.column += 1;
                }
                if ch == 0x1e {
                    self.st = St::Normal;
                }
                continue;
            }
            match self.scan(ch) {
                Ok(Some(v)) => return Next::Value(v),
                Ok(None) => {}
                Err(msg) => {
                    if ch != 0x1e && self.seq {
                        let e = self.make_error(format!("{} at line {}, column {} (need RS to resync)", msg, self.line, self.column));
                        self.reset();
                        self.st = St::WaitRs;
                        return e;
                    }
                    let e = self.make_error(format!("{} at line {}, column {}", msg, self.line, self.column));
                    self.reset();
                    if !self.seq {
                        self.dead = true;
                    }
                    return e;
                }
            }
        }
        let _ = ch;
        if self.partial {
            return Next::None;
        }
        self.eof = true;
        if self.st == St::WaitRs {
            return self.make_error(format!("Unfinished abandoned text at EOF at line {}, column {}", self.line, self.column));
        }
        if self.st != St::Normal {
            let e = self.make_error(format!("Unfinished string at EOF at line {}, column {}", self.line, self.column));
            self.reset();
            return e;
        }
        if let Err(msg) = self.check_literal() {
            let e = self.make_error(format!("{} at EOF at line {}, column {}", msg, self.line, self.column));
            self.reset();
            return e;
        }
        if !self.stack.is_empty() || !self.path.is_empty() {
            let e = self.make_error(format!("Unfinished JSON term at EOF at line {}, column {}", self.line, self.column));
            self.reset();
            return e;
        }
        if self.streaming {
            if let Some(v) = self.next.take() {
                return Next::Value(Value::arr(vec![Value::arr(vec![]), v]));
            }
            return Next::None;
        }
        match self.next.take() {
            Some(v) => {
                if self.seq && !self.last_ch_was_ws && matches!(v, Value::Num(_)) {
                    return Next::Error(format!("Potentially truncated top-level numeric value at EOF at line {}, column {}", self.line, self.column));
                }
                Next::Value(v)
            }
            None => Next::None,
        }
    }
}

fn unhex4(h: &[u8]) -> Option<u32> {
    let mut r = 0u32;
    for &c in h {
        let n = (c as char).to_digit(16)?;
        r = (r << 4) | n;
    }
    Some(r)
}

/// A number literal as decNumber reads it: [+-]digits[.digits][e[+-]digits], also "Infinity",
/// "Inf" and "NaN" in any case (NaN without a payload).
pub fn parse_number(t: &[u8]) -> Option<Num> {
    let s = std::str::from_utf8(t).ok()?;
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let lower = body.to_ascii_lowercase();
    if lower == "nan" {
        return Some(Num::f(f64::NAN));
    }
    if lower == "inf" || lower == "infinity" {
        return Some(Num::f(if neg { f64::NEG_INFINITY } else { f64::INFINITY }));
    }
    if !body.as_bytes().first().map_or(false, |c| c.is_ascii_digit() || *c == b'.') {
        return None;
    }
    let text = if neg { format!("-{body}") } else { body.to_string() };
    Num::from_literal(&text)
}

/// One JSON text (fromjson): exactly one value, errors with " (while parsing '...')" added by
/// the caller.
pub fn parse_one(s: &str) -> Result<Value, String> {
    let mut p = Parser::new(false);
    p.feed(s.as_bytes(), true);
    match p.next() {
        Next::Value(v) => match p.next() {
            Next::Value(_) => Err("Unexpected extra JSON values".into()),
            Next::Error(e) => Err(e),
            Next::None => Ok(v),
        },
        Next::Error(e) => Err(e),
        Next::None => Err("Expected JSON value".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn all(s: &str) -> Vec<String> {
        let mut p = Parser::new(false);
        p.feed(s.as_bytes(), true);
        let mut out = vec![];
        loop {
            match p.next() {
                Next::Value(v) => out.push(crate::value::dump(&v, &crate::value::Fmt::compact())),
                Next::Error(e) => {
                    out.push(format!("!{e}"));
                    break;
                }
                Next::None => break,
            }
        }
        out
    }
    #[test]
    fn parse() {
        assert_eq!(all("1 [2,{\"a\":3}] \"x\\u00e9\""), ["1", "[2,{\"a\":3}]", "\"xé\""]);
        assert_eq!(all("0x10\n"), ["!Invalid numeric literal at line 2, column 0"]);
        assert_eq!(all("[1,2"), ["!Unfinished JSON term at EOF at line 1, column 4"]);
        assert_eq!(all("NaN1"), ["!Invalid numeric literal at EOF at line 1, column 4"]);
        assert_eq!(all("{'a': 123}"), ["!Invalid string literal; expected \", but got ' at line 1, column 5"]);
        assert_eq!(all("1.000 1E2 .5"), ["1.000", "1E+2", "0.5"]);
    }
}
