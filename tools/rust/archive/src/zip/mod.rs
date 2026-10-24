//! zip: Info-ZIP's Zip 3.0 command line, written again (not an Info-ZIP release; its help and
//! license texts are Info-ZIP's, LICENSE-infozip). The modes (add, -u, -f, -FS, -d), the file
//! selection (-r, -R, -x, -i, -@, -t, -tt, -D, -y), the messages and exit codes, and the archive's
//! structure follow zip 3.0 as Unix builds it: version made by 3.0/Unix, the UT and ux extra
//! fields, the text bit, data descriptors when the output cannot be rewound (and for encrypted
//! entries), Zip64 for standard input and for what does not fit 32 bits, traditional encryption
//! (-e, -P), names in UTF-8 marked as such, an existing archive's entries copied as they are.
//!
//! The deflate data is zlib's (flate2), not Info-ZIP's own deflate, so its bytes and the ratio
//! shown can differ a little; whether a small file is kept stored follows zip's rule (all of it
//! in deflate's first block and deflating it gains nothing). The options for other systems,
//! splitting, logging and fixing archives (-s, -F, -FF, -A, -J, -U, -DF, -lf, -d? display, -k, ...)
//! are refused.

mod archive;
mod crypt;
mod pattern;

use archive::{Archive, Entry, ReadError, MAX32};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

// Exit codes and messages (ziperr.h)
const ZE_OK: i32 = 0;
const ZE_FORM: i32 = 3;
const ZE_TEST: i32 = 8;
const ZE_TEMP: i32 = 10;
const ZE_READ: i32 = 11;
const ZE_NONE: i32 = 12;
const ZE_NAME: i32 = 13;
const ZE_WRITE: i32 = 14;
const ZE_CREAT: i32 = 15;
const ZE_PARMS: i32 = 16;
const ZE_OPEN: i32 = 18;

fn errmsg(code: i32) -> &'static str {
    match code {
        2 => "Unexpected end of zip file",
        3 => "Zip file structure invalid",
        4 => "Out of memory",
        5 => "Internal logic error",
        6 => "Entry too big to split, read, or write",
        7 => "Invalid comment format",
        8 => "Zip file invalid, could not spawn unzip, or wrong unzip",
        9 => "Interrupted",
        10 => "Temporary file failure",
        11 => "Input file read failure",
        12 => "Nothing to do!",
        13 => "Missing or empty zip file",
        14 => "Output file write failure",
        15 => "Could not create output file",
        16 => "Invalid command arguments",
        18 => "File not found or no read permission",
        19 => "Not supported",
        20 => "Attempt to read unsupported Zip64 archive",
        _ => "Normal successful completion",
    }
}

/// The errors whose message starts with strerror ("zip I/O error: ...")
fn perr(code: i32) -> bool {
    matches!(code, 10 | 11 | 14 | 15 | 18)
}

/// An error that ends the run: its code and the text in parentheses.
struct ZipErr(i32, String);

type Res<T> = Result<T, ZipErr>;

fn parms(s: impl Into<String>) -> ZipErr {
    ZipErr(ZE_PARMS, s.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Add,
    Update,
    Freshen,
    FileSync,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Deflate,
    Bzip2,
    Store,
}

struct Opts {
    level: u32,
    method: Method,
    recurse: bool,
    recurse_patterns: bool,
    junk: bool,
    quiet: bool,
    verbose: bool,
    no_dirs: bool,
    symlinks: bool,
    excludes: Vec<Vec<u8>>,
    includes: Vec<Vec<u8>>,
    names_stdin: bool,
    mode: Mode,
    move_files: bool,
    encrypt: bool,
    password: Option<Vec<u8>>,
    no_extra: bool,
    latest: bool,
    special: Vec<Vec<u8>>,
    zip_comment: bool,
    entry_comments: bool,
    test: bool,
    unzip_cmd: Option<String>,
    show_files: bool,
    temp_dir: Option<PathBuf>,
    grow: bool,
    out_file: Option<OsString>,
    before: Option<u32>,
    after: Option<u32>,
    translate: u8,
    must_match: bool,
    descriptors: bool,
    force_zip64: bool,
    fifo: bool,
    show_command: bool,
    pat: pattern::Options,
    help: u8,
    license: bool,
    version: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            level: 6,
            method: Method::Deflate,
            recurse: false,
            recurse_patterns: false,
            junk: false,
            quiet: false,
            verbose: false,
            no_dirs: false,
            symlinks: false,
            excludes: Vec::new(),
            includes: Vec::new(),
            names_stdin: false,
            mode: Mode::Add,
            move_files: false,
            encrypt: false,
            password: None,
            no_extra: false,
            latest: false,
            special: split_suffixes(b".Z:.zip:.zoo:.arc:.lzh:.arj"),
            zip_comment: false,
            entry_comments: false,
            test: false,
            unzip_cmd: None,
            show_files: false,
            temp_dir: None,
            grow: false,
            out_file: None,
            before: None,
            after: None,
            translate: 0,
            must_match: false,
            descriptors: false,
            force_zip64: false,
            fifo: false,
            show_command: false,
            pat: pattern::Options::default(),
            help: 0,
            license: false,
            version: false,
        }
    }
}

fn split_suffixes(s: &[u8]) -> Vec<Vec<u8>> {
    s.split(|&c| c == b':' || c == b';').filter(|x| !x.is_empty()).map(|x| x.to_vec()).collect()
}

/// How an option takes a value (zip.c's o_NO_VALUE, o_REQUIRED_VALUE, o_VALUE_LIST).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Val {
    No,
    Required,
    List,
}

/// The options a Unix zip 3.0 knows: short name, long name, value, negatable, whether this zip
/// does it.
const OPTIONS: &[(&str, &str, Val, bool, bool)] = &[
    ("0", "store", Val::No, false, true),
    ("1", "compress-1", Val::No, false, true),
    ("2", "compress-2", Val::No, false, true),
    ("3", "compress-3", Val::No, false, true),
    ("4", "compress-4", Val::No, false, true),
    ("5", "compress-5", Val::No, false, true),
    ("6", "compress-6", Val::No, false, true),
    ("7", "compress-7", Val::No, false, true),
    ("8", "compress-8", Val::No, false, true),
    ("9", "compress-9", Val::No, false, true),
    ("A", "adjust-sfx", Val::No, false, false),
    ("b", "temp-path", Val::Required, false, true),
    ("c", "entry-comments", Val::No, false, true),
    ("d", "delete", Val::No, false, true),
    ("db", "display-bytes", Val::No, true, false),
    ("dc", "display-counts", Val::No, true, false),
    ("dd", "display-dots", Val::No, true, false),
    ("dg", "display-globaldots", Val::No, true, false),
    ("ds", "dot-size", Val::Required, false, false),
    ("du", "display-usize", Val::No, true, false),
    ("dv", "display-volume", Val::No, true, false),
    ("df", "datafork", Val::No, false, false),
    ("D", "no-dir-entries", Val::No, false, true),
    ("DF", "difference-archive", Val::No, false, false),
    ("e", "encrypt", Val::No, false, true),
    ("F", "fix", Val::No, false, false),
    ("FF", "fixfix", Val::No, false, false),
    ("FI", "fifo", Val::No, true, true),
    ("FS", "filesync", Val::No, false, true),
    ("f", "freshen", Val::No, false, true),
    ("fd", "force-descriptors", Val::No, false, true),
    ("fz", "force-zip64", Val::No, true, true),
    ("g", "grow", Val::No, false, true),
    ("h", "help", Val::No, false, true),
    ("H", "", Val::No, false, true),
    ("?", "", Val::No, false, true),
    ("h2", "more-help", Val::No, false, true),
    ("i", "include", Val::List, false, true),
    ("ic", "ignore-case", Val::No, true, true),
    ("j", "junk-paths", Val::No, false, true),
    ("jj", "absolute-path", Val::No, false, false),
    ("J", "junk-sfx", Val::No, false, false),
    ("k", "DOS-names", Val::No, false, false),
    ("l", "to-crlf", Val::No, false, true),
    ("ll", "from-crlf", Val::No, false, true),
    ("lf", "logfile-path", Val::Required, false, false),
    ("la", "log-append", Val::No, true, false),
    ("li", "log-info", Val::No, true, false),
    ("L", "license", Val::No, false, true),
    ("m", "move", Val::No, false, true),
    ("MM", "must-match", Val::No, false, true),
    ("n", "suffixes", Val::Required, false, true),
    ("nw", "no-wild", Val::No, false, true),
    ("o", "latest-time", Val::No, false, true),
    ("O", "output-file", Val::Required, false, true),
    ("p", "paths", Val::No, false, true),
    ("P", "password", Val::Required, false, true),
    ("q", "quiet", Val::No, false, true),
    ("r", "recurse-paths", Val::No, false, true),
    ("R", "recurse-patterns", Val::No, false, true),
    ("RE", "regex", Val::No, false, true),
    ("s", "split-size", Val::Required, false, false),
    ("sp", "split-pause", Val::No, false, false),
    ("sv", "split-verbose", Val::No, false, false),
    ("sb", "split-bell", Val::No, false, false),
    ("sc", "show-command", Val::No, false, true),
    ("sC", "create-files", Val::No, false, false),
    ("sd", "show-debug", Val::No, false, false),
    ("sf", "show-files", Val::No, true, true),
    ("so", "show-options", Val::No, false, false),
    ("su", "show-unicode", Val::No, true, true),
    ("sU", "show-just-unicode", Val::No, true, true),
    ("t", "from-date", Val::Required, false, true),
    ("tt", "before-date", Val::Required, false, true),
    ("T", "test", Val::No, false, true),
    ("TT", "unzip-command", Val::Required, false, true),
    ("u", "update", Val::No, false, true),
    ("U", "copy-entries", Val::No, false, false),
    ("UN", "unicode", Val::Required, false, false),
    ("v", "verbose", Val::No, false, true),
    ("", "version", Val::No, false, true),
    ("ws", "wild-stop-dirs", Val::No, false, true),
    ("x", "exclude", Val::List, false, true),
    ("X", "no-extra", Val::No, false, true),
    ("X", "strip-extra", Val::No, true, true),
    ("y", "symlinks", Val::No, false, true),
    ("z", "archive-comment", Val::No, false, true),
    ("Z", "compression-method", Val::Required, false, true),
    ("@", "names-stdin", Val::No, false, true),
];

