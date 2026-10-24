//! The zip format as zip(1) reads and writes it: an existing archive's central directory (with
//! Zip64), its entries copied byte for byte, and the records Info-ZIP's zip 3.0 writes for new
//! ones (local header, data descriptor, central header, end records), with its extra fields:
//! UT (0x5455, extended timestamp), ux (0x7875, Unix UID/GID) and Zip64 (0x0001).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const LOCAL_SIG: u32 = 0x0403_4b50;
pub const CENTRAL_SIG: u32 = 0x0201_4b50;
pub const END_SIG: u32 = 0x0605_4b50;
pub const END64_SIG: u32 = 0x0606_4b50;
pub const LOC64_SIG: u32 = 0x0706_4b50;
pub const DD_SIG: u32 = 0x0807_4b50;
pub const MAX32: u64 = 0xffff_ffff;
/// Version made by: Unix (3), zip 3.0
pub const MADE_BY: u16 = 0x031e;

pub fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// One entry of an existing archive, from its central header.
#[derive(Clone)]
pub struct Entry {
    pub made_by: u16,
    pub need: u16,
    pub flags: u16,
    pub method: u16,
    pub dostime: u32,
    pub crc: u32,
    pub csize: u64,
    pub usize: u64,
    pub name: Vec<u8>,
    /// The central extra field, without its Zip64 record (which is written again as needed)
    pub extra: Vec<u8>,
    pub comment: Vec<u8>,
    pub iattr: u16,
    pub eattr: u32,
    /// Where its local header starts in the archive being read
    pub offset: u64,
}

pub struct Archive {
    pub entries: Vec<Entry>,
    pub comment: Vec<u8>,
}

pub enum ReadError {
    /// "missing end signature--probably not a zip file"
    NoEnd,
    /// The structure is broken somewhere else (zip error 3)
    Bad,
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

fn read_at(f: &mut File, at: u64, n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0; n];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut b)?;
    Ok(b)
}

/// Splits a Zip64 extra record out of `extra`; `want` says which of (usize, csize, offset) were
/// 0xffffffff in the header and so are in it, in that order.
fn take_zip64(extra: &[u8], want: [bool; 3]) -> (Vec<u8>, [Option<u64>; 3]) {
    let mut rest = Vec::new();
    let mut vals = [None; 3];
    let mut i = 0;
    while i + 4 <= extra.len() {
        let tag = le16(extra, i);
        let n = le16(extra, i + 2) as usize;
        let end = (i + 4 + n).min(extra.len());
        if tag == 1 {
            let mut at = i + 4;
            for (k, w) in want.iter().enumerate() {
                if *w && at + 8 <= end {
                    vals[k] = Some(le64(extra, at));
                    at += 8;
                }
            }
        } else {
            rest.extend_from_slice(&extra[i..end]);
        }
        i = end;
    }
    (rest, vals)
}

