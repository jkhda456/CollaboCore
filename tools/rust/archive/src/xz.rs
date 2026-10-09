//! xz, unxz, xzcat, lzma, unlzma, lzcat: XZ Utils' command line (5.8) over lzma-rust2.
//!
//! Formats: .xz, .lzma (LZMA_Alone), .lz (lzip) and raw streams; checks CRC32, CRC64, SHA-256 or
//! none; the presets -0..-9 and -e, the filter options (--lzma1/2, the BCJ filters, --delta,
//! --filters), -T threads for compression, --list from the Index, and the file handling of the
//! original (suffixes, -k, -f, -c, sources removed, mode and times kept, XZ_DEFAULTS/XZ_OPT).
//! Left out: --block-list and --filters1..9 (accepted, ignored: the output is still valid, its
//! Blocks are cut differently), --ignore-check (checks are always verified), --no-sparse and
//! memory limits (accepted; nothing to limit).

use crate::common::*;
use crate::getopt::{Arg, Item, Long, Parser};
use crate::verify::{Codec, Hashing, Tee};
use crate::xzindex;
use lzma_rust2::{CheckType, EncodeMode, FilterConfig, LzmaOptions, MfType};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

const VERSION: &str = "5.8.4";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Compress,
    Decompress,
    Test,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Auto,
    Xz,
    Lzma,
    Lzip,
    Raw,
}

#[derive(Clone, Debug)]
enum Filter {
    Lzma1(LzmaOptions),
    Lzma2(LzmaOptions),
    Pre(FilterConfig),
}

const V_SILENT: i32 = 0;
const V_ERROR: i32 = 1;
const V_WARNING: i32 = 2;
const V_VERBOSE: i32 = 3;

struct Opts {
    mode: Mode,
    keep: bool,
    force: bool,
    stdout: bool,
    single_stream: bool,
    suffix: Option<String>,
    files_from: Option<(String, u8)>,
    format: Format,
    check: CheckType,
    preset: u32,
    extreme: bool,
    threads: u32,
    block_size: Option<u64>,
    filters: Option<Vec<Filter>>,
    verbosity: i32,
    no_warn: bool,
    robot: bool,
}

struct State {
    exit: i32,
    verbosity: i32,
    no_warn: bool,
}

impl State {
    fn error(&mut self, msg: &str) {
        if self.verbosity >= V_ERROR {
            eprintln!("{}: {}", prog(), msg);
        }
        self.exit = 1;
    }
    fn warning(&mut self, msg: &str) {
        if self.verbosity >= V_WARNING {
            eprintln!("{}: {}", prog(), msg);
        }
        if !self.no_warn && self.exit == 0 {
            self.exit = 2;
        }
    }
}

fn fatal(msg: &str) -> ! {
    eprintln!("{}: {}", prog(), msg);
    std::process::exit(1)
}

fn try_help() -> ! {
    eprintln!("{}: Try '{} --help' for more information.", prog(), prog());
    std::process::exit(1)
}

// Long option ids beyond the short options' characters.
const O_NO_SYNC: i32 = 1000;
const O_SINGLE_STREAM: i32 = 1001;
const O_NO_SPARSE: i32 = 1002;
const O_FILES: i32 = 1003;
const O_FILES0: i32 = 1004;
const O_IGNORE_CHECK: i32 = 1005;
const O_BLOCK_SIZE: i32 = 1006;
const O_BLOCK_LIST: i32 = 1007;
const O_MEM: i32 = 1008;
const O_NO_ADJUST: i32 = 1009;
const O_FLUSH_TIMEOUT: i32 = 1010;
const O_FILTERS: i32 = 1011;
const O_FILTERS_N: i32 = 1012;
const O_FILTERS_HELP: i32 = 1013;
const O_LZMA1: i32 = 1014;
const O_LZMA2: i32 = 1015;
const O_X86: i32 = 1016;
const O_POWERPC: i32 = 1017;
const O_IA64: i32 = 1018;
const O_ARM: i32 = 1019;
const O_ARMTHUMB: i32 = 1020;
const O_ARM64: i32 = 1021;
const O_SPARC: i32 = 1022;
const O_RISCV: i32 = 1023;
const O_DELTA: i32 = 1024;
const O_ROBOT: i32 = 1025;
const O_INFO_MEMORY: i32 = 1026;

const fn l(name: &'static str, arg: Arg, id: i32) -> Long {
    Long { name, arg, id }
}

const LONGS: &[Long] = &[
    l("compress", Arg::No, 'z' as i32),
    l("decompress", Arg::No, 'd' as i32),
    l("uncompress", Arg::No, 'd' as i32),
    l("test", Arg::No, 't' as i32),
    l("list", Arg::No, 'l' as i32),
    l("keep", Arg::No, 'k' as i32),
    l("force", Arg::No, 'f' as i32),
    l("stdout", Arg::No, 'c' as i32),
    l("to-stdout", Arg::No, 'c' as i32),
    l("no-sync", Arg::No, O_NO_SYNC),
    l("single-stream", Arg::No, O_SINGLE_STREAM),
    l("no-sparse", Arg::No, O_NO_SPARSE),
    l("suffix", Arg::Required, 'S' as i32),
    l("files", Arg::Optional, O_FILES),
    l("files0", Arg::Optional, O_FILES0),
    l("format", Arg::Required, 'F' as i32),
    l("check", Arg::Required, 'C' as i32),
    l("ignore-check", Arg::No, O_IGNORE_CHECK),
    l("block-size", Arg::Required, O_BLOCK_SIZE),
    l("block-list", Arg::Required, O_BLOCK_LIST),
    l("memlimit-compress", Arg::Required, O_MEM),
    l("memlimit-decompress", Arg::Required, O_MEM),
    l("memlimit-mt-decompress", Arg::Required, O_MEM),
    l("memlimit", Arg::Required, 'M' as i32),
    l("memory", Arg::Required, 'M' as i32),
    l("no-adjust", Arg::No, O_NO_ADJUST),
    l("threads", Arg::Required, 'T' as i32),
    l("flush-timeout", Arg::Required, O_FLUSH_TIMEOUT),
    l("extreme", Arg::No, 'e' as i32),
    l("fast", Arg::No, '0' as i32),
    l("best", Arg::No, '9' as i32),
    l("filters", Arg::Required, O_FILTERS),
    l("filters1", Arg::Required, O_FILTERS_N),
    l("filters2", Arg::Required, O_FILTERS_N),
    l("filters3", Arg::Required, O_FILTERS_N),
    l("filters4", Arg::Required, O_FILTERS_N),
    l("filters5", Arg::Required, O_FILTERS_N),
    l("filters6", Arg::Required, O_FILTERS_N),
    l("filters7", Arg::Required, O_FILTERS_N),
    l("filters8", Arg::Required, O_FILTERS_N),
    l("filters9", Arg::Required, O_FILTERS_N),
    l("filters-help", Arg::No, O_FILTERS_HELP),
    l("lzma1", Arg::Optional, O_LZMA1),
    l("lzma2", Arg::Optional, O_LZMA2),
    l("x86", Arg::Optional, O_X86),
    l("powerpc", Arg::Optional, O_POWERPC),
    l("ia64", Arg::Optional, O_IA64),
    l("arm", Arg::Optional, O_ARM),
    l("armthumb", Arg::Optional, O_ARMTHUMB),
    l("arm64", Arg::Optional, O_ARM64),
    l("sparc", Arg::Optional, O_SPARC),
    l("riscv", Arg::Optional, O_RISCV),
    l("delta", Arg::Optional, O_DELTA),
    l("quiet", Arg::No, 'q' as i32),
    l("verbose", Arg::No, 'v' as i32),
    l("no-warn", Arg::No, 'Q' as i32),
    l("robot", Arg::No, O_ROBOT),
    l("info-memory", Arg::No, O_INFO_MEMORY),
    l("help", Arg::No, 'h' as i32),
    l("long-help", Arg::No, 'H' as i32),
    l("version", Arg::No, 'V' as i32),
];