/// Where messages go: standard output, or standard error when the archive goes there.
struct Mesg {
    to_err: bool,
    quiet: bool,
    /// A line is started (an "  adding: name" waiting for its result)
    started: bool,
}

impl Mesg {
    fn out(&mut self, s: &str) {
        let _ = if self.to_err { io::stderr().write_all(s.as_bytes()) } else { io::stdout().write_all(s.as_bytes()) };
        if let Some(c) = s.chars().last() {
            self.started = c != '\n';
        }
    }
    fn flush(&self) {
        let _ = if self.to_err { io::stderr().flush() } else { io::stdout().flush() };
    }
    /// zipwarn: "\tzip warning: ..." on a line of its own, unless -q
    fn warn(&mut self, a: &str, b: &str) {
        if self.quiet {
            return;
        }
        if self.started {
            self.out("\n");
        }
        self.out(&format!("\tzip warning: {a}{b}\n"));
    }
    /// ziperr's message
    fn error(&mut self, e: &ZipErr, os: Option<&io::Error>) {
        if self.started {
            self.out("\n");
        }
        if perr(e.0) {
            let what = os.map(crate::common::errstr).unwrap_or_else(|| crate::common::errstr(&io::Error::last_os_error()));
            self.out(&format!("zip I/O error: {what}"));
        }
        self.out(&format!("\nzip error: {} ({})\n", errmsg(e.0), e.1));
        self.flush();
    }
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A name to put in the archive, found on the file system (or standard input).
struct Found {
    path: PathBuf,
    zname: Vec<u8>,
    meta: Option<Metadata>,
    is_dir: bool,
    is_link: bool,
    stdin: bool,
}

pub fn main(_name: &str, args: Vec<String>) -> i32 {
    let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
    let mut m = Mesg { to_err: false, quiet: false, started: false };
    match run(args, &mut m) {
        Ok(code) => code,
        Err(e) => {
            m.error(&e, None);
            e.0
        }
    }
}

struct Parsed {
    o: Opts,
    zipfile: Option<OsString>,
    /// The zip file as typed, for the "try:" hint
    args_before_files: Vec<String>,
    files: Vec<OsString>,
}

fn find_short(s: &[u8]) -> Option<&'static (&'static str, &'static str, Val, bool, bool)> {
    // The longest short option first (-sf before -s, -ll before -l)
    for len in [2, 1] {
        if s.len() >= len {
            if let Some(opt) = OPTIONS.iter().find(|o| !o.0.is_empty() && o.0.as_bytes() == &s[..len]) {
                return Some(opt);
            }
        }
    }
    None
}

fn parse(args: Vec<OsString>, m: &mut Mesg) -> Res<Parsed> {
    let mut o = Opts::default();
    let mut zipfile = None;
    let mut files = Vec::new();
    let mut before_files = Vec::new();
    let mut verbatim = false;
    let mut i = 0;
    let mut level_set = false;
    while i < args.len() {
        let a = args[i].as_bytes().to_vec();
        i += 1;
        if verbatim || a.len() < 2 || a[0] != b'-' {
            if a == b"--" && !verbatim {
                verbatim = true;
                continue;
            }
            if zipfile.is_none() {
                before_files.push(s(&a));
                zipfile = Some(OsString::from_vec(a));
            } else {
                files.push(OsString::from_vec(a));
            }
            continue;
        }
        if zipfile.is_none() || files.is_empty() {
            before_files.push(s(&a));
        }
        // One option word: --long[=value] or a cluster of short options
        let mut opts: Vec<(&'static (&str, &str, Val, bool, bool), bool, Option<Vec<u8>>)> = Vec::new();
        if a.starts_with(b"--") {
            let body = &a[2..];
            let (name, value) = match body.iter().position(|&c| c == b'=') {
                Some(p) => (&body[..p], Some(body[p + 1..].to_vec())),
                None => (body, None),
            };
            let (name, neg) = match name.strip_suffix(b"-") {
                Some(n) if OPTIONS.iter().any(|o| o.1.as_bytes() == n && o.3) => (n, true),
                _ => (name, false),
            };
            let Some(opt) = OPTIONS.iter().find(|o| !o.1.is_empty() && o.1.as_bytes() == name) else {
                return Err(parms(format!("long option '{}' not supported", s(name))));
            };
            if value.is_some() && opt.2 == Val::No {
                return Err(parms(format!("long option '{}' does not take a value", s(name))));
            }
            opts.push((opt, neg, value));
        } else {
            let mut j = 1;
            while j < a.len() {
                let Some(opt) = find_short(&a[j..]) else {
                    return Err(parms(format!("short option '{}' not supported", a[j] as char)));
                };
                j += opt.0.len();
                let mut neg = false;
                if opt.3 && a.get(j) == Some(&b'-') {
                    neg = true;
                    j += 1;
                }
                if opt.2 != Val::No {
                    // The rest of the word is the value (after an optional '='), else the next word
                    let rest = &a[j..];
                    let rest = rest.strip_prefix(b"=").unwrap_or(rest);
                    let value = if !rest.is_empty() { Some(rest.to_vec()) } else { None };
                    opts.push((opt, neg, value));
                    break;
                }
                opts.push((opt, neg, None));
            }
        }
        for (opt, neg, value) in opts {
            if !opt.4 {
                let name = if opt.0.is_empty() { format!("--{}", opt.1) } else { format!("-{}", opt.0) };
                return Err(parms(format!("{name} is not supported by this zip")));
            }
            let mut value = value;
            if opt.2 == Val::Required && value.is_none() {
                if i >= args.len() {
                    let name = if opt.0.is_empty() { opt.1 } else { opt.0 };
                    return Err(parms(format!("option {name} requires a value")));
                }
                value = Some(args[i].as_bytes().to_vec());
                i += 1;
            }
            let mut list = Vec::new();
            if opt.2 == Val::List {
                if let Some(v) = value.take() {
                    list.push(v);
                }
                while i < args.len() && !(args[i].as_bytes().starts_with(b"-") && args[i].len() > 1) {
                    list.push(args[i].as_bytes().to_vec());
                    i += 1;
                }
                // pattern@file: the patterns are its lines
                let mut expanded = Vec::new();
                for p in list {
                    if let Some(f) = p.strip_prefix(b"@") {
                        let text = fs::read(OsStr::from_bytes(f))
                            .map_err(|_| parms(format!("can't open include/exclude file {}", s(f))))?;
                        expanded.extend(text.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec()).filter(|l| !l.is_empty()));
                    } else {
                        expanded.push(p);
                    }
                }
                list = expanded;
            }
            let v = value.unwrap_or_default();
            match opt.0 {
                d @ ("0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                    o.level = d.parse().unwrap();
                    level_set = true;
                }
                "b" => o.temp_dir = Some(PathBuf::from(OsString::from_vec(v))),
                "c" => o.entry_comments = true,
                "d" => o.mode = Mode::Delete,
                "D" => o.no_dirs = true,
                "e" => o.encrypt = true,
                "FI" => o.fifo = !neg,
                "FS" => o.mode = Mode::FileSync,
                "f" => o.mode = Mode::Freshen,
                "fd" => o.descriptors = true,
                "fz" => o.force_zip64 = !neg,
                "g" => o.grow = true,
                "h" | "H" | "?" => o.help = 1,
                "h2" => o.help = 2,
                "i" => o.includes.extend(list),
                "ic" => {} // matches nothing differently on Unix, as in zip 3.0
                "j" => o.junk = true,
                "l" => o.translate = 1,
                "ll" => o.translate = 2,
                "L" => o.license = true,
                "m" => o.move_files = true,
                "MM" => o.must_match = true,
                "n" => o.special = split_suffixes(&v),
                "nw" => o.pat.no_wild = true,
                "o" => o.latest = true,
                "O" => o.out_file = Some(OsString::from_vec(v)),
                "p" => o.junk = false,
                "P" => {
                    o.password = Some(v);
                    o.encrypt = true;
                }
                "q" => o.quiet = true,
                "r" => o.recurse = true,
                "R" => o.recurse_patterns = true,
                "RE" => {}
                "sc" => o.show_command = true,
                "sf" | "su" | "sU" => o.show_files = !neg,
                "t" => o.before = Some(parse_date(&v, "-t")?),
                "tt" => o.after = Some(parse_date(&v, "-tt")?),
                "T" => o.test = true,
                "TT" => o.unzip_cmd = Some(s(&v)),
                "u" => o.mode = Mode::Update,
                "v" => o.verbose = true,
                "" => o.version = true,
                "ws" => o.pat.stop_at_dir = true,
                "x" => o.excludes.extend(list),
                "X" => o.no_extra = !neg,
                "y" => o.symlinks = true,
                "z" => o.zip_comment = true,
                "Z" => {
                    o.method = match v.to_ascii_lowercase().as_slice() {
                        b"store" | b"s" => Method::Store,
                        b"deflate" | b"d" => Method::Deflate,
                        b"bzip2" | b"b" | b"bz" | b"bzip" => Method::Bzip2,
                        _ => return Err(parms(format!("Unsupported compression method {}", s(&v)))),
                    }
                }
                "@" => o.names_stdin = true,
                _ => unreachable!(),
            }
        }
    }
    if o.method == Method::Store {
        o.level = 0;
    } else if o.level == 0 && level_set {
        o.method = Method::Store;
    }
    m.quiet = o.quiet;
    Ok(Parsed { o, zipfile, args_before_files: before_files, files })
}

