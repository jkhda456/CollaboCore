//! LZ4 high compression (levels 2..12): the mid compressor of level 2, the hash chain of levels
//! 3..9 and the optimal parser of 10..12, with their streaming state (LZ4_streamHC_t), ported
//! from lz4 1.10.0's lib/lz4hc.c so that they find the same matches as the original (64-bit
//! build) and a frame comes out byte for byte the same.
//!
//! The context keeps its memory as one buffer, the external dictionary followed by the prefix
//! (the block being compressed), whose first byte has index `low_limit`: the original's two
//! segments, laid end to end as its indexes lay them.
//!
//! lz4's library is Copyright (c) 2011-2020 Yann Collet, BSD-2-Clause (archive/LICENSE-lz4-lib).

use super::block::{count, push_length, read16, read32, read64, DISTANCE_MAX, LASTLITERALS, MAX_INPUT_SIZE, MFLIMIT, MINMATCH, ML_MASK, RUN_MASK};
use std::rc::Rc;

pub const CLEVEL_MIN: i32 = 2;
pub const CLEVEL_DEFAULT: i32 = 9;
pub const CLEVEL_MAX: i32 = 12;
const HASH_LOG: u32 = 15;
const HASHTABLESIZE: usize = 1 << HASH_LOG;
const MAXD: usize = 1 << 16;
const OPTIMAL_ML: isize = (ML_MASK - 1 + MINMATCH) as isize;
const OPT_NUM: usize = 1 << 12;
const TRAILING_LITERALS: usize = 3;
const MID_HASHLOG: u32 = HASH_LOG - 1;
const MID_HASHTABLESIZE: usize = 1 << MID_HASHLOG;
const MID_HASHSIZE: usize = 8;
const KB64: u32 = 65536;
const MIN_LENGTH: usize = MFLIMIT + 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strat {
    Mid,
    Hc,
    Opt,
}

#[derive(Clone, Copy)]
struct CParams {
    strat: Strat,
    nb_searches: i32,
    target_length: usize,
}

const CL_TABLE: [CParams; 13] = [
    CParams { strat: Strat::Mid, nb_searches: 2, target_length: 16 },
    CParams { strat: Strat::Mid, nb_searches: 2, target_length: 16 },
    CParams { strat: Strat::Mid, nb_searches: 2, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 4, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 8, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 16, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 32, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 64, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 128, target_length: 16 },
    CParams { strat: Strat::Hc, nb_searches: 256, target_length: 16 },
    CParams { strat: Strat::Opt, nb_searches: 96, target_length: 64 },
    CParams { strat: Strat::Opt, nb_searches: 512, target_length: 128 },
    CParams { strat: Strat::Opt, nb_searches: 16384, target_length: OPT_NUM },
];

fn clevel_params(level: i32) -> CParams {
    let l = if level < 1 { CLEVEL_DEFAULT } else { level.min(CLEVEL_MAX) };
    CL_TABLE[l as usize]
}

#[inline]
fn hash_ptr(w: &[u8], p: usize) -> usize {
    (read32(w, p).wrapping_mul(2_654_435_761) >> (32 - HASH_LOG)) as usize
}

#[inline]
fn mid_hash4(w: &[u8], p: usize) -> usize {
    (read32(w, p).wrapping_mul(2_654_435_761) >> (32 - MID_HASHLOG)) as usize
}

#[inline]
fn mid_hash8(w: &[u8], p: usize) -> usize {
    ((read64(w, p) << 8).wrapping_mul(58_295_818_150_454_627) >> (64 - MID_HASHLOG)) as usize
}

/// LZ4_count between two buffers (a match in a dictionary context).
fn count2(a: &[u8], mut ai: usize, b: &[u8], mut bi: usize, limit: usize) -> usize {
    let start = ai;
    while ai < limit && bi < b.len() && a[ai] == b[bi] {
        ai += 1;
        bi += 1;
    }
    ai - start
}

/// LZ4HC_countBack: how far the match extends backwards (as a negative count).
fn count_back(w: &[u8], ip: usize, mp: usize, i_min: usize, m_min: usize) -> isize {
    let mut back = 0isize;
    let min = (i_min as isize - ip as isize).max(m_min as isize - mp as isize);
    while back > min && w[(ip as isize + back - 1) as usize] == w[(mp as isize + back - 1) as usize] {
        back -= 1;
    }
    back
}

fn count_back2(w: &[u8], ip: usize, d: &[u8], mp: usize, i_min: usize, m_min: usize) -> isize {
    let mut back = 0isize;
    let min = (i_min as isize - ip as isize).max(m_min as isize - mp as isize);
    while back > min && w[(ip as isize + back - 1) as usize] == d[(mp as isize + back - 1) as usize] {
        back -= 1;
    }
    back
}

/// LZ4HC_countPattern: bytes from `ip` that repeat the 4-byte pattern (little-endian phase).
fn count_pattern(w: &[u8], ip: usize, iend: usize, pattern: u32) -> usize {
    let pb = pattern.to_le_bytes();
    let mut n = 0;
    while ip + n < iend && w[ip + n] == pb[n & 3] {
        n += 1;
    }
    n
}

/// LZ4HC_reverseCountPattern: bytes before `ip` (down to `ilow`) that repeat the pattern.
fn reverse_count_pattern(w: &[u8], ip: usize, ilow: usize, pattern: u32) -> usize {
    let pb = pattern.to_le_bytes();
    let mut n = 0;
    while ip - n > ilow && w[ip - n - 1] == pb[3 - (n & 3)] {
        n += 1;
    }
    n
}

fn rotate_pattern(rotate: usize, pattern: u32) -> u32 {
    let bits = (rotate & 3) << 3;
    if bits == 0 { pattern } else { pattern.rotate_left(bits as u32) }
}

fn protect_dict_end(dict_limit: u32, match_index: u32) -> bool {
    dict_limit.wrapping_sub(1).wrapping_sub(match_index) >= 3
}

