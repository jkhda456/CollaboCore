//! tracerx: `trace git-lfs: ...` lines (with a time prefix) when GIT_TRACE asks for them:
//! 1, 2 or true → stderr; another number → that file descriptor; an absolute path → that file.
//! GIT_TRANSFER_TRACE or GIT_CURL_VERBOSE stand in when GIT_TRACE is unset (lfs.go's init).

use std::io::Write;
use std::sync::{Mutex, OnceLock};

enum Out {
    Off,
    Stderr,
    File(std::fs::File),
}

fn tracer() -> &'static Mutex<Out> {
    static T: OnceLock<Mutex<Out>> = OnceLock::new();
    T.get_or_init(|| {
        let mut v = std::env::var("GIT_TRACE").unwrap_or_default();
        if v.is_empty() {
            v = std::env::var("GIT_TRANSFER_TRACE").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("GIT_CURL_VERBOSE").ok()).unwrap_or_default();
        }
        Mutex::new(match v.parse::<i64>() {
            Ok(0) => Out::Off,
            Ok(1) | Ok(2) => Out::Stderr,
            Ok(fd) => {
                use std::os::fd::FromRawFd;
                Out::File(unsafe { std::fs::File::from_raw_fd(fd as i32) })
            }
            Err(_) if v.starts_with('/') => match std::fs::OpenOptions::new().append(true).create(true).open(&v) {
                Ok(f) => Out::File(f),
                Err(e) => {
                    eprintln!("Could not open '{v}' for tracing: {e}\nDefaulting to tracing on stderr...");
                    Out::Stderr
                }
            },
            Err(_) if v.eq_ignore_ascii_case("true") => Out::Stderr,
            Err(_) => Out::Off,
        })
    })
}

pub fn enabled() -> bool {
    !matches!(*tracer().lock().unwrap(), Out::Off)
}

pub fn now_hms_micros() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:06}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_micros())
}

pub fn print(msg: &str) {
    let mut t = tracer().lock().unwrap();
    let line = format!("{} trace git-lfs: {}\n", now_hms_micros(), msg);
    match &mut *t {
        Out::Off => {}
        Out::Stderr => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        Out::File(f) => {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! trace {
    ($($a:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::print(&format!($($a)*));
        }
    };
}