/// A -t/-tt date (mmddyyyy or yyyy-mm-dd) as a DOS time at its midnight.
fn parse_date(v: &[u8], opt: &str) -> Res<u32> {
    let t = std::str::from_utf8(v).unwrap_or("");
    let (y, mo, d) = if t.len() == 10 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-' {
        (t[..4].parse::<u32>().ok(), t[5..7].parse::<u32>().ok(), t[8..10].parse::<u32>().ok())
    } else if t.len() == 8 && t.bytes().all(|c| c.is_ascii_digit()) {
        (t[4..].parse().ok(), t[..2].parse().ok(), t[2..4].parse().ok())
    } else {
        (None, None, None)
    };
    match (y, mo, d) {
        (Some(y), Some(mo), Some(d)) if (1980..2108).contains(&y) && (1..=12).contains(&mo) && (1..=31).contains(&d) => {
            Ok(((y - 1980) << 25) | (mo << 21) | (d << 16))
        }
        _ => Err(parms(format!("invalid date entered for {opt} option - use mmddyyyy or yyyy-mm-dd"))),
    }
}

/// The archive's name: ".zip" is added when the last component has no dot (ziptyp).
fn zip_path(name: &OsStr) -> PathBuf {
    let b = name.as_bytes();
    let base = b.rsplit(|&c| c == b'/').next().unwrap_or(b);
    if b != b"-" && !base.contains(&b'.') {
        let mut v = b.to_vec();
        v.extend_from_slice(b".zip");
        return PathBuf::from(OsString::from_vec(v));
    }
    PathBuf::from(name)
}

/// ex2in: the archive name of a file system name. Leading slashes and "./" go; -j keeps only the
/// last component; directories end in '/'.
fn ex2in(path: &[u8], is_dir: bool, junk: bool) -> Vec<u8> {
    let mut t = path;
    while t.first() == Some(&b'/') {
        t = &t[1..];
    }
    while t.starts_with(b"./") {
        t = &t[2..];
        while t.first() == Some(&b'/') {
            t = &t[1..];
        }
    }
    let mut n: Vec<u8> = t.to_vec();
    while is_dir && n.len() > 1 && n.ends_with(b"//") {
        n.pop();
    }
    if junk {
        let trimmed = n.strip_suffix(b"/").unwrap_or(&n);
        let last = trimmed.rsplit(|&c| c == b'/').next().unwrap_or(trimmed).to_vec();
        n = last;
    }
    if n == b"." || n == b"./" {
        return Vec::new();
    }
    if is_dir && !n.is_empty() && !n.ends_with(b"/") {
        n.push(b'/');
    }
    n
}

fn is_utf8_non_ascii(n: &[u8]) -> bool {
    !n.is_ascii() && std::str::from_utf8(n).is_ok()
}

/// What -x, -i, -D and the dates leave of a found name.
fn selected(o: &Opts, zname: &[u8], is_dir: bool, dostime: Option<u32>) -> bool {
    if zname.is_empty() {
        return false;
    }
    if is_dir && o.no_dirs {
        return false;
    }
    if o.excludes.iter().any(|p| pattern::matches(p, zname, o.pat)) {
        return false;
    }
    if !o.includes.is_empty() && !o.includes.iter().any(|p| pattern::matches(p, zname, o.pat)) {
        return false;
    }
    if let Some(t) = dostime {
        if o.before.is_some_and(|b| t < b) || o.after.is_some_and(|a| t >= a) {
            return false;
        }
    }
    true
}

struct Scan<'a> {
    o: &'a Opts,
    found: Vec<Found>,
    /// zname -> index in found
    names: HashMap<Vec<u8>, usize>,
    /// The archive being written, never added to itself
    skip: Option<(u64, u64)>,
    /// Two files with one name in the archive (check_dup reports it once the scan is done)
    dup: Option<(PathBuf, PathBuf, Vec<u8>)>,
}

impl Scan<'_> {
    /// newname(): one name to add, after the filters
    fn add(&mut self, path: &Path, meta: Option<Metadata>, is_dir: bool, is_link: bool, stdin: bool) -> Res<()> {
        let zname = if stdin { b"-".to_vec() } else { ex2in(path.as_os_str().as_bytes(), is_dir, self.o.junk) };
        let dostime = meta.as_ref().map(|md| archive::dos_time(md.mtime()));
        if !selected(self.o, &zname, is_dir, dostime) {
            return Ok(());
        }
        if let (Some(md), Some(skip)) = (&meta, self.skip) {
            if (md.dev(), md.ino()) == skip {
                return Ok(());
            }
        }
        if let Some(&k) = self.names.get(&zname) {
            let prev = &self.found[k];
            if prev.path == path {
                return Ok(());
            }
            if self.dup.is_none() {
                let (a, b) = if prev.path.as_os_str() <= path.as_os_str() { (prev.path.clone(), path.to_path_buf()) } else { (path.to_path_buf(), prev.path.clone()) };
                self.dup = Some((a, b, zname));
            }
            return Ok(());
        }
        self.names.insert(zname.clone(), self.found.len());
        self.found.push(Found { path: path.to_path_buf(), zname, meta, is_dir, is_link, stdin });
        Ok(())
    }

    /// procname(): a name from the command line (or -@, or recursion). False: not matched.
    fn procname(&mut self, m: &mut Mesg, path: &Path, recurse: bool) -> Res<bool> {
        if path.as_os_str().as_bytes() == b"-" {
            let md = File::open("/dev/stdin").and_then(|f| f.metadata()).ok();
            self.add(path, md, false, false, true)?;
            return Ok(true);
        }
        let Ok(lmeta) = fs::symlink_metadata(path) else { return Ok(false) };
        if lmeta.file_type().is_symlink() && self.o.symlinks {
            self.add(path, Some(lmeta), false, true, false)?;
            return Ok(true);
        }
        let Ok(meta) = fs::metadata(path) else { return Ok(false) };
        let ft = meta.file_type();
        if ft.is_dir() {
            self.add(path, Some(meta), true, false, false)?;
            if recurse {
                let Ok(rd) = fs::read_dir(path) else { return Ok(true) };
                let base = path.as_os_str().as_bytes();
                for ent in rd.flatten() {
                    let mut p = base.to_vec();
                    if !p.ends_with(b"/") {
                        p.push(b'/');
                    }
                    p.extend_from_slice(ent.file_name().as_bytes());
                    let child = PathBuf::from(OsString::from_vec(p));
                    if !self.procname(m, &child, true)? {
                        m.warn("name not matched: ", &child.display().to_string());
                    }
                }
            }
            return Ok(true);
        }
        if ft.is_fifo() && !self.o.fifo {
            m.warn("ignoring FIFO (Named Pipe) - use -FI to read: ", &path.display().to_string());
            return Ok(true);
        }
        if !ft.is_file() && !ft.is_fifo() {
            m.warn("ignoring special file: ", &path.display().to_string());
            return Ok(true);
        }
        self.add(path, Some(meta), false, false, false)?;
        Ok(true)
    }

    /// -R: every file under the current directory whose name matches one of the patterns.
    fn recurse_patterns(&mut self, m: &mut Mesg, dir: &Path, patterns: &[Vec<u8>], hits: &mut [bool]) -> Res<()> {
        let Ok(rd) = fs::read_dir(dir) else { return Ok(()) };
        for ent in rd.flatten() {
            let path = if dir == Path::new(".") { PathBuf::from(ent.file_name()) } else { dir.join(ent.file_name()) };
            let Ok(lmeta) = fs::symlink_metadata(&path) else { continue };
            let meta = if lmeta.file_type().is_symlink() && !self.o.symlinks {
                match fs::metadata(&path) {
                    Ok(md) => md,
                    Err(_) => {
                        m.warn("name not matched: ", &path.display().to_string());
                        continue;
                    }
                }
            } else {
                lmeta
            };
            let is_dir = meta.is_dir();
            if meta.file_type().is_fifo() && !self.o.fifo {
                m.warn("ignoring FIFO (Named Pipe) - use -FI to read: ", &path.display().to_string());
                continue;
            }
            let zname = ex2in(path.as_os_str().as_bytes(), is_dir, false);
            let mut hit = false;
            for (k, p) in patterns.iter().enumerate() {
                if pattern::matches(p, &zname, self.o.pat) {
                    hits[k] = true;
                    hit = true;
                }
            }
            if hit {
                let is_link = meta.file_type().is_symlink();
                self.add(&path, Some(meta), is_dir, is_link, false)?;
            }
            if is_dir {
                self.recurse_patterns(m, &path, patterns, hits)?;
            }
        }
        Ok(())
    }
}

