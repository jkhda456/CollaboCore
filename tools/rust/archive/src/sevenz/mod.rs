//! 7z (also as 7za, 7zr, 7zz): the 7-Zip command line (26.04) over the formats in formats.rs.
//!
//! Commands a, u, d, e, x, l, t, h, rn and i with 7-Zip's switch syntax and messages; exit codes
//! 0 (ok), 1 (warning), 2 (fatal error), 7 (command line error), 255 (stopped). Left out: b
//! (benchmark), SFX, volumes (-v), e-mail (-seml), NTFS streams/security (Windows only anyway).
//! An update re-encodes the items it keeps (7-Zip copies their packed data): same contents,
//! slower on big archives.

mod formats;
mod scan;

use crate::common::errstr;
use formats::{Data, DataError, Fmt, Item, Methods, NewItem, OpenError, Opened, Source};
use scan::{Pattern, Recurse};
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const VERSION: &str = "26.04";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cmd {
    Add,
    Update,
    Delete,
    Extract,
    ExtractFull,
    List,
    Test,
    Hash,
    Info,
    Rename,
    Bench,
}

struct Opts {
    cmd: Cmd,
    archive: Option<String>,
    names: Vec<String>,
    out_dir: Option<String>,
    password: Option<String>,
    yes: bool,
    recurse: Recurse,
    arc_type: Option<String>,
    methods: Vec<String>,
    overwrite: Option<char>,
    includes: Vec<Pattern>,
    excludes: Vec<Pattern>,
    stdout: bool,
    stdin: Option<String>,
    sdel: bool,
    slt: bool,
    ba: bool,
    bb: u32,
    full_paths: bool,
    spe: bool,
    case: bool,
    snl: bool,
    stl: bool,
    sse: bool,
    update_actions: Option<[u8; 7]>,
    hash_methods: Vec<String>,
}

/// Where messages go: stdout normally; stderr when stdout carries data (-so).
struct Out {
    so: Box<dyn Write>,
    bannered: bool,
    /// Errors in switches come before the banner; in the command, after it.
    switch_stage: bool,
}

macro_rules! say {
    ($o:expr, $($arg:tt)*) => { { let _ = write!($o.so, $($arg)*); } };
}

fn banner(o: &mut Out) {
    o.bannered = true;
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "C".into());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    say!(o, "\n7-Zip (collaboCore) {VERSION} : a Rust reimplementation of the 7-Zip command line\n {}-bit locale={} Threads:{}\n\n", usize::BITS, locale, threads);
}

fn size_smart(v: u64) -> String {
    let mut s = format!("{v} bytes");
    if v == 0 {
        return s;
    }
    let (bits, c) = if v >= 10 << 30 {
        (30, 'G')
    } else if v >= 10 << 20 {
        (20, 'M')
    } else {
        (10, 'K')
    };
    s += &format!(" ({} {}iB)", (v + (1u64 << bits) - 1) >> bits, c);
    s
}

fn dir_stat(dirs: u64, files: u64, size: Option<u64>) -> String {
    let mut s = String::new();
    if dirs != 0 {
        s += &format!("{} {}, ", dirs, if dirs == 1 { "folder" } else { "folders" });
    }
    s += &format!("{} {}", files, if files == 1 { "file" } else { "files" });
    if let Some(sz) = size {
        s += &format!(", {}", size_smart(sz));
    }
    s
}

