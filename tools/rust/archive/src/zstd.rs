//! zstd, unzstd, zstdcat, zstdmt: the Zstandard command line (1.5.7) over structured-zstd.
//!
//! What the original does is kept: its argument syntax (aggregated short options, `-o`, `-#`,
//! `--fast`, `--ultra`, `--long`, `--zstd=...`), its file handling (sources kept unless --rm,
//! `-o` and `-c` concatenating, -r, --filelist, --output-dir-flat, --exclude-compressed, the
//! overwrite question), its messages and summaries, `--list`, checksums written and verified,
//! the content size in the header, dictionaries (-D), pass-through (zstdcat, -dcf), and the
//! gzip, xz and lzma formats (--format=, and their files when decompressing).
//! Left out: benchmarking (-b), dictionary training (--train), --patch-from and --adapt (they
//! say so). -T is accepted; the encoder is single-threaded, which gives the same format.

use crate::common::{errstr, stdin_is_tty, stdout_is_tty};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use structured_zstd::decoding::{ContentChecksum, FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{CompressionLevel, CompressionParameters, StreamingEncoder};

const VERSION: &str = "v1.5.7";
const STDIN: &str = "/*stdin*\\";
const STDOUT: &str = "/*stdout*\\";
const NULL_DEV: &str = "/dev/null";
const CLEVEL_DEFAULT: i32 = 3;
const CLEVEL_MAX: i32 = 19;
const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_BASE: u32 = 0x184D_2A50;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Compress,
    Decompress,
    Test,
    List,
    Train,
    Bench,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CType {
    Zstd,
    Gzip,
    Xz,
    Lzma,
}

struct Prefs {
    level: i32,
    ultra: bool,
    long_wlog: Option<u32>,
    params: Vec<(String, u32)>,
    overwrite: bool,
    remove_src: bool,
    checksum: u8, // 0: no, 1: default (on), 2: asked for
    content_size: bool,
    dict_id: bool,
    pass_through: i8, // -1: default
    exclude_compressed: bool,
    ctype: CType,
    suffix: &'static str,
    test: bool,
    dict: Option<Vec<u8>>,
    stream_size: Option<u64>,
    size_hint: Option<u64>,
    literals: Option<bool>,
    target_block: Option<u32>,
}

struct Ctx {
    level: i32, // display level
    nb_files: usize,
    processed: usize,
    bytes_in: u64,
    bytes_out: u64,
    has_stdin_input: bool,
    has_stdout: bool,
}

macro_rules! display {
    ($ctx:expr, $l:expr, $($arg:tt)*) => {
        if $ctx.level >= $l { eprint!($($arg)*); }
    };
}

fn read_u32(s: &mut &str) -> u32 {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let mut v: u64 = 0;
    for c in s[..digits].bytes() {
        v = v * 10 + (c - b'0') as u64;
        if v > u32::MAX as u64 {
            eprintln!("error: numeric value overflows 32-bit unsigned int ");
            std::process::exit(1);
        }
    }
    *s = &s[digits..];
    let shift = match s.as_bytes().first() {
        Some(b'K') => 10,
        Some(b'M') => 20,
        Some(b'G') => 30,
        _ => 0,
    };
    if shift > 0 {
        v <<= shift;
        if v > u32::MAX as u64 {
            eprintln!("error: numeric value overflows 32-bit unsigned int ");
            std::process::exit(1);
        }
        *s = &s[1..];
        if s.starts_with('i') {
            *s = &s[1..];
        }
        if s.starts_with('B') {
            *s = &s[1..];
        }
    }
    v as u32
}

fn read_u64(s: &mut &str) -> u64 {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let mut v: u64 = s[..digits].parse().unwrap_or(0);
    *s = &s[digits..];
    let shift = match s.as_bytes().first() {
        Some(b'K') => 10,
        Some(b'M') => 20,
        Some(b'G') => 30,
        _ => 0,
    };
    if shift > 0 {
        v <<= shift;
        *s = &s[1..];
        if s.starts_with('i') {
            *s = &s[1..];
        }
        if s.starts_with('B') {
            *s = &s[1..];
        }
    }
    v
}

fn usage_short(p: &str) -> String {
    format!(
        r#"Compress or decompress the INPUT file(s); reads from STDIN if INPUT is `-` or not provided.

Usage: {p} [OPTIONS...] [INPUT... | -] [-o OUTPUT]

Options:
  -o OUTPUT                     Write output to a single file, OUTPUT.
  -c, --stdout                  Write to STDOUT (even if it is a console) and keep the INPUT file(s).
  -k, --keep                    Preserve INPUT file(s). [Default] 
  --rm                          Remove INPUT file(s) after successful (de)compression to file.

  -#                            Desired compression level, where `#` is a number between 1 and {CLEVEL_MAX};
                                lower numbers provide faster compression, higher numbers yield
                                better compression ratios. [Default: {CLEVEL_DEFAULT}]

  -d, --decompress              Perform decompression.
  -D DICT                       Use DICT as the dictionary for compression or decompression.

  -f, --force                   Disable input and output checks. Allows overwriting existing files,
                                receiving input from the console, printing output to STDOUT, and
                                operating on links, block devices, etc. Unrecognized formats will be
                                passed-through through as-is.

  -h                            Display short usage and exit.
  -H, --help                    Display full help and exit.
  -V, --version                 Display the program version and exit.

"#
    )
}

fn welcome() -> String {
    format!("*** Zstandard CLI (collaboCore, structured-zstd) (32-bit) {VERSION} ***\n")
}

fn usage_long(p: &str) -> String {
    let mut s = welcome();
    s += "\n";
    s += &usage_short(p);
    s += r#"Advanced options:
  -v, --verbose                 Enable verbose output; pass multiple times to increase verbosity.
  -q, --quiet                   Suppress warnings; pass twice to suppress errors.

  --[no-]progress               Forcibly show/hide the progress counter. NOTE: Any (de)compressed
                                output to terminal will mix with progress counter text.

  -r                            Operate recursively on directories.
  --filelist LIST               Read a list of files to operate on from LIST.
  --output-dir-flat DIR         Store processed files in DIR.

  --[no-]check                  Add XXH64 integrity checksums during compression. [Default: Add, Validate]
                                If `-d` is present, ignore/validate checksums during decompression.

  --                            Treat remaining arguments after `--` as files.

Advanced compression options:
  --ultra                       Enable levels beyond 19, up to 22; requires more memory.
  --fast[=#]                    Use to very fast compression levels. [Default: 1]
  --long[=#]                    Enable long distance matching with window log #. [Default: 27]
  -T#                           Spawn # compression threads. [Default: 1; pass 0 for core count.]
  --single-thread               Share a single thread for I/O and compression (slightly different than `-T1`).

  --exclude-compressed          Only compress files that are not already compressed.

  --stream-size=#               Specify size of streaming input from STDIN.
  --size-hint=#                 Optimize compression parameters for streaming input of approximately size #.
  --target-compressed-block-size=#
                                Generate compressed blocks of approximately # size.

  --no-dictID                   Don't write `dictID` into the header (dictionary compression only).
  --[no-]compress-literals      Force (un)compressed literals.

  --format=zstd                 Compress files to the `.zst` format. [Default]
  --format=gzip                 Compress files to the `.gz` format.
  --format=xz                   Compress files to the `.xz` format.
  --format=lzma                 Compress files to the `.lzma` format.

Advanced decompression options:
  -l                            Print information about Zstandard-compressed files.
  --test                        Test compressed file integrity.
  -M#                           Set the memory usage limit to # megabytes.
  --[no-]sparse                 Enable sparse mode. [Default: Disabled]
"#;
    let pt = if p == "zstdcat" || p == "zcat" || p == "gzcat" { "Enabled" } else { "Disabled" };
    s += &format!("  --[no-]pass-through           Pass through uncompressed files as-is. [Default: {pt}]\n");
    s
}

struct Args {
    op: Op,
    files: Vec<String>,
    filelists: Vec<String>,
    out: Option<String>,
    out_dir: Option<String>,
    dict_file: Option<String>,
    recursive: bool,
    follow_links: bool,
    force_stdin: bool,
    force_stdout: bool,
    allow_block: bool,
    level_set: bool,
    threads_non1: bool,
}

fn bad_usage(ctx_level: i32, p: &str, arg: &str) -> ! {
    if ctx_level >= 1 {
        eprintln!("Incorrect parameter: {arg} ");
    }
    if ctx_level >= 2 {
        eprint!("{}", usage_short(p));
    }
    std::process::exit(1)
}

pub fn main(argv0: &str, argv: Vec<String>) -> i32 {
    let p = argv0.to_string();
    let mut level: i32 = 2;
    let mut prefs = Prefs {
        level: CLEVEL_DEFAULT,
        ultra: false,
        long_wlog: None,
        params: vec![],
        overwrite: false,
        remove_src: false,
        checksum: 1,
        content_size: true,
        dict_id: true,
        pass_through: -1,
        exclude_compressed: false,
        ctype: CType::Zstd,
        suffix: ".zst",
        test: false,
        dict: None,
        stream_size: None,
        size_hint: None,
        literals: None,
        target_block: None,
    };
    let mut a = Args {
        op: Op::Compress,
        files: vec![],
        filelists: vec![],
        out: None,
        out_dir: None,
        dict_file: None,
        recursive: false,
        follow_links: false,
        force_stdin: false,
        force_stdout: false,
        allow_block: false,
        level_set: false,
        threads_non1: false,
    };
    // ZSTD_CLEVEL, as init_cLevel reads it.
    if let Ok(env) = std::env::var("ZSTD_CLEVEL") {
        let mut s = env.as_str();
        let neg = s.starts_with('-');
        if neg || s.starts_with('+') {
            s = &s[1..];
        }
        if s.starts_with(|c: char| c.is_ascii_digit()) {
            let mut t = s;
            let v = read_u32(&mut t) as i32;
            if t.is_empty() {
                prefs.level = if neg { -v } else { v };
            } else {
                eprintln!("Ignore environment variable setting ZSTD_CLEVEL={env}: not a valid integer value ");
            }
        } else {
            eprintln!("Ignore environment variable setting ZSTD_CLEVEL={env}: not a valid integer value ");
        }
    }
    match p.as_str() {
        "unzstd" => a.op = Op::Decompress,
        "zstdcat" | "zcat" | "gzcat" => {
            a.op = Op::Decompress;
            prefs.overwrite = true;
            a.force_stdout = true;
            a.follow_links = true;
            prefs.pass_through = 1;
            a.out = Some(STDOUT.into());
            level = 1;
        }
        _ => {}
    }

    let mut i = 0;
    let mut only_files = false;
    while i < argv.len() {
        let arg = argv[i].clone();
        i += 1;
        if only_files {
            a.files.push(arg);
            continue;
        }
        if arg == "-" {
            a.files.push(STDIN.into());
            continue;
        }
        if !arg.starts_with('-') {
            a.files.push(arg);
            continue;
        }
        // NEXT_FIELD: the value after '=' or in the next word, which may not be an option.
        let next_field = |rest: &str, i: &mut usize| -> String {
            if let Some(v) = rest.strip_prefix('=') {
                return v.to_string();
            }
            if *i >= argv.len() {
                eprintln!("error: missing command argument ");
                std::process::exit(1);
            }
            let v = argv[*i].clone();
            *i += 1;
            if v.starts_with('-') {
                eprintln!("error: command cannot be separated from its argument by another command ");
                std::process::exit(1);
            }
            v
        };
        let numeric = |v: &str| -> u32 {
            let mut s = v;
            let n = read_u32(&mut s);
            if !s.is_empty() {
                eprintln!("error: only numeric values with optional suffixes K, KB, KiB, M, MB, MiB, G, GB, GiB are allowed ");
                std::process::exit(1);
            }
            n
        };
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "" => {
                    only_files = true;
                    continue;
                }
                "list" => a.op = Op::List,
                "compress" => a.op = Op::Compress,
                "decompress" | "uncompress" => a.op = Op::Decompress,
                "force" => {
                    prefs.overwrite = true;
                    a.force_stdin = true;
                    a.force_stdout = true;
                    a.follow_links = true;
                    a.allow_block = true;
                }
                "version" => {
                    print_version(level);
                    return 0;
                }
                "help" => {
                    print!("{}", usage_long(&p));
                    return 0;
                }
                "verbose" => level += 1,
                "quiet" => level -= 1,
                "stdout" => {
                    a.force_stdout = true;
                    a.out = Some(STDOUT.into());
                }
                "ultra" => prefs.ultra = true,
                "check" => prefs.checksum = 2,
                "no-check" => prefs.checksum = 0,
                "sparse" | "no-sparse" | "asyncio" | "no-asyncio" | "no-row-match-finder" | "row-match-finder" | "mmap-dict" | "no-mmap-dict" | "rsyncable"
                | "single-thread" | "priority=rt" | "no-progress" | "progress" | "fake-stdin-is-console" | "fake-stdout-is-console"
                | "fake-stderr-is-console" | "trace-file-stat" => {}
                "pass-through" => prefs.pass_through = 1,
                "no-pass-through" => prefs.pass_through = 0,
                "test" => a.op = Op::Test,
                "train" => a.op = Op::Train,
                "no-dictID" => prefs.dict_id = false,
                "keep" => prefs.remove_src = false,
                "rm" => prefs.remove_src = true,
                "show-default-cparams" => {}
                "content-size" => prefs.content_size = true,
                "no-content-size" => prefs.content_size = false,
                "adapt" => {
                    eprintln!("zstd: --adapt is not supported by this zstd ");
                    return 1;
                }
                "format=zstd" => {
                    prefs.suffix = ".zst";
                    prefs.ctype = CType::Zstd;
                }
                "format=gzip" => {
                    prefs.suffix = ".gz";
                    prefs.ctype = CType::Gzip;
                }
                "format=xz" => {
                    prefs.suffix = ".xz";
                    prefs.ctype = CType::Xz;
                }
                "format=lzma" => {
                    prefs.suffix = ".lzma";
                    prefs.ctype = CType::Lzma;
                }
                "compress-literals" => prefs.literals = Some(true),
                "no-compress-literals" => prefs.literals = Some(false),
                "exclude-compressed" => prefs.exclude_compressed = true,
                "max" => {
                    eprintln!("--max is incompatible with 32-bit mode ");
                    bad_usage(level, &p, &arg);
                }
                _ => {
                    // Long options with an argument.
                    let with = |name: &str| long.strip_prefix(name);
                    if let Some(r) = with("threads") {
                        a.threads_non1 = numeric(&next_field(r, &mut i)) != 1;
                    } else if let Some(r) = with("memlimit-decompress").or_else(|| with("memlimit")).or_else(|| with("memory")) {
                        numeric(&next_field(r, &mut i));
                    } else if let Some(r) = with("block-size").or_else(|| with("split")).or_else(|| with("jobsize")) {
                        numeric(&next_field(r, &mut i));
                    } else if let Some(r) = with("maxdict").or_else(|| with("dictID")) {
                        numeric(&next_field(r, &mut i));
                    } else if let Some(r) = with("zstd=") {
                        if !parse_cparams(r, &mut prefs.params) {
                            bad_usage(level, &p, &arg);
                        }
                        prefs.ctype = CType::Zstd;
                    } else if let Some(r) = with("stream-size") {
                        let mut v = next_field(r, &mut i);
                        let mut s = v.as_str();
                        prefs.stream_size = Some(read_u64(&mut s));
                        v.clear();
                    } else if let Some(r) = with("target-compressed-block-size") {
                        prefs.target_block = Some(numeric(&next_field(r, &mut i)));
                    } else if let Some(r) = with("size-hint") {
                        prefs.size_hint = Some(numeric(&next_field(r, &mut i)) as u64);
                    } else if let Some(r) = with("output-dir-flat") {
                        let d = next_field(r, &mut i);
                        if d.is_empty() {
                            eprintln!("error: output dir cannot be empty string (did you mean to pass '.' instead?)");
                            return 1;
                        }
                        a.out_dir = Some(d);
                    } else if let Some(r) = with("auto-threads") {
                        next_field(r, &mut i);
                    } else if with("patch-from").is_some() || with("patch-apply").is_some() || with("output-dir-mirror").is_some() || with("trace").is_some() {
                        eprintln!("zstd: {} is not supported by this zstd ", arg.split('=').next().unwrap_or(&arg));
                        return 1;
                    } else if let Some(r) = with("train-") {
                        let _ = r;
                        a.op = Op::Train;
                    } else if let Some(r) = with("long") {
                        prefs.ultra = true;
                        if let Some(v) = r.strip_prefix('=') {
                            let mut s = v;
                            prefs.long_wlog = Some(read_u32(&mut s));
                        } else if !r.is_empty() {
                            bad_usage(level, &p, &arg);
                        } else {
                            prefs.long_wlog = Some(27);
                        }
                    } else if let Some(r) = with("fast") {
                        if let Some(v) = r.strip_prefix('=') {
                            let mut s = v;
                            let f = read_u32(&mut s).min(131072);
                            if f == 0 {
                                bad_usage(level, &p, &arg);
                            }
                            prefs.level = -(f as i32);
                        } else if !r.is_empty() {
                            bad_usage(level, &p, &arg);
                        } else {
                            prefs.level = -1;
                        }
                        a.level_set = true;
                    } else if let Some(r) = with("filelist") {
                        a.filelists.push(next_field(r, &mut i));
                    } else {
                        bad_usage(level, &p, &arg);
                    }
                }
            }
            continue;
        }
        // Short options, aggregated: -d19kq
        let mut s: &str = &arg[1..];
        while !s.is_empty() {
            let c = s.as_bytes()[0];
            if c.is_ascii_digit() {
                prefs.level = read_u32(&mut s) as i32;
                a.level_set = true;
                continue;
            }
            s = &s[1..];
            match c {
                b'V' => {
                    print_version(level);
                    return 0;
                }
                b'H' => {
                    print!("{}", usage_long(&p));
                    return 0;
                }
                b'h' => {
                    print!("{}", usage_short(&p));
                    return 0;
                }
                b'z' => a.op = Op::Compress,
                b'd' => a.op = if a.op == Op::Bench { Op::Bench } else { Op::Decompress },
                b'c' => {
                    a.force_stdout = true;
                    a.out = Some(STDOUT.into());
                }
                b'o' => a.out = Some(next_field(s, &mut i)),
                b'n' => {}
                b'D' => a.dict_file = Some(next_field(s, &mut i)),
                b'f' => {
                    prefs.overwrite = true;
                    a.force_stdin = true;
                    a.force_stdout = true;
                    a.follow_links = true;
                    a.allow_block = true;
                }
                b'v' => level += 1,
                b'q' => level -= 1,
                b'k' => prefs.remove_src = false,
                b'C' => prefs.checksum = 2,
                b't' => a.op = Op::Test,
                b'M' => {
                    read_u32(&mut s);
                }
                b'l' => a.op = Op::List,
                b'r' => a.recursive = true,
                b'T' => a.threads_non1 = read_u32(&mut s) != 1,
                b'B' | b's' | b'e' | b'i' | b'P' => {
                    read_u32(&mut s);
                }
                b'b' => a.op = Op::Bench,
                b'S' | b'p' => {}
                _ => bad_usage(level, &p, &format!("-{}", c as char)),
            }
        }
    }

    if a.op == Op::Decompress && a.threads_non1 {
        if level >= 2 {
            eprintln!("Warning : decompression does not support multi-threading");
        }
    }
    let mut ctx = Ctx { level, nb_files: 0, processed: 0, bytes_in: 0, bytes_out: 0, has_stdin_input: false, has_stdout: false };
    display!(ctx, 3, "{}", welcome());
    if a.op == Op::Bench {
        display!(ctx, 1, "zstd: benchmark mode not available in this build \n");
        return 1;
    }

    // Symbolic links are left alone unless followed (-f, zstdcat).
    if !a.follow_links {
        let before = a.files.len();
        a.files.retain(|f| {
            let link = f != STDIN && std::fs::symlink_metadata(f).map(|m| m.file_type().is_symlink()).unwrap_or(false);
            let fifo = std::fs::metadata(f).map(|m| m.file_type().is_fifo()).unwrap_or(false);
            if link && !fifo {
                display!(ctx, 2, "Warning : {} is a symbolic link, ignoring \n", f);
                false
            } else {
                true
            }
        });
        if a.files.is_empty() && before > 0 {
            return 1;
        }
    }
    for list in &a.filelists {
        match std::fs::read(list) {
            Ok(data) => {
                for line in data.split(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(line).trim_end_matches('\r').to_string();
                    if !line.is_empty() {
                        a.files.push(line);
                    }
                }
            }
            Err(_) => {
                display!(ctx, 1, "zstd: error reading {} \n", list);
                return 1;
            }
        }
    }
    let nb_input_names = a.files.len();
    if a.recursive {
        let mut expanded = vec![];
        for f in std::mem::take(&mut a.files) {
            expand(&f, a.follow_links, &mut expanded);
        }
        a.files = expanded;
    }

    if a.op == Op::List {
        return list_files(&ctx, &a.files);
    }
    if a.op == Op::Train {
        display!(ctx, 1, "training mode not available \n");
        return 1;
    }
    if a.op == Op::Test {
        prefs.test = true;
        a.out = Some(NULL_DEV.into());
        prefs.remove_src = false;
    }
    if a.files.is_empty() {
        if nb_input_names > 0 {
            display!(ctx, 1, "please provide correct input file(s) or non-empty directories -- ignored \n");
            return 0;
        }
        a.files.push(STDIN.into());
    }
    if a.files.len() == 1 && a.files[0] == STDIN && a.out.is_none() {
        a.out = Some(STDOUT.into());
    }
    let reads_stdin = a.files.iter().any(|f| f == STDIN);
    if !a.force_stdin && reads_stdin && stdin_is_tty() {
        display!(ctx, 1, "stdin is a console, aborting\n");
        return 1;
    }
    if a.out.as_deref().map_or(true, |o| o == STDOUT) && stdout_is_tty() && reads_stdin && !a.force_stdout && a.op != Op::Decompress {
        display!(ctx, 1, "stdout is a console, aborting\n");
        return 1;
    }
    let max_level = if prefs.ultra { 22 } else { CLEVEL_MAX };
    if prefs.level > max_level {
        display!(ctx, 2, "Warning : compression level higher than max, reduced to {}. ", max_level);
        display!(ctx, 2, "Specify --ultra to raise the limit to 22 and use --long=31 for maximum compression. Note that this requires high amounts of memory, and the resulting data might be rejected by third-party decoders and is therefore only recommended for archival purposes. \n");
        prefs.level = max_level;
    }
    ctx.has_stdout = a.out.as_deref() == Some(STDOUT);
    if ctx.has_stdout && ctx.level == 2 {
        ctx.level = 1;
    }
    if ctx.has_stdout && prefs.remove_src {
        display!(ctx, 3, "Note: src files are not removed when output is stdout \n");
        prefs.remove_src = false;
    }
    if let Some(d) = &a.dict_file {
        match std::fs::read(d) {
            Ok(v) => prefs.dict = Some(v),
            Err(e) => {
                eprintln!("zstd: error 33 : Couldn't open dictionary {}: {} ", d, errstr(&e));
                return 33;
            }
        }
    }
    ctx.nb_files = a.files.len();
    ctx.has_stdin_input = reads_stdin;
    crate::common::install_signal_cleanup();

    if a.op == Op::Compress {
        if a.files.len() == 1 && a.out.is_some() {
            compress_src(&mut ctx, &prefs, a.out.as_deref().unwrap(), &a.files[0], None, a.allow_block)
        } else {
            compress_many(&mut ctx, &mut prefs, &a)
        }
    } else if a.files.len() == 1 && a.out.is_some() {
        decompress_src(&mut ctx, &prefs, a.out.as_deref().unwrap(), &a.files[0], None, a.allow_block)
    } else {
        decompress_many(&mut ctx, &mut prefs, &a)
    }
}

fn print_version(level: i32) {
    if level < 2 {
        println!("{}", &VERSION[1..]);
    } else {
        print!("{}", welcome());
        if level >= 3 {
            println!("*** supports: zstd, gzip, lzma, xz ");
        }
    }
}

fn parse_cparams(s: &str, out: &mut Vec<(String, u32)>) -> bool {
    for part in s.split(',') {
        let Some((k, v)) = part.split_once('=') else { return false };
        let mut t = v;
        let n = read_u32(&mut t);
        if !t.is_empty() {
            return false;
        }
        let key = match k {
            "windowLog" | "wlog" => "wlog",
            "chainLog" | "clog" => "clog",
            "hashLog" | "hlog" => "hlog",
            "searchLog" | "slog" => "slog",
            "minMatch" | "mml" => "mml",
            "targetLength" | "tlen" => "tlen",
            "strategy" | "strat" => "strat",
            "overlapLog" | "ovlog" => continue,
            "ldmHashLog" | "lhlog" => "lhlog",
            "ldmMinMatch" | "lmml" => "lmml",
            "ldmBucketSizeLog" | "lblog" => "lblog",
            "ldmHashRateLog" | "lhrlog" => "lhrlog",
            _ => return false,
        };
        out.push((key.to_string(), n));
    }
    true
}

fn expand(path: &str, follow: bool, out: &mut Vec<String>) {
    let meta = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
    match meta {
        Ok(m) if m.is_dir() => {
            let entries: Vec<String> = match std::fs::read_dir(path) {
                Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
                Err(_) => return,
            };
            for e in entries {
                let child = if path.ends_with('/') { format!("{path}{e}") } else { format!("{path}/{e}") };
                expand(&child, follow, out);
            }
        }
        Ok(m) if m.file_type().is_symlink() => {}
        _ => out.push(path.to_string()),
    }
}

/// UTIL_makeHumanReadableSize: (value, precision, suffix).
fn hrs(level: i32, size: u64) -> (f64, usize, &'static str) {
    if level > 3 {
        if size >= 1 << 53 {
            return (size as f64 / (1u64 << 20) as f64, 2, " MiB");
        }
        return (size as f64, 0, " B");
    }
    let (v, suffix) = if size >= 1 << 60 {
        (size as f64 / (1u64 << 60) as f64, " EiB")
    } else if size >= 1 << 50 {
        (size as f64 / (1u64 << 50) as f64, " PiB")
    } else if size >= 1 << 40 {
        (size as f64 / (1u64 << 40) as f64, " TiB")
    } else if size >= 1 << 30 {
        (size as f64 / (1u64 << 30) as f64, " GiB")
    } else if size >= 1 << 20 {
        (size as f64 / (1u64 << 20) as f64, " MiB")
    } else if size >= 1 << 10 {
        (size as f64 / 1024.0, " KiB")
    } else {
        (size as f64, " B")
    };
    let p = if v >= 100.0 || v as u64 == size {
        0
    } else if v >= 10.0 {
        1
    } else if v > 1.0 {
        2
    } else {
        3
    };
    (v, p, suffix)
}

fn fmt_hrs(level: i32, size: u64, width: usize, suffix_width: usize) -> String {
    let (v, p, s) = hrs(level, size);
    format!("{:>w$.p$}{:>sw$}", v, s, w = width, p = p, sw = suffix_width)
}

const COMPRESSED_EXTS: &[&str] = &[
    ".zst", ".tzst", ".gz", ".tgz", ".lzma", ".xz", ".txz", ".lz4", ".tlz4", ".7z", ".aa3", ".aac", ".aar", ".ace", ".alac", ".ape", ".apk",
    ".apng", ".arc", ".archive", ".arj", ".ark", ".asf", ".avi", ".avif", ".ba", ".br", ".bz2", ".cab", ".cdx", ".chm", ".cr2", ".divx",
    ".dmg", ".dng", ".docm", ".docx", ".dotm", ".dotx", ".dsft", ".ear", ".eftx", ".emz", ".eot", ".epub", ".f4v", ".flac", ".flv", ".gho",
    ".gif", ".gifv", ".gnp", ".iso", ".jar", ".jpeg", ".jpg", ".jxl", ".lz", ".lzh", ".m4a", ".m4v", ".mkv", ".mov", ".mp2", ".mp3", ".mp4",
    ".mpa", ".mpc", ".mpe", ".mpeg", ".mpg", ".mpl", ".mpv", ".msi", ".odp", ".ods", ".odt", ".ogg", ".ogv", ".otp", ".ots", ".ott", ".pea",
    ".png", ".pptx", ".qt", ".rar", ".s7z", ".sfx", ".sit", ".sitx", ".sqx", ".svgz", ".swf", ".tbz2", ".tib", ".tlz", ".vob", ".war",
    ".webm", ".webp", ".wma", ".wmv", ".woff", ".woff2", ".wvl", ".xlsb", ".xlsm", ".xlsx", ".xpi", ".xps", ".zip", ".zipx", ".zoo", ".zpaq",
];

/// FIO_openSrcFile: stdin, or a regular file / FIFO (block devices with -f).
fn open_src(ctx: &Ctx, src: &str, allow_block: bool) -> Option<(Box<dyn Read>, Option<Metadata>)> {
    if src == STDIN {
        return Some((Box::new(io::stdin().lock()), None));
    }
    let meta = match std::fs::metadata(src) {
        Ok(m) => m,
        Err(e) => {
            display!(ctx, 1, "zstd: can't stat {} : {} -- ignored \n", src, errstr(&e));
            return None;
        }
    };
    let ft = meta.file_type();
    if !ft.is_file() && !ft.is_fifo() && !src.starts_with("/dev/fd/") && !src.starts_with("/proc/self/fd/") && !(allow_block && ft.is_block_device()) {
        display!(ctx, 1, "zstd: {} is not a regular file -- ignored \n", src);
        return None;
    }
    match File::open(src) {
        Ok(f) => Some((Box::new(f), Some(meta))),
        Err(e) => {
            display!(ctx, 1, "zstd: {}: {} \n", src, errstr(&e));
            None
        }
    }
}

enum Dst {
    Stdout,
    Null,
    File(File, String, bool), // file, name, transfer the source's stat
}

impl Dst {
    /// &File writes (the guest's std has no File::try_clone).
    fn writer(&self) -> Box<dyn Write + '_> {
        match self {
            Dst::Stdout => Box::new(io::stdout().lock()),
            Dst::Null => Box::new(crate::common::Sink),
            Dst::File(f, _, _) => Box::new(f),
        }
    }
}