/// The output: a file, possibly standard output, possibly one that cannot be rewound.
struct Out {
    f: io::BufWriter<File>,
    base: u64,
    pos: u64,
    seekable: bool,
}

impl Out {
    fn new(f: File, at: u64) -> Out {
        let seekable = (&f).stream_position().is_ok();
        let base = if seekable { (&f).stream_position().unwrap_or(0) - at } else { 0 };
        Out { f: io::BufWriter::with_capacity(1 << 16, f), base, pos: at, seekable }
    }
    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        self.f.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
    fn seek(&mut self, to: u64) -> io::Result<()> {
        self.f.flush()?;
        self.f.get_mut().seek(SeekFrom::Start(self.base + to))?;
        self.pos = to;
        Ok(())
    }
}

/// What a new entry's data passes through on its way out: encryption, then the count.
struct Sink<'a> {
    out: &'a mut Out,
    keys: Option<crypt::Keys>,
    n: u64,
    buf: Vec<u8>,
}

impl Write for Sink<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if let Some(k) = &mut self.keys {
            self.buf.clear();
            self.buf.extend_from_slice(b);
            k.encrypt(&mut self.buf);
            self.out.write(&self.buf)?;
        } else {
            self.out.write(b)?;
        }
        self.n += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The text bit (set_file_type): no black-listed control character, and at least one character
/// of text.
#[derive(Default)]
struct TextCheck {
    black: bool,
    white: bool,
}

impl TextCheck {
    fn feed(&mut self, b: &[u8]) {
        const MASK: u32 = 0xf3ff_c07f;
        for &c in b {
            if c < 32 {
                if MASK >> c & 1 != 0 {
                    self.black = true;
                } else if c == 9 || c == 10 || c == 13 {
                    self.white = true;
                }
            } else {
                self.white = true;
            }
        }
    }
    fn is_text(&self) -> bool {
        !self.black && self.white
    }
}

/// What is known of the data while it goes out: its CRC, its text bit, its size.
struct Acc {
    crc: crc32fast::Hasher,
    text: TextCheck,
    isize: u64,
}

impl Acc {
    fn take(&mut self, d: &[u8]) {
        self.crc.update(d);
        self.text.feed(d);
        self.isize += d.len() as u64;
    }
}

/// The rest of the source into `w` (through -l/-ll's conversion when given). Read errors are
/// returned as such; write errors as ErrorKind::Other with the write's error inside.
fn pump(source: &mut dyn Read, mut eol: Option<&mut Eol>, acc: &mut Acc, w: &mut dyn Write) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 16];
    let mut conv = Vec::new();
    loop {
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let data: &[u8] = match eol.as_deref_mut() {
            Some(e) => {
                e.apply(&buf[..n], &mut conv);
                &conv
            }
            None => &buf[..n],
        };
        acc.take(data);
        w.write_all(data)?;
    }
    if let Some(e) = eol {
        e.finish(&mut conv);
        acc.take(&conv);
        w.write_all(&conv)?;
    }
    Ok(())
}

/// -l / -ll: LF to CR LF, or CR LF to LF, carrying a CR across reads.
struct Eol {
    mode: u8,
    cr: bool,
}

impl Eol {
    fn apply(&mut self, b: &[u8], out: &mut Vec<u8>) {
        out.clear();
        for &c in b {
            if self.mode == 1 {
                if c == b'\n' {
                    out.push(b'\r');
                }
                out.push(c);
            } else {
                if self.cr {
                    self.cr = false;
                    if c != b'\n' {
                        out.push(b'\r');
                    }
                }
                if c == b'\r' {
                    self.cr = true;
                } else {
                    out.push(c);
                }
            }
        }
    }
    fn finish(&mut self, out: &mut Vec<u8>) {
        out.clear();
        if self.cr {
            out.push(b'\r');
            self.cr = false;
        }
    }
}

/// Info-ZIP's percent(): the rounded reduction from n to m.
fn percent(n: u64, m: u64) -> i64 {
    if n == 0 {
        return 0;
    }
    ((200 * (n as i64 - m as i64) / n as i64) + 1) / 2
}

/// WriteNumString: "8.6K", "20K", "2"
fn num_string(mut num: u64) -> String {
    let mut mult = 0;
    while num >= 10240 {
        num >>= 10;
        mult += 1;
    }
    let mut out = String::new();
    if num >= 1000 {
        num = (num * 10) >> 10;
        mult += 1;
        out.push((b'0' + (num / 10) as u8) as char);
        out.push('.');
        out.push((b'0' + (num % 10) as u8) as char);
    } else {
        out = num.to_string();
    }
    out.push_str(["", "K", "M", "G", "T"].get(mult).copied().unwrap_or("?"));
    out
}

const SMALL: usize = 32766;

/// The result of writing one new entry.
struct Written {
    entry: Entry,
    offset: u64,
}

struct Ctx<'a> {
    o: &'a Opts,
    m: &'a mut Mesg,
    password: Option<Vec<u8>>,
    total_in: u64,
    total_out: u64,
    bad_files: u64,
    bad_bytes: u64,
    good_files: u64,
    good_bytes: u64,
}

fn io_write(e: io::Error) -> ZipErr {
    ZipErr(ZE_WRITE, format!("unexpected error on zip file: {}", crate::common::errstr(&e)))
}

