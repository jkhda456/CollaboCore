//! Binary property lists ("bplist00"): CFBinaryPList.c's reader (the trailer's checks, every
//! object's bounds, the depth limit, and the cycle check below depth 15) and its writer (objects
//! in the order it flattens them, strings, numbers, dates and data uniqued by value, the same
//! integer, reference and offset sizes) — so the bytes are CF's, but for the order of a
//! dictionary's entries, which CF takes from its hash table and this from the input.

use crate::value::{Dict, Value};
use crate::xml::MAX_DEPTH;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct Trailer {
    offset_int_size: usize,
    object_ref_size: usize,
    num_objects: u64,
    top_object: u64,
    offset_table_offset: u64,
}

fn sized_int(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |acc, &x| (acc << 8).wrapping_add(x as u64))
}

/// __CFBinaryPlistGetTopLevelInfo: the trailer, if it holds together, and the top object's offset.
fn top_level(b: &[u8]) -> Option<(Trailer, u64)> {
    let len = b.len() as u64;
    if len < 32 + 8 + 1 || &b[..7] != b"bplist0" {
        return None;
    }
    let t = &b[b.len() - 32..];
    let trailer = Trailer {
        offset_int_size: t[6] as usize,
        object_ref_size: t[7] as usize,
        num_objects: sized_int(&t[8..16]),
        top_object: sized_int(&t[16..24]),
        offset_table_offset: sized_int(&t[24..32]),
    };
    let long_max = i64::MAX as u64;
    if long_max < trailer.num_objects || long_max < trailer.offset_table_offset {
        return None;
    }
    if trailer.num_objects < 1 || trailer.num_objects <= trailer.top_object || trailer.offset_table_offset < 9 {
        return None;
    }
    if len - 32 <= trailer.offset_table_offset || trailer.offset_int_size < 1 || trailer.object_ref_size < 1 {
        return None;
    }
    let table_size = trailer.num_objects.checked_mul(trailer.offset_int_size as u64)?;
    if table_size < 1 {
        return None;
    }
    let total = 8u64.checked_add(trailer.offset_table_offset - 8)?.checked_add(table_size)?.checked_add(32)?;
    if len != total {
        return None;
    }
    if trailer.object_ref_size < 8 && (1u64 << (8 * trailer.object_ref_size)) <= trailer.num_objects {
        return None;
    }
    if trailer.offset_int_size < 8 && (1u64 << (8 * trailer.offset_int_size)) <= trailer.offset_table_offset {
        return None;
    }
    let table = trailer.offset_table_offset as usize;
    let max_offset = trailer.offset_table_offset - 1;
    for idx in 0..trailer.num_objects as usize {
        let at = table + idx * trailer.offset_int_size;
        if max_offset < sized_int(&b[at..at + trailer.offset_int_size]) {
            return None;
        }
    }
    let at = table + trailer.top_object as usize * trailer.offset_int_size;
    let off = sized_int(&b[at..at + trailer.offset_int_size]);
    if off < 8 || trailer.offset_table_offset <= off {
        return None;
    }
    Some((trailer, off))
}

struct Reader<'a> {
    b: &'a [u8],
    t: Trailer,
    /// Objects already made, by offset (CF's `objects`): shared, as CF shares them.
    made: HashMap<u64, Value>,
    /// Collections being made, from depth 16 down (CF's cycle check).
    open: Option<HashSet<u64>>,
}

/// What kind of object an offset held, for dictionary keys (which may not be collections).
fn is_primitive(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Dict(_) | Value::Set(_) | Value::KeyedDict(_))
}

