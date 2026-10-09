//! The archive formats 7z works with, behind one interface: open (by type, extension or
//! signature), the items as 7-Zip lists them, their data in archive order, and writing a new
//! archive from items on the disk or extracted from the old one.
//!
//!   7z     sevenz-rust2: LZMA/LZMA2/PPMd/BZip2/Deflate/Copy, BCJ filters, Delta, AES-256
//!   zip    zip: Store/Deflate/Deflate64/BZip2/LZMA/XZ/PPMd, ZipCrypto and AES (read and write)
//!   tar    tar (GNU headers when writing, as 7-Zip)
//!   gzip, bzip2, xz, lzma, zstd   one file each (zstd: extract only, as 7-Zip)

use crate::common::errstr;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    SevenZ,
    Zip,
    Tar,
    GZip,
    BZip2,
    Xz,
    Lzma,
    Zstd,
}

impl Fmt {
    pub fn name(self) -> &'static str {
        match self {
            Fmt::SevenZ => "7z",
            Fmt::Zip => "zip",
            Fmt::Tar => "tar",
            Fmt::GZip => "gzip",
            Fmt::BZip2 => "bzip2",
            Fmt::Xz => "xz",
            Fmt::Lzma => "lzma",
            Fmt::Zstd => "zstd",
        }
    }

    /// -t names (7-Zip accepts the format name and its usual extensions).
    pub fn from_type(t: &str) -> Option<Fmt> {
        Some(match t.to_ascii_lowercase().as_str() {
            "7z" => Fmt::SevenZ,
            "zip" | "jar" | "xpi" | "odt" | "ods" | "docx" | "xlsx" | "epub" | "apk" => Fmt::Zip,
            "tar" | "ova" => Fmt::Tar,
            "gzip" | "gz" | "gzi" | "tgz" | "tpz" | "apm" => Fmt::GZip,
            "bzip2" | "bz2" | "bzip" | "tbz2" | "tbz" => Fmt::BZip2,
            "xz" | "txz" => Fmt::Xz,
            "lzma" => Fmt::Lzma,
            "zstd" | "zst" | "tzst" => Fmt::Zstd,
            _ => return None,
        })
    }

    pub fn from_ext(name: &str) -> Option<Fmt> {
        let base = name.rsplit('/').next().unwrap_or(name);
        let ext = base.rsplit_once('.')?.1;
        Fmt::from_type(ext)
    }

    pub fn detect(h: &[u8]) -> Option<Fmt> {
        if h.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
            Some(Fmt::SevenZ)
        } else if h.starts_with(b"PK\x03\x04") || h.starts_with(b"PK\x05\x06") || h.starts_with(b"PK\x07\x08") {
            Some(Fmt::Zip)
        } else if h.starts_with(&[0x1F, 0x8B]) {
            Some(Fmt::GZip)
        } else if h.starts_with(b"BZh") {
            Some(Fmt::BZip2)
        } else if h.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
            Some(Fmt::Xz)
        } else if h.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            Some(Fmt::Zstd)
        } else if h.len() >= 263 && &h[257..262] == b"ustar" {
            Some(Fmt::Tar)
        } else if h.len() >= 13 && h[0] == 0x5D && h[1] == 0 && h[2] == 0 {
            Some(Fmt::Lzma)
        } else if h.len() >= 512 && tar_checksum_ok(&h[..512]) {
            Some(Fmt::Tar)
        } else {
            None
        }
    }

    pub fn can_write(self) -> bool {
        !matches!(self, Fmt::Zstd)
    }

    pub fn single_file(self) -> bool {
        matches!(self, Fmt::GZip | Fmt::BZip2 | Fmt::Xz | Fmt::Lzma | Fmt::Zstd)
    }
}

fn tar_checksum_ok(h: &[u8]) -> bool {
    let field = std::str::from_utf8(&h[148..156]).unwrap_or("");
    let Ok(stored) = u32::from_str_radix(field.trim_matches(|c: char| c == '\0' || c == ' '), 8) else { return false };
    let sum: u32 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u32 }).sum();
    sum == stored && h[0] != 0
}

/// An item as listed.
#[derive(Clone, Default, Debug)]
pub struct Item {
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub packed: Option<u64>,
    /// Unix seconds and nanoseconds.
    pub mtime: Option<(i64, u32)>,
    pub ctime: Option<(i64, u32)>,
    pub atime: Option<(i64, u32)>,
    /// Windows attributes, with the Unix mode in the high 16 bits when 0x8000 is set.
    pub attrib: Option<u32>,
    pub mode: Option<u32>,
    pub crc: Option<u32>,
    pub encrypted: bool,
    pub method: Option<String>,
    pub block: Option<u64>,
    pub link: Option<String>,
    pub user: Option<String>,
    pub group: Option<String>,
}

impl Item {
    /// The Unix mode: from the attributes' high half, or the format's own field.
    pub fn unix_mode(&self) -> Option<u32> {
        if let Some(m) = self.mode {
            return Some(m);
        }
        match self.attrib {
            Some(a) if a & 0x8000 != 0 => Some(a >> 16),
            _ => None,
        }
    }
    pub fn is_symlink(&self) -> bool {
        self.link.is_some() || self.unix_mode().map_or(false, |m| m & 0o170000 == 0o120000)
    }
}

#[derive(Debug)]
pub enum OpenError {
    NotArchive,
    WrongPassword,
    NeedPassword,
    Io(io::Error),
    /// The format was recognized, its headers are broken.
    Headers(String),
}

#[derive(Debug)]
pub enum DataError {
    Crc,
    Data,
    WrongPassword,
    Unsupported(String),
    Io(io::Error),
    /// The reader callback asked to stop.
    Stopped,
}

pub struct Opened {
    pub fmt: Fmt,
    pub path: String,
    pub props: Vec<(String, String)>,
    pub items: Vec<Item>,
    inner: Inner,
    password: Option<String>,
}

enum Inner {
    SevenZ { archive: sevenz_rust2::Archive },
    Zip,
    Tar,
    Single,
}

fn nt_to_unix(t: sevenz_rust2::NtTime) -> (i64, u32) {
    let v = u64::from(t) as i64;
    let secs = v.div_euclid(10_000_000) - 11_644_473_600;
    let nanos = (v.rem_euclid(10_000_000) * 100) as u32;
    (secs, nanos)
}

pub fn unix_to_nt(secs: i64, nanos: u32) -> sevenz_rust2::NtTime {
    sevenz_rust2::NtTime::from(((secs + 11_644_473_600) * 10_000_000 + (nanos / 100) as i64) as u64)
}

/// 7-Zip's name for a dictionary size: the power of two, or a number with m/k/b.
fn size_value(v: u32) -> String {
    if v.is_power_of_two() {
        return v.trailing_zeros().to_string();
    }
    if v % (1 << 20) == 0 {
        format!("{}m", v >> 20)
    } else if v % (1 << 10) == 0 {
        format!("{}k", v >> 10)
    } else {
        format!("{v}b")
    }
}

fn lzma2_string(d: u8) -> String {
    if d > 40 {
        return String::new();
    }
    if d == 40 {
        return "4g".into();
    }
    if d & 1 == 0 {
        return ((d >> 1) as u32 + 12).to_string();
    }
    let mut e = (d >> 1) as u32 + 1;
    let mut c = 'k';
    if e >= 10 {
        c = 'm';
        e -= 10;
    }
    format!("{}{}", 3u32 << e, c)
}

