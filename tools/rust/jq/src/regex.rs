//! `_match_impl` (jq's f_match): Oniguruma regexes (Perl_NG syntax) run on fancy-regex. The
//! few places where the two differ are translated: `$` and `\Z` are "end, or before a final
//! newline" (Rust's `$` is the very end), `\h`/`\H` are hex digits. Offsets and lengths are in
//! codepoints, as jq reports them.

use crate::interp::{err, Flow};
use crate::value::{dump_trunc, Map, Value};
use fancy_regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

fn translate(re: &str) -> String {
    let mut out = String::new();
    let mut chars = re.chars().peekable();
    let mut class = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(n) = chars.next() else {
                    out.push('\\');
                    break;
                };
                match n {
                    'Z' if class == 0 => out.push_str(r"(?=\n?\z)"),
                    'h' => out.push_str(if class > 0 { "0-9a-fA-F" } else { "[0-9a-fA-F]" }),
                    'H' if class == 0 => out.push_str("[^0-9a-fA-F]"),
                    _ => {
                        out.push('\\');
                        out.push(n);
                    }
                }
            }
            '[' => {
                class += 1;
                out.push(c);
                // A ']' first in a class is literal; so is '^]'.
                if chars.peek() == Some(&'^') {
                    out.push(chars.next().unwrap());
                }
                if chars.peek() == Some(&']') {
                    chars.next();
                    out.push_str(r"\]");
                }
            }
            ']' if class > 0 => {
                class -= 1;
                out.push(c);
            }
            '$' if class == 0 => out.push_str(r"(?=\n?\z)"),
            _ => out.push(c),
        }
    }
    out
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<Regex>>> = RefCell::new(HashMap::new());
}

fn compile(pattern: &str) -> Result<Rc<Regex>, Flow> {
    if let Some(r) = CACHE.with(|c| c.borrow().get(pattern).cloned()) {
        return Ok(r);
    }
    match Regex::new(&translate(pattern)) {
        Ok(r) => {
            let r = Rc::new(r);
            CACHE.with(|c| {
                let mut c = c.borrow_mut();
                if c.len() > 256 {
                    c.clear();
                }
                c.insert(pattern.to_string(), r.clone());
            });
            Ok(r)
        }
        Err(e) => Err(err(format!("Regex failure: {}", onig_message(&e.to_string())))),
    }
}

/// Oniguruma's wording for the common syntax errors (fancy-regex's otherwise).
fn onig_message(e: &str) -> String {
    const MAP: &[(&str, &str)] = &[
        ("Opening parenthesis without closing parenthesis", "end pattern with unmatched parenthesis"),
        ("end of string not reached", "unmatched close parenthesis"),
        ("Invalid character class", "premature end of char-class"),
        ("Target of repeat operator is invalid", "target of repeat operator is not specified"),
        ("Unknown group flag", "undefined group option"),
        ("Could not parse group name", "invalid group name"),
        ("Invalid back reference", "invalid backref number/name"),
        ("Backslash without following character", "end pattern at escape"),
    ];
    for (k, v) in MAP {
        if e.contains(k) {
            return v.to_string();
        }
    }
    e.to_string()
}

fn type_error(v: &Value, msg: &str) -> Flow {
    err(format!("{} ({}) {}", v.kind(), dump_trunc(v, 30), msg))
}

fn cp_index(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

fn group(s: &str, m: Option<(usize, usize)>, name: Option<&str>) -> Value {
    let mut o = Map::new();
    match m {
        None => {
            o.insert(Rc::from("offset"), Value::num(-1.0));
            o.insert(Rc::from("string"), Value::Null);
            o.insert(Rc::from("length"), Value::num(0.0));
        }
        Some((b, e)) => {
            o.insert(Rc::from("offset"), Value::num(cp_index(s, b) as f64));
            if b == e {
                o.insert(Rc::from("string"), Value::str(""));
                o.insert(Rc::from("length"), Value::num(0.0));
            } else {
                o.insert(Rc::from("length"), Value::num(s[b..e].chars().count() as f64));
                o.insert(Rc::from("string"), Value::str(&s[b..e]));
            }
        }
    }
    o.insert(Rc::from("name"), name.map(Value::str).unwrap_or(Value::Null));
    Value::obj(o)
}

pub fn match_impl(input: &Value, regex: &Value, modifiers: &Value, testmode: &Value) -> Result<Value, Flow> {
    let test = matches!(testmode, Value::Bool(true));
    let Value::Str(s) = input else { return Err(type_error(input, "cannot be matched, as it is not a string")) };
    let Value::Str(re) = regex else { return Err(type_error(regex, "is not a string")) };
    let mut global = false;
    let mut not_empty = false;
    let mut flags = String::new();
    match modifiers {
        Value::Str(m) => {
            for c in m.chars() {
                match c {
                    'g' => global = true,
                    'i' => flags.push('i'),
                    'x' => flags.push('x'),
                    // Oniguruma's MULTILINE is dot-matches-newline; SINGLELINE is its default.
                    'm' | 'p' => flags.push('s'),
                    's' | 'l' => {}
                    'n' => not_empty = true,
                    _ => return Err(err(format!("{m} is not a valid modifier string"))),
                }
            }
        }
        Value::Null => {}
        other => return Err(type_error(other, "is not a string")),
    }
    let pattern = if flags.is_empty() { re.to_string() } else { format!("(?{flags}){re}") };
    let r = compile(&pattern)?;
    let names: Vec<Option<String>> = r.capture_names().map(|n| n.map(str::to_string)).collect();
    let mut result = vec![];
    let mut start = 0usize;
    let s: &str = s;
    while start <= s.len() {
        let caps = match r.captures_from_pos(s, start) {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => return Err(err(format!("Regex failure: {e}"))),
        };
        let m0 = caps.get(0).unwrap();
        if not_empty && m0.start() == m0.end() {
            // FIND_NOT_EMPTY: an empty match does not count; look further on.
            let next = s[m0.end()..].chars().next().map_or(1, |c| c.len_utf8());
            start = m0.end() + next;
            if !global && start > s.len() {
                break;
            }
            continue;
        }
        if test {
            return Ok(Value::Bool(true));
        }
        let mut o = Map::new();
        o.insert(Rc::from("offset"), Value::num(cp_index(s, m0.start()) as f64));
        o.insert(Rc::from("length"), Value::num(s[m0.start()..m0.end()].chars().count() as f64));
        o.insert(Rc::from("string"), Value::str(m0.as_str()));
        let mut captures = vec![];
        for i in 1..caps.len() {
            let g = caps.get(i).map(|m| (m.start(), m.end()));
            captures.push(group(s, g, names.get(i).and_then(|n| n.as_deref())));
        }
        o.insert(Rc::from("captures"), Value::arr(captures));
        result.push(Value::obj(o));
        if m0.start() == m0.end() {
            let next = s[m0.end()..].chars().next().map_or(1, |c| c.len_utf8());
            start = m0.end() + next;
        } else {
            start = m0.end();
        }
        if !global {
            break;
        }
    }
    if test {
        return Ok(Value::Bool(false));
    }
    Ok(Value::arr(result))
}