impl Archive {
    pub fn read(f: &mut File) -> Result<Archive, ReadError> {
        let len = f.seek(SeekFrom::End(0))?;
        if len < 22 {
            return Err(ReadError::NoEnd);
        }
        let tail_len = len.min(22 + 65535) as usize;
        let tail = read_at(f, len - tail_len as u64, tail_len)?;
        let Some(e) = (0..=tail_len - 22).rev().find(|&i| le32(&tail, i) == END_SIG) else {
            return Err(ReadError::NoEnd);
        };
        let end_pos = len - tail_len as u64 + e as u64;
        let mut count = le16(&tail, e + 10) as u64;
        let mut cd_size = le32(&tail, e + 12) as u64;
        let mut cd_off = le32(&tail, e + 16) as u64;
        let clen = le16(&tail, e + 20) as usize;
        let comment = tail[e + 22..(e + 22 + clen).min(tail_len)].to_vec();
        let mut cd_end = end_pos;
        // The Zip64 end record, through its locator just before the end record
        if end_pos >= 20 {
            let loc = read_at(f, end_pos - 20, 20)?;
            if le32(&loc, 0) == LOC64_SIG {
                let at = le64(&loc, 8);
                let rec = read_at(f, at, 56).map_err(|_| ReadError::Bad)?;
                if le32(&rec, 0) != END64_SIG {
                    return Err(ReadError::Bad);
                }
                count = le64(&rec, 32);
                cd_size = le64(&rec, 40);
                cd_off = le64(&rec, 48);
                cd_end = at;
            }
        }
        // A prefix (a self-extractor) moves everything: offsets are from the archive's start
        let shift = cd_end.checked_sub(cd_size).and_then(|start| start.checked_sub(cd_off)).unwrap_or(0);
        let cd = read_at(f, cd_off + shift, cd_size as usize).map_err(|_| ReadError::Bad)?;
        let mut entries = Vec::with_capacity(count as usize);
        let mut i = 0;
        while i + 46 <= cd.len() && le32(&cd, i) == CENTRAL_SIG {
            let nl = le16(&cd, i + 28) as usize;
            let el = le16(&cd, i + 30) as usize;
            let cl = le16(&cd, i + 32) as usize;
            if i + 46 + nl + el + cl > cd.len() {
                return Err(ReadError::Bad);
            }
            let csize = le32(&cd, i + 20) as u64;
            let usize = le32(&cd, i + 24) as u64;
            let offset = le32(&cd, i + 42) as u64;
            let raw_extra = &cd[i + 46 + nl..i + 46 + nl + el];
            let (extra, z) = take_zip64(raw_extra, [usize == MAX32, csize == MAX32, offset == MAX32]);
            entries.push(Entry {
                made_by: le16(&cd, i + 4),
                need: le16(&cd, i + 6),
                flags: le16(&cd, i + 8),
                method: le16(&cd, i + 10),
                dostime: le32(&cd, i + 12),
                crc: le32(&cd, i + 16),
                csize: z[1].unwrap_or(csize),
                usize: z[0].unwrap_or(usize),
                name: cd[i + 46..i + 46 + nl].to_vec(),
                extra,
                comment: cd[i + 46 + nl + el..i + 46 + nl + el + cl].to_vec(),
                iattr: le16(&cd, i + 36),
                eattr: le32(&cd, i + 38),
                offset: z[2].unwrap_or(offset) + shift,
            });
            i += 46 + nl + el + cl;
        }
        if entries.len() as u64 != count {
            return Err(ReadError::Bad);
        }
        Ok(Archive { entries, comment })
    }
}

/// How many bytes an existing entry takes from its local header to the end of its data (and
/// data descriptor), to copy it as it is.
pub fn local_span(f: &mut File, e: &Entry) -> io::Result<u64> {
    let h = read_at(f, e.offset, 30)?;
    if le32(&h, 0) != LOCAL_SIG {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad local header"));
    }
    let nl = le16(&h, 26) as u64;
    let el = le16(&h, 28) as u64;
    let mut span = 30 + nl + el + e.csize;
    if e.flags & 8 != 0 {
        // The descriptor: with or without its signature, sizes of 8 bytes when Zip64 was used
        let local_extra = read_at(f, e.offset + 30 + nl, el as usize)?;
        let zip64 = le32(&h, 18) as u64 == MAX32 || le32(&h, 22) as u64 == MAX32 || has_tag(&local_extra, 1) || e.csize >= MAX32 || e.usize >= MAX32;
        let sig = read_at(f, e.offset + span, 4).map(|b| le32(&b, 0) == DD_SIG).unwrap_or(false);
        span += if sig { 4 } else { 0 } + if zip64 { 20 } else { 12 };
    }
    Ok(span)
}

fn has_tag(extra: &[u8], want: u16) -> bool {
    let mut i = 0;
    while i + 4 <= extra.len() {
        if le16(extra, i) == want {
            return true;
        }
        i += 4 + le16(extra, i + 2) as usize;
    }
    false
}

pub fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
pub fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
pub fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// The fields of a local header.
pub struct Local<'a> {
    pub need: u16,
    pub flags: u16,
    pub method: u16,
    pub dostime: u32,
    pub crc: u32,
    pub csize: u64,
    pub usize: u64,
    pub name: &'a [u8],
    pub extra: &'a [u8],
    /// Sizes go in a Zip64 record (the header's own are 0xffffffff)
    pub zip64: bool,
}