/// FIO_openDstFile.
fn open_dst(ctx: &Ctx, prefs: &Prefs, src: Option<&str>, dst: &str, transfer: bool) -> Option<Dst> {
    if prefs.test {
        return Some(Dst::Null);
    }
    if dst == STDOUT {
        return Some(Dst::Stdout);
    }
    if dst == NULL_DEV {
        return Some(Dst::Null);
    }
    if let Some(s) = src {
        if s != STDIN {
            if let (Ok(a), Ok(b)) = (std::fs::metadata(s), std::fs::metadata(dst)) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    display!(ctx, 1, "zstd: Refusing to open an output file which will overwrite the input file \n");
                    return None;
                }
            }
        }
    }
    if std::fs::metadata(dst).map(|m| m.is_file()).unwrap_or(false) {
        if !prefs.overwrite {
            if ctx.level <= 1 {
                display!(ctx, 1, "zstd: {} already exists; not overwritten  \n", dst);
                return None;
            }
            eprint!("zstd: {} already exists; ", dst);
            if ctx.has_stdin_input {
                eprintln!("stdin is an input - not proceeding.");
                return None;
            }
            eprint!("overwrite (y/n) ? ");
            let mut line = String::new();
            let _ = io::stdin().lock().read_line(&mut line);
            if !line.starts_with(['y', 'Y']) {
                eprint!("Not overwritten  \n \n");
                return None;
            }
        }
        let _ = std::fs::remove_file(dst);
    }
    let mode = if transfer { 0o600 } else { 0o666 };
    match OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(dst) {
        Ok(f) => {
            crate::common::pending_output(Some(Path::new(dst)));
            Some(Dst::File(f, dst.to_string(), transfer))
        }
        Err(e) => {
            display!(ctx, 1, "zstd: {}: {}\n", dst, errstr(&e));
            None
        }
    }
}