fn usage(o: &mut Out) {
    say!(
        o,
        "{}",
        r#"Usage: 7z <command> [<switches>...] <archive_name> [<file_names>...] [@listfile]

<Commands>
  a : Add files to archive
  b : Benchmark
  d : Delete files from archive
  e : Extract files from archive (without using directory names)
  h : Calculate hash values for files
  i : Show information about supported formats
  l : List contents of archive
  rn : Rename files in archive
  t : Test integrity of archive
  u : Update files to archive
  x : eXtract files with full paths

<Switches>
  -- : Stop switches and @listfile parsing
  -ai[r[-|0]][m[-|2]][w[-]]{@listfile|!wildcard} : Include archives
  -ax[r[-|0]][m[-|2]][w[-]]{@listfile|!wildcard} : eXclude archives
  -ao{a|s|t|u} : set Overwrite mode
  -an : disable archive_name field
  -bb[0-3] : set output log level
  -bd : disable progress indicator
  -bs{o|e|p}{0|1|2} : set output stream for output/error/progress line
  -bt : show execution time statistics
  -i[r[-|0]][m[-|2]][w[-]]{@listfile|!wildcard} : Include filenames
  -m{Parameters} : set compression Method
    -mmt[N] : set number of CPU threads
    -mx[N] : set compression level: -mx1 (fastest) ... -mx9 (ultra)
  -o{Directory} : set Output directory
  -p{Password} : set Password
  -r[-|0] : Recurse subdirectories for name search
  -sa{a|e|s} : set Archive name mode
  -scc{UTF-8|WIN|DOS} : set charset for console input/output
  -scs{UTF-8|UTF-16LE|UTF-16BE|WIN|DOS|{id}} : set charset for list files
  -scrc[CRC32|CRC64|SHA1|SHA256|SHA512|MD5|*] : set hash function for x, e, h commands
  -sdel : delete files after compression
  -si[{name}] : read data from stdin
  -slt : show technical information for l (List) command
  -snl : store symbolic links as links
  -so : write data to stdout
  -spd : disable wildcard matching for file names
  -spe : eliminate duplication of root folder for extract command
  -spf[2] : use fully qualified file paths
  -ssc[-] : set sensitive case mode
  -sse : stop archive creating, if it can't open some input file
  -stl : set archive timestamp from the most recently modified file
  -t{Type} : Set type of archive
  -u[-][p#][q#][r#][x#][y#][z#][!newArchiveName] : Update options
  -w[{path}] : assign Work directory. Empty path means a temporary directory
  -x[r[-|0]][m[-|2]][w[-]]{@listfile|!wildcard} : eXclude filenames
  -y : assume Yes on all queries
"#
    );
}

/// The switch names, longest first where one is a prefix of another (7-Zip's parser picks the
/// longest name that matches).
const SWITCHES: &[&str] = &[
    "?", "h", "-help", "ba", "bd", "bt", "bb", "bso", "bse", "bsp", "y", "ad", "ao", "t", "stx", "m", "o", "w", "i", "x", "ai", "ax", "an", "u", "v",
    "r", "stm", "sfx", "seml", "scrc", "shd", "smemx", "si", "so", "slp", "scs", "scc", "slt", "slf", "slsl", "slmu", "ssp", "ssw", "sse", "ssc",
    "sa", "spm", "spd", "spe", "spf", "spo", "snh", "snld", "snl", "sni", "snoi", "snon", "snz", "sns", "snr", "snc", "snt", "sdel", "stl", "p",
];

fn cmd_error(o: &mut Out, msg: &str) -> i32 {
    if !o.bannered && !o.switch_stage {
        banner(o);
    }
    let _ = o.so.flush();
    let _ = writeln!(io::stderr(), "\n\nCommand Line Error:\n{msg}");
    7
}

pub fn main(_argv0: &str, args: Vec<String>) -> i32 {
    let mut o = Out { so: Box::new(io::stdout()), bannered: false, switch_stage: true };
    let mut opts = Opts {
        cmd: Cmd::List,
        archive: None,
        names: vec![],
        out_dir: None,
        password: None,
        yes: false,
        recurse: Recurse::Default,
        arc_type: None,
        methods: vec![],
        overwrite: None,
        includes: vec![],
        excludes: vec![],
        stdout: false,
        stdin: None,
        sdel: false,
        slt: false,
        ba: false,
        bb: 0,
        full_paths: false,
        spe: false,
        case: true,
        snl: false,
        stl: false,
        sse: false,
        update_actions: None,
        hash_methods: vec![],
    };
    // Switches anywhere before "--"; the first other word is the command, then the archive.
    let mut words = vec![];
    let mut raw_includes = vec![];
    let mut raw_excludes = vec![];
    let mut stop = false;
    for a in &args {
        if stop || !a.starts_with('-') || a == "-" {
            words.push(a.clone());
            continue;
        }
        if a == "--" {
            stop = true;
            continue;
        }
        let body = &a[1..];
        let lower = body.to_ascii_lowercase();
        let Some(name) = SWITCHES.iter().filter(|s| lower.starts_with(*s)).max_by_key(|s| s.len()) else {
            return cmd_error(&mut o, &format!("Unknown switch:\n{a}"));
        };
        let rest = &body[name.len()..];
        match *name {
            "?" | "h" | "-help" => {
                banner(&mut o);
                usage(&mut o);
                return 0;
            }
            "ba" => opts.ba = true,
            "bd" | "bt" | "bso" | "bse" | "bsp" | "ad" | "stx" | "w" | "an" | "stm" | "shd" | "smemx" | "slp" | "scs" | "scc" | "slf" | "slsl" | "slmu" | "ssp"
            | "ssw" | "sa" | "spm" | "spd" | "spo" | "snh" | "snld" | "sni" | "snoi" | "snon" | "snz" | "sns" | "snr" | "snc" | "snt" | "ai" | "ax" => {}
            "bb" => opts.bb = rest.parse().unwrap_or(1),
            "y" => opts.yes = true,
            "ao" => match rest.chars().next() {
                Some(c @ ('a' | 's' | 'u' | 't')) if rest.len() == 1 => opts.overwrite = Some(c),
                _ => return cmd_error(&mut o, &format!("Unsupported switch postfix -ao\n{a}")),
            },
            "t" => opts.arc_type = Some(rest.to_string()),
            "m" => opts.methods.push(rest.to_string()),
            "o" => {
                if rest.is_empty() {
                    return cmd_error(&mut o, &format!("Too short switch:\n{a}"));
                }
                opts.out_dir = Some(rest.to_string());
            }
            "i" => raw_includes.push(rest.to_string()),
            "x" => raw_excludes.push(rest.to_string()),
            "u" => {
                let mut acts = if opts.cmd == Cmd::Update { [1, 1, 2, 1, 2, 1, 2] } else { [1, 1, 2, 2, 2, 2, 2] };
                let mut s = rest;
                if let Some(r) = s.strip_prefix('-') {
                    s = r;
                }
                let mut cs = s.chars().peekable();
                while let Some(c) = cs.next() {
                    let Some(pos) = "pqrxyzw".find(c) else { return cmd_error(&mut o, &format!("Unsupported switch postfix -u\n{a}")) };
                    let Some(d) = cs.next().and_then(|d| d.to_digit(10)).filter(|d| *d <= 3) else {
                        return cmd_error(&mut o, &format!("Unsupported switch postfix -u\n{a}"));
                    };
                    acts[pos] = d as u8;
                }
                opts.update_actions = Some(acts);
            }
            "v" => return cmd_error(&mut o, "Volumes (-v) are not supported by this 7z"),
            "r" => {
                opts.recurse = match rest {
                    "" => Recurse::Yes,
                    "-" => Recurse::No,
                    "0" => Recurse::WildOnly,
                    _ => return cmd_error(&mut o, &format!("Unsupported switch postfix -r\n{a}")),
                }
            }
            "sfx" => return cmd_error(&mut o, "SFX archives (-sfx) are not supported by this 7z"),
            "seml" => return cmd_error(&mut o, "-seml is not supported by this 7z"),
            "scrc" => opts.hash_methods.push(if rest.is_empty() { "CRC32".into() } else { rest.to_string() }),
            "si" => opts.stdin = Some(rest.to_string()),
            "so" => opts.stdout = true,
            "slt" => opts.slt = true,
            "sse" => opts.sse = true,
            "ssc" => opts.case = rest != "-",
            "spe" => opts.spe = rest != "-",
            "spf" => opts.full_paths = true,
            "snl" => opts.snl = rest != "-",
            "sdel" => opts.sdel = true,
            "stl" => opts.stl = true,
            "p" => opts.password = Some(rest.to_string()),
            _ => return cmd_error(&mut o, &format!("Unknown switch:\n{a}")),
        }
    }
    o.switch_stage = false;
    if words.is_empty() {
        banner(&mut o);
        usage(&mut o);
        return 0;
    }
    opts.cmd = match words[0].to_ascii_lowercase().as_str() {
        "a" => Cmd::Add,
        "u" => Cmd::Update,
        "d" => Cmd::Delete,
        "e" => Cmd::Extract,
        "x" => Cmd::ExtractFull,
        "l" => Cmd::List,
        "t" => Cmd::Test,
        "h" => Cmd::Hash,
        "i" => Cmd::Info,
        "rn" => Cmd::Rename,
        "b" => Cmd::Bench,
        other => return cmd_error(&mut o, &format!("Unsupported command:\n{other}")),
    };
    // A -u given before the command was read with `a`'s defaults; `u` has its own.
    if opts.cmd == Cmd::Update {
        if let Some(acts) = opts.update_actions.as_mut() {
            let _ = acts;
        }
    }
    let default_rec = if opts.recurse == Recurse::Default { Recurse::Default } else { opts.recurse };
    for s in &raw_includes {
        match scan::parse_clude(s, default_rec) {
            Ok(p) => opts.includes.extend(p),
            Err(m) => return cmd_error(&mut o, &m),
        }
    }
    for s in &raw_excludes {
        match scan::parse_clude(s, default_rec) {
            Ok(p) => opts.excludes.extend(p),
            Err(m) => return cmd_error(&mut o, &m),
        }
    }
    let mut rest = words[1..].to_vec();
    let reads_stdin_archive = opts.stdin.is_some() && matches!(opts.cmd, Cmd::List | Cmd::Test | Cmd::Extract | Cmd::ExtractFull);
    if !matches!(opts.cmd, Cmd::Hash | Cmd::Info | Cmd::Bench) && !reads_stdin_archive {
        if rest.is_empty() {
            return cmd_error(&mut o, "Cannot find archive name");
        }
        opts.archive = Some(rest.remove(0));
    }
    // @listfile arguments (before "--").
    for w in rest {
        if let Some(f) = w.strip_prefix('@') {
            match std::fs::read_to_string(f) {
                Ok(t) => opts.names.extend(t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from)),
                Err(e) => return cmd_error(&mut o, &format!("Cannot open list file\n{f}\n{}", errstr(&e))),
            }
        } else {
            opts.names.push(w);
        }
    }
    if opts.stdout {
        o.so = Box::new(io::sink());
    }
    if !opts.ba {
        banner(&mut o);
    } else {
        o.bannered = true;
    }
    let code = match opts.cmd {
        Cmd::List | Cmd::Test | Cmd::Extract | Cmd::ExtractFull => read_cmd(&mut o, &opts),
        Cmd::Add | Cmd::Update | Cmd::Delete | Cmd::Rename => update_cmd(&mut o, &opts),
        Cmd::Hash => hash_cmd(&mut o, &opts),
        Cmd::Info => {
            say!(o, "Formats:\n");
            for f in ["7z", "zip", "tar", "gzip", "bzip2", "xz", "lzma", "zstd"] {
                let fm = Fmt::from_type(f).unwrap();
                say!(o, "  {:<6} {}\n", f, if fm.can_write() { "read, write" } else { "read" });
            }
            say!(o, "\nCodecs:\n  LZMA2 LZMA PPMD BZip2 Deflate Deflate64 Copy BCJ ARM ARMT ARM64 PPC IA64 SPARC RISCV Delta 7zAES\n\nHashers:\n  CRC32 CRC64 SHA1 SHA256 SHA512 MD5\n");
            0
        }
        Cmd::Bench => {
            let _ = writeln!(io::stderr(), "\nERROR: the benchmark (b) is not part of this 7z");
            2
        }
    };
    let _ = o.so.flush();
    code
}

fn fmt_time(t: Option<(i64, u32)>) -> String {
    match t {
        None => " ".repeat(19),
        Some((s, _)) => {
            let (y, mo, d, h, mi, se) = formats::unix_to_civil(s + formats::local_offset(s));
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{se:02}")
        }
    }
}

fn fmt_time_full(t: (i64, u32), frac: bool) -> String {
    let base = fmt_time(Some(t));
    if frac {
        format!("{}.{:07}", base, t.1 / 100)
    } else {
        base
    }
}

fn attr_letters(it: &Item) -> String {
    let a = it.attrib.unwrap_or(0) | if it.is_dir { 0x10 } else { 0 };
    let f = |bit: u32, c: char| if a & bit != 0 { c } else { '.' };
    format!("{}{}{}{}{}", f(0x10, 'D'), f(0x1, 'R'), f(0x2, 'H'), f(0x4, 'S'), f(0x20, 'A'))
}

fn mode_string(m: u32) -> String {
    let t = match m & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut s = String::from(t);
    let bits = [(0o400, 'r'), (0o200, 'w'), (0o100, 'x'), (0o040, 'r'), (0o020, 'w'), (0o010, 'x'), (0o004, 'r'), (0o002, 'w'), (0o001, 'x')];
    for (i, (b, c)) in bits.iter().enumerate() {
        let mut ch = if m & b != 0 { *c } else { '-' };
        if i == 2 && m & 0o4000 != 0 {
            ch = if ch == 'x' { 's' } else { 'S' };
        }
        if i == 5 && m & 0o2000 != 0 {
            ch = if ch == 'x' { 's' } else { 'S' };
        }
        if i == 8 && m & 0o1000 != 0 {
            ch = if ch == 'x' { 't' } else { 'T' };
        }
        s.push(ch);
    }
    s
}

fn arc_fmt_hint(opts: &Opts, name: &str) -> Result<Option<Fmt>, String> {
    if let Some(t) = &opts.arc_type {
        if t == "*" || t == "#" || t.is_empty() {
            return Ok(None);
        }
        return Fmt::from_type(t).map(Some).ok_or_else(|| format!("Unsupported archive type\n{t}"));
    }
    let _ = name;
    Ok(None)
}

fn open_msg(fmt: Option<Fmt>) -> String {
    match fmt {
        Some(f) => format!("Cannot open the file as [{}] archive", f.name()),
        None => "Cannot open the file as archive".into(),
    }
}

fn print_props(o: &mut Out, a: &Opened, path: &str) {
    say!(o, "--\nPath = {}\nType = {}\n", path, a.fmt.name());
    for (k, v) in &a.props {
        say!(o, "{k} = {v}\n");
    }
}

/// The archive's items a command acts on: all, or those its names (and -i/-x) select.
fn selected(opts: &Opts, it: &Item) -> bool {
    let rec = if opts.recurse == Recurse::Default { Recurse::Default } else { opts.recurse };
    let pats: Vec<Pattern> = opts.names.iter().map(|n| Pattern::new(n, rec)).chain(opts.includes.iter().cloned()).collect();
    if !pats.is_empty() && !pats.iter().any(|p| p.matches(&it.path, opts.case)) {
        return false;
    }
    !opts.excludes.iter().any(|p| p.matches(&it.path, opts.case))
}

