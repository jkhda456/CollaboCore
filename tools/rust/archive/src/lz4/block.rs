//! LZ4 blocks: the decoder (LZ4_decompress_safe_usingDict) and the fast compressor with its
//! streaming state (LZ4_stream_t), ported from lz4 1.10.0's lib/lz4.c so that frames come out
//! byte for byte as the original's (64-bit build): the same hashes, the same hash table carried
//! from block to block as the original carries it, the same output limits.
//!
//! Positions are indexes into a window: the dictionary (what precedes the block — the history
//! linked blocks keep, or a dictionary given with -D) followed by the block itself. The
//! original's index arithmetic sees them the same way: its dictionary always ends where the
//! block's indexes begin, and a match that runs off the end of the dictionary continues at the
//! start of the block.
//!
//! lz4's library is Copyright (c) 2011-2020 Yann Collet, BSD-2-Clause (archive/LICENSE-lz4-lib).

use std::borrow::Cow;
use std::rc::Rc;

pub const MINMATCH: usize = 4;
pub const LASTLITERALS: usize = 5;
pub const MFLIMIT: usize = 12;
pub const MIN_LENGTH: usize = MFLIMIT + 1;
pub const ML_MASK: usize = 15;
pub const RUN_MASK: usize = 15;
pub const DISTANCE_MAX: usize = 65535;
pub const MAX_INPUT_SIZE: usize = 0x7E00_0000;
const LIMIT_64K: usize = 65536 + MFLIMIT - 1;
const SKIP_TRIGGER: u32 = 6;
const HASHLOG: u32 = 12;
const HASH_SIZE_U32: usize = 1 << HASHLOG;
const KB64: u32 = 65536;
const ACCELERATION_MAX: i32 = 65537;
const FASTLOOP_SAFE_DISTANCE: usize = 64;

pub fn compress_bound(n: usize) -> usize {
    if n > MAX_INPUT_SIZE { 0 } else { n + n / 255 + 16 }
}

#[inline]
pub fn read16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}

#[inline]
pub fn read32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

