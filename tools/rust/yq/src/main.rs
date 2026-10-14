//! yq: a Rust reimplementation of mikefarah/yq v4.54.1 for the guest. The command line follows
//! yq's cobra/pflag setup (commands, flags, help and error texts), the evaluation its yqlib.

pub mod color;
pub mod envsubst;
pub mod eval;
pub mod formats;
pub mod formats_extra;
pub mod goregex;
pub mod gorand;
pub mod gostr;
pub mod gotime;
pub mod harness;
pub mod harness_extra;
pub mod main_support;
pub mod node;
pub mod ops;
pub mod ops2;
pub mod parser;
pub mod prefs;
pub mod printer;
pub mod util;
pub mod yaml;

use eval::{Ctx, Res};
use node::Engine;
use parser::Fmt;
use std::io::Read;

const VERSION: &str = "v4.54.1";
const PRETTY_PRINT_EXP: &str = r#"(... | (select(tag != "!!str"), select(tag == "!!str") | select(test("(?i)^(y|yes|n|no|on|off)$") | not))  ) style="""#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Root,
    Eval,
    EvalAll,
    Completion,
}

impl Cmd {
    fn help(self) -> &'static str {
        match self {
            Cmd::Root => include_str!("help/root_help.txt"),
            Cmd::Eval => include_str!("help/eval_help.txt"),
            Cmd::EvalAll => include_str!("help/evalall_help.txt"),
            Cmd::Completion => include_str!("help/completion_help.txt"),
        }
    }
    fn usage(self) -> &'static str {
        match self {
            Cmd::Root => include_str!("help/root_usage.txt"),
            Cmd::Eval => include_str!("help/eval_usage.txt"),
            Cmd::EvalAll => include_str!("help/evalall_usage.txt"),
            Cmd::Completion => include_str!("help/completion_usage.txt"),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FlagType {
    Bool,
    Str,
    Int,
    Char,
    Unwrap,
}

struct FlagDef {
    name: &'static str,
    short: Option<char>,
    typ: FlagType,
    /// persistent (global) flags apply to every command; the others only to the root
    persistent: bool,
}

const FLAGS: &[FlagDef] = &[
    FlagDef { name: "verbose", short: Some('v'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "debug-node-info", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "tojson", short: Some('j'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "output-format", short: Some('o'), typ: FlagType::Str, persistent: true },
    FlagDef { name: "input-format", short: Some('p'), typ: FlagType::Str, persistent: true },
    FlagDef { name: "xml-attribute-prefix", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "xml-content-name", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "xml-strict-mode", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "xml-keep-namespace", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "xml-raw-token", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "xml-proc-inst-prefix", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "xml-directive-name", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "xml-skip-proc-inst", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "xml-skip-directives", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "csv-auto-parse", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "csv-separator", short: None, typ: FlagType::Char, persistent: true },
    FlagDef { name: "tsv-auto-parse", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "lua-prefix", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "lua-suffix", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "lua-unquoted", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "lua-globals", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "ini-preserve-quotes", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "ini-key-value-delimiters", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "properties-separator", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "properties-array-brackets", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "shell-key-separator", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "string-interpolation", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "null-input", short: Some('n'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "no-doc", short: Some('N'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "indent", short: Some('I'), typ: FlagType::Int, persistent: true },
    FlagDef { name: "inplace", short: Some('i'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "unwrapScalar", short: Some('r'), typ: FlagType::Unwrap, persistent: true },
    FlagDef { name: "nul-output", short: Some('0'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "prettyPrint", short: Some('P'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "exit-status", short: Some('e'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "colors", short: Some('C'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "no-colors", short: Some('M'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "front-matter", short: Some('f'), typ: FlagType::Str, persistent: true },
    FlagDef { name: "expression", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "header-preprocess", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "yaml-fix-merge-anchor-to-spec", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "yaml-compact-seq-indent", short: Some('c'), typ: FlagType::Bool, persistent: true },
    FlagDef { name: "split-exp", short: Some('s'), typ: FlagType::Str, persistent: true },
    FlagDef { name: "split-exp-file", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "from-file", short: None, typ: FlagType::Str, persistent: true },
    FlagDef { name: "security-disable-env-ops", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "security-disable-file-ops", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "security-enable-system-operator", short: None, typ: FlagType::Bool, persistent: true },
    FlagDef { name: "version", short: Some('V'), typ: FlagType::Bool, persistent: false },
    FlagDef { name: "help", short: Some('h'), typ: FlagType::Bool, persistent: false },
];

struct Opts {
    prefs: prefs::Prefs,
    verbose: bool,
    print_node_info: bool,
    output_to_json: bool,
    output_format: String,
    input_format: String,
    null_input: bool,
    no_doc_separators: bool,
    indent: i64,
    write_inplace: bool,
    unwrap_explicit: Option<bool>,
    nul_sep_output: bool,
    pretty_print: bool,
    exit_status: bool,
    force_color: bool,
    force_no_color: bool,
    front_matter: String,
    force_expression: String,
    split_file_exp: String,
    split_file_exp_file: String,
    expression_file: String,
    version: bool,
    help: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            prefs: prefs::Prefs::default(),
            verbose: false,
            print_node_info: false,
            output_to_json: false,
            output_format: "auto".into(),
            input_format: "auto".into(),
            null_input: false,
            no_doc_separators: false,
            indent: 2,
            write_inplace: false,
            unwrap_explicit: None,
            nul_sep_output: false,
            pretty_print: false,
            exit_status: false,
            force_color: false,
            force_no_color: false,
            front_matter: String::new(),
            force_expression: String::new(),
            split_file_exp: String::new(),
            split_file_exp_file: String::new(),
            expression_file: String::new(),
            version: false,
            help: false,
        }
    }
}

fn parse_bool(v: &str) -> Result<bool, ()> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(()),
    }
}

fn flag_label(d: &FlagDef) -> String {
    match d.short {
        Some(s) => format!("-{s}, --{}", d.name),
        None => format!("--{}", d.name),
    }
}

fn set_flag(o: &mut Opts, d: &FlagDef, value: &str) -> Result<(), String> {
    let bad = |what: &str| format!("invalid argument {} for {} flag: {what}", util::go_quote(value), util::go_quote(&flag_label(d)));
    let b = || parse_bool(value).map_err(|_| bad(&format!("strconv.ParseBool: parsing {}: invalid syntax", util::go_quote(value))));
    let p = &mut o.prefs;
    match d.name {
        "verbose" => o.verbose = b()?,
        "debug-node-info" => o.print_node_info = b()?,
        "tojson" => {
            o.output_to_json = b()?;
            eprintln!("Flag --tojson has been deprecated, please use -o=json instead");
        }
        "output-format" => o.output_format = value.into(),
        "input-format" => o.input_format = value.into(),
        "xml-attribute-prefix" => p.xml.attribute_prefix = value.into(),
        "xml-content-name" => p.xml.content_name = value.into(),
        "xml-strict-mode" => p.xml.strict_mode = b()?,
        "xml-keep-namespace" => p.xml.keep_namespace = b()?,
        "xml-raw-token" => p.xml.use_raw_token = b()?,
        "xml-proc-inst-prefix" => p.xml.proc_inst_prefix = value.into(),
        "xml-directive-name" => p.xml.directive_name = value.into(),
        "xml-skip-proc-inst" => p.xml.skip_proc_inst = b()?,
        "xml-skip-directives" => p.xml.skip_directives = b()?,
        "csv-auto-parse" => p.csv.auto_parse = b()?,
        "csv-separator" => {
            let v = value.replace("\\n", "\n").replace("\\t", "\t").replace("\\r", "\r").replace("\\f", "\x0c").replace("\\v", "\x0b");
            if v.len() != 1 {
                return Err(bad(&format!("[{v}] is not a valid character. Must be length 1 was {}", v.len())));
            }
            p.csv.separator = v.chars().next().unwrap();
        }
        "tsv-auto-parse" => p.tsv.auto_parse = b()?,
        "lua-prefix" => p.lua.doc_prefix = value.into(),
        "lua-suffix" => p.lua.doc_suffix = value.into(),
        "lua-unquoted" => p.lua.unquoted_keys = b()?,
        "lua-globals" => p.lua.globals = b()?,
        "ini-preserve-quotes" => p.ini.preserve_surrounded_quote = b()?,
        "ini-key-value-delimiters" => p.ini.key_value_delimiters = value.into(),
        "properties-separator" => p.props.key_value_separator = value.into(),
        "properties-array-brackets" => p.props.use_array_brackets = b()?,
        "shell-key-separator" => p.shell.key_separator = value.into(),
        "string-interpolation" => p.string_interpolation = b()?,
        "null-input" => o.null_input = b()?,
        "no-doc" => o.no_doc_separators = b()?,
        "indent" => {
            o.indent = gostr::parse_int(value, 0).map_err(|e| match e {
                gostr::NumErr::Range => bad(&format!("strconv.ParseInt: parsing {}: value out of range", util::go_quote(value))),
                _ => bad(&format!("strconv.ParseInt: parsing {}: invalid syntax", util::go_quote(value))),
            })?
        }
        "inplace" => o.write_inplace = b()?,
        "unwrapScalar" => {
            let v = parse_bool(value);
            o.unwrap_explicit = Some(v.unwrap_or(false));
            if v.is_err() {
                return Err(bad(&format!("strconv.ParseBool: parsing {}: invalid syntax", util::go_quote(value))));
            }
        }
        "nul-output" => o.nul_sep_output = b()?,
        "prettyPrint" => o.pretty_print = b()?,
        "exit-status" => o.exit_status = b()?,
        "colors" => o.force_color = b()?,
        "no-colors" => o.force_no_color = b()?,
        "front-matter" => o.front_matter = value.into(),
        "expression" => o.force_expression = value.into(),
        "header-preprocess" => p.yaml.leading_content_pre_processing = b()?,
        "yaml-fix-merge-anchor-to-spec" => p.yaml.fix_merge_anchor_to_spec = b()?,
        "yaml-compact-seq-indent" => p.yaml.compact_sequence_indent = b()?,
        "split-exp" => o.split_file_exp = value.into(),
        "split-exp-file" => o.split_file_exp_file = value.into(),
        "from-file" => o.expression_file = value.into(),
        "security-disable-env-ops" => p.security.disable_env_ops = b()?,
        "security-disable-file-ops" => p.security.disable_file_ops = b()?,
        "security-enable-system-operator" => p.security.enable_system_ops = b()?,
        "version" => o.version = b()?,
        "help" => o.help = b()?,
        _ => {}
    }
    Ok(())
}

fn has_no_opt_default(d: &FlagDef) -> bool {
    matches!(d.typ, FlagType::Bool | FlagType::Unwrap)
}

/// pflag's parsing of the command's arguments: (args, error).
fn parse_flags(cmd: Cmd, args: &[String], o: &mut Opts) -> Result<Vec<String>, String> {
    let find_long = |n: &str| FLAGS.iter().find(|d| d.name == n && (d.persistent || cmd == Cmd::Root || d.name == "help"));
    let find_short = |c: char| FLAGS.iter().find(|d| d.short == Some(c) && (d.persistent || cmd == Cmd::Root || d.name == "help"));
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            out.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(body) = a.strip_prefix("--") {
            let (name, value) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let Some(d) = find_long(name) else {
                return Err(format!("unknown flag: --{name}"));
            };
            let value = match value {
                Some(v) => v,
                None if has_no_opt_default(d) => "true".to_string(),
                None => {
                    if i < args.len() {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        return Err(format!("flag needs an argument: --{name}"));
                    }
                }
            };
            set_flag(o, d, &value)?;
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                let Some(d) = find_short(c) else {
                    return Err(format!("unknown shorthand flag: '{c}' in {a}"));
                };
                let rest: String = chars[j + 1..].iter().collect();
                if rest.starts_with('=') {
                    set_flag(o, d, &rest[1..])?;
                    break;
                }
                if has_no_opt_default(d) {
                    set_flag(o, d, "true")?;
                    j += 1;
                    continue;
                }
                if !rest.is_empty() {
                    set_flag(o, d, &rest)?;
                    break;
                }
                if i < args.len() {
                    i += 1;
                    set_flag(o, d, &args[i - 1])?;
                    break;
                }
                return Err(format!("flag needs an argument: '{c}' in -{c}"));
            }
            continue;
        }
        out.push(a.clone());
    }
    Ok(out)
}

fn is_file(path: &str) -> bool {
    std::fs::metadata(path).map(|m| !m.is_dir()).unwrap_or(false)
}

fn stdin_is_piped() -> bool {
    // a character device (terminal) is not "piping"
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(0, &mut st) != 0 {
            return false;
        }
        (st.st_mode & libc::S_IFMT) != libc::S_IFCHR
    }
}

fn stdout_is_terminal() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

fn read_input(name: &str) -> Res<Vec<u8>> {
    if name == "-" {
        let mut b = Vec::new();
        std::io::stdin().read_to_end(&mut b).map_err(|e| e.to_string())?;
        return Ok(b);
    }
    std::fs::read(name).map_err(|e| format!("open {name}: {}", main_support::io_err(&e)))
}

struct Run {
    o: Opts,
    unwrap_scalar: bool,
    colors_enabled: bool,
}

fn run_eval(cmd: Cmd, o: Opts, args: Vec<String>) -> Result<(), (String, bool)> {
    let mut r = Run { o, unwrap_scalar: false, colors_enabled: false };
    let o = &mut r.o;
    // setupColors
    if o.force_color || (!o.force_no_color && stdout_is_terminal()) {
        r.colors_enabled = true;
    }
    // processArgs
    let mut expression = o.force_expression.clone();
    let mut args = args;
    let piping = stdin_is_piped();
    if !(o.null_input || !piping || args.len() > 1 || (!args.is_empty() && is_file(&args[0])) || args.iter().any(|a| a == "-")) {
        args.push("-".into());
    }
    let first_is_file = !args.is_empty() && is_file(&args[0]);
    if o.expression_file.is_empty() && first_is_file && args[0].ends_with(".yq") {
        o.expression_file = args.remove(0);
    }
    if !o.expression_file.is_empty() {
        let b = std::fs::read(&o.expression_file).map_err(|e| (format!("open {}: {}", o.expression_file, main_support::io_err(&e)), false))?;
        expression = String::from_utf8_lossy(&b).replace("\r\n", "\n");
    }
    if expression.is_empty() && !args.is_empty() && args[0] != "-" && !is_file(&args[0]) {
        expression = args.remove(0);
    }
    if !o.split_file_exp_file.is_empty() {
        let b = std::fs::read(&o.split_file_exp_file)
            .map_err(|e| (format!("open {}: {}", o.split_file_exp_file, main_support::io_err(&e)), false))?;
        o.split_file_exp = String::from_utf8_lossy(&b).into_owned();
    }
    if o.output_to_json {
        o.output_format = "json".into();
    }
    // validateCommandFlags
    if o.write_inplace && (args.is_empty() || args[0] == "-") {
        return Err(("write in place flag only applicable when giving an expression and at least one file".into(), false));
    }
    if !o.front_matter.is_empty() && args.is_empty() {
        return Err(("front matter flag only applicable when giving an expression and at least one file".into(), false));
    }
    if o.write_inplace && !o.split_file_exp.is_empty() {
        return Err(("write in place cannot be used with split file".into(), false));
    }
    if o.null_input && !args.is_empty() {
        return Err(("cannot pass files in when using null-input flag".into(), false));
    }
    if o.indent < 0 {
        return Err(("indent must not be negative".into(), false));
    }
    // configureFormats
    let input_filename = args.first().cloned().unwrap_or_default();
    let auto_out = |f: &str| f.is_empty() || f == "auto" || f == "a";
    if o.input_format.is_empty() || o.input_format == "auto" || o.input_format == "a" {
        o.input_format = formats::format_string_from_filename(&input_filename);
        if formats::format_from_string(&o.input_format).is_err() {
            o.input_format = "yaml".into();
            if auto_out(&o.output_format) {
                o.output_format = "yaml".into();
            }
        } else if auto_out(&o.output_format) {
            o.output_format = o.input_format.clone();
        }
    } else if auto_out(&o.output_format) {
        let would_be = formats::format_string_from_filename(&input_filename);
        if input_filename != "-" {
            util::warn(&format!(
                "yq default output is now 'auto' (based on the filename extension). Normally yq would output '{would_be}', but for backwards compatibility 'yaml' has been set. Please use -oy to specify yaml, or drop the -p flag."
            ));
        }
        o.output_format = "yaml".into();
    }
    let out_fmt = formats::format_from_string(&o.output_format).map_err(|e| (e, false))?;
    if out_fmt == Fmt::Yaml || out_fmt == Fmt::Props {
        r.unwrap_scalar = true;
    }
    if let Some(v) = o.unwrap_explicit {
        r.unwrap_scalar = v;
    }
    let mut engine = Engine::new(o.prefs.clone());
    if o.write_inplace {
        r.colors_enabled = o.force_color;
    }
    if !o.split_file_exp.is_empty() {
        r.colors_enabled = o.force_color;
    }
    // configureEncoder
    {
        let p = &mut engine.prefs;
        p.xml.indent = o.indent;
        p.yaml.indent = o.indent;
        p.kyaml.indent = o.indent;
        p.json.indent = o.indent;
        p.yaml.unwrap_scalar = r.unwrap_scalar;
        p.kyaml.unwrap_scalar = r.unwrap_scalar;
        p.props.unwrap_scalar = r.unwrap_scalar;
        p.json.unwrap_scalar = r.unwrap_scalar;
        p.shell.unwrap_scalar = r.unwrap_scalar;
        p.yaml.colors_enabled = r.colors_enabled;
        p.kyaml.colors_enabled = r.colors_enabled;
        p.json.colors_enabled = r.colors_enabled;
        p.hcl_colors = r.colors_enabled;
        p.toml_colors = r.colors_enabled;
        p.yaml.print_doc_separators = !o.no_doc_separators;
        p.kyaml.print_doc_separators = !o.no_doc_separators;
        p.debug = o.verbose;
    }
    let in_fmt = formats::format_from_string(&o.input_format).map_err(|e| (e, false))?;
    engine.prefs.yaml.evaluate_together = cmd == Cmd::EvalAll;
    let encoder = formats::encoder_for(&engine, out_fmt).ok_or_else(|| (format!("no support for {} output format", o.output_format), false))?;
    let writer = if !o.split_file_exp.is_empty() {
        let exp = parser::parse_expression(&mut engine, &o.split_file_exp).map_err(|e| (format!("bad split document expression: {e}"), false))?;
        let extension = match out_fmt {
            Fmt::Json => "json",
            Fmt::Props => "properties",
            _ => "yml",
        };
        printer::Writer::Split { exp, extension, index: 0 }
    } else {
        printer::Writer::Stdout(Vec::new())
    };
    let mut printer = printer::Printer::new(encoder, writer);
    if o.print_node_info && cmd != Cmd::EvalAll {
        printer.node_info = true;
    }
    if o.nul_sep_output {
        printer.nul_sep = true;
    }
    let decoder = formats::decoder_for(&engine, in_fmt);
    let decoder_err = || (format!("no support for {} input format", o.input_format), false);
    // front matter
    let mut filename_alias: Option<(String, String)> = None;
    let mut front_matter_input: Option<Vec<u8>> = None;
    if !o.front_matter.is_empty() {
        let original = args[0].clone();
        let data = read_input(&original).map_err(|e| (e, false))?;
        let data = if data.starts_with(&[0xEF, 0xBB, 0xBF]) { data[3..].to_vec() } else { data };
        let (front, rest) = split_front_matter(&data);
        front_matter_input = Some(front);
        filename_alias = Some((original.clone(), original));
        if o.front_matter == "process" {
            printer.appendix = Some(rest);
        }
    }
    let expression = process_expression(&expression, o.pretty_print);
    // in place: write to a buffer, then over the file
    let inplace_target = if o.write_inplace { Some(args[0].clone()) } else { None };
    if inplace_target.is_some() {
        printer.writer = printer::Writer::Buffer(Vec::new());
    }
    let result: Res<()> = (|| {
        if args.is_empty() {
            if o.null_input {
                return evaluate_new(&mut engine, &expression, &mut printer);
            }
            print!("{}\n", cmd_for_usage(cmd).usage());
            return Ok(());
        }
        let mut decoder = decoder.ok_or_else(|| decoder_err().0)?;
        let inputs = |i: usize, name: &str| -> Res<Vec<u8>> {
            if i == 0 {
                if let Some(f) = &front_matter_input {
                    return Ok(f.clone());
                }
            }
            read_input(name)
        };
        let display = |name: &str| -> String {
            match &filename_alias {
                Some((_, orig)) if name == args[0] => orig.clone(),
                _ => name.to_string(),
            }
        };
        if cmd == Cmd::EvalAll {
            let node = parser::parse_expression(&mut engine, &expression)?;
            let mut all = Vec::new();
            for (i, f) in args.iter().enumerate() {
                let data = inputs(i, f)?;
                let docs = printer::read_documents(&mut engine, data, &display(f), i as i64, decoder.as_mut())?;
                all.extend(docs);
            }
            if all.is_empty() {
                all.push(engine.scalar("!!null", ""));
            }
            let ctx = engine.get_matching_nodes(&Ctx::new(all), node.as_ref())?;
            printer.print_results(&mut engine, &ctx.nodes)?;
            return Ok(());
        }
        let node = parser::parse_expression(&mut engine, &expression)?;
        let mut total = 0u64;
        let mut file_index = 0i64;
        for (i, f) in args.iter().enumerate() {
            let data = inputs(i, f)?;
            let name = display(f);
            decoder.init(data)?;
            let mut idx = 0u64;
            loop {
                match decoder.decode(&mut engine) {
                    Ok(None) => {
                        file_index += 1;
                        break;
                    }
                    Ok(Some(n)) => {
                        let nn = engine.nm(n);
                        nn.document = idx;
                        nn.filename = name.as_str().into();
                        nn.file_index = file_index;
                        let ctx = engine.get_matching_nodes(&Ctx::new(vec![n]), node.as_ref())?;
                        printer.print_results(&mut engine, &ctx.nodes)?;
                        idx += 1;
                    }
                    Err(e) => return Err(format!("bad file '{name}': {e}")),
                }
            }
            total += idx;
        }
        if total == 0 {
            return evaluate_new(&mut engine, &expression, &mut printer);
        }
        Ok(())
    })();
    let completed = result.is_ok();
    if let Some(target) = &inplace_target {
        if completed {
            if let printer::Writer::Buffer(b) = &printer.writer {
                std::fs::write(target, b).map_err(|e| (format!("open {target}: {}", main_support::io_err(&e)), true))?;
            }
        }
    }
    result.map_err(|e| (e, true))?;
    if o.exit_status && !printer.printed_anything() {
        return Err(("no matches found".into(), true));
    }
    Ok(())
}

fn cmd_for_usage(cmd: Cmd) -> Cmd {
    match cmd {
        Cmd::Root => Cmd::Root,
        c => c,
    }
}

fn process_expression(expression: &str, pretty: bool) -> String {
    if pretty && expression.is_empty() {
        PRETTY_PRINT_EXP.into()
    } else if pretty {
        format!("{expression} | {PRETTY_PRINT_EXP}")
    } else {
        expression.into()
    }
}

fn evaluate_new(engine: &mut Engine, expression: &str, printer: &mut printer::Printer) -> Res<()> {
    let node = parser::parse_expression(engine, expression)?;
    let n = engine.scalar("!!null", "");
    let ctx = engine.get_matching_nodes(&Ctx::new(vec![n]), node.as_ref())?;
    printer.print_results(engine, &ctx.nodes)
}

/// frontMatterHandler.Split: the YAML front matter (up to the second `---` line) and the rest.
fn split_front_matter(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut pos = 0;
    let mut line_count = 0;
    let mut front = Vec::new();
    loop {
        if data.len() - pos < 3 {
            // Peek(3) fails at EOF: everything left stays in the reader
            break;
        }
        if line_count > 0 && &data[pos..pos + 3] == b"---" {
            break;
        }
        let end = match data[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => pos + i + 1,
            None => data.len(),
        };
        front.extend_from_slice(&data[pos..end]);
        pos = end;
        line_count += 1;
    }
    (front, data[pos..].to_vec())
}

fn completion(args: &[String]) -> Result<(), String> {
    let valid = ["bash", "zsh", "fish", "powershell"];
    if args.len() != 1 {
        return Err(format!("accepts 1 arg(s), received {}", args.len()));
    }
    let s = match args[0].as_str() {
        "bash" => include_str!("help/completion_bash.txt"),
        "zsh" => include_str!("help/completion_zsh.txt"),
        "fish" => include_str!("help/completion_fish.txt"),
        "powershell" => include_str!("help/completion_powershell.txt"),
        other => return Err(format!("invalid argument \"{other}\" for \"yq completion\"")),
    };
    let _ = valid;
    print!("{s}");
    Ok(())
}

fn main() {
    // test hooks
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--time-harness") {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let line = line.unwrap();
            let p: Vec<&str> = line.split('\t').collect();
            match gotime::parse(p[0], p[1]) {
                Ok(t) => println!("{} | {}", t.format(p[2]), t.sec),
                Err(e) => println!("ERR {e}"),
            }
        }
        return;
    }
    if argv.get(1).map(String::as_str) == Some("--format-harness") {
        std::process::exit(harness::run_formats(&argv[2], argv.get(3).map(String::as_str)));
    }
    if argv.get(1).map(String::as_str) == Some("--scenario-harness") {
        std::process::exit(harness::run(&argv[2], argv.get(3).map(String::as_str)));
    }
    if argv.get(1).map(String::as_str) == Some("--yaml-roundtrip") {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        let mut c = yaml::composer::Composer::new(&input);
        let mut out = std::io::stdout();
        loop {
            match c.next_document() {
                Ok(Some(doc)) => {
                    let opts = yaml::serializer::DumpOptions { indent: 2, compact_seq_indent: false, line_width: -1 };
                    match yaml::serializer::dump(&doc.content[0], opts) {
                        Ok(b) => {
                            std::io::Write::write_all(&mut out, &b).unwrap();
                            println!("---");
                        }
                        Err(e) => {
                            println!("DUMPERR {e}");
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    println!("ERR {e}");
                    break;
                }
            }
        }
        return;
    }
    std::process::exit(run_cli(argv[1..].to_vec()));
}

fn run_cli(args: Vec<String>) -> i32 {
    // yq's main: when the arguments do not name a command, run `eval`
    let commands = strip_flags(&args);
    let (cmd, rest): (Cmd, Vec<String>) = match commands.first().map(String::as_str) {
        Some(c @ ("eval" | "e")) => (Cmd::Eval, remove_first(&args, c)),
        Some(c @ ("eval-all" | "ea")) => (Cmd::EvalAll, remove_first(&args, c)),
        Some(c @ ("completion" | "shell-completion")) => (Cmd::Completion, remove_first(&args, c)),
        Some(_) => (Cmd::Eval, args.clone()),
        None => (Cmd::Root, args.clone()),
    };
    let mut o = Opts::default();
    if std::env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false) {
        o.force_no_color = true;
    }
    let pos = match parse_flags(cmd, &rest, &mut o) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            eprint!("{}", cmd.usage());
            return 1;
        }
    };
    if o.help {
        print!("{}", cmd.help());
        return 0;
    }
    if std::env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false) {
        o.force_no_color = true;
    }
    if cmd == Cmd::Completion {
        return match completion(&pos) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                eprint!("{}", cmd.usage());
                1
            }
        };
    }
    if cmd == Cmd::Root && o.version {
        print!("yq (https://github.com/mikefarah/yq/) version {VERSION}\n");
        return 0;
    }
    match run_eval(cmd, o, pos) {
        Ok(()) => 0,
        Err((e, _)) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// cobra's stripFlags: the non-flag arguments, skipping the values of flags that take one.
fn strip_flags(args: &[String]) -> Vec<String> {
    let no_opt = |name: &str| FLAGS.iter().find(|d| d.name == name).map(has_no_opt_default).unwrap_or(false);
    let short_no_opt = |c: &str| {
        let ch = c.chars().next();
        FLAGS.iter().find(|d| d.short == ch).map(has_no_opt_default).unwrap_or(false)
    };
    let mut commands = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        i += 1;
        if s == "--" {
            break;
        }
        let long_with_value = s.starts_with("--") && !s.contains('=') && !no_opt(&s[2..]);
        let short_with_value = s.starts_with('-') && !s.contains('=') && s.len() == 2 && !short_no_opt(&s[1..]);
        if long_with_value || short_with_value {
            if args.len() - i <= 1 {
                break;
            }
            i += 1;
            continue;
        }
        if !s.is_empty() && !s.starts_with('-') {
            commands.push(s.clone());
        }
    }
    commands
}

fn remove_first(args: &[String], item: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut removed = false;
    for a in args {
        if !removed && a == item {
            removed = true;
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// Go's runtime would fail a fork; the guest std references fork for Command.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn fork() -> i32 {
    unsafe { *libc::__errno_location() = libc::ENOSYS };
    -1
}