const SHORTS: &str = "cC:defF:hHlkM:qQrS:tT:vVz0123456789";

/// liblzma's presets (lzma_lzma_preset), including -e.
fn preset_options(level: u32, extreme: bool) -> LzmaOptions {
    const DICT_POW2: [u32; 10] = [18, 20, 21, 22, 22, 23, 23, 24, 25, 26];
    const DEPTHS: [i32; 4] = [4, 8, 24, 48];
    let mut o = LzmaOptions::with_preset(level.min(9));
    o.dict_size = 1 << DICT_POW2[level as usize];
    o.lc = 3;
    o.lp = 0;
    o.pb = 2;
    if level <= 3 {
        o.mode = EncodeMode::Fast;
        o.mf = MfType::Hc4;
        o.nice_len = if level <= 1 { 128 } else { 273 };
        o.depth_limit = DEPTHS[level as usize];
    } else {
        o.mode = EncodeMode::Normal;
        o.mf = MfType::Bt4;
        o.nice_len = match level {
            4 => 16,
            5 => 32,
            _ => 64,
        };
        o.depth_limit = 0;
    }
    if extreme {
        o.mode = EncodeMode::Normal;
        o.mf = MfType::Bt4;
        if level == 3 || level == 5 {
            o.nice_len = 192;
            o.depth_limit = 0;
        } else {
            o.nice_len = 273;
            o.depth_limit = 512;
        }
    }
    o
}

/// A number as xz's str_to_uint64 reads it: "max", or digits with an optional KiB/MiB/GiB
/// multiplier (k, Ki, KiB, KB, ...).
fn parse_num(s: &str, min: u64, max: u64, what: &str) -> u64 {
    if s == "max" {
        return max;
    }
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits == 0 {
        fatal(&format!("{}: Value is not a non-negative decimal integer", mask(s)));
    }
    let range = || -> ! { fatal(&format!("Value of the option '{what}' must be in the range [{min}, {max}]")) };
    let mut v: u64 = s[..digits].parse().unwrap_or_else(|_| range());
    let suffix = &s[digits..];
    if !suffix.is_empty() {
        let mult: u64 = match suffix.as_bytes()[0] {
            b'k' | b'K' => 1 << 10,
            b'm' | b'M' => 1 << 20,
            b'g' | b'G' => 1 << 30,
            _ => 0,
        };
        if mult == 0 || !["", "i", "iB", "B"].contains(&&suffix[1..]) {
            eprintln!("{}: {}: Invalid multiplier suffix", prog(), mask(suffix));
            fatal("Valid suffixes are 'KiB' (2^10), 'MiB' (2^20), and 'GiB' (2^30).");
        }
        v = v.checked_mul(mult).unwrap_or_else(|| range());
    }
    if v < min || v > max {
        range();
    }
    v
}

/// LZMA1/LZMA2 options as --lzma2=preset=6e,dict=64MiB,... spells them.
fn lzma_opts(spec: &str) -> LzmaOptions {
    let mut o = preset_options(6, false);
    for part in spec.split(',').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or_else(|| fatal(&format!("{part}: Options must be 'name=value' pairs separated with commas")));
        match k {
            "preset" => {
                let (digits, e) = v.strip_suffix('e').map(|d| (d, true)).unwrap_or((v, false));
                match digits.parse::<u32>() {
                    Ok(n) if n <= 9 && digits.len() == 1 => o = preset_options(n, e),
                    _ => fatal(&format!("{v}: Unsupported LZMA1/LZMA2 preset")),
                }
            }
            "dict" => o.dict_size = parse_num(v, 4096, 1536 << 20, "dict") as u32,
            "lc" => o.lc = parse_num(v, 0, 4, "lc") as u32,
            "lp" => o.lp = parse_num(v, 0, 4, "lp") as u32,
            "pb" => o.pb = parse_num(v, 0, 4, "pb") as u32,
            "mode" => {
                o.mode = match v {
                    "fast" => EncodeMode::Fast,
                    "normal" => EncodeMode::Normal,
                    _ => fatal(&format!("{v}: Invalid option value")),
                }
            }
            "nice" => o.nice_len = parse_num(v, 2, 273, "nice") as u32,
            // lzma-rust2 has the hash chain and binary tree match finders on four bytes: the
            // two- and three-byte ones map to them (same format, a little different output).
            "mf" => {
                o.mf = match v {
                    "hc3" | "hc4" => MfType::Hc4,
                    "bt2" | "bt3" | "bt4" => MfType::Bt4,
                    _ => fatal(&format!("{v}: Invalid option value")),
                }
            }
            "depth" => o.depth_limit = parse_num(v, 0, u32::MAX as u64, "depth") as i32,
            _ => fatal(&format!("{k}: Invalid option name")),
        }
    }
    if o.lc + o.lp > 4 {
        fatal("The sum of lc and lp must not exceed 4");
    }
    o
}

fn bcj_start(spec: Option<&str>) -> u32 {
    match spec {
        None | Some("") => 0,
        Some(s) => match s.strip_prefix("start=") {
            Some(v) => parse_num(v, 0, u32::MAX as u64, "start") as u32,
            None => fatal(&format!("{s}: Invalid option name")),
        },
    }
}

fn filter_from(id: i32, spec: Option<&str>) -> Filter {
    let start = || bcj_start(spec);
    match id {
        O_LZMA1 => Filter::Lzma1(lzma_opts(spec.unwrap_or(""))),
        O_LZMA2 => Filter::Lzma2(lzma_opts(spec.unwrap_or(""))),
        O_X86 => Filter::Pre(FilterConfig::new_bcj_x86(start())),
        O_POWERPC => Filter::Pre(FilterConfig::new_bcj_ppc(start())),
        O_IA64 => Filter::Pre(FilterConfig::new_bcj_ia64(start())),
        O_ARM => Filter::Pre(FilterConfig::new_bcj_arm(start())),
        O_ARMTHUMB => Filter::Pre(FilterConfig::new_bcj_arm_thumb(start())),
        O_ARM64 => Filter::Pre(FilterConfig::new_bcj_arm64(start())),
        O_SPARC => Filter::Pre(FilterConfig::new_bcj_sparc(start())),
        O_RISCV => Filter::Pre(FilterConfig::new_bcj_risc_v(start())),
        O_DELTA => {
            let dist = match spec {
                None | Some("") => 1,
                Some(s) => match s.strip_prefix("dist=") {
                    Some(v) => parse_num(v, 1, 256, "dist") as u32,
                    None => fatal(&format!("{s}: Invalid option name")),
                },
            };
            Filter::Pre(FilterConfig::new_delta(dist))
        }
        _ => unreachable!(),
    }
}