/// Closes a destination opened for one source: its stat, and removal when the work failed.
fn close_dst(ctx: &Ctx, dst: Dst, meta: Option<&Metadata>, mut result: i32) -> i32 {
    crate::common::pending_output(None);
    if let Dst::File(f, name, transfer) = dst {
        if transfer {
            if let Some(m) = meta {
                use std::os::fd::AsRawFd;
                unsafe {
                    libc::fchown(f.as_raw_fd(), u32::MAX, m.gid());
                    libc::fchmod(f.as_raw_fd(), (m.mode() & 0o777) as libc::mode_t);
                    libc::fchown(f.as_raw_fd(), m.uid(), u32::MAX);
                }
            }
        }
        if let Err(e) = f.sync_data() {
            if e.raw_os_error() != Some(libc::EINVAL) {
                display!(ctx, 1, "zstd: {}: {} \n", name, errstr(&e));
                result = 1;
            }
        }
        drop(f);
        if transfer {
            if let Some(m) = meta {
                let c = std::ffi::CString::new(name.as_bytes()).unwrap();
                let times = [crate::common::timespec(0, libc::UTIME_NOW as i64), crate::common::timespec(m.mtime(), m.mtime_nsec())];
                unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
            }
        }
        if result != 0 {
            let _ = std::fs::remove_file(&name);
        }
    }
    result
}