#[derive(Clone, Copy, Default)]
struct Match {
    off: i32,
    len: i32,
    back: isize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DictMode {
    NoDictCtx,
    UsingDictCtx,
}

#[derive(Clone, Copy, Default)]
struct Opt {
    price: i32,
    off: i32,
    mlen: i32,
    litlen: i32,
}

fn literals_price(litlen: i32) -> i32 {
    let mut price = litlen;
    if litlen >= RUN_MASK as i32 {
        price += 1 + (litlen - RUN_MASK as i32) / 255;
    }
    price
}

fn sequence_price(litlen: i32, mlen: i32) -> i32 {
    let mut price = 1 + 2 + literals_price(litlen);
    if mlen >= (ML_MASK + MINMATCH) as i32 {
        price += 1 + (mlen - (ML_MASK + MINMATCH) as i32) / 255;
    }
    price
}

/// The output of one block: where it starts in `out` and its limit (None: not limited).
struct Out<'a> {
    out: &'a mut Vec<u8>,
    start: usize,
    oend: Option<usize>,
}

impl Out<'_> {
    /// LZ4HC_encodeSequence: Err when the sequence does not fit (the caller gives up the block).
    fn encode_sequence(&mut self, w: &[u8], ip: &mut usize, anchor: &mut usize, match_length: i32, offset: i32) -> Result<(), ()> {
        let token = self.out.len();
        self.out.push(0);
        let length = *ip - *anchor;
        if let Some(oend) = self.oend {
            if self.out.len() + length / 255 + length + (2 + 1 + LASTLITERALS) > oend {
                return Err(());
            }
        }
        if length >= RUN_MASK {
            self.out[token] = (RUN_MASK << 4) as u8;
            push_length(self.out, length - RUN_MASK);
        } else {
            self.out[token] = (length << 4) as u8;
        }
        self.out.extend_from_slice(&w[*anchor..*ip]);
        debug_assert!(offset > 0 && offset as usize <= DISTANCE_MAX);
        self.out.extend_from_slice(&(offset as u16).to_le_bytes());
        let length = match_length as usize - MINMATCH;
        if let Some(oend) = self.oend {
            if self.out.len() + length / 255 + (1 + LASTLITERALS) > oend {
                return Err(());
            }
        }
        if length >= ML_MASK {
            self.out[token] += ML_MASK as u8;
            push_length(self.out, length - ML_MASK);
        } else {
            self.out[token] += length as u8;
        }
        *ip += match_length as usize;
        *anchor = *ip;
        Ok(())
    }

    /// The last literals: Err when they do not fit.
    fn last_literals(&mut self, w: &[u8], anchor: usize, iend: usize) -> Result<usize, ()> {
        let last_run = iend - anchor;
        let ll_add = (last_run + 255 - RUN_MASK) / 255;
        if let Some(oend) = self.oend {
            if self.out.len() + 1 + ll_add + last_run > oend {
                return Err(());
            }
        }
        if last_run >= RUN_MASK {
            self.out.push((RUN_MASK << 4) as u8);
            push_length(self.out, last_run - RUN_MASK);
        } else {
            self.out.push((last_run << 4) as u8);
        }
        self.out.extend_from_slice(&w[anchor..iend]);
        Ok(self.out.len() - self.start)
    }
}

/// LZ4HC_CCtx_internal
#[derive(Clone)]
pub struct HcCtx {
    hash: Vec<u32>,
    chain: Vec<u16>,
    /// external dictionary (dict_limit - low_limit bytes), then the prefix
    buf: Vec<u8>,
    dict_limit: u32,
    low_limit: u32,
    next_to_update: u32,
    compression_level: i32,
    favor_dec_speed: bool,
    dirty: bool,
    dict_ctx: Option<Rc<HcCtx>>,
    /// prefixStart != NULL
    started: bool,
    /// the next block follows the prefix in memory (right after an init)
    contiguous: bool,
}

impl Default for HcCtx {
    fn default() -> Self {
        let mut c = HcCtx {
            hash: vec![0; HASHTABLESIZE],
            chain: vec![0; MAXD],
            buf: vec![],
            dict_limit: 0,
            low_limit: 0,
            next_to_update: 0,
            compression_level: CLEVEL_DEFAULT,
            favor_dec_speed: false,
            dirty: false,
            dict_ctx: None,
            started: false,
            contiguous: false,
        };
        c.init();
        c
    }
}

impl HcCtx {
    fn ext_len(&self) -> usize {
        (self.dict_limit - self.low_limit) as usize
    }

    fn prefix_len(&self) -> usize {
        self.buf.len() - self.ext_len()
    }

    /// LZ4_initStreamHC
    pub fn init(&mut self) {
        self.hash.iter_mut().for_each(|x| *x = 0);
        self.chain.iter_mut().for_each(|x| *x = 0);
        self.buf.clear();
        self.dict_limit = 0;
        self.low_limit = 0;
        self.next_to_update = 0;
        self.favor_dec_speed = false;
        self.dirty = false;
        self.dict_ctx = None;
        self.started = false;
        self.contiguous = false;
        self.compression_level = CLEVEL_DEFAULT;
    }

    /// LZ4_setCompressionLevel
    pub fn set_compression_level(&mut self, level: i32) {
        self.compression_level = if level < 1 { CLEVEL_DEFAULT } else { level.min(CLEVEL_MAX) };
    }

    pub fn set_favor_dec_speed(&mut self, favor: bool) {
        self.favor_dec_speed = favor;
    }

    /// LZ4_resetStreamHC_fast
    pub fn reset_fast(&mut self, level: i32) {
        if self.dirty {
            self.init();
        } else {
            self.dict_limit = self.dict_limit.wrapping_add(self.prefix_len() as u32);
            self.buf.clear();
            self.low_limit = self.dict_limit;
            self.started = false;
            self.contiguous = false;
            self.dict_ctx = None;
        }
        self.set_compression_level(level);
    }

