//! Go's regexp (RE2 syntax) on the regex crate: Perl classes are ASCII-only in Go.

use regex::Regex;

/// Rewrites \d \s \w (and their negations) and \b to Go's ASCII meaning.
fn translate(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            let rep = match n {
                'd' => Some(if in_class { "0-9" } else { "[0-9]" }),
                'D' => Some(if in_class { "\\x00-/:-\\x{10FFFF}" } else { "[^0-9]" }),
                's' => Some(if in_class { "\\t\\n\\x0C\\r " } else { "[\\t\\n\\x0C\\r ]" }),
                'S' => Some(if in_class { "\\x00-\\x08\\x0B\\x0E-\\x1F!-\\x{10FFFF}" } else { "[^\\t\\n\\x0C\\r ]" }),
                'w' => Some(if in_class { "0-9A-Za-z_" } else { "[0-9A-Za-z_]" }),
                'W' => Some(if in_class { "\\x00-/:-@\\[-^`{-\\x{10FFFF}" } else { "[^0-9A-Za-z_]" }),
                'b' if !in_class => Some("(?-u:\\b)"),
                'B' if !in_class => Some("(?-u:\\B)"),
                _ => None,
            };
            match rep {
                Some(r) => out.push_str(r),
                None => {
                    out.push(c);
                    out.push(n);
                }
            }
            i += 2;
            continue;
        }
        if c == '[' && !in_class {
            in_class = true;
            out.push(c);
            i += 1;
            // a leading ^ and ] belong to the class
            if i < chars.len() && chars[i] == '^' {
                out.push('^');
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                out.push_str("\\]");
                i += 1;
            }
            continue;
        }
        if c == '[' && in_class && i + 1 < chars.len() && chars[i + 1] == ':' {
            // [:alpha:] inside a class
            if let Some(end) = src[src.char_indices().nth(i).unwrap().0..].find(":]") {
                let start_byte = src.char_indices().nth(i).unwrap().0;
                out.push_str(&src[start_byte..start_byte + end + 2]);
                let consumed = src[start_byte..start_byte + end + 2].chars().count();
                i += consumed;
                continue;
            }
        }
        if c == ']' && in_class {
            in_class = false;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// regexp.Compile
pub fn compile(src: &str) -> Result<Regex, String> {
    Regex::new(&translate(src)).map_err(|e| go_error(src, &e.to_string()))
}

fn go_error(src: &str, rust: &str) -> String {
    let what = if rust.contains("unclosed group") {
        format!("missing closing ): `{src}`")
    } else if rust.contains("unopened group") {
        format!("unexpected ): `{src}`")
    } else if rust.contains("unclosed character class") {
        let i = src.rfind('[').unwrap_or(0);
        format!("missing closing ]: `{}`", &src[i..])
    } else if rust.contains("repetition operator missing expression") {
        format!("missing argument to repetition operator: `{}`", src.chars().find(|c| matches!(c, '*' | '+' | '?')).unwrap_or('*'))
    } else {
        format!("invalid regexp: `{src}`")
    };
    format!("error parsing regexp: {what}")
}

/// Go's ReplaceAllString template: the same $name / ${name} syntax as the regex crate.
pub fn convert_replacement(s: &str) -> String {
    s.to_string()
}
