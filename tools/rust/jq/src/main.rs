//! jq, the JSON processor, written again in Rust for the guest: the language (lexer, parser,
//! evaluator), builtin.jq as jq ships it, the C builtins, and the CLI (main.c's options,
//! messages and exit codes). The reference is jq's development tree (1.8.x): numbers keep
//! their literal text (jq's decNumber build), and the error messages are jq's.

mod builtins;
mod interp;
mod json;
mod lexer;
mod modules;
mod parser;
mod regex;
mod runtests;
mod time;
mod value;

use interp::{Flow, Inputs, Interp, Pv};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;
use value::{dump, Colors, Fmt, Map, Value};

pub const VERSION: &str = "1.8.2";

/// strerror's wording for an I/O error (without Rust's " (os error N)").
pub fn io_error_text(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Where the inputs come from: the files (or stdin), read line by line as jq does (so that
/// input_line_number counts the lines read so far), parsed or raw, slurped or not.
pub struct InputState {
    files: VecDeque<String>,
    reader: Option<Box<dyn BufRead>>,
    filename: Option<String>,
    line: usize,
    parser: Option<json::Parser>,
    raw: bool,
    slurped: Option<Vec<Value>>,
    slurped_raw: Option<String>,
    queue: VecDeque<Value>,
    pub failures: usize,
    pending_raw: Option<String>,
    exhausted: bool,
    eof_fed: bool,
}

impl InputState {
    fn new(files: Vec<String>, raw: bool, slurp: bool, stream: Option<bool>, seq: bool) -> InputState {
        InputState {
            files: files.into(),
            reader: None,
            filename: None,
            line: 0,
            parser: match (raw, stream) {
                (true, _) => None,
                (false, Some(errors)) => Some(json::Parser::new_streaming(seq, errors)),
                (false, None) => Some(json::Parser::new(seq)),
            },
            raw,
            slurped: if slurp && !raw { Some(vec![]) } else { None },
            slurped_raw: if slurp && raw { Some(String::new()) } else { None },
            queue: VecDeque::new(),
            failures: 0,
            pending_raw: None,
            exhausted: false,
            eof_fed: false,
        }
    }

    /// One line (or what is left) of the current file; None at the end of all of them.
    fn read_more(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.reader.is_none() {
                let f = self.files.pop_front()?;
                self.line = 0;
                if f == "-" {
                    self.filename = Some("<stdin>".into());
                    self.reader = Some(Box::new(BufReader::with_capacity(65536, std::io::stdin())));
                } else {
                    self.filename = Some(f.clone());
                    match std::fs::File::open(&f) {
                        Ok(file) => self.reader = Some(Box::new(BufReader::with_capacity(65536, file))),
                        Err(e) => {
                            eprintln!("jq: error: Could not open file {}: {}", f, io_error_text(&e));
                            self.failures += 1;
                            continue;
                        }
                    }
                }
            }
            let r = self.reader.as_mut().unwrap();
            let mut buf = vec![];
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => {
                    self.reader = None;
                    continue;
                }
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        self.line += 1;
                    }
                    return Some(buf);
                }
                Err(e) => {
                    eprintln!("jq: error: {}", io_error_text(&e));
                    self.failures += 1;
                    self.reader = None;
                    continue;
                }
            }
        }
    }

    fn next_value(&mut self) -> Option<Result<Value, String>> {
        if let Some(v) = self.queue.pop_front() {
            return Some(Ok(v));
        }
        if self.exhausted {
            return None;
        }
        if self.raw {
            loop {
                match self.read_more() {
                    Some(buf) => {
                        let text = value::utf8_lossy(&buf);
                        if let Some(s) = &mut self.slurped_raw {
                            s.push_str(&text);
                            continue;
                        }
                        let mut cur = self.pending_raw.take().unwrap_or_default();
                        if let Some(t) = text.strip_suffix('\n') {
                            cur.push_str(t);
                            return Some(Ok(Value::string(cur)));
                        }
                        cur.push_str(&text);
                        self.pending_raw = Some(cur);
                    }
                    None => {
                        self.exhausted = true;
                        if let Some(s) = self.slurped_raw.take() {
                            return Some(Ok(Value::string(s)));
                        }
                        return self.pending_raw.take().map(|s| Ok(Value::string(s)));
                    }
                }
            }
        }
        loop {
            match self.parser.as_mut().unwrap().next() {
                json::Next::Value(v) => {
                    if let Some(sl) = &mut self.slurped {
                        sl.push(v);
                        continue;
                    }
                    return Some(Ok(v));
                }
                json::Next::Error(e) => return Some(Err(e)),
                json::Next::None => {}
            }
            if self.eof_fed {
                break;
            }
            match self.read_more() {
                Some(buf) => self.parser.as_mut().unwrap().feed(&buf, false),
                None => {
                    self.parser.as_mut().unwrap().feed(&[], true);
                    self.eof_fed = true;
                }
            }
        }
        self.exhausted = true;
        self.slurped.take().map(|sl| Ok(Value::arr(sl)))
    }

    fn position(&self) -> String {
        match &self.filename {
            Some(f) => format!("{}:{}", f, self.line),
            None => "<unknown>".into(),
        }
    }
}

