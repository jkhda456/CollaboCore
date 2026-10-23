//! The LZ4 frame format: the encoder as lz4's command line drives it (LZ4F_compressBegin,
//! LZ4F_compressUpdate with autoFlush, LZ4F_compressEnd, LZ4F_compressFrame) and the streaming
//! decoder (LZ4F_decompress), ported from lz4 1.10.0's lib/lz4frame.c. The decoder is the
//! original's state machine, hints included: how much the command line reads next, and the
//! number its "Unfinished stream" message shows, come from them.
//!
//! lz4's library is Copyright (c) 2011-2020 Yann Collet, BSD-2-Clause (archive/LICENSE-lz4-lib).

use super::block::{self, FastStream};
use super::hc::{self, HcCtx};
use std::hash::Hasher as _;
use std::rc::Rc;
use twox_hash::xxhash32::Hasher as Xxh32;

pub const MAGIC: u32 = 0x184D_2204;
pub const SKIPPABLE0: u32 = 0x184D_2A50;
pub const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;
pub const LEGACY_MAGIC: u32 = 0x184C_2102;
pub const HEADER_SIZE_MIN: usize = 7;
pub const HEADER_SIZE_MAX: usize = 19;
const BH_SIZE: usize = 4;
const BF_SIZE: usize = 4;
const UNCOMPRESSED_FLAG: u32 = 0x8000_0000;

pub fn xxh32(data: &[u8]) -> u32 {
    Xxh32::oneshot(0, data)
}

/// LZ4F_getBlockSize (of an id 4..7)
pub fn block_size(id: u32) -> usize {
    [64 << 10, 256 << 10, 1 << 20, 4 << 20][(id - 4) as usize]
}

fn header_checksum(header: &[u8]) -> u8 {
    (xxh32(header) >> 8) as u8
}

/// LZ4F_errorCodes, by the names the command line prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    MaxBlockSizeInvalid,
    HeaderVersionWrong,
    BlockChecksumInvalid,
    ReservedFlagSet,
    FrameHeaderIncomplete,
    FrameTypeUnknown,
    FrameSizeWrong,
    DecompressionFailed,
    HeaderChecksumInvalid,
    ContentChecksumInvalid,
}