fn ask_password() -> Option<String> {
    eprint!("\nEnter password:");
    let _ = io::stderr().flush();
    let mut s = String::new();
    match io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim_end_matches(['\n', '\r']).to_string()),
    }
}

fn break_signaled() -> i32 {
    let _ = writeln!(io::stderr(), "\n\nBreak signaled");
    255
}

// l, t, e, x

fn read_cmd(o: &mut Out, opts: &Opts) -> i32 {
    if opts.archive.is_none() {
        return read_stdin_archive(o, opts);
    }
    let arc = opts.archive.clone().unwrap();
    let hint = match arc_fmt_hint(opts, &arc) {
        Ok(h) => h,
        Err(m) => return cmd_error(o, &m),
    };
    let quiet = opts.ba && opts.cmd == Cmd::List;
    if !quiet {
        say!(o, "Scanning the drive for archives:\n");
    }
    let meta = match std::fs::metadata(&arc) {
        Ok(m) => m,
        Err(e) => {
            let _ = o.so.flush();
            let n = e.raw_os_error().unwrap_or(0);
            let _ = write!(io::stderr(), "\nERROR: errno={n} : {}\n{arc}\n\n\n\nSystem ERROR:\nerrno={n} : {}\n", errstr(&e), errstr(&e));
            return 2;
        }
    };
    let verb = match opts.cmd {
        Cmd::List => "Listing",
        Cmd::Test => "Testing",
        _ => "Extracting",
    };
    if !quiet {
        say!(o, "1 file, {}\n\n", size_smart(meta.len()));
        say!(o, "{} archive: {}\n", verb, arc);
    }
    let hint_ext = hint.or_else(|| Fmt::from_ext(&arc));
    let mut password = opts.password.clone();
    let listing = opts.cmd == Cmd::List;
    let open_failed = |o: &mut Out, err: String| -> i32 {
        let _ = o.so.flush();
        let _ = write!(io::stderr(), "{err}");
        if listing {
            say!(o, "\n\nErrors: 1\n");
        } else {
            say!(o, "\nCan't open as archive: 1\nFiles: 0\nSize:       0\nCompressed: 0\n");
        }
        2
    };
    let mut opened = loop {
        match formats::open(&arc, hint, password.as_deref()) {
            Ok(a) => break a,
            Err(OpenError::NeedPassword) if password.is_none() => {
                if opts.ba {
                    return 2;
                }
                let _ = o.so.flush();
                match ask_password() {
                    Some(p) => password = Some(p),
                    None => return break_signaled(),
                }
            }
            Err(OpenError::NeedPassword) | Err(OpenError::WrongPassword) => {
                let msg = if listing {
                    format!("\nERROR: {arc} : Cannot open encrypted archive. Wrong password?\n\nERRORS:\nHeaders Error\n\n")
                } else {
                    format!("ERROR: {arc}\nCannot open encrypted archive. Wrong password?\n\nERRORS:\nHeaders Error\n")
                };
                return open_failed(o, msg);
            }
            Err(OpenError::NotArchive) => {
                let msg = if listing {
                    format!("\nERROR: {arc} : {arc}\nOpen ERROR: {}\n\n\nERRORS:\nIs not archive\n\n", open_msg(hint_ext))
                } else {
                    format!("ERROR: {arc}\n{arc}\nOpen ERROR: {}\n\n\nERRORS:\nIs not archive\n", open_msg(hint_ext))
                };
                return open_failed(o, msg);
            }
            Err(OpenError::Headers(m)) => {
                let msg = if listing {
                    format!("\nERROR: {arc} : {arc}\nOpen ERROR: {}\n\n\nERRORS:\nHeaders Error\n{m}\n\n", open_msg(hint_ext))
                } else {
                    format!("ERROR: {arc}\n{arc}\nOpen ERROR: {}\n\n\nERRORS:\nHeaders Error\n{m}\n", open_msg(hint_ext))
                };
                return open_failed(o, msg);
            }
            Err(OpenError::Io(e)) => {
                return open_failed(o, format!("ERROR: {} : {}\n", arc, errstr(&e)));
            }
        }
    };
    if opts.cmd == Cmd::List && !quiet {
        say!(o, "\n");
    }
    if !quiet {
        print_props(o, &opened, &arc);
    }
    match opts.cmd {
        Cmd::List => list(o, opts, &opened),
        _ => extract(o, opts, &mut opened, password.as_deref(), meta.len()),
    }
}

/// x/l/t -si: the archive comes on stdin (its type given with -t); it is read into a temporary
/// file first, and shown as 7-Zip shows a stream: no path.
fn read_stdin_archive(o: &mut Out, opts: &Opts) -> i32 {
    let hint = match arc_fmt_hint(opts, "") {
        Ok(Some(h)) => h,
        Ok(None) => {
            say!(o, "\nListing archive: \n\n");
            let _ = o.so.flush();
            let _ = write!(io::stderr(), "\nERROR:  : opening : E_NOTIMPL : Not implemented\n");
            say!(o, "\nErrors: 1\n");
            let _ = write!(io::stderr(), "\n\nSystem ERROR:\nE_NOTIMPL : Not implemented\n");
            return 2;
        }
        Err(m) => return cmd_error(o, &m),
    };
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let tmp = format!("{dir}/collabo-7z-stdin{}", std::process::id());
    let r = File::create(&tmp).and_then(|mut f| io::copy(&mut io::stdin().lock(), &mut f));
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        let _ = writeln!(io::stderr(), "ERROR: {}", errstr(&e));
        return 2;
    }
    let verb = match opts.cmd {
        Cmd::List => "Listing",
        Cmd::Test => "Testing",
        _ => "Extracting",
    };
    say!(o, "\n{verb} archive: \n");
    let code = match formats::open(&tmp, Some(hint), opts.password.as_deref()) {
        Ok(mut a) => {
            if a.fmt.single_file() && a.items.len() == 1 && a.items[0].path == formats_single_name(&tmp) {
                a.items[0].path = "~".into();
            }
            if opts.cmd == Cmd::List {
                say!(o, "\n");
            }
            print_props(o, &a, "");
            let phys = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            if opts.cmd == Cmd::List {
                list(o, opts, &a)
            } else {
                extract(o, opts, &mut a, opts.password.as_deref(), phys)
            }
        }
        Err(_) => {
            let _ = o.so.flush();
            let _ = write!(io::stderr(), "ERROR: \nOpen ERROR: {}\n", open_msg(Some(hint)));
            2
        }
    };
    let _ = std::fs::remove_file(&tmp);
    code
}

fn list(o: &mut Out, opts: &Opts, a: &Opened) -> i32 {
    let items: Vec<&Item> = a.items.iter().filter(|it| selected(opts, it)).collect();
    if opts.slt {
        if !opts.ba {
            say!(o, "\n----------\n");
        }
        for it in &items {
            say!(o, "Path = {}\n", it.path);
            if a.fmt == Fmt::Zip || a.fmt == Fmt::Tar {
                say!(o, "Folder = {}\n", if it.is_dir { "+" } else { "-" });
            }
            say!(o, "Size = {}\n", it.size.map(|s| s.to_string()).unwrap_or_default());
            say!(o, "Packed Size = {}\n", it.packed.map(|s| s.to_string()).unwrap_or_default());
            let frac = a.fmt == Fmt::SevenZ;
            say!(o, "Modified = {}\n", it.mtime.map(|t| fmt_time_full(t, frac)).unwrap_or_default());
            if let Some(c) = it.ctime {
                say!(o, "Created = {}\n", fmt_time_full(c, frac));
            }
            if let Some(c) = it.atime {
                say!(o, "Accessed = {}\n", fmt_time_full(c, frac));
            }
            match a.fmt {
                Fmt::SevenZ | Fmt::Zip => {
                    let mut attr = String::new();
                    let al = attr_letters(it).replace('.', "");
                    attr += &al;
                    if let Some(m) = it.unix_mode() {
                        let m = if m & 0o170000 == 0 { m | if it.is_dir { 0o040000 } else { 0o100000 } } else { m };
                        if !attr.is_empty() {
                            attr.push(' ');
                        }
                        attr += &mode_string(m);
                    }
                    say!(o, "Attributes = {attr}\n");
                }
                Fmt::Tar => {
                    say!(o, "Mode = {}\n", it.unix_mode().map(mode_string).unwrap_or_default());
                    say!(o, "User = {}\nGroup = {}\n", it.user.clone().unwrap_or_default(), it.group.clone().unwrap_or_default());
                    say!(o, "Symbolic Link = {}\n", it.link.clone().unwrap_or_default());
                }
                _ => {}
            }
            if a.fmt != Fmt::Tar && !a.fmt.single_file() {
                say!(o, "CRC = {}\n", it.crc.map(|c| format!("{c:08X}")).unwrap_or_default());
                say!(o, "Encrypted = {}\n", if it.encrypted { "+" } else { "-" });
                say!(o, "Method = {}\n", it.method.clone().unwrap_or_default());
            }
            if a.fmt == Fmt::SevenZ {
                say!(o, "Block = {}\n", it.block.map(|b| b.to_string()).unwrap_or_default());
            }
            say!(o, "\n");
        }
        return 0;
    }
    if !opts.ba {
        say!(o, "\n   Date      Time    Attr         Size   Compressed  Name\n------------------- ----- ------------ ------------  ------------------------\n");
    }
    let (mut files, mut dirs) = (0u64, 0u64);
    let (mut size, mut packed): (Option<u64>, Option<u64>) = (None, None);
    let mut newest: Option<(i64, u32)> = None;
    for it in &items {
        say!(
            o,
            "{} {} {:>12} {:>12}  {}\n",
            fmt_time(it.mtime),
            attr_letters(it),
            it.size.map(|s| s.to_string()).unwrap_or_default(),
            it.packed.map(|s| s.to_string()).unwrap_or_default(),
            it.path
        );
        if it.is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
        if let Some(v) = it.size {
            size = Some(size.unwrap_or(0) + v);
        }
        if let Some(v) = it.packed {
            packed = Some(packed.unwrap_or(0) + v);
        }
        if let Some(t) = it.mtime {
            if newest.map_or(true, |n| t > n) {
                newest = Some(t);
            }
        }
    }
    // As 7-Zip: with no packed sizes known, the archive's size stands for them (0 when nothing
    // has data); sizes are 0 when there are no files at all.
    if packed.is_none() {
        let streams = items.iter().filter(|i| !i.is_dir).count();
        packed = Some(if streams == 0 { 0 } else { std::fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0) });
    }
    if files == 0 {
        size = size.or(Some(0));
    }
    if !opts.ba {
        let mut tail = format!("{files} files");
        if dirs > 0 {
            tail += &format!(", {dirs} folders");
        }
        say!(
            o,
            "------------------- ----- ------------ ------------  ------------------------\n{}       {:>12} {:>12}  {}\n",
            fmt_time(newest),
            size.map(|v| v.to_string()).unwrap_or_default(),
            packed.map(|v| v.to_string()).unwrap_or_default(),
            tail
        );
    }
    0
}