    /// LZ4HC_init_internal
    fn init_internal(&mut self) {
        let buffer_size = if self.started { self.prefix_len() } else { 0 };
        let mut start = buffer_size as u64 + self.dict_limit as u64;
        if start > 1 << 30 {
            self.hash.iter_mut().for_each(|x| *x = 0);
            self.chain.iter_mut().for_each(|x| *x = 0xFFFF);
            start = 0;
        }
        start += KB64 as u64;
        let start = start as u32;
        self.next_to_update = start;
        self.dict_limit = start;
        self.low_limit = start;
        self.buf.clear();
        self.started = true;
        self.contiguous = true;
    }

    /// LZ4_loadDictHC (the dictionary of LZ4F_createCDict)
    pub fn load_dict(&mut self, dictionary: &[u8]) {
        let d = &dictionary[dictionary.len().saturating_sub(65536)..];
        let level = self.compression_level;
        self.init();
        self.set_compression_level(level);
        let cp = clevel_params(level);
        self.init_internal();
        self.buf.extend_from_slice(d);
        if cp.strat == Strat::Mid {
            self.mid_fill_table(d.len());
        } else if d.len() >= 4 {
            let w = std::mem::take(&mut self.buf);
            self.insert(&w, w.len() - 3);
            self.buf = w;
        }
    }

    /// LZ4_attach_HC_dictionary
    pub fn attach_dictionary(&mut self, dict: Option<&Rc<HcCtx>>) {
        self.dict_ctx = dict.cloned();
    }

    /// LZ4MID_fillHTable
    fn mid_fill_table(&mut self, size: usize) {
        if size <= MID_HASHSIZE {
            return;
        }
        let prefix_idx = self.dict_limit;
        let target = prefix_idx + size as u32 - MID_HASHSIZE as u32;
        let base = self.ext_len();
        let w = std::mem::take(&mut self.buf);
        let pos = |idx: u32| base + (idx - prefix_idx) as usize;
        let mut idx = self.next_to_update;
        while idx < target {
            self.hash[mid_hash4(&w, pos(idx))] = idx;
            self.hash[MID_HASHTABLESIZE + mid_hash8(&w, pos(idx + 1))] = idx + 1;
            idx += 3;
        }
        idx = if size > 32 * 1024 + MID_HASHSIZE { target - 32 * 1024 } else { self.next_to_update };
        while idx < target {
            self.hash[MID_HASHTABLESIZE + mid_hash8(&w, pos(idx))] = idx;
            idx += 1;
        }
        self.next_to_update = target;
        self.buf = w;
    }

    /// LZ4HC_setExternalDict: the prefix becomes the external dictionary, the new block the prefix.
    fn set_external_dict(&mut self) {
        if self.prefix_len() >= 4 && clevel_params(self.compression_level).strat != Strat::Mid {
            let w = std::mem::take(&mut self.buf);
            let end = w.len();
            self.insert(&w, end - 3);
            self.buf = w;
        }
        let ext = self.ext_len();
        let prefix_len = self.prefix_len();
        self.buf.drain(..ext);
        self.low_limit = self.dict_limit;
        self.dict_limit += prefix_len as u32;
        self.next_to_update = self.dict_limit;
        self.dict_ctx = None;
        self.contiguous = true;
    }

    /// LZ4_saveDictHC: keeps the last 64 KB of the prefix (in the frame's buffer).
    pub fn save_dict(&mut self, size: usize) {
        let prefix_size = self.prefix_len();
        let mut size = size.min(65536);
        if size < 4 {
            size = 0;
        }
        size = size.min(prefix_size);
        let end_index = prefix_size as u32 + self.dict_limit;
        let keep = self.buf.len() - size;
        self.buf.drain(..keep);
        self.dict_limit = end_index - size as u32;
        self.low_limit = end_index - size as u32;
        if self.next_to_update < self.dict_limit {
            self.next_to_update = self.dict_limit;
        }
        self.contiguous = false;
    }

    /// LZ4_compress_HC_extStateHC_fastReset (a frame's independent blocks, the legacy format)
    pub fn compress_ext_state_fast_reset(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize, level: i32) -> usize {
        self.reset_fast(level);
        self.init_internal();
        let limit = if cap < super::block::compress_bound(src.len()) { Some(cap) } else { None };
        self.compress_generic(src, out, limit, level)
    }

    /// LZ4_compress_HC_continue
    pub fn compress_continue(&mut self, src: &[u8], out: &mut Vec<u8>, cap: usize) -> usize {
        let limit = if cap < super::block::compress_bound(src.len()) { Some(cap) } else { None };
        if !self.started {
            self.init_internal();
        }
        if self.prefix_len() as u64 + self.dict_limit as u64 > 2 << 30 {
            let n = self.prefix_len().min(65536);
            let d = self.buf[self.buf.len() - n..].to_vec();
            self.load_dict(&d);
            self.contiguous = false;
        }
        if !self.contiguous {
            self.set_external_dict();
        }
        let level = self.compression_level;
        self.compress_generic(src, out, limit, level)
    }

    /// LZ4HC_compress_generic
    fn compress_generic(&mut self, src: &[u8], out: &mut Vec<u8>, limit: Option<usize>, level: i32) -> usize {
        match self.dict_ctx.clone() {
            None => self.compress_internal(src, out, limit, level, DictMode::NoDictCtx),
            Some(dctx) => {
                let position = self.prefix_len() + self.ext_len();
                if position >= 65536 {
                    self.dict_ctx = None;
                    self.compress_internal(src, out, limit, level, DictMode::NoDictCtx)
                } else if position == 0
                    && src.len() > 4096
                    && (clevel_params(self.compression_level).strat == Strat::Mid)
                        == (clevel_params(dctx.compression_level).strat == Strat::Mid)
                {
                    *self = (*dctx).clone();
                    self.set_external_dict();
                    self.compression_level = level;
                    self.compress_internal(src, out, limit, level, DictMode::NoDictCtx)
                } else {
                    self.compress_internal(src, out, limit, level, DictMode::UsingDictCtx)
                }
            }
        }
    }

