//! plutil, written again in Rust for the guest. The reference is Apple's own: the Swift plutil
//! and the CoreFoundation property list code of swift-corelibs-foundation 6.4.2 (Apache-2.0,
//! LICENSE-swift-corelibs-foundation) — the command line, its messages and exit statuses
//! (PLUContext*.swift), the formats as CF reads and writes them (CFPropertyList.c,
//! CFBinaryPList.c, CFOldStylePList.c), JSON as JSONSerialization does it, and the Swift and
//! Objective-C literals (PLULiteralOutput.swift). Where it differs, on purpose:
//!
//!   - `-help` has no "Running in Swift mode" line (it reads as macOS's plutil does);
//!   - `-p` shows data bytes as NSData does (two hex digits each, in groups of four), where the
//!     Swift code drops leading zeros and skips every other byte of long data;
//!   - keyed archiver UIDs (CF$UID) convert, as CF writes them, where the Swift code refuses them;
//!   - JSON of a single value (`-extract key json`) is written as the fragment it is, where
//!     Foundation would stop the program; JSON numbers of more than 17 digits are Doubles;
//!   - a dictionary keeps the order its file had (CF's is its hash table's): it shows in binary
//!     and compact JSON output only, the other formats sort their keys;
//!   - an XML plist's declared encoding is UTF-8, UTF-16, UTF-32, US-ASCII or ISO-8859-1.
//!
//! A library: `main` is the `plutil` applet of collabo-data (../data).

mod binary;
mod date;
mod json;
mod literal;
mod num;
mod openstep;
mod path;
mod value;
mod xml;

use std::io::{Read, Write};
use value::{natural_compare, type_name, Dict, Value};

const HELP: &str = ": [command_option] [other_options] file...
The file '-' means stdin
Command options are (-lint is the default):
 -help                         show this message and exit
 -lint                         check the property list files for syntax errors
 -convert fmt                  rewrite property list files in format
                               fmt is one of: xml1 binary1 json swift objc
                               note: objc can additionally create a header by adding -header
 -insert keypath -type value   insert a value into the property list before writing it out
                               keypath is a key-value coding key path, with one extension:
                               a numerical path component applied to an array will act on the object at that index in the array
                               or insert it into the array if the numerical path component is the last one in the key path
                               type is one of: bool, integer, float, date, string, data, xml, json
                               -bool: YES if passed \"YES\" or \"true\", otherwise NO
                               -integer: any valid 64 bit integer
                               -float: any valid 64 bit float
                               -string: UTF8 encoded string
                               -date: a date in XML property list format, not supported if outputting JSON
                               -data: a base-64 encoded string
                               -xml: an XML property list, useful for inserting compound values
                               -json: a JSON fragment, useful for inserting compound values
                               -dictionary: inserts an empty dictionary, does not use value
                               -array: inserts an empty array, does not use value

                               optionally, -append may be specified if the keypath references an array to append to the
                               end of the array
                               value YES, NO, a number, a date, or a base-64 encoded blob of data
 -replace keypath -type value  same as -insert, but it will overwrite an existing value
 -remove keypath               removes the value at 'keypath' from the property list before writing it out
 -extract keypath fmt          outputs the value at 'keypath' in the property list as a new plist of type 'fmt'
                               fmt is one of: xml1 binary1 json raw
                               an additional \"-expect type\" option can be provided to test that
                               the value at the specified keypath is of the specified \"type\", which
                               can be one of: bool, integer, float, string, date, data, dictionary, array

                               when fmt is raw:
                                   the following is printed to stdout for each value type:
                                       bool: the string \"true\" or \"false\"
                                       integer: the numeric value
                                       float: the numeric value
                                       string: as UTF8-encoded string
                                       date: as RFC3339-encoded string in UTC timezone
                                       data: as base64-encoded string
                                       dictionary: each key on a new line
                                       array: the count of items in the array
                                   by default, the output is to stdout unless -o is specified
 -type keypath                 outputs the type of the value at 'keypath' in the property list
                               can be one of: bool, integer, float, string, date, data, dictionary, array
 -create fmt                   creates an empty plist of the specified format
                               file may be '-' for stdout
 -p                            print property list in a human-readable fashion
                               (not for machine parsing! this 'format' is not stable)
There are some additional optional arguments that apply to the -convert, -insert, -remove, -replace, and -extract verbs:
 -s                            be silent on success
 -o path                       specify alternate file path name for result;
                               the -o option is used with -convert, and is only
                               useful with one file argument (last file overwrites);
                               the path '-' means stdout
 -e extension                  specify alternate extension for converted files
 -r                            if writing JSON, output in human-readable form
 -n                            prevent printing a terminating newline if it is not part of the format, such as with raw
 --                            specifies that all further arguments are file names

";

/// What a command fails with. `Ns` is a Foundation error, shown as "(description)"; the others
/// are plutil's own (PLUContextError).
enum Failure {
    Argument(String),
    Object(String),
    Plist { plist: String, json: String },
    Ns(String),
}

