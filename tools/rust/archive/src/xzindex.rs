//! What `xz --list` shows, read from a .xz file's ends: each Stream's footer and Index, from the
//! last Stream backwards (Stream Padding between them), without decompressing anything.

use std::io::{self, Read, Seek, SeekFrom};

pub const MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
const FOOTER_MAGIC: [u8; 2] = [b'Y', b'Z'];

#[derive(Debug)]
pub enum ListError {
    Io(io::Error),
    /// Not a .xz file at all ("File format not recognized").
    Format,
    /// A .xz file whose structure is broken ("Compressed data is corrupt").
    Corrupt,
    /// Shorter than a Stream Header and Footer ("Too small to be a valid .xz file").
    TooSmall,
}

impl From<io::Error> for ListError {
    fn from(e: io::Error) -> Self {
        ListError::Io(e)
    }
}

pub struct Block {
    pub number_in_stream: u64,
    pub number_in_file: u64,
    pub compressed_file_offset: u64,
    pub uncompressed_file_offset: u64,
    /// Block Header + Compressed Data + Block Padding + Check.
    pub total_size: u64,
    pub uncompressed_size: u64,
}

pub struct Stream {
    pub number: u64,
    pub check: u8,
    pub compressed_offset: u64,
    pub uncompressed_offset: u64,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    /// Stream Padding after this Stream.
    pub padding: u64,
    pub blocks: Vec<Block>,
}

pub struct FileInfo {
    pub streams: Vec<Stream>,
    pub file_size: u64,
}

impl FileInfo {
    pub fn block_count(&self) -> u64 {
        self.streams.iter().map(|s| s.blocks.len() as u64).sum()
    }
    pub fn uncompressed_size(&self) -> u64 {
        self.streams.iter().map(|s| s.uncompressed_size).sum()
    }
    pub fn padding(&self) -> u64 {
        self.streams.iter().map(|s| s.padding).sum()
    }
    /// A bit per Check ID used, as lzma_index_checks gives.
    pub fn checks(&self) -> u32 {
        self.streams.iter().fold(0, |m, s| m | 1 << s.check)
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn vli(buf: &[u8], pos: &mut usize) -> Result<u64, ListError> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *buf.get(*pos).ok_or(ListError::Corrupt)?;
        *pos += 1;
        v |= ((b & 0x7F) as u64) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err(ListError::Corrupt);
            }
            return Ok(v);
        }
    }
    Err(ListError::Corrupt)
}

pub fn check_size(check: u8) -> u64 {
    match check {
        0 => 0,
        1..=3 => 4,
        4..=6 => 8,
        7..=9 => 16,
        10..=12 => 32,
        _ => 64,
    }
}

fn read_at<F: Read + Seek>(f: &mut F, off: u64, buf: &mut [u8]) -> io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
}

pub fn parse<F: Read + Seek>(f: &mut F) -> Result<FileInfo, ListError> {
    let file_size = f.seek(SeekFrom::End(0))?;
    // As xz: anything shorter than a Stream Header and Footer is reported as too small.
    if file_size < 24 {
        return Err(ListError::TooSmall);
    }
    let mut head = [0u8; 6];
    read_at(f, 0, &mut head)?;
    if head != MAGIC {
        return Err(ListError::Format);
    }
    if file_size % 4 != 0 || file_size < 32 {
        return Err(ListError::Corrupt);
    }
    let mut streams_rev: Vec<Stream> = vec![];
    let mut pos = file_size; // the end of what is left to parse
    while pos > 0 {
        // Stream Padding: zero bytes, a multiple of four, before the footer.
        let mut padding = 0u64;
        loop {
            if pos < 12 {
                return Err(ListError::Corrupt);
            }
            let mut w = [0u8; 4];
            read_at(f, pos - 4, &mut w)?;
            if w != [0; 4] {
                break;
            }
            pos -= 4;
            padding += 4;
        }
        if pos < 32 {
            return Err(ListError::Corrupt);
        }
        let mut footer = [0u8; 12];
        read_at(f, pos - 12, &mut footer)?;
        if footer[10..] != FOOTER_MAGIC || crc32(&footer[4..10]) != u32::from_le_bytes(footer[..4].try_into().unwrap()) {
            return Err(ListError::Corrupt);
        }
        if footer[8] != 0 || footer[9] & 0xF0 != 0 {
            return Err(ListError::Corrupt);
        }
        let check = footer[9] & 0x0F;
        let index_size = (u32::from_le_bytes(footer[4..8].try_into().unwrap()) as u64 + 1) * 4;
        if index_size + 12 + 12 > pos {
            return Err(ListError::Corrupt);
        }
        let index_start = pos - 12 - index_size;
        let mut index = vec![0u8; index_size as usize];
        read_at(f, index_start, &mut index)?;
        let body = index.len() - 4;
        if index[0] != 0 || crc32(&index[..body]) != u32::from_le_bytes(index[body..].try_into().unwrap()) {
            return Err(ListError::Corrupt);
        }
        let mut p = 1usize;
        let count = vli(&index[..body], &mut p)?;
        let mut blocks = vec![];
        let (mut blocks_size, mut uncompressed) = (0u64, 0u64);
        for i in 0..count {
            let unpadded = vli(&index[..body], &mut p)?;
            let usize_ = vli(&index[..body], &mut p)?;
            if unpadded < 5 + check_size(check) {
                return Err(ListError::Corrupt);
            }
            let total = (unpadded + 3) & !3;
            blocks.push(Block {
                number_in_stream: i + 1,
                number_in_file: 0,
                compressed_file_offset: blocks_size, // within the stream for now
                uncompressed_file_offset: uncompressed,
                total_size: total,
                uncompressed_size: usize_,
            });
            blocks_size += total;
            uncompressed += usize_;
        }
        // Index Padding, then the CRC32 already checked.
        while p < body {
            if index[p] != 0 {
                return Err(ListError::Corrupt);
            }
            p += 1;
        }
        if blocks_size + 12 > index_start {
            return Err(ListError::Corrupt);
        }
        let stream_start = index_start - blocks_size - 12;
        let mut header = [0u8; 12];
        read_at(f, stream_start, &mut header)?;
        if header[..6] != MAGIC || header[6..8] != footer[8..10] || crc32(&header[6..8]) != u32::from_le_bytes(header[8..12].try_into().unwrap()) {
            return Err(ListError::Corrupt);
        }
        streams_rev.push(Stream {
            number: 0,
            check,
            compressed_offset: stream_start,
            uncompressed_offset: 0,
            compressed_size: pos - stream_start,
            uncompressed_size: uncompressed,
            padding,
            blocks,
        });
        pos = stream_start;
    }
    streams_rev.reverse();
    let (mut unc, mut nblock) = (0u64, 0u64);
    for (i, s) in streams_rev.iter_mut().enumerate() {
        s.number = i as u64 + 1;
        s.uncompressed_offset = unc;
        for b in s.blocks.iter_mut() {
            nblock += 1;
            b.number_in_file = nblock;
            b.compressed_file_offset += s.compressed_offset + 12;
            b.uncompressed_file_offset += unc;
        }
        unc += s.uncompressed_size;
    }
    Ok(FileInfo { streams: streams_rev, file_size })
}