/// --filters: names with their options, separated by spaces or "--".
fn parse_filters(spec: &str) -> Vec<Filter> {
    let mut out = vec![];
    for word in spec.split(|c: char| c.is_whitespace()).flat_map(|w| w.split("--")).filter(|w| !w.is_empty()) {
        let (name, opts) = match word.split_once(|c| c == ':' || c == '=') {
            Some((n, o)) => (n, Some(o)),
            None => (word, None),
        };
        let id = match name {
            "lzma1" => O_LZMA1,
            "lzma2" => O_LZMA2,
            "x86" => O_X86,
            "powerpc" => O_POWERPC,
            "ia64" => O_IA64,
            "arm" => O_ARM,
            "armthumb" => O_ARMTHUMB,
            "arm64" => O_ARM64,
            "sparc" => O_SPARC,
            "riscv" => O_RISCV,
            "delta" => O_DELTA,
            _ => {
                eprintln!("{}: Error in --filters=FILTERS option:", prog());
                eprintln!("{}: {}", prog(), spec);
                fatal("Unknown filter name");
            }
        };
        // A preset may stand alone: "lzma2:6e" is not valid, but "6e" for "lzma2:preset=6e" is.
        out.push(filter_from(id, opts));
    }
    if out.len() > 4 {
        fatal("Maximum number of filters is four");
    }
    out
}