/// The overwrite question, as 7-Zip asks it (on stdout, answer from stdin).
fn ask_overwrite(o: &mut Out, existing: &Path, it: &Item) -> char {
    let m = std::fs::metadata(existing).ok();
    say!(
        o,
        "\nWould you like to replace the existing file:\n  Path:     {}\n  Size:     {}\n  Modified: {}\nwith the file from archive:\n  Path:     {}\n  Size:     {}\n  Modified: {}\n? (Y)es / (N)o / (A)lways / (S)kip all / A(u)to rename all / (Q)uit? ",
        existing.display(),
        m.as_ref().map(|m| size_smart(m.len())).unwrap_or_default(),
        fmt_time(m.as_ref().map(|m| (m.mtime(), 0))),
        it.path,
        it.size.map(size_smart).unwrap_or_default(),
        fmt_time(it.mtime)
    );
    let _ = o.so.flush();
    let mut s = String::new();
    match io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => 'q',
        Ok(_) => s.trim().chars().next().map(|c| c.to_ascii_lowercase()).unwrap_or('n'),
    }
}

fn auto_rename(p: &Path) -> PathBuf {
    let parent = p.parent().unwrap_or(Path::new(""));
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    for n in 1.. {
        let c = parent.join(format!("{stem}_{n}{ext}"));
        if std::fs::symlink_metadata(&c).is_err() {
            return c;
        }
    }
    unreachable!()
}

/// A path from the archive made safe to create under the output directory: no root, no `..`.
fn safe_rel(path: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for c in path.split('/') {
        match c {
            "" | "." | ".." => {}
            c => p.push(c),
        }
    }
    p
}

fn set_times_mode(path: &Path, it: &Item, is_link: bool) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return };
    if !is_link {
        if let Some(m) = it.unix_mode() {
            let perm = m & 0o7777;
            if perm != 0 || m & 0o170000 != 0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(perm));
            }
        } else if it.attrib.map_or(false, |a| a & 1 != 0) && !it.is_dir {
            if let Ok(meta) = std::fs::metadata(path) {
                let mut p = meta.permissions();
                p.set_mode(p.mode() & !0o222);
                let _ = std::fs::set_permissions(path, p);
            }
        }
    }
    if let Some((s, ns)) = it.mtime {
        let times = [crate::common::timespec(0, libc::UTIME_OMIT as i64), crate::common::timespec(s, ns as i64)];
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), if is_link { libc::AT_SYMLINK_NOFOLLOW } else { 0 }) };
    }
}

fn data_error_text(e: &DataError, encrypted: bool) -> String {
    match e {
        DataError::Crc if encrypted => "CRC Failed in encrypted file. Wrong password?".into(),
        DataError::Crc => "CRC Failed".into(),
        DataError::Data if encrypted => "Data Error in encrypted file. Wrong password?".into(),
        DataError::Data => "Data Error".into(),
        DataError::WrongPassword => "Wrong password".into(),
        DataError::Unsupported(m) => format!("Unsupported Method ({m})"),
        DataError::Io(e) => errstr(e),
        DataError::Stopped => "Stopped".into(),
    }
}

fn extract(o: &mut Out, opts: &Opts, a: &mut Opened, password: Option<&str>, phys: u64) -> i32 {
    let test = opts.cmd == Cmd::Test;
    let flat = opts.cmd == Cmd::Extract;
    let out_dir = PathBuf::from(opts.out_dir.clone().unwrap_or_default());
    let selection: Vec<bool> = a.items.iter().map(|it| selected(opts, it)).collect();
    // -spe: when everything is under one folder named like the output folder, drop it.
    let strip: Option<String> = if opts.spe {
        let out_name = Path::new(opts.out_dir.as_deref().unwrap_or(".")).file_name().map(|n| n.to_string_lossy().into_owned());
        let first = a.items.first().map(|i| i.path.split('/').next().unwrap_or("").to_string());
        first.filter(|f| Some(f) == out_name.as_ref() && a.items.iter().all(|i| i.path == *f || i.path.starts_with(&format!("{f}/"))))
    } else {
        None
    };
    let needs_password = a.items.iter().enumerate().any(|(i, it)| selection[i] && it.encrypted);
    let mut password = password.map(String::from);
    if needs_password && password.is_none() && !opts.ba {
        let _ = o.so.flush();
        match ask_password() {
            Some(p) => password = Some(p),
            None => return break_signaled(),
        }
    }
    if let Some(p) = &password {
        if a.fmt == Fmt::Zip || a.fmt == Fmt::SevenZ {
            // Reopen with the password (7z needs it for the reader; zip per entry).
            if let Ok(re) = formats::open(&a.path, Some(a.fmt), Some(p)) {
                *a = re;
            }
        }
    }
    let mut overwrite = opts.overwrite.or(if opts.yes { Some('a') } else { None });
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut size = 0u64;
    let mut errors: Vec<(usize, String)> = vec![];
    let mut quit = false;
    let mut dir_times: Vec<(PathBuf, Item)> = vec![];
    let mut stdout = io::stdout().lock();
    let bb = opts.bb;
    let items_snapshot = a.items.clone();
    // The item lines (-bb1) come right after the archive's properties.
    say!(o, "\n");
    {
        let mut on_error = |i: usize, e: DataError| {
            let it = &items_snapshot[i];
            errors.push((i, format!("{} : {}", data_error_text(&e, it.encrypted), it.path)));
        };
        let mut sink = |_i: usize, it: &Item, data: Data| -> Result<(), DataError> {
            if quit {
                return Err(DataError::Stopped);
            }
            if let Data::Passed(r) = data {
                // Decoded on the way through a solid block: counted, as 7-Zip does.
                files += 1;
                size += io::copy(r, &mut io::sink()).map_err(DataError::Io)?;
                return Ok(());
            }
            let shown = if it.is_dir { format!("{}/", it.path) } else { it.path.clone() };
            let mut rel = if flat {
                PathBuf::from(it.path.rsplit('/').next().unwrap_or(&it.path))
            } else {
                safe_rel(&it.path)
            };
            if let Some(s) = &strip {
                if let Ok(r) = rel.strip_prefix(s) {
                    rel = r.to_path_buf();
                }
            }
            if test || opts.stdout {
                match data {
                    Data::Dir => dirs += 1,
                    Data::Link(t) => {
                        files += 1;
                        size += t.len() as u64;
                        if opts.stdout {
                            stdout.write_all(t.as_bytes()).map_err(DataError::Io)?;
                        }
                    }
                    Data::File(r) => {
                        files += 1;
                        let n = if opts.stdout { io::copy(r, &mut stdout) } else { io::copy(r, &mut io::sink()) }.map_err(DataError::Io)?;
                        size += n;
                    }
                    Data::Passed(_) => unreachable!(),
                }
                if bb >= 1 {
                    say!(o, "{} {}\n", if test { "T" } else { "-" }, shown);
                }
                return Ok(());
            }
            let dest = out_dir.join(&rel);
            if rel.as_os_str().is_empty() {
                return Ok(());
            }
            match data {
                Data::Dir => {
                    dirs += 1;
                    std::fs::create_dir_all(&dest).map_err(DataError::Io)?;
                    if !flat {
                        dir_times.push((dest, it.clone()));
                    }
                    if bb >= 1 {
                        say!(o, "- {shown}\n");
                    }
                    Ok(())
                }
                data => {
                    if let Some(parent) = dest.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).map_err(DataError::Io)?;
                        }
                    }
                    files += 1;
                    let mut target = dest.clone();
                    if std::fs::symlink_metadata(&dest).is_ok() {
                        let mode = match overwrite {
                            Some(m) => m,
                            None => match ask_overwrite(o, &dest, it) {
                                'y' => 'y',
                                'n' => 'n',
                                'a' => {
                                    overwrite = Some('a');
                                    'a'
                                }
                                's' => {
                                    overwrite = Some('s');
                                    's'
                                }
                                'u' => {
                                    overwrite = Some('t');
                                    't'
                                }
                                _ => {
                                    quit = true;
                                    return Err(DataError::Stopped);
                                }
                            },
                        };
                        match mode {
                            'n' | 's' => {
                                // Skipped, yet read through (and counted) as 7-Zip does.
                                if let Data::File(r) = data {
                                    size += io::copy(r, &mut io::sink()).map_err(DataError::Io)?;
                                }
                                return Ok(());
                            }
                            't' => target = auto_rename(&dest),
                            'u' => {
                                let renamed = auto_rename(&dest);
                                std::fs::rename(&dest, &renamed).map_err(DataError::Io)?;
                            }
                            _ => {
                                if std::fs::symlink_metadata(&dest).map(|m| m.is_dir()).unwrap_or(false) {
                                    return Err(DataError::Io(io::Error::from_raw_os_error(libc::EISDIR)));
                                }
                                let _ = std::fs::remove_file(&dest);
                            }
                        }
                    }
                    match data {
                        Data::Link(t) => {
                            std::os::unix::fs::symlink(&t, &target).map_err(DataError::Io)?;
                            size += t.len() as u64;
                            set_times_mode(&target, it, true);
                        }
                        Data::File(r) => {
                            let mut f = File::create(&target).map_err(DataError::Io)?;
                            match io::copy(r, &mut f) {
                                Ok(n) => size += n,
                                Err(e) => {
                                    drop(f);
                                    return Err(DataError::Io(e));
                                }
                            }
                            drop(f);
                            set_times_mode(&target, it, false);
                        }
                        Data::Dir | Data::Passed(_) => unreachable!(),
                    }
                    if bb >= 1 {
                        say!(o, "- {shown}\n");
                    }
                    Ok(())
                }
            }
        };
        let want = |i: usize| selection[i];
        if let Err(e) = a.extract(&want, &mut sink, &mut on_error) {
            let _ = writeln!(io::stderr(), "ERROR: {}", data_error_text(&e, false));
            return 2;
        }
    }
    // Directories last: their times, once their contents are in.
    for (p, it) in dir_times.iter().rev() {
        set_times_mode(p, it, false);
    }
    if quit {
        return break_signaled();
    }
    let _ = stdout.flush();
    let _ = o.so.flush();
    for (_, m) in &errors {
        let _ = writeln!(io::stderr(), "ERROR: {m}");
    }
    if errors.is_empty() {
        say!(o, "Everything is Ok\n\n");
        if dirs > 0 {
            say!(o, "Folders: {dirs}\n");
        }
        if files != 1 || dirs != 0 {
            say!(o, "Files: {files}\n");
        }
        say!(o, "Size:       {size}\nCompressed: {phys}\n");
        0
    } else {
        say!(o, "\nSub items Errors: {}\n\nArchives with Errors: 1\n\nSub items Errors: {}\n", errors.len(), errors.len());
        2
    }
}

