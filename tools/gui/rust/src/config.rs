//! Where the server lives and what it allows. The settings come from the app that started the
//! sandbox (`start.config.gui`, `--gui-config KEY=VALUE`; the engine writes them to
//! /etc/collabo/gui.json):
//!
//!   defaultSize  "1024x768"   a canvas's size when its program does not ask for one
//!   maxSize      "1920x1080"  no canvas grows beyond this (programs and `gui resize` alike)
//!   maxCanvases  8            canvases one program may hold at once
//!   maxMemoryMB  256          all canvases' pixels together
//!   capture      true         `gui screenshot` / `gui view` may read a canvas
//!   input        true         `gui click` / `key` / `type` / ... may send input
//!   clipboard    true         `gui clipboard` may read and set the clipboard

use std::path::PathBuf;

pub const MIN_SIDE: u32 = 16;
/// The hard ceiling, whatever the settings say: 8192 x 8192 x 4 bytes is 256 MiB.
pub const MAX_SIDE: u32 = 8192;

#[derive(Clone, Debug)]
pub struct Config {
    pub default_size: (u32, u32),
    pub max_size: (u32, u32),
    pub max_canvases: u32,
    pub max_memory: usize,
    pub capture: bool,
    pub input: bool,
    pub clipboard: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            default_size: (1024, 768),
            max_size: (1920, 1080),
            max_canvases: 8,
            max_memory: 256 << 20,
            capture: true,
            input: true,
            clipboard: true,
        }
    }
}

/// The server's folder: socket, lock, logs. `COLLABO_GUI_DIR` moves it (tests, a second display).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("COLLABO_GUI_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from("/tmp/.collabo-gui"),
    }
}

/// The socket a program connects to: `COLLABO_GUI_SOCKET`, else the runtime folder's.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("COLLABO_GUI_SOCKET") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => runtime_dir().join("socket"),
    }
}

pub fn settings_path() -> PathBuf {
    match std::env::var_os("COLLABO_GUI_CONFIG") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => PathBuf::from("/etc/collabo/gui.json"),
    }
}

pub fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

impl Config {
    /// The settings file, if there is one; a missing file is the defaults, a bad value is
    /// reported and skipped.
    pub fn load() -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();
        let path = settings_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return (cfg, warnings),
        };
        let pairs = match flat_json(&text) {
            Some(p) => p,
            None => {
                warnings.push(format!("{}: not a JSON object; using the defaults", path.display()));
                return (cfg, warnings);
            }
        };
        for (key, value) in pairs {
            let ok = match key.as_str() {
                "defaultSize" => value.as_size().map(|s| cfg.default_size = s).is_some(),
                "maxSize" => value.as_size().map(|s| cfg.max_size = s).is_some(),
                "maxCanvases" => value.as_u64().filter(|n| *n >= 1).map(|n| cfg.max_canvases = n.min(1024) as u32).is_some(),
                "maxMemoryMB" => value.as_u64().filter(|n| *n >= 1).map(|n| cfg.max_memory = (n.min(16384) as usize) << 20).is_some(),
                "capture" => value.as_bool().map(|b| cfg.capture = b).is_some(),
                "input" => value.as_bool().map(|b| cfg.input = b).is_some(),
                "clipboard" => value.as_bool().map(|b| cfg.clipboard = b).is_some(),
                _ => true, // a newer setting this version does not know
            };
            if !ok {
                warnings.push(format!("{}: ignoring {key} = {}", path.display(), value.raw()));
            }
        }
        cfg.normalize();
        (cfg, warnings)
    }

    pub fn normalize(&mut self) {
        let side = |v: u32| v.clamp(MIN_SIDE, MAX_SIDE);
        self.max_size = (side(self.max_size.0), side(self.max_size.1));
        self.default_size = (side(self.default_size.0).min(self.max_size.0), side(self.default_size.1).min(self.max_size.1));
    }

    /// A requested size brought within the limits; 0 means "the default" for that side.
    pub fn clamp(&self, w: u32, h: u32) -> (u32, u32) {
        let w = if w == 0 { self.default_size.0 } else { w };
        let h = if h == 0 { self.default_size.1 } else { h };
        (w.clamp(MIN_SIDE, self.max_size.0), h.clamp(MIN_SIDE, self.max_size.1))
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Other(String),
}