fn usage(long: bool) -> String {
    let p = prog();
    let mut s = format!(
        r#"Usage: {p} [OPTION]... [FILE]...
Compress or decompress FILEs in the .xz format.

Mandatory arguments to long options are mandatory for short options too.

 Operation mode:
  -z, --compress      force compression
  -d, --decompress    force decompression
  -t, --test          test compressed file integrity
  -l, --list          list information about .xz files

 Operation modifiers:
  -k, --keep          keep (don't delete) input files
  -f, --force         force overwrite of output file and (de)compress links
  -c, --stdout        write to standard output and don't delete input files
"#
    );
    if long {
        s += r#"      --single-stream decompress only the first stream, and silently
                      ignore possible remaining input data
      --no-sparse     do not create sparse files when decompressing
  -S, --suffix=.SUF   use the suffix '.SUF' on compressed files
      --files[=FILE]  read filenames to process from FILE; if FILE is
                      omitted, filenames are read from the standard input;
                      filenames must be terminated with the newline character
      --files0[=FILE] like --files but use the null character as terminator

 Basic file format and compression options:
  -F, --format=FMT    file format to encode or decode; possible values are
                      'auto' (default), 'xz', 'lzma', 'lzip', and 'raw'
  -C, --check=CHECK   integrity check type: 'none' (use with caution),
                      'crc32', 'crc64' (default), or 'sha256'
      --ignore-check  don't verify the integrity check when decompressing
"#;
    }
    s += r#"  -0 ... -9           compression preset; default is 6; take compressor *and*
                      decompressor memory usage into account before using 7-9!
  -e, --extreme       try to improve compression ratio by using more CPU time;
                      does not affect decompressor memory requirements
  -T, --threads=NUM   use at most NUM threads; the default is 0 which uses
                      as many threads as there are processor cores
"#;
    if long {
        s += r#"      --block-size=SIZE
                      start a new .xz block after every SIZE bytes of input;
                      use this to set the block size for threaded compression

 Custom filter chain for compression (alternative to using presets):
  --filters=FILTERS   set the filter chain using the liblzma filter string
                      syntax; use --filters-help for more information

  --lzma1[=OPTS]      LZMA1 or LZMA2; OPTS is a comma-separated list of zero or
  --lzma2[=OPTS]      more of the following options (valid values; default):
                        preset=PRE reset options to a preset (0-9[e])
                        dict=NUM   dictionary size (4KiB - 1536MiB; 8MiB)
                        lc=NUM     number of literal context bits (0-4; 3)
                        lp=NUM     number of literal position bits (0-4; 0)
                        pb=NUM     number of position bits (0-4; 2)
                        mode=MODE  compression mode (fast, normal; normal)
                        nice=NUM   nice length of a match (2-273; 64)
                        mf=NAME    match finder (hc3, hc4, bt2, bt3, bt4; bt4)
                        depth=NUM  maximum search depth; 0=automatic (default)

  --x86[=OPTS]        x86 BCJ filter (32-bit and 64-bit)
  --arm[=OPTS]        ARM BCJ filter
  --armthumb[=OPTS]   ARM-Thumb BCJ filter
  --arm64[=OPTS]      ARM64 BCJ filter
  --powerpc[=OPTS]    PowerPC BCJ filter (big endian only)
  --ia64[=OPTS]       IA-64 (Itanium) BCJ filter
  --sparc[=OPTS]      SPARC BCJ filter
  --riscv[=OPTS]      RISC-V BCJ filter
                      Valid OPTS for all BCJ filters:
                        start=NUM  start offset for conversions (default=0)

  --delta[=OPTS]      Delta filter; valid OPTS (valid values; default):
                        dist=NUM   distance between bytes being subtracted
                                   from each other (1-256; 1)
"#;
    }
    s += r#"
 Other options:
  -q, --quiet         suppress warnings; specify twice to suppress errors too
  -v, --verbose       be verbose; specify twice for even more verbose
"#;
    if long {
        s += r#"  -Q, --no-warn       make warnings not affect the exit status
      --robot         use machine-parsable messages (useful for scripts)

      --info-memory   display the total amount of RAM and the currently active
                      memory usage limits, and exit
  -h, --help          display the short help (lists only the basic options)
  -H, --long-help     display this long help and exit
"#;
    } else {
        s += r#"  -h, --help          display this short help and exit
  -H, --long-help     display the long help (lists also the advanced options)
"#;
    }
    s += r#"  -V, --version       display the version number and exit

With no FILE, or when FILE is -, read standard input.

This xz is collaboCore's reimplementation of XZ Utils' xz in Rust (lzma-rust2).
"#;
    s
}

pub fn main(argv0: &str, args: Vec<String>) -> i32 {
    let mut o = Opts {
        mode: Mode::Compress,
        keep: false,
        force: false,
        stdout: false,
        single_stream: false,
        suffix: None,
        files_from: None,
        format: Format::Auto,
        check: CheckType::Crc64,
        preset: 6,
        extreme: false,
        threads: 0,
        block_size: None,
        filters: None,
        verbosity: V_WARNING,
        no_warn: false,
        robot: false,
    };
    match argv0 {
        "unxz" => o.mode = Mode::Decompress,
        "xzcat" => {
            o.mode = Mode::Decompress;
            o.stdout = true;
        }
        "lzma" => o.format = Format::Lzma,
        "unlzma" => {
            o.mode = Mode::Decompress;
            o.format = Format::Lzma;
        }
        "lzcat" => {
            o.mode = Mode::Decompress;
            o.format = Format::Lzma;
            o.stdout = true;
        }
        _ => {}
    }
    // XZ_DEFAULTS, then XZ_OPT, then the command line, as xz reads them.
    let mut operands = vec![];
    for (env, words) in [("XZ_DEFAULTS", true), ("XZ_OPT", true), ("", false)] {
        let list: Vec<String> = if words {
            match std::env::var(env) {
                Ok(v) => v.split_whitespace().map(String::from).collect(),
                Err(_) => continue,
            }
        } else {
            args.clone()
        };
        let mut p = Parser::new(list, SHORTS, LONGS);
        while let Some(item) = p.next() {
            match item {
                Err(msg) => {
                    eprintln!("{}: {}", prog(), msg);
                    try_help();
                }
                // xz ignores file names in XZ_DEFAULTS and XZ_OPT.
                Ok(Item::Operand(s)) => {
                    if !words {
                        operands.push(s);
                    }
                }
                Ok(Item::Opt(id, val)) => apply(&mut o, id, val),
            }
        }
    }

    let mut st = State { exit: 0, verbosity: o.verbosity, no_warn: o.no_warn };
    if o.mode == Mode::List && o.format != Format::Auto && o.format != Format::Xz {
        fatal("--list works only on .xz files (--format=xz or --format=auto)");
    }
    if o.mode == Mode::List && o.stdout {
        // xz ignores -c in list mode.
        o.stdout = false;
    }
    if o.mode == Mode::Compress && o.format == Format::Auto {
        o.format = Format::Xz;
    }
    if o.format == Format::Raw && o.suffix.is_none() && !o.stdout && o.mode != Mode::Test {
        fatal("With --format=raw, --suffix=.SUF is required unless writing to stdout");
    }
    if o.mode == Mode::Compress {
        if let Some(f) = &o.filters {
            validate_chain(&o, f);
        } else if o.format == Format::Raw {
            if o.verbosity >= V_WARNING {
                eprintln!("{}: Using a preset in raw mode is discouraged.", prog());
                eprintln!("{}: The exact options of the presets may vary between software versions.", prog());
            }
            o.filters = Some(vec![Filter::Lzma2(preset_options(o.preset, o.extreme))]);
        }
    } else if o.format == Format::Raw && o.filters.is_none() {
        o.filters = Some(vec![Filter::Lzma2(preset_options(o.preset, o.extreme))]);
    }
    if o.threads == 0 {
        o.threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    }

    // The list of files: operands, --files, or standard input.
    let mut names: Vec<String> = operands;
    if let Some((src, term)) = &o.files_from {
        let data = if src == "-" {
            let mut v = vec![];
            io::stdin().read_to_end(&mut v).map(|_| v)
        } else {
            std::fs::read(src)
        };
        match data {
            Ok(d) => {
                for n in d.split(|&b| b == *term) {
                    if n.is_empty() {
                        continue;
                    }
                    names.push(String::from_utf8_lossy(n).into_owned());
                }
            }
            Err(e) => fatal(&format!("{}: {}", mask(src), errstr(&e))),
        }
    } else if names.is_empty() {
        names.push("-".into());
    }

    let reads_stdin = names.iter().any(|n| n == "-");
    if reads_stdin {
        if o.mode == Mode::List {
            fatal("--list does not support reading from standard input");
        }
        if o.mode != Mode::Compress && stdin_is_tty() && !o.force {
            fatal("Compressed data cannot be read from a terminal");
        }
    }
    if o.mode == Mode::Compress && (o.stdout || reads_stdin) && stdout_is_tty() && !o.force {
        fatal("Compressed data cannot be written to a terminal");
    }

    install_signal_cleanup();
    let total = names.len();
    let mut totals = ListTotals::default();
    for (i, name) in names.iter().enumerate() {
        if name.is_empty() {
            st.error("Empty filename, skipping");
            continue;
        }
        if o.mode == Mode::List {
            list_file(&o, &mut st, name, i + 1, total, &mut totals);
        } else {
            process(&o, &mut st, name, i + 1, total);
        }
    }
    if o.mode == Mode::List {
        list_totals(&o, &totals);
    }
    st.exit
}

fn apply(o: &mut Opts, id: i32, val: Option<String>) {
    let v = || val.clone().unwrap_or_default();
    match id {
        x if (b'0' as i32..=b'9' as i32).contains(&x) => {
            o.preset = (x - b'0' as i32) as u32;
            o.filters = None;
        }
        x if x == 'z' as i32 => o.mode = Mode::Compress,
        x if x == 'd' as i32 => o.mode = Mode::Decompress,
        x if x == 't' as i32 => o.mode = Mode::Test,
        x if x == 'l' as i32 => o.mode = Mode::List,
        x if x == 'k' as i32 => o.keep = true,
        x if x == 'f' as i32 => o.force = true,
        x if x == 'c' as i32 => o.stdout = true,
        x if x == 'e' as i32 => {
            o.extreme = true;
            o.filters = None;
        }
        x if x == 'S' as i32 => {
            let s = v();
            if s.is_empty() || s.contains('/') {
                fatal(&format!("{}: Invalid filename suffix", mask(&s)));
            }
            o.suffix = Some(s);
        }
        x if x == 'F' as i32 => {
            o.format = match v().as_str() {
                "auto" => Format::Auto,
                "xz" => Format::Xz,
                "lzma" | "alone" => Format::Lzma,
                "lzip" => Format::Lzip,
                "raw" => Format::Raw,
                f => fatal(&format!("{}: Unknown file format type", mask(f))),
            }
        }
        x if x == 'C' as i32 => {
            o.check = match v().as_str() {
                "none" => CheckType::None,
                "crc32" => CheckType::Crc32,
                "crc64" => CheckType::Crc64,
                "sha256" => CheckType::Sha256,
                c => fatal(&format!("{}: Unsupported integrity check type", mask(c))),
            }
        }
        x if x == 'T' as i32 => o.threads = parse_num(&v(), 0, 16384, "threads") as u32,
        x if x == 'M' as i32 => {}
        x if x == 'q' as i32 => o.verbosity = (o.verbosity - 1).max(V_SILENT),
        x if x == 'v' as i32 => o.verbosity = (o.verbosity + 1).min(4),
        x if x == 'Q' as i32 => o.no_warn = true,
        x if x == 'h' as i32 || x == 'H' as i32 => {
            print!("{}", usage(x == 'H' as i32));
            std::process::exit(0);
        }
        x if x == 'V' as i32 => {
            if o.robot {
                println!("XZ_VERSION=50080042\nLIBLZMA_VERSION=50080042");
            } else {
                println!("xz (collaboCore) {VERSION}\nliblzma-compatible: lzma-rust2 0.21");
            }
            std::process::exit(0);
        }
        x if x == 'r' as i32 => {}
        O_NO_SYNC | O_NO_SPARSE | O_IGNORE_CHECK | O_MEM | O_NO_ADJUST | O_FLUSH_TIMEOUT | O_BLOCK_LIST | O_FILTERS_N => {}
        O_SINGLE_STREAM => o.single_stream = true,
        O_FILES => o.files_from = Some((val.filter(|s| !s.is_empty()).unwrap_or_else(|| "-".into()), b'\n')),
        O_FILES0 => o.files_from = Some((val.filter(|s| !s.is_empty()).unwrap_or_else(|| "-".into()), 0)),
        O_BLOCK_SIZE => o.block_size = Some(parse_num(&v(), 1, u64::MAX, "block-size")),
        O_FILTERS => o.filters = Some(parse_filters(&v())),
        O_FILTERS_HELP => {
            println!(
                r#"Filter chains are set using the --filters=FILTERS or
--filters1=FILTERS ... --filters9=FILTERS options. Each filter in the chain
can be separated by spaces or '--'. Alternatively a preset <0-9>[e] can be
specified instead of a filter chain.

The supported filters and their options are:

  lzma1/lzma2:
    preset=<0-9>[e], dict=NUM, lc=NUM, lp=NUM, pb=NUM, mode=fast|normal,
    nice=NUM, mf=hc3|hc4|bt2|bt3|bt4, depth=NUM

  x86, arm, armthumb, arm64, powerpc, ia64, sparc, riscv:
    start=NUM

  delta:
    dist=NUM"#
            );
            std::process::exit(0);
        }
        O_LZMA1 | O_LZMA2 | O_X86 | O_POWERPC | O_IA64 | O_ARM | O_ARMTHUMB | O_ARM64 | O_SPARC | O_RISCV | O_DELTA => {
            let f = filter_from(id, val.as_deref());
            let chain = o.filters.get_or_insert_with(Vec::new);
            chain.push(f);
            if chain.len() > 4 {
                fatal("Maximum number of filters is four");
            }
        }
        O_ROBOT => o.robot = true,
        O_INFO_MEMORY => {
            let mem = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) as u64 * libc::sysconf(libc::_SC_PAGESIZE) as u64 };
            if o.robot {
                println!("{mem}\t0\t0\t0\t0\t{}", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
            } else {
                println!("Hardware information:\n  Amount of physical memory (RAM):  {}\n  Number of processor threads:      {}\n\nMemory usage limits:\n  Compression:                      Disabled\n  Decompression:                    Disabled",
                    nicestr(mem, true), std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
            }
            std::process::exit(0);
        }
        _ => {}
    }
}

fn validate_chain(o: &Opts, chain: &[Filter]) {
    let last = chain.last();
    match o.format {
        Format::Lzma | Format::Lzip => {
            if chain.len() != 1 || !matches!(last, Some(Filter::Lzma1(_))) {
                fatal(if o.format == Format::Lzma {
                    "The .lzma format supports only the LZMA1 filter"
                } else {
                    "The .lz format supports only the LZMA1 filter"
                });
            }
        }
        Format::Xz | Format::Auto => {
            if chain.iter().any(|f| matches!(f, Filter::Lzma1(_))) {
                fatal("LZMA1 cannot be used with the .xz format");
            }
            if !matches!(last, Some(Filter::Lzma2(_))) || chain[..chain.len() - 1].iter().any(|f| !matches!(f, Filter::Pre(_))) {
                fatal("Unsupported filter chain or filter options");
            }
        }
        Format::Raw => {
            if chain.len() != 1 || matches!(last, Some(Filter::Pre(_))) {
                fatal("Unsupported filter chain or filter options");
            }
        }
    }
}

/// The LZMA options compression uses: the chain's last filter, or the preset.
fn chain_lzma(o: &Opts) -> (LzmaOptions, Vec<FilterConfig>) {
    match &o.filters {
        Some(chain) => {
            let mut pre = vec![];
            let mut lz = preset_options(o.preset, o.extreme);
            for f in chain {
                match f {
                    Filter::Pre(c) => pre.push(c.clone()),
                    Filter::Lzma1(l) | Filter::Lzma2(l) => lz = l.clone(),
                }
            }
            (lz, pre)
        }
        None => (preset_options(o.preset, o.extreme), vec![]),
    }
}

/// Why a file could not be (de)compressed, in xz's words.
enum CodecError {
    Verify(String),
    Read(io::Error),
    Write(io::Error),
    Format,
    Corrupt,
    Truncated,
    Options,
    Memory,
}

fn classify(e: io::Error, src_err: &mut Option<io::Error>, dst_err: &mut Option<io::Error>) -> CodecError {
    if let Some(v) = crate::verify::as_verify(&e) {
        return CodecError::Verify(v.to_string());
    }
    if let Some(r) = src_err.take() {
        return CodecError::Read(r);
    }
    if let Some(w) = dst_err.take() {
        return CodecError::Write(w);
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => CodecError::Truncated,
        io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput => CodecError::Options,
        io::ErrorKind::OutOfMemory => CodecError::Memory,
        _ => {
            let m = e.to_string();
            if m.contains("magic") {
                CodecError::Format
            } else if m.contains("unsupported") || m.contains("Unsupported") {
                CodecError::Options
            } else {
                CodecError::Corrupt
            }
        }
    }
}

fn codec_msg(e: &CodecError) -> String {
    match e {
        CodecError::Verify(m) => m.clone(),
        CodecError::Read(e) => format!("Read error: {}", errstr(e)),
        CodecError::Write(e) => format!("Write error: {}", errstr(e)),
        CodecError::Format => "File format not recognized".into(),
        CodecError::Corrupt => "Compressed data is corrupt".into(),
        CodecError::Truncated => "Unexpected end of input".into(),
        CodecError::Options => "Unsupported options".into(),
        CodecError::Memory => "Cannot allocate memory".into(),
    }
}

/// Compresses `input` into `out` as `o` says; the result is decoded again on the way (verify.rs),
/// raw streams excepted (they cannot be read without the options).
fn compress<R: Read, W: Write>(o: &Opts, input: &mut R, out: W) -> io::Result<()> {
    let (lz, pre) = chain_lzma(o);
    if o.format == Format::Raw {
        if let Some([Filter::Lzma1(l)]) = o.filters.as_deref() {
            let mut w = lzma_rust2::LzmaWriter::new_no_header(out, l, true)?;
            io::copy(input, &mut w)?;
            w.finish()?;
        } else {
            let mut w = lzma_rust2::Lzma2Writer::new(out, lzma_rust2::Lzma2Options { lzma_options: lz, chunk_size: None });
            io::copy(input, &mut w)?;
            w.finish()?;
        }
        return Ok(());
    }
    let mut input = Hashing::new(input);
    let codec = match o.format {
        Format::Lzma => Codec::Lzma,
        Format::Lzip => Codec::Lzip,
        _ => Codec::Xz,
    };
    let tee = Tee::new(out, codec, None);
    let tee = match o.format {
        Format::Lzma => {
            let mut w = lzma_rust2::LzmaWriter::new_use_header(tee, &lz, None)?;
            io::copy(&mut input, &mut w)?;
            w.finish()?
        }
        Format::Lzip => {
            let mut w = lzma_rust2::LzipWriter::new(tee, lzma_rust2::LzipOptions { lzma_options: lz, member_size: None });
            io::copy(&mut input, &mut w)?;
            w.finish()?
        }
        _ => {
            let mut x = lzma_rust2::XzOptions::with_preset(6);
            x.lzma_options = lz.clone();
            x.check_type = o.check;
            for f in pre.iter().rev() {
                x.prepend_pre_filter(f.filter_type, f.property);
            }
            if o.threads > 1 {
                // As xz: Blocks of three times the dictionary, at least 1 MiB, so threads
                // have independent work.
                let bs = o.block_size.unwrap_or_else(|| (3 * lz.dict_size as u64).max(1 << 20));
                x.set_block_size(std::num::NonZeroU64::new(bs));
                let mut w = lzma_rust2::XzWriterMt::new(tee, x, o.threads)?;
                io::copy(&mut input, &mut w)?;
                w.finish()?
            } else {
                x.set_block_size(o.block_size.and_then(std::num::NonZeroU64::new));
                let mut w = lzma_rust2::XzWriter::new(tee, x)?;
                io::copy(&mut input, &mut w)?;
                w.finish()?
            }
        }
    };
    tee.finish(input.sum()).map_err(crate::verify::failed)
}

/// The LZMA_Alone header test: a valid properties byte; in auto mode (picky, as
/// lzma_alone_decoder is there) also a dictionary size of 2^n or 2^n + 2^(n-1) and a known
/// size below 256 GiB.
fn looks_like_lzma(h: &[u8], picky: bool) -> bool {
    if h.is_empty() || h[0] > (4 * 5 + 4) * 9 + 8 {
        return false;
    }
    if !picky {
        return true;
    }
    if h.len() < 13 {
        return false;
    }
    let dict = u32::from_le_bytes(h[1..5].try_into().unwrap());
    if dict != u32::MAX {
        let mut d = dict.wrapping_sub(1);
        d |= d >> 2;
        d |= d >> 3;
        d |= d >> 4;
        d |= d >> 8;
        d |= d >> 16;
        d = d.wrapping_add(1);
        if d != dict {
            return false;
        }
    }
    let size = u64::from_le_bytes(h[5..13].try_into().unwrap());
    size == u64::MAX || size < (1u64 << 38)
}

/// The format `head` (the input's first bytes) is decoded as; Ok(None): not a known one, to
/// be copied as it is (-dcf in auto mode).
fn detect(o: &Opts, head: &[u8], may_pass: bool) -> Result<Option<Format>, CodecError> {
    let xz = head.len() >= 6 && head[..6] == xzindex::MAGIC;
    let lzip = head.len() >= 4 && &head[..4] == b"LZIP";
    match o.format {
        Format::Auto if xz => Ok(Some(Format::Xz)),
        Format::Auto if lzip => Ok(Some(Format::Lzip)),
        Format::Auto if looks_like_lzma(head, true) => Ok(Some(Format::Lzma)),
        Format::Auto if may_pass => Ok(None),
        Format::Xz if xz => Ok(Some(Format::Xz)),
        Format::Lzip if lzip => Ok(Some(Format::Lzip)),
        Format::Lzma if looks_like_lzma(head, false) => Ok(Some(Format::Lzma)),
        Format::Raw => Ok(Some(Format::Raw)),
        _ => Err(CodecError::Format),
    }
}

fn decode<R: Read, W: Write>(o: &Opts, format: Option<Format>, head: Vec<u8>, input: &mut SrcReader<R>, out: &mut DestWriter<W>) -> Result<(), CodecError> {
    let mut joined = BufReader::with_capacity(1 << 16, io::Cursor::new(head).chain(&mut *input));
    let result = match format {
        None => io::copy(&mut joined, out).map(|_| ()),
        Some(Format::Xz) => decode_xz(o, &mut joined, out),
        Some(Format::Lzma) => lzma_rust2::LzmaReader::new_mem_limit(&mut joined, u32::MAX, None).and_then(|mut r| io::copy(&mut r, out).map(|_| ())),
        Some(Format::Lzip) => io::copy(&mut lzma_rust2::LzipReader::new(&mut joined), out).map(|_| ()),
        Some(Format::Raw) => match o.filters.as_deref() {
            Some([Filter::Lzma1(l)]) => lzma_rust2::LzmaReader::new(&mut joined, u64::MAX, l.lc, l.lp, l.pb, l.dict_size, None)
                .and_then(|mut r| io::copy(&mut r, out).map(|_| ())),
            Some([Filter::Lzma2(l)]) => io::copy(&mut lzma_rust2::Lzma2Reader::new(&mut joined, l.dict_size, None), out).map(|_| ()),
            _ => return Err(CodecError::Options),
        },
        Some(Format::Auto) => unreachable!(),
    };
    drop(joined);
    result.map_err(|e| classify(e, &mut input.read_error, &mut out.write_error))
}

/// .xz Streams one after another, as liblzma's concatenated mode reads them: Stream Padding
/// between and after them in multiples of four bytes, and anything else after the first Stream
/// an error (a partial Stream Header: unexpected end of input).
fn decode_xz<R: BufRead, W: Write>(o: &Opts, r: &mut R, out: &mut W) -> io::Result<()> {
    let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "corrupt");
    let mut header: Vec<u8> = vec![];
    loop {
        {
            let mut s = lzma_rust2::XzReader::new(io::Cursor::new(&header[..]).chain(&mut *r), false);
            io::copy(&mut s, out)?;
        }
        if o.single_stream {
            return Ok(());
        }
        let mut padding = 0usize;
        loop {
            let buf = r.fill_buf()?;
            if buf.is_empty() {
                return if padding % 4 == 0 { Ok(()) } else { Err(corrupt()) };
            }
            let zeros = buf.iter().take_while(|&&b| b == 0).count();
            let more = zeros < buf.len();
            r.consume(zeros);
            padding += zeros;
            if more {
                break;
            }
        }
        if padding % 4 != 0 {
            return Err(corrupt());
        }
        header = peek(r, 12)?;
        if header.len() < 12 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if header[..6] != xzindex::MAGIC {
            return Err(corrupt());
        }
    }
}