impl Ctx<'_> {
    /// zipup(): one new or replaced entry, with its "  adding: name (deflated n%)" line.
    fn zipup(&mut self, out: &mut Out, f: &Found, replacing: bool) -> Res<Option<Written>> {
        let o = self.o;
        let verb = if replacing && o.mode == Mode::Freshen {
            "freshening: "
        } else if replacing {
            "updating: "
        } else {
            "  adding: "
        };
        if !o.quiet {
            self.m.out(&format!("{verb}{}", s(&f.zname)));
            self.m.flush();
        }
        // Stat again now, as zipup does (an earlier read may have moved the access time)
        let fresh = if f.stdin {
            File::open("/dev/stdin").and_then(|f| f.metadata()).ok()
        } else if f.is_link {
            fs::symlink_metadata(&f.path).ok()
        } else {
            fs::metadata(&f.path).ok()
        };
        let meta = fresh.as_ref().or(f.meta.as_ref());
        let mtime = meta.map(|md| md.mtime()).unwrap_or_else(now);
        let atime = meta.map(|md| md.atime()).unwrap_or(mtime);
        let dostime = archive::dos_time(mtime);
        let mode = meta.map(|md| md.mode()).unwrap_or(0o100600);
        let mut eattr = (mode << 16) | if mode & 0o200 == 0 { 1 } else { 0 };
        if f.is_dir {
            eattr |= 0x10;
        }
        let size: Option<u64> = if f.is_dir {
            Some(0)
        } else if f.stdin || meta.is_some_and(|md| md.file_type().is_fifo()) {
            None
        } else {
            meta.map(|md| md.len())
        };

        // The data source
        let mut source: Box<dyn Read> = if f.is_dir {
            Box::new(io::empty())
        } else if f.is_link {
            let target = fs::read_link(&f.path).map(|t| t.into_os_string().into_vec()).unwrap_or_default();
            Box::new(io::Cursor::new(target))
        } else if f.stdin {
            Box::new(io::stdin())
        } else {
            match File::open(&f.path) {
                Ok(file) => Box::new(file),
                Err(e) => {
                    if self.m.started {
                        self.m.out("\n");
                    }
                    self.m.flush();
                    eprintln!("zip warning: {}", crate::common::errstr(&e));
                    self.m.warn("could not open for reading: ", &f.path.display().to_string());
                    self.bad_files += 1;
                    self.bad_bytes += size.unwrap_or(0);
                    return Ok(None);
                }
            }
        };
        let size = if f.is_link { Some(fs::read_link(&f.path).map(|t| t.as_os_str().len() as u64).unwrap_or(0)) } else { size };

        let suffix_store = !f.is_dir && {
            let n = f.zname.to_ascii_lowercase();
            o.special.iter().any(|sfx| n.ends_with(&sfx.to_ascii_lowercase()))
        };
        let mut method = if f.is_dir || f.is_link || size == Some(0) || o.method == Method::Store || o.level == 0 || suffix_store { Method::Store } else { o.method };
        let encrypt = !f.is_dir && self.password.is_some();
        let streaming = !out.seekable || o.descriptors;
        let zip64_local = !f.is_dir && (size.is_none() || size.is_some_and(|q| q >= MAX32) || o.force_zip64);
        let mut flags: u16 = if f.is_dir { 0 } else { 8 };
        if encrypt {
            flags |= 1;
        }
        if is_utf8_non_ascii(&f.zname) {
            flags |= 0x800;
        }
        let level_bits = |method: Method| -> u16 {
            if method == Method::Deflate {
                if o.level <= 2 {
                    4
                } else if o.level >= 8 {
                    2
                } else {
                    0
                }
            } else {
                0
            }
        };
        let need = |method: Method| -> u16 {
            let v = match method {
                Method::Store => 10,
                Method::Deflate => 20,
                Method::Bzip2 => 46,
            };
            if zip64_local {
                45.max(v)
            } else {
                v
            }
        };
        let method_code = |method: Method| -> u16 {
            match method {
                Method::Store => 0,
                Method::Deflate => 8,
                Method::Bzip2 => 12,
            }
        };
        let (lextra, cextra) = if o.no_extra || f.stdin {
            (Vec::new(), Vec::new())
        } else {
            let (uid, gid) = meta.map(|md| (md.uid(), md.gid())).unwrap_or((0, 0));
            let ux = archive::ux_extra(uid, gid);
            let mut l = archive::ut_extra(mtime, atime, true);
            l.extend_from_slice(&ux);
            let mut c = archive::ut_extra(mtime, atime, false);
            c.extend_from_slice(&ux);
            (l, c)
        };
        let check = if encrypt { dostime << 16 } else { 0 };

        // Read a small file whole first: zip stores it if deflating gains nothing (only when the
        // header can be rewritten, as in trees.c's first-and-last-block rule)
        let mut eol = Eol { mode: o.translate, cr: false };
        let mut ebuf = Vec::new();
        let mut head: Vec<u8> = Vec::new();
        let mut eof = f.is_dir;
        if !f.is_dir {
            let mut chunk = vec![0u8; SMALL + 1];
            let mut got = 0;
            while got < chunk.len() {
                match source.read(&mut chunk[got..]) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => got += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(ZipErr(ZE_READ, format!("{}: {}", f.path.display(), crate::common::errstr(&e)))),
                }
            }
            chunk.truncate(got);
            head = chunk;
        }
        // -l/-ll only on text: the first buffer says
        let translate = o.translate != 0 && !f.is_dir && {
            let mut t = TextCheck::default();
            t.feed(&head);
            if !t.is_text() && !head.is_empty() {
                self.m.warn(if o.translate == 1 { "has binary so -l ignored" } else { "has binary so -ll ignored" }, "");
                false
            } else {
                true
            }
        };
        if translate {
            eol.apply(&head, &mut ebuf);
            if eof {
                let mut tail = Vec::new();
                eol.finish(&mut tail);
                ebuf.extend_from_slice(&tail);
            }
            head = std::mem::take(&mut ebuf);
        }
        let small = eof && !streaming && method == Method::Deflate && head.len() <= SMALL;
        let mut deflated_small: Option<Vec<u8>> = None;
        if small {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(o.level));
            enc.write_all(&head).map_err(io_write)?;
            let d = enc.finish().map_err(io_write)?;
            if d.len() >= head.len() {
                method = Method::Store;
            } else {
                deflated_small = Some(d);
            }
        }

        let offset = out.pos;
        let lflags = flags | if streaming { level_bits(method) } else { 0 };
        let first = archive::local_header(&archive::Local {
            need: need(method),
            flags: lflags,
            method: method_code(method),
            dostime,
            crc: check,
            csize: if method == Method::Store { size.unwrap_or(0) } else { 0 },
            usize: size.unwrap_or(0),
            name: &f.zname,
            extra: &lextra,
            zip64: zip64_local,
        });
        out.write(&first).map_err(io_write)?;

        let mut sink = Sink { out, keys: None, n: 0, buf: Vec::new() };
        if encrypt {
            let (head, keys) = crypt::header(self.password.as_ref().unwrap(), check);
            sink.out.write(&head).map_err(io_write)?;
            sink.keys = Some(keys);
        }
        let mut acc = Acc { crc: crc32fast::Hasher::new(), text: TextCheck::default(), isize: 0 };
        let read_err = |e: io::Error| ZipErr(ZE_READ, format!("{}: {}", f.path.display(), crate::common::errstr(&e)));
        if method == Method::Store || deflated_small.is_some() {
            acc.take(&head);
            match &deflated_small {
                Some(d) => sink.write_all(d).map_err(io_write)?,
                None => sink.write_all(&head).map_err(io_write)?,
            }
            if !eof {
                pump(&mut source, translate.then_some(&mut eol), &mut acc, &mut sink).map_err(read_err)?;
            }
        } else {
            let mut enc: Box<dyn Write + '_> = match method {
                Method::Bzip2 => Box::new(bzip2::write::BzEncoder::new(&mut sink, bzip2::Compression::new(o.level.clamp(1, 9)))),
                _ => Box::new(flate2::write::DeflateEncoder::new(&mut sink, flate2::Compression::new(o.level))),
            };
            acc.take(&head);
            enc.write_all(&head).map_err(io_write)?;
            if !eof {
                pump(&mut source, translate.then_some(&mut eol), &mut acc, &mut enc).map_err(read_err)?;
            }
            enc.flush().map_err(io_write)?;
            drop(enc);
        }
        let Acc { crc, text, isize } = acc;
        let csize = sink.n;
        drop(sink);
        let crc = crc.finalize();
        let full_csize = csize + if encrypt { 12 } else { 0 };
        if let (Some(q), false) = (size, translate) {
            if q != isize && !f.is_link {
                self.m.warn(" file size changed while zipping ", &s(&f.zname));
            }
        }
        // Either rewrite the local header (the descriptor stays only for encrypted entries), or
        // follow the data with the descriptor
        let mut flags = flags | level_bits(method);
        let zip64_dd = zip64_local || isize >= MAX32 || full_csize >= MAX32;
        if f.is_dir {
            flags &= !8;
        } else if streaming {
            if method == Method::Store && size.is_none() {
                return Err(parms("zip -0 not supported for I/O on pipes or devices"));
            }
            out.write(&archive::data_descriptor(crc, full_csize, isize, zip64_dd)).map_err(io_write)?;
        } else {
            if !encrypt {
                flags &= !8;
            }
            let end = out.pos;
            out.seek(offset).map_err(io_write)?;
            let again = archive::local_header(&archive::Local {
                need: need(method),
                flags,
                method: method_code(method),
                dostime,
                crc,
                csize: full_csize,
                usize: isize,
                name: &f.zname,
                extra: &lextra,
                zip64: zip64_local,
            });
            if again.len() != first.len() {
                return Err(ZipErr(ZE_WRITE, "local header changed size".into()));
            }
            out.write(&again).map_err(io_write)?;
            out.seek(end).map_err(io_write)?;
            if encrypt {
                out.write(&archive::data_descriptor(crc, full_csize, isize, zip64_dd)).map_err(io_write)?;
            }
        }
        if !f.is_dir && !zip64_local && (isize >= MAX32 || full_csize >= MAX32) && !streaming {
            return Err(ZipErr(6, s(&f.zname)));
        }

        // The result line
        if !o.quiet {
            let what = match method {
                Method::Store => "stored".to_string(),
                Method::Deflate => "deflated".to_string(),
                Method::Bzip2 => "bzipped".to_string(),
            };
            let pct = if method == Method::Store { 0 } else { percent(isize, csize) };
            if o.verbose && !f.is_dir {
                self.m.out(&format!("\t(in={isize}) (out={csize}) ({what} {pct}%)\n"));
            } else {
                self.m.out(&format!(" ({what} {pct}%)\n"));
            }
        }
        self.total_in += isize;
        self.total_out += csize;
        self.good_files += 1;
        self.good_bytes += isize;
        let iattr = if method == Method::Store && (o.level == 0 || suffix_store || o.method == Method::Store) || f.is_dir || f.is_link || isize == 0 {
            0
        } else {
            text.is_text() as u16
        };
        Ok(Some(Written {
            entry: Entry {
                made_by: archive::MADE_BY,
                need: need(method),
                flags,
                method: method_code(method),
                dostime,
                crc,
                csize: full_csize,
                usize: isize,
                name: f.zname.clone(),
                extra: cextra,
                comment: Vec::new(),
                iattr,
                eattr,
                offset,
            },
            offset,
        }))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A password from the terminal, with echo off (getp).