    /// LZ4HC_compress_generic_internal: the block is appended to the prefix and compressed.
    fn compress_internal(&mut self, src: &[u8], out: &mut Vec<u8>, limit: Option<usize>, level: i32, dict: DictMode) -> usize {
        if src.len() > MAX_INPUT_SIZE {
            return 0;
        }
        self.buf.extend_from_slice(src);
        // the next block is somewhere else (the frame reads each into the same buffer)
        self.contiguous = false;
        let cp = clevel_params(level);
        let favor = self.favor_dec_speed;
        let w = std::mem::take(&mut self.buf);
        let start = out.len();
        let mut o = Out { out, start, oend: limit.map(|l| start + l) };
        let src_start = w.len() - src.len();
        let r = match cp.strat {
            Strat::Mid => self.mid_compress(&w, src_start, &mut o, dict),
            Strat::Hc => self.hash_chain(&w, src_start, &mut o, cp.nb_searches, dict),
            Strat::Opt => self.optimal(&w, src_start, &mut o, cp.nb_searches, cp.target_length, level >= CLEVEL_MAX, dict, favor),
        };
        self.buf = w;
        let r = r.unwrap_or(0);
        if r == 0 {
            o.out.truncate(start);
            self.dirty = true;
        }
        r
    }

    #[inline]
    fn pos(&self, idx: u32) -> usize {
        (idx - self.low_limit) as usize
    }

    /// LZ4HC_Insert: the chains up to the position `ip` (excluded).
    fn insert(&mut self, w: &[u8], ip: usize) {
        let target = ip as u32 + self.low_limit;
        let mut idx = self.next_to_update;
        while idx < target {
            let h = hash_ptr(w, self.pos(idx));
            let delta = idx.wrapping_sub(self.hash[h]).min(DISTANCE_MAX as u32);
            self.chain[idx as u16 as usize] = delta as u16;
            self.hash[h] = idx;
            idx += 1;
        }
        self.next_to_update = target;
    }

    /// LZ4HC_InsertAndGetWiderMatch
    #[allow(clippy::too_many_arguments)]
    fn wider_match(
        &mut self,
        w: &[u8],
        ip: usize,
        i_low: usize,
        i_high: usize,
        mut longest: i32,
        max_attempts: i32,
        pattern_analysis: bool,
        chain_swap: bool,
        dict: DictMode,
        favor: bool,
    ) -> Match {
        let lo = self.low_limit;
        let prefix_idx = self.dict_limit;
        let ppos = (prefix_idx - lo) as usize;
        let ip_index = ip as u32 + lo;
        let within_start = lo.wrapping_add(DISTANCE_MAX as u32 + 1) > ip_index;
        let lowest = if within_start { lo } else { ip_index - DISTANCE_MAX as u32 };
        let dict_idx = lo;
        let look_back = (ip - i_low) as isize;
        let mut attempts = max_attempts;
        let mut chain_pos: u32 = 0;
        let pattern = read32(w, ip);
        #[derive(PartialEq)]
        enum Rep {
            Untested,
            Not,
            Confirmed,
        }
        let mut repeat = Rep::Untested;
        let mut src_pattern_len = 0usize;
        let mut offset = 0i32;
        let mut s_back = 0isize;

        self.insert(w, ip);
        let mut match_index = self.hash[hash_ptr(w, ip)];

        while match_index >= lowest && attempts > 0 {
            let mut match_length = 0i32;
            attempts -= 1;
            if favor && ip_index - match_index < 8 {
                // favorDecSpeed skips matches with offsets under 8
            } else if match_index >= prefix_idx {
                let mp = self.pos(match_index);
                // the original's 2-byte prefilter; a read before the prefix (memory of no buffer
                // there) is taken as a mismatch
                let a = i_low as isize + longest as isize - 1;
                let b = mp as isize - look_back + longest as isize - 1;
                if b >= ppos as isize && read16(w, a as usize) == read16(w, b as usize) && read32(w, mp) == pattern {
                    let back = if look_back != 0 { count_back(w, ip, mp, i_low, ppos) } else { 0 };
                    match_length = (MINMATCH + count(w, ip + MINMATCH, mp + MINMATCH, i_high)) as i32;
                    match_length -= back as i32;
                    if match_length > longest {
                        longest = match_length;
                        offset = (ip_index - match_index) as i32;
                        s_back = back;
                    }
                }
            } else {
                let mp = self.pos(match_index);
                if match_index <= prefix_idx.wrapping_sub(4) && read32(w, mp) == pattern {
                    let vlimit = (ip + (prefix_idx - match_index) as usize).min(i_high);
                    let mut ml = count(w, ip + MINMATCH, mp + MINMATCH, vlimit) + MINMATCH;
                    if ip + ml == vlimit && vlimit < i_high {
                        ml += count(w, ip + ml, ppos, i_high);
                    }
                    let back = if look_back != 0 { count_back(w, ip, mp, i_low, 0) } else { 0 };
                    let ml = ml as i32 - back as i32;
                    match_length = ml;
                    if ml > longest {
                        longest = ml;
                        offset = (ip_index - match_index) as i32;
                        s_back = back;
                    }
                }
            }

            if chain_swap && match_length == longest && match_index + longest as u32 <= ip_index {
                let k_trigger = 4;
                let mut distance_to_next = 1u32;
                let end = longest - MINMATCH as i32 + 1;
                let mut step;
                let mut accel = 1i32 << k_trigger;
                let mut p = 0i32;
                while p < end {
                    let cand = self.chain[match_index.wrapping_add(p as u32) as u16 as usize] as u32;
                    step = accel >> k_trigger;
                    accel += 1;
                    if cand > distance_to_next {
                        distance_to_next = cand;
                        chain_pos = p as u32;
                        accel = 1 << k_trigger;
                    }
                    p += step;
                }
                if distance_to_next > 1 {
                    if distance_to_next > match_index {
                        break;
                    }
                    match_index -= distance_to_next;
                    continue;
                }
            }

            let dist_next_match = self.chain[match_index as u16 as usize] as u32;
            if pattern_analysis && dist_next_match == 1 && chain_pos == 0 {
                let cand_idx = match_index.wrapping_sub(1);
                if repeat == Rep::Untested {
                    if (pattern & 0xFFFF) == (pattern >> 16) && (pattern & 0xFF) == (pattern >> 24) {
                        repeat = Rep::Confirmed;
                        src_pattern_len = count_pattern(w, ip + 4, i_high, pattern) + 4;
                    } else {
                        repeat = Rep::Not;
                    }
                }
                if repeat == Rep::Confirmed && cand_idx >= lowest && protect_dict_end(prefix_idx, cand_idx) {
                    let ext = cand_idx < prefix_idx;
                    let mp = self.pos(cand_idx);
                    if read32(w, mp) == pattern {
                        let i_limit = if ext { ppos } else { i_high };
                        let mut fwd = count_pattern(w, mp + 4, i_limit, pattern) + 4;
                        if ext && mp + fwd == i_limit {
                            let rotated = rotate_pattern(fwd, pattern);
                            fwd += count_pattern(w, ppos, i_high, rotated);
                        }
                        let lowest_ptr = if ext { 0 } else { ppos };
                        let mut back_len = reverse_count_pattern(w, mp, lowest_ptr, pattern);
                        if !ext && mp - back_len == ppos && dict_idx < prefix_idx {
                            let rotated = rotate_pattern((back_len as u32).wrapping_neg() as usize, pattern);
                            back_len += reverse_count_pattern(w, ppos, 0, rotated);
                        }
                        back_len = (cand_idx - cand_idx.wrapping_sub(back_len as u32).max(lowest)) as usize;
                        let seg = back_len + fwd;
                        if seg >= src_pattern_len && fwd <= src_pattern_len {
                            let new_mi = cand_idx.wrapping_add(fwd as u32).wrapping_sub(src_pattern_len as u32);
                            match_index = if protect_dict_end(prefix_idx, new_mi) { new_mi } else { prefix_idx };
                        } else {
                            let new_mi = cand_idx - back_len as u32;
                            if !protect_dict_end(prefix_idx, new_mi) {
                                match_index = prefix_idx;
                            } else {
                                match_index = new_mi;
                                if look_back == 0 {
                                    let max_ml = seg.min(src_pattern_len);
                                    if (longest as usize) < max_ml {
                                        if ip_index - match_index > DISTANCE_MAX as u32 {
                                            break;
                                        }
                                        longest = max_ml as i32;
                                        offset = (ip_index - match_index) as i32;
                                    }
                                    let dist = self.chain[match_index as u16 as usize] as u32;
                                    if dist > match_index {
                                        break;
                                    }
                                    match_index -= dist;
                                }
                            }
                        }
                        continue;
                    }
                }
            }

            match_index = match_index.wrapping_sub(self.chain[match_index.wrapping_add(chain_pos) as u16 as usize] as u32);
        }

        if dict == DictMode::UsingDictCtx && attempts > 0 && within_start {
            let d = self.dict_ctx.clone().unwrap();
            let db = &d.buf;
            let dict_end_offset = d.buf.len() as u32 + d.low_limit;
            let mut dict_match_index = d.hash[hash_ptr(w, ip)];
            let mut match_index = dict_match_index.wrapping_add(lowest).wrapping_sub(dict_end_offset);
            while ip_index.wrapping_sub(match_index) <= DISTANCE_MAX as u32 && attempts > 0 {
                attempts -= 1;
                if dict_match_index >= d.low_limit {
                    let mp = (dict_match_index - d.low_limit) as usize;
                    if mp + 4 <= db.len() && read32(db, mp) == pattern {
                        let vlimit = (ip + (dict_end_offset - dict_match_index) as usize).min(i_high);
                        let mut mlt = (count2(w, ip + MINMATCH, db, mp + MINMATCH, vlimit) + MINMATCH) as i32;
                        let back = if look_back != 0 { count_back2(w, ip, db, mp, i_low, 0) } else { 0 };
                        mlt -= back as i32;
                        if mlt > longest {
                            longest = mlt;
                            offset = ip_index.wrapping_sub(match_index) as i32;
                            s_back = back;
                        }
                    }
                }
                let next = d.chain[dict_match_index as u16 as usize] as u32;
                dict_match_index = dict_match_index.wrapping_sub(next);
                match_index = match_index.wrapping_sub(next);
            }
        }
        Match { len: longest, off: offset, back: s_back }
    }