impl Failure {
    fn text(&self) -> String {
        match self {
            Failure::Argument(s) | Failure::Object(s) => s.clone(),
            Failure::Plist { plist, json } => format!("Property List error: {plist} / JSON error: {json}"),
            Failure::Ns(s) => format!("({s})"),
        }
    }
}

type Res<T> = Result<T, Failure>;

fn argument<T>(s: impl Into<String>) -> Res<T> {
    Err(Failure::Argument(s.into()))
}

/// The output formats (PlutilEmissionFormat); None is "NoConversion".
#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    OpenStep,
    Xml,
    Binary,
    Json,
    Swift,
    Objc,
    Raw,
    Type,
}

fn format(arg: &str) -> Res<Option<Format>> {
    Ok(Some(match arg {
        "NoConversion" => return Ok(None),
        "openStep" => Format::OpenStep,
        "xml1" => Format::Xml,
        "binary1" => Format::Binary,
        "json" => Format::Json,
        "swift" => Format::Swift,
        "objc" => Format::Objc,
        "raw" => Format::Raw,
        "type" => Format::Type,
        _ => return argument(format!("Unknown format specifier: {arg}")),
    }))
}

const EXPECT_TYPES: [&str; 9] = ["(any)", "bool", "integer", "float", "string", "array", "dictionary", "date", "data"];

/// PLUContextArguments: the options every command but -help takes, and the files.
struct Args {
    paths: Vec<String>,
    readable: bool,
    newline: bool,
    output: Option<String>,
    extension: Option<String>,
    silent: Option<bool>,
}

fn context_args(arguments: &[String]) -> Res<Args> {
    let mut a = Args { paths: vec![], readable: false, newline: true, output: None, extension: None, silent: None };
    let mut it = arguments.iter();
    let mut rest = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                rest = true;
                break;
            }
            "-n" => a.newline = false,
            "-s" => a.silent = Some(true),
            "-r" => a.readable = true,
            "-o" => match it.next() {
                Some(next) => a.output = Some(next.clone()),
                None => return argument("Missing argument for -o."),
            },
            "-e" => match it.next() {
                Some(next) => a.extension = Some(next.clone()),
                None => return argument("Missing argument for -e."),
            },
            _ => {
                if arg.starts_with('-') && arg.chars().count() > 1 {
                    return argument(format!("unrecognized option: {arg}"));
                }
                a.paths.push(arg.clone());
            }
        }
    }
    if rest {
        a.paths.extend(it.cloned());
    }
    if a.paths.is_empty() {
        return argument("No files specified.");
    }
    Ok(a)
}

struct Io {
    out: std::io::Stdout,
    err: std::io::Stderr,
}

impl Io {
    fn out(&mut self, b: &[u8]) {
        let _ = self.out.write_all(b);
    }

    fn err_about(&mut self, path: &str, f: &Failure) {
        let shown = if path == "-" { "<stdin>" } else { path };
        let _ = self.out.flush();
        let _ = writeln!(self.err, "{shown}: {}", f.text());
    }
}

// ---- reading ---------------------------------------------------------------------------------

/// Foundation's localizedDescription of a file read error.
fn read_error(path: &str, e: &std::io::Error) -> Failure {
    let name = path::last_component(path);
    Failure::Ns(match e.kind() {
        std::io::ErrorKind::NotFound => format!("The file “{name}” couldn’t be opened because there is no such file."),
        std::io::ErrorKind::PermissionDenied => format!("The file “{name}” couldn’t be opened because you don’t have permission to view it."),
        _ => format!("The file “{name}” couldn’t be opened."),
    })
}

/// Foundation's localizedDescription of a file write error.
fn write_error(path: &str, e: &std::io::Error) -> Failure {
    let name = path::last_component(path);
    let folder = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).filter(|p| !p.is_empty()).unwrap_or_else(|| ".".into());
    let folder = path::last_component(&std::fs::canonicalize(&folder).map(|p| p.to_string_lossy().into_owned()).unwrap_or(folder));
    Failure::Ns(match e.raw_os_error() {
        Some(2) => format!("The folder “{folder}” doesn’t exist."),
        Some(13) | Some(1) => format!("You don’t have permission to save the file “{name}” in the folder “{folder}”."),
        Some(30) => format!("The file “{name}” couldn’t be saved because the volume is read only."),
        Some(28) => format!("The file “{name}” couldn’t be saved because there isn’t enough space."),
        _ => format!("The file “{name}” couldn’t be saved."),
    })
}

/// readPath: the file, or stdin for "-".
fn read_path(path: &str) -> Res<Vec<u8>> {
    if path == "-" {
        let mut data = Vec::new();
        return match std::io::stdin().read_to_end(&mut data) {
            Ok(_) => Ok(data),
            Err(_) => Err(Failure::Ns("Unable to read file from standard input".into())),
        };
    }
    if path.is_empty() {
        return argument("Empty path specified");
    }
    std::fs::read(path).map_err(|e| read_error(path, &e))
}

/// encodingForXMLData: the encoding (as a decoder to UTF-8) and the BOM's length.
enum Encoding {
    Utf8,
    Utf16(bool),
    Utf32(bool),
    Ascii,
    Latin1,
}