fn get_password(prompt: &str) -> io::Result<Vec<u8>> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    use std::os::unix::io::AsRawFd;
    let fd = tty.as_raw_fd();
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    let have = unsafe { libc::tcgetattr(fd, &mut old) } == 0;
    if have {
        let mut t = old;
        t.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &t) };
    }
    tty.write_all(prompt.as_bytes())?;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while tty.read(&mut b)? == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    if have {
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &old) };
    }
    tty.write_all(b"\n")?;
    Ok(line)
}

fn help(level: u8) -> String {
    let mut t = String::from(
        "Copyright (c) 1990-2008 Info-ZIP - Type 'zip \"-L\"' for software license.\n\
Zip 3.0 as rewritten for collaboCore (collabo-archive, not an Info-ZIP release). Usage:\n\
zip [-options] [-b path] [-t mmddyyyy] [-n suffixes] [zipfile list] [-xi list]\n\
  The default action is to add or replace zipfile entries from list, which\n\
  can include the special name - to compress standard input.\n\
  If zipfile and list are omitted, zip compresses stdin to stdout.\n\
  -f   freshen: only changed files  -u   update: only changed or new files\n\
  -d   delete entries in zipfile    -m   move into zipfile (delete OS files)\n\
  -r   recurse into directories     -j   junk (don't record) directory names\n\
  -0   store only                   -l   convert LF to CR LF (-ll CR LF to LF)\n\
  -1   compress faster              -9   compress better\n\
  -q   quiet operation              -v   verbose operation/print version info\n\
  -c   add one-line comments        -z   add zipfile comment\n\
  -@   read names from stdin        -o   make zipfile as old as latest entry\n\
  -x   exclude the following names  -i   include only the following names\n\
  -D   do not add directory entries -T   test zipfile integrity\n\
  -X   eXclude eXtra file attributes\n\
  -y   store symbolic links as the link instead of the referenced file\n\
  -e   encrypt                      -n   don't compress these suffixes\n\
  -h2  show more help\n",
    );
    if level == 2 {
        t.push_str(
            "\n\
Basic modes:\n\
 External modes (selects files from file system):\n\
        add      - add new files/update existing files in archive (default)\n\
  -u    update   - add new files/update existing files only if later date\n\
  -f    freshen  - update existing files only (no files added)\n\
  -FS   filesync - update if date or size changed, delete if no OS match\n\
 Internal modes (selects entries in archive):\n\
  -d    delete   - delete files from archive (see below)\n\
\n\
More options:\n\
  -R        recurse current dir and match patterns (-R \"*.c\")\n\
  -ws       wildcards stop at directory separators (** crosses them)\n\
  -nw       no wildcards (wildcards are like any other character)\n\
  -ic       ignore case when matching archive entries\n\
  -t date   exclude before date, -tt date include before date (mmddyyyy or yyyy-mm-dd)\n\
  -P pswd   encrypt with this password (unsafe: other users can see it)\n\
  -Z cm     compression method: store, deflate, bzip2\n\
  -b path   put the temporary zip file in path\n\
  -O out    write the result to out, leaving zipfile as it is\n\
  -g        grow: append to the existing archive in place\n\
  -fd       force data descriptors, -fz force Zip64\n\
  -FI       read FIFOs (named pipes), -MM must match all input names\n\
  -sf       show the files that would be operated on, and exit\n\
  -TT cmd   test with this command (the zip file's name is added) instead of in-process\n\
\n\
Not in this zip: splits (-s), fixing (-F, -FF), self-extractors (-A, -J), copy mode (-U),\n\
difference archives (-DF), log files (-lf), progress displays (-db, -dc, -dd, ...).\n",
        );
    }
    t
}

fn version_info() -> String {
    "Copyright (c) 1990-2008 Info-ZIP - Type 'zip \"-L\"' for software license.\n\
This is Zip 3.0 as rewritten in Rust for collaboCore's guest (collabo-archive): it follows\n\
Info-ZIP's Zip 3.0 (July 5th 2008) but is not an Info-ZIP release.\n\
\n\
Zip special compilation options:\n\
\tUSE_EF_UT_TIME       (store Universal Time)\n\
\tSYMLINK_SUPPORT      (symbolic links supported)\n\
\tLARGE_FILE_SUPPORT   (can read and write large files on file system)\n\
\tZIP64_SUPPORT        (use Zip64 to store large files in archives)\n\
\tSTORE_UNIX_UIDs_GIDs (store UID/GID sizes/values using new extra field)\n\
\tBZIP2_SUPPORT        (bzip2 library)\n\
\t[encryption: traditional PKWARE]\n"
        .to_string()
}

const LICENSE: &str = include_str!("../../LICENSE-infozip");