    /// LZ4HC_searchExtDict through LZ4MID_searchHCDict: the dictionary context of a mid block
    /// (made at level 9, so with hash chains).
    fn mid_search_hc_dict(&self, w: &[u8], ip: usize, ip_index: u32, i_high: usize, g_dict_end_index: u32) -> Match {
        let d = self.dict_ctx.as_ref().unwrap();
        let db = &d.buf;
        let l_dict_end = db.len() as u32 + d.low_limit;
        let mut l_dict_match = d.hash[hash_ptr(w, ip)];
        let mut match_index = l_dict_match.wrapping_add(g_dict_end_index).wrapping_sub(l_dict_end);
        let mut best = MINMATCH as i32 - 1;
        let mut offset = 0;
        let mut attempts = 2;
        while ip_index.wrapping_sub(match_index) <= DISTANCE_MAX as u32 && attempts > 0 {
            attempts -= 1;
            if l_dict_match >= d.low_limit {
                let mp = (l_dict_match - d.low_limit) as usize;
                if mp + 4 <= db.len() && read32(db, mp) == read32(w, ip) {
                    let vlimit = (ip + (l_dict_end - l_dict_match) as usize).min(i_high);
                    let mlt = (count2(w, ip + MINMATCH, db, mp + MINMATCH, vlimit) + MINMATCH) as i32;
                    if mlt > best {
                        best = mlt;
                        offset = ip_index.wrapping_sub(match_index) as i32;
                    }
                }
            }
            let next = d.chain[l_dict_match as u16 as usize] as u32;
            l_dict_match = l_dict_match.wrapping_sub(next);
            match_index = match_index.wrapping_sub(next);
        }
        Match { len: best, off: offset, back: 0 }
    }