pub fn local_header(l: &Local) -> Vec<u8> {
    let mut v = Vec::with_capacity(30 + l.name.len() + l.extra.len() + 20);
    put32(&mut v, LOCAL_SIG);
    put16(&mut v, l.need);
    put16(&mut v, l.flags);
    put16(&mut v, l.method);
    put32(&mut v, l.dostime);
    put32(&mut v, l.crc);
    if l.zip64 {
        put32(&mut v, MAX32 as u32);
        put32(&mut v, MAX32 as u32);
    } else {
        put32(&mut v, l.csize as u32);
        put32(&mut v, l.usize as u32);
    }
    put16(&mut v, l.name.len() as u16);
    put16(&mut v, (l.extra.len() + if l.zip64 { 20 } else { 0 }) as u16);
    v.extend_from_slice(l.name);
    v.extend_from_slice(l.extra);
    if l.zip64 {
        put16(&mut v, 1);
        put16(&mut v, 16);
        put64(&mut v, l.usize);
        put64(&mut v, l.csize);
    }
    v
}

pub fn data_descriptor(crc: u32, csize: u64, usize: u64, zip64: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(24);
    put32(&mut v, DD_SIG);
    put32(&mut v, crc);
    if zip64 {
        put64(&mut v, csize);
        put64(&mut v, usize);
    } else {
        put32(&mut v, csize as u32);
        put32(&mut v, usize as u32);
    }
    v
}

/// The central header of an entry written at `offset`; sizes and the offset that do not fit in
/// 32 bits go in a Zip64 record (usize, csize, offset, in that order), and Zip64 needs version 45.
pub fn central_header(e: &Entry, offset: u64) -> Vec<u8> {
    let big = [e.usize >= MAX32, e.csize >= MAX32, offset >= MAX32];
    let mut z64 = Vec::new();
    for (k, val) in [e.usize, e.csize, offset].into_iter().enumerate() {
        if big[k] {
            put64(&mut z64, val);
        }
    }
    let mut v = Vec::with_capacity(46 + e.name.len() + e.extra.len() + z64.len() + 4 + e.comment.len());
    put32(&mut v, CENTRAL_SIG);
    put16(&mut v, e.made_by);
    put16(&mut v, if z64.is_empty() { e.need } else { e.need.max(45) });
    put16(&mut v, e.flags);
    put16(&mut v, e.method);
    put32(&mut v, e.dostime);
    put32(&mut v, e.crc);
    put32(&mut v, if big[1] { MAX32 as u32 } else { e.csize as u32 });
    put32(&mut v, if big[0] { MAX32 as u32 } else { e.usize as u32 });
    put16(&mut v, e.name.len() as u16);
    put16(&mut v, (e.extra.len() + if z64.is_empty() { 0 } else { 4 + z64.len() }) as u16);
    put16(&mut v, e.comment.len() as u16);
    put16(&mut v, 0); // disk
    put16(&mut v, e.iattr);
    put32(&mut v, e.eattr);
    put32(&mut v, if big[2] { MAX32 as u32 } else { offset as u32 });
    v.extend_from_slice(&e.name);
    v.extend_from_slice(&e.extra);
    if !z64.is_empty() {
        put16(&mut v, 1);
        put16(&mut v, z64.len() as u16);
        v.extend_from_slice(&z64);
    }
    v.extend_from_slice(&e.comment);
    v
}

