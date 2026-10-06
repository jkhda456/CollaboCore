//! The few system calls std does not wrap: poll, a SIGCHLD self-pipe, signals to process groups,
//! flock, the terminal's width.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// The guest has no fork. std starts programs with posix_spawn (no pre_exec, a process group,
/// a working directory) and names fork only in fallbacks this program never takes; this stub
/// completes the link and fails if one ever is taken.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
extern "C" fn fork() -> libc::pid_t {
    // SAFETY: errno is this thread's own.
    unsafe { *libc::__errno_location() = libc::ENOSYS };
    -1
}

pub const IN: i16 = libc::POLLIN;
pub const OUT: i16 = libc::POLLOUT;

/// Waits on the descriptors; `timeout_ms` < 0 waits for ever. Returns the revents, in order.
pub fn poll(fds: &[(RawFd, i16)], timeout_ms: i32) -> io::Result<Vec<i16>> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect();
    loop {
        // SAFETY: p is a valid array of p.len() pollfds.
        let n = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
        if n >= 0 {
            return Ok(p.iter().map(|x| x.revents).collect());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        if timeout_ms >= 0 {
            // A signal (SIGCHLD) cut the wait short; the caller's loop works out what is due.
            return Ok(vec![0; p.len()]);
        }
    }
}

static SIGCHLD_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_sigchld(_: libc::c_int) {
    let fd = SIGCHLD_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write(2) is async-signal-safe; a full pipe already says "a child changed".
        unsafe { libc::write(fd, b"c".as_ptr() as *const libc::c_void, 1) };
    }
}

/// A pipe that becomes readable when a child exits. Returns its read end.
pub fn sigchld_pipe() -> io::Result<RawFd> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds has room for the two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    SIGCHLD_PIPE.store(fds[1], Ordering::Relaxed);
    // SAFETY: a plain handler; SA_RESTART keeps other calls from failing with EINTR.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigchld as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut());
    }
    Ok(fds[0])
}

pub fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: buf is writable for its length.
    while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {}
}

/// Writes to a closed socket are errors, not a death.
pub fn ignore_sigpipe() {
    // SAFETY: setting a disposition.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
}

/// Sends `signal` to the process group `pgid` (the program and whatever it started).
pub fn kill_group(pgid: u32, signal: i32) -> bool {
    // SAFETY: kill(2) with a negative pid names the group.
    unsafe { libc::kill(-(pgid as libc::pid_t), signal) == 0 || libc::kill(pgid as libc::pid_t, signal) == 0 }
}

pub fn signal_number(name: &str) -> Option<i32> {
    let n = name.trim_start_matches("SIG").to_ascii_uppercase();
    Some(match n.as_str() {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        _ => return n.parse().ok().filter(|v| (1..65).contains(v)),
    })
}

/// A pipe for a program's stdout and stderr: our non-blocking read end, and two write ends for
/// the program (a dup made here: the guest's std cannot duplicate a descriptor).
pub fn output_pipe() -> io::Result<(std::fs::File, std::process::Stdio, std::process::Stdio)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds has room for the two descriptors; each is owned exactly once below.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(io::Error::last_os_error());
        }
        let second = libc::fcntl(fds[1], libc::F_DUPFD_CLOEXEC, 3);
        if second < 0 {
            let e = io::Error::last_os_error();
            libc::close(fds[0]);
            libc::close(fds[1]);
            return Err(e);
        }
        let flags = libc::fcntl(fds[0], libc::F_GETFL);
        libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
        Ok((
            std::fs::File::from_raw_fd(fds[0]),
            std::process::Stdio::from(std::os::fd::OwnedFd::from_raw_fd(fds[1])),
            std::process::Stdio::from(std::os::fd::OwnedFd::from_raw_fd(second)),
        ))
    }
}

/// An exclusive, non-blocking lock on the file, held for as long as the file stays open.
pub fn try_lock(file: &std::fs::File) -> io::Result<bool> {
    // SAFETY: flock on an open descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// A new session for the server, so the terminal (or the command) that started it can go away
/// without taking it along. Fails if we lead a process group already (we are started without one).
pub fn setsid() -> bool {
    // SAFETY: no arguments.
    unsafe { libc::setsid() >= 0 }
}

/// The terminal's size (columns, rows), if stdout is one.
pub fn terminal_size() -> Option<(u16, u16)> {
    // SAFETY: ws is written by the ioctl.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}