// a, u, d, rn

/// -m parameters for the writer.
fn parse_methods(opts: &Opts, fmt: Fmt) -> Result<Methods, String> {
    let mut m = Methods { password: opts.password.clone().filter(|p| !p.is_empty()), ..Default::default() };
    if fmt == Fmt::Zip {
        m.solid = false;
    }
    for raw in &opts.methods {
        let (k, v) = match raw.split_once('=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.to_string()),
            None => {
                // -mx9, -mmt4, -ms=on, -mhe, -m0=LZMA2
                let lower = raw.to_ascii_lowercase();
                let split = lower.find(|c: char| c.is_ascii_digit() || c == '-' || c == '+').unwrap_or(lower.len());
                (lower[..split].to_string(), raw[split..].to_string())
            }
        };
        let on = |v: &str| !matches!(v.to_ascii_lowercase().as_str(), "off" | "-" | "0" | "false");
        match k.as_str() {
            "x" => m.level = if v.is_empty() { 5 } else { v.parse().map_err(|_| format!("Incorrect parameter\n{raw}"))? },
            "mt" => m.threads = v.parse().unwrap_or(0),
            "s" => m.solid = on(&v),
            "he" => m.header_encryption = v.is_empty() || on(&v),
            "hc" | "qs" | "tr" | "ta" | "tp" | "fb" | "mc" | "lc" | "lp" | "pb" | "a" | "mf" | "pass" | "cu" | "cl" | "cp" | "yx" | "mm" if fmt != Fmt::Zip => {}
            "f" => m.filter = Some(v),
            "d" => m.dict = Some(parse_dict(&v).ok_or_else(|| format!("Incorrect parameter\n{raw}"))?),
            "tm" => m.store_times.0 = on(&v),
            "tc" => m.store_times.1 = on(&v),
            "ta" => m.store_times.2 = on(&v),
            "em" => m.zip_aes = v.to_ascii_lowercase().starts_with("aes"),
            "m" | "0" => {
                let mut parts = v.split(':');
                let name = parts.next().unwrap_or("").to_string();
                for p in parts {
                    if let Some(d) = p.strip_prefix('d').or_else(|| p.strip_prefix('D')) {
                        m.dict = parse_dict(d);
                    }
                }
                m.method = Some(name);
            }
            "1" | "2" | "3" => {
                // A second coder in the chain, as 7-Zip writes BCJ: -m0=BCJ -m1=LZMA2.
                let name = v.split(':').next().unwrap_or("").to_string();
                let low = name.to_ascii_lowercase();
                if matches!(low.as_str(), "bcj" | "arm" | "armt" | "arm64" | "ppc" | "ia64" | "sparc" | "riscv") || low.starts_with("delta") {
                    m.filter = Some(name);
                } else {
                    m.method = Some(name);
                }
            }
            _ => {}
        }
    }
    if let Some(mm) = &m.method {
        let low = mm.to_ascii_lowercase();
        if matches!(low.as_str(), "bcj" | "arm" | "armt" | "arm64" | "ppc" | "ia64" | "sparc" | "riscv") {
            m.filter = Some(mm.clone());
            m.method = None;
        }
    }
    Ok(m)
}

fn parse_dict(v: &str) -> Option<u32> {
    let l = v.to_ascii_lowercase();
    let (num, mult) = match l.chars().last()? {
        'b' => (&l[..l.len() - 1], 1u64),
        'k' => (&l[..l.len() - 1], 1 << 10),
        'm' => (&l[..l.len() - 1], 1 << 20),
        'g' => (&l[..l.len() - 1], 1 << 30),
        _ => {
            // A bare number below 32 is a power of two.
            let n: u64 = l.parse().ok()?;
            return Some(if n < 32 { 1u32 << n } else { n as u32 });
        }
    };
    num.parse::<u64>().ok().map(|n| (n * mult).min(u32::MAX as u64) as u32)
}

struct OldItem {
    item: Item,
    index: usize,
}