fn xml_encoding(b: &[u8]) -> Result<(Encoding, usize), String> {
    let len = b.len();
    if len > 4 {
        if b.starts_with(&[0, 0, 0xfe, 0xff]) {
            return Ok((Encoding::Utf32(true), 4));
        }
        if b.starts_with(&[0xff, 0xfe, 0, 0]) {
            return Ok((Encoding::Utf32(false), 4));
        }
    }
    if len > 3 && b.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Ok((Encoding::Utf8, 3));
    }
    if len > 2 {
        if b.starts_with(&[0xff, 0xfe]) {
            return Ok((Encoding::Utf16(false), 2));
        }
        if b.starts_with(&[0xfe, 0xff]) {
            return Ok((Encoding::Utf16(true), 2));
        }
        if b[0] == 0 || b[1] == 0 {
            // No BOM but a zero byte: CF's kCFStringEncodingUnicode, the first unit skipped.
            return Ok((Encoding::Utf16(true), 2));
        }
    }
    if len < 5 || &b[..5] != b"<?xml" {
        return Ok((Encoding::Utf8, 0));
    }
    let mut idx = 5;
    loop {
        if idx >= len {
            return Ok((Encoding::Utf8, 0));
        }
        let c = b[idx];
        if c == b'?' || c == b'>' {
            return Ok((Encoding::Utf8, 0));
        }
        idx += 1;
        if idx + 8 >= len {
            return Err("End of buffer while looking for encoding name".into());
        }
        if c == b'e' && &b[idx..idx + 8] == b"ncoding=" {
            idx += 8;
            break;
        }
    }
    let quote = b[idx];
    if quote != b'\'' && quote != b'"' {
        return Ok((Encoding::Utf8, 0));
    }
    let base = idx + 1;
    let Some(n) = b[base..].iter().position(|&c| c == quote) else { return Ok((Encoding::Utf8, 0)) };
    let name: String = b[base..base + n].iter().map(|&c| c as char).collect();
    Ok((
        match name.to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Encoding::Utf8,
            "utf-16" | "utf-16be" | "unicode-1-1" => Encoding::Utf16(true),
            "utf-16le" => Encoding::Utf16(false),
            "utf-32" | "utf-32be" => Encoding::Utf32(true),
            "utf-32le" => Encoding::Utf32(false),
            "us-ascii" | "ascii" | "ansi_x3.4-1968" => Encoding::Ascii,
            "iso-8859-1" | "iso_8859-1" | "latin1" | "l1" | "iso-latin-1" => Encoding::Latin1,
            _ => return Err(format!("Encountered unknown encoding ({name})")),
        },
        0,
    ))
}