    /// LZ4MID_compress (level 2)
    fn mid_compress(&mut self, w: &[u8], src_start: usize, o: &mut Out, dict: DictMode) -> Result<usize, ()> {
        let iend = w.len();
        let input = iend - src_start;
        let mut ip = src_start;
        let mut anchor = ip;
        let mflimit = iend as isize - MFLIMIT as isize;
        let matchlimit = iend - LASTLITERALS;
        let ilimit = iend as isize - MID_HASHSIZE as isize;
        let lo = self.low_limit;
        let prefix_idx = self.dict_limit;
        let ppos = (prefix_idx - lo) as usize;
        let ilimit_idx = (ilimit + lo as isize) as u32;
        let dict_idx = lo;
        let g_dict_end_index = lo;
        let h8 = MID_HASHTABLESIZE;

        if input >= MIN_LENGTH {
            'main: while ip as isize <= mflimit {
                let ip_index = ip as u32 + lo;
                let mut match_length: usize;
                let mut match_distance: u32;
                'found: {
                    // long match
                    {
                        let h = mid_hash8(w, ip);
                        let pos8 = self.hash[h8 + h];
                        self.hash[h8 + h] = ip_index;
                        if ip_index.wrapping_sub(pos8) <= DISTANCE_MAX as u32 {
                            if pos8 >= prefix_idx {
                                let mp = self.pos(pos8);
                                match_length = count(w, ip, mp, matchlimit);
                                if match_length >= MINMATCH {
                                    match_distance = ip_index - pos8;
                                    break 'found;
                                }
                            } else if pos8 >= dict_idx {
                                let mp = self.pos(pos8);
                                let safe_len = ((prefix_idx - pos8) as usize).min(matchlimit - ip);
                                match_length = count(w, ip, mp, ip + safe_len);
                                if match_length >= MINMATCH {
                                    match_distance = ip_index - pos8;
                                    break 'found;
                                }
                            }
                        }
                    }
                    // short match
                    {
                        let h = mid_hash4(w, ip);
                        let pos4 = self.hash[h];
                        self.hash[h] = ip_index;
                        if ip_index.wrapping_sub(pos4) <= DISTANCE_MAX as u32 {
                            if pos4 >= prefix_idx {
                                let mp = self.pos(pos4);
                                match_length = count(w, ip, mp, matchlimit);
                                if match_length >= MINMATCH {
                                    // a short match: is there a longer one at ip+1?
                                    let hh = mid_hash8(w, ip + 1);
                                    let pos8 = self.hash[h8 + hh];
                                    let m2_distance = (ip_index + 1).wrapping_sub(pos8);
                                    match_distance = ip_index - pos4;
                                    if m2_distance <= DISTANCE_MAX as u32 && pos8 >= prefix_idx && (ip as isize) < mflimit {
                                        let m2 = self.pos(pos8);
                                        let ml2 = count(w, ip + 1, m2, matchlimit);
                                        if ml2 > match_length {
                                            self.hash[h8 + hh] = ip_index + 1;
                                            ip += 1;
                                            match_length = ml2;
                                            match_distance = m2_distance;
                                        }
                                    }
                                    break 'found;
                                }
                            } else if pos4 >= dict_idx {
                                let mp = self.pos(pos4);
                                let safe_len = ((prefix_idx - pos4) as usize).min(matchlimit - ip);
                                match_length = count(w, ip, mp, ip + safe_len);
                                if match_length >= MINMATCH {
                                    match_distance = ip_index - pos4;
                                    break 'found;
                                }
                            }
                        }
                    }
                    // the dictionary context
                    if dict == DictMode::UsingDictCtx && ip_index.wrapping_sub(g_dict_end_index) < DISTANCE_MAX as u32 - 8 {
                        let m = self.mid_search_hc_dict(w, ip, ip_index, matchlimit, g_dict_end_index);
                        if m.len >= MINMATCH as i32 {
                            match_length = m.len as usize;
                            match_distance = m.off as u32;
                            break 'found;
                        }
                    }
                    ip += 1 + ((ip - anchor) >> 9);
                    continue 'main;
                }

                // catch back
                while ip > anchor && ((ip - ppos) as u32) > match_distance && w[ip - 1] == w[ip - match_distance as usize - 1] {
                    ip -= 1;
                    match_length += 1;
                }
                // the beginning of the match (with the index of the position searched: the
                // original's, after catching back)
                self.hash[h8 + mid_hash8(w, ip + 1)] = ip_index + 1;
                self.hash[h8 + mid_hash8(w, ip + 2)] = ip_index + 2;
                self.hash[mid_hash4(w, ip + 1)] = ip_index + 1;

                o.encode_sequence(w, &mut ip, &mut anchor, match_length as i32, match_distance as i32)?;

                // the end of the match
                let end_idx = ip as u32 + lo;
                let pos_m2 = end_idx - 2;
                if pos_m2 < ilimit_idx {
                    if ip - ppos > 5 {
                        self.hash[h8 + mid_hash8(w, ip - 5)] = end_idx - 5;
                    }
                    self.hash[h8 + mid_hash8(w, ip - 3)] = end_idx - 3;
                    self.hash[h8 + mid_hash8(w, ip - 2)] = end_idx - 2;
                    self.hash[mid_hash4(w, ip - 2)] = end_idx - 2;
                    self.hash[mid_hash4(w, ip - 1)] = end_idx - 1;
                }
            }
        }
        o.last_literals(w, anchor, iend)
    }

    /// LZ4HC_compress_hashChain (levels 3..9)
    fn hash_chain(&mut self, w: &[u8], src_start: usize, o: &mut Out, max_attempts: i32, dict: DictMode) -> Result<usize, ()> {
        let iend = w.len();
        let input = iend - src_start;
        let pattern_analysis = max_attempts > 128;
        let mut ip = src_start;
        let mut anchor = ip;
        let mflimit = iend as isize - MFLIMIT as isize;
        let matchlimit = iend - LASTLITERALS;
        let nomatch = Match::default();

        if input >= MIN_LENGTH {
            'outer: while ip as isize <= mflimit {
                let mut m1 = self.wider_match(w, ip, ip, matchlimit, MINMATCH as i32 - 1, max_attempts, pattern_analysis, false, dict, false);
                if m1.len < MINMATCH as i32 {
                    ip += 1;
                    continue;
                }
                let mut start0 = ip;
                let mut m0 = m1;
                let mut start2: usize;
                let mut m2: Match;
                let mut start3: usize = 0;
                let mut m3: Match;
                'search2: loop {
                    if (ip + m1.len as usize) as isize <= mflimit {
                        start2 = ip + m1.len as usize - 2;
                        m2 = self.wider_match(w, start2, ip, matchlimit, m1.len, max_attempts, pattern_analysis, false, dict, false);
                        start2 = (start2 as isize + m2.back) as usize;
                    } else {
                        start2 = 0;
                        m2 = nomatch;
                    }
                    if m2.len <= m1.len {
                        o.encode_sequence(w, &mut ip, &mut anchor, m1.len, m1.off)?;
                        continue 'outer;
                    }
                    if start0 < ip && start2 < ip + m0.len as usize {
                        ip = start0;
                        m1 = m0;
                    }
                    if (start2 as isize - ip as isize) < 3 {
                        ip = start2;
                        m1 = m2;
                        continue 'search2;
                    }
                    'search3: loop {
                        if (start2 as isize - ip as isize) < OPTIMAL_ML {
                            let mut new_ml = m1.len.min(OPTIMAL_ML as i32);
                            if ip as isize + new_ml as isize > start2 as isize + m2.len as isize - MINMATCH as isize {
                                new_ml = (start2 - ip) as i32 + m2.len - MINMATCH as i32;
                            }
                            let correction = new_ml - (start2 - ip) as i32;
                            if correction > 0 {
                                start2 += correction as usize;
                                m2.len -= correction;
                            }
                        }
                        if (start2 + m2.len as usize) as isize <= mflimit {
                            start3 = start2 + m2.len as usize - 3;
                            m3 = self.wider_match(w, start3, start2, matchlimit, m2.len, max_attempts, pattern_analysis, false, dict, false);
                            start3 = (start3 as isize + m3.back) as usize;
                        } else {
                            m3 = nomatch;
                        }
                        if m3.len <= m2.len {
                            if start2 < ip + m1.len as usize {
                                m1.len = (start2 - ip) as i32;
                            }
                            o.encode_sequence(w, &mut ip, &mut anchor, m1.len, m1.off)?;
                            ip = start2;
                            o.encode_sequence(w, &mut ip, &mut anchor, m2.len, m2.off)?;
                            continue 'outer;
                        }
                        if start3 < ip + m1.len as usize + 3 {
                            if start3 >= ip + m1.len as usize {
                                if start2 < ip + m1.len as usize {
                                    let correction = (ip + m1.len as usize - start2) as i32;
                                    start2 += correction as usize;
                                    m2.len -= correction;
                                    if m2.len < MINMATCH as i32 {
                                        start2 = start3;
                                        m2 = m3;
                                    }
                                }
                                o.encode_sequence(w, &mut ip, &mut anchor, m1.len, m1.off)?;
                                ip = start3;
                                m1 = m3;
                                start0 = start2;
                                m0 = m2;
                                continue 'search2;
                            }
                            start2 = start3;
                            m2 = m3;
                            continue 'search3;
                        }
                        if start2 < ip + m1.len as usize {
                            if (start2 as isize - ip as isize) < OPTIMAL_ML {
                                if m1.len > OPTIMAL_ML as i32 {
                                    m1.len = OPTIMAL_ML as i32;
                                }
                                if ip as isize + m1.len as isize > start2 as isize + m2.len as isize - MINMATCH as isize {
                                    m1.len = (start2 - ip) as i32 + m2.len - MINMATCH as i32;
                                }
                                let correction = m1.len - (start2 - ip) as i32;
                                if correction > 0 {
                                    start2 += correction as usize;
                                    m2.len -= correction;
                                }
                            } else {
                                m1.len = (start2 - ip) as i32;
                            }
                        }
                        o.encode_sequence(w, &mut ip, &mut anchor, m1.len, m1.off)?;
                        ip = start2;
                        m1 = m2;
                        start2 = start3;
                        m2 = m3;
                        continue 'search3;
                    }
                }
            }
        }
        o.last_literals(w, anchor, iend)
    }

    /// LZ4HC_FindLongerMatch
    fn find_longer_match(&mut self, w: &[u8], ip: usize, i_high: usize, min_len: i32, nb_searches: i32, dict: DictMode, favor: bool) -> Match {
        let mut md = self.wider_match(w, ip, ip, i_high, min_len, nb_searches, true, true, dict, favor);
        if md.len <= min_len {
            return Match::default();
        }
        if favor && md.len > 18 && md.len <= 36 {
            md.len = 18;
        }
        md
    }

    /// LZ4HC_compress_optimal (levels 10..12)
    #[allow(clippy::too_many_arguments)]
    fn optimal(
        &mut self,
        w: &[u8],
        src_start: usize,
        o: &mut Out,
        nb_searches: i32,
        sufficient_len: usize,
        full_update: bool,
        dict: DictMode,
        favor: bool,
    ) -> Result<usize, ()> {
        let iend = w.len();
        let mut ip = src_start;
        let mut anchor = ip;
        let mflimit = iend as isize - MFLIMIT as isize;
        let matchlimit = iend - LASTLITERALS;
        let sufficient_len = sufficient_len.min(OPT_NUM - 1);
        let mut opt = vec![Opt::default(); OPT_NUM + TRAILING_LITERALS];
        let favor_i = favor as i32;

        while ip as isize <= mflimit {
            let llen = (ip - anchor) as i32;
            let first = self.find_longer_match(w, ip, matchlimit, MINMATCH as i32 - 1, nb_searches, dict, favor);
            if first.len == 0 {
                ip += 1;
                continue;
            }
            if first.len as usize > sufficient_len {
                o.encode_sequence(w, &mut ip, &mut anchor, first.len, first.off)?;
                continue;
            }
            for r in 0..MINMATCH {
                opt[r] = Opt { price: literals_price(llen + r as i32), off: 0, mlen: 1, litlen: llen + r as i32 };
            }
            for mlen in MINMATCH as i32..=first.len {
                opt[mlen as usize] = Opt { price: sequence_price(llen, mlen), off: first.off, mlen, litlen: llen };
            }
            let mut last_match_pos = first.len as usize;
            for add in 1..=TRAILING_LITERALS {
                opt[last_match_pos + add] = Opt { price: opt[last_match_pos].price + literals_price(add as i32), off: 0, mlen: 1, litlen: add as i32 };
            }

            let mut cur = 1usize;
            let mut best: Option<(i32, i32)> = None;
            while cur < last_match_pos {
                let cur_ptr = ip + cur;
                if cur_ptr as isize > mflimit {
                    break;
                }
                if full_update {
                    if opt[cur + 1].price <= opt[cur].price && opt[cur + MINMATCH].price < opt[cur].price + 3 {
                        cur += 1;
                        continue;
                    }
                } else if opt[cur + 1].price <= opt[cur].price {
                    cur += 1;
                    continue;
                }
                let new = if full_update {
                    self.find_longer_match(w, cur_ptr, matchlimit, MINMATCH as i32 - 1, nb_searches, dict, favor)
                } else {
                    self.find_longer_match(w, cur_ptr, matchlimit, (last_match_pos - cur) as i32, nb_searches, dict, favor)
                };
                if new.len == 0 {
                    cur += 1;
                    continue;
                }
                if new.len as usize > sufficient_len || new.len as usize + cur >= OPT_NUM {
                    best = Some((new.len, new.off));
                    last_match_pos = cur + 1;
                    break;
                }
                let base_litlen = opt[cur].litlen;
                for litlen in 1..MINMATCH as i32 {
                    let price = opt[cur].price - literals_price(base_litlen) + literals_price(base_litlen + litlen);
                    let pos = cur + litlen as usize;
                    if price < opt[pos].price {
                        opt[pos] = Opt { price, off: 0, mlen: 1, litlen: base_litlen + litlen };
                    }
                }
                let match_ml = new.len;
                for ml in MINMATCH as i32..=match_ml {
                    let pos = cur + ml as usize;
                    let (ll, price) = if opt[cur].mlen == 1 {
                        let ll = opt[cur].litlen;
                        let base = if cur as i32 > ll { opt[cur - ll as usize].price } else { 0 };
                        (ll, base + sequence_price(ll, ml))
                    } else {
                        (0, opt[cur].price + sequence_price(0, ml))
                    };
                    if pos > last_match_pos + TRAILING_LITERALS || price <= opt[pos].price - favor_i {
                        if ml == match_ml && last_match_pos < pos {
                            last_match_pos = pos;
                        }
                        opt[pos] = Opt { price, off: new.off, mlen: ml, litlen: ll };
                    }
                }
                for add in 1..=TRAILING_LITERALS {
                    opt[last_match_pos + add] = Opt { price: opt[last_match_pos].price + literals_price(add as i32), off: 0, mlen: 1, litlen: add as i32 };
                }
                cur += 1;
            }

            // encode, from the position the last match starts at (the immediate one's: cur)
            let (best_mlen, best_off) = best.unwrap_or((opt[last_match_pos].mlen, opt[last_match_pos].off));
            let mut candidate_pos = match best {
                Some(_) => last_match_pos - 1,
                None => last_match_pos - best_mlen as usize,
            };
            let mut selected_ml = best_mlen;
            let mut selected_off = best_off;
            loop {
                let next_ml = opt[candidate_pos].mlen;
                let next_off = opt[candidate_pos].off;
                opt[candidate_pos].mlen = selected_ml;
                opt[candidate_pos].off = selected_off;
                selected_ml = next_ml;
                selected_off = next_off;
                if next_ml as usize > candidate_pos {
                    break;
                }
                candidate_pos -= next_ml as usize;
            }
            let mut rpos = 0usize;
            while rpos < last_match_pos {
                let ml = opt[rpos].mlen;
                let off = opt[rpos].off;
                if ml == 1 {
                    ip += 1;
                    rpos += 1;
                    continue;
                }
                rpos += ml as usize;
                o.encode_sequence(w, &mut ip, &mut anchor, ml, off)?;
            }
        }
        o.last_literals(w, anchor, iend)
    }
}