fn shown_name(src: &str) -> &str {
    src
}

struct Counting<'a, R> {
    inner: R,
    n: &'a mut u64,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        *self.n += k as u64;
        Ok(k)
    }
}

struct CountingW<'a, W> {
    inner: W,
    n: &'a mut u64,
}

impl<W: Write> Write for CountingW<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let k = self.inner.write(buf)?;
        *self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn zstd_params(prefs: &Prefs, size: Option<u64>) -> Result<CompressionParameters, String> {
    let mut b = CompressionParameters::builder(CompressionLevel::from_level(prefs.level));
    if let Some(w) = prefs.long_wlog {
        b = b.enable_long_distance_matching(true).window_log(w);
    }
    for (k, v) in &prefs.params {
        b = match k.as_str() {
            "wlog" => b.window_log(*v),
            "clog" => b.chain_log(*v),
            "hlog" => b.hash_log(*v),
            "slog" => b.search_log(*v),
            "mml" => b.min_match(*v),
            "tlen" => b.target_length(*v),
            "strat" => match structured_zstd::encoding::Strategy::from_ordinal(*v) {
                Some(s) => b.strategy(s),
                None => return Err(format!("strategy {v} is out of range")),
            },
            "lhlog" => b.enable_long_distance_matching(true).ldm_hash_log(*v),
            "lmml" => b.enable_long_distance_matching(true).ldm_min_match(*v),
            "lblog" => b.enable_long_distance_matching(true).ldm_bucket_size_log(*v),
            "lhrlog" => b.enable_long_distance_matching(true).ldm_hash_rate_log(*v),
            _ => b,
        };
    }
    if let Some(l) = prefs.literals {
        b = b.literal_compression(if l {
            structured_zstd::encoding::LiteralCompressionMode::Enable
        } else {
            structured_zstd::encoding::LiteralCompressionMode::Disable
        });
    }
    let _ = size;
    b.build().map_err(|e| format!("{e:?}"))
}

