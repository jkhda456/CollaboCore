//! LFS pointers (lfs/pointer.go): the `version / oid / size` text, extensions, and the
//! parse errors git-lfs reports.

use crate::errors::{Error, Kind, Result};
use crate::fs::EMPTY_OBJECT_SHA256;

pub const LATEST: &str = "https://git-lfs.github.com/spec/v1";
pub const V1_ALIASES: &[&str] = &["http://git-media.io/v/2", "https://hawser.github.com/spec/v1", "https://git-lfs.github.com/spec/v1"];
pub const BLOB_SIZE_CUTOFF: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Extension {
    pub name: String,
    pub priority: i64,
    pub oid: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Pointer {
    pub oid: String,
    pub size: i64,
    pub extensions: Vec<Extension>,
    pub canonical: bool,
}

impl Pointer {
    pub fn new(oid: &str, size: i64, extensions: Vec<Extension>) -> Pointer {
        Pointer { oid: oid.to_string(), size, extensions, canonical: true }
    }

    pub fn empty() -> Pointer {
        Pointer::new(EMPTY_OBJECT_SHA256, 0, vec![])
    }

    /// The pointer text (empty for an empty file).
    pub fn encoded(&self) -> String {
        if self.size == 0 {
            return String::new();
        }
        let mut s = format!("version {LATEST}\n");
        for e in &self.extensions {
            s.push_str(&format!("ext-{}-{} sha256:{}\n", e.priority, e.name, e.oid));
        }
        s.push_str(&format!("oid sha256:{}\nsize {}\n", self.oid, self.size));
        s
    }
}

fn not_a_pointer(msg: impl Into<String>) -> Error {
    Error::not_a_pointer(Error::new(msg))
}

fn is_oid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_oid(value: &str) -> Result<String> {
    let Some((typ, oid)) = value.split_once(':') else { return Err(Error::new(format!("Invalid OID value: {value}"))) };
    if typ != "sha256" {
        return Err(Error::new(format!("Invalid OID type: {typ}")));
    }
    if !is_oid(oid) {
        return Err(Error::new(format!("Invalid OID: {oid}")));
    }
    Ok(oid.to_string())
}

/// DecodeFrom on a pointer-sized prefix: an empty input is the empty pointer.
pub fn decode(buf: &[u8]) -> Result<Pointer> {
    if buf.is_empty() {
        return Ok(Pointer::empty());
    }
    let trimmed = trim_space(buf);
    let mut p = decode_kv(trimmed)?;
    p.canonical = p.encoded().as_bytes() == buf;
    Ok(p)
}

fn trim_space(b: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let s = b.iter().position(|c| !ws(c)).unwrap_or(b.len());
    let e = b.iter().rposition(|c| !ws(c)).map_or(s, |i| i + 1);
    &b[s..e.max(s)]
}

fn decode_kv(data: &[u8]) -> Result<Pointer> {
    let text = String::from_utf8_lossy(data);
    if !(text.contains("git-media") || text.contains("hawser") || text.contains("git-lfs")) {
        return Err(not_a_pointer("invalid header"));
    }
    let keys = ["version", "oid", "size"];
    let mut kv: Vec<(String, String)> = vec![];
    let mut exts: Vec<(String, String)> = vec![];
    let mut line = 0;
    let ext_re = regex::Regex::new(r"\Aext-\d{1}-\w+").unwrap();
    for t in text.split('\n') {
        let t = t.strip_suffix('\r').unwrap_or(t);
        if t.is_empty() {
            continue;
        }
        let Some((key, value)) = t.split_once(' ') else {
            return Err(not_a_pointer(format!("error reading line {line}: {t}")));
        };
        if line >= keys.len() {
            return Err(not_a_pointer(format!("extra line: {t}")));
        }
        let expected = keys[line];
        if key != expected {
            if !ext_re.is_match(key) {
                // BadPointerKeyError: a missing version makes it "not a pointer".
                let e = Error::new(format!("Expected key {expected}, got {key}")).wrap("pointer parsing").with_kind(Kind::BadPointerKey);
                return Err(if expected == "version" { Error::not_a_pointer(e) } else { e });
            }
            if let Some(x) = exts.iter_mut().find(|(k, _)| k == key) {
                x.1 = value.to_string();
            } else {
                exts.push((key.to_string(), value.to_string()));
            }
            continue;
        }
        line += 1;
        kv.push((key.to_string(), value.to_string()));
    }
    let get = |k: &str| kv.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    let version = get("version").unwrap_or_default();
    if version.is_empty() {
        return Err(not_a_pointer("Missing version"));
    }
    if !V1_ALIASES.contains(&version.as_str()) {
        return Err(Error::new(format!("Invalid version: {version}")));
    }
    let Some(oidv) = get("oid") else { return Err(Error::new("Invalid OID")) };
    let oid = parse_oid(&oidv)?;
    let sizev = get("size").unwrap_or_default();
    let size: i64 = match sizev.parse::<i64>() {
        Ok(n) if n >= 0 => n,
        _ => return Err(Error::new(format!("invalid size: {}", crate::tools::quote(&sizev)))),
    };
    let mut extensions = vec![];
    for (k, v) in exts {
        let parts: Vec<&str> = k.splitn(3, '-').collect();
        if parts.len() != 3 || parts[0] != "ext" {
            return Err(Error::new(format!("Invalid extension value: {v}")));
        }
        let p: i64 = match parts[1].parse() {
            Ok(n) if n >= 0 => n,
            _ => return Err(Error::new(format!("Invalid priority: {}", parts[1]))),
        };
        let oid = parse_oid(&v)?;
        extensions.push(Extension { name: parts[2].to_string(), priority: p, oid });
    }
    for (i, e) in extensions.iter().enumerate() {
        if extensions[..i].iter().any(|x| x.priority == e.priority) {
            return Err(Error::new(format!("duplicate priority found: {}", e.priority)));
        }
    }
    extensions.sort_by_key(|e| e.priority);
    Ok(Pointer::new(&oid, size, extensions))
}

/// DecodePointerFromFile.
pub fn decode_from_file(path: &str) -> Result<Pointer> {
    let m = std::fs::symlink_metadata(path).map_err(|e| crate::tools::path_err("lstat", path, &e))?;
    if !m.is_file() {
        return Err(Error::new(format!("not a regular file: {}", crate::tools::quote(path))));
    }
    if m.len() as usize >= BLOB_SIZE_CUTOFF {
        return Err(Error::not_a_pointer(Error::new("file size exceeds Git LFS pointer size cutoff")));
    }
    let data = std::fs::read(path).map_err(|e| crate::tools::path_err("open", path, &e))?;
    decode(&data)
}