impl Reader<'_> {
    fn objects_end(&self) -> u64 {
        self.t.offset_table_offset - 1
    }

    /// An int object (a count) at `ptr`: (value, the position after it).
    fn read_int(&self, ptr: u64) -> Option<(u64, u64)> {
        if self.objects_end() < ptr {
            return None;
        }
        let marker = self.b[ptr as usize];
        if marker & 0xf0 != 0x10 {
            return None;
        }
        let cnt = 1u64 << (marker & 0x0f);
        let start = ptr + 1;
        if self.objects_end() < (start + cnt).checked_sub(1)? {
            return None;
        }
        Some((sized_int(&self.b[start as usize..(start + cnt) as usize]), start + cnt))
    }

    /// A count in the marker's low nibble, or 0xf and an int object after it.
    fn count(&self, marker: u8, ptr: u64) -> Option<(u64, u64)> {
        match marker & 0x0f {
            0xf => {
                let (n, next) = self.read_int(ptr)?;
                (n <= i64::MAX as u64).then_some((n, next))
            }
            n => Some((n as u64, ptr)),
        }
    }

    /// CF's extent check: the last byte (start + len - 1) within the object table.
    fn fits(&self, start: u64, len: u64) -> bool {
        start as i128 + len as i128 - 1 <= self.objects_end() as i128
    }

    /// The offset of the object referenced at `ptr`.
    fn ref_at(&self, ptr: u64) -> Option<u64> {
        let size = self.t.object_ref_size as u64;
        if ptr < 8 || self.t.offset_table_offset - size < ptr {
            return None;
        }
        let r = sized_int(&self.b[ptr as usize..(ptr + size) as usize]);
        if self.t.num_objects <= r {
            return None;
        }
        let at = (self.t.offset_table_offset + r * self.t.offset_int_size as u64) as usize;
        Some(sized_int(&self.b[at..at + self.t.offset_int_size]))
    }

    /// __CFBinaryPlistCreateObjectFiltered.
    fn object(&mut self, off: u64, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(v) = self.made.get(&off) {
            return Some(v.clone());
        }
        if self.open.as_ref().is_some_and(|open| open.contains(&off)) {
            return None;
        }
        if off < 8 || self.objects_end() < off {
            return None;
        }
        let marker = self.b[off as usize];
        let body = off + 1;
        let v = match marker & 0xf0 {
            0x00 => match marker {
                0x00 => return Some(Value::Null),
                0x08 => return Some(Value::Bool(false)),
                0x09 => return Some(Value::Bool(true)),
                _ => return None,
            },
            0x10 => {
                let cnt = 1u64 << (marker & 0x0f);
                if self.objects_end() < body + cnt - 1 || 16 < cnt {
                    return None;
                }
                let n = sized_int(&self.b[body as usize..(body + cnt) as usize]);
                // 16 bytes: CF's SInt128 with high = 0 and the low eight bytes (unsigned, "Q").
                if 8 < cnt {
                    Value::UInt(n)
                } else {
                    Value::Int(n as i64)
                }
            }
            0x20 => match marker & 0x0f {
                2 => {
                    if self.objects_end() < body + 3 {
                        return None;
                    }
                    let bytes: [u8; 4] = self.b[body as usize..body as usize + 4].try_into().unwrap();
                    Value::Float(f32::from_be_bytes(bytes))
                }
                3 => {
                    if self.objects_end() < body + 7 {
                        return None;
                    }
                    let bytes: [u8; 8] = self.b[body as usize..body as usize + 8].try_into().unwrap();
                    Value::Real(f64::from_be_bytes(bytes))
                }
                _ => return None,
            },
            0x30 => {
                if marker != 0x33 || self.objects_end() < body + 7 {
                    return None;
                }
                let bytes: [u8; 8] = self.b[body as usize..body as usize + 8].try_into().unwrap();
                Value::Date(f64::from_be_bytes(bytes))
            }
            0x40 | 0x50 | 0x60 => {
                let (cnt, start) = self.count(marker, body)?;
                let bytes = if marker & 0xf0 == 0x60 { cnt.checked_mul(2)? } else { cnt };
                if !self.fits(start, bytes) {
                    return None;
                }
                let raw = &self.b[start as usize..(start + bytes) as usize];
                match marker & 0xf0 {
                    0x40 => Value::data(raw.to_vec()),
                    0x50 => {
                        // kCFStringEncodingASCII: bytes above 0x7f make no string.
                        if !raw.is_ascii() {
                            return None;
                        }
                        Value::String(String::from_utf8(raw.to_vec()).unwrap())
                    }
                    _ => {
                        let units: Vec<u16> = raw.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                        // CFString keeps lone surrogates; Rust strings cannot, so they become U+FFFD.
                        Value::String(String::from_utf16_lossy(&units))
                    }
                }
            }
            0x80 => {
                let cnt = ((marker & 0x0f) + 1) as u64;
                if self.objects_end() < body + cnt - 1 {
                    return None;
                }
                let n = sized_int(&self.b[body as usize..(body + cnt) as usize]);
                if (u32::MAX as u64) < n {
                    return None;
                }
                Value::Uid(n)
            }
            0xa0 | 0xc0 => {
                let (cnt, start) = self.count(marker, body)?;
                let bytes = cnt.checked_mul(self.t.object_ref_size as u64)?;
                if !self.fits(start, bytes) {
                    return None;
                }
                let made_set = self.open.is_none() && depth > 15;
                if made_set {
                    self.open = Some(HashSet::new());
                }
                if let Some(open) = self.open.as_mut() {
                    open.insert(off);
                }
                let mut items = Vec::with_capacity(cnt.min(4096) as usize);
                let mut ptr = start;
                let mut ok = true;
                for _ in 0..cnt {
                    match self.ref_at(ptr).and_then(|o| self.object(o, depth + 1)) {
                        Some(v) => items.push(v),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                    ptr += self.t.object_ref_size as u64;
                }
                if let Some(open) = self.open.as_mut() {
                    open.remove(&off);
                }
                if made_set {
                    self.open = None;
                }
                if !ok {
                    return None;
                }
                if marker & 0xf0 == 0xa0 {
                    Value::array(items)
                } else {
                    Value::Set(Rc::new(items))
                }
            }
            0xd0 => {
                let (cnt, start) = self.count(marker, body)?;
                let entries = cnt.checked_mul(2)?;
                let bytes = entries.checked_mul(self.t.object_ref_size as u64)?;
                if !self.fits(start, bytes) {
                    return None;
                }
                let made_set = self.open.is_none() && depth > 15;
                if made_set {
                    self.open = Some(HashSet::new());
                }
                if let Some(open) = self.open.as_mut() {
                    open.insert(off);
                }
                let mut list = Vec::with_capacity(entries.min(8192) as usize);
                let mut ptr = start;
                let mut ok = true;
                for idx in 0..entries {
                    match self.ref_at(ptr).and_then(|o| self.object(o, depth + 1)) {
                        Some(v) if idx >= cnt || is_primitive(&v) => list.push(v),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                    ptr += self.t.object_ref_size as u64;
                }
                if let Some(open) = self.open.as_mut() {
                    open.remove(&off);
                }
                if made_set {
                    self.open = None;
                }
                if !ok {
                    return None;
                }
                let values = list.split_off(cnt as usize);
                if list.iter().all(|k| matches!(k, Value::String(_))) {
                    let mut d = Dict::with_capacity(list.len());
                    for (k, v) in list.into_iter().zip(values) {
                        let Value::String(k) = k else { unreachable!() };
                        // CFDictionaryAddValue: the first of equal keys stays.
                        d.entry(k).or_insert(v);
                    }
                    Value::dict(d)
                } else {
                    Value::KeyedDict(Rc::new(list.into_iter().zip(values).collect()))
                }
            }
            _ => return None,
        };
        self.made.insert(off, v.clone());
        Some(v)
    }
}

/// __CFTryParseBinaryPlist: the property list, or None (CF says no more than that a binary
/// plist is corrupt, and goes on to try XML).
pub fn read(b: &[u8]) -> Option<Value> {
    if b.len() < 8 {
        return None;
    }
    let (t, off) = top_level(b)?;
    let mut r = Reader { b, t, made: HashMap::new(), open: None };
    r.object(off, 0)
}

// ---- writing ---------------------------------------------------------------------------------

/// An object of the object table: a value, or a dictionary's key.
enum Obj<'a> {
    V(&'a Value),
    Key(&'a str),
}

/// What numbers are uniqued by (CFNumberCompare's equality): integral values as integers,
/// whatever their type, the others by their bits as Float64.
#[derive(Hash, PartialEq, Eq)]
enum NumKey {
    Int(i128),
    Bits(u64),
}

fn num_key(v: &Value) -> NumKey {
    let f = match *v {
        Value::Int(i) => return NumKey::Int(i as i128),
        Value::UInt(u) => return NumKey::Int(u as i128),
        Value::Real(r) => r,
        Value::Float(f) => f as f64,
        _ => unreachable!(),
    };
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1.7e38 {
        NumKey::Int(f as i128)
    } else if f.is_nan() {
        NumKey::Bits(f64::NAN.to_bits())
    } else {
        NumKey::Bits(f.to_bits())
    }
}

struct Writer<'a> {
    objects: Vec<Obj<'a>>,
    strings: HashMap<&'a str, usize>,
    numbers: HashMap<NumKey, usize>,
    dates: HashMap<u64, usize>,
    datas: HashMap<&'a [u8], usize>,
    /// Bools by value, as CF's two CFBoolean objects.
    bools: [Option<usize>; 2],
    /// Collections by their Rc (CF's object identity), UIDs by their place.
    identities: HashMap<usize, usize>,
}

fn identity(v: &Value) -> usize {
    match v {
        Value::Array(a) => Rc::as_ptr(a) as usize,
        Value::Dict(d) => Rc::as_ptr(d) as usize,
        other => other as *const Value as usize,
    }
}

impl<'a> Writer<'a> {
    /// The object number `v` is referenced by.
    fn reference(&self, v: &'a Value) -> usize {
        match v {
            Value::String(s) => self.strings[s.as_str()],
            Value::Date(d) => self.dates[&date_bits(*d)],
            Value::Data(d) => self.datas[d.as_slice()],
            Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => self.numbers[&num_key(v)],
            Value::Bool(b) => self.bools[*b as usize].unwrap(),
            other => self.identities[&identity(other)],
        }
    }

    /// The first object number of a uniqued value, or None after giving it `n`.
    fn unique(&mut self, v: &'a Value, n: usize) -> Option<usize> {
        match v {
            Value::String(s) => insert_new(&mut self.strings, s.as_str(), n),
            Value::Date(d) => insert_new(&mut self.dates, date_bits(*d), n),
            Value::Data(d) => insert_new(&mut self.datas, d.as_slice(), n),
            Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => insert_new(&mut self.numbers, num_key(v), n),
            _ => None,
        }
    }

    /// _flattenPlist: depth first; a dictionary's keys, then its values.
    fn flatten(&mut self, v: &'a Value) {
        let n = self.objects.len();
        if self.unique(v, n).is_some() {
            return;
        }
        match v {
            // Not uniqued: written each time met, referenced as the first time.
            Value::Bool(b) => {
                self.bools[*b as usize].get_or_insert(n);
            }
            Value::Array(_) | Value::Dict(_) | Value::Uid(_) => {
                self.identities.entry(identity(v)).or_insert(n);
            }
            _ => {}
        }
        self.objects.push(Obj::V(v));
        match v {
            Value::Dict(d) => {
                for k in d.keys() {
                    let n = self.objects.len();
                    if insert_new(&mut self.strings, k.as_str(), n).is_none() {
                        self.objects.push(Obj::Key(k));
                    }
                }
                for value in d.values() {
                    self.flatten(value);
                }
            }
            Value::Array(a) => {
                for item in a.iter() {
                    self.flatten(item);
                }
            }
            _ => {}
        }
    }
}

fn insert_new<K: std::hash::Hash + Eq>(map: &mut HashMap<K, usize>, key: K, n: usize) -> Option<usize> {
    match map.get(&key) {
        Some(&first) => Some(first),
        None => {
            map.insert(key, n);
            None
        }
    }
}

/// CFDate equality is by time: 0.0 and -0.0 are one date.
fn date_bits(d: f64) -> u64 {
    if d == 0.0 {
        0
    } else {
        d.to_bits()
    }
}

/// _byteCount: the power of two (1, 2, 4, 8) of bytes `count` needs.
fn byte_count(count: u64) -> usize {
    let mut size = 0usize;
    let mut mask = !0u64;
    while count & mask != 0 {
        size += 1;
        mask <<= 8;
    }
    while !matches!(size, 1 | 2 | 4 | 8) && size <= 8 {
        size += 1;
    }
    size
}

fn append_int(out: &mut Vec<u8>, n: u64) {
    let (marker, bytes) = if n <= 0xff {
        (0x10, 1)
    } else if n <= 0xffff {
        (0x11, 2)
    } else if n <= 0xffff_ffff {
        (0x12, 4)
    } else {
        (0x13, 8)
    };
    out.push(marker);
    out.extend_from_slice(&n.to_be_bytes()[8 - bytes..]);
}

fn append_count(out: &mut Vec<u8>, marker: u8, count: usize) {
    out.push(marker | if count < 15 { count as u8 } else { 0xf });
    if count >= 15 {
        append_int(out, count as u64);
    }
}

fn append_string(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        append_count(out, 0x50, s.len());
        out.extend_from_slice(s.as_bytes());
    } else {
        let units: Vec<u16> = s.encode_utf16().collect();
        append_count(out, 0x60, units.len());
        for u in units {
            out.extend_from_slice(&u.to_be_bytes());
        }
    }
}