/// One source into an open destination as one frame (or gzip member, .xz stream), which is
/// decoded again on the way and must give the source back (verify.rs).
fn compress_stream(prefs: &Prefs, input: &mut dyn Read, out: &mut dyn Write, size: Option<u64>) -> Result<(), String> {
    use crate::verify::{Codec, Hashing, Tee};
    let mut input = Hashing::new(input);
    let codec = match prefs.ctype {
        CType::Zstd => Codec::Zstd,
        CType::Gzip => Codec::Gzip,
        CType::Xz => Codec::Xz,
        CType::Lzma => Codec::Lzma,
    };
    let tee = Tee::new(&mut *out, codec, if prefs.ctype == CType::Zstd { prefs.dict.clone() } else { None });
    let tee = match prefs.ctype {
        CType::Zstd => {
            let mut enc = StreamingEncoder::new(tee, CompressionLevel::from_level(prefs.level));
            if !prefs.params.is_empty() || prefs.long_wlog.is_some() || prefs.literals.is_some() {
                let p = zstd_params(prefs, size)?;
                enc.set_parameters(&p).map_err(|e| format!("{e:?}"))?;
            }
            enc.set_content_checksum(prefs.checksum != 0).map_err(|e| format!("{e:?}"))?;
            let pledged = size.or(prefs.stream_size);
            if prefs.content_size {
                if let Some(s) = pledged {
                    enc.set_pledged_content_size(s).map_err(|e| format!("{e:?}"))?;
                }
            } else {
                enc.set_content_size_flag(false).map_err(|e| format!("{e:?}"))?;
            }
            if let Some(h) = prefs.size_hint.filter(|_| pledged.is_none()) {
                enc.set_source_size_hint(h).map_err(|e| format!("{e:?}"))?;
            }
            if let Some(t) = prefs.target_block {
                enc.set_target_block_size(Some(t)).map_err(|e| format!("{e:?}"))?;
            }
            if let Some(d) = &prefs.dict {
                enc.set_dictionary_from_bytes(d).map_err(|e| format!("{e:?}"))?;
                if !prefs.dict_id {
                    enc.set_dictionary_id_flag(false).map_err(|e| format!("{e:?}"))?;
                }
            }
            io::copy(&mut input, &mut enc).map_err(|e| errstr(&e))?;
            enc.finish().map_err(|e| format!("{e:?}"))?
        }
        CType::Gzip => {
            let lvl = prefs.level.clamp(0, 9) as u32;
            let mut enc = flate2::write::GzEncoder::new(tee, flate2::Compression::new(lvl));
            io::copy(&mut input, &mut enc).map_err(|e| errstr(&e))?;
            enc.finish().map_err(|e| errstr(&e))?
        }
        CType::Xz => {
            let lvl = prefs.level.clamp(0, 9) as u32;
            let mut w = lzma_rust2::XzWriter::new(tee, lzma_rust2::XzOptions::with_preset(lvl)).map_err(|e| errstr(&e))?;
            io::copy(&mut input, &mut w).map_err(|e| errstr(&e))?;
            w.finish().map_err(|e| errstr(&e))?
        }
        CType::Lzma => {
            let lvl = prefs.level.clamp(0, 9) as u32;
            let o = lzma_rust2::LzmaOptions::with_preset(lvl);
            let mut w = lzma_rust2::LzmaWriter::new_use_header(tee, &o, None).map_err(|e| errstr(&e))?;
            io::copy(&mut input, &mut w).map_err(|e| errstr(&e))?;
            w.finish().map_err(|e| errstr(&e))?
        }
    };
    tee.finish(input.sum()).map_err(|m| crate::verify::VerifyError(m).to_string())
}

/// FIO_compressFilename_srcFile (+ _dstFile when `open` is None).
fn compress_src(ctx: &mut Ctx, prefs: &Prefs, dst_name: &str, src: &str, open: Option<&mut dyn Write>, allow_block: bool) -> i32 {
    if src != STDIN {
        if let Ok(m) = std::fs::metadata(src) {
            if m.is_dir() {
                display!(ctx, 1, "zstd: {} is a directory -- ignored \n", src);
                return 1;
            }
        }
        if prefs.exclude_compressed && COMPRESSED_EXTS.iter().any(|e| src.len() > e.len() && src.ends_with(e)) {
            display!(ctx, 4, "File is already compressed : {} \n", src);
            return 0;
        }
    }
    let Some((reader, meta)) = open_src(ctx, src, allow_block) else { return 1 };
    let size = meta.as_ref().filter(|m| m.is_file()).map(|m| m.len());
    let mut read = 0u64;
    let mut written = 0u64;
    let mut input = Counting { inner: BufReader::with_capacity(1 << 17, reader), n: &mut read };
    let name = if src == STDIN { STDIN } else { src };
    let result = match open {
        Some(w) => {
            let mut out = CountingW { inner: w, n: &mut written };
            compress_stream(prefs, &mut input, &mut out, size)
        }
        None => {
            let transfer = src != STDIN && dst_name != STDOUT && meta.as_ref().map_or(false, |m| m.is_file());
            let Some(dst) = open_dst(ctx, prefs, Some(src), dst_name, transfer) else { return 1 };
            let r = {
                let mut out = CountingW { inner: BufWriter::with_capacity(1 << 17, dst.writer()), n: &mut written };
                compress_stream(prefs, &mut input, &mut out, size).and_then(|_| out.flush().map_err(|e| errstr(&e)))
            };
            let status = match &r {
                Ok(_) => 0,
                Err(m) => {
                    if m.contains("Broken pipe") {
                        std::process::exit(1);
                    }
                    display!(ctx, 1, "zstd: {}: {} \n", name, m);
                    1
                }
            };
            let status = close_dst(ctx, dst, meta.as_ref(), status);
            if status != 0 {
                return status;
            }
            r.map(|_| ())
        }
    };
    if let Err(m) = result {
        display!(ctx, 1, "zstd: {}: {} \n", name, m);
        return 1;
    }
    ctx.bytes_in += read;
    ctx.bytes_out += written;
    if ctx.nb_files <= 1 || ctx.level >= 3 {
        let (iv, ip, is) = hrs(ctx.level, read);
        let (ov, op, os) = hrs(ctx.level, written);
        let shown_dst = if dst_name == STDOUT { STDOUT } else { dst_name };
        if ctx.level >= 2 {
            if read == 0 {
                eprintln!("{:<20} :  ({:>6.ip$}{} => {:>6.op$}{}, {}) ", shown_name(name), iv, is, ov, os, shown_dst, ip = ip, op = op);
            } else {
                eprintln!(
                    "{:<20} :{:>6.2}%   ({:>6.ip$}{} => {:>6.op$}{}, {}) ",
                    shown_name(name),
                    written as f64 / read as f64 * 100.0,
                    iv,
                    is,
                    ov,
                    os,
                    shown_dst,
                    ip = ip,
                    op = op
                );
            }
        }
    }
    if prefs.remove_src && src != STDIN {
        crate::common::pending_output(None);
        if let Err(e) = std::fs::remove_file(src) {
            display!(ctx, 1, "zstd: error 1 : zstd: {}: {} \n", src, errstr(&e));
            std::process::exit(1);
        }
    }
    0
}

