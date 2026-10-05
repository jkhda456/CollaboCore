//! The command line's logs: the network log (`--log-requests`, `--log-file`) and the command log
//! (`--log-commands`, `--command-log`), one line per event, to stderr or appended to a file.
//!
//! What goes into a line comes from the guest (URLs, argv, host names), so every line is made
//! safe here: control characters are escaped (a guest cannot start a line of its own or move the
//! terminal's cursor), and a line longer than `LINE_LIMIT` is cut, saying how much was left out.
//! A file can have a size limit (`--log-max-size`); when it is reached the file is either kept and
//! later lines are dropped, or rotated to FILE.1 … FILE.N (`--log-rotate N`).
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

/// Bytes of a line (after escaping, before its time) that are kept.
pub const LINE_LIMIT: usize = 4096;

/// Where lines go: one line each, from any thread.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// How large a log file may grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// No limit when `None`.
    pub max_size: Option<u64>,
    /// Old files kept when the limit is reached (FILE.1 is the newest); 0 keeps the file as it is
    /// and drops the lines that do not fit.
    pub rotate: u32,
}

/// Room kept for the ` …[+N bytes]` that ends a cut line.
const MARKER_ROOM: usize = 40;

/// `line` made safe for one line of a log: control characters (and the ones that reorder text)
/// escaped, and at most `LINE_LIMIT` bytes, a cut line ending with ` …[+N bytes]` saying how much
/// was left out. Already clean lines come back as they are.
pub fn clean(line: &str) -> String {
    clean_cut(line, 0)
}

/// `clean`, for a line whose caller already left `missing` bytes out of its end: the marker says
/// so, and counts them with whatever is cut here.
pub fn clean_cut(line: &str, missing: usize) -> String {
    let room = LINE_LIMIT - MARKER_ROOM;
    let mut out = String::with_capacity(line.len().min(LINE_LIMIT));
    // Where to cut if it comes to that: the escaped length and the source byte.
    let mut cut = None;
    for (at, c) in line.char_indices() {
        let before = out.len();
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
            // C1 controls, and the bidirectional overrides and isolates (U+202A–202E, 2066–2069).
            '\u{80}'..='\u{9f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32))
            }
            c => out.push(c),
        }
        if out.len() > room && cut.is_none() {
            cut = Some((before, at));
        }
        if out.len() > LINE_LIMIT {
            break;
        }
    }
    let fits = out.len() <= if missing > 0 { room } else { LINE_LIMIT };
    match cut {
        Some((length, at)) if !fits => {
            out.truncate(length);
            out.push_str(&format!(" …[+{} bytes]", line.len() - at + missing));
        }
        _ if missing > 0 => out.push_str(&format!(" …[+{missing} bytes]")),
        _ => {}
    }
    out
}

/// `text` cut to its first `max` bytes (at a character), saying how many it had more: for a part
/// of a line that something must follow, such as a URL before its status.
pub fn shorten(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max {
        return text.into();
    }
    let at = (0..=max).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0);
    format!("{} …[+{} bytes]", &text[..at], text.len() - at).into()
}

/// One argument as a shell would need it typed: bare when that is unambiguous, else in single
/// quotes (a `'` as `'\''`).
pub fn quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b) || b >= 0x80);
    match plain {
        true => argument.to_string(),
        false => format!("'{}'", argument.replace('\'', "'\\''")),
    }
}

/// argv as one line of shell words.
pub fn quote_all<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|argument| quote(argument.as_ref())).collect::<Vec<_>>().join(" ")
}

/// A size: bytes, or a number with K, M or G (KiB, MiB, GiB; `10M`, `512k`).
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last() {
        Some((at, c)) if c.is_ascii_alphabetic() => (&text[..at], c.to_ascii_uppercase()),
        _ => (text, 'B'),
    };
    let shift = match unit {
        'B' => 0,
        'K' => 10,
        'M' => 20,
        'G' => 30,
        _ => bail!("{text}: a size is bytes, or a number with K, M or G"),
    };
    let number: u64 = number.trim().parse().with_context(|| format!("{text}: a size is bytes, or a number with K, M or G"))?;
    if number == 0 {
        bail!("{text}: a size limit must be more than 0");
    }
    number.checked_mul(1 << shift).with_context(|| format!("{text}: too large"))
}