/// LZ4_compress_HC with a state of its own (the legacy format's blocks).
pub fn compress_hc(src: &[u8], out: &mut Vec<u8>, cap: usize, level: i32) -> usize {
    let mut ctx = HcCtx::default();
    ctx.compress_ext_state_fast_reset(src, out, cap, level)
}

#[cfg(test)]
mod tests {
    use super::super::block::{compress_bound, decompress};
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x = 7u32;
        while v.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let words = [&b"alpha "[..], b"beta ", b"gamma ", b"aaaaaaaaaaaa", b"abababab", b"\n"];
            v.extend_from_slice(words[(x >> 16) as usize % words.len()]);
            if x % 5 == 0 {
                v.push((x >> 8) as u8);
            }
        }
        v.truncate(n);
        v
    }

    #[test]
    fn roundtrip_levels() {
        for level in 2..=12 {
            for n in [0, 5, 13, 100, 4000, 70000, 200000] {
                let src = sample(n);
                let mut c = vec![];
                let r = compress_hc(&src, &mut c, compress_bound(n), level);
                assert_eq!(r, c.len(), "level {level} size {n}");
                let mut d = vec![];
                decompress(&c, &[], n.max(1), &mut d).unwrap();
                assert!(d == src, "level {level} size {n}");
            }
        }
    }

    #[test]
    fn roundtrip_linked() {
        for level in [2, 5, 9, 11] {
            let src = sample(300000);
            let mut ctx = HcCtx::default();
            ctx.reset_fast(level);
            let mut hist: Vec<u8> = vec![];
            for chunk in src.chunks(65536) {
                let mut c = vec![];
                let r = ctx.compress_continue(chunk, &mut c, chunk.len() - 1);
                ctx.save_dict(65536);
                if r != 0 {
                    let mut d = vec![];
                    decompress(&c, &hist, 65536, &mut d).unwrap();
                    assert!(d == chunk, "level {level}");
                }
                hist.extend_from_slice(chunk);
            }
        }
    }
}