/// FIO_multiFilesConcatWarning: true when the run must stop.
fn concat_warning(ctx: &Ctx, prefs: &mut Prefs, out: Option<&str>) -> bool {
    if prefs.test || ctx.nb_files == 1 {
        return false;
    }
    let Some(out) = out else { return false };
    if ctx.has_stdout {
        display!(ctx, 2, "zstd: WARNING: all input files will be processed and concatenated into stdout. \n");
    } else {
        display!(ctx, 2, "zstd: WARNING: all input files will be processed and concatenated into a single output file: {} \n", out);
    }
    display!(ctx, 2, "The concatenated output CANNOT regenerate original file names nor directory structure. \n");
    if prefs.remove_src {
        display!(ctx, 2, "Since it's a destructive operation, input files will not be removed. \n");
        prefs.remove_src = false;
    }
    if ctx.has_stdout || prefs.overwrite {
        return false;
    }
    if ctx.level <= 1 {
        display!(ctx, 1, "Concatenating multiple processed inputs into a single output loses file metadata. \n");
        display!(ctx, 1, "Aborting. \n");
        return true;
    }
    if ctx.has_stdin_input {
        eprintln!("stdin is an input - not proceeding.");
        return true;
    }
    eprint!("Proceed? (y/n): ");
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    if !line.starts_with(['y', 'Y']) {
        eprint!("Aborting... \n");
        return true;
    }
    false
}

fn out_dir_name(src: &str, dir: &str, suffix: &str) -> String {
    let base = Path::new(src).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let d = dir.trim_end_matches('/');
    format!("{d}/{base}{suffix}")
}

fn compress_many(ctx: &mut Ctx, prefs: &mut Prefs, a: &Args) -> i32 {
    let mut error = 0;
    if let Some(out) = a.out.as_deref() {
        if concat_warning(ctx, prefs, Some(out)) {
            return 1;
        }
        let Some(dst) = open_dst(ctx, prefs, None, out, false) else { return 1 };
        {
            let mut w = BufWriter::with_capacity(1 << 17, dst.writer());
            for f in &a.files {
                let s = compress_src(ctx, prefs, out, f, Some(&mut w), a.allow_block);
                if s == 0 {
                    ctx.processed += 1;
                }
                error |= s;
            }
            if let Err(e) = w.flush() {
                eprintln!("zstd: error 29 : Write error ({}) : cannot properly close {} ", errstr(&e), out);
                std::process::exit(29);
            }
        }
        close_dst(ctx, dst, None, 0);
    } else {
        for f in &a.files {
            let dst = match &a.out_dir {
                Some(d) => out_dir_name(f, d, prefs.suffix),
                None => format!("{f}{}", prefs.suffix),
            };
            let s = compress_src(ctx, prefs, &dst, f, None, a.allow_block);
            if s == 0 {
                ctx.processed += 1;
            }
            error |= s;
        }
    }
    if ctx.processed >= 1 && ctx.nb_files > 1 && ctx.level >= 2 {
        let (iv, ip, is) = hrs(ctx.level, ctx.bytes_in);
        let (ov, op, os) = hrs(ctx.level, ctx.bytes_out);
        if ctx.bytes_in == 0 {
            eprintln!("{:>3} files compressed : ({:>6.ip$}{:>4} => {:>6.op$}{:>4})", ctx.processed, iv, is, ov, os, ip = ip, op = op);
        } else {
            eprintln!(
                "{:>3} files compressed : {:.2}% ({:>6.ip$}{:>4} => {:>6.op$}{:>4})",
                ctx.processed,
                ctx.bytes_out as f64 / ctx.bytes_in as f64 * 100.0,
                iv,
                is,
                ov,
                os,
                ip = ip,
                op = op
            );
        }
    }
    error
}

const DECOMPRESS_SUFFIXES: &[&str] = &[".zst", ".tzst", ".zstd", ".gz", ".tgz", ".lzma", ".xz", ".txz"];
const SUFFIX_LIST: &str = ".zst/.tzst/.gz/.tgz/.lzma/.xz/.txz";

fn determine_dst(ctx: &Ctx, src: &str, out_dir: Option<&str>) -> Option<String> {
    if src == STDIN {
        return Some(STDOUT.into());
    }
    let unknown = || {
        display!(
            ctx,
            1,
            "zstd: {}: unknown suffix ({} expected). Can't derive the output file name. Specify it with -o dstFileName. Ignoring.\n",
            src,
            SUFFIX_LIST
        );
        None
    };
    let Some(dot) = src.rfind('.') else { return unknown() };
    let suffix = &src[dot..];
    if src.len() <= suffix.len() || !DECOMPRESS_SUFFIXES.contains(&suffix) {
        return unknown();
    }
    let tar = if suffix.as_bytes()[1] == b't' { ".tar" } else { "" };
    let stem = &src[..dot];
    Some(match out_dir {
        Some(d) => out_dir_name(stem, d, tar),
        None => format!("{stem}{tar}"),
    })
}

fn decompress_many(ctx: &mut Ctx, prefs: &mut Prefs, a: &Args) -> i32 {
    let mut error = 0;
    if let Some(out) = a.out.as_deref() {
        if concat_warning(ctx, prefs, Some(out)) {
            return 1;
        }
        let Some(dst) = open_dst(ctx, prefs, None, out, false) else { return 1 };
        {
            let mut w = BufWriter::with_capacity(1 << 17, dst.writer());
            for f in &a.files {
                let s = decompress_src(ctx, prefs, out, f, Some(&mut w), a.allow_block);
                if s == 0 {
                    ctx.processed += 1;
                }
                error |= s;
            }
            if w.flush().is_err() {
                eprintln!("zstd: error 72 : Write error : {} : cannot properly close output file ", out);
                std::process::exit(72);
            }
        }
        close_dst(ctx, dst, None, 0);
    } else {
        for f in &a.files {
            let Some(dst) = determine_dst(ctx, f, a.out_dir.as_deref()) else {
                error = 1;
                continue;
            };
            let s = decompress_src(ctx, prefs, &dst, f, None, a.allow_block);
            if s == 0 {
                ctx.processed += 1;
            }
            error |= s;
        }
    }
    if ctx.processed >= 1 && ctx.nb_files > 1 && ctx.level >= 2 {
        eprintln!("{} files decompressed : {:>6} bytes total ", ctx.processed, ctx.bytes_out);
    }
    error
}

/// FIO_decompressSrcFile (+ _dstFile when `open` is None).
fn decompress_src(ctx: &mut Ctx, prefs: &Prefs, dst_name: &str, src: &str, open: Option<&mut dyn Write>, allow_block: bool) -> i32 {
    if src != STDIN && std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
        display!(ctx, 1, "zstd: {} is a directory -- ignored \n", src);
        return 1;
    }
    let Some((reader, meta)) = open_src(ctx, src, allow_block) else { return 1 };
    let mut input = Feed::new(reader);
    let pass = if prefs.pass_through == -1 { prefs.overwrite && dst_name == STDOUT } else { prefs.pass_through == 1 };
    let status = match open {
        Some(w) => decompress_frames(ctx, prefs, &mut input, w, src, pass),
        None => {
            let transfer = src != STDIN && dst_name != STDOUT && meta.as_ref().map_or(false, |m| m.is_file());
            let Some(dst) = open_dst(ctx, prefs, Some(src), dst_name, transfer) else { return 1 };
            let s = {
                let mut w = BufWriter::with_capacity(1 << 17, dst.writer());
                let s = decompress_frames(ctx, prefs, &mut input, &mut w, src, pass);
                match w.flush() {
                    Err(e) if s == 0 => {
                        if e.kind() == io::ErrorKind::BrokenPipe {
                            std::process::exit(1);
                        }
                        display!(ctx, 1, "zstd: {}: {} \n", dst_name, errstr(&e));
                        1
                    }
                    _ => s,
                }
            };
            close_dst(ctx, dst, meta.as_ref(), s)
        }
    };
    if status == 0 && prefs.remove_src && src != STDIN {
        crate::common::pending_output(None);
        if let Err(e) = std::fs::remove_file(src) {
            display!(ctx, 1, "zstd: {}: {} \n", src, errstr(&e));
            return 1;
        }
    }
    status
}

