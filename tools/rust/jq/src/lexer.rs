//! jq's tokens (lexer.l): keywords, operators, .field, $name, @format, numbers, and strings
//! with \(...) interpolation (a string's interpolated queries are lexed in place, as jq's
//! start-condition stack does). Every token carries its byte span, for error messages.

use crate::value::{Num, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Field(String),
    Binding(String),
    Format(String),
    Literal(Value),
    /// A string: its parts (text, or the tokens of an interpolated query).
    Str(Vec<StrPart>),
    Kw(&'static str),
    Op(&'static str),
    Loc,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Text(String),
    Interp(Vec<Token>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

pub const KEYWORDS: &[&str] = &[
    "as", "import", "include", "module", "def", "if", "then", "else", "elif", "and", "or", "end", "reduce", "foreach", "try", "catch", "label", "break",
];

const OPS: &[&str] = &[
    "!=", "==", "//=", "//", "|=", "+=", "-=", "*=", "/=", "%=", "<=", ">=", "..", ".", "?", "=", ";", ",", ":", "|", "+", "-", "*", "/", "%", "$", "<", ">", "[",
    "]", "{", "}", "(", ")",
];

/// A lexing error: message and the span it is about.
pub type LexErr = (String, usize, usize);

pub struct Lexer<'a> {
    s: &'a [u8],
    pub pos: usize,
    /// The last lexeme (token, whitespace run or comment): bison places the end of file there.
    last: (usize, usize),
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

impl<'a> Lexer<'a> {
    pub fn new(s: &'a str) -> Self {
        Lexer { s: s.as_bytes(), pos: 0, last: (0, 0) }
    }

    fn skip_ws(&mut self) {
        loop {
            let ws = self.pos;
            while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            }
            if self.pos > ws {
                self.last = (ws, self.pos);
            }
            if self.pos < self.s.len() && self.s[self.pos] == b'#' {
                let c0 = self.pos;
                // To the end of the line; a backslash escapes a backslash or the newline.
                self.pos += 1;
                while self.pos < self.s.len() {
                    let c = self.s[self.pos];
                    if c == b'\\' && self.pos + 1 < self.s.len() {
                        let n = self.s[self.pos + 1];
                        if n == b'\\' || n == b'\n' {
                            self.pos += 2;
                            continue;
                        }
                        if n == b'\r' && self.pos + 2 < self.s.len() && self.s[self.pos + 2] == b'\n' {
                            self.pos += 3;
                            continue;
                        }
                    }
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
                self.last = (c0, self.pos);
                continue;
            }
            break;
        }
    }

    /// An identifier with optional `mod::` prefixes.
    fn ident(&mut self) -> String {
        let start = self.pos;
        loop {
            while self.pos < self.s.len() && is_ident(self.s[self.pos]) {
                self.pos += 1;
            }
            if self.pos + 2 < self.s.len() && self.s[self.pos] == b':' && self.s[self.pos + 1] == b':' && is_ident_start(self.s[self.pos + 2]) {
                self.pos += 2;
                continue;
            }
            break;
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()
    }

    /// Tokens up to the end, or (in an interpolation) up to its closing parenthesis.
    pub fn tokens(&mut self, interp: bool) -> Result<Vec<Token>, LexErr> {
        let mut out: Vec<Token> = vec![];
        let mut depth = 0i32;
        loop {
            if let Some(t) = out.last() {
                if t.end > self.last.1 {
                    self.last = (t.start, t.end);
                }
            }
            self.skip_ws();
            let at = self.pos;
            let push = |out: &mut Vec<Token>, tok: Tok, end: usize| out.push(Token { tok, start: at, end });
            if self.pos >= self.s.len() {
                let (a, b) = self.last;
                if interp {
                    let complete = !matches!(out.last().map(|t: &Token| &t.tok), None | Some(Tok::Op(_)) | Some(Tok::Kw(_))) || matches!(out.last().map(|t| &t.tok), Some(Tok::Op(")" | "]" | "}" | "." | "..")));
                    let msg = if complete { "syntax error, unexpected end of file, expecting QQSTRING_INTERP_END or '|' or ','" } else { "syntax error, unexpected end of file" };
                    return Err((msg.into(), a, b));
                }
                out.push(Token { tok: Tok::Eof, start: a, end: b });
                return Ok(out);
            }
            let c = self.s[self.pos];
            if c == b'"' {
                self.pos += 1;
                self.last = (at, self.pos);
                let parts = self.string()?;
                push(&mut out, Tok::Str(parts), self.pos);
                self.last = (at, self.pos);
                continue;
            }
            if c == b'@' && self.pos + 1 < self.s.len() && is_ident(self.s[self.pos + 1]) {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.s.len() && is_ident(self.s[self.pos]) {
                    self.pos += 1;
                }
                push(&mut out, Tok::Format(String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()), self.pos);
                continue;
            }
            if c.is_ascii_digit() || (c == b'.' && self.pos + 1 < self.s.len() && self.s[self.pos + 1].is_ascii_digit()) {
                let start = self.pos;
                while self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                if self.pos < self.s.len() && self.s[self.pos] == b'.' {
                    self.pos += 1;
                    while self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                }
                if self.pos < self.s.len() && (self.s[self.pos] == b'e' || self.s[self.pos] == b'E') {
                    let save = self.pos;
                    self.pos += 1;
                    if self.pos < self.s.len() && (self.s[self.pos] == b'+' || self.s[self.pos] == b'-') {
                        self.pos += 1;
                    }
                    if self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
                        while self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
                            self.pos += 1;
                        }
                    } else {
                        self.pos = save;
                    }
                }
                let text = std::str::from_utf8(&self.s[start..self.pos]).unwrap();
                let n = Num::from_literal(text).unwrap_or(Num::f(0.0));
                push(&mut out, Tok::Literal(Value::Num(n)), self.pos);
                continue;
            }
            if c == b'.' && self.pos + 1 < self.s.len() && is_ident_start(self.s[self.pos + 1]) {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.s.len() && is_ident(self.s[self.pos]) {
                    self.pos += 1;
                }
                push(&mut out, Tok::Field(String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()), self.pos);
                continue;
            }
            if c == b'$' && self.pos + 1 < self.s.len() && is_ident_start(self.s[self.pos + 1]) {
                self.pos += 1;
                let name = self.ident();
                push(&mut out, if name == "__loc__" { Tok::Loc } else { Tok::Binding(name) }, self.pos);
                continue;
            }
            if is_ident_start(c) {
                let name = self.ident();
                let tok = match KEYWORDS.iter().find(|k| **k == name) {
                    Some(k) => Tok::Kw(k),
                    None => Tok::Ident(name),
                };
                push(&mut out, tok, self.pos);
                continue;
            }
            let rest = &self.s[self.pos..];
            let Some(op) = OPS.iter().find(|o| rest.starts_with(o.as_bytes())) else {
                // One whole (possibly multibyte) character.
                let mut end = self.pos + 1;
                while end < self.s.len() && (self.s[end] & 0xC0) == 0x80 {
                    end += 1;
                }
                return Err(("syntax error, unexpected INVALID_CHARACTER".into(), at, end));
            };
            self.pos += op.len();
            if interp {
                match *op {
                    "(" | "[" | "{" => depth += 1,
                    ")" if depth == 0 => return Ok(out),
                    ")" | "]" | "}" => depth -= 1,
                    _ => {}
                }
            }
            push(&mut out, Tok::Op(op), self.pos);
        }
    }

    fn string(&mut self) -> Result<Vec<StrPart>, LexErr> {
        let mut parts = vec![];
        let mut text: Vec<u8> = vec![];
        let mut text_start = self.pos;
        loop {
            if self.pos >= self.s.len() {
                let (a, b) = if self.pos > text_start { (text_start, self.pos) } else { self.last };
                return Err(("syntax error, unexpected end of file, expecting QQSTRING_TEXT or QQSTRING_INTERP_START or QQSTRING_END".into(), a, b));
            }
            let c = self.s[self.pos];
            if c == b'"' {
                self.pos += 1;
                if !text.is_empty() || parts.is_empty() {
                    parts.push(StrPart::Text(String::from_utf8_lossy(&text).into_owned()));
                }
                return Ok(parts);
            }
            if c == b'\\' && self.s.get(self.pos + 1) == Some(&b'(') {
                self.pos += 2;
                if !text.is_empty() {
                    parts.push(StrPart::Text(String::from_utf8_lossy(&text).into_owned()));
                    text.clear();
                }
                self.last = (self.pos - 2, self.pos);
                let toks = self.tokens(true)?;
                parts.push(StrPart::Interp(toks));
                self.last = (self.pos - 1, self.pos);
                text_start = self.pos;
                continue;
            }
            if c == b'\\' {
                // A run of escapes goes through the JSON parser, as jq passes it there.
                let start = self.pos;
                while self.pos < self.s.len() && self.s[self.pos] == b'\\' && self.s.get(self.pos + 1) != Some(&b'(') {
                    if self.s.get(self.pos + 1) == Some(&b'u') {
                        self.pos += 2;
                        let mut k = 0;
                        while k < 4 && self.pos < self.s.len() && self.s[self.pos].is_ascii_alphanumeric() {
                            self.pos += 1;
                            k += 1;
                        }
                    } else if self.pos + 1 < self.s.len() {
                        self.pos += 2;
                        // Keep a multibyte escaped character whole.
                        while self.pos < self.s.len() && (self.s[self.pos] & 0xC0) == 0x80 {
                            self.pos += 1;
                        }
                    } else {
                        self.pos += 1;
                    }
                }
                let run = String::from_utf8_lossy(&self.s[start..self.pos]).into_owned();
                let json = format!("\"{run}\"");
                match crate::json::parse_one(&json) {
                    Ok(Value::Str(s)) => text.extend_from_slice(s.as_bytes()),
                    Ok(_) => {}
                    Err(e) => return Err((format!("{e} (while parsing '{json}')"), start, self.pos)),
                }
                continue;
            }
            text.push(c);
            self.pos += 1;
        }
    }
}