impl Value {
    fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Str(s) => match s.as_str() {
                "true" | "yes" | "on" | "allow" | "1" => Some(true),
                "false" | "no" | "off" | "deny" | "0" => Some(false),
                _ => None,
            },
            Value::Num(n) => Some(*n != 0.0),
            Value::Other(_) => None,
        }
    }
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    fn as_size(&self) -> Option<(u32, u32)> {
        match self {
            Value::Str(s) => parse_size(s),
            _ => None,
        }
    }
    fn raw(&self) -> String {
        match self {
            Value::Str(s) => format!("{s:?}"),
            Value::Num(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Other(s) => s.clone(),
        }
    }
}

/// The top-level members of a JSON object, with nested values kept as their text. Enough for a
/// settings file, without a JSON library in the binary.
pub fn flat_json(text: &str) -> Option<Vec<(String, Value)>> {
    let b = text.as_bytes();
    let mut p = Parser { b, i: 0 };
    p.ws();
    p.eat(b'{')?;
    let mut out = Vec::new();
    p.ws();
    if p.peek() == Some(b'}') {
        return Some(out);
    }
    loop {
        p.ws();
        let key = p.string()?;
        p.ws();
        p.eat(b':')?;
        p.ws();
        let start = p.i;
        let value = match p.peek()? {
            b'"' => Value::Str(p.string()?),
            b't' | b'f' => {
                let v = p.word()?;
                Value::Bool(match v { "true" => true, "false" => false, _ => return None })
            }
            b'-' | b'0'..=b'9' => Value::Num(p.word()?.parse().ok()?),
            _ => {
                p.skip()?;
                Value::Other(text[start..p.i].to_string())
            }
        };
        out.push((key, value));
        p.ws();
        match p.peek()? {
            b',' => p.i += 1,
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }
    fn word(&mut self) -> Option<&'a str> {
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.')) {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i]).ok()
    }
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                            self.i += 4;
                            let mut cp = u32::from_str_radix(hex, 16).ok()?;
                            if (0xd800..0xdc00).contains(&cp) && self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                let lo = u32::from_str_radix(std::str::from_utf8(self.b.get(self.i + 2..self.i + 6)?).ok()?, 16).ok()?;
                                self.i += 6;
                                cp = 0x10000 + ((cp - 0xd800) << 10) + (lo.wrapping_sub(0xdc00) & 0x3ff);
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
    }
    /// Any value, nested or not.
    fn skip(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.string().map(|_| ()),
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            _ => self.word().filter(|w| !w.is_empty()).map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let v = flat_json(r#"{"maxSize": "800x600", "capture": false, "nested": {"a": [1, "}"]}, "maxCanvases": 3, "s": "é\n"}"#).unwrap();
        assert_eq!(v.len(), 5);
        assert!(matches!(&v[0].1, Value::Str(s) if s == "800x600"));
        assert!(matches!(v[1].1, Value::Bool(false)));
        assert!(matches!(&v[2].1, Value::Other(s) if s == r#"{"a": [1, "}"]}"#));
        assert!(matches!(&v[4].1, Value::Str(s) if s == "é\n"));
        assert!(flat_json("[1]").is_none());
        assert!(flat_json("{}").unwrap().is_empty());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("640x480"), Some((640, 480)));
        assert_eq!(parse_size("0x480"), None);
        let mut c = Config { max_size: (100_000, 10), default_size: (5000, 5000), ..Config::default() };
        c.normalize();
        assert_eq!(c.max_size, (MAX_SIDE, MIN_SIDE));
        assert_eq!(c.default_size, (5000, MIN_SIDE));
        assert_eq!(Config::default().clamp(0, 99999), (1024, 1080));
    }
}