impl FrameError {
    pub fn name(self) -> &'static str {
        match self {
            FrameError::MaxBlockSizeInvalid => "ERROR_maxBlockSize_invalid",
            FrameError::HeaderVersionWrong => "ERROR_headerVersion_wrong",
            FrameError::BlockChecksumInvalid => "ERROR_blockChecksum_invalid",
            FrameError::ReservedFlagSet => "ERROR_reservedFlag_set",
            FrameError::FrameHeaderIncomplete => "ERROR_frameHeader_incomplete",
            FrameError::FrameTypeUnknown => "ERROR_frameType_unknown",
            FrameError::FrameSizeWrong => "ERROR_frameSize_wrong",
            FrameError::DecompressionFailed => "ERROR_decompressionFailed",
            FrameError::HeaderChecksumInvalid => "ERROR_headerChecksum_invalid",
            FrameError::ContentChecksumInvalid => "ERROR_contentChecksum_invalid",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compression

#[derive(Clone, Copy)]
pub struct Prefs {
    pub level: i32,
    pub block_size_id: u32,
    pub linked: bool,
    pub block_checksum: bool,
    pub content_checksum: bool,
    /// 0: not in the header
    pub content_size: u64,
    pub favor_dec_speed: bool,
}

/// LZ4F_CDict: a dictionary digested once for both compressors.
pub struct CDict {
    fast: Rc<FastStream>,
    hc: Rc<HcCtx>,
}

impl CDict {
    pub fn new(dict: &[u8]) -> CDict {
        let d = &dict[dict.len().saturating_sub(65536)..];
        let mut fast = FastStream::default();
        fast.load_dict_slow(d);
        let mut hcc = HcCtx::default();
        hcc.set_compression_level(hc::CLEVEL_DEFAULT);
        hcc.load_dict(d);
        CDict { fast: Rc::new(fast), hc: Rc::new(hcc) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CtxType {
    None,
    Fast,
    Hc,
}

/// LZ4F_cctx: one for a whole run, as the command line keeps it (the compressors' state
/// carries over from file to file).
pub struct CCtx {
    alloc: CtxType,
    ctype: CtxType,
    fast: FastStream,
    hc: HcCtx,
    prefs: Prefs,
    max_block_size: usize,
    cdict: Option<Rc<CDict>>,
    xxh: Xxh32,
    total_in: u64,
}

impl Default for CCtx {
    fn default() -> Self {
        CCtx {
            alloc: CtxType::None,
            ctype: CtxType::None,
            fast: FastStream::default(),
            hc: HcCtx::default(),
            prefs: Prefs {
                level: 0,
                block_size_id: 4,
                linked: false,
                block_checksum: false,
                content_checksum: false,
                content_size: 0,
                favor_dec_speed: false,
            },
            max_block_size: 64 << 10,
            cdict: None,
            xxh: Xxh32::with_seed(0),
            total_in: 0,
        }
    }
}

impl CCtx {
    /// LZ4F_compressBegin_usingCDict: the frame header.
    pub fn begin(&mut self, prefs: &Prefs, cdict: Option<&Rc<CDict>>, out: &mut Vec<u8>) {
        self.prefs = *prefs;
        let required = if prefs.level < hc::CLEVEL_MIN { CtxType::Fast } else { CtxType::Hc };
        if self.alloc < required {
            if required == CtxType::Fast {
                self.fast = FastStream::default();
            } else {
                self.hc = HcCtx::default();
            }
            self.alloc = required;
            self.ctype = required;
        } else if self.ctype != required {
            if required == CtxType::Fast {
                self.fast.init();
            } else {
                self.hc.init();
                self.hc.set_compression_level(prefs.level);
            }
            self.ctype = required;
        }
        self.max_block_size = block_size(self.prefs.block_size_id);
        self.xxh = Xxh32::with_seed(0);
        self.cdict = cdict.cloned();
        if self.prefs.linked {
            self.init_stream();
        }
        if prefs.level >= hc::CLEVEL_MIN {
            self.hc.set_favor_dec_speed(prefs.favor_dec_speed);
        }
        out.extend_from_slice(&MAGIC.to_le_bytes());
        let h = out.len();
        out.push(
            (1 << 6)
                | ((!self.prefs.linked as u8) << 5)
                | ((self.prefs.block_checksum as u8) << 4)
                | (((self.prefs.content_size > 0) as u8) << 3)
                | ((self.prefs.content_checksum as u8) << 2),
        );
        out.push(((self.prefs.block_size_id & 7) << 4) as u8);
        if self.prefs.content_size != 0 {
            out.extend_from_slice(&self.prefs.content_size.to_le_bytes());
            self.total_in = 0;
        }
        let hc = header_checksum(&out[h..]);
        out.push(hc);
    }

    /// LZ4F_initStream
    fn init_stream(&mut self) {
        let level = self.prefs.level;
        if level < hc::CLEVEL_MIN {
            if self.cdict.is_some() || self.prefs.linked {
                self.fast.reset_fast();
                if let Some(c) = &self.cdict {
                    self.fast.attach_dictionary(Some(&c.fast));
                }
            }
        } else {
            self.hc.reset_fast(level);
            if let Some(c) = &self.cdict {
                self.hc.attach_dictionary(Some(&c.hc));
            }
        }
    }

    /// The block compressor LZ4F_selectCompression picks; returns its size, 0 when it did not
    /// fit in `cap`.
    fn compress_block(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize) -> usize {
        let level = self.prefs.level;
        if level < hc::CLEVEL_MIN {
            let accel = if level < 0 { -level + 1 } else { 1 };
            if !self.prefs.linked {
                if self.cdict.is_some() {
                    self.init_stream_independent();
                    self.fast.compress_continue(src, out, cap, accel)
                } else {
                    self.fast.compress_fast_reset(src, out, cap, accel)
                }
            } else {
                self.fast.compress_continue(src, out, cap, accel)
            }
        } else if !self.prefs.linked {
            self.init_stream_independent();
            if self.cdict.is_some() {
                self.hc.compress_continue(src, out, cap)
            } else {
                self.hc.compress_ext_state_fast_reset(src, out, cap, level)
            }
        } else {
            self.hc.compress_continue(src, out, cap)
        }
    }

    /// LZ4F_initStream for an independent block
    fn init_stream_independent(&mut self) {
        let level = self.prefs.level;
        if level < hc::CLEVEL_MIN {
            if self.cdict.is_some() {
                self.fast.reset_fast();
                let c = self.cdict.clone().unwrap();
                self.fast.attach_dictionary(Some(&c.fast));
            }
        } else {
            self.hc.reset_fast(level);
            if let Some(c) = self.cdict.clone() {
                self.hc.attach_dictionary(Some(&c.hc));
            }
        }
    }

    /// LZ4F_makeBlock: compressed, or stored when that is not smaller.
    fn make_block(&mut self, src: &[u8], out: &mut Vec<u8>) {
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        let c_size = self.compress_block(src, out, src.len() - 1);
        let c_size = if c_size == 0 || c_size >= src.len() {
            out.truncate(at + 4);
            out.extend_from_slice(src);
            out[at..at + 4].copy_from_slice(&(src.len() as u32 | UNCOMPRESSED_FLAG).to_le_bytes());
            src.len()
        } else {
            out[at..at + 4].copy_from_slice(&(c_size as u32).to_le_bytes());
            c_size
        };
        if self.prefs.block_checksum {
            let crc = xxh32(&out[at + 4..at + 4 + c_size]);
            out.extend_from_slice(&crc.to_le_bytes());
        }
    }

    /// LZ4F_compressUpdate with autoFlush: every byte given is compressed into blocks.
    /// `stable`: the source stays where it is (the single-pass frame), so linked blocks need
    /// no copy of their history.
    pub fn update(&mut self, src: &[u8], out: &mut Vec<u8>, stable: bool) {
        let bs = self.max_block_size;
        let mut p = 0;
        let mut last_from_src = false;
        while src.len() - p >= bs {
            self.make_block(&src[p..p + bs], out);
            p += bs;
            last_from_src = true;
        }
        if p < src.len() {
            self.make_block(&src[p..], out);
            last_from_src = true;
        }
        if self.prefs.linked && last_from_src && !stable {
            if self.prefs.level < hc::CLEVEL_MIN {
                self.fast.save_dict(65536);
            } else {
                self.hc.save_dict(65536);
            }
        }
        if self.prefs.content_checksum {
            self.xxh.write(src);
        }
        self.total_in += src.len() as u64;
    }

    /// LZ4F_compressEnd: the end mark and the content checksum.
    pub fn end(&mut self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        out.extend_from_slice(&[0; 4]);
        if self.prefs.content_checksum {
            out.extend_from_slice(&self.xxh.finish_32().to_le_bytes());
        }
        if self.prefs.content_size != 0 && self.prefs.content_size != self.total_in {
            return Err(FrameError::FrameSizeWrong);
        }
        Ok(())
    }

    /// LZ4F_compressFrame_usingCDict: a whole frame in one pass, its block size the smallest
    /// that holds the source.
    pub fn compress_frame(&mut self, src: &[u8], prefs: &Prefs, cdict: Option<&Rc<CDict>>, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let mut p = *prefs;
        if p.content_size != 0 {
            p.content_size = src.len() as u64;
        }
        p.block_size_id = optimal_bsid(p.block_size_id, src.len());
        if src.len() <= block_size(p.block_size_id) {
            p.linked = false;
        }
        self.begin(&p, cdict, out);
        self.update(src, out, true);
        self.end(out)
    }
}

/// LZ4F_optimalBSID
fn optimal_bsid(requested: u32, size: usize) -> u32 {
    let mut proposed = 4;
    let mut max = 64 << 10;
    while requested > proposed {
        if size <= max {
            return proposed;
        }
        proposed += 1;
        max <<= 2;
    }
    requested
}

// ---------------------------------------------------------------------------------------------
// Decompression

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Stage {
    GetFrameHeader,
    StoreFrameHeader,
    Init,
    GetBlockHeader,
    StoreBlockHeader,
    CopyDirect,
    GetBlockChecksum,
    GetCBlock,
    StoreCBlock,
    FlushOut,
    GetSuffix,
    StoreSuffix,
    GetSFrameSize,
    StoreSFrameSize,
    SkipSkippable,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct FrameInfo {
    pub linked: bool,
    pub block_checksum: bool,
    pub content_checksum: bool,
    pub block_size_id: u32,
    pub content_size: u64,
    pub skippable: bool,
}

/// LZ4F_dctx
pub struct DCtx {
    stage: Stage,
    pub info: FrameInfo,
    frame_remaining: u64,
    max_block_size: usize,
    header: [u8; HEADER_SIZE_MAX],
    tmp_in: Vec<u8>,
    tmp_in_size: usize,
    tmp_in_target: usize,
    tmp_out: Vec<u8>,
    tmp_out_start: usize,
    /// the dictionary, then what linked blocks decoded (its last 64 KB)
    hist: Vec<u8>,
    dict: Vec<u8>,
    xxh: Xxh32,
    block_xxh: Xxh32,
    skip_checksum: bool,
}

impl DCtx {
    pub fn new(dict: Vec<u8>) -> DCtx {
        DCtx {
            stage: Stage::GetFrameHeader,
            info: FrameInfo::default(),
            frame_remaining: 0,
            max_block_size: 0,
            header: [0; HEADER_SIZE_MAX],
            tmp_in: vec![],
            tmp_in_size: 0,
            tmp_in_target: 0,
            tmp_out: vec![],
            tmp_out_start: 0,
            hist: vec![],
            dict,
            xxh: Xxh32::with_seed(0),
            block_xxh: Xxh32::with_seed(0),
            skip_checksum: false,
        }
    }

    /// LZ4F_resetDecompressionContext
    fn reset(&mut self) {
        self.stage = Stage::GetFrameHeader;
        self.skip_checksum = false;
        self.frame_remaining = 0;
    }

    /// LZ4F_decodeHeader: bytes read, or the header is stored for more.
    fn decode_header(&mut self, src: &[u8], from_header: bool) -> Result<usize, FrameError> {
        if src.len() < HEADER_SIZE_MIN {
            return Err(FrameError::FrameHeaderIncomplete);
        }
        self.info = FrameInfo::default();
        let magic = u32::from_le_bytes(src[..4].try_into().unwrap());
        if magic & SKIPPABLE_MASK == SKIPPABLE0 {
            self.info.skippable = true;
            if from_header {
                self.tmp_in_size = src.len();
                self.tmp_in_target = 8;
                self.stage = Stage::StoreSFrameSize;
                return Ok(src.len());
            }
            self.stage = Stage::GetSFrameSize;
            return Ok(4);
        }
        if magic != MAGIC {
            return Err(FrameError::FrameTypeUnknown);
        }
        let flg = src[4];
        let version = (flg >> 6) & 3;
        let block_checksum = (flg >> 4) & 1 != 0;
        let linked = (flg >> 5) & 1 == 0;
        let content_size_flag = (flg >> 3) & 1 != 0;
        let content_checksum = (flg >> 2) & 1 != 0;
        let dict_id_flag = flg & 1 != 0;
        if (flg >> 1) & 1 != 0 {
            return Err(FrameError::ReservedFlagSet);
        }
        if version != 1 {
            return Err(FrameError::HeaderVersionWrong);
        }
        let fhs = HEADER_SIZE_MIN + if content_size_flag { 8 } else { 0 } + if dict_id_flag { 4 } else { 0 };
        if src.len() < fhs {
            if !from_header {
                self.header[..src.len()].copy_from_slice(src);
            }
            self.tmp_in_size = src.len();
            self.tmp_in_target = fhs;
            self.stage = Stage::StoreFrameHeader;
            return Ok(src.len());
        }
        let bd = src[5];
        let bsid = ((bd >> 4) & 7) as u32;
        if (bd >> 7) & 1 != 0 {
            return Err(FrameError::ReservedFlagSet);
        }
        if bsid < 4 {
            return Err(FrameError::MaxBlockSizeInvalid);
        }
        if bd & 15 != 0 {
            return Err(FrameError::ReservedFlagSet);
        }
        if header_checksum(&src[4..fhs - 1]) != src[fhs - 1] {
            return Err(FrameError::HeaderChecksumInvalid);
        }
        self.info.linked = linked;
        self.info.block_checksum = block_checksum;
        self.info.content_checksum = content_checksum;
        self.info.block_size_id = bsid;
        self.max_block_size = block_size(bsid);
        if content_size_flag {
            self.info.content_size = u64::from_le_bytes(src[6..14].try_into().unwrap());
            self.frame_remaining = self.info.content_size;
        }
        self.stage = Stage::Init;
        Ok(fhs)
    }

    fn update_dict(&mut self, data: &[u8]) {
        self.hist.extend_from_slice(data);
        if self.hist.len() > 2 * 65536 {
            let cut = self.hist.len() - 65536;
            self.hist.drain(..cut);
        }
    }

    /// LZ4F_decompress_usingDict: decodes what it can of `src` into `dst`, which takes at most
    /// `dst_cap` more bytes. Returns (bytes of src consumed, hint: the size of the next read;
    /// 0 at the end of a frame).
    pub fn decompress(&mut self, dst: &mut Vec<u8>, dst_cap: usize, src: &[u8], skip_checksums: bool) -> Result<(usize, usize), FrameError> {
        if self.stage <= Stage::Init {
            self.hist = self.dict.clone();
        }
        let dst_start = dst.len();
        let room = |dst: &Vec<u8>| dst_cap - (dst.len() - dst_start);
        let src_end = src.len();
        let mut sp = 0usize;
        let mut hint = 1usize;
        self.skip_checksum |= skip_checksums;

        'machine: loop {
            match self.stage {
                Stage::GetFrameHeader => {
                    if src_end - sp >= HEADER_SIZE_MAX {
                        let h = self.decode_header(&src[sp..], false)?;
                        sp += h;
                        continue;
                    }
                    self.tmp_in_size = 0;
                    if src_end - sp == 0 {
                        return Ok((sp, HEADER_SIZE_MIN));
                    }
                    self.tmp_in_target = HEADER_SIZE_MIN;
                    self.stage = Stage::StoreFrameHeader;
                }
                Stage::StoreFrameHeader => {
                    let n = (self.tmp_in_target - self.tmp_in_size).min(src_end - sp);
                    self.header[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                    self.tmp_in_size += n;
                    sp += n;
                    if self.tmp_in_size < self.tmp_in_target {
                        hint = self.tmp_in_target - self.tmp_in_size + BH_SIZE;
                        break 'machine;
                    }
                    let header = self.header;
                    self.decode_header(&header[..self.tmp_in_target], true)?;
                }
                Stage::Init => {
                    if self.info.content_checksum {
                        self.xxh = Xxh32::with_seed(0);
                    }
                    self.tmp_in = vec![0; self.max_block_size + BF_SIZE];
                    self.tmp_in_size = 0;
                    self.tmp_in_target = 0;
                    self.tmp_out.clear();
                    self.tmp_out_start = 0;
                    self.stage = Stage::GetBlockHeader;
                }
                Stage::GetBlockHeader | Stage::StoreBlockHeader => {
                    let bh: u32;
                    if self.stage == Stage::GetBlockHeader && src_end - sp >= BH_SIZE {
                        bh = u32::from_le_bytes(src[sp..sp + 4].try_into().unwrap());
                        sp += BH_SIZE;
                    } else {
                        if self.stage == Stage::GetBlockHeader {
                            self.tmp_in_size = 0;
                            self.stage = Stage::StoreBlockHeader;
                        }
                        let n = (BH_SIZE - self.tmp_in_size).min(src_end - sp);
                        self.tmp_in[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                        sp += n;
                        self.tmp_in_size += n;
                        if self.tmp_in_size < BH_SIZE {
                            hint = BH_SIZE - self.tmp_in_size;
                            break 'machine;
                        }
                        bh = u32::from_le_bytes(self.tmp_in[..4].try_into().unwrap());
                    }
                    let next = (bh & 0x7FFF_FFFF) as usize;
                    let crc_size = if self.info.block_checksum { BF_SIZE } else { 0 };
                    if bh == 0 {
                        self.stage = Stage::GetSuffix;
                        continue;
                    }
                    if next > self.max_block_size {
                        return Err(FrameError::MaxBlockSizeInvalid);
                    }
                    if bh & UNCOMPRESSED_FLAG != 0 {
                        self.tmp_in_target = next;
                        if self.info.block_checksum {
                            self.block_xxh = Xxh32::with_seed(0);
                        }
                        self.stage = Stage::CopyDirect;
                        continue;
                    }
                    self.tmp_in_target = next + crc_size;
                    self.stage = Stage::GetCBlock;
                    if room(dst) == 0 || sp == src_end {
                        hint = BH_SIZE + next + crc_size;
                        break 'machine;
                    }
                }
                Stage::CopyDirect => {
                    let n = self.tmp_in_target.min(src_end - sp).min(room(dst));
                    let data = &src[sp..sp + n];
                    dst.extend_from_slice(data);
                    if !self.skip_checksum {
                        if self.info.block_checksum {
                            self.block_xxh.write(data);
                        }
                        if self.info.content_checksum {
                            self.xxh.write(data);
                        }
                    }
                    if self.info.content_size != 0 {
                        self.frame_remaining = self.frame_remaining.wrapping_sub(n as u64);
                    }
                    if self.info.linked {
                        self.update_dict(data);
                    }
                    sp += n;
                    if n == self.tmp_in_target {
                        if self.info.block_checksum {
                            self.tmp_in_size = 0;
                            self.stage = Stage::GetBlockChecksum;
                        } else {
                            self.stage = Stage::GetBlockHeader;
                        }
                        continue;
                    }
                    self.tmp_in_target -= n;
                    hint = self.tmp_in_target + if self.info.block_checksum { BF_SIZE } else { 0 } + BH_SIZE;
                    break 'machine;
                }
                Stage::GetBlockChecksum => {
                    let read_crc: u32;
                    if src_end - sp >= 4 && self.tmp_in_size == 0 {
                        read_crc = u32::from_le_bytes(src[sp..sp + 4].try_into().unwrap());
                        sp += 4;
                    } else {
                        let n = (4 - self.tmp_in_size).min(src_end - sp);
                        self.header[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                        self.tmp_in_size += n;
                        sp += n;
                        if self.tmp_in_size < 4 {
                            break 'machine;
                        }
                        read_crc = u32::from_le_bytes(self.header[..4].try_into().unwrap());
                    }
                    if !self.skip_checksum && read_crc != self.block_xxh.finish_32() {
                        return Err(FrameError::BlockChecksumInvalid);
                    }
                    self.stage = Stage::GetBlockHeader;
                }
                Stage::GetCBlock | Stage::StoreCBlock => {
                    let from_src;
                    if self.stage == Stage::GetCBlock {
                        if src_end - sp < self.tmp_in_target {
                            self.tmp_in_size = 0;
                            self.stage = Stage::StoreCBlock;
                            continue;
                        }
                        from_src = true;
                    } else {
                        let n = (self.tmp_in_target - self.tmp_in_size).min(src_end - sp);
                        self.tmp_in[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                        self.tmp_in_size += n;
                        sp += n;
                        if self.tmp_in_size < self.tmp_in_target {
                            hint = self.tmp_in_target - self.tmp_in_size + if self.info.block_checksum { BF_SIZE } else { 0 } + BH_SIZE;
                            break 'machine;
                        }
                        from_src = false;
                    }
                    let target = self.tmp_in_target;
                    let selected: Vec<u8>;
                    let sel: &[u8] = if from_src {
                        let s = &src[sp..sp + target];
                        sp += target;
                        s
                    } else {
                        selected = self.tmp_in[..target].to_vec();
                        &selected
                    };
                    let mut data_len = target;
                    if self.info.block_checksum {
                        data_len -= 4;
                        self.tmp_in_target = data_len;
                        let read = u32::from_le_bytes(sel[data_len..data_len + 4].try_into().unwrap());
                        if read != xxh32(&sel[..data_len]) {
                            return Err(FrameError::BlockChecksumInvalid);
                        }
                    }
                    self.tmp_out.clear();
                    if block::decompress(&sel[..data_len], &self.hist, self.max_block_size, &mut self.tmp_out).is_err() {
                        return Err(FrameError::DecompressionFailed);
                    }
                    if self.info.content_checksum && !self.skip_checksum {
                        self.xxh.write(&self.tmp_out);
                    }
                    if self.info.content_size != 0 {
                        self.frame_remaining = self.frame_remaining.wrapping_sub(self.tmp_out.len() as u64);
                    }
                    self.tmp_out_start = 0;
                    self.stage = Stage::FlushOut;
                }
                Stage::FlushOut => {
                    let n = (self.tmp_out.len() - self.tmp_out_start).min(room(dst));
                    let s = self.tmp_out_start;
                    dst.extend_from_slice(&self.tmp_out[s..s + n]);
                    if self.info.linked {
                        let piece = self.tmp_out[s..s + n].to_vec();
                        self.update_dict(&piece);
                    }
                    self.tmp_out_start += n;
                    if self.tmp_out_start == self.tmp_out.len() {
                        self.stage = Stage::GetBlockHeader;
                        continue;
                    }
                    hint = BH_SIZE;
                    break 'machine;
                }
                Stage::GetSuffix | Stage::StoreSuffix => {
                    let read_crc: u32;
                    if self.stage == Stage::GetSuffix {
                        if self.frame_remaining != 0 {
                            return Err(FrameError::FrameSizeWrong);
                        }
                        if !self.info.content_checksum {
                            self.reset();
                            hint = 0;
                            break 'machine;
                        }
                        if src_end - sp < 4 {
                            self.tmp_in_size = 0;
                            self.stage = Stage::StoreSuffix;
                            continue;
                        }
                        read_crc = u32::from_le_bytes(src[sp..sp + 4].try_into().unwrap());
                        sp += 4;
                    } else {
                        let n = (4 - self.tmp_in_size).min(src_end - sp);
                        self.tmp_in[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                        sp += n;
                        self.tmp_in_size += n;
                        if self.tmp_in_size < 4 {
                            hint = 4 - self.tmp_in_size;
                            break 'machine;
                        }
                        read_crc = u32::from_le_bytes(self.tmp_in[..4].try_into().unwrap());
                    }
                    if !self.skip_checksum && read_crc != self.xxh.finish_32() {
                        return Err(FrameError::ContentChecksumInvalid);
                    }
                    self.reset();
                    hint = 0;
                    break 'machine;
                }
                Stage::GetSFrameSize | Stage::StoreSFrameSize => {
                    if self.stage == Stage::GetSFrameSize {
                        if src_end - sp >= 4 {
                            self.header[4..8].copy_from_slice(&src[sp..sp + 4]);
                            sp += 4;
                            self.tmp_in_size = 8;
                            self.tmp_in_target = 8;
                        } else {
                            self.tmp_in_size = 4;
                            self.tmp_in_target = 8;
                            self.stage = Stage::StoreSFrameSize;
                        }
                    }
                    if self.stage == Stage::StoreSFrameSize {
                        let n = (self.tmp_in_target - self.tmp_in_size).min(src_end - sp);
                        self.header[self.tmp_in_size..self.tmp_in_size + n].copy_from_slice(&src[sp..sp + n]);
                        sp += n;
                        self.tmp_in_size += n;
                        if self.tmp_in_size < self.tmp_in_target {
                            hint = self.tmp_in_target - self.tmp_in_size;
                            break 'machine;
                        }
                    }
                    let size = u32::from_le_bytes(self.header[4..8].try_into().unwrap()) as usize;
                    self.info.content_size = size as u64;
                    self.tmp_in_target = size;
                    self.stage = Stage::SkipSkippable;
                }
                Stage::SkipSkippable => {
                    let n = self.tmp_in_target.min(src_end - sp);
                    sp += n;
                    self.tmp_in_target -= n;
                    hint = self.tmp_in_target;
                    if hint == 0 {
                        self.reset();
                    }
                    break 'machine;
                }
            }
        }
        Ok((sp, hint))
    }
}

/// LZ4F_headerSize
pub fn header_size(src: &[u8]) -> Result<usize, FrameError> {
    if src.len() < 5 {
        return Err(FrameError::FrameHeaderIncomplete);
    }
    let magic = u32::from_le_bytes(src[..4].try_into().unwrap());
    if magic & SKIPPABLE_MASK == SKIPPABLE0 {
        return Ok(8);
    }
    if magic != MAGIC {
        return Err(FrameError::FrameTypeUnknown);
    }
    let flg = src[4];
    Ok(HEADER_SIZE_MIN + if (flg >> 3) & 1 != 0 { 8 } else { 0 } + if flg & 1 != 0 { 4 } else { 0 })
}

/// LZ4F_getFrameInfo on a whole header (for --list).
pub fn frame_info(header: &[u8]) -> Result<FrameInfo, FrameError> {
    let mut d = DCtx::new(vec![]);
    let h = header_size(header)?;
    if header.len() < h {
        return Err(FrameError::FrameHeaderIncomplete);
    }
    d.decode_header(&header[..h], false)?;
    Ok(d.info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(src: &[u8], prefs: &Prefs, block: usize) -> Vec<u8> {
        let mut c = CCtx::default();
        let mut out = vec![];
        c.begin(prefs, None, &mut out);
        for chunk in src.chunks(block) {
            c.update(chunk, &mut out, false);
        }
        c.end(&mut out).unwrap();
        out
    }

    fn unframe(data: &[u8], step: usize) -> Vec<u8> {
        let mut d = DCtx::new(vec![]);
        let mut out = vec![];
        let mut p = 0;
        loop {
            let n = step.min(data.len() - p);
            let (used, hint) = d.decompress(&mut out, 1 << 30, &data[p..p + n], false).unwrap();
            p += used;
            if hint == 0 || p == data.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn frames_roundtrip() {
        let src: Vec<u8> = (0..400_000u32).map(|i| ((i * 7) % 251) as u8 ^ (i >> 9) as u8).collect();
        for level in [1, 2, 9, 12, -3] {
            for linked in [false, true] {
                let prefs = Prefs {
                    level,
                    block_size_id: 4,
                    linked,
                    block_checksum: true,
                    content_checksum: true,
                    content_size: 0,
                    favor_dec_speed: false,
                };
                let f = frame(&src, &prefs, 65536);
                for step in [1, 7, 4096, 1 << 20] {
                    assert!(unframe(&f, step) == src, "level {level} linked {linked} step {step}");
                }
            }
        }
    }
}