fn zstd_error_name(e: &io::Error) -> &'static str {
    let m = format!("{e:?}");
    if m.contains("ChecksumMismatch") || m.contains("checksum mismatch") {
        "Restored data doesn't match checksum"
    } else if m.contains("WindowSizeTooBig") {
        "Frame requires too much memory for decoding"
    } else if m.contains("DictNotProvided") || m.contains("DictIdMismatch") {
        "Dictionary mismatch"
    } else if m.contains("FrameHeaderError") || m.contains("ReadFrameHeaderError") {
        "Unknown frame descriptor"
    } else {
        "Data corruption detected"
    }
}

/// FIO_decompressFrames: zstd frames (and skippable ones), gzip members, .xz and .lzma streams,
/// one after another; returns 0 or 1 (the message given).
fn decompress_frames<R: Read>(ctx: &mut Ctx, prefs: &Prefs, input: &mut Feed<R>, out: &mut dyn Write, src: &str, pass: bool) -> i32 {
    let name = src;
    let mut filesize = 0u64;
    let mut read_something = false;
    let dict = match &prefs.dict {
        Some(d) => match structured_zstd::decoding::DictionaryHandle::decode_dict(d) {
            Ok(h) => Some(h),
            Err(_) => {
                eprintln!("zstd: error 32 : Couldn't load the dictionary ");
                std::process::exit(32);
            }
        },
        None => None,
    };
    let mut decoder = FrameDecoder::new();
    loop {
        let buf = match input.fill_buf() {
            Ok(b) => b,
            Err(e) => {
                display!(ctx, 1, "zstd: {}: {} \n", name, errstr(&e));
                return 1;
            }
        };
        if buf.is_empty() {
            if !read_something {
                display!(ctx, 1, "zstd: {}: unexpected end of file \n", name);
                return 1;
            }
            break;
        }
        read_something = true;
        let head = match input.ensure(4) {
            Ok(h) => h.to_vec(),
            Err(e) => {
                display!(ctx, 1, "zstd: {}: {} \n", name, errstr(&e));
                return 1;
            }
        };
        if head.len() < 4 {
            if pass && filesize == 0 {
                if out.write_all(&head).is_err() {
                    return 1;
                }
                break;
            }
            display!(ctx, 1, "zstd: {}: unknown header \n", name);
            return 1;
        }
        let magic = u32::from_le_bytes(head[..4].try_into().unwrap());
        let mut counted = 0u64;
        let r: Result<(), io::Error> = if magic == ZSTD_MAGIC || magic & 0xFFFF_FFF0 == SKIPPABLE_BASE {
            let built = match &dict {
                Some(h) => StreamingDecoder::new_with_decoder_and_dictionary_handle(&mut *input, &mut decoder, h),
                None => StreamingDecoder::new_with_decoder(&mut *input, &mut decoder),
            };
            match built {
                Ok(mut d) => {
                    d.decoder_mut().set_content_checksum(if prefs.checksum == 0 { ContentChecksum::None } else { ContentChecksum::Verify });
                    let mut w = CountingW { inner: &mut *out, n: &mut counted };
                    io::copy(&mut d, &mut w).map(|_| ())
                }
                Err(structured_zstd::decoding::errors::FrameDecoderError::ReadFrameHeaderError(
                    structured_zstd::decoding::errors::ReadFrameHeaderError::SkipFrame { length, .. },
                )) => {
                    let skipped = io::copy(&mut input.by_ref().take(length as u64), &mut io::sink()).unwrap_or(0);
                    if skipped != length as u64 {
                        display!(ctx, 1, "{} : Read error (39) : premature end \n", name);
                        return 1;
                    }
                    Ok(())
                }
                Err(e) => Err(io::Error::other(format!("{e:?}"))),
            }
        } else if head[0] == 31 && head[1] == 139 {
            let mut d = flate2::bufread::GzDecoder::new(&mut *input);
            let mut w = CountingW { inner: &mut *out, n: &mut counted };
            match io::copy(&mut d, &mut w) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    display!(ctx, 1, "zstd: {}: premature gz end \n", name);
                    return 1;
                }
                Err(_) => {
                    display!(ctx, 1, "zstd: {}: inflate error -3 \n", name);
                    return 1;
                }
            }
        } else if (head[0] == 0xFD && head[1] == 0x37) || (head[0] == 0x5D && head[1] == 0) {
            let mut w = CountingW { inner: &mut *out, n: &mut counted };
            let r = if head[0] == 0xFD {
                io::copy(&mut lzma_rust2::XzReader::new(&mut *input, false), &mut w).map(|_| ())
            } else {
                lzma_rust2::LzmaReader::new_mem_limit(&mut *input, u32::MAX, None).and_then(|mut r| io::copy(&mut r, &mut w).map(|_| ()))
            };
            match r {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    display!(ctx, 1, "zstd: {}: premature lzma end \n", name);
                    return 1;
                }
                Err(_) => {
                    display!(ctx, 1, "zstd: {}: lzma_code decoding error 9 \n", name);
                    return 1;
                }
            }
        } else if pass {
            if filesize == 0 {
                let mut w = CountingW { inner: &mut *out, n: &mut counted };
                if io::copy(input, &mut w).is_err() {
                    return 1;
                }
                return 0;
            }
            display!(ctx, 1, "zstd: {}: unsupported format \n", name);
            return 1;
        } else {
            display!(ctx, 1, "zstd: {}: unsupported format \n", name);
            return 1;
        };
        filesize += counted;
        if let Err(e) = r {
            if e.kind() == io::ErrorKind::BrokenPipe {
                std::process::exit(1);
            }
            let exhausted = input.ensure(1).map(|b| b.is_empty()).unwrap_or(false);
            if e.kind() == io::ErrorKind::UnexpectedEof || (exhausted && zstd_error_name(&e) != "Restored data doesn't match checksum") {
                display!(ctx, 1, "{} : Read error (39) : premature end \n", name);
            } else if e.raw_os_error().is_some() {
                display!(ctx, 1, "zstd: {}: {} \n", name, errstr(&e));
            } else {
                display!(ctx, 1, "{} : Decoding error (36) : {} \n", name, zstd_error_name(&e));
            }
            return 1;
        }
    }
    ctx.bytes_out += filesize;
    if (ctx.nb_files <= 1 || ctx.level >= 3) && ctx.level >= 2 {
        eprintln!("{:<20}: {} bytes ", name, filesize);
    }
    0
}

/// A buffered reader that can be asked for at least n bytes in its buffer (a magic number
/// split across pipe reads), without consuming them.
pub struct Feed<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> Feed<R> {
    pub fn new(inner: R) -> Self {
        Feed { inner, buf: Vec::with_capacity(1 << 17), pos: 0 }
    }
    pub fn ensure(&mut self, n: usize) -> io::Result<&[u8]> {
        while self.buf.len() - self.pos < n {
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let have = self.buf.len();
            self.buf.resize(have.max(n).max(1 << 17), 0);
            let got = loop {
                match self.inner.read(&mut self.buf[have..]) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => break r,
                }
            };
            match got {
                Ok(k) => {
                    self.buf.truncate(have + k);
                    if k == 0 {
                        break;
                    }
                }
                Err(e) => {
                    self.buf.truncate(have);
                    return Err(e);
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }
}

impl<R: Read> Read for Feed<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<R: Read> BufRead for Feed<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            self.pos = 0;
            self.buf.clear();
            self.ensure(1)?;
        }
        Ok(&self.buf[self.pos..])
    }
    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.buf.len());
    }
}

// --list

#[derive(Default, Clone)]
struct Info {
    decompressed: u64,
    compressed: u64,
    window: u64,
    frames: i64,
    skippable: i64,
    unknown_size: bool,
    uses_check: bool,
    checksum: [u8; 4],
    files: u32,
    dict_id: u32,
}