/// The end records after a central directory of `count` entries, `size` bytes at `offset`; the
/// Zip64 end record and its locator come first when a value does not fit (or `force`).
pub fn end_records(count: u64, size: u64, offset: u64, comment: &[u8], force: bool) -> Vec<u8> {
    let mut v = Vec::new();
    let zip64 = force || count >= 0xffff || size >= MAX32 || offset >= MAX32;
    if zip64 {
        let at = offset + size;
        put32(&mut v, END64_SIG);
        put64(&mut v, 44);
        put16(&mut v, MADE_BY);
        put16(&mut v, 45);
        put32(&mut v, 0);
        put32(&mut v, 0);
        put64(&mut v, count);
        put64(&mut v, count);
        put64(&mut v, size);
        put64(&mut v, offset);
        put32(&mut v, LOC64_SIG);
        put32(&mut v, 0);
        put64(&mut v, at);
        put32(&mut v, 1);
    }
    put32(&mut v, END_SIG);
    put16(&mut v, 0);
    put16(&mut v, 0);
    let n = if count >= 0xffff { 0xffff } else { count as u16 };
    put16(&mut v, n);
    put16(&mut v, n);
    put32(&mut v, if size >= MAX32 { MAX32 as u32 } else { size as u32 });
    put32(&mut v, if offset >= MAX32 || (force && zip64) { MAX32 as u32 } else { offset as u32 });
    put16(&mut v, comment.len() as u16);
    v.extend_from_slice(comment);
    v
}

/// The UT extra field: in the local header the modification and access times, in the central
/// one the modification time only (flags 3 in both, as Info-ZIP writes them).
pub fn ut_extra(mtime: i64, atime: i64, local: bool) -> Vec<u8> {
    let mut v = Vec::new();
    put16(&mut v, 0x5455);
    put16(&mut v, if local { 9 } else { 5 });
    v.push(3);
    put32(&mut v, mtime as u32);
    if local {
        put32(&mut v, atime as u32);
    }
    v
}

/// The ux extra field (new Unix UID/GID, version 1, 4-byte IDs).
pub fn ux_extra(uid: u32, gid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    put16(&mut v, 0x7875);
    put16(&mut v, 11);
    v.push(1);
    v.push(4);
    put32(&mut v, uid);
    v.push(4);
    put32(&mut v, gid);
    v
}

/// A Unix time as an MS-DOS date and time (local time), as unix2dostime: odd seconds round up,
/// dates before 1980 become 1980-01-01 00:00.
pub fn dos_time(t: i64) -> u32 {
    let t = (t + 1) & !1;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return (1 << 21) | (1 << 16);
    }
    let year = tm.tm_year + 1900;
    if year < 1980 {
        return (1 << 21) | (1 << 16);
    }
    (((year - 1980) as u32) << 25)
        | (((tm.tm_mon + 1) as u32) << 21)
        | ((tm.tm_mday as u32) << 16)
        | ((tm.tm_hour as u32) << 11)
        | ((tm.tm_min as u32) << 5)
        | ((tm.tm_sec as u32) >> 1)
}

/// An MS-DOS date and time (local time) as a Unix time.
pub fn unix_time(d: u32) -> i64 {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = ((d >> 25) & 0x7f) as i32 + 80;
    tm.tm_mon = ((d >> 21) & 0xf) as i32 - 1;
    tm.tm_mday = ((d >> 16) & 0x1f) as i32;
    tm.tm_hour = ((d >> 11) & 0x1f) as i32;
    tm.tm_min = ((d >> 5) & 0x3f) as i32;
    tm.tm_sec = ((d & 0x1f) * 2) as i32;
    tm.tm_isdst = -1;
    unsafe { libc::mktime(&mut tm) as i64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_time_round_trip() {
        std::env::set_var("TZ", "UTC");
        let t = 1_800_000_001; // odd: rounds up
        assert_eq!(unix_time(dos_time(t)), t + 1);
        assert_eq!(dos_time(0), (1 << 21) | (1 << 16));
    }

    #[test]
    fn central_zip64() {
        let e = Entry {
            made_by: MADE_BY,
            need: 20,
            flags: 0,
            method: 8,
            dostime: 0,
            crc: 1,
            csize: 10,
            usize: 5_000_000_000,
            name: b"big".to_vec(),
            extra: Vec::new(),
            comment: Vec::new(),
            iattr: 0,
            eattr: 0,
            offset: 0,
        };
        let h = central_header(&e, 7);
        assert_eq!(le16(&h, 6), 45);
        assert_eq!(le32(&h, 24), MAX32 as u32);
        assert_eq!(le32(&h, 20), 10);
        assert_eq!(le16(&h, 30), 12);
        assert_eq!(le64(&h, 46 + 3 + 4), 5_000_000_000);
    }
}