fn method_id(id: &[u8]) -> u64 {
    id.iter().fold(0u64, |a, &b| (a << 8) | b as u64)
}

fn coder_name(id: &[u8], props: &[u8]) -> String {
    let n = method_id(id);
    let (name, p): (&str, String) = match n {
        0x00 => ("Copy", String::new()),
        0x21 => ("LZMA2", if props.len() == 1 { lzma2_string(props[0]) } else { String::new() }),
        0x030101 => {
            let mut s = String::new();
            if props.len() == 5 {
                s = size_value(u32::from_le_bytes(props[1..5].try_into().unwrap()));
                let mut d = props[0] as u32;
                if d != 0x5D {
                    let lc = d % 9;
                    d /= 9;
                    let (pb, lp) = (d / 5, d % 5);
                    if lc != 3 {
                        s += &format!(":lc{lc}");
                    }
                    if lp != 0 {
                        s += &format!(":lp{lp}");
                    }
                    if pb != 2 {
                        s += &format!(":pb{pb}");
                    }
                }
            }
            ("LZMA", s)
        }
        0x030401 => ("PPMD", if props.len() == 5 { format!("o{}:mem{}", props[0], size_value(u32::from_le_bytes(props[1..5].try_into().unwrap()))) } else { String::new() }),
        0x03 => ("Delta", if props.len() == 1 { (props[0] as u32 + 1).to_string() } else { String::new() }),
        0x0A => ("ARM64", if props.len() == 4 { u32::from_le_bytes(props.try_into().unwrap()).to_string() } else { String::new() }),
        0x0B => ("RISCV", if props.len() == 4 { u32::from_le_bytes(props.try_into().unwrap()).to_string() } else { String::new() }),
        0x0303011B => ("BCJ2", String::new()),
        0x03030103 => ("BCJ", String::new()),
        0x03030205 => ("PPC", String::new()),
        0x03030401 => ("IA64", String::new()),
        0x03030501 => ("ARM", String::new()),
        0x03030701 => ("ARMT", String::new()),
        0x03030805 => ("SPARC", String::new()),
        0x040202 => ("BZip2", String::new()),
        0x040108 => ("Deflate", String::new()),
        0x040109 => ("Deflate64", String::new()),
        0x04F71101 => ("ZSTD", String::new()),
        0x06F10701 => ("7zAES", if !props.is_empty() { (props[0] & 0x3F).to_string() } else { String::new() }),
        _ => return format!("{n:X}"),
    };
    if p.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{p}")
    }
}

fn archive_method_name(id: u64, lzma2: u8, lzma_dic: u32) -> String {
    match id {
        0x21 => format!("LZMA2:{}", lzma2_string(lzma2)),
        0x030101 => format!("LZMA:{}", size_value(lzma_dic)),
        _ => {
            let n = coder_name(&id.to_be_bytes()[(id.leading_zeros() / 8) as usize..], &[]);
            n.split(':').next().unwrap_or("").to_string()
        }
    }
}

fn block_method(b: &sevenz_rust2::Block) -> String {
    // Shown last coder first, as 7-Zip builds the string backwards.
    let names: Vec<String> = b.coders.iter().map(|c| coder_name(c.encoder_method_id(), c.properties())).collect();
    names.into_iter().rev().collect::<Vec<_>>().join(" ")
}

fn map_7z_open(e: sevenz_rust2::Error) -> OpenError {
    use sevenz_rust2::Error as E;
    match e {
        E::BadSignature(_) => OpenError::NotArchive,
        E::PasswordRequired => OpenError::NeedPassword,
        E::MaybeBadPassword(_) => OpenError::WrongPassword,
        E::Io(e, _) | E::FileOpen(e, _) => {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                OpenError::Headers("Unexpected end of archive".into())
            } else {
                OpenError::Io(e)
            }
        }
        other => OpenError::Headers(format!("{other:?}")),
    }
}

fn map_7z_data(e: sevenz_rust2::Error) -> DataError {
    use sevenz_rust2::Error as E;
    match e {
        E::ChecksumVerificationFailed | E::NextHeaderCrcMismatch => DataError::Crc,
        // 7-Zip says "Data Error in encrypted file. Wrong password?" for a 7z.
        E::MaybeBadPassword(_) | E::PasswordRequired => DataError::Data,
        E::UnsupportedCompressionMethod(m) => DataError::Unsupported(m),
        E::Io(e, _) => {
            if let Some(inner) = e.get_ref() {
                if inner.to_string() == "stopped" {
                    return DataError::Stopped;
                }
            }
            if e.kind() != io::ErrorKind::BrokenPipe && e.raw_os_error().is_none() {
                if e.to_string().to_lowercase().contains("crc") || e.to_string().to_lowercase().contains("checksum") {
                    DataError::Crc
                } else {
                    DataError::Data
                }
            } else {
                DataError::Io(e)
            }
        }
        _ => DataError::Data,
    }
}

/// The name a single-file archive's item gets from the archive's name: without the suffix,
/// .tgz and the like becoming .tar.
fn single_item_name(arc: &str, fmt: Fmt) -> String {
    let base = arc.rsplit('/').next().unwrap_or(arc);
    let lower = base.to_ascii_lowercase();
    let pairs: &[(&str, &str)] = match fmt {
        Fmt::GZip => &[(".tgz", ".tar"), (".tpz", ".tar"), (".gz", ""), (".gzip", "")],
        Fmt::BZip2 => &[(".tbz2", ".tar"), (".tbz", ".tar"), (".bz2", ""), (".bzip2", "")],
        Fmt::Xz => &[(".txz", ".tar"), (".xz", "")],
        Fmt::Lzma => &[(".lzma", "")],
        Fmt::Zstd => &[(".tzst", ".tar"), (".zst", ""), (".zstd", "")],
        _ => &[],
    };
    for (suf, rep) in pairs {
        if lower.ends_with(suf) && base.len() > suf.len() {
            return format!("{}{}", &base[..base.len() - suf.len()], rep);
        }
    }
    base.to_string()
}

