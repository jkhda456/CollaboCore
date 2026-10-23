//! lz4, unlz4, lz4cat: the LZ4 command line (1.10.0), its single-threaded build, over a port of
//! lz4's own library (block.rs, hc.rs, frame.rs) that writes the same frames byte for byte.
//!
//! What the original does is kept: its argument syntax (aggregated short options, `lz4 [arg]
//! [input] [output]`, -m and -r, `-B#`, `--fast[=#]`, `-l` for the legacy format, `-D` for a
//! dictionary, the LZ4_CLEVEL and LZ4_NBWORKERS variables), its file handling (sources kept unless
//! --rm, the overwrite question, sparse files, the source's times, owner and mode given to the
//! result), its messages and exit codes, `--list`, pass-through (lz4cat, -dcf), skippable and
//! concatenated frames. Left out: benchmarking (-b says so). -T is accepted, as the original's
//! single-threaded build accepts it (with its warning).
//!
//! This command line is a port of lz4's programs/lz4cli.c and lz4io.c (Copyright Yann Collet),
//! GPL-2.0-or-later (archive/COPYING-lz4-programs), as is this file.

mod block;
mod frame;
mod hc;

use crate::common::{errstr, stdin_is_tty, stdout_is_tty};
use frame::{CCtx, CDict, DCtx, FrameError, Prefs};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::Instant;

use frame::{LEGACY_MAGIC, MAGIC};

const VERSION: &str = "1.10.0";
const STDINMARK: &str = "stdin";
const STDOUTMARK: &str = "stdout";
const NULL_OUTPUT: &str = "null";
const NULMARK: &str = "/dev/null";
const EXT: &str = ".lz4";
const CLEVEL_DEFAULT: i32 = 1;
const NBWORKERS_DEFAULT: u32 = 0;
const BLOCKSIZEID_DEFAULT: u32 = 7;
const LEGACY_BLOCKSIZE: usize = 8 << 20;
const MAGICNUMBER_SIZE: usize = 4;
const D_BUFFER_SIZE: usize = 64 << 10;
const PT_SIZE: usize = 64 << 10;
const MAX_DICT_SIZE: usize = 64 << 10;
const REFRESH_NS: u128 = 200_000_000;

/// END_PROCESS, unwound to main so that the output is flushed (the original's fflush(NULL)).
struct Exit(i32);
type R<T> = Result<T, Exit>;

fn welcome() -> String {
    format!("*** lz4 v{} {}-bit single-thread, by Yann Collet ***\n", VERSION, usize::BITS)
}

/// The display: the command line's level, and lz4io's (set from it once the arguments are read),
/// with its progress throttle.
struct Disp {
    level: i32,
    last_update: Option<Instant>,
}

impl Disp {
    fn show(&self, l: i32, s: &str) {
        if self.level >= l {
            eprint!("{s}");
        }
    }

    fn show_bytes(&self, l: i32, b: &[u8]) {
        if self.level >= l {
            let _ = io::stderr().write_all(b);
        }
    }

    /// DISPLAYUPDATE: at most every 200 ms (always at -vv and above); the first one at once.
    fn update(&mut self, l: i32, s: &str) {
        if self.level >= l {
            let due = match self.last_update {
                None => true,
                Some(t) => t.elapsed().as_nanos() > REFRESH_NS,
            };
            if due || self.level >= 4 {
                self.last_update = Some(Instant::now());
                eprint!("{s}");
            }
        }
    }

    /// END_PROCESS's message (lz4io's form: " \n" after it).
    fn fatal(&self, code: i32, msg: &str) -> Exit {
        self.show(1, &format!("Error {code} : "));
        self.show(1, msg);
        self.show(1, " \n");
        Exit(code)
    }
}

macro_rules! fatal {
    ($d:expr, $code:expr, $($arg:tt)*) => {
        return Err($d.fatal($code, &format!($($arg)*)))
    };
}

#[derive(Clone)]
struct IoPrefs {
    pass_through: bool,
    overwrite: bool,
    test_mode: bool,
    block_size_id: u32,
    block_size: usize,
    block_checksum: bool,
    stream_checksum: bool,
    block_independence: bool,
    sparse: i32,
    content_size: bool,
    dictionary: Option<String>,
    favor_dec_speed: bool,
    remove_src: bool,
}

impl IoPrefs {
    fn set_block_size_id(&mut self, id: u32) -> usize {
        if !(4..=7).contains(&id) {
            return 0;
        }
        self.block_size_id = id;
        self.block_size = frame::block_size(id);
        self.block_size
    }

    fn set_block_size(&mut self, size: usize) -> usize {
        let size = size.clamp(32, 4 << 20);
        self.block_size = size;
        let mut s = size - 1;
        let mut id = 0;
        loop {
            s >>= 2;
            if s == 0 {
                break;
            }
            id += 1;
        }
        if id < 7 {
            id = 7;
        }
        self.block_size_id = id - 3;
        size
    }
}

// ---------------------------------------------------------------------------------------------
// Files

/// A FILE* opened for reading: fread's semantics (as much as asked, unless the end or an error
/// comes first), feof, ferror, ftell and fseeko.
struct Src {
    file: Option<File>,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    err: bool,
}

impl Src {
    fn new(file: Option<File>) -> Src {
        Src { file, buf: vec![0; 64 << 10], pos: 0, len: 0, eof: false, err: false }
    }

    fn fd(&self) -> i32 {
        self.file.as_ref().map(|f| f.as_raw_fd()).unwrap_or(0)
    }

    fn fill(&mut self) -> bool {
        loop {
            let n = unsafe { libc::read(self.fd(), self.buf.as_mut_ptr() as *mut libc::c_void, self.buf.len()) };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.err = true;
                return false;
            }
            if n == 0 {
                self.eof = true;
                return false;
            }
            self.pos = 0;
            self.len = n as usize;
            return true;
        }
    }

    /// fread
    fn read(&mut self, out: &mut [u8]) -> usize {
        let mut got = 0;
        while got < out.len() {
            if self.pos == self.len && !self.fill() {
                break;
            }
            let n = (self.len - self.pos).min(out.len() - got);
            out[got..got + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            got += n;
        }
        got
    }

    /// ftell (-1 on a pipe)
    fn tell(&self) -> i64 {
        let p = unsafe { libc::lseek(self.fd(), 0, libc::SEEK_CUR) };
        if p < 0 {
            return -1;
        }
        p as i64 - (self.len - self.pos) as i64
    }

    /// fseeko(SEEK_CUR)
    fn seek_cur(&mut self, off: i64) -> bool {
        let rel = off - (self.len - self.pos) as i64;
        let r = unsafe { libc::lseek(self.fd(), rel as libc::off_t, libc::SEEK_CUR) };
        if r < 0 {
            return false;
        }
        self.pos = 0;
        self.len = 0;
        self.eof = false;
        true
    }

    /// fseeko(SEEK_END)
    fn seek_end(&mut self, off: i64) -> bool {
        let r = unsafe { libc::lseek(self.fd(), off as libc::off_t, libc::SEEK_END) };
        if r < 0 {
            return false;
        }
        self.pos = 0;
        self.len = 0;
        self.eof = false;
        true
    }

    /// UTIL_getOpenFileSize: a regular file's size, else 0
    fn size(&self) -> u64 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.fd(), &mut st) } != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return 0;
        }
        st.st_size as u64
    }
}

/// A FILE* opened for writing (stdout, or a file), buffered as stdio is; fseeko for sparse files.
struct Dst {
    file: Option<File>,
    buf: Vec<u8>,
    stdout: bool,
}

const DST_BUF: usize = 64 << 10;

impl Dst {
    fn fd(&self) -> i32 {
        self.file.as_ref().map(|f| f.as_raw_fd()).unwrap_or(1)
    }