fn update_cmd(o: &mut Out, opts: &Opts) -> i32 {
    let arc = opts.archive.clone().unwrap();
    let to_stdout = opts.stdout;
    let exists = std::fs::metadata(&arc).is_ok();
    let hint = match arc_fmt_hint(opts, &arc) {
        Ok(h) => h,
        Err(m) => return cmd_error(o, &m),
    };
    // The archive as it is.
    let mut old: Option<Opened> = None;
    if exists {
        say!(o, "Open archive: {arc}\n");
        match formats::open(&arc, hint, opts.password.as_deref()) {
            Ok(a) => {
                print_props(o, &a, &arc);
                say!(o, "\n");
                old = Some(a);
            }
            Err(OpenError::NotArchive) => {
                let _ = o.so.flush();
                let _ = write!(
                    io::stderr(),
                    "ERROR: {arc}\n{arc}\nOpen ERROR: {}\n\n\nERRORS:\nIs not archive\n\n\nSystem ERROR:\nerrno=1 : Operation not permitted\n",
                    open_msg(hint.or_else(|| Fmt::from_ext(&arc)))
                );
                return 2;
            }
            Err(OpenError::NeedPassword) | Err(OpenError::WrongPassword) => {
                let _ = writeln!(io::stderr(), "ERROR: {arc} : Cannot open encrypted archive. Wrong password?");
                return 2;
            }
            Err(OpenError::Headers(m)) => {
                let _ = writeln!(io::stderr(), "ERROR: {arc}\nOpen ERROR: Headers Error: {m}");
                return 2;
            }
            Err(OpenError::Io(e)) => {
                let _ = writeln!(io::stderr(), "ERROR: {arc} : {}", errstr(&e));
                return 2;
            }
        }
    }
    let fmt = match (&old, hint) {
        (Some(a), _) => a.fmt,
        (None, Some(h)) => h,
        (None, None) => Fmt::from_ext(&arc).unwrap_or(Fmt::SevenZ),
    };
    let base_has_ext = arc.rsplit('/').next().map_or(false, |b| b.contains('.'));
    let arc = if !exists && !base_has_ext && !to_stdout {
        // 7-Zip adds the type's extension to a new archive's name without one.
        format!("{arc}.{}", if fmt == Fmt::GZip { "gz" } else if fmt == Fmt::BZip2 { "bz2" } else { fmt.name() })
    } else {
        arc
    };
    if !fmt.can_write() {
        let _ = writeln!(io::stderr(), "\nERROR: {} : {}\n\n\nSystem ERROR:\nE_NOTIMPL", arc, "Writing this format is not supported");
        return 2;
    }
    let methods = match parse_methods(opts, fmt) {
        Ok(m) => m,
        Err(m) => return cmd_error(o, &m),
    };
    let old_items: Vec<OldItem> = old.as_ref().map(|a| a.items.iter().cloned().enumerate().map(|(index, item)| OldItem { item, index }).collect()).unwrap_or_default();

    // What the command does to each old item, and what comes new from the disk (or stdin).
    let mut keep: Vec<usize> = vec![]; // indexes into old_items
    let mut deleted = (0u64, 0u64, 0u64);
    let mut new_items: Vec<NewItem> = vec![];
    let mut disk_read = 0u64;
    let mut scan_warnings: Vec<(String, String)> = vec![];
    let mut add_stats = (0u64, 0u64, 0u64);
    let mut keep_stats = (0u64, 0u64, 0u64);
    let mut renames: Vec<(String, String)> = vec![];
    let mut sdel_paths: Vec<PathBuf> = vec![];
    let mut stdin_temp: Option<PathBuf> = None;
    let mut stdin_meta: Option<std::fs::Metadata> = None;

    match opts.cmd {
        Cmd::Delete => {
            for (k, oi) in old_items.iter().enumerate() {
                let sel = opts.names.iter().any(|n| Pattern::new(n, if opts.recurse == Recurse::Default { Recurse::Default } else { opts.recurse }).matches(&oi.item.path, opts.case))
                    || opts.includes.iter().any(|p| p.matches(&oi.item.path, opts.case));
                let sel = sel && !opts.excludes.iter().any(|p| p.matches(&oi.item.path, opts.case));
                if sel {
                    if oi.item.is_dir {
                        deleted.0 += 1;
                    } else {
                        deleted.1 += 1;
                        deleted.2 += oi.item.size.unwrap_or(0);
                    }
                } else {
                    keep.push(k);
                }
            }
        }
        Cmd::Rename => {
            if opts.names.len() % 2 != 0 {
                return cmd_error(o, "Incorrect item in listfile.\nCheck the list file");
            }
            for pair in opts.names.chunks(2) {
                renames.push((pair[0].clone(), pair[1].clone()));
            }
            keep = (0..old_items.len()).collect();
        }
        _ => {
            let scanned = if let Some(name) = &opts.stdin {
                let name = if name.is_empty() {
                    if fmt.single_file() { formats_single_name(&arc) } else { "stdin".into() }
                } else {
                    name.clone()
                };
                // stdin goes to a file first; a redirected file gives its times and mode.
                let tmpf = PathBuf::from(format!("{arc}.collabo-stdin{}", std::process::id()));
                let copied = File::create(&tmpf).and_then(|mut f| io::copy(&mut io::stdin().lock(), &mut f));
                if let Err(e) = copied {
                    let _ = std::fs::remove_file(&tmpf);
                    return report_write_error(&arc, &errstr(&e));
                }
                stdin_temp = Some(tmpf.clone());
                let meta = std::fs::metadata(&tmpf).unwrap();
                let st = std::fs::metadata("/dev/stdin").ok().filter(|m| m.is_file());
                stdin_meta = st;
                vec![scan::DiskItem { name, path: tmpf, meta, is_dir: false, link: None }]
            } else {
                say!(o, "Scanning the drive:\n");
                let names: Vec<String> = if opts.names.is_empty() && opts.includes.is_empty() { vec!["*".into()] } else { opts.names.clone() };
                let mut res = scan::scan(
                    &names,
                    &scan::ScanOpts { recurse: opts.recurse, excludes: &opts.excludes, full_paths: opts.full_paths, store_links: opts.snl, case: opts.case },
                );
                for p in &opts.includes {
                    let joined = p.parts.join("/");
                    let more = scan::scan(&[joined], &scan::ScanOpts { recurse: p.recurse, excludes: &opts.excludes, full_paths: opts.full_paths, store_links: opts.snl, case: opts.case });
                    res.items.extend(more.items);
                    res.missing.extend(more.missing);
                }
                scan_warnings = res.missing;
                // The archive itself is never added to itself.
                let arc_canon = std::fs::canonicalize(&arc).ok();
                res.items.retain(|i| arc_canon.is_none() || std::fs::canonicalize(&i.path).ok() != arc_canon);
                res.items
            };
            for (n, e) in &scan_warnings {
                let _ = writeln!(io::stderr(), "\nWARNING: errno=2 : {e}\n{n}\n");
            }
            if opts.stdin.is_none() {
                // A link stored as a link (-snl) counts nothing while scanning.
                let (d, f, s) = scanned.iter().fold((0, 0, 0), |(d, f, s), i| {
                    if i.is_dir {
                        (d + 1, f, s)
                    } else if i.link.is_some() {
                        (d, f + 1, s)
                    } else {
                        (d, f + 1, s + i.meta.len())
                    }
                });
                say!(o, "{}\n\n", dir_stat(d, f, Some(s)));
            }
            // The update pairs: old items against what the disk has.
            let acts = opts.update_actions.unwrap_or(if opts.cmd == Cmd::Update { [1, 1, 2, 1, 2, 1, 2] } else { [1, 1, 2, 2, 2, 2, 2] });
            let mut on_disk: std::collections::HashMap<String, usize> = Default::default();
            for (i, s) in scanned.iter().enumerate() {
                on_disk.insert(norm_key(&s.name, opts.case), i);
            }
            let mut from_disk = vec![false; scanned.len()];
            for (k, oi) in old_items.iter().enumerate() {
                let key = norm_key(&oi.item.path, opts.case);
                let action = match on_disk.get(&key) {
                    None => {
                        let matched = opts.names.iter().any(|n| Pattern::new(n, opts.recurse).matches(&oi.item.path, opts.case));
                        if matched { acts[1] } else { acts[0] }
                    }
                    Some(&di) => {
                        let disk_t = scanned[di].meta.mtime();
                        let arc_t = oi.item.mtime.map(|t| t.0).unwrap_or(0);
                        let state = if scanned[di].is_dir != oi.item.is_dir {
                            6
                        } else if arc_t > disk_t {
                            3
                        } else if arc_t < disk_t {
                            4
                        } else {
                            5
                        };
                        let a = acts[state];
                        if a == 2 {
                            from_disk[di] = true;
                        }
                        a
                    }
                };
                if action == 1 {
                    keep.push(k);
                }
            }
            for (i, s) in scanned.iter().enumerate() {
                let key = norm_key(&s.name, opts.case);
                let in_old = old_items.iter().any(|oi| norm_key(&oi.item.path, opts.case) == key);
                if !in_old && acts[2] == 2 {
                    from_disk[i] = true;
                }
            }
            for (i, s) in scanned.into_iter().enumerate() {
                if !from_disk[i] {
                    continue;
                }
                let is_link = s.link.is_some() && opts.snl;
                let size = if s.is_dir {
                    0
                } else if is_link {
                    s.link.as_ref().map(|l| l.len() as u64).unwrap_or(0)
                } else {
                    s.meta.len()
                };
                if s.is_dir {
                    add_stats.0 += 1;
                } else {
                    add_stats.1 += 1;
                    add_stats.2 += size;
                }
                if opts.sdel {
                    sdel_paths.push(s.path.clone());
                }
                let m = stdin_meta.as_ref().unwrap_or(&s.meta);
                new_items.push(NewItem {
                    name: s.name,
                    is_dir: s.is_dir,
                    source: if s.is_dir { Source::None } else { Source::Disk(s.path.clone()) },
                    size: if is_link { 0 } else { size },
                    mtime: (m.mtime(), m.mtime_nsec() as u32),
                    atime: Some((m.atime(), m.atime_nsec() as u32)),
                    ctime: Some((m.ctime(), m.ctime_nsec() as u32)),
                    mode: if opts.stdin.is_some() && stdin_meta.is_none() { 0o100644 } else { m.mode() },
                    link: if is_link { s.link } else { None },
                    from_disk: true,
                });
            }
        }
    }
    for &k in &keep {
        let it = &old_items[k].item;
        if it.is_dir {
            keep_stats.0 += 1;
        } else {
            keep_stats.1 += 1;
            keep_stats.2 += it.size.unwrap_or(0);
        }
    }

    say!(o, "{} archive: {}\n\n", if exists { "Updating" } else { "Creating" }, if to_stdout { "StdOut" } else { &arc });
    if opts.cmd == Cmd::Delete && exists {
        say!(o, "\n");
    }
    if deleted.0 + deleted.1 > 0 || (opts.cmd == Cmd::Delete && exists) {
        say!(o, "Delete data from archive: {}\n", dir_stat(deleted.0, deleted.1, Some(deleted.2)));
    }
    if !keep.is_empty() {
        say!(o, "Keep old data in archive: {}\n", dir_stat(keep_stats.0, keep_stats.1, Some(keep_stats.2)));
    }
    say!(o, "Add new data to archive: {}\n\n", dir_stat(add_stats.0, add_stats.1, Some(add_stats.2)));
    let _ = disk_read;

    // Old items that stay are taken out of the old archive first (into a work directory beside
    // the archive), then the whole new archive is written to a temporary file and renamed.
    let work = PathBuf::from(format!("{arc}.collabo-tmp{}", std::process::id()));
    let mut items: Vec<NewItem> = vec![];
    if let Some(a) = old.as_mut() {
        if !keep.is_empty() {
            if let Err(m) = take_old(a, &old_items, &keep, &work, &mut items, &renames, opts.case) {
                formats::remove_tree(&work);
                let _ = writeln!(io::stderr(), "\nERROR: {m}");
                return 2;
            }
        }
    }
    // New items replace old ones of the same name; directories first, as 7-Zip orders a 7z.
    for n in new_items {
        items.retain(|i| norm_key(&i.name, opts.case) != norm_key(&n.name, opts.case));
        items.push(n);
    }
    // The order 7-Zip writes: by path; in a 7z the folders first, then the empty files.
    items.sort_by(|a, b| a.name.cmp(&b.name));
    if fmt == Fmt::SevenZ {
        items.sort_by_key(|i| if i.is_dir { 0 } else if i.size == 0 && i.link.is_none() { 1 } else { 2 });
    }
    if fmt.single_file() && items.iter().filter(|i| !i.is_dir).count() > 1 {
        formats::remove_tree(&work);
        let _ = o.so.flush();
        let _ = write!(io::stderr(), "\n\nSystem ERROR:\nE_INVALIDARG : One or more arguments are invalid\n");
        return 2;
    }
    // What is read from the disk, as each format's writer opens it: a 7z skips empty files, a
    // tar takes folders too.
    for it in &items {
        if !it.from_disk || (it.is_dir && fmt != Fmt::Tar) {
            continue;
        }
        if fmt == Fmt::SevenZ && it.size == 0 && it.link.is_none() {
            continue;
        }
        disk_read += 1;
        if opts.bb >= 1 && !it.is_dir {
            say!(o, "+ {}\n", it.name);
        }
    }
    let result = if to_stdout {
        let mut buf = io::Cursor::new(Vec::new());
        let r = if fmt.single_file() {
            match items.iter().find(|i| !i.is_dir) {
                Some(it) => {
                    let mut stdout = io::stdout().lock();
                    let mut src: Box<dyn Read> = match &it.source {
                        Source::Disk(p) | Source::Temp(p) => match File::open(p) {
                            Ok(f) => Box::new(f),
                            Err(e) => return report_write_error(&arc, &errstr(&e)),
                        },
                        Source::None => Box::new(io::empty()),
                    };
                    formats::write_single(fmt, &mut src, &mut stdout, it, &methods)
                }
                None => Ok(()),
            }
        } else if fmt == Fmt::Tar {
            formats::write(fmt, &mut buf, &items, &methods).and_then(|_| io::stdout().lock().write_all(buf.get_ref()).map_err(|e| errstr(&e)))
        } else {
            Err("E_NOTIMPL".into())
        };
        r.map(|_| 0u64)
    } else {
        let tmp = format!("{arc}.collabo-new{}", std::process::id());
        let r = File::create(&tmp).map_err(|e| errstr(&e)).and_then(|mut f| {
            let mut w = io::BufWriter::new(&mut f);
            formats::write(fmt, &mut w, &items, &methods)?;
            w.flush().map_err(|e| errstr(&e))?;
            drop(w);
            f.sync_all().map_err(|e| errstr(&e))?;
            Ok(())
        });
        match r {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &arc) {
                    let _ = std::fs::remove_file(&tmp);
                    Err(errstr(&e))
                } else {
                    Ok(std::fs::metadata(&arc).map(|m| m.len()).unwrap_or(0))
                }
            }
            Err(m) => {
                let _ = std::fs::remove_file(&tmp);
                Err(m)
            }
        }
    };
    formats::remove_tree(&work);
    if let Some(t) = &stdin_temp {
        let _ = std::fs::remove_file(t);
    }
    let arc_size = match result {
        Ok(s) => s,
        Err(m) => return report_write_error(&arc, &m),
    };
    if opts.stl && !to_stdout {
        if let Some(newest) = items.iter().map(|i| i.mtime).max() {
            let c = std::ffi::CString::new(arc.as_bytes()).unwrap();
            let times = [crate::common::timespec(newest.0, newest.1 as i64), crate::common::timespec(newest.0, newest.1 as i64)];
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        }
    }
    say!(o, "\nFiles read from disk: {disk_read}\n");
    if !to_stdout {
        say!(o, "Archive size: {}\n", size_smart(arc_size));
    }
    if opts.sdel {
        // Files first, then the directories (deepest first) that are left empty.
        for p in sdel_paths.iter().filter(|p| !p.is_dir()) {
            let _ = std::fs::remove_file(p);
        }
        for p in sdel_paths.iter().rev().filter(|p| p.is_dir()) {
            let _ = std::fs::remove_dir(p);
        }
    }
    if scan_warnings.is_empty() {
        if opts.sdel {
            say!(o, "\n");
        }
        say!(o, "Everything is Ok\n");
        0
    } else {
        say!(o, "\nScan WARNINGS for files and folders:\n\n");
        for (n, e) in &scan_warnings {
            say!(o, "{n} : errno=2 : {e}\n");
        }
        say!(o, "----------------\nScan WARNINGS: {}\n", scan_warnings.len());
        1
    }
}

