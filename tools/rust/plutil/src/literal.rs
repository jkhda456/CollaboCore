//! `-convert swift` and `-convert objc`: the property list as a Swift or Objective-C literal, as
//! plutil's PLULiteralOutput.swift writes it (keys sorted plutil's way, four-space indentation,
//! the variable named after the file).

use crate::num;
use crate::path;
use crate::value::{natural_compare, type_name, Value};

fn indentation(depth: usize) -> String {
    " ".repeat(depth * 4)
}

/// varName(from:): the file's stem, '-' and ' ' as '_', symbols and control characters left out.
fn var_name(file: &str) -> String {
    let stem = path::stem(file);
    stem.replace(['-', ' '], "_").chars().filter(|&c| !is_symbol(c) && !c.is_control()).collect()
}

/// CharacterSet.symbols (Unicode Sm, Sc, Sk, So), for the characters a file name has.
fn is_symbol(c: char) -> bool {
    matches!(c, '$' | '+' | '<' | '=' | '>' | '^' | '`' | '|' | '~')
        || matches!(c, '\u{a2}'..='\u{a9}' | '\u{ac}' | '\u{ae}'..='\u{b1}' | '\u{b4}' | '\u{b8}' | '\u{d7}' | '\u{f7}')
        && !matches!(c, '\u{a7}')
        || ('\u{2190}'..='\u{2bff}').contains(&c)
        || ('\u{20a0}'..='\u{20cf}').contains(&c)
        || ('\u{1f000}'..='\u{1faff}').contains(&c)
}

/// escapedForQuotesAndEscapes: a backslash before `"`, and before the escapes regular
/// expressions use (\b \s \w \. \| \* \) \().
fn escaped(s: &str) -> String {
    let mut result = s.to_string();
    for e in ["\\b", "\\s", "\"", "\\w", "\\.", "\\|", "\\*", "\\)", "\\("] {
        result = result.replace(e, &format!("\\{e}"));
    }
    result
}

/// NSNumber.propertyListFormatted(objCStyle:).
pub fn number(v: &Value, objc: bool) -> String {
    let s = match *v {
        Value::Bool(true) => (if objc { "YES" } else { "true" }).to_string(),
        Value::Bool(false) => (if objc { "NO" } else { "false" }).to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Real(r) => num::c_f(r),
        Value::Float(f) => num::c_f(f as f64),
        _ => unreachable!(),
    };
    if objc {
        format!("@{s}")
    } else {
        s
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Date(_) => "Date",
        Value::Data(_) => "Data",
        Value::Uid(_) => "CFKeyedArchiverUID",
        Value::Null => "NSNull",
        Value::Set(_) => "NSSet",
        _ => "NSDictionary",
    }
}

fn swift_value(v: &Value, depth: usize, indent: bool, file: &str) -> Result<String, String> {
    let mut out = String::new();
    if depth == 0 {
        out.push_str(&format!("/// Generated from {}\n", path::last_component(file)));
        out.push_str(&format!("let {}", var_name(file)));
        let mixed = |types: Vec<&'static str>| types.windows(2).any(|w| w[0] != w[1]);
        match v {
            Value::Dict(d) if mixed(d.values().map(type_name).collect()) => out.push_str(" : [String : Any]"),
            Value::Array(a) if mixed(a.iter().map(type_name).collect()) => out.push_str(" : [Any]"),
            _ => {}
        }
        out.push_str(" = ");
    }
    if indent {
        out.push_str(&indentation(depth));
    }
    match v {
        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => out.push_str(&number(v, false)),
        Value::String(s) => out.push_str(&format!("\"{}\"", escaped(s).replace('\n', "\\n"))),
        Value::Array(a) => {
            out.push_str("[\n");
            for item in a.iter() {
                out.push_str(&swift_value(item, depth + 1, true, file)?);
                out.push_str(",\n");
            }
            out.push_str(&indentation(depth));
            out.push(']');
        }
        Value::Dict(d) => {
            out.push_str("[\n");
            let mut keys: Vec<&String> = d.keys().collect();
            keys.sort_by(|a, b| natural_compare(a, b));
            for k in keys {
                out.push_str(&indentation(depth + 1));
                out.push_str(&format!("\"{k}\" : "));
                out.push_str(&swift_value(&d[k.as_str()], depth + 1, false, file)?);
                out.push_str(",\n");
            }
            out.push_str(&indentation(depth));
            out.push(']');
        }
        Value::Data(bytes) => {
            out.push_str("Data(bytes: [");
            for b in bytes.iter() {
                out.push_str(&format!("0x{b:02X},"));
            }
            out.push_str("])");
        }
        Value::Date(at) => out.push_str(&format!("Date(timeIntervalSinceReferenceDate: {})", num::swift_double(*at))),
        other => return Err(format!("Swift literal syntax does not support classes of type {}", kind(other))),
    }
    Ok(out)
}