fn compressed_suffix(o: &Opts) -> String {
    if let Some(s) = &o.suffix {
        return s.clone();
    }
    match o.format {
        Format::Lzma => ".lzma".into(),
        Format::Lzip => ".lz".into(),
        _ => ".xz".into(),
    }
}

/// The destination's name, or None (with the warning given) when the file is skipped.
fn dest_name(o: &Opts, st: &mut State, src: &str) -> Option<String> {
    let has = |suf: &str| src.len() > suf.len() && src.ends_with(suf) && !src[..src.len() - suf.len()].ends_with('/');
    if o.mode == Mode::Compress {
        let known: &[&str] = match o.format {
            Format::Lzma => &[".lzma", ".tlz"],
            Format::Lzip => &[".lz", ".tlz"],
            Format::Raw => &[],
            _ => &[".xz", ".txz"],
        };
        for s in known.iter().copied().chain(o.suffix.as_deref()) {
            if has(s) {
                st.warning(&format!("{}: File already has '{}' suffix, skipping", mask(src), s));
                return None;
            }
        }
        return Some(format!("{src}{}", compressed_suffix(o)));
    }
    if o.format != Format::Raw {
        for (c, u) in [(".xz", ""), (".txz", ".tar"), (".lzma", ""), (".tlz", ".tar"), (".lz", "")] {
            if has(c) {
                return Some(format!("{}{}", &src[..src.len() - c.len()], u));
            }
        }
    }
    if let Some(s) = &o.suffix {
        if has(s) {
            return Some(src[..src.len() - s.len()].to_string());
        }
    }
    st.warning(&format!("{}: Filename has an unknown suffix, skipping", mask(src)));
    None
}