/// A log on stderr, lines as they are.
pub fn to_stderr() -> Log {
    Arc::new(|line: &str| eprintln!("{}", clean(line)))
}

/// A log appending to `path` (made when missing, its folder too), each line after its UTC time.
/// A write that fails is dropped: the guest goes on.
pub fn to_file(path: &str, limits: Limits) -> Result<Log> {
    let path = PathBuf::from(path);
    if let Some(folder) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(folder).with_context(|| format!("making the folder of the log file {}", path.display()))?;
    }
    let file = open(&path).with_context(|| format!("opening the log file {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let file = Mutex::new(LogFile { path, file: Some(file), size, limits, full: false });
    Ok(Arc::new(move |line: &str| {
        let line = format!("{} {}\n", utc_now(), clean(line));
        if let Ok(mut file) = file.lock() {
            file.write(&line);
        }
    }))
}

fn open(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

struct LogFile {
    path: PathBuf,
    /// `None` while it is being rotated, or when it could not be opened again.
    file: Option<File>,
    size: u64,
    limits: Limits,
    /// At its limit without rotation: the note is written, later lines are dropped.
    full: bool,
}

impl LogFile {
    fn write(&mut self, line: &str) {
        if let Some(max) = self.limits.max_size {
            if self.limits.rotate == 0 {
                if self.full {
                    return;
                }
                // The lines leave room for the note that ends the file.
                let note = format!("{} [log] reached its size limit ({max} bytes): later lines are dropped\n", utc_now());
                if self.size + (line.len() + note.len()) as u64 <= max {
                    self.append(line);
                    return;
                }
                self.full = true;
                if self.size + note.len() as u64 <= max {
                    self.append(&note);
                }
                return;
            }
            if self.size + line.len() as u64 > max {
                // A line still goes in when a fresh file is too small for it.
                if self.size > 0 {
                    self.rotate();
                }
            }
        }
        self.append(line);
    }

    fn append(&mut self, text: &str) {
        if let Some(file) = self.file.as_mut() {
            if file.write_all(text.as_bytes()).is_ok() {
                self.size += text.len() as u64;
            }
        }
    }

    /// FILE.N-1 → FILE.N, …, FILE → FILE.1, and a new FILE. The file is closed first (Windows
    /// does not rename an open file). If FILE cannot be moved, it is emptied instead, so the limit
    /// holds.
    fn rotate(&mut self) {
        self.file = None;
        let numbered = |n: u32| {
            let mut name = self.path.clone().into_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        let _ = std::fs::remove_file(numbered(self.limits.rotate));
        for n in (1..self.limits.rotate).rev() {
            let _ = std::fs::rename(numbered(n), numbered(n + 1));
        }
        let moved = std::fs::rename(&self.path, numbered(1)).is_ok();
        self.file = match moved {
            true => open(&self.path).ok(),
            false => OpenOptions::new().create(true).write(true).truncate(true).open(&self.path).ok(),
        };
        self.size = 0;
    }
}

/// This moment as 2026-09-30T12:34:56Z.
fn utc_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_escaped_and_cut() {
        assert_eq!(clean("GET http://a/\n2026 [exec] forged\x1b[2J"), "GET http://a/\\n2026 [exec] forged\\x1b[2J");
        assert_eq!(clean("tab\there\r\u{7f}\u{85}\u{202e}"), "tab\\there\\r\\x7f\\u{85}\\u{202e}");
        assert_eq!(clean("한글 그대로"), "한글 그대로");
        let long = format!("[network] GET http://x/{}", "가".repeat(3000));
        let cut = clean(&long);
        assert!(cut.len() <= LINE_LIMIT, "{}", cut.len());
        let kept = cut.find(" …[+").unwrap();
        let left = |line: &str| -> usize { line[line.find(" …[+").unwrap() + " …[+".len()..].trim_end_matches(" bytes]").parse().unwrap() };
        assert_eq!(kept + left(&cut), long.len(), "what is kept and what is left out add up");
        assert_eq!(clean(&cut), cut, "a clean line stays as it is");
        // Escaping counts toward the limit: a line of control characters is cut too.
        assert!(clean(&"\x01".repeat(10_000)).len() <= LINE_LIMIT);
        // What the caller left out is counted in.
        assert_eq!(clean_cut("echo aaa", 4980), "echo aaa …[+4980 bytes]");
        let both = clean_cut(&long, 1000);
        assert!(both.len() <= LINE_LIMIT);
        assert_eq!(both.find(" …[+").unwrap() + left(&both), long.len() + 1000);
        let exact = "x".repeat(LINE_LIMIT);
        assert_eq!(clean(&exact), exact, "a line of exactly the limit is kept whole");
    }

    #[test]
    fn a_part_is_shortened_at_a_character() {
        assert_eq!(shorten("http://x/", 100), "http://x/");
        assert_eq!(shorten("http://x/가나", 11), "http://x/ …[+6 bytes]");
    }

    #[test]
    fn arguments_are_quoted_like_a_shell() {
        assert_eq!(quote_all(&["ls", "-la", "/work/a b", "", "it's", "x=1", "한글"]), "ls -la '/work/a b' '' 'it'\\''s' x=1 한글");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert_eq!(parse_size("10M").unwrap(), 10 << 20);
        assert_eq!(parse_size("512k").unwrap(), 512 << 10);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert!(parse_size("0").is_err());
        assert!(parse_size("10X").is_err());
        assert!(parse_size("ten").is_err());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("collabo-log-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path).unwrap_or_default().lines().map(|l| l[21..].to_string()).collect()
    }

    #[test]
    fn a_full_file_without_rotation_keeps_its_lines() {
        let dir = scratch("full");
        let path = dir.join("logs/net.log");
        let log = to_file(path.to_str().unwrap(), Limits { max_size: Some(200), rotate: 0 }).unwrap();
        for n in 0..20 {
            log(&format!("line {n:02} ............"));
        }
        let kept = lines(&path);
        assert_eq!(kept[0], "line 00 ............");
        assert!(kept.last().unwrap().starts_with("[log] reached its size limit"), "{kept:?}");
        assert!(std::fs::metadata(&path).unwrap().len() <= 200);
        // Opened again at its limit: still nothing more.
        let log = to_file(path.to_str().unwrap(), Limits { max_size: Some(200), rotate: 0 }).unwrap();
        log("more");
        assert_eq!(lines(&path), kept);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotation_keeps_n_old_files() {
        let dir = scratch("rotate");
        let path = dir.join("cmd.log");
        let log = to_file(path.to_str().unwrap(), Limits { max_size: Some(100), rotate: 2 }).unwrap();
        for n in 0..12 {
            log(&format!("line {n:02} xxxxxxxx"));
        }
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let (one, two, three) = (dir.join("cmd.log.1"), dir.join("cmd.log.2"), dir.join("cmd.log.3"));
        assert!(size(&path) <= 100 && size(&one) <= 100 && size(&two) <= 100);
        assert!(!three.exists());
        assert_eq!(lines(&path).last().unwrap(), "line 11 xxxxxxxx");
        let (newer, older) = (lines(&one), lines(&two));
        assert_eq!(newer.last().unwrap(), &format!("line {:02} xxxxxxxx", 11 - lines(&path).len()));
        assert!(older.last().unwrap() < newer.first().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unlimited_by_default() {
        let dir = scratch("unlimited");
        let path = dir.join("a.log");
        let log = to_file(path.to_str().unwrap(), Limits::default()).unwrap();
        for _ in 0..100 {
            log(&"x".repeat(1000));
        }
        assert_eq!(lines(&path).len(), 100);
        let _ = std::fs::remove_dir_all(dir);
    }
}