pub fn open(path: &str, want: Option<Fmt>, password: Option<&str>) -> Result<Opened, OpenError> {
    let mut f = File::open(path).map_err(OpenError::Io)?;
    let phys = f.metadata().map_err(OpenError::Io)?.len();
    let mut head = vec![0u8; 1024];
    let n = crate::common::peek(&mut f, 1024).map_err(OpenError::Io)?.len();
    f.seek(SeekFrom::Start(0)).map_err(OpenError::Io)?;
    let n2 = f.read(&mut head[..n]).map_err(OpenError::Io)?;
    head.truncate(n2);
    f.seek(SeekFrom::Start(0)).map_err(OpenError::Io)?;
    let detected = Fmt::detect(&head);
    let fmt = match want {
        Some(w) => {
            if detected != Some(w) && !(w == Fmt::Tar && detected.is_none() && phys == 0) {
                return Err(OpenError::NotArchive);
            }
            w
        }
        None => detected.ok_or(OpenError::NotArchive)?,
    };
    let mut props = vec![];
    let mut items = vec![];
    let inner = match fmt {
        Fmt::SevenZ => {
            let pw = sevenz_rust2::Password::from(password.unwrap_or(""));
            let archive = sevenz_rust2::Archive::read(&mut f, &pw).map_err(map_7z_open)?;
            let packed: u64 = archive.pack_sizes().iter().sum();
            props.push(("Physical Size".into(), phys.to_string()));
            props.push(("Headers Size".into(), phys.saturating_sub(packed).to_string()));
            // The archive's methods: unique ids in order, LZMA2 with its largest dictionary.
            let mut ids: Vec<u64> = vec![];
            let (mut lzma2, mut dic) = (0u8, 0u32);
            for b in &archive.blocks {
                for c in &b.coders {
                    let id = method_id(c.encoder_method_id());
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                    if id == 0x21 && c.properties().len() == 1 {
                        lzma2 = lzma2.max(c.properties()[0]);
                    }
                    if id == 0x030101 && c.properties().len() == 5 {
                        dic = dic.max(u32::from_le_bytes(c.properties()[1..5].try_into().unwrap()));
                    }
                }
            }
            ids.sort();
            if !ids.is_empty() {
                props.push(("Method".into(), ids.iter().map(|&i| archive_method_name(i, lzma2, dic)).collect::<Vec<_>>().join(" ")));
            }
            let solid = archive.stream_map.block_first_file_index.iter().enumerate().any(|(bi, _)| {
                archive.stream_map.file_block_index.iter().filter(|x| **x == Some(bi)).count() > 1
            });
            props.push(("Solid".into(), if solid { "+" } else { "-" }.into()));
            props.push(("Blocks".into(), archive.blocks.len().to_string()));
            let encrypted_blocks: Vec<bool> = archive.blocks.iter().map(|b| b.coders.iter().any(|c| method_id(c.encoder_method_id()) == 0x06F10701)).collect();
            let mut seen_block = vec![false; archive.blocks.len()];
            for (i, e) in archive.files.iter().enumerate() {
                let bi = archive.stream_map.file_block_index.get(i).copied().flatten();
                let mut it = Item {
                    path: e.name.clone(),
                    is_dir: e.is_directory,
                    size: Some(e.size),
                    mtime: e.has_last_modified_date.then(|| nt_to_unix(e.last_modified_date)),
                    ctime: e.has_creation_date.then(|| nt_to_unix(e.creation_date)),
                    atime: e.has_access_date.then(|| nt_to_unix(e.access_date)),
                    attrib: e.has_windows_attributes.then_some(e.windows_attributes),
                    crc: (e.has_crc && e.has_stream).then_some(e.crc as u32),
                    ..Default::default()
                };
                if let Some(b) = bi {
                    it.block = Some(b as u64);
                    it.method = Some(block_method(&archive.blocks[b]));
                    it.encrypted = encrypted_blocks[b];
                    if !seen_block[b] {
                        seen_block[b] = true;
                        let firsts = archive.stream_map.block_first_pack_stream_index();
                        let first = firsts[b];
                        let n = if b + 1 < firsts.len() { firsts[b + 1] - first } else { archive.pack_sizes().len() - first }.max(1);
                        it.packed = Some(archive.pack_sizes()[first..first + n].iter().sum());
                    }
                } else {
                    it.packed = if e.is_directory || !e.has_stream { Some(0) } else { None };
                    if !e.has_stream {
                        it.size = Some(0);
                    }
                }
                items.push(it);
            }
            Inner::SevenZ { archive }
        }
        Fmt::Zip => {
            let mut z = zip::ZipArchive::new(BufReader::new(&mut f)).map_err(|e| match e {
                zip::result::ZipError::InvalidArchive(_) => OpenError::NotArchive,
                zip::result::ZipError::Io(e) => OpenError::Io(e),
                other => OpenError::Headers(other.to_string()),
            })?;
            props.push(("Physical Size".into(), phys.to_string()));
            for i in 0..z.len() {
                let e = z.by_index_raw(i).map_err(|e| OpenError::Headers(e.to_string()))?;
                let mut m = match e.compression() {
                    zip::CompressionMethod::Stored => "Store".to_string(),
                    zip::CompressionMethod::Deflated => "Deflate".to_string(),
                    zip::CompressionMethod::Deflate64 => "Deflate64".to_string(),
                    zip::CompressionMethod::Bzip2 => "BZip2".to_string(),
                    zip::CompressionMethod::Lzma => "LZMA".to_string(),
                    zip::CompressionMethod::Xz => "xz".to_string(),
                    zip::CompressionMethod::Ppmd => "PPMd".to_string(),
                    zip::CompressionMethod::Aes => "AES".to_string(),
                    other => format!("{other:?}"),
                };
                if e.encrypted() {
                    m = if e.compression() == zip::CompressionMethod::Aes { m } else { format!("ZipCrypto {m}") };
                }
                let (nt_m, nt_a, nt_c) = zip_extra_times(e.extra_data().unwrap_or(&[]));
                let mtime = nt_m.or_else(|| {
                    e.last_modified().and_then(|d| {
                        civil_to_unix(d.year() as i64, d.month() as u32, d.day() as u32, d.hour() as u32, d.minute() as u32, d.second() as u32)
                            .map(|s| (s - local_offset(s), 0u32))
                    })
                });
                let mode = e.unix_mode();
                let is_dir = e.is_dir();
                items.push(Item {
                    path: e.name().trim_end_matches('/').to_string(),
                    is_dir,
                    size: Some(e.size()),
                    packed: Some(e.compressed_size()),
                    mtime,
                    atime: nt_a,
                    ctime: nt_c,
                    attrib: Some(if is_dir { 0x10 } else { 0 } | mode.map(|m| 0x8000 | (m << 16)).unwrap_or(0)),
                    mode,
                    crc: (!is_dir).then_some(e.crc32()),
                    encrypted: e.encrypted(),
                    method: Some(m),
                    ..Default::default()
                });
            }
            for it in items.iter_mut() {
                if it.is_symlink() && !it.is_dir {
                    // The target is the entry's data; read when extracting.
                }
            }
            drop(z);
            Inner::Zip
        }
        Fmt::Tar => {
            let mut a = tar::Archive::new(BufReader::new(&mut f));
            let mut end = 0u64;
            let (mut gnu, mut posix) = (false, false);
            let entries = a.entries().map_err(OpenError::Io)?;
            for e in entries {
                let e = e.map_err(|e| OpenError::Headers(errstr(&e)))?;
                let h = e.header();
                gnu |= h.as_gnu().is_some();
                posix |= h.as_ustar().is_some();
                let path = e.path().map_err(|e| OpenError::Headers(errstr(&e)))?.to_string_lossy().trim_end_matches('/').to_string();
                let et = h.entry_type();
                let is_dir = et.is_dir();
                let mode = h.mode().ok();
                let size_on = if et.is_symlink() || et.is_hard_link() || is_dir { 0 } else { e.size() };
                let ty_bits = match et {
                    tar::EntryType::Directory => 0o040000,
                    tar::EntryType::Symlink => 0o120000,
                    tar::EntryType::Char => 0o020000,
                    tar::EntryType::Block => 0o060000,
                    tar::EntryType::Fifo => 0o010000,
                    _ => 0o100000,
                };
                let size = e.size();
                end = e.raw_file_position() + size.div_ceil(512) * 512 + 1024;
                items.push(Item {
                    path,
                    is_dir,
                    size: Some(size_on),
                    packed: Some(size_on.div_ceil(512) * 512),
                    mtime: h.mtime().ok().map(|t| (t as i64, 0)),
                    mode: mode.map(|m| (m & 0o7777) | ty_bits),
                    link: if et.is_symlink() || et.is_hard_link() { e.link_name().ok().flatten().map(|l| l.to_string_lossy().into_owned()) } else { None },
                    user: h.username().ok().flatten().map(String::from),
                    group: h.groupname().ok().flatten().map(String::from),
                    ..Default::default()
                });
            }
            let phys_t = phys.max(end);
            props.push(("Physical Size".into(), phys_t.to_string()));
            props.push(("Headers Size".into(), phys_t.saturating_sub(items.iter().map(|i| i.packed.unwrap_or(0)).sum::<u64>()).to_string()));
            props.push(("Code Page".into(), "UTF-8".into()));
            let mut ch = vec![];
            if gnu {
                ch.push("GNU");
            } else if posix {
                ch.push("POSIX");
            }
            if items.iter().all(|i| i.path.is_ascii() && i.link.as_deref().map_or(true, |l| l.is_ascii())) {
                ch.push("ASCII");
            }
            if !ch.is_empty() {
                props.push(("Characteristics".into(), ch.join(" ")));
            }
            Inner::Tar
        }
        _ => {
            let (name, size, mtime, headers, extra) = single_info(&mut f, fmt, path, phys).map_err(OpenError::Io)?;
            if fmt != Fmt::BZip2 {
                props.push(("Physical Size".into(), phys.to_string()));
            }
            if let Some(h) = headers {
                props.push(("Headers Size".into(), h.to_string()));
            }
            props.extend(extra);
            let packed = if fmt == Fmt::BZip2 { None } else { Some(phys) };
            items.push(Item { path: name, size, packed, mtime, ..Default::default() });
            Inner::Single
        }
    };
    if fmt == Fmt::GZip {
        // 7-Zip shows no Physical Size for gzip.
        props.retain(|(k, _)| k != "Physical Size");
    }
    Ok(Opened { fmt, path: path.to_string(), props, items, inner, password: password.map(String::from) })
}