/// The .swift file for `-convert swift`.
pub fn swift(v: &Value, original: &str) -> Result<Vec<u8>, String> {
    Ok(format!("{}\n", swift_value(v, 0, true, original)?).into_bytes())
}

/// propertyListIsValidForLiteralFormat(.objc): strings, numbers, arrays and dictionaries only.
pub fn objc_valid(v: &Value) -> bool {
    match v {
        Value::String(_) | Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => true,
        Value::Array(a) => a.iter().all(objc_valid),
        Value::Dict(d) => d.values().all(objc_valid),
        _ => false,
    }
}

fn objc_variable(v: &Value, header: bool, original: &str, output: &str) -> Result<String, String> {
    let class = match v {
        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => "NSNumber",
        Value::String(_) => "NSString",
        Value::Array(_) => "NSArray",
        Value::Dict(_) => "NSDictionary",
        other => return Err(format!("Objective-C literal syntax does not support classes of type {}", kind(other))),
    };
    let mut out = String::new();
    if header {
        out.push_str("#import <Foundation/Foundation.h>\n\n");
    } else if output != "-" {
        out.push_str(&format!("#import \"{}.h\"\n\n", path::stem(output)));
    }
    out.push_str(&format!("/// Generated from {}\n", path::last_component(original)));
    out.push_str("__attribute__((visibility(\"hidden\")))\n");
    if header {
        out.push_str("extern ");
    }
    out.push_str(&format!("{class} * const {}", var_name(original)));
    if !header {
        out.push_str(" = ");
    }
    Ok(out)
}

fn objc_value(v: &Value, depth: usize, indent: bool, original: &str, output: &str) -> Result<String, String> {
    let mut out = String::new();
    if depth == 0 {
        out.push_str(&objc_variable(v, false, original, output)?);
    }
    if indent {
        out.push_str(&indentation(depth));
    }
    match v {
        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Real(_) | Value::Float(_) => out.push_str(&number(v, true)),
        Value::String(s) => out.push_str(&format!("@\"{}\"", escaped(s))),
        Value::Array(a) => {
            out.push_str("@[\n");
            for item in a.iter() {
                out.push_str(&objc_value(item, depth + 1, true, original, output)?);
                out.push_str(",\n");
            }
            out.push_str(&indentation(depth));
            out.push(']');
        }
        Value::Dict(d) => {
            out.push_str("@{\n");
            let mut keys: Vec<&String> = d.keys().collect();
            keys.sort_by(|a, b| natural_compare(a, b));
            for k in keys {
                out.push_str(&indentation(depth + 1));
                out.push_str(&format!("@\"{k}\" : "));
                out.push_str(&objc_value(&d[k.as_str()], depth + 1, false, original, output)?);
                out.push_str(",\n");
            }
            out.push_str(&indentation(depth));
            out.push('}');
        }
        other => return Err(format!("Objective-C literal syntax does not support classes of type {}", kind(other))),
    }
    Ok(out)
}

/// The .m file for `-convert objc`.
pub fn objc(v: &Value, original: &str, output: &str) -> Result<Vec<u8>, String> {
    Ok(format!("{};\n", objc_value(v, 0, true, original, output)?).into_bytes())
}

/// The .h file `-convert objc -header` writes beside it.
pub fn objc_header(v: &Value, original: &str, output: &str) -> Result<Vec<u8>, String> {
    Ok(format!("{};\n", objc_variable(v, true, original, output)?).into_bytes())
}
