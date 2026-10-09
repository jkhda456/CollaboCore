//! Helpers from git-lfs's tools package, and Go-style error texts.

use crate::errors::{Error, Result};
use std::path::{Path, PathBuf};

/// Go's syscall.Errno text (strerror, lower case as Go spells them).
pub fn errno_text(code: i32) -> String {
    let s = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }.to_string_lossy().into_owned();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => s,
    }
}

pub fn io_err(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => errno_text(code),
        None => e.to_string(),
    }
}

/// os.PathError's text: `op path: err`.
pub fn path_err(op: &str, path: impl AsRef<Path>, e: &std::io::Error) -> Error {
    Error::new(format!("{} {}: {}", op, path.as_ref().display(), io_err(e)))
}

pub fn indent(s: &str) -> String {
    let ind = s.replace('\n', "\n\t");
    if ind.is_empty() {
        ind
    } else {
        format!("\t{ind}")
    }
}

/// Removes leading spaces and tabs line-wise.
pub fn undent(s: &str) -> String {
    s.split('\n').map(|l| l.trim_start_matches([' ', '\t'])).collect::<Vec<_>>().join("\n")
}

pub fn clean_paths(paths: &str, delim: &str) -> Vec<String> {
    let paths = paths.trim();
    if paths.is_empty() {
        return vec![];
    }
    paths
        .split(delim)
        .map(|p| {
            let mut p = p.trim();
            for sep in ["/", "\\"] {
                if let Some(x) = p.strip_suffix(sep) {
                    p = x;
                    break;
                }
            }
            p.to_string()
        })
        .collect()
}

pub fn executable_permissions(perms: u32) -> u32 {
    perms | ((perms & 0o444) >> 2)
}

pub fn umask() -> u32 {
    unsafe {
        let m = libc::umask(0);
        libc::umask(m);
        m as u32
    }
}

/// os.MkdirAll with the repository's permissions (and that umask).
pub fn mkdir_all(path: impl AsRef<Path>, perms: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mode = executable_permissions(perms);
    unsafe {
        let old = libc::umask((0o777 & !mode) as libc::mode_t);
        let r = std::fs::DirBuilder::new().recursive(true).mode(mode).create(path);
        libc::umask(old);
        r
    }
}

pub fn file_exists(p: impl AsRef<Path>) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

pub fn dir_exists(p: impl AsRef<Path>) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn file_exists_of_size(p: impl AsRef<Path>, size: i64) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir() && m.len() as i64 == size).unwrap_or(false)
}

/// filepath.Abs (lexically cleaned against the current directory).
pub fn abs(p: impl AsRef<Path>) -> PathBuf {
    let p = p.as_ref();
    let joined = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    clean(&joined)
}

/// filepath.Clean.
pub fn clean(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(clean_str(&s))
}

/// filepath.Join: the non-empty elements joined and cleaned.
pub fn join(parts: &[&str]) -> String {
    let v: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if v.is_empty() {
        return String::new();
    }
    clean_str(&v.join("/"))
}

pub fn clean_str(s: &str) -> String {
    if s.is_empty() {
        return ".".into();
    }
    let rooted = s.starts_with('/');
    let mut out: Vec<&str> = vec![];
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().map_or(false, |l| *l != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            p => out.push(p),
        }
    }
    let j = out.join("/");
    match (rooted, j.is_empty()) {
        (true, _) => format!("/{j}"),
        (false, true) => ".".into(),
        (false, false) => j,
    }
}

/// CanonicalizeSystemPath: absolute with symlinks resolved.
pub fn canonicalize_system_path(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(abs(p))
}

pub fn resolve_symlinks(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    canonicalize_system_path(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.to_string())
}

/// CanonicalizePath: absolute, symlinks resolved; with `missing_ok`, a missing path is just
/// made absolute.
pub fn canonicalize_path(p: &str, missing_ok: bool) -> Result<String> {
    if p.is_empty() {
        return Ok(String::new());
    }
    let a = abs(p);
    match std::fs::canonicalize(&a) {
        Ok(r) => Ok(r.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && missing_ok => Ok(a.display().to_string()),
        Err(e) => Err(path_err("lstat", &a, &e)),
    }
}

/// ExpandPath: `~` and `~user` at the start.
pub fn expand_path(path: &str) -> Result<String> {
    if !path.starts_with('~') {
        return Ok(path.to_string());
    }
    let rest = &path[1..];
    let (user, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let home = if user.is_empty() {
        std::env::var("HOME").unwrap_or_default()
    } else {
        let c = std::ffi::CString::new(user).unwrap_or_default();
        let pw = unsafe { libc::getpwnam(c.as_ptr()) };
        if pw.is_null() {
            return Err(Error::new(format!("user: unknown user {user}")).wrap(format!("could not find user {user}")));
        }
        unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) }.to_string_lossy().into_owned()
    };
    Ok(format!("{home}{tail}"))
}

pub fn expand_config_path(path: &str, default_path: &str) -> Result<String> {
    if !path.is_empty() {
        return expand_path(path);
    }
    let cfg_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
    if !cfg_home.is_empty() {
        return Ok(format!("{cfg_home}/{default_path}"));
    }
    expand_path(&format!("~/.config/{default_path}"))
}

/// Go's strconv.Quote (for the %q verb): ASCII printable kept, the rest escaped.
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\x0b' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn format_fraction(x: i64, y: i64) -> String {
    if y >= 0 {
        format!("{x}/{y}")
    } else {
        format!("{x}/{}", "?".repeat(x.to_string().len()))
    }
}

