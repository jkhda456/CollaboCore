//! What the gzip-like tools (xz, zstd) share: the program name in messages, opening a source
//! the way they check it, creating the destination exclusively, giving it the source's mode,
//! owner and times, and removing a half-written destination when a signal ends the run.

use std::ffi::CString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Mutex;

/// The name messages start with (argv[0]'s base name).
pub static PROG: Mutex<String> = Mutex::new(String::new());

pub fn prog() -> String {
    PROG.lock().unwrap().clone()
}

/// strerror's text for an io::Error, without Rust's " (os error N)".
pub fn errstr(e: &io::Error) -> String {
    if let Some(code) = e.raw_os_error() {
        let s = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) };
        return s.to_string_lossy().into_owned();
    }
    e.to_string()
}

/// Names shown in messages: control characters become '?', as tuklib_mask_nonprint does.
pub fn mask(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    io::stderr().is_terminal()
}

// The destination being written, removed by the signal handler. A C string, so the handler
// calls only unlink and _exit.
static mut PENDING: [u8; 4096] = [0; 4096];
static mut PENDING_SET: bool = false;

extern "C" fn on_signal(sig: libc::c_int) {
    unsafe {
        if PENDING_SET {
            libc::unlink(std::ptr::addr_of!(PENDING) as *const libc::c_char);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
        libc::_exit(128 + sig);
    }
}

/// Removes the destination named by `pending_output` on SIGINT, SIGTERM, SIGHUP and SIGPIPE.
pub fn install_signal_cleanup() {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, on_signal as *const () as libc::sighandler_t);
        }
    }
}

pub fn pending_output(path: Option<&Path>) {
    unsafe {
        PENDING_SET = false;
        if let Some(p) = path {
            let bytes = p.as_os_str().as_bytes();
            if bytes.len() < 4095 {
                let buf = &mut *std::ptr::addr_of_mut!(PENDING);
                buf[..bytes.len()].copy_from_slice(bytes);
                buf[bytes.len()] = 0;
                PENDING_SET = true;
            }
        }
    }
}

/// Creates the destination: exclusively, or replacing a file that is there when `force`.
/// Its mode starts as 0600 until the source's is copied over.
pub fn create_dest(path: &Path, force: bool) -> io::Result<File> {
    let open = || OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path);
    match open() {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && force => {
            std::fs::remove_file(path)?;
            open()
        }
        r => r,
    }
}

/// Gives `dest` the source's owner, group, mode (without setuid, setgid and sticky) and times,
/// as xz does: when the group cannot be set, the group's and others' permissions are cut to
/// what both had. Returns the warnings (the data is fine either way).
pub fn copy_metadata(dest: &File, meta: &Metadata) -> Vec<String> {
    use std::os::fd::AsRawFd;
    let fd = dest.as_raw_fd();
    let mut warnings = vec![];
    let src_mode = meta.mode();
    unsafe {
        // Only root can give a file away; others keep their own uid silently.
        if libc::fchown(fd, meta.uid(), u32::MAX) != 0 && libc::geteuid() == 0 {
            warnings.push(format!("Cannot set the file owner: {}", errstr(&io::Error::last_os_error())));
        }
        let mode = if libc::fchown(fd, u32::MAX, meta.gid()) != 0 {
            warnings.push(format!("Cannot set the file group: {}", errstr(&io::Error::last_os_error())));
            let both = ((src_mode & 0o070) >> 3) & (src_mode & 0o007);
            (src_mode & 0o700) | (both << 3) | both
        } else {
            src_mode & 0o777
        };
        if libc::fchmod(fd, mode as libc::mode_t) != 0 {
            warnings.push(format!("Cannot set the file permissions: {}", errstr(&io::Error::last_os_error())));
        }
        let times = [timespec(meta.atime(), meta.atime_nsec()), timespec(meta.mtime(), meta.mtime_nsec())];
        libc::futimens(fd, times.as_ptr());
    }
    warnings
}

/// A timespec (the guest's ILP32 libc has padding fields, so no struct literal).
pub fn timespec(sec: i64, nsec: i64) -> libc::timespec {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    t.tv_sec = sec as _;
    t.tv_nsec = nsec as _;
    t
}

/// lstat or stat, as the tools look at their sources.
pub fn source_meta(path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
}

pub fn unlink(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if unsafe { libc::unlink(c.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A reader that remembers whether an error came from the file itself, so a codec's error can
/// be told from a read error.
pub struct SrcReader<R> {
    pub inner: R,
    pub read_error: Option<io::Error>,
    pub count: u64,
}

impl<R: Read> SrcReader<R> {
    pub fn new(inner: R) -> Self {
        SrcReader { inner, read_error: None, count: 0 }
    }
}

impl<R: Read> Read for SrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => {
                    self.count += n as u64;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let copy = io::Error::new(e.kind(), errstr(&e));
                    self.read_error = Some(e);
                    return Err(copy);
                }
            }
        }
    }
}

/// The writer counterpart: remembers write errors and counts bytes.
pub struct DestWriter<W> {
    pub inner: W,
    pub write_error: Option<io::Error>,
    pub count: u64,
}

impl<W: Write> DestWriter<W> {
    pub fn new(inner: W) -> Self {
        DestWriter { inner, write_error: None, count: 0 }
    }
}

impl<W: Write> Write for DestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.write(buf) {
            Ok(n) => {
                self.count += n as u64;
                Ok(n)
            }
            Err(e) => {
                let copy = io::Error::new(e.kind(), errstr(&e));
                self.write_error = Some(e);
                Err(copy)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|e| {
            if self.write_error.is_none() {
                self.write_error = Some(io::Error::new(e.kind(), errstr(e)));
            }
        })
    }
}

/// A sink for test modes: counts what would be written.
pub struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads up to `n` bytes without consuming them past what the caller keeps: returns them so
/// they can be chained in front of the rest.
pub fn peek<R: Read>(r: &mut R, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(got);
    Ok(buf)
}