/// The interpreter reaches the inputs through this (input/inputs, input_filename...), while
/// the main loop holds the same state.
struct SharedInputs(Rc<std::cell::RefCell<InputState>>);

impl Inputs for SharedInputs {
    fn next(&mut self) -> Option<Result<Value, String>> {
        self.0.borrow_mut().next_value()
    }
    fn filename(&self) -> Value {
        match &self.0.borrow().filename {
            Some(f) => Value::str(f),
            None => Value::Null,
        }
    }
    fn line(&self) -> usize {
        self.0.borrow().line
    }
}

fn die() -> ! {
    eprintln!("Use jq --help for help with command-line options,");
    eprintln!("or see the jq manpage, or online docs at https://jqlang.org");
    std::process::exit(2);
}

fn usage(code: i32, short: bool) -> ! {
    let mut s = format!(
        "jq - commandline JSON processor [version {VERSION}]\n\nUsage:\tjq [options] <jq filter> [file...]\n\tjq [options] --args <jq filter> [strings...]\n\tjq [options] --jsonargs <jq filter> [JSON_TEXTS...]\n\n\
jq is a tool for processing JSON inputs, applying the given filter to\nits JSON text inputs and producing the filter's results as JSON on\nstandard output.\n\n\
The simplest filter is ., which copies jq's input to its output\nunmodified except for formatting. For more advanced filters see\nthe jq(1) manpage (\"man jq\") and/or https://jqlang.org/.\n\n\
Example:\n\n\t$ echo '{{\"foo\": 0}}' | jq .\n\t{{\n\t  \"foo\": 0\n\t}}\n\n"
    );
    if short {
        s.push_str("For listing the command options, use jq --help.\n");
    } else {
        s.push_str(
            "Command options:\n\
  -n, --null-input          use `null` as the single input value;\n\
  -R, --raw-input           read each line as string instead of JSON;\n\
  -s, --slurp               read all inputs into an array and use it as\n                            the single input value;\n\
  -c, --compact-output      compact instead of pretty-printed output;\n\
  -r, --raw-output          output strings without escapes and quotes;\n\
      --raw-output0         implies -r and output NUL after each output;\n\
  -j, --join-output         implies -r and output without newline after\n                            each output;\n\
  -a, --ascii-output        output strings by only ASCII characters\n                            using escape sequences;\n\
  -S, --sort-keys           sort keys of each object on output;\n\
  -C, --color-output        colorize JSON output;\n\
  -M, --monochrome-output   disable colored output;\n\
      --tab                 use tabs for indentation;\n\
      --indent n            use n spaces for indentation (max 7 spaces);\n\
      --unbuffered          flush output stream after each output;\n\
      --stream              parse the input value in streaming fashion;\n\
      --stream-errors       implies --stream and report parse error as\n                            an array;\n\
      --seq                 parse input/output as application/json-seq;\n\
  -f, --from-file           load the filter from a file;\n\
  -L, --library-path dir    search modules from the directory;\n\
      --arg name value      set $name to the string value;\n\
      --argjson name value  set $name to the JSON value;\n\
      --slurpfile name file set $name to an array of JSON values read\n                            from the file;\n\
      --rawfile name file   set $name to string contents of file;\n\
      --args                consume remaining arguments as positional\n                            string values;\n\
      --jsonargs            consume remaining arguments as positional\n                            JSON values;\n\
  -e, --exit-status         set exit status code based on the output;\n\
  -V, --version             show the version;\n\
  --build-configuration     show jq's build configuration;\n\
  -h, --help                show the help;\n\
  --                        terminates argument processing;\n\n\
Named arguments are also available as $ARGS.named[], while\npositional arguments are available as $ARGS.positional[].\n",
        );
    }
    if code == 0 {
        print!("{s}");
        let _ = std::io::stdout().flush();
    } else {
        eprint!("{s}");
    }
    std::process::exit(code);
}