/// The times in a zip entry's extra fields: NTFS (0x000A, FILETIMEs: modified, accessed,
/// created) as 7-Zip writes them, else the extended timestamp (0x5455, Unix seconds).
fn zip_extra_times(x: &[u8]) -> (Option<(i64, u32)>, Option<(i64, u32)>, Option<(i64, u32)>) {
    let mut p = 0;
    let mut ext: (Option<(i64, u32)>, Option<(i64, u32)>, Option<(i64, u32)>) = (None, None, None);
    while p + 4 <= x.len() {
        let id = u16::from_le_bytes([x[p], x[p + 1]]);
        let len = u16::from_le_bytes([x[p + 2], x[p + 3]]) as usize;
        let body = &x[(p + 4).min(x.len())..(p + 4 + len).min(x.len())];
        if id == 0x000A && body.len() >= 32 && u16::from_le_bytes([body[4], body[5]]) == 1 {
            let ft = |o: usize| {
                let v = u64::from_le_bytes(body[o..o + 8].try_into().unwrap());
                (v != 0).then(|| nt_to_unix(sevenz_rust2::NtTime::from(v)))
            };
            return (ft(8), ft(16), ft(24));
        }
        if id == 0x5455 && !body.is_empty() {
            let flags = body[0];
            let mut q = 1;
            let mut next = |present: bool| -> Option<(i64, u32)> {
                if present && q + 4 <= body.len() {
                    let v = i32::from_le_bytes(body[q..q + 4].try_into().unwrap()) as i64;
                    q += 4;
                    Some((v, 0))
                } else {
                    None
                }
            };
            let m = next(flags & 1 != 0);
            let a = next(flags & 2 != 0);
            let c = next(flags & 4 != 0);
            ext = (m, a, c);
        }
        p += 4 + len;
    }
    ext
}

#[allow(clippy::type_complexity)]
fn single_info(f: &mut File, fmt: Fmt, path: &str, phys: u64) -> io::Result<(String, Option<u64>, Option<(i64, u32)>, Option<u64>, Vec<(String, String)>)> {
    let mut extra = vec![];
    let mut name = single_item_name(path, fmt);
    let mut size = None;
    let mut mtime = None;
    let mut headers = None;
    match fmt {
        Fmt::GZip => {
            let mut h = [0u8; 10];
            f.read_exact(&mut h)?;
            let flags = h[3];
            let t = u32::from_le_bytes(h[4..8].try_into().unwrap());
            if t != 0 {
                mtime = Some((t as i64, 0));
            }
            let mut hl = 10u64;
            if flags & 4 != 0 {
                let mut x = [0u8; 2];
                f.read_exact(&mut x)?;
                let xl = u16::from_le_bytes(x) as i64;
                f.seek(SeekFrom::Current(xl))?;
                hl += 2 + xl as u64;
            }
            if flags & 8 != 0 {
                let mut n = vec![];
                let mut b = [0u8; 1];
                loop {
                    f.read_exact(&mut b)?;
                    hl += 1;
                    if b[0] == 0 {
                        break;
                    }
                    n.push(b[0]);
                }
                name = String::from_utf8_lossy(&n).into_owned();
            }
            headers = Some(hl);
            if phys >= 18 {
                f.seek(SeekFrom::End(-4))?;
                let mut s = [0u8; 4];
                f.read_exact(&mut s)?;
                size = Some(u32::from_le_bytes(s) as u64);
            }
        }
        Fmt::Xz => {
            if let Ok(info) = crate::xzindex::parse(f) {
                size = Some(info.uncompressed_size());
                // The first Block Header's filters, as 7-Zip shows them, then the check.
                let mut method = vec![];
                if let Some(b) = info.streams.first().and_then(|s| s.blocks.first()) {
                    f.seek(SeekFrom::Start(b.compressed_file_offset))?;
                    let mut hb = [0u8; 1024];
                    let n = f.read(&mut hb)?;
                    let hsize = (hb[0] as usize + 1) * 4;
                    if n >= hsize && hsize >= 8 {
                        let flags = hb[1];
                        let nfilt = (flags & 3) as usize + 1;
                        let mut p = 2usize;
                        let vli = |p: &mut usize| -> u64 {
                            let mut v = 0u64;
                            for k in 0..9 {
                                let byte = hb[*p];
                                *p += 1;
                                v |= ((byte & 0x7F) as u64) << (7 * k);
                                if byte & 0x80 == 0 {
                                    break;
                                }
                            }
                            v
                        };
                        if flags & 0x40 != 0 {
                            vli(&mut p);
                        }
                        if flags & 0x80 != 0 {
                            vli(&mut p);
                        }
                        let mut names = vec![];
                        for _ in 0..nfilt {
                            if p >= hsize {
                                break;
                            }
                            let id = vli(&mut p);
                            let psize = vli(&mut p) as usize;
                            let props = &hb[p..(p + psize).min(hsize)];
                            p += psize;
                            names.push(match id {
                                0x21 => format!("LZMA2:{}", lzma2_string(props.first().copied().unwrap_or(0))),
                                0x03 => format!("Delta:{}", props.first().map(|d| *d as u32 + 1).unwrap_or(1)),
                                0x04 => "BCJ".into(),
                                0x05 => "PPC".into(),
                                0x06 => "IA64".into(),
                                0x07 => "ARM".into(),
                                0x08 => "ARMT".into(),
                                0x09 => "SPARC".into(),
                                0x0A => "ARM64".into(),
                                0x0B => "RISCV".into(),
                                other => format!("{other:X}"),
                            });
                        }
                        names.reverse();
                        method.extend(names);
                    }
                }
                let check = info.streams.first().map(|s| s.check).unwrap_or(0);
                method.push(match check {
                    0 => "NoCheck".into(),
                    1 => "CRC32".into(),
                    4 => "CRC64".into(),
                    10 => "SHA256".into(),
                    c => format!("Check{c}"),
                });
                extra.push(("Method".into(), method.join(" ")));
                extra.push(("Streams".into(), info.streams.len().to_string()));
                extra.push(("Blocks".into(), info.block_count().to_string()));
            }
        }
        Fmt::Lzma => {
            let mut h = [0u8; 13];
            f.read_exact(&mut h)?;
            let s = u64::from_le_bytes(h[5..13].try_into().unwrap());
            if s != u64::MAX {
                size = Some(s);
            }
        }
        Fmt::Zstd => {
            let mut h = [0u8; 18];
            let n = f.read(&mut h)?;
            if let Ok(info) = structured_zstd::decoding::read_frame_header_info(&h[..n], false) {
                if let structured_zstd::decoding::FrameContentSize::Known(s) = info.content_size {
                    size = Some(s);
                }
            }
        }
        _ => {}
    }
    f.seek(SeekFrom::Start(0))?;
    Ok((name, size, mtime, headers, extra))
}