fn decode(b: &[u8], e: &Encoding) -> Option<String> {
    match *e {
        Encoding::Utf8 => String::from_utf8(b.to_vec()).ok(),
        Encoding::Ascii => b.is_ascii().then(|| String::from_utf8(b.to_vec()).unwrap()),
        Encoding::Latin1 => Some(b.iter().map(|&c| c as char).collect()),
        Encoding::Utf16(be) => {
            let units: Vec<u16> = b.chunks(2).filter(|c| c.len() == 2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
            String::from_utf16(&units).ok()
        }
        Encoding::Utf32(be) => b
            .chunks(4)
            .map(|c| {
                let a: [u8; 4] = c.try_into().ok()?;
                char::from_u32(if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
            })
            .collect(),
    }
}

/// CFPropertyListCreateWithData with every format allowed: binary, then XML, then old-style.
/// The format it was, or CF's debug description of why it is none.
fn read_plist(data: &[u8]) -> Result<(Value, Format), String> {
    if data.is_empty() {
        return Err("Cannot parse a NULL or zero-length data".into());
    }
    if let Some(v) = binary::read(data) {
        return Ok((v, Format::Binary));
    }
    let (encoding, skip) = xml_encoding(data)?;
    let (xml_bytes, old_style): (std::borrow::Cow<[u8]>, Option<String>) = match encoding {
        Encoding::Utf8 => (std::borrow::Cow::Borrowed(&data[skip..]), String::from_utf8(data.to_vec()).ok()),
        ref other => {
            let Some(s) = decode(&data[skip..], other) else {
                return Err("Could not determine the encoding of the XML data (string creation failed)".into());
            };
            if s.is_empty() {
                return Err("Conversion of string failed. The string is empty.".into());
            }
            (std::borrow::Cow::Owned(s.clone().into_bytes()), Some(s))
        }
    };
    let mut parser = xml::Parser::new(&xml_bytes);
    if let Some(v) = parser.parse() {
        return Ok((v, Format::Xml));
    }
    let xml_error = parser.error.unwrap_or_else(|| "Encountered unknown error during parse".into());
    if let Some(text) = old_style {
        if let Ok(v) = openstep::parse(&text) {
            return Ok((v, Format::OpenStep));
        }
    }
    Err(xml_error)
}

/// readPropertyList: a property list, else JSON.
fn read_property_list(data: &[u8]) -> Res<(Value, Format)> {
    match read_plist(data) {
        Ok(r) => Ok(r),
        Err(plist) => match json::parse(data, false) {
            Ok(v) => Ok((v, Format::Json)),
            Err(json::Error(json)) => Err(Failure::Plist { plist, json }),
        },
    }
}

/// An NSError's description, as `"\(error)"` puts it into a message.
fn ns_error_description(debug: &str) -> String {
    format!("Error Domain=NSCocoaErrorDomain Code=3840 \"The data couldn’t be read because it isn’t in the correct format.\" UserInfo={{NSDebugDescription={debug}}}")
}

// ---- writing ---------------------------------------------------------------------------------

/// fixPath: -o as it is; else -e's extension; then .m or .swift for the literal formats.
fn fix_path(path: &str, format: Format, output: Option<&str>, extension: Option<&str>) -> String {
    if let Some(o) = output {
        return o.to_string();
    }
    let mut result = path.to_string();
    if let Some(e) = extension {
        result = path::replace_extension(&result, e);
    }
    match format {
        Format::Objc => path::replace_extension(&result, "m"),
        Format::Swift => path::replace_extension(&result, "swift"),
        _ => result,
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// Data(base64Encoded:options: .ignoreUnknownCharacters).
fn unbase64(s: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let chars: Vec<u8> = s.bytes().filter(|&c| digit(c).is_some() || c == b'=').collect();
    if chars.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(chars.len() / 4 * 3);
    for (i, q) in chars.chunks(4).enumerate() {
        let last = i == chars.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) || q[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | digit(c)?;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// rawStringWithPropertyList.
fn raw_string(v: &Value) -> String {
    match v {
        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => literal::number(v, false),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.len().to_string(),
        Value::Dict(d) => value::sorted_keys(d, natural_compare).iter().map(|k| k.as_str()).collect::<Vec<_>>().join("\n"),
        Value::Date(at) => date::iso8601(*at),
        Value::Data(d) => base64(d),
        _ => String::new(),
    }
}

struct Output<'a> {
    path: &'a str,
    format: Format,
    output: Option<&'a str>,
    extension: Option<&'a str>,
    key_path: Option<&'a str>,
    readable: bool,
    newline: bool,
    header: bool,
}

/// writePropertyList.
fn write_property_list(io: &mut Io, v: &Value, w: &Output) -> Res<()> {
    if !v.is_plist() {
        return Err(Failure::Object("Dictionaries are required to have string keys in property lists".into()));
    }
    let new_path = fix_path(w.path, w.format, w.output, w.extension);
    let data = match w.format {
        Format::Xml | Format::Binary => {
            if !xml::is_valid(v, w.format == Format::Xml) {
                return Err(Failure::Object("Invalid object in plist for property list format".into()));
            }
            if w.format == Format::Xml {
                xml::write(v)
            } else {
                binary::write(v)
            }
        }
        Format::Swift => {
            if !xml::is_valid(v, false) {
                return Err(Failure::Object("Input contains an object that cannot be represented in Swift literal syntax".into()));
            }
            literal::swift(v, w.path).map_err(Failure::Object)?
        }
        Format::Objc => {
            if !literal::objc_valid(v) {
                return Err(Failure::Object("Input contains an object that cannot be represented in Obj-C literal syntax".into()));
            }
            literal::objc(v, w.path, &new_path).map_err(Failure::Object)?
        }
        Format::Json => {
            if !json::is_valid(v) {
                return Err(Failure::Object("Invalid object in plist for JSON format".into()));
            }
            json::write(v, w.readable)
        }
        Format::Raw => {
            if matches!(v, Value::Uid(_)) {
                return Err(Failure::Object(format!(
                    "Value at {} is a {} type and cannot be extracted in raw format",
                    w.key_path.unwrap_or("unknown key path"),
                    type_name(v)
                )));
            }
            raw_string(v).into_bytes()
        }
        Format::Type => type_name(v).as_bytes().to_vec(),
        Format::OpenStep => return Err(Failure::Object("Conversion to OpenStep format is not supported".into())),
    };
    let text = matches!(w.format, Format::Raw | Format::Type);
    if w.output == Some("-") || (w.output.is_none() && text) {
        io.out(&data);
        if w.newline && text {
            io.out(b"\n");
        }
        return Ok(());
    }
    std::fs::write(&new_path, &data).map_err(|e| write_error(&new_path, &e))?;
    if w.format == Format::Objc && w.header {
        let header = literal::objc_header(v, w.path, &new_path).map_err(Failure::Object)?;
        let header_path = path::replace_extension(&new_path, "h");
        std::fs::write(&header_path, header).map_err(|e| write_error(&header_path, &e))?;
    }
    Ok(())
}

// ---- key paths ---------------------------------------------------------------------------------

/// escapedKeyPathSplit: '.' separates, "\." is a dot in a key.
fn split_key_path(k: &str) -> Vec<String> {
    k.replace("\\.", "A_DOT_WAS_HERE").split('.').map(|s| s.replace("A_DOT_WAS_HERE", ".")).collect()
}

fn join_key_path(parts: &[String]) -> String {
    parts.iter().map(|p| p.replace('.', "\\.")).collect::<Vec<_>>().join(".")
}

/// Swift's Int(String): an optional sign and decimal digits, within Int's range.
fn swift_int(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

/// An index into `len` elements, if `key` is one.
fn index_in(key: &str, len: usize) -> Option<usize> {
    swift_int(key).filter(|&i| i >= 0 && (i as usize) < len).map(|i| i as usize)
}

fn value_at<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    let Some((key, rest)) = path.split_first() else { return Some(v) };
    if key.is_empty() {
        return None;
    }
    match v {
        Value::Dict(d) => value_at(d.get(key.as_str())?, rest),
        Value::Array(a) => value_at(&a[index_in(key, a.len())?], rest),
        _ => None,
    }
}

/// _removeValue: None when the whole value goes.
fn remove_at(v: &Value, path: &[String], all: &[String]) -> Res<Option<Value>> {
    let Some((key, rest)) = path.split_first() else { return Ok(None) };
    let missing = || Failure::Argument(format!("No value to remove at key path {}", join_key_path(all)));
    if key.is_empty() {
        return Err(missing());
    }
    match v {
        Value::Dict(d) => {
            let existing = d.get(key.as_str()).ok_or_else(missing)?;
            let mut new = (**d).clone();
            match remove_at(existing, rest, all)? {
                Some(changed) => {
                    new.insert(key.clone(), changed);
                }
                None => {
                    new.shift_remove(key.as_str());
                }
            }
            Ok(Some(Value::dict(new)))
        }
        Value::Array(a) => {
            let i = index_in(key, a.len()).ok_or_else(missing)?;
            let mut new = (**a).clone();
            match remove_at(&a[i], rest, all)? {
                Some(changed) => new[i] = changed,
                None => {
                    new.remove(i);
                }
            }
            Ok(Some(Value::array(new)))
        }
        _ => Err(missing()),
    }
}

/// _insertValue.
fn insert_at(value: &Value, path: &[String], all: &[String], v: &Value, replacing: bool, appending: bool) -> Res<Value> {
    let joined = || join_key_path(all);
    let Some(key) = path.first() else { return argument(format!("Key path not found {}", joined())) };
    match v {
        Value::Dict(d) => {
            let existing = d.get(key.as_str());
            if path.len() > 1 {
                let Some(existing) = existing else { return argument(format!("Key path not found {}", joined())) };
                let mut new = (**d).clone();
                new.insert(key.clone(), insert_at(value, &path[1..], all, existing, replacing, appending)?);
                return Ok(Value::dict(new));
            }
            let mut new = (**d).clone();
            if !replacing {
                if let Some(existing) = existing {
                    if !appending {
                        return argument(format!("Value already exists at key path {}", joined()));
                    }
                    let Value::Array(a) = existing else {
                        return argument(format!("Appending to a non-array at key path {}", joined()));
                    };
                    let mut a = (**a).clone();
                    a.push(value.clone());
                    new.insert(key.clone(), Value::array(a));
                    return Ok(Value::dict(new));
                }
            }
            new.insert(key.clone(), value.clone());
            Ok(Value::dict(new))
        }
        Value::Array(a) => {
            let Some(n) = swift_int(key) else {
                return argument(format!("Unable to index into array with key path {}", joined()));
            };
            let contains = n >= 0 && (n as usize) < a.len();
            let i = if n < 0 { usize::MAX } else { n as usize };
            if path.len() > 1 {
                if !contains {
                    return argument(format!("Index {n} out of bounds in array at key path {}", joined()));
                }
                let mut new = (**a).clone();
                new[i] = insert_at(value, &path[1..], all, &a[i], replacing, appending)?;
                return Ok(Value::array(new));
            }
            let mut new = (**a).clone();
            if appending {
                // Swift traps on an index out of range here; a refusal is kinder.
                let Some(Value::Array(inner)) = a.get(i) else {
                    return argument(format!("Attempt to append value to non-array at key path {}", joined()));
                };
                let mut inner = (**inner).clone();
                inner.push(value.clone());
                new[i] = Value::array(inner);
            } else if contains {
                new.insert(i, value.clone());
            } else if i == a.len() {
                new.push(value.clone());
            } else {
                return argument(format!("Index {n} out of bounds in array at key path {}", joined()));
            }
            Ok(Value::array(new))
        }
        _ => argument(format!("Unable to insert value at key path {}", joined())),
    }
}

// ---- -p ------------------------------------------------------------------------------------------

/// NSData's description: `{length = 3, bytes = 0x010203}`; above 24 bytes, the first 16 and the
/// last 8, in groups of four.
fn data_description(d: &[u8]) -> String {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    if d.len() <= 24 {
        return format!("{{length = {}, bytes = 0x{}}}", d.len(), hex(d));
    }
    let head: Vec<String> = d[..16].chunks(4).map(hex).collect();
    let tail: String = d[d.len() - 8..].chunks(4).map(|c| format!("{} ", hex(c))).collect();
    format!("{{length = {}, bytes = 0x{} ... {}}}", d.len(), head.join(" "), tail)
}

/// PrintCommand.prettyPrint.
fn pretty(v: &Value, indent: usize, spacing: usize, out: &mut String) {
    match v {
        Value::Dict(d) => {
            out.push_str("{\n");
            for k in value::sorted_keys(d, natural_compare) {
                out.push_str(&" ".repeat(indent));
                out.push_str(&format!("\"{k}\" => "));
                pretty(&d[k.as_str()], indent + spacing, spacing, out);
            }
            out.push_str(&" ".repeat(indent - spacing));
            out.push_str("}\n");
        }
        Value::Array(a) => {
            out.push_str("[\n");
            for (i, item) in a.iter().enumerate() {
                out.push_str(&" ".repeat(indent));
                out.push_str(&format!("{i} => "));
                pretty(item, indent + spacing, spacing, out);
            }
            out.push_str(&" ".repeat(indent - spacing));
            out.push_str("]\n");
        }
        Value::String(s) => out.push_str(&format!("\"{s}\"\n")),
        Value::Data(d) => out.push_str(&format!("{}\n", data_description(d))),
        Value::Date(at) => out.push_str(&format!("{}\n", date::description(*at))),
        Value::Bool(b) => out.push_str(if *b { "true\n" } else { "false\n" }),
        Value::Int(i) => out.push_str(&format!("{i}\n")),
        Value::UInt(u) => out.push_str(&format!("{u}\n")),
        Value::Real(r) => out.push_str(&format!("{}\n", num::swift_double(*r))),
        Value::Float(f) => out.push_str(&format!("{}\n", num::swift_float(*f))),
        Value::Uid(u) => out.push_str(&format!("<CFKeyedArchiverUID>{{value = {u}}}\n")),
        Value::Null => out.push_str("<null>\n"),
        Value::Set(items) => {
            let mut s = String::new();
            for item in items.iter() {
                pretty(item, indent + spacing, spacing, &mut s);
            }
            out.push_str(&format!("{{(\n{s})}}\n"));
        }
        Value::KeyedDict(entries) => out.push_str(&format!("<dictionary of {} entries with keys that are not strings>\n", entries.len())),
    }
}

// ---- the commands --------------------------------------------------------------------------------

fn lint(io: &mut Io, arguments: &[String]) -> Res<bool> {
    let a = context_args(arguments)?;
    if a.output.is_some() {
        return argument("-o is not used with -lint.");
    }
    if a.extension.is_some() {
        return argument("-e is not used with -lint.");
    }
    let mut failed = false;
    for path in &a.paths {
        let result = read_path(path).and_then(|data| read_plist(&data).map_err(Failure::Ns));
        match result {
            Ok(_) => {
                if !a.silent.unwrap_or(false) {
                    let shown = if path == "-" { "<stdin>" } else { path };
                    io.out(format!("{shown}: OK\n").as_bytes());
                }
            }
            Err(f) => {
                failed = true;
                io.err_about(path, &f);
            }
        }
    }
    Ok(!failed)
}

/// What each file goes through: read, changed, written; the first failure is reported for it.
fn each_file(io: &mut Io, a: &Args, mut f: impl FnMut(&mut Io, &str, Value, Format) -> Res<()>) -> bool {
    let mut failed = false;
    for path in &a.paths {
        let result = read_path(path).and_then(|data| read_property_list(&data)).and_then(|(v, fmt)| f(io, path, v, fmt));
        if let Err(e) = result {
            failed = true;
            io.err_about(path, &e);
        }
    }
    !failed
}

fn convert(io: &mut Io, arguments: &[String], specified: Option<Format>, header: bool) -> Res<bool> {
    let a = context_args(arguments)?;
    Ok(each_file(io, &a, |io, path, v, existing| {
        let w = Output {
            path,
            format: specified.unwrap_or(existing),
            output: a.output.as_deref(),
            extension: a.extension.as_deref(),
            key_path: None,
            readable: a.readable,
            newline: a.newline,
            header,
        };
        write_property_list(io, &v, &w)
    }))
}

fn insert(io: &mut Io, arguments: &[String], replace: bool) -> Res<bool> {
    let mut rest: std::collections::VecDeque<String> = arguments.iter().cloned().collect();
    let missing = "'Insert' and 'Replace' require a key path, a type, and a value.";
    let missing_value = "'Insert' and 'Replace' require a key path, a type, and a value2.";
    let Some(key_path) = rest.pop_front() else { return argument(missing) };
    let Some(kind) = rest.pop_front() else { return argument(missing) };
    let mut next = || rest.pop_front().ok_or_else(|| Failure::Argument(missing_value.into()));
    let value = match kind.as_str() {
        "-bool" => {
            let b = next()?.to_lowercase();
            Value::Bool(b == "true" || b == "yes")
        }
        "-integer" => Value::Int(num::ns_integer_value(&next()?)),
        "-date" => {
            let text = next()?;
            let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>value</key><date>{text}</date></dict></plist>");
            match read_plist(xml.as_bytes()) {
                Ok((Value::Dict(d), _)) if d.contains_key("value") => d["value"].clone(),
                // Swift's plutil traps on a date it cannot read.
                _ => return argument(format!("Unable to parse date \"{text}\"")),
            }
        }
        "-data" => match unbase64(&next()?) {
            Some(d) => Value::data(d),
            None => return argument("Invalid base64 data in argument"),
        },
        "-float" => Value::Real(num::ns_double_value(&next()?)),
        "-xml" => match read_plist(next()?.as_bytes()) {
            Ok((v, _)) => v,
            Err(e) => return argument(format!("Unable to parse xml property list: {}", ns_error_description(&e))),
        },
        "-json" => match json::parse(next()?.as_bytes(), true) {
            Ok(v) => v,
            Err(json::Error(e)) => return argument(format!("Unable to parse JSON: {}", ns_error_description(&e))),
        },
        "-string" => Value::String(next()?),
        "-dictionary" => Value::dict(Dict::new()),
        "-array" => Value::array(Vec::new()),
        _ => return argument(format!("Unknown insert or replace type {kind}")),
    };
    let mut append = false;
    if rest.front().map(String::as_str) == Some("-append") {
        rest.pop_front();
        append = !replace;
    }
    let rest: Vec<String> = rest.into_iter().collect();
    let a = context_args(&rest)?;
    let parts = split_key_path(&key_path);
    Ok(each_file(io, &a, |io, path, v, format| {
        let changed = insert_at(&value, &parts, &parts, &v, replace, append)?;
        let w = Output {
            path,
            format,
            output: a.output.as_deref(),
            extension: a.extension.as_deref(),
            key_path: Some(&key_path),
            readable: a.readable,
            newline: a.newline,
            header: false,
        };
        write_property_list(io, &changed, &w)
    }))
}

fn remove(io: &mut Io, arguments: &[String], key_path: &str) -> Res<bool> {
    let a = context_args(arguments)?;
    let parts = split_key_path(key_path);
    Ok(each_file(io, &a, |io, path, v, format| {
        let Some(changed) = remove_at(&v, &parts, &parts)? else {
            return argument(format!("Removing value resulted in an empty property list at {key_path}"));
        };
        let w = Output {
            path,
            format,
            output: a.output.as_deref(),
            extension: a.extension.as_deref(),
            key_path: Some(key_path),
            readable: a.readable,
            newline: a.newline,
            header: false,
        };
        write_property_list(io, &changed, &w)
    }))
}

fn extract(io: &mut Io, arguments: &[String], key_path: &str, specified: Option<Format>, expect: Option<&str>) -> Res<bool> {
    let a = context_args(arguments)?;
    let parts = split_key_path(key_path);
    Ok(each_file(io, &a, |io, path, v, existing| {
        let Some(found) = value_at(&v, &parts) else {
            return argument(format!("Could not extract value, error: No value at that key path or invalid key path: {key_path}"));
        };
        if let Some(expect) = expect {
            let actual = type_name(found);
            if actual != expect {
                return Err(Failure::Object(format!("Value at [{key_path}] expected to be {expect} but is {actual}")));
            }
        }
        let w = Output {
            path,
            format: specified.unwrap_or(existing),
            output: a.output.as_deref(),
            extension: a.extension.as_deref(),
            key_path: Some(key_path),
            readable: a.readable,
            newline: a.newline,
            header: false,
        };
        write_property_list(io, found, &w)
    }))
}

fn print(io: &mut Io, arguments: &[String]) -> Res<bool> {
    let a = context_args(arguments)?;
    if a.output.is_some() {
        return argument("-o is not used with -p.");
    }
    if a.extension.is_some() {
        return argument("-e is not used with -p.");
    }
    if a.silent.is_some() {
        let _ = io.err.write_all(b"-s doesn't make a lot of sense with -p.\n");
    }
    Ok(each_file(io, &a, |io, _, v, _| {
        let mut s = String::new();
        pretty(&v, 2, 2, &mut s);
        io.out(s.as_bytes());
        Ok(())
    }))
}

fn create(io: &mut Io, arguments: &[String], format: Format) -> Res<bool> {
    let a = context_args(arguments)?;
    let mut failed = false;
    for path in &a.paths {
        let data: Res<Vec<u8>> = match format {
            Format::Json => Ok(json::write(&Value::dict(Dict::new()), a.readable)),
            Format::Xml => Ok(xml::write(&Value::dict(Dict::new()))),
            Format::Binary => Ok(binary::write(&Value::dict(Dict::new()))),
            Format::Swift => Ok(b"/// Generated from -\nlet - = [\n    ]".to_vec()),
            Format::Objc => Ok(b"/// Generated from -\n__attribute__((visibility(\"hidden\")))\nNSDictionary * const _ = @{\n};".to_vec()),
            Format::Raw => Ok(b"\n".to_vec()),
            Format::Type => Ok(b"dictionary".to_vec()),
            Format::OpenStep => argument("Cannot create open step property lists"),
        };
        let result = data.and_then(|data| {
            if path == "-" {
                io.out(&data);
                if a.newline && matches!(format, Format::Raw | Format::Type) {
                    io.out(b"\n");
                }
                Ok(())
            } else {
                std::fs::write(path, &data).map_err(|e| write_error(path, &e))
            }
        });
        if let Err(e) = result {
            failed = true;
            io.err_about(path, &e);
        }
    }
    Ok(!failed)
}

fn expect_arg(arguments: &mut std::collections::VecDeque<String>) -> Res<Option<String>> {
    if arguments.front().map(String::as_str) != Some("-expect") {
        return Ok(None);
    }
    arguments.pop_front();
    let Some(t) = arguments.pop_front() else { return argument("-expect requires a type argument.") };
    if !EXPECT_TYPES.contains(&t.as_str()) {
        return argument(format!("-expect type [{t}] not valid."));
    }
    Ok(Some(t))
}

fn run(io: &mut Io, argv: Vec<String>) -> Res<bool> {
    let mut args: std::collections::VecDeque<String> = argv.into();
    let Some(program) = args.pop_front() else { return argument("No files specified.") };
    let Some(command) = args.pop_front() else { return argument("No files specified.") };
    let rest = |a: &std::collections::VecDeque<String>| a.iter().cloned().collect::<Vec<_>>();
    match command.as_str() {
        "-help" => {
            let name = path::last_component(&program);
            io.out(format!("{}{HELP}", if name.is_empty() { "plutil" } else { &name }).as_bytes());
            Ok(true)
        }
        "-lint" => lint(io, &rest(&args)),
        "-convert" => {
            let Some(f) = args.pop_front() else { return argument("Missing format specifier for command.") };
            let f = format(&f)?;
            let header = args.front().map(String::as_str) == Some("-header");
            if header {
                if f != Some(Format::Objc) {
                    return argument("-header is only valid for objc literal conversions.");
                }
                args.pop_front();
            }
            convert(io, &rest(&args), f, header)
        }
        "-p" => print(io, &rest(&args)),
        "-insert" => insert(io, &rest(&args), false),
        "-replace" => insert(io, &rest(&args), true),
        "-remove" => {
            let Some(k) = args.pop_front() else { return argument("'Remove' requires a key path.") };
            remove(io, &rest(&args), &k)
        }
        "-extract" => {
            let missing = "'Extract' requires a key path and a plist format.";
            let Some(k) = args.pop_front() else { return argument(missing) };
            let Some(f) = args.pop_front() else { return argument(missing) };
            let f = format(&f)?;
            let expect = expect_arg(&mut args)?;
            extract(io, &rest(&args), &k, f, expect.as_deref())
        }
        "-type" => {
            let Some(k) = args.pop_front() else { return argument("'Extract' requires a key path and a plist format.") };
            let expect = expect_arg(&mut args)?;
            extract(io, &rest(&args), &k, Some(Format::Type), expect.as_deref())
        }
        "-create" => {
            let Some(f) = args.pop_front() else { return argument("'Create' requires a plist format.") };
            let f = format(&f)?.unwrap_or(Format::Xml);
            create(io, &rest(&args), f)
        }
        _ => {
            // -lint, with what looked like the command as the first file.
            let mut all = vec![command];
            all.extend(args);
            lint(io, &all)
        }
    }
}

/// plutil's command line: `argv` with the program's name first. Returns the exit status.
pub fn main(argv: Vec<String>) -> i32 {
    let mut io = Io { out: std::io::stdout(), err: std::io::stderr() };
    let result = run(&mut io, argv);
    let _ = io.out.flush();
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(f) => {
            let _ = writeln!(io.err, "{}", f.text());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_paths_and_base64() {
        assert_eq!(split_key_path("a.b\\.c.0"), ["a", "b.c", "0"]);
        assert_eq!(join_key_path(&["a".into(), "b.c".into()]), "a.b\\.c");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(unbase64("aGVs bG8=\n").unwrap(), b"hello");
        assert!(unbase64("aGVsbG8").is_none());
        assert_eq!(data_description(&[1, 2, 255]), "{length = 3, bytes = 0x0102ff}");
        let long: Vec<u8> = (0..32).collect();
        assert_eq!(data_description(&long), "{length = 32, bytes = 0x00010203 04050607 08090a0b 0c0d0e0f ... 18191a1b 1c1d1e1f }");
    }

    #[test]
    fn formats_are_told_apart_as_cf_does() {
        assert!(matches!(read_plist(b"{a = (1, 2);}"), Ok((_, Format::OpenStep))));
        assert!(matches!(read_plist(b"<plist><true/></plist>"), Ok((Value::Bool(true), Format::Xml))));
        assert_eq!(read_plist(b"").unwrap_err(), "Cannot parse a NULL or zero-length data");
        assert_eq!(read_plist(b"[1,2]").unwrap_err(), "Unexpected character [ at line 1");
        assert!(matches!(read_property_list(b"[1,2]"), Ok((_, Format::Json))));
        match read_property_list(b"[1,") {
            Err(f) => assert_eq!(f.text(), "Property List error: Unexpected character [ at line 1 / JSON error: Unexpected end of file during JSON parse."),
            Ok(_) => panic!(),
        }
    }
}
