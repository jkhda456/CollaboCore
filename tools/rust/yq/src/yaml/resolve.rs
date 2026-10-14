//! go-yaml's resolve: the implicit tag of a plain scalar.

use super::*;
use crate::gostr::{parse_float, parse_int, parse_uint};

fn resolve_map(s: &str) -> Option<&'static str> {
    Some(match s {
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => BOOL_TAG,
        "" | "~" | "null" | "Null" | "NULL" => NULL_TAG,
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF" | "-0"
        | "-0.0" => FLOAT_TAG,
        "<<" => MERGE_TAG,
        _ => return None,
    })
}

/// `^[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?$`
fn yaml_style_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return false;
        }
    } else {
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return false;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return false;
        }
    }
    i == b.len()
}

/// go-yaml's parseTimestamp.
pub fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i != 4 || i == b.len() || b[i] != b'-' {
        return false;
    }
    for layout in [
        "2006-1-2T15:4:5.999999999Z07:00",
        "2006-1-2t15:4:5.999999999Z07:00",
        "2006-1-2 15:4:5.999999999",
        "2006-1-2",
    ] {
        if crate::gotime::parse(layout, s).is_ok() {
            return true;
        }
    }
    false
}

/// The tag go-yaml gives an untagged plain scalar.
pub fn resolve(s: &str) -> &'static str {
    let hint = match s.as_bytes().first() {
        None => b'N',
        Some(c) => match c {
            b'+' | b'-' => b'S',
            b'0'..=b'9' => b'D',
            b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~' | b'<' => b'M',
            b'.' => b'.',
            _ => 0,
        },
    };
    if hint != 0 {
        if let Some(tag) = resolve_map(s) {
            return tag;
        }
        match hint {
            b'.' => {
                if parse_float(s).is_ok() {
                    return FLOAT_TAG;
                }
            }
            b'D' | b'S' => {
                if is_timestamp(s) {
                    return TIMESTAMP_TAG;
                }
                let plain: String = s.chars().filter(|c| *c != '_').collect();
                if parse_int(&plain, 0).is_ok() {
                    return INT_TAG;
                }
                if parse_uint(&plain, 0).is_ok() {
                    return INT_TAG;
                }
                if yaml_style_float(&plain) && parse_float(&plain).is_ok() {
                    return FLOAT_TAG;
                }
            }
            _ => {}
        }
    }
    STR_TAG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(resolve("12"), INT_TAG);
        assert_eq!(resolve("0x1F"), INT_TAG);
        assert_eq!(resolve("08"), FLOAT_TAG);
        assert_eq!(resolve("1_000"), INT_TAG);
        assert_eq!(resolve("1.5"), FLOAT_TAG);
        assert_eq!(resolve(".5"), FLOAT_TAG);
        assert_eq!(resolve("1e400"), STR_TAG);
        assert_eq!(resolve("true"), BOOL_TAG);
        assert_eq!(resolve("yes"), STR_TAG);
        assert_eq!(resolve(""), NULL_TAG);
        assert_eq!(resolve("~"), NULL_TAG);
        assert_eq!(resolve("2001-12-14"), TIMESTAMP_TAG);
        assert_eq!(resolve("2001-12-14T21:59:43.10-05:00"), TIMESTAMP_TAG);
        assert_eq!(resolve("2001-12-14 21:59:43.10"), TIMESTAMP_TAG);
        assert_eq!(resolve("2001-13-14"), STR_TAG);
        assert_eq!(resolve("<<"), MERGE_TAG);
        assert_eq!(resolve("-"), STR_TAG);
        assert_eq!(resolve("+1"), INT_TAG);
        assert_eq!(resolve("cat"), STR_TAG);
        assert_eq!(resolve(".inf"), FLOAT_TAG);
    }
}