/// Days from civil (Howard Hinnant's algorithm): seconds since the epoch, UTC.
pub fn civil_to_unix(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64)
}

pub fn unix_to_civil(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, (secs / 3600) as u32, (secs / 60 % 60) as u32, (secs % 60) as u32)
}

/// The local time zone's offset from UTC at `t`, in seconds (0 in the guest unless TZ says).
pub fn local_offset(t: i64) -> i64 {
    unsafe {
        let tt: libc::time_t = t as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&tt, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

/// What an extraction callback is given for each selected item.
pub enum Data<'a> {
    Dir,
    File(&'a mut dyn Read),
    Link(String),
    /// Not asked for, but decoded on the way to what is (a solid block): 7-Zip counts it.
    Passed(&'a mut dyn Read),
}

pub type Sink<'a> = dyn FnMut(usize, &Item, Data) -> Result<(), DataError> + 'a;

impl Opened {
    /// Hands each selected item (in archive order) to `f` with its data; errors in one item's
    /// data come back through `f`'s Result for that item: `on_error` is told and the next item
    /// follows where the format allows.
    pub fn extract(&mut self, want: &dyn Fn(usize) -> bool, f: &mut Sink, on_error: &mut dyn FnMut(usize, DataError)) -> Result<(), DataError> {
        let mut file = File::open(&self.path).map_err(DataError::Io)?;
        match &mut self.inner {
            Inner::SevenZ { archive } => {
                let pw = sevenz_rust2::Password::from(self.password.as_deref().unwrap_or(""));
                let items = &self.items;
                let fbi = &archive.stream_map.file_block_index;
                // Items without data first (folders, empty files), in index order.
                for (i, e) in archive.files.iter().enumerate() {
                    if fbi.get(i).copied().flatten().is_some() || !want(i) {
                        continue;
                    }
                    let item = &items[i];
                    let data = if e.is_directory {
                        Data::Dir
                    } else if item.is_symlink() {
                        Data::Link(String::new())
                    } else {
                        Data::File(&mut io::empty())
                    };
                    match f(i, item, data) {
                        Ok(()) => {}
                        Err(DataError::Stopped) => return Ok(()),
                        Err(e) => on_error(i, e),
                    }
                }
                let mut src = BufReader::with_capacity(1 << 16, &mut file);
                let base = archive.files.as_ptr() as usize;
                let size = std::mem::size_of::<sevenz_rust2::ArchiveEntry>();
                for b in 0..archive.blocks.len() {
                    let members: Vec<usize> = (0..archive.files.len()).filter(|&i| fbi[i] == Some(b)).collect();
                    let Some(&last) = members.iter().rev().find(|&&i| want(i)) else { continue };
                    let mut current: Option<usize> = None;
                    let mut stop = false;
                    let dec = sevenz_rust2::BlockDecoder::new(1, b, archive, &pw, &mut src);
                    let r = dec.for_each_entries(&mut |entry: &sevenz_rust2::ArchiveEntry, rd: &mut dyn Read| {
                        let i = (entry as *const _ as usize - base) / size;
                        current = Some(i);
                        let item = &items[i];
                        let mut tagged = Tagged { inner: rd, failed: None };
                        if i > last {
                            return Ok(false);
                        }
                        if !want(i) {
                            return match f(i, item, Data::Passed(&mut tagged)) {
                                Err(_) if tagged.failed.is_some() => Err(sevenz_rust2::Error::from(tagged.failed.take().unwrap())),
                                _ => Ok(true),
                            };
                        }
                        let data = if item.is_symlink() {
                            let mut t = vec![];
                            tagged.read_to_end(&mut t)?;
                            Data::Link(String::from_utf8_lossy(&t).into_owned())
                        } else {
                            Data::File(&mut tagged)
                        };
                        match f(i, item, data) {
                            Ok(()) => Ok(true),
                            Err(DataError::Stopped) => {
                                stop = true;
                                Ok(false)
                            }
                            Err(e) => {
                                if let Some(re) = tagged.failed.take() {
                                    // The data could not be read: the block is damaged from here.
                                    return Err(sevenz_rust2::Error::from(re));
                                }
                                on_error(i, e);
                                io::copy(&mut tagged, &mut io::sink())?;
                                Ok(true)
                            }
                        }
                    });
                    if stop {
                        return Ok(());
                    }
                    if let Err(e) = r {
                        let kind = map_7z_data(e);
                        let from = current.and_then(|c| members.iter().position(|&m| m == c)).unwrap_or(0);
                        for &m in &members[from..] {
                            if want(m) && items[m].size.unwrap_or(0) > 0 {
                                on_error(m, match &kind {
                                    DataError::Crc => DataError::Crc,
                                    DataError::WrongPassword => DataError::WrongPassword,
                                    DataError::Unsupported(u) => DataError::Unsupported(u.clone()),
                                    _ => DataError::Data,
                                });
                            }
                        }
                    }
                }
                Ok(())
            }
            Inner::Zip => {
                let mut z = zip::ZipArchive::new(BufReader::new(&mut file)).map_err(|e| DataError::Unsupported(e.to_string()))?;
                for i in 0..self.items.len() {
                    if !want(i) {
                        continue;
                    }
                    let item = &self.items[i];
                    let res = if item.encrypted {
                        match &self.password {
                            Some(p) => z.by_index_decrypt(i, p.as_bytes()),
                            None => {
                                on_error(i, DataError::WrongPassword);
                                continue;
                            }
                        }
                    } else {
                        z.by_index(i)
                    };
                    let mut zf = match res {
                        Ok(zf) => zf,
                        Err(zip::result::ZipError::InvalidPassword) => {
                            on_error(i, DataError::WrongPassword);
                            continue;
                        }
                        Err(zip::result::ZipError::UnsupportedArchive(m)) => {
                            on_error(i, DataError::Unsupported(m.to_string()));
                            continue;
                        }
                        Err(e) => {
                            on_error(i, DataError::Unsupported(e.to_string()));
                            continue;
                        }
                    };
                    let data = if item.is_dir {
                        Data::Dir
                    } else if item.is_symlink() {
                        let mut t = vec![];
                        if let Err(e) = zf.read_to_end(&mut t) {
                            on_error(i, zip_io(e));
                            continue;
                        }
                        Data::Link(String::from_utf8_lossy(&t).into_owned())
                    } else {
                        Data::File(&mut zf)
                    };
                    match f(i, item, data) {
                        Ok(()) => {}
                        Err(DataError::Stopped) => return Ok(()),
                        Err(DataError::Io(e)) if e.kind() == io::ErrorKind::InvalidData || e.to_string().contains("checksum") => {
                            on_error(i, zip_io(e))
                        }
                        Err(e) => on_error(i, e),
                    }
                }
                Ok(())
            }
            Inner::Tar => {
                let mut a = tar::Archive::new(BufReader::new(&mut file));
                let entries = a.entries().map_err(DataError::Io)?;
                for (i, e) in entries.enumerate() {
                    let mut e = match e {
                        Ok(e) => e,
                        Err(err) => {
                            on_error(i, DataError::Io(err));
                            return Ok(());
                        }
                    };
                    if i >= self.items.len() || !want(i) {
                        continue;
                    }
                    let item = &self.items[i];
                    let data = if item.is_dir {
                        Data::Dir
                    } else if let Some(l) = &item.link {
                        Data::Link(l.clone())
                    } else {
                        Data::File(&mut e)
                    };
                    match f(i, item, data) {
                        Ok(()) => {}
                        Err(DataError::Stopped) => return Ok(()),
                        Err(err) => on_error(i, err),
                    }
                }
                Ok(())
            }
            Inner::Single => {
                if !want(0) {
                    return Ok(());
                }
                let item = &self.items[0];
                let mut input = BufReader::with_capacity(1 << 16, &mut file);
                let r = match self.fmt {
                    Fmt::GZip => {
                        let mut d = flate2::bufread::MultiGzDecoder::new(&mut input);
                        f(0, item, Data::File(&mut d))
                    }
                    Fmt::BZip2 => {
                        let mut d = bzip2::bufread::MultiBzDecoder::new(&mut input);
                        f(0, item, Data::File(&mut d))
                    }
                    Fmt::Xz => {
                        let mut d = lzma_rust2::XzReader::new(&mut input, true);
                        f(0, item, Data::File(&mut d))
                    }
                    Fmt::Lzma => match lzma_rust2::LzmaReader::new_mem_limit(&mut input, u32::MAX, None) {
                        Ok(mut d) => f(0, item, Data::File(&mut d)),
                        Err(e) => Err(DataError::Io(e)),
                    },
                    Fmt::Zstd => {
                        let mut d = ZstdMulti { input: Some(&mut input), dec: None };
                        f(0, item, Data::File(&mut d))
                    }
                    _ => unreachable!(),
                };
                if let Err(e) = r {
                    match e {
                        DataError::Stopped => {}
                        DataError::Io(e) if e.kind() != io::ErrorKind::BrokenPipe && e.raw_os_error().is_none() => on_error(0, DataError::Data),
                        other => on_error(0, other),
                    }
                }
                Ok(())
            }
        }
    }
}

/// A reader that remembers its own failure, so a failed copy can be told apart: the archive's
/// data (damaged) or the destination (disk full, ...).
struct Tagged<'a> {
    inner: &'a mut dyn Read,
    failed: Option<io::Error>,
}

impl Read for Tagged<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                let copy = io::Error::new(e.kind(), e.to_string());
                self.failed = Some(e);
                Err(copy)
            }
        }
    }
}