enum InfoErr {
    Frame(String),
    NotZstd,
    File,
    Truncated,
}

fn analyze(info: &mut Info, f: &mut File) -> Result<(), InfoErr> {
    loop {
        let mut hdr = [0u8; 18];
        let pos = f.stream_position().map_err(|_| InfoErr::File)?;
        let mut n = 0;
        while n < hdr.len() {
            match f.read(&mut hdr[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(InfoErr::File),
            }
        }
        if n < 6 {
            if n == 0 && info.compressed > 0 {
                if pos != info.compressed {
                    eprintln!("Error: seeked to position {pos}, which is beyond file size of {}\n ", info.compressed);
                    return Err(InfoErr::Truncated);
                }
                return Ok(());
            }
            if n < hdr.len() {
                eprintln!("Error: reached end of file with incomplete frame ");
                return Err(InfoErr::NotZstd);
            }
        }
        let magic = u32::from_le_bytes(hdr[..4].try_into().unwrap());
        if magic == ZSTD_MAGIC {
            let h = structured_zstd::decoding::read_frame_header_info(&hdr[..n], false)
                .map_err(|_| InfoErr::Frame("Error: could not decode frame header".into()))?;
            match h.content_size {
                structured_zstd::decoding::FrameContentSize::Known(s) => info.decompressed += s,
                _ => info.unknown_size = true,
            }
            let id = h.dictionary_id.unwrap_or(0);
            if info.dict_id != 0 && info.dict_id != id {
                print!("WARNING: File contains multiple frames with different dictionary IDs. Showing dictID 0 instead");
                info.dict_id = 0;
            } else {
                info.dict_id = id;
            }
            info.window = h.window_size;
            f.seek(SeekFrom::Start(pos + h.header_size as u64)).map_err(|_| InfoErr::Frame("Error: could not move to end of frame header".into()))?;
            loop {
                let mut b = [0u8; 3];
                f.read_exact(&mut b).map_err(|_| InfoErr::Frame("Error while reading block header".into()))?;
                let bh = u32::from_le_bytes([b[0], b[1], b[2], 0]);
                let ty = (bh >> 1) & 3;
                if ty == 3 {
                    return Err(InfoErr::Frame("Error: unsupported block type".into()));
                }
                let size = if ty == 1 { 1 } else { (bh >> 3) as i64 };
                f.seek(SeekFrom::Current(size)).map_err(|_| InfoErr::Frame("Error: could not skip to end of block".into()))?;
                if bh & 1 == 1 {
                    break;
                }
            }
            if h.content_checksum {
                info.uses_check = true;
                f.read_exact(&mut info.checksum).map_err(|_| InfoErr::Frame("Error: could not read checksum".into()))?;
            }
            info.frames += 1;
        } else if magic & 0xFFFF_FFF0 == SKIPPABLE_BASE {
            let size = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as u64;
            f.seek(SeekFrom::Start(pos + 8 + size)).map_err(|_| InfoErr::Frame("Error: could not find end of skippable frame".into()))?;
            info.skippable += 1;
        } else {
            return Err(InfoErr::NotZstd);
        }
        if f.stream_position().map(|p| p > info.compressed).unwrap_or(false) {
            let p = f.stream_position().unwrap_or(0);
            eprintln!("Error: seeked to position {p}, which is beyond file size of {}\n ", info.compressed);
            return Err(InfoErr::Truncated);
        }
    }
}

fn list_one(ctx: &Ctx, total: &mut Info, name: &str) -> i32 {
    let mut info = Info::default();
    let is_file = std::fs::metadata(name).map(|m| m.is_file()).unwrap_or(false);
    let r = if !is_file {
        display!(ctx, 1, "Error : {} is not a file \n", name);
        Err(InfoErr::File)
    } else {
        match File::open(name) {
            Ok(mut f) => {
                info.compressed = f.metadata().map(|m| m.len()).unwrap_or(0);
                info.files = 1;
                analyze(&mut info, &mut f)
            }
            Err(_) => {
                display!(ctx, 1, "Error: could not open source file {} \n", name);
                Err(InfoErr::File)
            }
        }
    };
    let mut status = 0;
    match r {
        Ok(()) => {}
        Err(InfoErr::Frame(msg)) => {
            display!(ctx, 1, "{} \n", msg);
            display!(ctx, 1, "Error while parsing \"{}\" \n", name);
            status = 1;
        }
        Err(InfoErr::NotZstd) => {
            println!("File \"{}\" not compressed by zstd ", name);
            if ctx.level > 2 {
                println!();
            }
            return 1;
        }
        Err(InfoErr::File) => {
            if ctx.level > 2 {
                println!();
            }
            return 1;
        }
        Err(InfoErr::Truncated) => {
            println!("File \"{}\" is truncated ", name);
            if ctx.level > 2 {
                println!();
            }
            return 1;
        }
    }
    let ratio = if info.compressed == 0 { 0.0 } else { info.decompressed as f64 / info.compressed as f64 };
    let check = if info.uses_check { "XXH64" } else { "None" };
    if ctx.level <= 2 {
        if !info.unknown_size {
            println!(
                "{:>6}  {:>5}  {}  {}  {:>5.3}  {:>5}  {}",
                info.frames + info.skippable,
                info.skippable,
                fmt_hrs(ctx.level, info.compressed, 6, 4),
                fmt_hrs(ctx.level, info.decompressed, 8, 4),
                ratio,
                check,
                name
            );
        } else {
            println!("{:>6}  {:>5}  {}                       {:>5}  {}", info.frames + info.skippable, info.skippable, fmt_hrs(ctx.level, info.compressed, 6, 4), check, name);
        }
    } else {
        let one = |v: u64| {
            let (x, p, s) = hrs(ctx.level, v);
            format!("{:.p$}{}", x, s, p = p)
        };
        println!("{} ", name);
        println!("# Zstandard Frames: {}", info.frames);
        if info.skippable > 0 {
            println!("# Skippable Frames: {}", info.skippable);
        }
        println!("DictID: {}", info.dict_id);
        println!("Window Size: {} ({} B)", one(info.window), info.window);
        println!("Compressed Size: {} ({} B)", one(info.compressed), info.compressed);
        if !info.unknown_size {
            println!("Decompressed Size: {} ({} B)", one(info.decompressed), info.decompressed);
            println!("Ratio: {:.4}", ratio);
        }
        if info.uses_check && info.frames == 1 {
            println!("Check: {} {:02x}{:02x}{:02x}{:02x}", check, info.checksum[3], info.checksum[2], info.checksum[1], info.checksum[0]);
        } else {
            println!("Check: {check}");
        }
        println!();
    }
    total.frames += info.frames;
    total.skippable += info.skippable;
    total.compressed += info.compressed;
    total.decompressed += info.decompressed;
    total.unknown_size |= info.unknown_size;
    total.uses_check &= info.uses_check;
    total.files += info.files;
    status
}

fn list_files(ctx: &Ctx, files: &[String]) -> i32 {
    if files.iter().any(|f| f == STDIN) {
        display!(ctx, 1, "zstd: --list does not support reading from standard input \n");
        return 1;
    }
    if files.is_empty() {
        if !stdin_is_tty() {
            display!(ctx, 1, "zstd: --list does not support reading from standard input \n");
        }
        display!(ctx, 1, "No files given \n");
        return 1;
    }
    if ctx.level <= 2 {
        println!("Frames  Skips  Compressed  Uncompressed  Ratio  Check  Filename");
    }
    let mut total = Info { uses_check: true, ..Default::default() };
    let mut error = 0;
    for f in files {
        error |= list_one(ctx, &mut total, f);
    }
    if files.len() > 1 && ctx.level <= 2 {
        let ratio = if total.compressed == 0 { 0.0 } else { total.decompressed as f64 / total.compressed as f64 };
        let check = if total.uses_check { "XXH64" } else { "" };
        println!("----------------------------------------------------------------- ");
        if total.unknown_size {
            println!("{:>6}  {:>5}  {}                       {:>5}  {} files", total.frames + total.skippable, total.skippable, fmt_hrs(ctx.level, total.compressed, 6, 4), check, total.files);
        } else {
            println!(
                "{:>6}  {:>5}  {}  {}  {:>5.3}  {:>5}  {} files",
                total.frames + total.skippable,
                total.skippable,
                fmt_hrs(ctx.level, total.compressed, 6, 4),
                fmt_hrs(ctx.level, total.decompressed, 8, 4),
                ratio,
                check,
                total.files
            );
        }
    }
    error
}
