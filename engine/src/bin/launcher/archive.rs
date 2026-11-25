//! The archives the launcher unpacks: an add-on's zip (addons.rs), and a release's runtime, a
//! .tar.gz or, for Windows, a .zip (update.rs). Only what those hold: deflated or stored zip
//! entries without zip64 or encryption; tar's files and folders (ustar, GNU long names, pax paths).
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A file in a zip, as its central directory describes it.
#[derive(Debug)]
pub struct Entry {
    pub name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

pub fn central_directory(file: &mut (impl Read + Seek)) -> Result<Vec<Entry>, String> {
    let io = |e: std::io::Error| e.to_string();
    let len = file.seek(SeekFrom::End(0)).map_err(io)?;
    // The end record is 22 bytes and a comment of at most 65535.
    let tail_len = len.min(22 + 65535);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len)).map_err(io)?;
    file.read_exact(&mut tail).map_err(io)?;
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(&tail, i) == 0x0605_4b50)
        .ok_or("not a zip archive")?;
    let (count, size, start) = (u16_at(&tail, end + 10), u32_at(&tail, end + 12), u32_at(&tail, end + 16));
    if count == 0xffff || size == u32::MAX || start == u32::MAX {
        return Err("a zip64 archive (not supported)".into());
    }
    let mut directory = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(start as u64)).map_err(io)?;
    file.read_exact(&mut directory).map_err(io)?;

    let mut entries = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        if at + 46 > directory.len() || u32_at(&directory, at) != 0x0201_4b50 {
            return Err("a broken central directory".into());
        }
        let (name_len, extra_len, comment_len) =
            (u16_at(&directory, at + 28) as usize, u16_at(&directory, at + 30) as usize, u16_at(&directory, at + 32) as usize);
        let name = directory.get(at + 46..at + 46 + name_len).ok_or("a broken central directory")?;
        if u16_at(&directory, at + 8) & 1 != 0 {
            return Err(format!("{} is encrypted", String::from_utf8_lossy(name)));
        }
        entries.push(Entry {
            name: String::from_utf8_lossy(name).into_owned(),
            method: u16_at(&directory, at + 10),
            crc: u32_at(&directory, at + 16),
            compressed: u32_at(&directory, at + 20) as u64,
            size: u32_at(&directory, at + 24) as u64,
            offset: u32_at(&directory, at + 42) as u64,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Writes one entry's bytes to `to`, checking their size and CRC.
pub fn extract(file: &mut File, entry: &Entry, to: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut local = [0u8; 30];
    file.seek(SeekFrom::Start(entry.offset)).map_err(io)?;
    file.read_exact(&mut local).map_err(io)?;
    if u32_at(&local, 0) != 0x0403_4b50 {
        return Err("a broken local header".into());
    }
    let skip = u16_at(&local, 26) as i64 + u16_at(&local, 28) as i64;
    file.seek(SeekFrom::Current(skip)).map_err(io)?;
    let data = (&mut *file).take(entry.compressed);
    let mut reader: Box<dyn Read + '_> = match entry.method {
        0 => Box::new(data),
        8 => Box::new(flate2::read::DeflateDecoder::new(data)),
        method => return Err(format!("compression method {method} (not supported)")),
    };
    let mut out = std::io::BufWriter::new(File::create(to).map_err(|e| format!("{}: {e}", to.display()))?);
    let (mut buffer, mut crc, mut size) = (vec![0u8; 256 * 1024], crc32fast::Hasher::new(), 0u64);
    loop {
        let n = reader.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        crc.update(&buffer[..n]);
        size += n as u64;
        out.write_all(&buffer[..n]).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    out.flush().map_err(|e| format!("{}: {e}", to.display()))?;
    if size != entry.size || crc.finalize() != entry.crc {
        return Err("its size or CRC does not match (a broken download?)".into());
    }
    Ok(())
}

/// A regular file of a tar, as it streams by.
#[derive(Debug)]
pub struct TarFile {
    pub name: String,
    /// Its permission bits (set on Unix).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub mode: u32,
}

/// A tar header's number: octal digits (ending in a space or NUL), or GNU's base-256 when the
/// first byte has its top bit set.
fn tar_number(field: &[u8]) -> Result<u64, String> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return Ok(field[1..].iter().fold((field[0] & 0x7f) as u64, |n, b| (n << 8) | *b as u64));
    }
    let digits = String::from_utf8_lossy(field);
    let digits = digits.trim_matches(|c: char| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(digits, 8).map_err(|_| format!("a broken tar header (number {digits:?})"))
}

/// The text up to the first NUL.
fn tar_text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// The `path` record of a pax header ("LEN path=VALUE\n" records), if it has one.
fn pax_path(records: &[u8]) -> Option<String> {
    let mut rest = records;
    while !rest.is_empty() {
        let space = rest.iter().position(|b| *b == b' ')?;
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        let record = rest.get(space + 1..len)?;
        if let Some(value) = record.strip_prefix(b"path=") {
            return Some(String::from_utf8_lossy(value.strip_suffix(b"\n").unwrap_or(value)).into_owned());
        }
        rest = &rest[len..];
    }
    None
}

/// Reads exactly one 512-byte block; false at the end of the stream.
fn read_block(reader: &mut impl Read, block: &mut [u8; 512]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < 512 {
        match reader.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("the archive ends inside a header (a broken download?)".into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(true)
}

fn skip(reader: &mut impl Read, bytes: u64) -> Result<(), String> {
    let skipped = std::io::copy(&mut reader.take(bytes), &mut std::io::sink()).map_err(|e| e.to_string())?;
    if skipped != bytes {
        return Err("the archive ends inside a file (a broken download?)".into());
    }
    Ok(())
}

/// Calls `each` with every regular file of a gzipped tar and a reader of exactly its bytes.
/// Folders are passed over (`each` makes a file's folders); a link or a device is an error, as
/// a runtime has none.
pub fn tar_gz_files(file: File, each: &mut dyn FnMut(&TarFile, &mut dyn Read) -> Result<(), String>) -> Result<(), String> {
    let mut tar = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    loop {
        if !read_block(&mut tar, &mut header)? || header.iter().all(|b| *b == 0) {
            // On to the gzip trailer, so its CRC is checked: a bit flipped in a download can
            // still inflate.
            return std::io::copy(&mut tar, &mut std::io::sink())
                .map(|_| ())
                .map_err(|e| format!("after the files: {e} (a broken download?)"));
        }
        let stored = tar_number(&header[148..156])?;
        let sum: u64 = header.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { b' ' as u64 } else { *b as u64 }).sum();
        if stored != sum {
            return Err("a tar header's checksum does not match (not a tar, or a broken download?)".into());
        }
        let size = tar_number(&header[124..136])?;
        let padding = size.next_multiple_of(512) - size;
        let name = long_name.take().unwrap_or_else(|| {
            let name = tar_text(&header[..100]);
            let prefix = if &header[257..262] == b"ustar" { tar_text(&header[345..500]) } else { String::new() };
            if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
        });
        match header[156] {
            b'0' | b'\0' | b'7' => {
                let entry = TarFile { name, mode: tar_number(&header[100..108])? as u32 & 0o777 };
                let mut body = (&mut tar).take(size);
                each(&entry, &mut body)?;
                // whatever `each` left unread, then the padding to the next block
                let left = body.limit();
                skip(&mut body, left)?;
                skip(&mut tar, padding)?;
            }
            b'5' | b'g' => skip(&mut tar, size + padding)?,
            kind @ (b'L' | b'x') => {
                let mut data = Vec::new();
                (&mut tar).take(size).read_to_end(&mut data).map_err(|e| e.to_string())?;
                skip(&mut tar, padding)?;
                long_name = if kind == b'L' { Some(tar_text(&data)) } else { pax_path(&data) };
            }
            kind => return Err(format!("{name}: a link or special file (tar type {:?}), which a runtime does not have", kind as char)),
        }
    }
}