fn zip_io(e: io::Error) -> DataError {
    let m = e.to_string().to_lowercase();
    if m.contains("checksum") || m.contains("crc") {
        DataError::Crc
    } else if m.contains("password") {
        DataError::WrongPassword
    } else if e.raw_os_error().is_some() {
        DataError::Io(e)
    } else {
        DataError::Data
    }
}

/// Zstandard frames one after another (skippable frames skipped), checksums verified.
struct ZstdMulti<R: io::BufRead> {
    input: Option<R>,
    dec: Option<structured_zstd::decoding::StreamingDecoder<R, structured_zstd::decoding::FrameDecoder>>,
}

impl<R: io::BufRead> Read for ZstdMulti<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(d) = self.dec.as_mut() {
                let n = d.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                // The frame is done: the reader comes back for what follows.
                self.input = Some(self.dec.take().unwrap().into_inner());
            }
            let mut r = self.input.take().expect("reader");
            if r.fill_buf()?.is_empty() {
                self.input = Some(r);
                return Ok(0);
            }
            let mut head = [0u8; 8];
            let avail = r.fill_buf()?;
            let n = avail.len().min(8);
            head[..n].copy_from_slice(&avail[..n]);
            let magic = u32::from_le_bytes(head[..4].try_into().unwrap());
            if n >= 8 && magic & 0xFFFF_FFF0 == 0x184D_2A50 {
                let len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as u64;
                r.consume(8);
                io::copy(&mut (&mut r).take(len), &mut io::sink())?;
                self.input = Some(r);
                continue;
            }
            match structured_zstd::decoding::StreamingDecoder::new(r) {
                Ok(mut d) => {
                    d.decoder_mut().set_content_checksum(structured_zstd::decoding::ContentChecksum::Verify);
                    self.dec = Some(d);
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))),
            }
        }
    }
}

// Writing

/// Where a new archive item's data comes from.
pub enum Source {
    Disk(PathBuf),
    /// Extracted from the old archive into the work directory.
    Temp(PathBuf),
    None,
}

pub struct NewItem {
    pub name: String,
    pub is_dir: bool,
    pub source: Source,
    pub size: u64,
    pub mtime: (i64, u32),
    pub atime: Option<(i64, u32)>,
    pub ctime: Option<(i64, u32)>,
    pub mode: u32,
    pub link: Option<String>,
    /// Read from the disk (not kept from the old archive).
    pub from_disk: bool,
}

#[derive(Clone, Debug)]
pub struct Methods {
    pub level: u32,
    pub method: Option<String>,
    pub solid: bool,
    pub header_encryption: bool,
    pub password: Option<String>,
    pub threads: u32,
    pub dict: Option<u32>,
    pub filter: Option<String>,
    pub zip_aes: bool,
    pub store_times: (bool, bool, bool), // mtime, ctime, atime
}

impl Default for Methods {
    fn default() -> Self {
        Methods { level: 5, method: None, solid: true, header_encryption: false, password: None, threads: 0, dict: None, filter: None, zip_aes: false, store_times: (true, false, false) }
    }
}

fn open_source(s: &Source) -> io::Result<Box<dyn Read>> {
    Ok(match s {
        Source::Disk(p) | Source::Temp(p) => Box::new(File::open(p)?),
        Source::None => Box::new(io::empty()),
    })
}