fn process(o: &Opts, st: &mut State, name: &str, pos: usize, total: usize) {
    let to_stdout = o.stdout || o.mode == Mode::Test || name == "-";
    let shown = if name == "-" { "(stdin)".to_string() } else { mask(name) };
    let started = Instant::now();

    // The source.
    let (file, meta): (Box<dyn Read>, Option<std::fs::Metadata>) = if name == "-" {
        (Box::new(io::stdin().lock()), None)
    } else {
        let path = Path::new(name);
        let meta = match source_meta(path, to_stdout || o.force) {
            Ok(m) => m,
            Err(e) => return st.error(&format!("{}: {}", shown, errstr(&e))),
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            return st.warning(&format!("{shown}: Is a symbolic link, skipping"));
        }
        if ft.is_dir() {
            return st.warning(&format!("{shown}: Is a directory, skipping"));
        }
        if !to_stdout {
            if !ft.is_file() {
                return st.warning(&format!("{shown}: Not a regular file, skipping"));
            }
            if !o.force && !o.keep {
                if meta.mode() & 0o6000 != 0 {
                    return st.warning(&format!("{shown}: File has setuid or setgid bit set, skipping"));
                }
                if meta.mode() & 0o1000 != 0 {
                    return st.warning(&format!("{shown}: File has sticky bit set, skipping"));
                }
                if meta.nlink() > 1 {
                    return st.warning(&format!("{shown}: Input file has more than one hard link, skipping"));
                }
            }
        } else if ft.is_fifo() || ft.is_socket() || ft.is_block_device() || ft.is_char_device() {
            // Fine to stream from (xz reads from devices and pipes with -c).
        }
        match File::open(path) {
            Ok(f) => (Box::new(f), Some(meta)),
            Err(e) => return st.error(&format!("{}: {}", shown, errstr(&e))),
        }
    };

    // What to decode it as, known before the destination is named (as xz: an unrecognized
    // file is an error even when its name would be skipped).
    let mut input = SrcReader::new(BufReader::with_capacity(1 << 16, file));
    let mut head = vec![];
    let mut format = None;
    if o.mode != Mode::Compress {
        head = match peek(&mut input, 13) {
            Ok(h) => h,
            Err(e) => return st.error(&format!("{}: Read error: {}", shown, errstr(&e))),
        };
        format = match detect(o, &head, o.mode == Mode::Decompress && o.stdout && o.force && o.format == Format::Auto) {
            Ok(f) => f,
            Err(e) => return st.error(&format!("{}: {}", shown, codec_msg(&e))),
        };
    }

    // The destination.
    let dest_path: Option<PathBuf> = if to_stdout {
        None
    } else {
        match dest_name(o, st, name) {
            Some(d) => Some(PathBuf::from(d)),
            None => return,
        }
    };
    let dest_file = match &dest_path {
        None => None,
        Some(p) => {
            if let Ok(m) = std::fs::symlink_metadata(p) {
                if !o.force {
                    return st.error(&format!("{}: File exists", mask(&p.to_string_lossy())));
                }
                if !m.file_type().is_file() && !m.file_type().is_symlink() {
                    return st.error(&format!("{}: Destination is not a regular file", mask(&p.to_string_lossy())));
                }
            }
            match create_dest(p, o.force) {
                Ok(f) => {
                    pending_output(Some(p));
                    Some(f)
                }
                Err(e) => return st.error(&format!("{}: {}", mask(&p.to_string_lossy()), errstr(&e))),
            }
        }
    };

    // &File writes (the guest's std has no File::try_clone).
    let sink: Box<dyn Write + '_> = match &dest_file {
        Some(f) => Box::new(f),
        None if o.mode == Mode::Test => Box::new(Sink),
        None => Box::new(io::stdout().lock()),
    };
    let mut out = DestWriter::new(BufWriter::with_capacity(1 << 16, sink));

    let result = match o.mode {
        Mode::Compress => compress(o, &mut input, &mut out).map_err(|e| classify(e, &mut input.read_error, &mut out.write_error)),
        _ => decode(o, format, head, &mut input, &mut out),
    }
    .and_then(|_| out.flush().map_err(CodecError::Write));
    let produced = out.count;
    drop(out);

    match result {
        Ok(_) => {
            if let (Some(f), Some(p)) = (&dest_file, &dest_path) {
                if let Some(m) = &meta {
                    for w in copy_metadata(f, m) {
                        st.warning(&format!("{}: {}", mask(&p.to_string_lossy()), w));
                    }
                }
                if let Err(e) = f.sync_all().and(Ok(())) {
                    let _ = unlink(p);
                    pending_output(None);
                    return st.error(&format!("{}: Closing the file failed: {}", mask(&p.to_string_lossy()), errstr(&e)));
                }
            }
            pending_output(None);
            if o.verbosity >= V_VERBOSE {
                let (c, u) = if o.mode == Mode::Compress { (produced, input.count) } else { (input.count, produced) };
                progress_line(&shown, pos, total, c, u, started);
            }
            if dest_path.is_some() && !o.keep && name != "-" {
                if let Err(e) = std::fs::remove_file(name) {
                    st.warning(&format!("{}: Cannot remove: {}", shown, errstr(&e)));
                }
            }
        }
        Err(e) => {
            if let Some(p) = &dest_path {
                let _ = unlink(p);
            }
            pending_output(None);
            match &e {
                CodecError::Write(w) if w.kind() == io::ErrorKind::BrokenPipe => std::process::exit(1),
                CodecError::Write(_) => {
                    let n = dest_path.as_ref().map(|p| mask(&p.to_string_lossy())).unwrap_or_else(|| "(stdout)".into());
                    st.error(&format!("{}: {}", n, codec_msg(&e)));
                }
                _ => st.error(&format!("{}: {}", shown, codec_msg(&e))),
            }
        }
    }
}