fn run(args: Vec<OsString>, m: &mut Mesg) -> Res<i32> {
    // No arguments: a filter from standard input to standard output, unless both are terminals
    if args.is_empty() {
        if crate::common::stdin_is_tty() || crate::common::stdout_is_tty() {
            m.out(&help(1));
            return Ok(ZE_OK);
        }
        return run(vec!["-".into(), "-".into()], m);
    }
    let p = parse(args.clone(), m)?;
    let mut o = p.o;
    if o.show_command {
        m.out("command line:\n");
        for a in &args {
            m.out(&format!("'{}'  ", a.to_string_lossy()));
        }
        m.out("\n");
        return Ok(ZE_OK);
    }
    if o.license {
        m.out(LICENSE);
        return Ok(ZE_OK);
    }
    if o.help > 0 {
        m.out(&help(o.help));
        return Ok(ZE_OK);
    }
    if (o.version || o.verbose) && p.zipfile.is_none() {
        m.out(&version_info());
        return Ok(ZE_OK);
    }
    let Some(zarg) = p.zipfile else {
        return Err(ZipErr(ZE_NAME, "no zip file name given".into()));
    };
    let to_stdout = zarg.as_bytes() == b"-";
    m.to_err = to_stdout;
    let zpath = zip_path(&zarg);
    let zname_shown = zpath.display().to_string();

    // The existing archive
    let mut zin: Option<File> = None;
    let mut arch = Archive { entries: Vec::new(), comment: Vec::new() };
    let mut skip = None;
    if !to_stdout {
        if let Ok(mut f) = File::open(&zpath) {
            let md = f.metadata().ok();
            if md.as_ref().is_some_and(|md| md.len() > 0) {
                match Archive::read(&mut f) {
                    Ok(a) => arch = a,
                    Err(ReadError::NoEnd) => {
                        m.warn("missing end signature--probably not a zip file (did you", "");
                        m.warn("remember to use binary mode when you transferred it?)", "");
                        m.warn("(if you are trying to read a damaged archive try -F)", "");
                        return Err(ZipErr(ZE_FORM, zname_shown));
                    }
                    Err(ReadError::Bad) => return Err(ZipErr(ZE_FORM, zname_shown)),
                    Err(ReadError::Io(e)) => return Err(ZipErr(ZE_READ, format!("{zname_shown}: {}", crate::common::errstr(&e)))),
                }
            }
            skip = md.map(|md| (md.dev(), md.ino()));
            zin = Some(f);
        }
    }

    // The names to work on
    let mut files = p.files;
    if o.names_stdin {
        let stdin = io::stdin();
        for line in stdin.lock().split(b'\n').map_while(Result::ok) {
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            if !line.is_empty() {
                files.push(OsString::from_vec(line));
            }
        }
    }
    let mut unmatched: Vec<OsString> = Vec::new();
    let mut scan = Scan { o: &o, found: Vec::new(), names: HashMap::new(), skip, dup: None };
    let mut delete = vec![false; arch.entries.len()];
    if o.mode == Mode::Delete {
        for a in &files {
            let pat = a.as_bytes();
            let mut hit = false;
            for (k, e) in arch.entries.iter().enumerate() {
                if pattern::matches(pat, &e.name, o.pat) && selected(&o, &e.name, false, None) {
                    delete[k] = true;
                    hit = true;
                }
            }
            if !hit {
                unmatched.push(a.clone());
            }
        }
    } else if o.recurse_patterns {
        let pats: Vec<Vec<u8>> = files.iter().map(|f| f.as_bytes().to_vec()).collect();
        let mut hits = vec![false; pats.len()];
        scan.recurse_patterns(m, Path::new("."), &pats, &mut hits)?;
        for (k, h) in hits.iter().enumerate() {
            if !h {
                unmatched.push(files[k].clone());
            }
        }
    } else {
        for a in &files {
            if !scan.procname(m, Path::new(a), o.recurse)? {
                m.warn("name not matched: ", &a.to_string_lossy());
                unmatched.push(a.clone());
            }
        }
    }
    if o.mode == Mode::Delete || o.recurse_patterns {
        for u in &unmatched {
            m.warn("name not matched: ", &u.to_string_lossy());
        }
    }
    if let Some((a, b, zname)) = scan.dup.take() {
        if !m.quiet {
            if m.started {
                m.out("\n");
            }
            m.out(&format!(
                "\tzip warning:   first full name: {}\n                      second full name: {}\n                     name in zip file repeated: {}\n                     this may be a result of using -j\n",
                a.display(),
                b.display(),
                s(&zname)
            ));
        }
        return Err(parms("cannot repeat names in zip file"));
    }
    if o.must_match && !unmatched.is_empty() {
        let e = ZipErr(ZE_OPEN, unmatched[0].to_string_lossy().into_owned());
        m.error(&e, Some(&io::Error::from_raw_os_error(libc::ENOENT)));
        return Ok(e.0);
    }
    let found = std::mem::take(&mut scan.found);
    drop(scan);

    // Match the found names with the archive's entries
    let index: HashMap<&[u8], usize> = arch.entries.iter().enumerate().map(|(k, e)| (e.name.as_slice(), k)).collect();
    let mut replace: Vec<Option<usize>> = vec![None; arch.entries.len()]; // entry -> found
    let mut new: Vec<usize> = Vec::new();
    let mut any_found = false;
    for (fi, f) in found.iter().enumerate() {
        any_found = true;
        match index.get(f.zname.as_slice()) {
            Some(&k) => {
                let e = &arch.entries[k];
                let ftime = f.meta.as_ref().map(|md| archive::dos_time(md.mtime())).unwrap_or(0);
                let fsize = f.meta.as_ref().map(|md| if f.is_dir { 0 } else { md.len() }).unwrap_or(0);
                let take = match o.mode {
                    Mode::Add => true,
                    Mode::Update | Mode::Freshen => f.stdin || ftime > e.dostime,
                    Mode::FileSync => ftime != e.dostime || fsize != e.usize,
                    Mode::Delete => false,
                };
                if take {
                    replace[k] = Some(fi);
                }
            }
            None => {
                if o.mode != Mode::Freshen {
                    new.push(fi);
                }
            }
        }
    }
    if o.mode == Mode::FileSync {
        let names: std::collections::HashSet<&[u8]> = found.iter().map(|f| f.zname.as_slice()).collect();
        for (k, e) in arch.entries.iter().enumerate() {
            if !names.contains(e.name.as_slice()) {
                delete[k] = true;
            }
        }
    }
    let marked = replace.iter().filter(|r| r.is_some()).count() + delete.iter().filter(|&&d| d).count();

    if o.show_files {
        let listing_all = files.is_empty() && !o.recurse_patterns;
        if listing_all {
            m.out("Archive contains:\n");
        } else if o.mode == Mode::Delete {
            m.out("Would Delete:\n");
        } else if o.mode == Mode::Freshen {
            m.out("Would Freshen:\n");
        } else {
            m.out("Would Add/Update:\n");
        }
        let (mut count, mut bytes) = (0u64, 0u64);
        for (k, e) in arch.entries.iter().enumerate() {
            if listing_all || replace[k].is_some() || delete[k] {
                count += 1;
                bytes += e.usize;
                m.out(&format!("  {}\n", s(&e.name)));
            }
        }
        for &fi in &new {
            let f = &found[fi];
            count += 1;
            bytes += if f.is_dir { 0 } else { f.meta.as_ref().map(|md| md.len()).unwrap_or(0) };
            m.out(&format!("  {}\n", s(&f.zname)));
        }
        m.out(&format!("Total {count} entries ({bytes} bytes)\n"));
        return Ok(ZE_OK);
    }

    let editing = !arch.entries.is_empty() && (o.latest || o.zip_comment || o.entry_comments);
    let allow_empty = arch.entries.is_empty() && zin.is_none() && !o.includes.is_empty() && o.mode != Mode::Delete;
    if marked == 0 && new.is_empty() && !allow_empty {
        if o.test && zin.is_some() && !arch.entries.is_empty() {
            return test_archive(&o, m, &zpath).map(|_| ZE_OK);
        }
        if o.mode == Mode::FileSync && any_found {
            m.out("Archive is current\n");
            return Ok(ZE_OK);
        }
        if !editing {
            if any_found && matches!(o.mode, Mode::Update | Mode::Freshen) {
                return Ok(ZE_NONE);
            }
            if o.recurse && !files.is_empty() {
                let mut hint = String::from("try: zip");
                for a in &p.args_before_files {
                    hint.push(' ');
                    hint.push_str(a);
                }
                hint.push_str(" . -i");
                for a in &files {
                    hint.push(' ');
                    hint.push_str(&a.to_string_lossy());
                }
                return Err(ZipErr(ZE_NONE, hint));
            }
            return Err(ZipErr(ZE_NONE, zname_shown));
        }
    }

    // The password
    let password = if o.encrypt {
        match o.password.take() {
            Some(p) => Some(p),
            None => {
                let a = get_password("Enter password: ").map_err(|_| parms("could not read password"))?;
                let b = get_password("Verify password: ").map_err(|_| parms("could not read password"))?;
                if a != b {
                    return Err(parms("password verification failed"));
                }
                Some(a)
            }
        }
    } else {
        None
    };

    // Where to write: in place (-g), standard output, or a temporary file renamed at the end
    let out_path = o.out_file.as_deref().map(zip_path).unwrap_or_else(|| zpath.clone());
    let grow = o.grow && zin.is_some() && out_path == zpath && !arch.entries.is_empty() && marked == 0;
    let mut temp: Option<PathBuf> = None;
    let mut out = if to_stdout {
        let f = unsafe { <File as std::os::unix::io::FromRawFd>::from_raw_fd(1) };
        Out::new(f, 0)
    } else if grow {
        let f = OpenOptions::new().read(true).write(true).open(&zpath).map_err(|e| ZipErr(ZE_CREAT, format!("{zname_shown}: {}", crate::common::errstr(&e))))?;
        let mut o2 = Out::new(f, 0);
        // New entries go where the central directory was
        let start = central_start(zin.as_mut().unwrap()).map_err(|e| ZipErr(ZE_READ, crate::common::errstr(&e)))?;
        o2.seek(start).map_err(io_write)?;
        o2
    } else {
        let dir = o.temp_dir.clone().unwrap_or_else(|| out_path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")));
        let (f, path) = make_temp(&dir).map_err(|e| ZipErr(ZE_TEMP, format!("{}: {}", dir.display(), crate::common::errstr(&e))))?;
        temp = Some(path);
        Out::new(f, 0)
    };
    let cleanup = |temp: &Option<PathBuf>| {
        if let Some(t) = temp {
            let _ = fs::remove_file(t);
        }
    };

    let mut ctx = Ctx { o: &o, m, password, total_in: 0, total_out: 0, bad_files: 0, bad_bytes: 0, good_files: 0, good_bytes: 0 };
    let mut central: Vec<(Entry, u64)> = Vec::new();
    let mut moved: Vec<&Found> = Vec::new();
    let result: Res<()> = (|| {
        for (k, e) in arch.entries.iter().enumerate() {
            if delete[k] {
                if !ctx.o.quiet {
                    ctx.m.out(&format!("deleting: {}\n", s(&e.name)));
                }
                continue;
            }
            if let Some(fi) = replace[k] {
                if let Some(w) = ctx.zipup(&mut out, &found[fi], true)? {
                    let mut ent = w.entry;
                    ent.comment = e.comment.clone();
                    central.push((ent, w.offset));
                    moved.push(&found[fi]);
                } else {
                    central.push((e.clone(), e.offset));
                }
                continue;
            }
            // Kept: copied as it is (in place when growing)
            if grow {
                central.push((e.clone(), e.offset));
            } else {
                let src = zin.as_mut().unwrap();
                let span = archive::local_span(src, e).map_err(|_| ZipErr(ZE_FORM, zname_shown.clone()))?;
                let at = out.pos;
                copy_range(src, e.offset, span, &mut out)?;
                central.push((e.clone(), at));
            }
        }
        for &fi in &new {
            if let Some(w) = ctx.zipup(&mut out, &found[fi], false)? {
                central.push((w.entry, w.offset));
                moved.push(&found[fi]);
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        cleanup(&temp);
        return Err(e);
    }
    let Ctx { m, total_in, total_out, bad_files, bad_bytes, good_files, good_bytes, .. } = ctx;

    // Comments
    let mut comment = arch.comment.clone();
    if o.entry_comments {
        let stdin = io::stdin();
        let mut lines = stdin.lock();
        for (ent, _) in central.iter_mut() {
            if !found.iter().any(|f| f.zname == ent.name) {
                continue;
            }
            m.out(&format!("Enter comment for {}:\n", s(&ent.name)));
            m.flush();
            let mut line = Vec::new();
            if lines.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if !line.is_empty() {
                    ent.comment = line;
                }
            }
        }
    }
    if o.zip_comment {
        m.out("enter new zip file comment (end with .):\n");
        m.flush();
        let stdin = io::stdin();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        for line in stdin.lock().split(b'\n').map_while(Result::ok) {
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            if line == b"." {
                break;
            }
            parts.push(line);
        }
        comment = parts.join(&b"\r\n"[..]);
    }

    // The central directory and the end records
    let cd_start = out.pos;
    let mut cd_len = 0u64;
    let mut latest = 0u32;
    for (e, off) in &central {
        let h = archive::central_header(e, *off);
        cd_len += h.len() as u64;
        latest = latest.max(e.dostime);
        if let Err(err) = out.write(&h) {
            cleanup(&temp);
            return Err(io_write(err));
        }
    }
    let end = archive::end_records(central.len() as u64, cd_len, cd_start, &comment, o.force_zip64);
    let finish = out.write(&end).and_then(|_| out.f.flush());
    if let Err(err) = finish {
        cleanup(&temp);
        return Err(io_write(err));
    }
    if grow {
        let len = out.pos;
        let _ = out.f.get_mut().set_len(len);
    }
    let out_file = out.f.into_inner().map_err(|e| io_write(e.into_error()))?;
    if to_stdout {
        std::mem::forget(out_file);
    } else {
        drop(out_file);
    }

    if central.is_empty() && !to_stdout {
        m.warn("zip file empty", "");
    }
    if o.verbose && !o.quiet && total_in > 0 {
        m.out(&format!("total bytes={total_in}, compressed={total_out} -> {}% savings\n", percent(total_in, total_out)));
    }

    // -T: the new archive is checked before it replaces the old one
    if o.test && !to_stdout {
        let check = temp.clone().unwrap_or_else(|| zpath.clone());
        if test_archive_as(&o, m, &check, &out_path).is_err() {
            cleanup(&temp);
            return Ok(ZE_TEST);
        }
    }
    if let Some(t) = &temp {
        if fs::rename(t, &out_path).is_err() {
            let copied = fs::copy(t, &out_path);
            let _ = fs::remove_file(t);
            copied.map_err(|e| ZipErr(ZE_CREAT, format!("{}: {}", out_path.display(), crate::common::errstr(&e))))?;
        }
    }
    if o.latest && !to_stdout && latest != 0 {
        let t = archive::unix_time(latest);
        let ts = [crate::common::timespec(t, 0), crate::common::timespec(t, 0)];
        let c = std::ffi::CString::new(out_path.as_os_str().as_bytes()).unwrap();
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), 0) };
    }

    // -m: the originals go, files first, then the directories that are left empty
    if o.move_files {
        for f in moved.iter().filter(|f| !f.is_dir && !f.stdin) {
            let _ = fs::remove_file(&f.path);
        }
        let mut dirs: Vec<&&Found> = moved.iter().filter(|f| f.is_dir).collect();
        dirs.sort_by_key(|f| std::cmp::Reverse(f.path.as_os_str().len()));
        for d in dirs {
            let _ = fs::remove_dir(&d.path);
        }
    }

    if bad_files > 0 {
        m.out("\nzip warning: Not all files were readable\n");
        m.out(&format!(
            "  files/entries read:  {} ({} bytes)  skipped:  {} ({} bytes)\n",
            good_files,
            num_string(good_bytes),
            bad_files,
            num_string(bad_bytes)
        ));
        return Ok(ZE_OPEN);
    }
    Ok(ZE_OK)
}