/// 7-Zip's LZMA/LZMA2 settings for -mx (LzmaEncProps_Normalize), the dictionary reduced to the
/// data's size as 7-Zip does (2^n or 3*2^(n-1), at least 4 KiB).
fn seven_lzma(level: u32, dict: Option<u32>, total: u64) -> lzma_rust2::LzmaOptions {
    let l = level.min(9);
    let mut d = dict.unwrap_or(if l <= 3 {
        1 << (l * 2 + 16)
    } else if l <= 6 {
        1 << (l + 19)
    } else if l == 7 {
        1 << 25
    } else {
        1 << 26
    });
    if dict.is_none() && total < d as u64 {
        for i in 11..=30 {
            if total <= 2u64 << i {
                d = 2 << i;
                break;
            }
            if total <= 3u64 << i {
                d = 3 << i;
                break;
            }
        }
    }
    let mut o = lzma_rust2::LzmaOptions::with_preset(6);
    o.dict_size = d;
    o.lc = 3;
    o.lp = 0;
    o.pb = 2;
    if l < 5 {
        o.mode = lzma_rust2::EncodeMode::Fast;
        o.mf = lzma_rust2::MfType::Hc4;
        o.depth_limit = 0;
    } else {
        o.mode = lzma_rust2::EncodeMode::Normal;
        o.mf = lzma_rust2::MfType::Bt4;
        o.depth_limit = 0;
    }
    o.nice_len = if l < 7 { 32 } else { 64 };
    o
}

fn seven_methods(m: &Methods, total: u64) -> Result<Vec<sevenz_rust2::EncoderConfiguration>, String> {
    use sevenz_rust2::encoder_options::*;
    use sevenz_rust2::{EncoderConfiguration as C, EncoderMethod as M};
    let name = m.method.clone().unwrap_or_else(|| if m.level == 0 { "copy".into() } else { "lzma2".into() }).to_ascii_lowercase();
    let main = match name.as_str() {
        "copy" => C::new(M::COPY),
        "lzma2" => {
            let mut o = Lzma2Options::from_level(m.level.min(9));
            let l = seven_lzma(m.level, m.dict, total);
            o.set_dictionary_size(l.dict_size);
            o.set_nice_len(l.nice_len);
            C::new(M::LZMA2).with_options(EncoderOptions::Lzma2(o))
        }
        "lzma" => {
            let mut o = LzmaOptions::from_level(m.level.min(9));
            let l = seven_lzma(m.level, m.dict, total);
            o.set_dictionary_size(l.dict_size);
            o.set_nice_len(l.nice_len);
            C::new(M::LZMA).with_options(EncoderOptions::Lzma(o))
        }
        "bzip2" => C::new(M::BZIP2).with_options(EncoderOptions::Bzip2(Bzip2Options::from_level(m.level.clamp(1, 9)))),
        "deflate" => C::new(M::DEFLATE).with_options(EncoderOptions::Deflate(DeflateOptions::from_level(m.level.min(9)))),
        "ppmd" => C::new(M::PPMD).with_options(EncoderOptions::Ppmd(PpmdOptions::from_level(m.level.clamp(1, 9)))),
        other => return Err(format!("Unsupported Method : {other}")),
    };
    let mut v = vec![];
    if let Some(p) = &m.password {
        v.push(C::new(M::AES256_SHA256).with_options(EncoderOptions::Aes(AesEncoderOptions::new(sevenz_rust2::Password::from(p.as_str())))));
    }
    v.push(main);
    match m.filter.as_deref().map(|s| s.to_ascii_lowercase()) {
        None => {}
        Some(f) if f == "off" || f == "-" => {}
        Some(f) => {
            let filt = match f.as_str() {
                "bcj" | "x86" | "on" | "+" => M::BCJ_X86_FILTER,
                "arm" => M::BCJ_ARM_FILTER,
                "armt" => M::BCJ_ARM_THUMB_FILTER,
                "arm64" => M::BCJ_ARM64_FILTER,
                "ppc" => M::BCJ_PPC_FILTER,
                "ia64" => M::BCJ_IA64_FILTER,
                "sparc" => M::BCJ_SPARC_FILTER,
                "riscv" => M::BCJ_RISCV_FILTER,
                d if d.starts_with("delta") => {
                    let dist: u32 = d.trim_start_matches("delta").trim_start_matches(':').parse().unwrap_or(1);
                    v.push(C::new(M::DELTA_FILTER).with_options(EncoderOptions::Delta(DeltaOptions::from_distance(dist))));
                    return Ok(v);
                }
                other => return Err(format!("Unsupported Method : {other}")),
            };
            v.push(C::new(filt));
        }
    }
    Ok(v)
}

fn seven_entry(it: &NewItem, m: &Methods) -> sevenz_rust2::ArchiveEntry {
    let mut e = if it.is_dir { sevenz_rust2::ArchiveEntry::new_directory(&it.name) } else { sevenz_rust2::ArchiveEntry::new_file(&it.name) };
    let ty = if it.is_dir {
        0o040000
    } else if it.link.is_some() {
        0o120000
    } else {
        0o100000
    };
    e.has_windows_attributes = true;
    e.windows_attributes = if it.is_dir { 0x10 } else { 0x20 } | 0x8000 | (((it.mode & 0o7777) | ty) << 16);
    if m.store_times.0 {
        e.has_last_modified_date = true;
        e.last_modified_date = unix_to_nt(it.mtime.0, it.mtime.1);
    }
    if let (true, Some(c)) = (m.store_times.1, it.ctime) {
        e.has_creation_date = true;
        e.creation_date = unix_to_nt(c.0, c.1);
    }
    if let (true, Some(a)) = (m.store_times.2, it.atime) {
        e.has_access_date = true;
        e.access_date = unix_to_nt(a.0, a.1);
    }
    e
}

/// Reads an item's data (a link: its target).
fn item_reader(it: &NewItem) -> io::Result<Box<dyn Read>> {
    if let Some(t) = &it.link {
        return Ok(Box::new(io::Cursor::new(t.clone().into_bytes())));
    }
    open_source(&it.source)
}