const BUILD_CONFIGURATION: &str = "collabo-jq (Rust reimplementation; regex: fancy-regex; decNumber-compatible literals)";

/// A whole file: its JSON texts as an array (--slurpfile), or its text (--rawfile).
pub fn load_file(path: &str, raw: bool) -> Result<Value, String> {
    let data = std::fs::read(path).map_err(|e| format!("Could not open {}: {}", path, io_error_text(&e)))?;
    if raw {
        return Ok(Value::string(value::utf8_lossy(&data)));
    }
    let mut p = json::Parser::new(false);
    p.feed(&data, true);
    let mut out = vec![];
    loop {
        match p.next() {
            json::Next::Value(v) => out.push(v),
            json::Next::Error(e) => return Err(e),
            json::Next::None => break,
        }
    }
    Ok(Value::arr(out))
}

/// main.c's isoption: a short option eats its letter; a long one the whole text.
fn isoption(text: &mut String, is_short: bool, short: char, long: &str) -> bool {
    if is_short {
        if short != '\0' && text.starts_with(short) {
            text.remove(0);
            return true;
        }
        false
    } else if text == long {
        text.clear();
        true
    } else {
        false
    }
}

fn isoptish(a: &str) -> bool {
    let b = a.as_bytes();
    b.len() >= 2 && b[0] == b'-' && (b[1] == b'-' || b[1].is_ascii_alphabetic())
}

fn env_object() -> Value {
    let mut m = Map::new();
    for (k, v) in std::env::vars_os() {
        m.insert(Rc::from(k.to_string_lossy().as_ref()), Value::string(v.to_string_lossy().into_owned()));
    }
    Value::obj(m)
}

const OK: i32 = 0;
const OK_NULL_KIND: i32 = -1;
const ERROR_SYSTEM: i32 = 2;
const ERROR_COMPILE: i32 = 3;
const OK_NO_OUTPUT: i32 = -4;
const ERROR_UNKNOWN: i32 = 5;

struct Out {
    w: std::io::BufWriter<std::io::Stdout>,
    fmt: Fmt,
    raw: bool,
    raw0: bool,
    no_lf: bool,
    ascii: bool,
    seq: bool,
    unbuffered: bool,
    failed: bool,
}

impl Out {
    fn write(&mut self, b: &[u8]) {
        if self.w.write_all(b).is_err() {
            self.failed = true;
        }
    }
}