/// "1.2 KiB" as xz writes sizes: bytes below 10000, else the largest binary unit that keeps the
/// number at most 9999.9, with one decimal.
fn nicestr(v: u64, also_bytes: bool) -> String {
    let mut s = if v < 10000 {
        format!("{v} B")
    } else {
        let mut d = v as f64;
        let mut unit = 0;
        loop {
            d /= 1024.0;
            unit += 1;
            if !(d > 9999.9 && unit < 4) {
                break;
            }
        }
        format!("{:.1} {}", d, ["B", "KiB", "MiB", "GiB", "TiB"][unit])
    };
    if also_bytes && v >= 10000 {
        s += &format!(" ({v} B)");
    }
    s
}

fn ratio(c: u64, u: u64) -> String {
    if u == 0 {
        return "---".into();
    }
    let r = c as f64 / u as f64;
    if r > 9.999 {
        "---".into()
    } else {
        format!("{r:.3}")
    }
}

fn progress_line(name: &str, pos: usize, total: usize, c: u64, u: u64, started: Instant) {
    let secs = started.elapsed().as_secs_f64();
    let r = if u > 0 { c as f64 / u as f64 } else { 16.0 };
    let sizes = format!("{} / {}{}", nicestr(c, false), nicestr(u, false), if r > 9.999 { " > 9.999".into() } else { format!(" = {r:.3}") });
    let speed = if secs >= 3.0 {
        let mut d = u as f64 / secs / 1024.0;
        let mut unit = 0;
        while d > 9999.9 && unit < 3 {
            d /= 1024.0;
            unit += 1;
        }
        format!("{:.1} {}", d, ["KiB/s", "MiB/s", "GiB/s", "TiB/s"][unit])
    } else {
        String::new()
    };
    let t = secs as u64;
    let time = if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) };
    if stderr_is_tty() {
        if !(total == 1 && name == "(stdin)") {
            eprintln!("{name} ({pos}/{total})");
        }
        eprintln!("  100 % {:>35}   {:>9} {:>10}", sizes, speed, time);
    } else {
        let mut line = format!("{name}: {sizes}");
        if !speed.is_empty() {
            line += &format!(", {speed}");
        }
        line += &format!(", {time}");
        eprintln!("{line}");
    }
}