/// tools.TrimCurrentPrefix.
pub fn trim_current_prefix(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

const SIZES: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// humanize.FormatBytes.
pub fn format_bytes(s: u64) -> String {
    let e = if s == 0 { 0.0 } else { ((s as f64).ln() / 1000f64.ln()).floor() };
    let unit = 1000f64.powf(e) as u64;
    format!("{} {}", format_bytes_unit(s, unit), SIZES[(e as usize).min(5)])
}

pub fn format_bytes_unit(s: u64, u: u64) -> String {
    let rounded = if s < 10 { s as f64 } else { ((s as f64) / (u as f64) * 10.0 + 0.5).floor() / 10.0 };
    if rounded < 10.0 && u > 1 {
        format!("{rounded:.1}")
    } else {
        format!("{rounded:.0}")
    }
}

/// humanize.FormatByteRate.
pub fn format_byte_rate(s: u64, secs: f64) -> String {
    let eps = 7.0f64 / 3.0 - 4.0 / 3.0 - 1.0;
    let mut f = s as f64;
    let mut e = 0.0;
    if f != 0.0 {
        f /= secs.max(1e-9);
        e = (f.ln() / 1000f64.ln()).floor();
        if e <= eps {
            e = 0.0;
        }
    }
    let unit = 1000f64.powf(e) as u64;
    format!("{} {}/s", format_bytes_unit(f.ceil() as u64, unit), SIZES[(e as usize).min(5)])
}

/// humanize.ParseBytes.
pub fn parse_bytes(s: &str) -> Result<u64> {
    let sep = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').count();
    let num = s[..sep].replace(',', "");
    let f: f64 = if num.is_empty() { 0.0 } else { num.parse().map_err(|_| Error::new(format!("strconv.ParseFloat: parsing {}: invalid syntax", quote(&num))))? };
    let unit = s[sep..].trim().to_lowercase();
    let m: u64 = match unit.as_str() {
        "" | "b" => 1,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        "pib" => 1 << 50,
        "kb" => 1000,
        "mb" => 1000_000,
        "gb" => 1000_000_000,
        "tb" => 1000_000_000_000,
        "pb" => 1000_000_000_000_000,
        _ => return Err(Error::new(format!("unknown unit: {}", quote(&unit)))),
    };
    Ok((f * m as f64) as u64)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(data))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month, day) of a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn unix_time(secs: i64, nanos: u32) -> std::time::SystemTime {
    if secs >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
    } else {
        std::time::UNIX_EPOCH - std::time::Duration::from_secs((-secs) as u64) + std::time::Duration::from_nanos(nanos as u64)
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// time.Parse(time.RFC1123, s): "Mon, 02 Jan 2006 15:04:05 MST" (the zone taken as UTC).
pub fn parse_rfc1123(s: &str) -> Option<std::time::SystemTime> {
    let re = regex::Regex::new(r"^[A-Z][a-z]{2}, (\d{2}) ([A-Z][a-z]{2}) (\d{4}) (\d{2}):(\d{2}):(\d{2}) [A-Za-z]+$").unwrap();
    let m = re.captures(s.trim())?;
    let mon = MONTHS.iter().position(|x| *x == &m[2])? as i64 + 1;
    let n = |i: usize| m[i].parse::<i64>().unwrap();
    let days = days_from_civil(n(3), mon, n(1));
    Some(unix_time(days * 86400 + n(4) * 3600 + n(5) * 60 + n(6), 0))
}

/// time.Parse(time.RFC3339, s).
pub fn parse_rfc3339(s: &str) -> Option<std::time::SystemTime> {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$").unwrap();
    let m = re.captures(s)?;
    let n = |i: usize| m[i].parse::<i64>().unwrap();
    let mut secs = days_from_civil(n(1), n(2), n(3)) * 86400 + n(4) * 3600 + n(5) * 60 + n(6);
    let nanos = m.get(7).map_or(0, |f| {
        let d = &f.as_str()[1..];
        let d9: String = d.chars().chain(std::iter::repeat('0')).take(9).collect();
        d9.parse::<u32>().unwrap_or(0)
    });
    let z = &m[8];
    if z.len() == 6 {
        let off = z[1..3].parse::<i64>().ok()? * 3600 + z[4..6].parse::<i64>().ok()? * 60;
        secs -= if z.starts_with('-') { -off } else { off };
    }
    Some(unix_time(secs, nanos))
}

/// Seconds since the epoch (negative before it).
pub fn unix_secs(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// time.Time's RFC3339 form in UTC ("0001-01-01T00:00:00Z" for None, Go's zero time).
pub fn format_rfc3339_utc(t: Option<std::time::SystemTime>) -> String {
    let Some(t) = t else { return "0001-01-01T00:00:00Z".into() };
    let s = unix_secs(t);
    let (y, mo, d) = civil_from_days(s.div_euclid(86400));
    let r = s.rem_euclid(86400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", r / 3600, r / 60 % 60, r % 60)
}

/// time.RFC822 ("02 Jan 06 15:04 MST") in local time.
pub fn format_rfc822_local(t: std::time::SystemTime) -> String {
    let secs = unix_secs(t) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let zone = if tm.tm_zone.is_null() { "UTC".to_string() } else { unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned() };
    format!("{:02} {} {:02} {:02}:{:02} {}", tm.tm_mday, MONTHS[tm.tm_mon as usize], tm.tm_year % 100, tm.tm_hour, tm.tm_min, zone)
}

static AT_EXIT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A temporary file to remove when the command ends (remove_exit_files).
pub fn remove_at_exit(p: &str) {
    AT_EXIT.lock().unwrap().push(p.to_string());
}

pub fn remove_exit_files() {
    for p in AT_EXIT.lock().unwrap().drain(..) {
        let _ = std::fs::remove_file(p);
    }
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The sha256 (hex) of what a reader yields.
pub fn sha256_reader(r: &mut dyn std::io::Read) -> std::io::Result<String> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}