fn report_write_error(arc: &str, m: &str) -> i32 {
    let _ = writeln!(io::stderr(), "\nERROR: {arc}\n{m}\n\n\nSystem ERROR:\n{m}");
    2
}

fn formats_single_name(arc: &str) -> String {
    let base = arc.rsplit('/').next().unwrap_or(arc);
    for suf in [".gz", ".bz2", ".xz", ".lzma", ".zst", ".tgz", ".tbz2", ".txz"] {
        if let Some(s) = base.strip_suffix(suf) {
            return if suf.starts_with(".t") && suf.len() == 4 { format!("{s}.tar") } else { s.to_string() };
        }
    }
    base.to_string()
}

fn norm_key(s: &str, case: bool) -> String {
    let n = scan::split_path(s).join("/");
    if case { n } else { n.to_lowercase() }
}

/// Extracts the old items that stay into `work`, as NewItems (renamed if asked).
fn take_old(a: &mut Opened, old: &[OldItem], keep: &[usize], work: &Path, out: &mut Vec<NewItem>, renames: &[(String, String)], case: bool) -> Result<(), String> {
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {}", work.display(), errstr(&e)))?;
    let wanted: std::collections::HashSet<usize> = keep.iter().map(|&k| old[k].index).collect();
    let mut failures = vec![];
    let mut collected: Vec<(usize, NewItem)> = vec![];
    {
        let mut sink = |i: usize, it: &Item, data: Data| -> Result<(), DataError> {
            let mut name = it.path.clone();
            for (from, to) in renames {
                let f = norm_key(from, case);
                let n = norm_key(&name, case);
                if n == f {
                    name = to.clone();
                } else if let Some(rest) = n.strip_prefix(&format!("{f}/")) {
                    name = format!("{to}/{rest}");
                }
            }
            let mode = it.unix_mode().map(|m| m & 0o7777).unwrap_or(if it.is_dir { 0o755 } else { 0o644 });
            let mtime = it.mtime.unwrap_or((0, 0));
            let ni = match data {
                Data::Passed(r) => {
                    io::copy(r, &mut io::sink()).map_err(DataError::Io)?;
                    return Ok(());
                }
                Data::Dir => NewItem { name, is_dir: true, source: Source::None, size: 0, mtime, atime: it.atime, ctime: it.ctime, mode, link: None, from_disk: false },
                Data::Link(t) => NewItem { name, is_dir: false, source: Source::None, size: 0, mtime, atime: it.atime, ctime: it.ctime, mode, link: Some(t), from_disk: false },
                Data::File(r) => {
                    let p = work.join(format!("{i}"));
                    let mut f = File::create(&p).map_err(DataError::Io)?;
                    let n = io::copy(r, &mut f).map_err(DataError::Io)?;
                    NewItem { name, is_dir: false, source: Source::Temp(p), size: n, mtime, atime: it.atime, ctime: it.ctime, mode, link: None, from_disk: false }
                }
            };
            collected.push((i, ni));
            Ok(())
        };
        let want = |i: usize| wanted.contains(&i);
        let mut on_error = |i: usize, e: DataError| failures.push(format!("{} : {}", data_error_text(&e, false), old[i].item.path));
        a.extract(&want, &mut sink, &mut on_error).map_err(|e| data_error_text(&e, false))?;
    }
    if !failures.is_empty() {
        return Err(failures.join("\n"));
    }
    collected.sort_by_key(|(i, _)| *i);
    out.extend(collected.into_iter().map(|(_, n)| n));
    Ok(())
}

// h

/// 7-Zip's hashers, in its order (-scrc*).
const HASHERS: &[&str] = &["CRC32", "CRC64", "SHA256", "SHA1", "BLAKE2sp", "MD5", "XXH64", "SHA384", "SHA512", "SHA3-256"];

/// One hash method's running state, as 7-Zip's CHasherState: the item's digest (little-endian
/// for the CRCs, as their hashers emit them) and the sums with their carry bytes.
struct HashSum {
    name: String,
    size: usize,
    data: Vec<u8>,
    names: Vec<u8>,
    n_data: u64,
    n_names: u64,
}

fn add_digests(acc: &mut [u8], d: &[u8]) {
    // Little-endian byte addition over the digest, the carry running into 8 extra bytes.
    let mut next = 0u32;
    for i in 0..d.len() {
        next += acc[i] as u32 + d[i] as u32;
        acc[i] = next as u8;
        next >>= 8;
    }
    for i in d.len()..acc.len() {
        next += acc[i] as u32;
        acc[i] = next as u8;
        next >>= 8;
    }
}