fn real_main() -> i32 {
    let argv: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut program: Option<String> = None;
    let mut positional: Vec<Value> = vec![];
    let mut named = Map::new();
    let mut files: Vec<String> = vec![];
    let mut further_strings = false;
    let mut further_json = false;
    let mut args_done = false;
    let mut lib_paths: Option<Vec<String>> = None;
    let (mut slurp, mut raw_in, mut null_in, mut raw_out, mut raw0, mut no_lf, mut ascii, mut color, mut no_color, mut sort, mut from_file, mut unbuffered, mut exit_status, mut seq) =
        (false, false, false, false, false, false, false, false, false, false, false, false, false, false);
    let mut stream: Option<bool> = None;
    let mut indent: i64 = 2;
    let mut pretty = true;
    let mut tab = false;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if args_done || !isoptish(a) {
            if program.is_none() {
                program = Some(a.clone());
            } else if further_strings {
                positional.push(Value::str(a));
            } else if further_json {
                match json::parse_one(a) {
                    Ok(v) => positional.push(v),
                    Err(_) => {
                        eprintln!("jq: invalid JSON text passed to --jsonargs");
                        die();
                    }
                }
            } else {
                files.push(a.clone());
            }
            i += 1;
            continue;
        }
        if a == "--" {
            args_done = true;
            i += 1;
            continue;
        }
        let (mut text, is_short) = if let Some(t) = a.strip_prefix("--") { (t.to_string(), false) } else { (a[1..].to_string(), true) };
        let mut consumed = 0;
        loop {
            if text.is_empty() {
                break;
            }
            macro_rules! opt {
                ($s:expr, $l:expr) => {
                    isoption(&mut text, is_short, $s, $l)
                };
            }
            if opt!('s', "slurp") {
                slurp = true;
            } else if opt!('r', "raw-output") {
                raw_out = true;
            } else if opt!('\0', "raw-output0") {
                raw_out = true;
                no_lf = true;
                raw0 = true;
            } else if opt!('j', "join-output") {
                raw_out = true;
                no_lf = true;
            } else if opt!('c', "compact-output") {
                indent = 0;
                tab = false;
                pretty = false;
            } else if opt!('C', "color-output") {
                color = true;
            } else if opt!('M', "monochrome-output") {
                no_color = true;
            } else if opt!('a', "ascii-output") {
                ascii = true;
            } else if opt!('\0', "unbuffered") {
                unbuffered = true;
            } else if opt!('S', "sort-keys") {
                sort = true;
            } else if opt!('R', "raw-input") {
                raw_in = true;
            } else if opt!('n', "null-input") {
                null_in = true;
            } else if opt!('f', "from-file") {
                from_file = true;
            } else if opt!('L', "library-path") {
                let lp = lib_paths.get_or_insert_with(Vec::new);
                let dir = if !text.is_empty() {
                    std::mem::take(&mut text)
                } else if i + consumed + 1 >= argv.len() {
                    eprintln!("-L takes a parameter: (e.g. -L /search/path or -L/search/path)");
                    die();
                } else {
                    consumed += 1;
                    argv[i + consumed].clone()
                };
                lp.push(std::fs::canonicalize(&dir).map(|p| p.display().to_string()).unwrap_or(dir));
            } else if opt!('b', "binary") {
            } else if opt!('\0', "tab") {
                tab = true;
                indent = 0;
                pretty = true;
            } else if opt!('\0', "indent") {
                if i + consumed + 1 >= argv.len() {
                    eprintln!("jq: --indent takes one parameter");
                    die();
                }
                let t = &argv[i + consumed + 1];
                match t.parse::<i64>() {
                    Ok(n) if (-1..=7).contains(&n) && !t.starts_with(|c: char| c.is_whitespace() || c == '+') => {
                        // JV_PRINT_INDENT_FLAGS: always pretty; -1 is tabs.
                        indent = n.max(0);
                        tab = n < 0;
                        pretty = true;
                    }
                    _ => {
                        eprintln!("jq: --indent takes a number between -1 and 7");
                        die();
                    }
                }
                consumed += 1;
            } else if opt!('\0', "seq") {
                seq = true;
            } else if opt!('\0', "stream") {
                stream = Some(stream.unwrap_or(false));
            } else if opt!('\0', "stream-errors") {
                stream = Some(true);
            } else if opt!('e', "exit-status") {
                exit_status = true;
            } else if opt!('\0', "args") {
                further_strings = true;
                further_json = false;
            } else if opt!('\0', "jsonargs") {
                further_strings = false;
                further_json = true;
            } else if opt!('\0', "arg") {
                if i + consumed + 2 >= argv.len() {
                    eprintln!("jq: --arg takes two parameters (e.g. --arg varname value)");
                    die();
                }
                let k = argv[i + consumed + 1].clone();
                if !named.contains_key(k.as_str()) {
                    named.insert(Rc::from(k.as_str()), Value::str(&argv[i + consumed + 2]));
                }
                consumed += 2;
            } else if opt!('\0', "argjson") {
                if i + consumed + 2 >= argv.len() {
                    eprintln!("jq: --argjson takes two parameters (e.g. --argjson varname text)");
                    die();
                }
                let k = argv[i + consumed + 1].clone();
                if !named.contains_key(k.as_str()) {
                    match json::parse_one(&argv[i + consumed + 2]) {
                        Ok(v) => {
                            named.insert(Rc::from(k.as_str()), v);
                        }
                        Err(_) => {
                            eprintln!("jq: invalid JSON text passed to --argjson");
                            die();
                        }
                    }
                }
                consumed += 2;
            } else if !is_short && (text == "rawfile" || text == "slurpfile") {
                let which = std::mem::take(&mut text);
                let raw = which == "rawfile";
                if i + consumed + 2 >= argv.len() {
                    eprintln!("jq: --{which} takes two parameters (e.g. --{which} varname filename)");
                    die();
                }
                let k = argv[i + consumed + 1].clone();
                let f = argv[i + consumed + 2].clone();
                if !named.contains_key(k.as_str()) {
                    match load_file(&f, raw) {
                        Ok(v) => {
                            named.insert(Rc::from(k.as_str()), v);
                        }
                        Err(e) => {
                            eprintln!("jq: Bad JSON in --{which} {k} {f}: {e}");
                            return ERROR_SYSTEM;
                        }
                    }
                }
                consumed += 2;
            } else if opt!('\0', "debug-dump-disasm") || opt!('\0', "debug-trace=all") || opt!('\0', "debug-trace") {
            } else if opt!('h', "help") {
                usage(0, false);
            } else if opt!('V', "version") {
                println!("jq-{VERSION}");
                return OK;
            } else if opt!('\0', "build-configuration") {
                println!("{BUILD_CONFIGURATION}");
                return OK;
            } else if opt!('\0', "run-tests") {
                let rest = &argv[i + consumed + 1..];
                return runtests::testsuite(lib_paths.clone().unwrap_or_default(), rest);
            } else {
                if is_short {
                    eprintln!("jq: Unknown option -{}", text.chars().next().unwrap());
                } else {
                    eprintln!("jq: Unknown option --{text}");
                }
                die();
            }
        }
        i += 1 + consumed;
    }

    let stdout_tty = unsafe { libc::isatty(1) } == 1;
    let stdin_tty = unsafe { libc::isatty(0) } == 1;
    let mut use_color = stdout_tty && std::env::var("NO_COLOR").map_or(true, |v| v.is_empty());
    if color {
        use_color = true;
    }
    if no_color {
        use_color = false;
    }
    let mut colors = Colors::default();
    if let Ok(spec) = std::env::var("JQ_COLORS") {
        match Colors::from_env(&spec) {
            Some(c) => colors = c,
            None => eprintln!("Failed to set $JQ_COLORS"),
        }
    }
    let lib_dirs = lib_paths.unwrap_or_else(|| vec!["~/.jq".into(), "$ORIGIN/../lib/jq".into(), "$ORIGIN/../lib".into()]);
    let jq_origin = {
        let a0 = argv.first().cloned().unwrap_or_default();
        match a0.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => a0[..i].to_string(),
            None => ".".to_string(),
        }
    };
    if program.is_none() && !from_file && (!stdout_tty || !stdin_tty) {
        program = Some(".".into());
    }
    let Some(program) = program else { usage(2, true) };

    let (src, fname, prog_origin) = if from_file {
        let data = match std::fs::read(&program) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("jq: Could not open {}: {}", program, io_error_text(&e));
                return ERROR_SYSTEM;
            }
        };
        if data.contains(&0) {
            eprintln!("jq: program file contains NUL bytes");
            return ERROR_SYSTEM;
        }
        let dir = match program.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => program[..i].to_string(),
            None => ".".to_string(),
        };
        let dir = std::fs::canonicalize(&dir).map(|p| p.display().to_string()).unwrap_or(dir);
        (value::utf8_lossy(&data), program.clone(), dir)
    } else {
        let dir = std::fs::canonicalize(".").map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into());
        (program.clone(), "<top-level>".to_string(), dir)
    };

    let mut args_obj = Map::new();
    args_obj.insert(Rc::from("positional"), Value::arr(positional));
    args_obj.insert(Rc::from("named"), Value::obj(named.clone()));
    let mut globals: Vec<(String, Value)> = named.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    globals.push(("ARGS".into(), Value::obj(args_obj)));
    if !named.contains_key("JQ_BUILD_CONFIGURATION") {
        globals.push(("JQ_BUILD_CONFIGURATION".into(), Value::str(BUILD_CONFIGURATION)));
    }

    let mut it = Interp::new();
    it.env_value = env_object();
    it.search_list = lib_dirs.clone();
    it.jq_origin = jq_origin.clone();
    it.prog_origin = Value::string(prog_origin.clone());
    let opts = modules::Options { lib_dirs, jq_origin, prog_origin, globals };
    let compiled = match modules::compile(&mut it, &src, &fname, &opts) {
        Ok(c) => c,
        Err(errs) => {
            for e in &errs {
                eprintln!("{e}");
            }
            eprintln!("jq: {} compile {}", errs.len(), if errs.len() == 1 { "error" } else { "errors" });
            return ERROR_COMPILE;
        }
    };

    let fmt = Fmt { pretty, indent: indent as usize, tab, sort, ascii, color: if use_color { Some(colors) } else { None } };
    let dbg_fmt = Fmt { pretty: false, indent: 0, tab: false, ..fmt.clone() };
    it.debug_out = Box::new(move |v: &Value| {
        let s = dump(&Value::arr(vec![Value::str("DEBUG:"), v.clone()]), &dbg_fmt);
        let mut e = std::io::stderr().lock();
        let _ = writeln!(e, "{s}");
    });
    it.stderr_out = Box::new(|v: &Value| {
        let mut e = std::io::stderr().lock();
        match v {
            Value::Str(s) => {
                let _ = e.write_all(s.as_bytes());
            }
            other => {
                let _ = e.write_all(dump(other, &Fmt::compact()).as_bytes());
            }
        }
    });

    if files.is_empty() {
        files.push("-".into());
    }
    let state = Rc::new(std::cell::RefCell::new(InputState::new(files, raw_in, slurp, stream, seq)));
    it.inputs = Some(Box::new(SharedInputs(state.clone())));
    let mut out = Out { w: std::io::BufWriter::with_capacity(65536, std::io::stdout()), fmt, raw: raw_out, raw0, no_lf, ascii, seq, unbuffered, failed: false };

    let mut ret = OK_NO_OUTPUT;
    let mut last_result: i32 = -1;
    let mut halted = false;
    if null_in {
        let (r, h) = process(&mut it, &compiled, Value::Null, &mut out, &state);
        ret = r;
        halted = h;
    } else {
        loop {
            if state.borrow().failures != 0 {
                break;
            }
            let next = state.borrow_mut().next_value();
            match next {
                None => break,
                Some(Ok(v)) => {
                    let (r, h) = process(&mut it, &compiled, v, &mut out, &state);
                    ret = r;
                    if ret <= 0 && ret != OK_NO_OUTPUT {
                        last_result = (ret != OK_NULL_KIND) as i32;
                    }
                    if h {
                        halted = true;
                        break;
                    }
                }
                Some(Err(msg)) => {
                    let _ = out.w.flush();
                    if !seq {
                        ret = ERROR_UNKNOWN;
                        eprintln!("jq: parse error: {msg}");
                        break;
                    }
                    eprintln!("jq: ignoring parse error: {msg}");
                }
            }
        }
    }
    let _ = halted;
    if state.borrow().failures != 0 {
        ret = ERROR_SYSTEM;
    }
    if out.w.flush().is_err() || out.failed {
        eprintln!("jq: error: writing output failed: {}", io_error_text(&std::io::Error::last_os_error()));
        ret = ERROR_SYSTEM;
    }
    if exit_status {
        if ret != OK_NO_OUTPUT {
            return ret.abs();
        }
        return match last_result {
            -1 => OK_NO_OUTPUT.abs(),
            0 => OK_NULL_KIND.abs(),
            _ => OK,
        };
    }
    if ret > 0 {
        ret
    } else {
        0
    }
}