#[inline]
pub fn read64(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

/// LZ4_count: how many bytes at `a` equal those at `b` (b < a), counting up to `limit` for `a`.
#[inline]
pub fn count(w: &[u8], mut a: usize, mut b: usize, limit: usize) -> usize {
    let start = a;
    while a + 8 <= limit {
        let x = read64(w, a) ^ read64(w, b);
        if x != 0 {
            return a - start + (x.trailing_zeros() / 8) as usize;
        }
        a += 8;
        b += 8;
    }
    while a < limit && w[a] == w[b] {
        a += 1;
        b += 1;
    }
    a - start
}

/// A literal or match length past its 4 bits in the token: 255s, then the rest.
pub fn push_length(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

// ---------------------------------------------------------------------------------------------
// Decoding

/// LZ4_decompress_generic's variable length: `None` when it runs past `ilimit` (or the input).
fn read_varlen(src: &[u8], ip: &mut usize, ilimit: usize, initial_check: bool) -> Option<usize> {
    if initial_check && *ip >= ilimit {
        return None;
    }
    let mut len = 0usize;
    loop {
        let s = *src.get(*ip)? as usize;
        *ip += 1;
        len += s;
        if *ip > ilimit {
            return None;
        }
        if s != 255 {
            return Some(len);
        }
    }
}

/// Copies a match of `len` bytes from `offset` back, byte by byte where it overlaps; an offset
/// of 0 (invalid, but the original copies the output onto itself after zeroing it) gives zeros.
#[inline]
fn copy_match(buf: &mut Vec<u8>, offset: usize, len: usize) {
    if offset == 0 {
        buf.resize(buf.len() + len, 0);
        return;
    }
    let start = buf.len() - offset;
    if offset >= len {
        buf.extend_from_within(start..start + len);
    } else {
        for i in 0..len {
            let b = buf[start + i];
            buf.push(b);
        }
    }
}

/// LZ4_decompress_safe_usingDict: decodes `src` (one block) after the history `hist` (the
/// dictionary, or what linked blocks decoded before) into `out`, which must hold no more than
/// `cap` bytes. The checks are the original's, fast loop and safe loop alike, so the same
/// malformed blocks fail. Err(()): the block is malformed.
pub fn decompress(src: &[u8], hist: &[u8], cap: usize, out: &mut Vec<u8>) -> Result<(), ()> {
    // Only the last 64 KB of history can be referenced.
    let hist = &hist[hist.len().saturating_sub(65536)..];
    let h = hist.len();
    let mut buf = Vec::with_capacity(h + cap);
    buf.extend_from_slice(hist);
    let iend = src.len();
    let oend = cap as isize;
    if cap == 0 {
        return if src == [0] { Ok(()) } else { Err(()) };
    }
    if src.is_empty() {
        return Err(());
    }
    let shortiend = iend as isize - 14 - 2;
    let shortoend = oend - 14 - 18;
    let mut ip = 0usize;
    // o: bytes of this block decoded so far (buf.len() - h)
    macro_rules! o {
        () => {
            (buf.len() - h) as isize
        };
    }
    // `offset` reaches before the history: the original's checkOffset (its prefix is at most
    // the history it was given).
    macro_rules! offset_ok {
        ($offset:expr) => {
            ($offset as isize) <= h as isize + o!()
        };
    }

    // The fast loop, while the output is far from its end; its literal and match exits go to
    // the safe loop's code with the token half decoded.
    enum Resume {
        Token,
        Literals { token: u8, len: usize },
        Match { offset: usize, len: usize },
    }
    let mut resume = Resume::Token;
    if oend - o!() >= FASTLOOP_SAFE_DISTANCE as isize {
        loop {
            debug_assert!(oend - o!() >= FASTLOOP_SAFE_DISTANCE as isize);
            let token = *src.get(ip).ok_or(())?;
            ip += 1;
            let mut len = (token >> 4) as usize;
            if len == RUN_MASK {
                len += read_varlen(src, &mut ip, iend.wrapping_sub(RUN_MASK), true).ok_or(())?;
                if o!() + len as isize > oend - 32 || (ip + len) as isize > iend as isize - 32 {
                    resume = Resume::Literals { token, len };
                    break;
                }
                buf.extend_from_slice(&src[ip..ip + len]);
                ip += len;
            } else if (ip as isize) <= iend as isize - 17 {
                buf.extend_from_slice(&src[ip..ip + len]);
                ip += len;
            } else {
                resume = Resume::Literals { token, len };
                break;
            }
            let offset = read16(src, ip) as usize;
            ip += 2;
            let mut mlen = (token & 15) as usize;
            if mlen == ML_MASK {
                mlen += read_varlen(src, &mut ip, (iend + 1).wrapping_sub(LASTLITERALS), false).ok_or(())?;
                mlen += MINMATCH;
                if o!() + mlen as isize >= oend - FASTLOOP_SAFE_DISTANCE as isize {
                    resume = Resume::Match { offset, len: mlen };
                    break;
                }
            } else {
                mlen += MINMATCH;
                if o!() + mlen as isize >= oend - FASTLOOP_SAFE_DISTANCE as isize {
                    resume = Resume::Match { offset, len: mlen };
                    break;
                }
                if offset >= 8 && offset_ok!(offset) {
                    copy_match(&mut buf, offset, mlen);
                    continue;
                }
            }
            if !offset_ok!(offset) {
                return Err(());
            }
            copy_match(&mut buf, offset, mlen);
        }
    }

    // The safe loop.
    loop {
        let (token, mut len) = match std::mem::replace(&mut resume, Resume::Token) {
            Resume::Match { offset, len } => {
                safe_match(&mut buf, h, offset, len, oend)?;
                continue;
            }
            Resume::Literals { token, len } => (token, len),
            Resume::Token => {
                let token = *src.get(ip).ok_or(())?;
                ip += 1;
                let len = (token >> 4) as usize;
                if len != RUN_MASK && (ip as isize) < shortiend && o!() <= shortoend {
                    // the shortcut: literals of 0..14 and a match of 4..18 copied without the
                    // end checks, there being room for both
                    buf.extend_from_slice(&src[ip..ip + len]);
                    ip += len;
                    let mlen = (token & 15) as usize;
                    let offset = read16(src, ip) as usize;
                    ip += 2;
                    if mlen != ML_MASK && offset >= 8 && offset_ok!(offset) {
                        copy_match(&mut buf, offset, mlen + MINMATCH);
                        continue;
                    }
                    let mlen = match_length(src, &mut ip, iend, mlen)?;
                    safe_match(&mut buf, h, offset, mlen, oend)?;
                    continue;
                }
                let mut len = len;
                if len == RUN_MASK {
                    len += read_varlen(src, &mut ip, iend.wrapping_sub(RUN_MASK), true).ok_or(())?;
                }
                (token, len)
            }
        };
        // literals
        let cpy = o!() + len as isize;
        if cpy > oend - MFLIMIT as isize || (ip + len) as isize > iend as isize - (2 + 1 + LASTLITERALS) as isize {
            // necessarily the last sequence: exactly the rest of the input, within the output
            if ip + len != iend || cpy > oend {
                return Err(());
            }
            buf.extend_from_slice(&src[ip..ip + len]);
            break;
        }
        buf.extend_from_slice(&src[ip..ip + len]);
        ip += len;
        let offset = read16(src, ip) as usize;
        ip += 2;
        len = match_length(src, &mut ip, iend, (token & 15) as usize)?;
        safe_match(&mut buf, h, offset, len, oend)?;
    }
    out.extend_from_slice(&buf[h..]);
    Ok(())
}

fn match_length(src: &[u8], ip: &mut usize, iend: usize, mut mlen: usize) -> Result<usize, ()> {
    if mlen == ML_MASK {
        mlen += read_varlen(src, ip, (iend + 1).wrapping_sub(LASTLITERALS), false).ok_or(())?;
    }
    Ok(mlen + MINMATCH)
}

/// The safe loop's match copy: within the history, and leaving the last 5 bytes of the output
/// for literals.
fn safe_match(buf: &mut Vec<u8>, h: usize, offset: usize, len: usize, oend: isize) -> Result<(), ()> {
    let o = (buf.len() - h) as isize;
    if offset as isize > h as isize + o {
        return Err(());
    }
    if o + len as isize > oend - LASTLITERALS as isize {
        return Err(());
    }
    copy_match(buf, offset, len);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Fast compression

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TableType {
    Cleared,
    U32,
    U16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DictMode {
    NoDict,
    Prefix,
    ExtDict,
    DictCtx,
}

#[inline]
fn hash_position(w: &[u8], p: usize, tt: TableType) -> usize {
    if tt != TableType::U16 {
        // LZ4_hash5 of 8 bytes (64-bit build)
        let seq = read64(w, p);
        ((seq << 24).wrapping_mul(889_523_592_379) >> (64 - HASHLOG)) as usize
    } else {
        (read32(w, p).wrapping_mul(2_654_435_761) >> (32 - (HASHLOG + 1))) as usize
    }
}

/// LZ4_stream_t: the fast compressor's state, which the original carries from block to block
/// and from file to file (one context per run).
#[derive(Clone)]
pub struct FastStream {
    t32: Vec<u32>,
    t16: Vec<u16>,
    current_offset: u32,
    table_type: TableType,
    /// The dictionary's bytes: its last 64 KB (no match reaches further back).
    dict: Vec<u8>,
    dict_size: u32,
    dict_ctx: Option<Rc<FastStream>>,
}

impl Default for FastStream {
    fn default() -> Self {
        FastStream {
            t32: vec![0; HASH_SIZE_U32],
            t16: vec![0; HASH_SIZE_U32 * 2],
            current_offset: 0,
            table_type: TableType::Cleared,
            dict: vec![],
            dict_size: 0,
            dict_ctx: None,
        }
    }
}

impl FastStream {
    /// LZ4_initStream
    pub fn init(&mut self) {
        self.t32.iter_mut().for_each(|x| *x = 0);
        self.t16.iter_mut().for_each(|x| *x = 0);
        self.current_offset = 0;
        self.table_type = TableType::Cleared;
        self.dict.clear();
        self.dict_size = 0;
        self.dict_ctx = None;
    }

    fn get(&self, h: usize, tt: TableType) -> u32 {
        if tt == TableType::U16 { self.t16[h] as u32 } else { self.t32[h] }
    }

    fn put(&mut self, h: usize, idx: u32, tt: TableType) {
        if tt == TableType::U16 {
            self.t16[h] = idx as u16;
        } else {
            self.t32[h] = idx;
        }
    }

    /// LZ4_prepareTable
    fn prepare_table(&mut self, input_size: usize, tt: TableType) {
        if self.table_type != TableType::Cleared
            && (self.table_type != tt
                || (tt == TableType::U16 && self.current_offset as usize + input_size >= 0xFFFF)
                || (tt == TableType::U32 && self.current_offset > (1 << 30))
                || input_size >= 4096)
        {
            self.t32.iter_mut().for_each(|x| *x = 0);
            self.t16.iter_mut().for_each(|x| *x = 0);
            self.current_offset = 0;
            self.table_type = TableType::Cleared;
        }
        if self.current_offset != 0 && tt == TableType::U32 {
            self.current_offset += KB64;
        }
        self.dict_ctx = None;
        self.dict.clear();
        self.dict_size = 0;
    }

    /// LZ4_resetStream_fast
    pub fn reset_fast(&mut self) {
        self.prepare_table(0, TableType::U32);
    }

    /// LZ4_loadDictSlow (the dictionary of LZ4F_createCDict)
    pub fn load_dict_slow(&mut self, dictionary: &[u8]) {
        self.init();
        self.current_offset += KB64;
        if dictionary.len() < 8 {
            return;
        }
        let d = &dictionary[dictionary.len().saturating_sub(65536)..];
        self.dict = d.to_vec();
        self.dict_size = d.len() as u32;
        self.table_type = TableType::U32;
        let start = self.current_offset - self.dict_size;
        let mut p = 0;
        while p + 8 <= d.len() {
            let h = hash_position(d, p, TableType::U32);
            self.t32[h] = start + p as u32;
            p += 3;
        }
        let limit = self.current_offset - KB64;
        p = 0;
        while p + 8 <= d.len() {
            let h = hash_position(d, p, TableType::U32);
            if self.t32[h] <= limit {
                self.t32[h] = start + p as u32;
            }
            p += 1;
        }
    }

    /// LZ4_attach_dictionary
    pub fn attach_dictionary(&mut self, dict: Option<&Rc<FastStream>>) {
        let mut d = dict.cloned();
        if let Some(ctx) = &d {
            if self.current_offset == 0 {
                self.current_offset = KB64;
            }
            if ctx.dict_size == 0 {
                d = None;
            }
        }
        self.dict_ctx = d;
    }

    /// LZ4_saveDict (into the frame's buffer: what is kept is a copy)
    pub fn save_dict(&mut self, size: usize) {
        let size = size.min(65536).min(self.dict_size as usize);
        let keep = self.dict.len() - size.min(self.dict.len());
        self.dict.drain(..keep);
        self.dict_size = size as u32;
    }

    fn set_dict_from_block(&mut self, src: &[u8]) {
        self.dict.clear();
        self.dict.extend_from_slice(&src[src.len().saturating_sub(65536)..]);
        self.dict_size = src.len() as u32;
    }

    /// LZ4_compress_fast_extState (legacy format): a fresh state for each block.
    pub fn compress_ext_state(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize, accel: i32) -> usize {
        self.init();
        let accel = accel.clamp(1, ACCELERATION_MAX);
        let limit = if cap >= compress_bound(src.len()) { None } else { Some(cap) };
        let tt = if src.len() < LIMIT_64K { TableType::U16 } else { TableType::U32 };
        self.compress_generic(src, out, limit, tt, DictMode::NoDict, false, accel)
    }

    /// LZ4_compress_fast_extState_fastReset (a frame's independent blocks)
    pub fn compress_fast_reset(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize, accel: i32) -> usize {
        let accel = accel.clamp(1, ACCELERATION_MAX);
        let limit = if cap >= compress_bound(src.len()) { None } else { Some(cap) };
        if src.len() < LIMIT_64K {
            self.prepare_table(src.len(), TableType::U16);
            let small = self.current_offset != 0;
            self.compress_generic(src, out, limit, TableType::U16, DictMode::NoDict, small, accel)
        } else {
            self.prepare_table(src.len(), TableType::U32);
            self.compress_generic(src, out, limit, TableType::U32, DictMode::NoDict, false, accel)
        }
    }

    /// LZ4_renormDictT
    fn renorm(&mut self, next_size: usize) {
        if self.current_offset as u64 + next_size as u64 > 0x8000_0000 {
            let delta = self.current_offset - KB64;
            for x in self.t32.iter_mut() {
                *x = if *x < delta { 0 } else { *x - delta };
            }
            self.current_offset = KB64;
            if self.dict_size > KB64 {
                self.dict_size = KB64;
            }
        }
    }

    /// LZ4_compress_fast_continue (linked blocks, and blocks with a dictionary). The block is
    /// never contiguous with the dictionary here (the frame keeps its history in a buffer of its
    /// own), except for the empty history of a frame's first block.
    pub fn compress_continue(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize, accel: i32) -> usize {
        let tt = TableType::U32;
        self.renorm(src.len());
        let accel = accel.clamp(1, ACCELERATION_MAX);
        let limit = if cap >= compress_bound(src.len()) { None } else { Some(cap) };
        if self.dict_size < 4 && !src.is_empty() && self.dict_ctx.is_none() {
            // a tiny dictionary is dropped: prefix mode, from an empty prefix
            self.dict.clear();
            self.dict_size = 0;
            let small = self.dict_size < KB64 && self.dict_size < self.current_offset;
            let r = self.compress_generic(src, out, limit, tt, DictMode::Prefix, small, accel);
            self.set_dict_from_block(src);
            return r;
        }
        let r = if let Some(ctx) = self.dict_ctx.clone() {
            if src.len() > 4096 {
                *self = (*ctx).clone();
                self.compress_generic(src, out, limit, tt, DictMode::ExtDict, false, accel)
            } else {
                self.compress_generic(src, out, limit, tt, DictMode::DictCtx, false, accel)
            }
        } else {
            let small = self.dict_size < KB64 && self.dict_size < self.current_offset;
            self.compress_generic(src, out, limit, tt, DictMode::ExtDict, small, accel)
        };
        self.set_dict_from_block(src);
        r
    }

    /// LZ4_compress_generic_validated (without the fillOutput mode, which the frame does not
    /// use): appends the block to `out`, or returns 0 when it does not fit in `limit` bytes
    /// (the table keeps what was inserted, as in the original).
    #[allow(clippy::too_many_arguments)]
    fn compress_generic(
        &mut self,
        src: &[u8],
        out: &mut Vec<u8>,
        limit: Option<usize>,
        tt: TableType,
        mode: DictMode,
        dict_small: bool,
        accel: i32,
    ) -> usize {
        let input_size = src.len();
        if input_size > MAX_INPUT_SIZE {
            return 0;
        }
        let out_start = out.len();
        let olimit = limit.map(|l| out_start + l);
        if input_size == 0 {
            if limit == Some(0) {
                return 0;
            }
            out.push(0);
            return 1;
        }
        let start_index = self.current_offset;
        let dctx = if mode == DictMode::DictCtx { self.dict_ctx.clone() } else { None };
        let (dict_bytes, dict_size): (&[u8], u32) = match mode {
            DictMode::NoDict => (&[], 0),
            DictMode::DictCtx => {
                let c = dctx.as_ref().unwrap();
                (&c.dict, c.dict_size)
            }
            _ => (&self.dict, self.dict_size),
        };
        let dict_delta = dctx.as_ref().map(|c| start_index.wrapping_sub(c.current_offset)).unwrap_or(0);
        let window: Cow<[u8]> = if dict_bytes.is_empty() {
            Cow::Borrowed(src)
        } else {
            let mut v = Vec::with_capacity(dict_bytes.len() + src.len());
            v.extend_from_slice(dict_bytes);
            v.extend_from_slice(src);
            Cow::Owned(v)
        };
        let w: &[u8] = &window;
        let src_pos = w.len() - input_size;
        // window position of an index of this context
        let shift = src_pos as i64 - start_index as i64;
        let prefix_idx_limit = start_index.wrapping_sub(dict_size);
        let iend = w.len();
        let mflimit_plus_one = iend - MFLIMIT + 1;
        let matchlimit = iend - LASTLITERALS;
        let dict_low = src_pos - dict_bytes.len();
        let mut low_limit = if mode == DictMode::Prefix { src_pos - dict_bytes.len() } else { src_pos };

        // update the context
        if mode == DictMode::DictCtx {
            self.dict_ctx = None;
            self.dict_size = input_size as u32;
        } else {
            self.dict_size = self.dict_size.wrapping_add(input_size as u32);
        }
        self.current_offset = self.current_offset.wrapping_add(input_size as u32);
        self.table_type = tt;

        let mut ip = src_pos;
        let mut anchor = src_pos;
        let idx_of = |p: usize| -> u32 { (p as i64 - shift) as u32 };

        'compress: {
            if input_size < MIN_LENGTH {
                break 'compress;
            }
            let h = hash_position(w, ip, tt);
            self.put(h, start_index, tt);
            ip += 1;
            let mut forward_h = hash_position(w, ip, tt);

            loop {
                // find a match
                let mut mpos;
                {
                    let mut forward_ip = ip;
                    let mut step = 1usize;
                    let mut search_match_nb = (accel as u32) << SKIP_TRIGGER;
                    loop {
                        let h = forward_h;
                        let current = idx_of(forward_ip);
                        let mut match_index = self.get(h, tt);
                        ip = forward_ip;
                        forward_ip += step;
                        step = (search_match_nb >> SKIP_TRIGGER) as usize;
                        search_match_nb += 1;
                        if forward_ip > mflimit_plus_one {
                            break 'compress;
                        }
                        if mode == DictMode::DictCtx {
                            if match_index < start_index {
                                let c = dctx.as_ref().unwrap();
                                match_index = c.t32[h].wrapping_add(dict_delta);
                                low_limit = dict_low;
                            } else {
                                low_limit = src_pos;
                            }
                        } else if mode == DictMode::ExtDict {
                            low_limit = if match_index < start_index { dict_low } else { src_pos };
                        }
                        forward_h = hash_position(w, forward_ip, tt);
                        self.put(h, current, tt);
                        if dict_small && match_index < prefix_idx_limit {
                            continue;
                        }
                        if tt != TableType::U16 && match_index.wrapping_add(DISTANCE_MAX as u32) < current {
                            continue;
                        }
                        let mp = match_index as i64 + shift;
                        if mp < 0 {
                            continue;
                        }
                        mpos = mp as usize;
                        if read32(w, mpos) == read32(w, ip) {
                            break;
                        }
                    }
                }

                // catch up
                if mpos > low_limit && w[ip - 1] == w[mpos - 1] {
                    loop {
                        ip -= 1;
                        mpos -= 1;
                        if !(ip > anchor && mpos > low_limit && w[ip - 1] == w[mpos - 1]) {
                            break;
                        }
                    }
                }

                // literals
                let lit_len = ip - anchor;
                let mut token = out.len();
                out.push(0);
                if let Some(ol) = olimit {
                    if out.len() + lit_len + (2 + 1 + LASTLITERALS) + lit_len / 255 > ol {
                        out.truncate(out_start);
                        return 0;
                    }
                }
                if lit_len >= RUN_MASK {
                    out[token] = (RUN_MASK << 4) as u8;
                    push_length(out, lit_len - RUN_MASK);
                } else {
                    out[token] = (lit_len << 4) as u8;
                }
                out.extend_from_slice(&w[anchor..ip]);

                loop {
                    // offset
                    let offset = ip - mpos;
                    debug_assert!(offset > 0 && offset <= DISTANCE_MAX);
                    out.extend_from_slice(&(offset as u16).to_le_bytes());
                    // match length
                    let mut match_code = count(w, ip + MINMATCH, mpos + MINMATCH, matchlimit);
                    ip += match_code + MINMATCH;
                    if let Some(ol) = olimit {
                        if out.len() + (1 + LASTLITERALS) + (match_code + 240) / 255 > ol {
                            out.truncate(out_start);
                            return 0;
                        }
                    }
                    if match_code >= ML_MASK {
                        out[token] += ML_MASK as u8;
                        match_code -= ML_MASK;
                        push_length(out, match_code);
                    } else {
                        out[token] += match_code as u8;
                    }
                    anchor = ip;
                    if ip >= mflimit_plus_one {
                        break 'compress;
                    }
                    // fill table
                    let h = hash_position(w, ip - 2, tt);
                    self.put(h, idx_of(ip - 2), tt);
                    // test next position
                    let h = hash_position(w, ip, tt);
                    let current = idx_of(ip);
                    let mut match_index = self.get(h, tt);
                    if mode == DictMode::DictCtx {
                        if match_index < start_index {
                            let c = dctx.as_ref().unwrap();
                            match_index = c.t32[h].wrapping_add(dict_delta);
                            low_limit = dict_low;
                        } else {
                            low_limit = src_pos;
                        }
                    } else if mode == DictMode::ExtDict {
                        low_limit = if match_index < start_index { dict_low } else { src_pos };
                    }
                    self.put(h, current, tt);
                    let mp = match_index as i64 + shift;
                    if (!dict_small || match_index >= prefix_idx_limit)
                        && (tt == TableType::U16 || match_index.wrapping_add(DISTANCE_MAX as u32) >= current)
                        && mp >= 0
                        && read32(w, mp as usize) == read32(w, ip)
                    {
                        mpos = mp as usize;
                        token = out.len();
                        out.push(0);
                        continue;
                    }
                    break;
                }
                // prepare next loop
                ip += 1;
                forward_h = hash_position(w, ip, tt);
            }
        }

        // last literals
        let last_run = iend - anchor;
        if let Some(ol) = olimit {
            if out.len() + last_run + 1 + (last_run + 255 - RUN_MASK) / 255 > ol {
                out.truncate(out_start);
                return 0;
            }
        }
        if last_run >= RUN_MASK {
            out.push((RUN_MASK << 4) as u8);
            push_length(out, last_run - RUN_MASK);
        } else {
            out.push((last_run << 4) as u8);
        }
        out.extend_from_slice(&w[anchor..iend]);
        out.len() - out_start
    }
}

/// LZ4_compress_fast with a state of its own (the legacy format's blocks).
pub fn compress_fast(src: &[u8], out: &mut Vec<u8>, cap: usize, accel: i32) -> usize {
    FastStream::default().compress_ext_state(src, out, cap, accel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x = 1u32;
        while v.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let words = [&b"lorem "[..], b"ipsum ", b"dolor ", b"sit ", b"amet, ", b"\n"];
            v.extend_from_slice(words[(x >> 16) as usize % words.len()]);
            if x % 7 == 0 {
                v.push((x >> 8) as u8);
            }
        }
        v.truncate(n);
        v
    }

    #[test]
    fn roundtrip_fast() {
        for n in [0, 1, 12, 13, 100, 5000, 70000, 300000] {
            let src = sample(n);
            let mut c = vec![];
            let r = compress_fast(&src, &mut c, compress_bound(n), 1);
            assert_eq!(r, c.len());
            let mut d = vec![];
            decompress(&c, &[], n.max(1), &mut d).unwrap();
            assert_eq!(d, src);
        }
    }

    #[test]
    fn roundtrip_continue() {
        let src = sample(200000);
        let mut s = FastStream::default();
        s.reset_fast();
        let mut hist: Vec<u8> = vec![];
        for chunk in src.chunks(65536) {
            let mut c = vec![];
            let r = s.compress_continue(chunk, &mut c, chunk.len() - 1, 1);
            s.save_dict(65536);
            if r == 0 {
                hist.extend_from_slice(chunk);
                continue;
            }
            let mut d = vec![];
            decompress(&c, &hist, 65536, &mut d).unwrap();
            assert_eq!(d, chunk);
            hist.extend_from_slice(chunk);
        }
    }
}