/// HashHexToString: digests up to 8 bytes as a little-endian number in upper case, longer ones
/// as bytes in lower case.
fn digest_hex(d: &[u8]) -> String {
    if d.len() > 8 {
        d.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        d.iter().rev().map(|b| format!("{b:02X}")).collect()
    }
}

fn sum_string(h: &HashSum, which: &[u8], count: u64) -> String {
    let mut s = digest_hex(&which[..h.size]);
    if count != 1 {
        let extra = &which[h.size..h.size + 8];
        let n = if extra[4..].iter().any(|&b| b != 0) { 8 } else { 4 };
        s += "-";
        s += &digest_hex(&extra[..n]);
    }
    s
}

fn hash_cmd(o: &mut Out, opts: &Opts) -> i32 {
    let methods: Vec<String> = if opts.hash_methods.is_empty() { vec!["CRC32".into()] } else { opts.hash_methods.iter().flat_map(|m| m.split(',').map(|s| s.to_ascii_uppercase())).collect() };
    let methods: Vec<String> = if methods.iter().any(|m| m == "*") { HASHERS.iter().map(|h| h.to_string()).collect() } else { methods };
    let methods: Vec<String> = methods.iter().map(|m| HASHERS.iter().find(|h| h.eq_ignore_ascii_case(m)).map(|h| h.to_string()).unwrap_or_else(|| m.clone())).collect();
    for m in &methods {
        if !HASHERS.contains(&m.as_str()) {
            return cmd_error(o, &format!("Unsupported hash method\n{m}"));
        }
    }
    say!(o, "Scanning\n");
    let names: Vec<String> = if opts.names.is_empty() { vec!["*".into()] } else { opts.names.clone() };
    let res = scan::scan(&names, &scan::ScanOpts { recurse: opts.recurse, excludes: &opts.excludes, full_paths: false, store_links: false, case: opts.case });
    let (nd, nf, total_size) = res.items.iter().fold((0u64, 0u64, 0u64), |(d, f, s), i| if i.is_dir { (d + 1, f, s) } else { (d, f + 1, s + i.meta.len()) });
    say!(o, "{}\n\n", dir_stat(nd, nf, Some(total_size)));
    let mut sums: Vec<HashSum> = methods.iter().map(|m| {
        let size = hash_width(m) / 2;
        HashSum { name: m.clone(), size, data: vec![0; size + 8], names: vec![0; size + 8], n_data: 0, n_names: 0 }
    }).collect();
    let col = |h: &HashSum| (h.size * 2).max(h.name.len());
    let mut head = String::new();
    let mut dash = String::new();
    for h in &sums {
        head += &format!("{:<w$} ", h.name, w = col(h));
        dash += &format!("{} ", "-".repeat(col(h)));
    }
    say!(o, "{}{:>13}  Name\n{}-------------  ------------\n", head, "Size", dash);
    let mut errors = 0;
    for it in &res.items {
        let digests: Vec<Vec<u8>> = if it.is_dir {
            sums.iter().map(|h| vec![0u8; h.size]).collect()
        } else {
            match hash_file(&it.path, &methods) {
                Ok(d) => d,
                Err(e) => {
                    let _ = writeln!(io::stderr(), "\nERROR: {} : {}", it.path.display(), errstr(&e));
                    errors += 1;
                    continue;
                }
            }
        };
        let mut line = String::new();
        for (h, dg) in sums.iter_mut().zip(&digests) {
            if it.is_dir {
                line += &format!("{} ", " ".repeat(col(h)));
            } else {
                line += &format!("{:<w$} ", digest_hex(dg), w = col(h));
                add_digests(&mut h.data, dg);
                h.n_data += 1;
            }
            // The digest of "data and names": a 16-byte prefix (1 for a folder), the item's
            // digest and its path in UTF-16LE, hashed with the same method.
            let mut pre = vec![0u8; 16];
            if it.is_dir {
                pre[0] = 1;
            }
            pre.extend_from_slice(dg);
            for u in it.name.encode_utf16() {
                pre.extend_from_slice(&u.to_le_bytes());
            }
            let nd = hash_bytes(&h.name, &pre);
            add_digests(&mut h.names, &nd);
            h.n_names += 1;
        }
        if it.is_dir {
            say!(o, "{}{:>13}  {}/\n", line, "", it.name);
        } else {
            say!(o, "{}{:>13}  {}\n", line, it.meta.len(), it.name);
        }
    }
    let mut line = String::new();
    for h in &sums {
        line += &format!("{:<w$} ", sum_string(h, &h.data, h.n_data), w = col(h));
    }
    say!(o, "{}-------------  ------------\n{}{:>13}  \n\n", dash, line, total_size);
    if nf != 1 || nd != 0 {
        if nd != 0 {
            say!(o, "Folders: {nd}\n");
        }
        say!(o, "Files: {nf}\n");
    }
    say!(o, "Size: {total_size}\n\n");
    for h in &sums {
        say!(o, "{:<6} for data:              {}\n", h.name, sum_string(h, &h.data, h.n_data));
        if nf != 1 || nd != 0 {
            say!(o, "{:<6} for data and names:    {}\n", h.name, sum_string(h, &h.names, h.n_names));
        }
        say!(o, "\n");
    }
    if errors == 0 {
        say!(o, "Everything is Ok\n");
        0
    } else {
        2
    }
}

/// One method's digest of a byte string.
fn hash_bytes(method: &str, data: &[u8]) -> Vec<u8> {
    let mut h = Hasher::new(method);
    h.update(data);
    h.finish()
}

fn hash_width(m: &str) -> usize {
    match m {
        "CRC32" => 8,
        "CRC64" | "XXH64" => 16,
        "SHA1" => 40,
        "SHA256" | "BLAKE2sp" | "SHA3-256" => 64,
        "SHA384" => 96,
        "SHA512" => 128,
        "MD5" => 32,
        _ => 8,
    }
}

fn crc64(data: &[u8], mut crc: u64) -> u64 {
    const POLY: u64 = 0xC96C_5795_D787_0F42;
    crc = !crc;
    for &b in data {
        crc ^= b as u64;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
        }
    }
    !crc
}

enum Hasher {
    Crc32(crc32fast::Hasher),
    Crc64(u64),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
    Sha3(sha3::Sha3_256),
    Md5(md5::Md5),
    Xxh64(twox_hash::XxHash64),
    Blake2sp(blake2s_simd::blake2sp::State),
}

impl Hasher {
    fn new(m: &str) -> Hasher {
        use md5::Digest as _;
        match m {
            "CRC32" => Hasher::Crc32(crc32fast::Hasher::new()),
            "CRC64" => Hasher::Crc64(0),
            "SHA1" => Hasher::Sha1(sha1::Sha1::new()),
            "SHA256" => Hasher::Sha256(sha2::Sha256::new()),
            "SHA384" => Hasher::Sha384(sha2::Sha384::new()),
            "SHA512" => Hasher::Sha512(sha2::Sha512::new()),
            "SHA3-256" => Hasher::Sha3(sha3::Sha3_256::new()),
            "XXH64" => Hasher::Xxh64(twox_hash::XxHash64::with_seed(0)),
            "BLAKE2sp" => Hasher::Blake2sp(blake2s_simd::blake2sp::State::new()),
            _ => Hasher::Md5(md5::Md5::new()),
        }
    }
    fn update(&mut self, b: &[u8]) {
        use md5::Digest as _;
        use std::hash::Hasher as _;
        match self {
            Hasher::Crc32(h) => h.update(b),
            Hasher::Crc64(c) => *c = crc64(b, *c),
            Hasher::Sha1(h) => h.update(b),
            Hasher::Sha256(h) => h.update(b),
            Hasher::Sha384(h) => h.update(b),
            Hasher::Sha512(h) => h.update(b),
            Hasher::Sha3(h) => h.update(b),
            Hasher::Md5(h) => h.update(b),
            Hasher::Xxh64(h) => h.write(b),
            Hasher::Blake2sp(h) => {
                h.update(b);
            }
        }
    }
    /// The digest as 7-Zip's hasher writes it (little-endian for the CRCs and XXH64).
    fn finish(self) -> Vec<u8> {
        use md5::Digest as _;
        use std::hash::Hasher as _;
        match self {
            Hasher::Crc32(h) => h.finalize().to_le_bytes().to_vec(),
            Hasher::Crc64(c) => c.to_le_bytes().to_vec(),
            Hasher::Sha1(h) => h.finalize().to_vec(),
            Hasher::Sha256(h) => h.finalize().to_vec(),
            Hasher::Sha384(h) => h.finalize().to_vec(),
            Hasher::Sha512(h) => h.finalize().to_vec(),
            Hasher::Sha3(h) => h.finalize().to_vec(),
            Hasher::Md5(h) => h.finalize().to_vec(),
            Hasher::Xxh64(h) => h.finish().to_le_bytes().to_vec(),
            Hasher::Blake2sp(h) => h.finalize().as_bytes().to_vec(),
        }
    }
}

fn hash_file(path: &Path, methods: &[String]) -> io::Result<Vec<Vec<u8>>> {
    let mut f = File::open(path)?;
    let mut hs: Vec<Hasher> = methods.iter().map(|m| Hasher::new(m)).collect();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for h in hs.iter_mut() {
            h.update(&buf[..n]);
        }
    }
    Ok(hs.into_iter().map(Hasher::finish).collect())
}