/// Runs the program on one input, printing its outputs; (status, halted).
fn process(it: &mut Interp, c: &modules::Compiled, input: Value, out: &mut Out, state: &Rc<std::cell::RefCell<InputState>>) -> (i32, bool) {
    let mut ret = OK_NO_OUTPUT;
    let r = it.eval(c.main, &c.env, Pv::val(input), false, &mut |_, pv| {
        let v = pv.v;
        match &v {
            Value::Str(s) if out.raw => {
                if out.ascii {
                    let mut t = String::new();
                    value::dump_string(s, true, &mut t);
                    out.write(t.as_bytes());
                } else if out.raw0 && s.contains('\0') {
                    return Err(interp::err("Cannot dump a string containing NUL with --raw-output0 option"));
                } else {
                    out.write(s.as_bytes());
                }
                ret = OK;
            }
            _ => {
                ret = if matches!(v, Value::Bool(false) | Value::Null) { OK_NULL_KIND } else { OK };
                if out.seq {
                    out.write(b"\x1e");
                }
                let s = dump(&v, &out.fmt);
                out.write(s.as_bytes());
            }
        }
        if !out.no_lf {
            out.write(b"\n");
        }
        if out.raw0 {
            out.write(b"\0");
        }
        if out.unbuffered {
            let _ = out.w.flush();
        }
        Ok(())
    });
    let r = match r {
        Err(Flow::Outer(e)) => Err(*e),
        r => r,
    };
    match r {
        Ok(()) => (ret, false),
        Err(Flow::Halt(code, msg)) => {
            let _ = out.w.flush();
            let mut e = std::io::stderr().lock();
            match msg {
                Some(Value::Str(s)) => {
                    let _ = e.write_all(s.as_bytes());
                }
                Some(Value::Null) | None => {}
                Some(v) => {
                    let _ = writeln!(e, "{}", dump(&v, &Fmt::compact()));
                }
            }
            (code, true)
        }
        Err(Flow::Err(msg)) => {
            let _ = out.w.flush();
            let pos = state.borrow().position();
            match msg {
                Value::Str(s) => eprintln!("jq: error (at {pos}): {s}"),
                other => eprintln!("jq: error (at {pos}) (not a string): {}", dump(&other, &Fmt::compact())),
            }
            (ERROR_UNKNOWN, false)
        }
        Err(Flow::Break(_)) | Err(Flow::Outer(_)) => (ERROR_UNKNOWN, false),
    }
}

/// The interpreter recurses (continuations); it runs on a thread with a big stack.
pub fn run_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    #[cfg(target_arch = "wasm32")]
    const STACK: usize = 64 << 20;
    #[cfg(not(target_arch = "wasm32"))]
    const STACK: usize = 1 << 30;
    let h = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            interp::set_stack_base(STACK);
            f()
        })
        .expect("thread");
    match h.join() {
        Ok(v) => v,
        Err(_) => std::process::exit(5),
    }
}

fn main() {
    // SIGPIPE: quietly end, as a C program does.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let code = run_big_stack(real_main);
    std::process::exit(code);
}