#[derive(Default)]
struct ListTotals {
    printed: bool,
    files: u64,
    streams: u64,
    blocks: u64,
    compressed: u64,
    uncompressed: u64,
    padding: u64,
    checks: u32,
}

fn check_names(mask_: u32, sep: &str) -> String {
    let m = if mask_ == 0 { 1 } else { mask_ };
    let mut out = vec![];
    for i in 0..16u8 {
        if m & (1 << i) != 0 {
            out.push(check_name(i));
        }
    }
    out.join(sep)
}

fn check_name(i: u8) -> String {
    match i {
        0 => "None".into(),
        1 => "CRC32".into(),
        4 => "CRC64".into(),
        10 => "SHA-256".into(),
        n => format!("Unknown-{n}"),
    }
}

fn list_file(o: &Opts, st: &mut State, name: &str, pos: usize, total: usize, totals: &mut ListTotals) {
    let shown = mask(name);
    let meta = match std::fs::metadata(name) {
        Ok(m) => m,
        Err(e) => return st.error(&format!("{}: {}", shown, errstr(&e))),
    };
    if meta.is_dir() {
        return st.warning(&format!("{shown}: Is a directory, skipping"));
    }
    if !meta.is_file() {
        return st.warning(&format!("{shown}: Not a regular file, skipping"));
    }
    let mut f = match File::open(name) {
        Ok(f) => f,
        Err(e) => return st.error(&format!("{}: {}", shown, errstr(&e))),
    };
    // In verbose mode the name comes first, before anything can go wrong with the file.
    if o.verbosity >= V_VERBOSE && !o.robot {
        if totals.printed {
            println!();
        }
        totals.printed = true;
        println!("{shown} ({pos}/{total})");
    }
    let info = match xzindex::parse(&mut f) {
        Ok(i) => i,
        Err(xzindex::ListError::Io(e)) => return st.error(&format!("{}: Read error: {}", shown, errstr(&e))),
        Err(xzindex::ListError::Format) => return st.error(&format!("{shown}: File format not recognized")),
        Err(xzindex::ListError::Corrupt) => return st.error(&format!("{shown}: Compressed data is corrupt")),
        Err(xzindex::ListError::TooSmall) => return st.error(&format!("{shown}: Too small to be a valid .xz file")),
    };
    totals.files += 1;
    totals.streams += info.streams.len() as u64;
    totals.blocks += info.block_count();
    totals.compressed += info.file_size;
    totals.uncompressed += info.uncompressed_size();
    totals.padding += info.padding();
    totals.checks |= info.checks();

    if o.robot {
        println!("name\t{shown}");
        println!(
            "file\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            info.streams.len(),
            info.block_count(),
            info.file_size,
            info.uncompressed_size(),
            ratio(info.file_size, info.uncompressed_size()),
            check_names(info.checks(), ","),
            info.padding()
        );
        if o.verbosity >= V_VERBOSE {
            for s in &info.streams {
                println!(
                    "stream\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    s.number,
                    s.blocks.len(),
                    s.compressed_offset,
                    s.uncompressed_offset,
                    s.compressed_size,
                    s.uncompressed_size,
                    ratio(s.compressed_size, s.uncompressed_size),
                    check_name(s.check),
                    s.padding
                );
            }
            for s in &info.streams {
                for b in &s.blocks {
                    println!(
                        "block\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        s.number,
                        b.number_in_stream,
                        b.number_in_file,
                        b.compressed_file_offset,
                        b.uncompressed_file_offset,
                        b.total_size,
                        b.uncompressed_size,
                        ratio(b.total_size, b.uncompressed_size),
                        check_name(s.check)
                    );
                }
            }
        }
        return;
    }
    if o.verbosity < V_VERBOSE {
        if totals.files == 1 {
            println!("Strms  Blocks   Compressed Uncompressed  Ratio  Check   Filename");
        }
        println!(
            "{:>5} {:>7}  {:>11}  {:>11}  {:>5}  {:<7} {}",
            info.streams.len(),
            info.block_count(),
            nicestr(info.file_size, false),
            nicestr(info.uncompressed_size(), false),
            ratio(info.file_size, info.uncompressed_size()),
            check_names(info.checks(), ","),
            shown
        );
        return;
    }
    adv_summary(info.streams.len() as u64, info.block_count(), info.file_size, info.uncompressed_size(), info.checks(), info.padding());
    println!("  Streams:");
    println!("    Stream    Blocks      CompOffset    UncompOffset        CompSize      UncompSize  Ratio  Check      Padding");
    for s in &info.streams {
        println!(
            "    {:>6} {:>9} {:>15} {:>15} {:>15} {:>15}  {:>5}  {:<10} {:>7}",
            s.number,
            s.blocks.len(),
            s.compressed_offset,
            s.uncompressed_offset,
            s.compressed_size,
            s.uncompressed_size,
            ratio(s.compressed_size, s.uncompressed_size),
            check_name(s.check),
            s.padding
        );
    }
    if info.block_count() > 0 {
        println!("  Blocks:");
        println!("    Stream     Block      CompOffset    UncompOffset       TotalSize      UncompSize  Ratio  Check");
        for s in &info.streams {
            for b in &s.blocks {
                println!(
                    "    {:>6} {:>9} {:>15} {:>15} {:>15} {:>15}  {:>5}  {}",
                    s.number,
                    b.number_in_stream,
                    b.compressed_file_offset,
                    b.uncompressed_file_offset,
                    b.total_size,
                    b.uncompressed_size,
                    ratio(b.total_size, b.uncompressed_size),
                    check_name(s.check)
                );
            }
        }
    }
}

fn adv_summary(streams: u64, blocks: u64, c: u64, u: u64, checks: u32, padding: u64) {
    println!("  Streams:           {streams}");
    println!("  Blocks:            {blocks}");
    println!("  Compressed size:   {}", nicestr(c, true));
    println!("  Uncompressed size: {}", nicestr(u, true));
    println!("  Ratio:             {}", ratio(c, u));
    println!("  Check:             {}", check_names(checks, ", "));
    println!("  Stream Padding:    {}", nicestr(padding, true));
}

fn list_totals(o: &Opts, t: &ListTotals) {
    if o.robot {
        println!(
            "totals\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            t.streams,
            t.blocks,
            t.compressed,
            t.uncompressed,
            ratio(t.compressed, t.uncompressed),
            check_names(t.checks, ","),
            t.padding,
            t.files
        );
        return;
    }
    if t.files <= 1 {
        return;
    }
    if o.verbosity < V_VERBOSE {
        println!("{}", "-".repeat(79));
        println!(
            "{:>5} {:>7}  {:>11}  {:>11}  {:>5}  {:<7} {} files",
            t.streams,
            t.blocks,
            nicestr(t.compressed, false),
            nicestr(t.uncompressed, false),
            ratio(t.compressed, t.uncompressed),
            check_names(t.checks, ","),
            t.files
        );
    } else {
        println!("\nTotals:");
        println!("  Number of files:   {}", t.files);
        adv_summary(t.streams, t.blocks, t.compressed, t.uncompressed, t.checks, t.padding);
    }
}