pub fn write(fmt: Fmt, out: &mut (impl Write + Seek), items: &[NewItem], m: &Methods) -> Result<(), String> {
    match fmt {
        Fmt::SevenZ => {
            let total: u64 = items.iter().map(|i| i.size).sum();
            let mut w = sevenz_rust2::ArchiveWriter::new(&mut *out).map_err(|e| format!("{e:?}"))?;
            w.set_content_methods(seven_methods(m, total)?);
            w.set_encrypt_header(m.header_encryption && m.password.is_some());
            // Directories first, in their order; then the files.
            for it in items.iter().filter(|i| i.is_dir) {
                w.push_archive_entry::<&[u8]>(seven_entry(it, m), None).map_err(|e| format!("{e:?}"))?;
            }
            let files: Vec<&NewItem> = items.iter().filter(|i| !i.is_dir).collect();
            let (empty, data): (Vec<&NewItem>, Vec<&NewItem>) = files.into_iter().partition(|i| i.size == 0 && i.link.is_none());
            if m.solid && data.len() > 1 {
                let mut entries = vec![];
                let mut readers = vec![];
                for it in &data {
                    entries.push(seven_entry(it, m));
                    readers.push(sevenz_rust2::SourceReader::new(LazyItem { item: it, r: None }));
                }
                w.push_archive_entries(entries, readers).map_err(|e| format!("{e:?}"))?;
            } else {
                for it in &data {
                    let r = item_reader(it).map_err(|e| format!("{}: {}", it.name, errstr(&e)))?;
                    w.push_archive_entry(seven_entry(it, m), Some(r)).map_err(|e| format!("{e:?}"))?;
                }
            }
            for it in &empty {
                w.push_archive_entry::<&[u8]>(seven_entry(it, m), None).map_err(|e| format!("{e:?}"))?;
            }
            w.finish().map_err(|e| errstr(&e))?;
        }
        Fmt::Zip => {
            let mut z = zip::ZipWriter::new(&mut *out);
            for it in items {
                let (y, mo, d, h, mi, s) = unix_to_civil(it.mtime.0 + local_offset(it.mtime.0));
                let dt = zip::DateTime::from_date_and_time(y.clamp(1980, 2107) as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8)
                    .unwrap_or_else(|_| zip::DateTime::default());
                let method = match m.method.as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
                    _ if m.level == 0 => zip::CompressionMethod::Stored,
                    None | Some("deflate") => zip::CompressionMethod::Deflated,
                    Some("copy") | Some("store") => zip::CompressionMethod::Stored,
                    Some("deflate64") => zip::CompressionMethod::Deflated,
                    Some("bzip2") => zip::CompressionMethod::Bzip2,
                    Some("lzma") => zip::CompressionMethod::Lzma,
                    Some("xz") => zip::CompressionMethod::Xz,
                    Some("ppmd") => zip::CompressionMethod::Ppmd,
                    Some(other) => return Err(format!("Unsupported Method : {other}")),
                };
                let mut opts = zip::write::FullFileOptions::default()
                    .compression_method(method)
                    .last_modified_time(dt)
                    .unix_permissions(it.mode & 0o7777)
                    .large_file(it.size >= 0xFFFF_FFFF);
                if method != zip::CompressionMethod::Stored {
                    opts = opts.compression_level(Some(m.level.clamp(1, 9) as i64));
                }
                // The exact times, as 7-Zip stores them (the DOS time has two-second steps).
                let ft = |t: (i64, u32)| u64::from(unix_to_nt(t.0, t.1)).to_le_bytes();
                let mut ntfs = vec![0u8; 4];
                ntfs.extend_from_slice(&1u16.to_le_bytes());
                ntfs.extend_from_slice(&24u16.to_le_bytes());
                ntfs.extend_from_slice(&ft(it.mtime));
                ntfs.extend_from_slice(&ft(it.atime.unwrap_or(it.mtime)));
                ntfs.extend_from_slice(&ft(it.ctime.unwrap_or(it.mtime)));
                opts.add_extra_data(0x000A, ntfs, false).map_err(|e| e.to_string())?;
                if it.is_dir {
                    z.add_directory(format!("{}/", it.name), opts.clone()).map_err(|e| e.to_string())?;
                    continue;
                }
                if let Some(t) = &it.link {
                    z.add_symlink(&it.name, t, opts.clone()).map_err(|e| e.to_string())?;
                    continue;
                }
                let r = match &m.password {
                    Some(p) if m.zip_aes => {
                        z.start_file(&it.name, opts.clone().with_aes_encryption(zip::AesMode::Aes256, p)).map_err(|e| e.to_string())?;
                        item_reader(it)
                    }
                    Some(p) => {
                        use zip::unstable::write::FileOptionsExt;
                        z.start_file(&it.name, opts.clone().with_deprecated_encryption(p.as_bytes()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                        item_reader(it)
                    }
                    None => {
                        z.start_file(&it.name, opts.clone()).map_err(|e| e.to_string())?;
                        item_reader(it)
                    }
                };
                let mut r = r.map_err(|e| format!("{}: {}", it.name, errstr(&e)))?;
                io::copy(&mut r, &mut z).map_err(|e| errstr(&e))?;
            }
            z.finish().map_err(|e| e.to_string())?;
        }
        Fmt::Tar => write_tar(out, items)?,
        Fmt::GZip | Fmt::BZip2 | Fmt::Xz | Fmt::Lzma => {
            let files: Vec<&NewItem> = items.iter().filter(|i| !i.is_dir).collect();
            if files.len() != 1 {
                return Err("E_INVALIDARG".into());
            }
            let it = files[0];
            let mut r = item_reader(it).map_err(|e| format!("{}: {}", it.name, errstr(&e)))?;
            write_single(fmt, &mut r, out, it, m)?;
        }
        Fmt::Zstd => return Err("E_NOTIMPL".into()),
    }
    Ok(())
}

/// A file opened only when the solid block reaches it.
struct LazyItem<'a> {
    item: &'a NewItem,
    r: Option<Box<dyn Read>>,
}

impl Read for LazyItem<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.r.is_none() {
            self.r = Some(item_reader(self.item)?);
        }
        let n = self.r.as_mut().unwrap().read(buf)?;
        if n == 0 {
            self.r = None;
        }
        Ok(n)
    }
}

pub fn write_single(fmt: Fmt, r: &mut dyn Read, out: &mut dyn Write, it: &NewItem, m: &Methods) -> Result<(), String> {
    let lvl = m.level.min(9);
    match fmt {
        Fmt::GZip => {
            let mut b = flate2::GzBuilder::new().mtime(it.mtime.0.max(0) as u32);
            if !it.name.is_empty() {
                b = b.filename(it.name.rsplit('/').next().unwrap_or(&it.name).as_bytes());
            }
            let mut e = b.write(&mut *out, flate2::Compression::new(lvl));
            io::copy(r, &mut e).map_err(|e| errstr(&e))?;
            e.finish().map_err(|e| errstr(&e))?;
        }
        Fmt::BZip2 => {
            let mut e = bzip2::write::BzEncoder::new(&mut *out, bzip2::Compression::new(lvl.clamp(1, 9)));
            io::copy(r, &mut e).map_err(|e| errstr(&e))?;
            e.finish().map_err(|e| errstr(&e))?;
        }
        Fmt::Xz => {
            let mut o = lzma_rust2::XzOptions::with_preset(lvl);
            o.lzma_options = seven_lzma(m.level, m.dict, it.size);
            let mut w = lzma_rust2::XzWriter::new(&mut *out, o).map_err(|e| errstr(&e))?;
            io::copy(r, &mut w).map_err(|e| errstr(&e))?;
            w.finish().map_err(|e| errstr(&e))?;
        }
        Fmt::Lzma => {
            let o = seven_lzma(m.level, m.dict, it.size);
            let mut w = lzma_rust2::LzmaWriter::new_use_header(&mut *out, &o, Some(it.size)).map_err(|e| errstr(&e))?;
            io::copy(r, &mut w).map_err(|e| errstr(&e))?;
            w.finish().map_err(|e| errstr(&e))?;
        }
        _ => return Err("E_NOTIMPL".into()),
    }
    Ok(())
}

fn write_tar(out: &mut dyn Write, items: &[NewItem]) -> Result<(), String> {
    let mut b = tar::Builder::new(out);
    b.mode(tar::HeaderMode::Complete);
    for it in items {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(it.mtime.0.max(0) as u64);
        h.set_mode(it.mode & 0o7777);
        h.set_uid(0);
        h.set_gid(0);
        if it.is_dir {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            b.append_data(&mut h, format!("{}/", it.name), io::empty()).map_err(|e| errstr(&e))?;
        } else if let Some(t) = &it.link {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, &it.name, t).map_err(|e| errstr(&e))?;
        } else {
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(it.size);
            let r = open_source(&it.source).map_err(|e| format!("{}: {}", it.name, errstr(&e)))?;
            b.append_data(&mut h, &it.name, r.take(it.size)).map_err(|e| errstr(&e))?;
        }
    }
    b.finish().map_err(|e| errstr(&e))?;
    Ok(())
}

/// Removes leftovers: the work directory of extracted old items.
pub fn remove_tree(p: &Path) {
    let _ = std::fs::remove_dir_all(p);
}