/// A binary property list, as __CFBinaryPlistWrite writes it (bplist00).
pub fn write(v: &Value) -> Vec<u8> {
    let mut w = Writer {
        objects: Vec::new(),
        strings: HashMap::new(),
        numbers: HashMap::new(),
        dates: HashMap::new(),
        datas: HashMap::new(),
        bools: [None, None],
        identities: HashMap::new(),
    };
    w.flatten(v);
    let count = w.objects.len();
    let ref_size = byte_count(count as u64);
    let reference = |out: &mut Vec<u8>, n: usize| out.extend_from_slice(&(n as u64).to_be_bytes()[8 - ref_size..]);
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(count);
    for obj in &w.objects {
        offsets.push(out.len() as u64);
        let v = match obj {
            Obj::Key(k) => {
                append_string(&mut out, k);
                continue;
            }
            Obj::V(v) => *v,
        };
        match v {
            Value::String(s) => append_string(&mut out, s),
            // CFNumberGetValue(kCFNumberSInt64Type) as uint64: negatives take 8 bytes.
            Value::Int(i) => append_int(&mut out, *i as u64),
            Value::UInt(u) => {
                out.push(0x14);
                out.extend_from_slice(&0u64.to_be_bytes());
                out.extend_from_slice(&u.to_be_bytes());
            }
            Value::Real(r) => {
                out.push(0x23);
                out.extend_from_slice(&r.to_be_bytes());
            }
            Value::Float(f) => {
                out.push(0x22);
                out.extend_from_slice(&f.to_be_bytes());
            }
            Value::Bool(b) => out.push(if *b { 0x09 } else { 0x08 }),
            Value::Date(d) => {
                out.push(0x33);
                out.extend_from_slice(&d.to_be_bytes());
            }
            Value::Data(d) => {
                append_count(&mut out, 0x40, d.len());
                out.extend_from_slice(d);
            }
            Value::Uid(u) => {
                let bytes = if *u <= 0xff {
                    1
                } else if *u <= 0xffff {
                    2
                } else if *u <= 0xffff_ffff {
                    4
                } else {
                    8
                };
                out.push(0x80 | (bytes as u8 - 1));
                out.extend_from_slice(&u.to_be_bytes()[8 - bytes..]);
            }
            Value::Array(a) => {
                append_count(&mut out, 0xa0, a.len());
                for item in a.iter() {
                    reference(&mut out, w.reference(item));
                }
            }
            Value::Dict(d) => {
                append_count(&mut out, 0xd0, d.len());
                for k in d.keys() {
                    reference(&mut out, w.strings[k.as_str()]);
                }
                for value in d.values() {
                    reference(&mut out, w.reference(value));
                }
            }
            Value::Null | Value::Set(_) | Value::KeyedDict(_) => {}
        }
    }
    let table_offset = out.len() as u64;
    let offset_size = byte_count(table_offset);
    for off in offsets {
        out.extend_from_slice(&off.to_be_bytes()[8 - offset_size..]);
    }
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, offset_size as u8, ref_size as u8]);
    out.extend_from_slice(&(count as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&table_offset.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_uniquing() {
        let mut d = Dict::new();
        d.insert("a".into(), Value::Int(1));
        d.insert("b".into(), Value::Real(1.0));
        d.insert("c".into(), Value::String("a".into()));
        d.insert("d".into(), Value::array(vec![Value::Bool(true), Value::Bool(true), Value::Int(-1), Value::UInt(u64::MAX), Value::Uid(7)]));
        let v = Value::dict(d);
        let bytes = write(&v);
        assert_eq!(&bytes[..8], b"bplist00");
        let back = read(&bytes).unwrap();
        let back = back.as_dict().unwrap();
        // 1 and 1.0 are one object, written as the first met: the integer.
        assert!(matches!(back["b"], Value::Int(1)));
        assert!(matches!(back["c"], Value::String(ref s) if s == "a"));
        let a = back["d"].as_array().unwrap();
        assert!(matches!(a[2], Value::Int(-1)) && matches!(a[3], Value::UInt(u64::MAX)) && matches!(a[4], Value::Uid(7)));
        // objects: dict, 4 keys ("c"'s value is the key "a"), 1, array, true, true, -1, max, uid
        assert_eq!(u64::from_be_bytes(bytes[bytes.len() - 24..bytes.len() - 16].try_into().unwrap()), 12);
    }

    #[test]
    fn corrupt_is_refused() {
        let bytes = write(&Value::String("hello".into()));
        assert!(read(&bytes).is_some());
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 1] ^= 0x40;
        assert!(read(&bad).is_none());
        assert!(read(&bytes[..bytes.len() - 1]).is_none());
    }
}