    fn write_raw(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = unsafe { libc::write(self.fd(), data.as_ptr() as *const libc::c_void, data.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            data = &data[n as usize..];
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        let b = std::mem::take(&mut self.buf);
        let r = self.write_raw(&b);
        self.buf = b;
        self.buf.clear();
        r
    }

    /// fwrite: all of it, or an error
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.buf.len() + data.len() > DST_BUF {
            self.flush()?;
            if data.len() >= DST_BUF {
                return self.write_raw(data);
            }
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn seek_cur(&mut self, off: u64) -> io::Result<()> {
        self.flush()?;
        if unsafe { libc::lseek(self.fd(), off as libc::off_t, libc::SEEK_CUR) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Dst {
    /// fclose (its errors not looked at, as the original does not)
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn c_path(p: &str) -> std::ffi::CString {
    std::ffi::CString::new(p.as_bytes()).unwrap_or_default()
}

fn stat(p: &str) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c_path(p).as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st)
}

fn is_reg(st: &libc::stat) -> bool {
    (st.st_mode & libc::S_IFMT) == libc::S_IFREG
}

/// UTIL_getFileStat: a regular file's stat
fn file_stat(p: &str) -> Option<libc::stat> {
    stat(p).filter(is_reg)
}

fn is_directory(p: &str) -> bool {
    stat(p).map(|s| (s.st_mode & libc::S_IFMT) == libc::S_IFDIR).unwrap_or(false)
}

/// UTIL_setFileStat: modification time (access time now), owner, mode.
fn set_file_stat(p: &str, st: &libc::stat) {
    if file_stat(p).is_none() {
        return;
    }
    let c = c_path(p);
    let mut times = [crate::common::timespec(0, 0), crate::common::timespec(st.st_mtime as i64, 0)];
    times[0].tv_nsec = libc::UTIME_NOW;
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
        libc::chown(c.as_ptr(), st.st_uid, st.st_gid);
        libc::chmod(c.as_ptr(), st.st_mode & 0o7777);
    }
}

fn is_stdin(s: &str) -> bool {
    s == STDINMARK
}

fn is_stdout(s: &str) -> bool {
    s == STDOUTMARK
}

fn is_dev_null(s: &str) -> bool {
    s == NULMARK
}

/// LZ4IO_openSrcFile
fn open_src(d: &Disp, name: &str) -> Option<Src> {
    if is_stdin(name) {
        d.show(4, "Using stdin for input \n");
        return Some(Src::new(None));
    }
    if is_directory(name) {
        d.show(1, &format!("lz4: {name} is a directory -- ignored \n"));
        return None;
    }
    match File::open(name) {
        Ok(f) => Some(Src::new(Some(f))),
        Err(e) => {
            d.show(1, &format!("{}: {} \n", name, errstr(&e)));
            None
        }
    }
}

/// LZ4IO_openDstFile
fn open_dst(d: &Disp, name: &str, prefs: &IoPrefs) -> Option<Dst> {
    if is_stdout(name) {
        d.show(4, "Using stdout for output \n");
        if prefs.sparse == 1 {
            d.show(4, "Sparse File Support automatically disabled on stdout ; to force-enable it, add --sparse command \n");
        }
        return Some(Dst { file: None, buf: vec![], stdout: true });
    }
    if !prefs.overwrite && !is_dev_null(name) && File::open(name).is_ok() {
        if d.level <= 1 {
            eprint!("{name} already exists; not overwritten  \n");
            return None;
        }
        eprint!("{name} already exists; do you want to overwrite (y/N) ? ");
        let ch = getchar();
        if ch != Some(b'Y') && ch != Some(b'y') {
            eprint!("    not overwritten  \n");
            return None;
        }
        let mut c = ch;
        while c.is_some() && c != Some(b'\n') {
            c = getchar();
        }
    }
    match OpenOptions::new().write(true).create(true).truncate(true).mode(0o666).open(name) {
        Ok(f) => Some(Dst { file: Some(f), buf: vec![], stdout: false }),
        Err(e) => {
            d.show(1, &format!("{}: {}\n", name, errstr(&e)));
            None
        }
    }
}

fn getchar() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return if n == 1 { Some(b[0]) } else { None };
    }
}

/// LZ4IO_createDict: the last 64 KB of the file.
fn create_dict(d: &Disp, name: &str) -> R<Vec<u8>> {
    let mut f = match open_src(d, name) {
        Some(f) => f,
        None => fatal!(d, 27, "Dictionary error : could not open dictionary file"),
    };
    if !is_stdin(name) {
        f.seek_end(-(MAX_DICT_SIZE as i64));
    }
    let mut all = vec![];
    let mut chunk = vec![0u8; MAX_DICT_SIZE];
    loop {
        let n = f.read(&mut chunk);
        if n == 0 {
            break;
        }
        all.extend_from_slice(&chunk[..n]);
        if all.len() > 2 * MAX_DICT_SIZE {
            all.drain(..all.len() - MAX_DICT_SIZE);
        }
    }
    let keep = all.len().min(MAX_DICT_SIZE);
    Ok(all[all.len() - keep..].to_vec())
}

// ---------------------------------------------------------------------------------------------
// Sparse writes

/// LZ4IO_fwriteSparse: zeros become holes (on a file; on stdout only with --sparse).
fn fwrite_sparse(d: &Disp, f: &mut Dst, buf: &[u8], sparse: i32, mut skips: u64) -> R<u64> {
    let sparse_mode = (sparse - f.stdout as i32) > 0;
    if !sparse_mode {
        if f.write(buf).is_err() {
            fatal!(d, 70, "Write error : cannot write decoded block");
        }
        return Ok(0);
    }
    const GB: u64 = 1 << 30;
    if skips > GB {
        if f.seek_cur(GB).is_err() {
            fatal!(d, 71, "1 GB skip error (sparse file support)");
        }
        skips -= GB;
    }
    const W: usize = 8;
    let words = buf.len() / W;
    let seg_words = (32 << 10) / W;
    let mut p = 0;
    let mut left = words;
    while p < words {
        let seg = seg_words.min(left);
        left -= seg;
        let mut nb0 = 0;
        while nb0 < seg && buf[(p + nb0) * W..(p + nb0 + 1) * W].iter().all(|&b| b == 0) {
            nb0 += 1;
        }
        skips += (nb0 * W) as u64;
        if nb0 != seg {
            if let Err(e) = f.seek_cur(skips) {
                fatal!(d, 72, "Sparse skip error({}): {} ; try --no-sparse", e.raw_os_error().unwrap_or(0), errstr(&e));
            }
            skips = 0;
            if f.write(&buf[(p + nb0) * W..(p + seg) * W]).is_err() {
                fatal!(d, 73, "Write error : cannot write decoded block");
            }
        }
        p += seg;
    }
    let rest = &buf[words * W..];
    if !rest.is_empty() {
        let z = rest.iter().take_while(|&&b| b == 0).count();
        skips += z as u64;
        if z != rest.len() {
            if f.seek_cur(skips).is_err() {
                fatal!(d, 74, "Sparse skip error ; try --no-sparse");
            }
            skips = 0;
            if f.write(&rest[z..]).is_err() {
                fatal!(d, 75, "Write error : cannot write decoded end of block");
            }
        }
    }
    Ok(skips)
}

fn fwrite_sparse_end(d: &Disp, f: &mut Dst, skips: u64) -> R<()> {
    if skips > 0 {
        if f.seek_cur(skips - 1).is_err() {
            fatal!(d, 69, "Final skip error (sparse file)\n");
        }
        if f.write(&[0]).is_err() {
            fatal!(d, 69, "Write error : cannot write last zero\n");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Compression

extern "C" {
    fn clock() -> libc::c_long;
}

/// clock(): the process's processor time, CLOCKS_PER_SEC (POSIX: a million) to a second
fn cpu_clock() -> i64 {
    unsafe { clock() as i64 }
}

fn final_time_display(d: &Disp, start: Instant, cpu_start: i64, size: u64) {
    let ns = start.elapsed().as_nanos() as f64;
    let seconds = (ns + if ns == 0.0 { 1.0 } else { 0.0 }) / 1e9;
    let cpu = (cpu_clock() - cpu_start) as f64 / 1_000_000.0;
    d.show(3, &format!("Done in {:.2} s ==> {:.2} MiB/s  (cpu load : {:.0}%)\n", seconds, size as f64 / seconds / 1024.0 / 1024.0, cpu / seconds * 100.0));
}

fn pct(a: u64, b: u64) -> f64 {
    a as f64 / b as f64 * 100.0
}

/// What is written is decoded again on the way (verify.rs) and must give back the source.
struct Check {
    tee: crate::verify::Tee<io::Sink>,
    input: crc32fast::Hasher,
    len: u64,
}

impl Check {
    fn new(dict: Option<Vec<u8>>) -> Check {
        Check { tee: crate::verify::Tee::new(io::sink(), crate::verify::Codec::Lz4, dict), input: crc32fast::Hasher::new(), len: 0 }
    }

    fn add(&mut self, src: &[u8], compressed: &[u8]) {
        self.input.update(src);
        self.len += src.len() as u64;
        let _ = self.tee.write_all(compressed);
    }

    fn finish(self) -> Result<(), String> {
        let sum = (self.len, self.input.finalize());
        self.tee.finish(sum).map_err(|m| crate::verify::VerifyError(m).to_string())
    }
}

/// cRess_t
struct CRess {
    ctx: CCtx,
    prepared: Prefs,
    cdict: Option<Rc<CDict>>,
    dict: Option<Vec<u8>>,
    src_buffer: Vec<u8>,
}

fn create_c_resources(d: &Disp, io: &IoPrefs) -> R<CRess> {
    let dict = match &io.dictionary {
        Some(name) => Some(create_dict(d, name)?),
        None => None,
    };
    let cdict = dict.as_ref().map(|b| Rc::new(CDict::new(b)));
    Ok(CRess {
        ctx: CCtx::default(),
        prepared: Prefs {
            level: 0,
            block_size_id: io.block_size_id,
            linked: !io.block_independence,
            block_checksum: io.block_checksum,
            content_checksum: io.stream_checksum,
            content_size: 0,
            favor_dec_speed: io.favor_dec_speed,
        },
        cdict,
        dict,
        src_buffer: vec![0; 4 << 20],
    })
}

/// LZ4IO_compressFilename_extRess_ST: 0, or 1 when a file could not be opened.
fn compress_file(d: &mut Disp, ress: &mut CRess, src: &str, dst: &str, level: i32, io: &IoPrefs, in_size: &mut u64) -> R<i32> {
    let mut filesize = 0u64;
    let mut compressed = 0u64;
    let block_size = io.block_size;
    let mut sf = match open_src(d, src) {
        Some(f) => f,
        None => return Ok(1),
    };
    let mut df = match open_dst(d, dst, io) {
        Some(f) => f,
        None => return Ok(1),
    };
    let mut prefs = ress.prepared;
    prefs.level = level;
    if io.content_size {
        let size = sf.size();
        prefs.content_size = size;
        if size == 0 {
            d.show(3, "Warning : cannot determine input content size \n");
        }
    }
    let cdict = ress.cdict.clone();
    let mut check = Check::new(ress.dict.clone());
    let mut read_size = sf.read(&mut ress.src_buffer[..block_size]);
    if sf.err {
        fatal!(d, 40, "Error reading {} ", src);
    }
    filesize += read_size as u64;
    let mut out = Vec::with_capacity(block_size + block_size / 255 + 64);
    if read_size < block_size {
        if let Err(e) = ress.ctx.compress_frame(&ress.src_buffer[..read_size], &prefs, cdict.as_ref(), &mut out) {
            fatal!(d, 41, "Compression failed : {}", e.name());
        }
        compressed = out.len() as u64;
        check.add(&ress.src_buffer[..read_size], &out);
        d.update(2, &format!("\rRead : {} MiB   ==> {:.2}%   ", (filesize >> 20) as u32, pct(compressed, filesize + (filesize == 0) as u64)));
        if df.write(&out).is_err() {
            fatal!(d, 42, "Write error : failed writing single-block compressed frame");
        }
    } else {
        ress.ctx.begin(&prefs, cdict.as_ref(), &mut out);
        check.add(&[], &out);
        if df.write(&out).is_err() {
            fatal!(d, 44, "Write error : cannot write header");
        }
        compressed += out.len() as u64;
        while read_size > 0 {
            out.clear();
            ress.ctx.update(&ress.src_buffer[..read_size], &mut out, false);
            check.add(&ress.src_buffer[..read_size], &out);
            compressed += out.len() as u64;
            d.update(2, &format!("\rRead : {} MiB   ==> {:.2}%   ", (filesize >> 20) as u32, pct(compressed, filesize)));
            if df.write(&out).is_err() {
                fatal!(d, 46, "Write error : cannot write compressed block");
            }
            read_size = sf.read(&mut ress.src_buffer[..block_size]);
            filesize += read_size as u64;
        }
        if sf.err {
            fatal!(d, 47, "Error reading {} ", src);
        }
        out.clear();
        if let Err(e) = ress.ctx.end(&mut out) {
            fatal!(d, 48, "End of frame error : {}", e.name());
        }
        check.add(&[], &out);
        if df.write(&out).is_err() {
            fatal!(d, 49, "Write error : cannot write end of frame");
        }
        compressed += out.len() as u64;
    }
    if let Err(m) = check.finish() {
        fatal!(d, 41, "Compression failed : {}", m);
    }
    drop(sf);
    if !is_stdout(dst) {
        drop(df);
    } else {
        let _ = df.flush();
    }
    if !is_stdin(src) && !is_stdout(dst) && !is_dev_null(dst) {
        if let Some(st) = file_stat(src) {
            set_file_stat(dst, &st);
        }
    }
    if io.remove_src {
        if let Err(e) = std::fs::remove_file(src) {
            fatal!(d, 50, "Remove error : {}: {}", src, errstr(&e));
        }
    }
    d.show(2, &format!("\r{:79}\r", ""));
    d.show(2, &format!("Compressed {} bytes into {} bytes ==> {:.2}%\n", filesize, compressed, pct(compressed, filesize + (filesize == 0) as u64)));
    *in_size = filesize;
    Ok(0)
}

fn compress_filename(d: &mut Disp, src: &str, dst: &str, level: i32, io: &IoPrefs) -> R<i32> {
    let start = Instant::now();
    let cpu = cpu_clock();
    let mut ress = create_c_resources(d, io)?;
    let mut processed = 0;
    let r = compress_file(d, &mut ress, src, dst, level, io, &mut processed)?;
    final_time_display(d, start, cpu, processed);
    Ok(r)
}

fn compress_multiple(d: &mut Disp, files: &[String], suffix: &str, level: i32, io: &IoPrefs) -> R<i32> {
    let start = Instant::now();
    let cpu = cpu_clock();
    let mut ress = create_c_resources(d, io)?;
    let mut missed = 0;
    let mut total = 0;
    for f in files {
        let mut processed = 0;
        if is_stdout(suffix) {
            missed += compress_file(d, &mut ress, f, STDOUTMARK, level, io, &mut processed)?;
        } else {
            missed += compress_file(d, &mut ress, f, &format!("{f}{suffix}"), level, io, &mut processed)?;
        }
        total += processed;
    }
    final_time_display(d, start, cpu, total);
    Ok(missed)
}

/// LZ4IO_compressLegacy_internal: the legacy format, 8 MB blocks each compressed alone.
fn compress_legacy_file(d: &mut Disp, src: &str, dst: &str, level: i32, io: &IoPrefs, read: &mut u64) -> R<i32> {
    *read = 0;
    let mut sf = match open_src(d, src) {
        Some(f) => f,
        None => return Ok(1),
    };
    let mut df = match open_dst(d, dst, io) {
        Some(f) => f,
        None => return Ok(1),
    };
    if df.write(&LEGACY_MAGIC.to_le_bytes()).is_err() {
        fatal!(d, 23, "Write error : cannot write header");
    }
    let mut total_c = MAGICNUMBER_SIZE as u64;
    let mut total_read = 0u64;
    let mut inbuf = vec![0u8; LEGACY_BLOCKSIZE];
    let cap = block::compress_bound(LEGACY_BLOCKSIZE) + 4;
    let mut blocks = 0u64;
    let mut check = Check::new(None);
    check.add(&[], &LEGACY_MAGIC.to_le_bytes());
    loop {
        let n = sf.read(&mut inbuf);
        total_read += n as u64;
        if n == 0 {
            break;
        }
        let mut out = vec![0u8; 4];
        let c = if level < 3 {
            let accel = if level < 0 { -level } else { 0 };
            block::compress_fast(&inbuf[..n], &mut out, cap, accel)
        } else {
            hc::compress_hc(&inbuf[..n], &mut out, cap, level)
        };
        if c == 0 {
            if level < 3 {
                fatal!(d, 51, "fast compression failed");
            }
            fatal!(d, 52, "HC compression failed");
        }
        out[..4].copy_from_slice(&(c as u32).to_le_bytes());
        check.add(&inbuf[..n], &out);
        if df.write(&out).is_err() {
            fatal!(d, 38, "Write error : cannot write compressed block");
        }
        total_c += out.len() as u64;
        blocks += 1;
        let processed = blocks * LEGACY_BLOCKSIZE as u64;
        d.update(2, &format!("\rRead : {} MiB   ==> {:.2}%   ", (processed >> 20) as u32, pct(total_c - MAGICNUMBER_SIZE as u64, processed)));
        if n < LEGACY_BLOCKSIZE {
            break;
        }
    }
    if let Err(m) = check.finish() {
        fatal!(d, 52, "Compression failed : {}", m);
    }
    d.show(2, &format!("\r{:79}\r", ""));
    d.show(2, &format!("Compressed {} bytes into {} bytes ==> {:.2}% \n", total_read, total_c, pct(total_c, total_read + (total_read == 0) as u64)));
    *read = total_read;
    drop(sf);
    if is_stdout(dst) {
        let _ = df.flush();
    }
    Ok(0)
}

fn compress_legacy(d: &mut Disp, src: &str, dst: &str, level: i32, io: &IoPrefs) -> R<i32> {
    let start = Instant::now();
    let cpu = cpu_clock();
    let mut processed = 0;
    let r = compress_legacy_file(d, src, dst, level, io, &mut processed)?;
    final_time_display(d, start, cpu, processed);
    Ok(r)
}

fn compress_legacy_multiple(d: &mut Disp, files: &[String], suffix: &str, level: i32, io: &IoPrefs) -> R<i32> {
    let start = Instant::now();
    let cpu = cpu_clock();
    let mut missed = 0;
    let mut total = 0;
    for f in files {
        let mut processed = 0;
        if is_stdout(suffix) {
            missed += compress_legacy_file(d, f, STDOUTMARK, level, io, &mut processed)?;
        } else {
            missed += compress_legacy_file(d, f, &format!("{f}{suffix}"), level, io, &mut processed)?;
        }
        total += processed;
    }
    final_time_display(d, start, cpu, total);
    Ok(missed)
}

// ---------------------------------------------------------------------------------------------
// Decompression

/// dRess_t, and selectDecoder's statics (nbFrames, g_magicRead), which carry over from file to
/// file as the original's do.
struct DRess {
    dctx: DCtx,
    src: Vec<u8>,
    dst: Vec<u8>,
    nb_frames: u32,
    magic_read: u32,
}

fn create_d_resources(d: &Disp, io: &IoPrefs) -> R<DRess> {
    let dict = match &io.dictionary {
        Some(name) => create_dict(d, name)?,
        None => vec![],
    };
    Ok(DRess { dctx: DCtx::new(dict), src: vec![0; D_BUFFER_SIZE], dst: Vec::with_capacity(D_BUFFER_SIZE), nb_frames: 0, magic_read: 0 })
}

const ENDOFSTREAM: u64 = u64::MAX;
const DECODING_ERROR: u64 = u64::MAX - 1;

/// LZ4IO_decodeLegacyStream (single-threaded)
fn decode_legacy(d: &mut Disp, ress: &mut DRess, sf: &mut Src, df: &mut Dst, io: &IoPrefs) -> R<u64> {
    let mut stream = 0u64;
    let mut skips = 0u64;
    let mut inbuf = vec![0u8; block::compress_bound(LEGACY_BLOCKSIZE)];
    loop {
        let mut h = [0u8; 4];
        let n = sf.read(&mut h);
        if n == 0 {
            break;
        }
        if n != 4 {
            fatal!(d, 62, "Error: cannot read block size in Legacy format");
        }
        let bs = u32::from_le_bytes(h);
        if bs as usize > block::compress_bound(LEGACY_BLOCKSIZE) {
            ress.magic_read = bs;
            break;
        }
        if sf.read(&mut inbuf[..bs as usize]) != bs as usize {
            fatal!(d, 63, "Read error : cannot access compressed block !");
        }
        let mut out = Vec::with_capacity(LEGACY_BLOCKSIZE);
        if block::decompress(&inbuf[..bs as usize], &[], LEGACY_BLOCKSIZE, &mut out).is_err() {
            fatal!(d, 64, "Decoding Failed ! Corrupted input detected !");
        }
        stream += out.len() as u64;
        skips = fwrite_sparse(d, df, &out, io.sparse, skips)?;
    }
    if sf.err {
        fatal!(d, 65, "Read error : ferror");
    }
    fwrite_sparse_end(d, df, skips)?;
    Ok(stream)
}

/// LZ4IO_decompressLZ4F (single-threaded)
fn decompress_lz4f(d: &mut Disp, ress: &mut DRess, sf: &mut Src, df: &mut Dst, io: &IoPrefs) -> R<u64> {
    let mut filesize = 0u64;
    let mut skips = 0u64;
    let skip_crc = !io.block_checksum && !io.stream_checksum;
    let mut next_to_load;
    {
        let mut none = vec![];
        match ress.dctx.decompress(&mut none, 0, &MAGIC.to_le_bytes(), skip_crc) {
            Ok((_, hint)) => next_to_load = hint,
            Err(e) => fatal!(d, 62, "Header error : {}", e.name()),
        }
    }
    while next_to_load != 0 {
        let mut pos = 0;
        let mut decoded = D_BUFFER_SIZE;
        if next_to_load > D_BUFFER_SIZE {
            next_to_load = D_BUFFER_SIZE;
        }
        let read_size = sf.read(&mut ress.src[..next_to_load]);
        if read_size == 0 {
            break;
        }
        while pos < read_size || decoded == D_BUFFER_SIZE {
            ress.dst.clear();
            match ress.dctx.decompress(&mut ress.dst, D_BUFFER_SIZE, &ress.src[pos..read_size], false) {
                Ok((used, hint)) => {
                    next_to_load = hint;
                    pos += used;
                }
                Err(e) => fatal!(d, 66, "Decompression error : {}", e.name()),
            }
            decoded = ress.dst.len();
            if decoded > 0 {
                if !io.test_mode {
                    let out = std::mem::take(&mut ress.dst);
                    skips = fwrite_sparse(d, df, &out, io.sparse, skips)?;
                    ress.dst = out;
                }
                filesize += decoded as u64;
                d.update(2, &format!("\rDecompressed : {} MiB  ", (filesize >> 20) as u32));
            }
            if next_to_load == 0 {
                break;
            }
        }
    }
    if sf.err {
        fatal!(d, 67, "Read error");
    }
    if !io.test_mode {
        fwrite_sparse_end(d, df, skips)?;
    }
    if next_to_load != 0 {
        fatal!(d, 68, "Unfinished stream (nextToLoad={})", next_to_load as u32);
    }
    Ok(filesize)
}

/// LZ4IO_passThrough
fn pass_through(d: &mut Disp, sf: &mut Src, df: &mut Dst, mn: &[u8; 4], sparse: i32) -> R<u64> {
    let mut total = MAGICNUMBER_SIZE as u64;
    let mut skips = 0;
    if df.write(mn).is_err() {
        fatal!(d, 50, "Pass-through write error");
    }
    let mut buf = vec![0u8; PT_SIZE];
    loop {
        let n = sf.read(&mut buf);
        total += n as u64;
        skips = fwrite_sparse(d, df, &buf[..n], sparse, skips)?;
        if n == 0 {
            break;
        }
    }
    if sf.err {
        fatal!(d, 51, "Read Error");
    }
    fwrite_sparse_end(d, df, skips)?;
    Ok(total)
}

/// fseek_u32: forward, by reading when the input cannot seek
fn skip_forward(sf: &mut Src, mut off: u64) -> bool {
    let step = 1u64 << 30;
    while off > 0 {
        let s = off.min(step);
        if sf.seek_cur(s as i64) {
            off -= s;
            continue;
        }
        let mut buf = vec![0u8; 16 << 10];
        while off > 0 {
            let t = off.min(buf.len() as u64) as usize;
            if sf.read(&mut buf[..t]) != t {
                return false;
            }
            off -= t as u64;
        }
    }
    true
}

/// selectDecoder: one frame (or a skippable one, or a legacy stream, or pass-through).
fn select_decoder(d: &mut Disp, ress: &mut DRess, sf: &mut Src, df: &mut Dst, io: &IoPrefs) -> R<u64> {
    let mut mn = [0u8; 4];
    ress.nb_frames += 1;
    let mut magic = if ress.magic_read != 0 {
        let m = ress.magic_read;
        ress.magic_read = 0;
        m
    } else {
        let n = sf.read(&mut mn);
        if n == 0 {
            ress.nb_frames = 0;
            return Ok(ENDOFSTREAM);
        }
        if n != MAGICNUMBER_SIZE {
            fatal!(d, 40, "Unrecognized header : Magic Number unreadable");
        }
        u32::from_le_bytes(mn)
    };
    if magic & frame::SKIPPABLE_MASK == frame::SKIPPABLE0 {
        magic = frame::SKIPPABLE0;
    }
    match magic {
        frame::MAGIC => decompress_lz4f(d, ress, sf, df, io),
        frame::LEGACY_MAGIC => {
            d.show(4, "Detected : Legacy format \n");
            decode_legacy(d, ress, sf, df, io)
        }
        frame::SKIPPABLE0 => {
            d.show(4, "Skipping detected skippable area \n");
            if sf.read(&mut mn) != 4 {
                fatal!(d, 42, "Stream error : skippable size unreadable");
            }
            if !skip_forward(sf, u32::from_le_bytes(mn) as u64) {
                fatal!(d, 43, "Stream error : cannot skip skippable area");
            }
            Ok(0)
        }
        _ => {
            if ress.nb_frames == 1 {
                if !io.test_mode && io.overwrite && io.pass_through {
                    ress.nb_frames = 0;
                    return pass_through(d, sf, df, &mn, io.sparse);
                }
                fatal!(d, 44, "Unrecognized header : file cannot be decoded");
            }
            let position = sf.tell();
            d.show(2, "Stream followed by undecodable data ");
            if position != -1 {
                d.show(2, &format!("at position {} ", position as i32));
            }
            d.show(2, "\n");
            Ok(DECODING_ERROR)
        }
    }
}

/// LZ4IO_decompressSrcFile
fn decompress_src(d: &mut Disp, ress: &mut DRess, df: &mut Dst, src: &str, io: &IoPrefs, out_size: &mut u64) -> R<i32> {
    let mut filesize = 0u64;
    let mut result = 0;
    let mut sf = match open_src(d, src) {
        Some(f) => f,
        None => return Ok(1),
    };
    loop {
        let n = select_decoder(d, ress, &mut sf, df, io)?;
        if n == ENDOFSTREAM {
            break;
        }
        if n == DECODING_ERROR {
            result = 1;
            break;
        }
        filesize += n;
    }
    drop(sf);
    if io.remove_src {
        if let Err(e) = std::fs::remove_file(src) {
            fatal!(d, 45, "Remove error : {}: {}", src, errstr(&e));
        }
    }
    d.show(2, &format!("\r{:79}\r", ""));
    let mut line = vec![];
    let name = src.as_bytes();
    let shown = &name[..name.len().min(30)];
    line.extend_from_slice(shown);
    line.resize(line.len() + 30 - shown.len(), b' ');
    line.extend_from_slice(format!(" : decoded {filesize} bytes \n").as_bytes());
    d.show_bytes(2, &line);
    *out_size = filesize;
    Ok(result)
}

/// LZ4IO_decompressDstFile
fn decompress_dst(d: &mut Disp, ress: &mut DRess, src: &str, dst: &str, io: &IoPrefs, out_size: &mut u64) -> R<i32> {
    let mut df = match open_dst(d, dst, io) {
        Some(f) => f,
        None => return Ok(1),
    };
    let st = if !is_stdin(src) { file_stat(src) } else { None };
    let r = decompress_src(d, ress, &mut df, src, io, out_size)?;
    if is_stdout(dst) {
        let _ = df.flush();
    } else {
        drop(df);
    }
    if let Some(st) = st {
        if !is_stdout(dst) && !is_dev_null(dst) {
            set_file_stat(dst, &st);
        }
    }
    Ok(r)
}

fn decompress_filename(d: &mut Disp, src: &str, dst: &str, io: &IoPrefs) -> R<i32> {
    let mut ress = create_d_resources(d, io)?;
    let start = Instant::now();
    let cpu = cpu_clock();
    let mut processed = 0;
    let r = decompress_dst(d, &mut ress, src, dst, io, &mut processed)?;
    if r != 0 {
        final_time_display(d, start, cpu, processed);
    }
    Ok(r)
}

fn decompress_multiple(d: &mut Disp, files: &[String], suffix: &str, io: &IoPrefs) -> R<i32> {
    let mut total = 0;
    let mut skipped = 0;
    let mut missing = 0;
    let mut ress = create_d_resources(d, io)?;
    let start = Instant::now();
    let cpu = cpu_clock();
    if !io.block_checksum && !io.stream_checksum {
        d.show(4, "disabling checksum validation during decoding \n");
    }
    let mut stdout_dst = open_dst(d, STDOUTMARK, io);
    for f in files {
        let mut processed = 0;
        if is_stdout(suffix) || is_dev_null(suffix) {
            let df = stdout_dst.as_mut().unwrap();
            missing += decompress_src(d, &mut ress, df, f, io, &mut processed)?;
            total += processed;
            continue;
        }
        if f.len() <= suffix.len() || !f.ends_with(suffix) {
            d.show(1, &format!("File extension doesn't match expected LZ4_EXTENSION ({:>4}); will not process file: {}\n", suffix, f));
            skipped += 1;
            continue;
        }
        let out = &f[..f.len() - suffix.len()];
        missing += decompress_dst(d, &mut ress, f, out, io, &mut processed)?;
        total += processed;
    }
    if let Some(df) = stdout_dst.as_mut() {
        let _ = df.flush();
    }
    final_time_display(d, start, cpu, total);
    Ok(missing + skipped)
}

// ---------------------------------------------------------------------------------------------
// --list

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameType {
    Lz4,
    Legacy,
    Skippable,
}

const FRAME_TYPE_NAMES: [&str; 3] = ["LZ4Frame", "LegacyFrame", "SkippableFrame"];

fn frame_type_name(t: FrameType) -> &'static str {
    FRAME_TYPE_NAMES[t as usize]
}

#[derive(Clone, Copy)]
struct FInfo {
    info: frame::FrameInfo,
    ftype: FrameType,
}

impl Default for FInfo {
    fn default() -> Self {
        FInfo { info: frame::FrameInfo { linked: true, block_size_id: 4, ..Default::default() }, ftype: FrameType::Lz4 }
    }
}

struct CFileInfo {
    file_size: u64,
    frame_count: u64,
    summary: FInfo,
    eq_frame_types: bool,
    eq_block_types: bool,
    all_content_size: bool,
}

fn block_type_id(bsid: u32, linked: bool) -> String {
    format!("B{}{}", bsid, if linked { 'D' } else { 'I' })
}

fn to_human(mut size: f64) -> String {
    let units = ['\0', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];
    let mut i = 0;
    while size >= 1024.0 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{size:.2}") } else { format!("{size:.2}{}", units[i]) }
}

/// LZ4IO_skipBlocksData
fn skip_blocks_data(sf: &mut Src, block_checksum: bool, content_checksum: bool) -> u64 {
    let mut info = [0u8; 4];
    let mut total = 0u64;
    loop {
        if sf.read(&mut info) == 0 {
            if sf.eof {
                return total;
            }
            return 0;
        }
        total += 4;
        let next = (u32::from_le_bytes(info) & 0x7FFF_FFFF) as u64;
        let next_block = next + if block_checksum { 4 } else { 0 };
        if next == 0 {
            if content_checksum {
                if !sf.seek_cur(4) {
                    return 0;
                }
                total += 4;
            }
            break;
        }
        total += next_block;
        if !sf.seek_cur(next_block as i64) {
            return 0;
        }
    }
    total
}

const LEGACY_UNDECODABLE: u64 = u64::MAX;

/// LZ4IO_skipLegacyBlocksData
fn skip_legacy_blocks_data(d: &Disp, sf: &mut Src) -> R<u64> {
    let mut info = [0u8; 4];
    let mut total = 0u64;
    loop {
        let n = sf.read(&mut info);
        if n == 0 {
            if sf.eof {
                return Ok(total);
            }
            return Ok(LEGACY_UNDECODABLE);
        }
        if n != 4 {
            return Ok(LEGACY_UNDECODABLE);
        }
        let next = u32::from_le_bytes(info);
        if next == LEGACY_MAGIC || next == MAGIC || (next & frame::SKIPPABLE_MASK) == frame::SKIPPABLE0 {
            if !sf.seek_cur(-4) {
                fatal!(d, 37, "impossible to skip backward");
            }
            break;
        }
        if next as usize > LEGACY_BLOCKSIZE {
            d.show(4, "Error : block in legacy frame is too large \n");
            return Ok(LEGACY_UNDECODABLE);
        }
        total += 4 + next as u64;
        if !sf.seek_cur(next as i64) {
            return Ok(LEGACY_UNDECODABLE);
        }
    }
    Ok(total)
}

#[derive(PartialEq)]
enum InfoResult {
    Ok,
    NotKnown,
    NotAFile,
}

/// LZ4IO_getCompressedFileInfo
fn get_file_info(d: &Disp, cf: &mut CFileInfo, name: &str, display_now: bool) -> R<InfoResult> {
    let mut result = InfoResult::NotKnown;
    let mut buffer = [0u8; frame::HEADER_SIZE_MAX];
    let mut sf = match open_src(d, name) {
        Some(f) => f,
        None => return Ok(InfoResult::NotAFile),
    };
    cf.file_size = sf.size();
    let mut out = io::stdout().lock();
    while !sf.eof {
        let mut fi = FInfo::default();
        let n = sf.read(&mut buffer[..4]);
        if n == 0 {
            break;
        }
        result = InfoResult::NotKnown;
        if n != 4 {
            fatal!(d, 40, "Unrecognized header : Magic Number unreadable");
        }
        let mut magic = u32::from_le_bytes(buffer[..4].try_into().unwrap());
        if magic & frame::SKIPPABLE_MASK == frame::SKIPPABLE0 {
            magic = frame::SKIPPABLE0;
        }
        match magic {
            frame::MAGIC => 'lz4: {
                if cf.summary.ftype != FrameType::Lz4 {
                    cf.eq_frame_types = false;
                }
                let r = sf.read(&mut buffer[4..frame::HEADER_SIZE_MIN]);
                if r == 0 || sf.err {
                    fatal!(d, 71, "Error reading {}", name);
                }
                let h_size = match frame::header_size(&buffer[..frame::HEADER_SIZE_MIN]) {
                    Ok(h) => h,
                    Err(_) => break 'lz4,
                };
                if h_size > frame::HEADER_SIZE_MIN + MAGICNUMBER_SIZE {
                    let r = sf.read(&mut buffer[frame::HEADER_SIZE_MIN..h_size]);
                    if r == 0 || sf.err {
                        fatal!(d, 72, "Error reading {}", name);
                    }
                }
                let info = match frame::frame_info(&buffer[..h_size]) {
                    Ok(i) => i,
                    Err(_) => break 'lz4,
                };
                fi.info = info;
                if (cf.summary.info.block_size_id != info.block_size_id || cf.summary.info.linked != info.linked) && cf.frame_count != 0 {
                    cf.eq_block_types = false;
                }
                let total = skip_blocks_data(&mut sf, info.block_checksum, info.content_checksum);
                if total != 0 {
                    if display_now {
                        let _ = write!(
                            out,
                            "    {:6} {:>14} {:>5} {:>8}",
                            cf.frame_count + 1,
                            frame_type_name(fi.ftype),
                            block_type_id(info.block_size_id, info.linked),
                            if info.content_checksum { "XXH32" } else { "-" }
                        );
                    }
                    if info.content_size != 0 {
                        let ratio = (total + h_size as u64) as f64 / info.content_size as f64 * 100.0;
                        if display_now {
                            let _ = writeln!(out, " {:20} {:20} {:9.2}%", total + h_size as u64, info.content_size, ratio);
                        }
                        fi.info.content_size += cf.summary.info.content_size;
                    } else {
                        if display_now {
                            let _ = writeln!(out, " {:20} {:>20} {:>9} ", total + h_size as u64, "-", "-");
                        }
                        cf.all_content_size = false;
                    }
                    result = InfoResult::Ok;
                }
            }
            frame::LEGACY_MAGIC => {
                fi.ftype = FrameType::Legacy;
                if cf.summary.ftype != FrameType::Legacy && cf.frame_count != 0 {
                    cf.eq_frame_types = false;
                }
                cf.eq_block_types = false;
                cf.all_content_size = false;
                let total = skip_legacy_blocks_data(d, &mut sf)?;
                if total == LEGACY_UNDECODABLE {
                    d.show(1, "Corrupted legacy frame \n");
                    result = InfoResult::NotKnown;
                } else if total != 0 {
                    if display_now {
                        let _ = writeln!(out, "    {:6} {:>14} {:>5} {:>8} {:20} {:>20} {:>9}", cf.frame_count + 1, frame_type_name(fi.ftype), "-", "-", total + 4, "-", "-");
                    }
                    result = InfoResult::Ok;
                }
            }
            frame::SKIPPABLE0 => {
                fi.ftype = FrameType::Skippable;
                if cf.summary.ftype != FrameType::Skippable && cf.frame_count != 0 {
                    cf.eq_frame_types = false;
                }
                cf.eq_block_types = false;
                cf.all_content_size = false;
                if sf.read(&mut buffer[..4]) != 4 {
                    fatal!(d, 42, "Stream error : skippable size unreadable");
                }
                let size = u32::from_le_bytes(buffer[..4].try_into().unwrap());
                if !skip_forward(&mut sf, size as u64) {
                    fatal!(d, 43, "Stream error : cannot skip skippable area");
                }
                if display_now {
                    let _ = writeln!(out, "    {:6} {:>14} {:>5} {:>8} {:20} {:>20} {:>9}", cf.frame_count + 1, "SkippableFrame", "-", "-", size.wrapping_add(8), "-", "-");
                }
                result = InfoResult::Ok;
            }
            _ => {
                let position = sf.tell();
                d.show(3, "Stream followed by undecodable data ");
                if position != -1 {
                    d.show(3, &format!("at position {} ", position as i32));
                }
                result = InfoResult::NotKnown;
                d.show(3, "\n");
            }
        }
        if result != InfoResult::Ok {
            break;
        }
        cf.summary = fi;
        cf.frame_count += 1;
    }
    Ok(result)
}

/// LZ4IO_displayCompressedFilesInfo
fn display_files_info(d: &Disp, files: &[String]) -> R<i32> {
    if d.level < 3 {
        println!("{:>10} {:>14} {:>5} {:>11} {:>13} {:>8}   {}", "Frames", "Type", "Block", "Compressed", "Uncompressed", "Ratio", "Filename");
    }
    for (idx, name) in files.iter().enumerate() {
        let base = match name.rfind('/').or_else(|| name.rfind('\\')) {
            Some(i) => &name[i + 1..],
            None => name.as_str(),
        };
        let regular = if is_stdin(name) {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            unsafe { libc::fstat(0, &mut st) == 0 && is_reg(&st) }
        } else {
            file_stat(name).is_some()
        };
        if !regular {
            d.show(1, &format!("lz4: {name} is not a regular file \n"));
            return Ok(1);
        }
        if d.level >= 3 {
            println!("{}({}/{})", base, idx + 1, files.len());
            println!("    {:>6} {:>14} {:>5} {:>8} {:>20} {:>20} {:>9}", "Frame", "Type", "Block", "Checksum", "Compressed", "Uncompressed", "Ratio");
        }
        let mut cf = CFileInfo { file_size: 0, frame_count: 0, summary: FInfo::default(), eq_frame_types: true, eq_block_types: true, all_content_size: true };
        if get_file_info(d, &mut cf, name, d.level >= 3)? != InfoResult::Ok {
            d.show(1, &format!("lz4: {name}: File format not recognized \n"));
            return Ok(1);
        }
        if d.level >= 3 {
            println!();
        } else {
            print!(
                "{:10} {:>14} {:>5} {:>11} {:>13} ",
                cf.frame_count,
                if cf.eq_frame_types { frame_type_name(cf.summary.ftype) } else { "-" },
                if cf.eq_block_types { block_type_id(cf.summary.info.block_size_id, cf.summary.info.linked) } else { "-".into() },
                to_human(cf.file_size as f64),
                if cf.all_content_size { to_human(cf.summary.info.content_size as f64) } else { "-".into() }
            );
            if cf.all_content_size {
                let ratio = cf.file_size as f64 / cf.summary.info.content_size as f64 * 100.0;
                println!("{:8.2}%  {} ", ratio, base);
            } else {
                println!("{:>8}   {}", "-", base);
            }
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// The command line

fn usage(exe: &str) {
    let lines = [
        "Usage : \n".to_string(),
        format!("      {exe} [arg] [input] [output] \n"),
        "\n".into(),
        "input   : a filename \n".into(),
        format!("          with no FILE, or when FILE is - or {STDINMARK}, read standard input\n"),
        "Arguments : \n".into(),
        " -1     : fast compression (default) \n".into(),
        format!(" -{:2}    : slowest compression level \n", hc::CLEVEL_MAX),
        format!(" -T#    : use # threads for compression (default:{NBWORKERS_DEFAULT}==auto) \n"),
        format!(" -d     : decompression (default for {EXT} extension)\n"),
        " -f     : overwrite output without prompting \n".into(),
        " -k     : preserve source files(s)  (default) \n".into(),
        "--rm    : remove source file(s) after successful de/compression \n".into(),
        " -h/-H  : display help/long help and exit \n".into(),
    ];
    eprint!("{}", lines.concat());
}

fn usage_advanced(exe: &str, legacy: bool) {
    eprint!("{}", welcome());
    usage(exe);
    let lines = [
        "\n".to_string(),
        "Advanced arguments :\n".into(),
        " -V     : display Version number and exit \n".into(),
        " -v     : verbose mode \n".into(),
        " -q     : suppress warnings; specify twice to suppress errors too\n".into(),
        " -c     : force write to standard output, even if it is the console\n".into(),
        " -t     : test compressed file integrity\n".into(),
        " -m     : multiple input files (implies automatic output filenames)\n".into(),
        " -r     : operate recursively on directories (sets also -m) \n".into(),
        " -l     : compress using Legacy format (Linux kernel compression)\n".into(),
        " -z     : force compression \n".into(),
        " -D FILE: use FILE as dictionary (compression & decompression)\n".into(),
        " -B#    : cut file into blocks of size # bytes [32+] \n".into(),
        format!("                     or predefined block size [4-7] (default: {BLOCKSIZEID_DEFAULT}) \n"),
        " -BI    : Block Independence (default) \n".into(),
        " -BD    : Block dependency (improves compression ratio) \n".into(),
        " -BX    : enable block checksum (default:disabled) \n".into(),
        "--no-frame-crc : disable stream checksum (default:enabled) \n".into(),
        "--content-size : compressed frame includes original size (default:not present)\n".into(),
        "--list FILE : lists information about .lz4 files (useful for files compressed with --content-size flag)\n".into(),
        "--[no-]sparse  : sparse mode (default:enabled on file, disabled on stdout)\n".into(),
        "--favor-decSpeed: compressed files decompress faster, but are less compressed \n".into(),
        "--fast[=#]: switch to ultra fast compression level (default: 1)\n".into(),
        format!("--best  : same as -{}\n", hc::CLEVEL_MAX),
        "Benchmark arguments : \n".into(),
        " -b#    : benchmark file(s), using # compression level (default : 1) \n".into(),
        " -e#    : test all compression levels from -bX to # (default : 1)\n".into(),
        " -i#    : minimum evaluation time in seconds (default : 3s) \n".into(),
    ];
    eprint!("{}", lines.concat());
    if legacy {
        let lines = [
            "Legacy arguments : \n",
            " -c0    : fast compression \n",
            " -c1    : high compression \n",
            " -c2,-hc: very high compression \n",
            " -y     : overwrite output without prompting \n",
        ];
        eprint!("{}", lines.concat());
    }
}

fn usage_longhelp(exe: &str, legacy: bool) {
    usage_advanced(exe, legacy);
    let lines = [
        "\n".to_string(),
        "****************************\n".into(),
        "***** Advanced comment *****\n".into(),
        "****************************\n".into(),
        "\n".into(),
        "Which values can [output] have ? \n".into(),
        "---------------------------------\n".into(),
        "[output] : a filename \n".into(),
        format!("          '{STDOUTMARK}', or '-' for standard output (pipe mode)\n"),
        format!("          '{NULL_OUTPUT}' to discard output (test mode) \n"),
        "[output] can be left empty. In this case, it receives the following value :\n".into(),
        "          - if stdout is not the console, then [output] = stdout \n".into(),
        "          - if stdout is console : \n".into(),
        format!("               + for compression, output to filename{EXT} \n"),
        format!("               + for decompression, output to filename without '{EXT}'\n"),
        format!("                    > if input filename has no '{EXT}' extension : error \n"),
        "\n".into(),
        "Compression levels : \n".into(),
        "---------------------\n".into(),
        "-0 ... -2  => Fast compression, all identical\n".into(),
        format!("-3 ... -{} => High compression; higher number == more compression but slower\n", hc::CLEVEL_MAX),
        "\n".into(),
        "stdin, stdout and the console : \n".into(),
        "--------------------------------\n".into(),
        "To protect the console from binary flooding (bad argument mistake)\n".into(),
        format!("{exe} will refuse to read from console, or write to console \n"),
        "except if '-c' command is specified, to force output to console \n".into(),
        "\n".into(),
        "Simple example :\n".into(),
        "----------------\n".into(),
        "1 : compress 'filename' fast, using default output name 'filename.lz4'\n".into(),
        format!("          {exe} filename\n"),
        "\n".into(),
        "Short arguments can be aggregated. For example :\n".into(),
        "----------------------------------\n".into(),
        "2 : compress 'filename' in high compression mode, overwrite output if exists\n".into(),
        format!("          {exe} -9 -f filename \n"),
        "    is equivalent to :\n".into(),
        format!("          {exe} -9f filename \n"),
        "\n".into(),
        format!("{exe} can be used in 'pure pipe mode'. For example :\n"),
        "-------------------------------------\n".into(),
        "3 : compress data stream from 'generator', send result to 'consumer'\n".into(),
        format!("          generator | {exe} | consumer \n"),
    ];
    eprint!("{}", lines.concat());
    if legacy {
        let lines = [
            "\n".to_string(),
            "***** Warning  ***** \n".into(),
            "Legacy arguments take precedence. Therefore : \n".into(),
            "--------------------------------- \n".into(),
            format!("          {exe} -hc filename \n"),
            "means 'compress filename in high compression mode' \n".into(),
            "It is not equivalent to : \n".into(),
            format!("          {exe} -h -c filename \n"),
            "which displays help text and exits \n".into(),
        ];
        eprint!("{}", lines.concat());
    }
}

/// readU32FromChar: digits, then K/M with an optional i and B (wrapping as unsigned does).
fn read_u32(s: &[u8], p: &mut usize) -> u32 {
    let mut r: u32 = 0;
    while *p < s.len() && s[*p].is_ascii_digit() {
        r = r.wrapping_mul(10).wrapping_add((s[*p] - b'0') as u32);
        *p += 1;
    }
    if *p < s.len() && (s[*p] == b'K' || s[*p] == b'M') {
        r <<= 10;
        if s[*p] == b'M' {
            r <<= 10;
        }
        *p += 1;
        if *p < s.len() && s[*p] == b'i' {
            *p += 1;
        }
        if *p < s.len() && s[*p] == b'B' {
            *p += 1;
        }
    }
    r
}

fn exe_name_match(exe: &str, test: &str) -> bool {
    exe.starts_with(test) && (exe.len() == test.len() || exe.as_bytes()[test.len()] == b'.')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Compress,
    Decompress,
    Test,
    Bench,
    List,
}

fn determine_op_mode(name: &str) -> Mode {
    if name.len() >= EXT.len() && name.ends_with(EXT) || name == EXT { Mode::Decompress } else { Mode::Compress }
}

/// UTIL_createFileList: directories expanded (in readdir's order), other names as they are.
fn create_file_list(names: &[String]) -> Option<Vec<String>> {
    fn walk(dir: &str, out: &mut Vec<String>) -> bool {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Cannot open directory '{}': {}", dir, errstr(&e));
                return true;
            }
        };
        for ent in rd {
            let ent = match ent {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("readdir({}) error: {}", dir, errstr(&e));
                    return false;
                }
            };
            let path = format!("{}/{}", dir, ent.file_name().to_string_lossy());
            if is_directory(&path) {
                if !walk(&path, out) {
                    return false;
                }
            } else {
                out.push(path);
            }
        }
        true
    }
    let mut out = vec![];
    for n in names {
        if !is_directory(n) {
            out.push(n.clone());
        } else if !walk(n, &mut out) {
            return None;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn main(argv0: &str, argv: Vec<String>) -> i32 {
    match run(argv0, argv) {
        Ok(code) | Err(Exit(code)) => code,
    }
}

fn run(argv0: &str, argv: Vec<String>) -> R<i32> {
    let exe = argv0.rsplit('/').next().unwrap_or(argv0).to_string();
    let exe = exe.rsplit('\\').next().unwrap_or(&exe).to_string();
    let mut dl: i32 = 2;
    let mut legacy_commands = false;
    let arg_count = argv.len() + 1;

    let env_u32 = |name: &str, default: u32, dl: i32| -> u32 {
        if let Ok(v) = std::env::var(name) {
            let b = v.as_bytes();
            if !b.is_empty() && b[0].is_ascii_digit() {
                let mut p = 0;
                return read_u32(b, &mut p);
            }
            if dl >= 2 {
                eprint!("Ignore environment variable setting {name}={v}: not a valid unsigned value \n");
            }
        }
        default
    };
    let mut c_level = env_u32("LZ4_CLEVEL", CLEVEL_DEFAULT as u32, dl) as i32;
    let mut nb_workers = env_u32("LZ4_NBWORKERS", NBWORKERS_DEFAULT, dl);
    let mut legacy_format = false;
    let mut force_stdout = false;
    let mut force_overwrite = false;
    let mut multiple_inputs = false;
    let mut all_files = false;
    let mut mode = Mode::Auto;
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut dictionary: Option<String> = None;
    let mut in_names: Vec<String> = vec![STDINMARK.to_string(); arg_count];
    let mut ifn = 0usize;
    let mut recursive = false;
    let mut io = IoPrefs {
        pass_through: false,
        overwrite: true,
        test_mode: false,
        block_size_id: 7,
        block_size: 0,
        block_checksum: false,
        stream_checksum: true,
        block_independence: true,
        sparse: 1,
        content_size: false,
        dictionary: None,
        favor_dec_speed: false,
        remove_src: false,
    };
    let mut block_size = io.set_block_size_id(BLOCKSIZEID_DEFAULT);
    io.overwrite = false;

    macro_rules! show {
        ($l:expr, $($arg:tt)*) => {
            if dl >= $l { eprint!($($arg)*); }
        };
    }
    macro_rules! badusage {
        () => {{
            show!(1, "Incorrect parameters\n");
            if dl >= 1 {
                usage(&exe);
            }
            return Ok(1);
        }};
    }
    macro_rules! error_out {
        ($m:expr) => {{
            show!(1, "{} \n", $m);
            return Ok(1);
        }};
    }

    if exe_name_match(&exe, "lz4cat") {
        mode = Mode::Decompress;
        io.overwrite = true;
        io.pass_through = true;
        io.remove_src = false;
        force_stdout = true;
        output = Some(STDOUTMARK.into());
        dl = 1;
        multiple_inputs = true;
    }
    if exe_name_match(&exe, "unlz4") {
        mode = Mode::Decompress;
    }
    if exe_name_match(&exe, "lz4c") {
        legacy_commands = true;
    }

    let mut an = 0usize;
    while an < argv.len() {
        let argument = argv[an].clone();
        let a = argument.as_bytes();
        if !all_files && a.first() == Some(&b'-') {
            if a.len() == 1 {
                if input.is_none() {
                    input = Some(STDINMARK.into());
                } else {
                    output = Some(STDOUTMARK.into());
                }
                an += 1;
                continue;
            }
            if a[1] == b'-' {
                let mut handled = true;
                match argument.as_str() {
                    "--" => all_files = true,
                    "--compress" => mode = Mode::Compress,
                    "--decompress" | "--uncompress" => {
                        if mode != Mode::Bench {
                            mode = Mode::Decompress;
                        }
                    }
                    "--multiple" => multiple_inputs = true,
                    "--test" => mode = Mode::Test,
                    "--force" => io.overwrite = true,
                    "--no-force" => io.overwrite = false,
                    "--stdout" | "--to-stdout" => {
                        force_stdout = true;
                        output = Some(STDOUTMARK.into());
                    }
                    "--frame-crc" => io.stream_checksum = true,
                    "--no-frame-crc" => io.stream_checksum = false,
                    "--no-crc" => {
                        io.stream_checksum = false;
                        io.block_checksum = false;
                    }
                    "--content-size" => io.content_size = true,
                    "--no-content-size" => io.content_size = false,
                    "--list" => {
                        mode = Mode::List;
                        multiple_inputs = true;
                    }
                    "--sparse" => io.sparse = 2,
                    "--no-sparse" => io.sparse = 0,
                    "--favor-decSpeed" => io.favor_dec_speed = true,
                    "--verbose" => dl += 1,
                    "--quiet" => {
                        if dl > 0 {
                            dl -= 1;
                        }
                    }
                    "--version" => {
                        print!("{}", welcome());
                        return Ok(0);
                    }
                    "--help" => {
                        usage_advanced(&exe, legacy_commands);
                        return Ok(0);
                    }
                    "--keep" => io.remove_src = false,
                    "--rm" => io.remove_src = true,
                    "--best" => c_level = hc::CLEVEL_MAX,
                    _ => handled = false,
                }
                if handled {
                    an += 1;
                    continue;
                }
                if let Some(rest) = argument.strip_prefix("--threads") {
                    // NEXT_FIELD
                    let field: String;
                    if let Some(v) = rest.strip_prefix('=') {
                        field = v.to_string();
                    } else {
                        an += 1;
                        if an >= argv.len() {
                            show!(1, "error: missing command argument \n");
                            return Ok(1);
                        }
                        field = argv[an].clone();
                        if field.starts_with('-') {
                            show!(1, "error: command cannot be separated from its argument by another command \n");
                            return Ok(1);
                        }
                    }
                    let mut p = 0;
                    let v = read_u32(field.as_bytes(), &mut p);
                    if p != field.len() {
                        error_out!("error: only numeric values with optional suffixes K, KB, KiB, M, MB, MiB are allowed");
                    }
                    nb_workers = v;
                    an += 1;
                    continue;
                }
                if let Some(rest) = argument.strip_prefix("--fast") {
                    let r = rest.as_bytes();
                    if r.first() == Some(&b'=') {
                        let mut p = 1;
                        let fast = read_u32(r, &mut p);
                        if fast != 0 {
                            c_level = -(fast as i32);
                        } else {
                            badusage!();
                        }
                    } else if !r.is_empty() {
                        badusage!();
                    } else {
                        c_level = -1;
                    }
                    an += 1;
                    continue;
                }
                // an unknown long option goes on as aggregated short ones (from its second '-')
            }

            let mut p = 1usize;
            while p < a.len() {
                if legacy_commands {
                    let rest = &argument[p..];
                    match rest {
                        "c0" => {
                            c_level = 0;
                            p += 2;
                            continue;
                        }
                        "c1" => {
                            c_level = 9;
                            p += 2;
                            continue;
                        }
                        "c2" | "hc" => {
                            c_level = 12;
                            p += 2;
                            continue;
                        }
                        "y" => {
                            io.overwrite = true;
                            p += 1;
                            continue;
                        }
                        _ => {}
                    }
                }
                let c = a[p];
                if c.is_ascii_digit() {
                    c_level = read_u32(a, &mut p) as i32;
                    continue;
                }
                match c {
                    b'V' => {
                        print!("{}", welcome());
                        return Ok(0);
                    }
                    b'h' => {
                        usage_advanced(&exe, legacy_commands);
                        return Ok(0);
                    }
                    b'H' => {
                        usage_longhelp(&exe, legacy_commands);
                        return Ok(0);
                    }
                    b'e' => {
                        p += 1;
                        read_u32(a, &mut p);
                        continue;
                    }
                    b'z' => mode = Mode::Compress,
                    b'T' => {
                        p += 1;
                        nb_workers = read_u32(a, &mut p);
                        continue;
                    }
                    b'D' => {
                        if p + 1 == a.len() {
                            if an + 1 == argv.len() {
                                badusage!();
                            }
                            an += 1;
                            dictionary = Some(argv[an].clone());
                        } else {
                            dictionary = Some(argument[p + 1..].to_string());
                        }
                        p = a.len();
                        continue;
                    }
                    b'l' => {
                        legacy_format = true;
                        block_size = LEGACY_BLOCKSIZE;
                    }
                    b'd' => {
                        if mode != Mode::Bench {
                            mode = Mode::Decompress;
                        }
                    }
                    b'c' => {
                        force_stdout = true;
                        output = Some(STDOUTMARK.into());
                        io.pass_through = true;
                    }
                    b't' => mode = Mode::Test,
                    b'f' => {
                        force_overwrite = true;
                        io.overwrite = true;
                    }
                    b'v' => dl += 1,
                    b'q' => {
                        if dl > 0 {
                            dl -= 1;
                        }
                    }
                    b'k' => io.remove_src = false,
                    b'B' => {
                        while p + 1 < a.len() {
                            match a[p + 1] {
                                b'D' => {
                                    io.block_independence = false;
                                    p += 1;
                                }
                                b'I' => {
                                    io.block_independence = true;
                                    p += 1;
                                }
                                b'X' => {
                                    io.block_checksum = true;
                                    p += 1;
                                }
                                x if x.is_ascii_digit() => {
                                    p += 1;
                                    let b = read_u32(a, &mut p);
                                    p -= 1;
                                    if b < 4 {
                                        badusage!();
                                    }
                                    if b <= 7 {
                                        block_size = io.set_block_size_id(b);
                                        show!(2, "using blocks of size {} KB \n", (block_size >> 10) as u32);
                                    } else {
                                        if b < 32 {
                                            badusage!();
                                        }
                                        block_size = io.set_block_size(b as usize);
                                        if block_size >= 1024 {
                                            show!(2, "using blocks of size {} KB \n", (block_size >> 10) as u32);
                                        } else {
                                            show!(2, "using blocks of size {} bytes \n", block_size as u32);
                                        }
                                    }
                                }
                                _ => break,
                            }
                        }
                    }
                    b'b' => {
                        mode = Mode::Bench;
                        multiple_inputs = true;
                    }
                    b'S' => {}
                    b'r' => {
                        recursive = true;
                        multiple_inputs = true;
                    }
                    b'm' => multiple_inputs = true,
                    b'i' => {
                        p += 1;
                        read_u32(a, &mut p);
                        continue;
                    }
                    b'p' => {}
                    _ => badusage!(),
                }
                p += 1;
            }
            an += 1;
            continue;
        }

        if multiple_inputs {
            in_names[ifn] = argument;
            ifn += 1;
            an += 1;
            continue;
        }
        if input.is_none() {
            input = Some(argument);
            an += 1;
            continue;
        }
        if output.is_none() {
            output = Some(if argument == NULL_OUTPUT { NULMARK.to_string() } else { argument });
            an += 1;
            continue;
        }
        show!(1, "{} : {} won't be used ! Do you want multiple input files (-m) ? \n", if force_overwrite { "Warning" } else { "Error" }, argument);
        if !force_overwrite {
            return Ok(1);
        }
        an += 1;
    }

    show!(3, "{}", welcome());
    show!(4, "_POSIX_C_SOURCE defined: 200809L\n");
    show!(4, "_POSIX_VERSION defined: 200809L\n");
    show!(4, "PLATFORM_POSIX_VERSION defined: 200809L\n");
    if nb_workers > 1 {
        show!(2, "warning: this executable doesn't support multithreading \n");
    }
    if mode == Mode::Compress || mode == Mode::Bench {
        show!(4, "Blocks size : {} KB\n", (block_size >> 10) as u32);
    }

    let mut files: Vec<String> = in_names[..ifn].to_vec();
    if multiple_inputs {
        input = Some(in_names[0].clone());
        if recursive {
            if let Some(list) = create_file_list(&files) {
                for (u, f) in list.iter().enumerate() {
                    show!(4, "{} {}\n", u, f);
                }
                files = list;
                ifn = files.len();
            }
        }
    }

    if let Some(dict) = &dictionary {
        if is_stdin(dict) && stdin_is_tty() {
            show!(1, "refusing to read from a console\n");
            return Ok(1);
        }
        io.dictionary = Some(dict.clone());
    }

    if mode == Mode::Bench {
        show!(1, "lz4: benchmark mode not available in this build \n");
        return Ok(1);
    }
    if mode == Mode::Test {
        io.test_mode = true;
        output = Some(NULMARK.into());
        mode = Mode::Decompress;
    }

    let mut input_name = input.unwrap_or_else(|| STDINMARK.to_string());
    if is_stdin(&input_name) && stdin_is_tty() {
        show!(1, "refusing to read from a console\n");
        return Ok(1);
    }
    if is_stdin(&input_name) && output.is_none() {
        output = Some(STDOUTMARK.into());
    }

    if output.is_none() && !multiple_inputs {
        if mode == Mode::Auto {
            mode = determine_op_mode(&input_name);
        }
        if mode == Mode::Compress {
            let o = format!("{input_name}{EXT}");
            show!(2, "Compressed filename will be : {} \n", o);
            output = Some(o);
        }
        if mode == Mode::Decompress {
            let l = input_name.len();
            if l <= 4 || !input_name.ends_with(EXT) {
                show!(1, "Cannot determine an output filename \n");
                badusage!();
            }
            let o = input_name[..l - 4].to_string();
            show!(2, "Decoding file {} \n", o);
            output = Some(o);
        }
    }

    if mode == Mode::List && !multiple_inputs {
        files = vec![input_name.clone()];
        ifn = 1;
    }
    let output_name = output.unwrap_or_else(|| "*\\dummy^!//".to_string());

    if is_stdout(&output_name) && mode != Mode::List && stdout_is_tty() && !force_stdout {
        show!(1, "refusing to write to console without -c \n");
        return Ok(1);
    }
    if is_stdout(&output_name) && dl == 2 {
        dl = 1;
    }
    if multiple_inputs && dl == 2 {
        dl = 1;
    }
    if mode == Mode::Auto {
        mode = determine_op_mode(&input_name);
    }

    let mut d = Disp { level: dl, last_update: None };
    if ifn == 0 {
        multiple_inputs = false;
    }
    let files = &files[..ifn.min(files.len())];
    let r = if mode == Mode::Decompress {
        if multiple_inputs {
            let suffix = if is_stdout(&output_name) {
                STDOUTMARK
            } else if is_dev_null(&output_name) {
                NULMARK
            } else {
                EXT
            };
            decompress_multiple(&mut d, files, suffix, &io)?
        } else {
            decompress_filename(&mut d, &input_name, &output_name, &io)?
        }
    } else if mode == Mode::List {
        display_files_info(&d, files)?
    } else if legacy_format {
        d.show(3, "! Generating LZ4 Legacy format (deprecated) ! \n");
        if multiple_inputs {
            let suffix = if is_stdout(&output_name) { STDOUTMARK } else { EXT };
            compress_legacy_multiple(&mut d, files, suffix, c_level, &io)?
        } else {
            compress_legacy(&mut d, &input_name, &output_name, c_level, &io)?
        }
    } else if multiple_inputs {
        let suffix = if is_stdout(&output_name) { STDOUTMARK } else { EXT };
        compress_multiple(&mut d, files, suffix, c_level, &io)?
    } else {
        input_name = input_name.clone();
        compress_filename(&mut d, &input_name, &output_name, c_level, &io)?
    };
    let _ = io::stdout().flush();
    Ok(r)
}

/// For the check of verify.rs: decodes .lz4 data as `lz4 -d` does (frames, skippable frames,
/// legacy streams, one after another) into `out`, as it arrives.
pub fn decode_all<R: io::Read, W: io::Write>(mut input: R, out: &mut W, dict: Vec<u8>) -> io::Result<()> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut buf: Vec<u8> = vec![];
    let mut eof = false;
    let mut chunk = vec![0u8; 64 << 10];
    let mut fill = |buf: &mut Vec<u8>, need: usize, eof: &mut bool| -> io::Result<()> {
        while buf.len() < need && !*eof {
            let n = input.read(&mut chunk)?;
            if n == 0 {
                *eof = true;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok(())
    };
    loop {
        fill(&mut buf, 4, &mut eof)?;
        if buf.is_empty() {
            return Ok(());
        }
        if buf.len() < 4 {
            return Err(bad("truncated magic number"));
        }
        let magic = u32::from_le_bytes(buf[..4].try_into().unwrap());
        if magic & frame::SKIPPABLE_MASK == frame::SKIPPABLE0 {
            fill(&mut buf, 8, &mut eof)?;
            if buf.len() < 8 {
                return Err(bad("truncated skippable frame"));
            }
            let mut skip = 8 + u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            while skip > 0 {
                fill(&mut buf, 1, &mut eof)?;
                if buf.is_empty() {
                    break;
                }
                let n = skip.min(buf.len());
                buf.drain(..n);
                skip -= n;
            }
            continue;
        }
        if magic == LEGACY_MAGIC {
            buf.drain(..4);
            loop {
                fill(&mut buf, 4, &mut eof)?;
                if buf.is_empty() {
                    break;
                }
                if buf.len() < 4 {
                    return Err(bad("truncated legacy block size"));
                }
                let bs = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
                if bs > block::compress_bound(LEGACY_BLOCKSIZE) {
                    break;
                }
                fill(&mut buf, 4 + bs, &mut eof)?;
                if buf.len() < 4 + bs {
                    return Err(bad("truncated legacy block"));
                }
                let mut o = vec![];
                block::decompress(&buf[4..4 + bs], &[], LEGACY_BLOCKSIZE, &mut o).map_err(|_| bad("corrupted legacy block"))?;
                out.write_all(&o)?;
                buf.drain(..4 + bs);
            }
            continue;
        }
        let mut dctx = DCtx::new(dict.clone());
        loop {
            if buf.is_empty() {
                fill(&mut buf, 1, &mut eof)?;
                if buf.is_empty() {
                    return Err(bad("unfinished frame"));
                }
            }
            let mut o = vec![];
            let (used, hint) = dctx.decompress(&mut o, usize::MAX / 2, &buf, false).map_err(|e: FrameError| bad(e.name()))?;
            out.write_all(&o)?;
            buf.drain(..used);
            if hint == 0 {
                break;
            }
            if used == 0 {
                let need = buf.len() + 1;
                fill(&mut buf, need, &mut eof)?;
                if buf.len() < need {
                    return Err(bad("unfinished frame"));
                }
            }
        }
    }
}