/// Where an archive's central directory starts.
fn central_start(f: &mut File) -> io::Result<u64> {
    let len = f.seek(SeekFrom::End(0))?;
    let n = len.min(22 + 65535) as usize;
    let mut tail = vec![0; n];
    f.seek(SeekFrom::Start(len - n as u64))?;
    f.read_exact(&mut tail)?;
    let e = (0..=n - 22).rev().find(|&i| archive::le32(&tail, i) == archive::END_SIG).unwrap_or(0);
    let mut start = archive::le32(&tail, e + 16) as u64;
    let end_pos = len - n as u64 + e as u64;
    if end_pos >= 20 && e >= 20 && archive::le32(&tail, e - 20) == archive::LOC64_SIG {
        let at = archive::le64(&tail, e - 12);
        let mut rec = [0u8; 56];
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut rec)?;
        start = archive::le64(&rec, 48);
    }
    Ok(start)
}

fn copy_range(src: &mut File, at: u64, len: u64, out: &mut Out) -> Res<()> {
    src.seek(SeekFrom::Start(at)).map_err(|e| ZipErr(ZE_READ, crate::common::errstr(&e)))?;
    let mut left = len;
    let mut buf = vec![0u8; 1 << 16];
    while left > 0 {
        let n = (left as usize).min(buf.len());
        src.read_exact(&mut buf[..n]).map_err(|e| ZipErr(ZE_READ, crate::common::errstr(&e)))?;
        out.write(&buf[..n]).map_err(io_write)?;
        left -= n as u64;
    }
    Ok(())
}

/// A new file "ziXXXXXX" in dir, as tempname() names it.
fn make_temp(dir: &Path) -> io::Result<(File, PathBuf)> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = now() as u64 ^ (std::process::id() as u64).rotate_left(17);
    for _ in 0..100 {
        let mut name = b"zi".to_vec();
        for _ in 0..6 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            name.push(CHARS[(seed >> 33) as usize % CHARS.len()]);
        }
        let path = dir.join(OsStr::from_bytes(&name));
        match OpenOptions::new().read(true).write(true).create_new(true).mode(0o666).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::from_raw_os_error(libc::EEXIST))
}

fn test_archive(o: &Opts, m: &mut Mesg, path: &Path) -> Res<()> {
    test_archive_as(o, m, path, path).map_err(|_| ZipErr(ZE_TEST, "original files unmodified".into()))
}

/// -T: every entry read back and its CRC compared (or -TT's command run on it); "test of NAME OK".
fn test_archive_as(o: &Opts, m: &mut Mesg, path: &Path, shown: &Path) -> Result<(), ()> {
    let ok = if let Some(cmd) = &o.unzip_cmd {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{cmd} \"$1\""))
            .arg("sh")
            .arg(path)
            .status()
            .is_ok_and(|st| st.success())
    } else {
        verify(path, o.password.as_deref()).is_ok()
    };
    if ok {
        if !o.quiet {
            m.out(&format!("test of {} OK\n", shown.display()));
        }
        Ok(())
    } else {
        m.out(&format!("test of {} FAILED\n", shown.display()));
        m.error(&ZipErr(ZE_TEST, "original files unmodified".into()), None);
        Err(())
    }
}

fn verify(path: &Path, password: Option<&[u8]>) -> Result<(), String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    let mut z = ::zip::ZipArchive::new(io::BufReader::new(f)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 16];
    for i in 0..z.len() {
        let mut ent = match password {
            Some(p) if z.by_index_raw(i).map(|e| e.encrypted()).unwrap_or(false) => z.by_index_decrypt(i, p).map_err(|e| e.to_string())?,
            _ => z.by_index(i).map_err(|e| e.to_string())?,
        };
        loop {
            match ent.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    Ok(())
}
