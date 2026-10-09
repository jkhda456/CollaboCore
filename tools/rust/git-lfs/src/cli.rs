//! Command-line parsing as cobra/pflag do it for git-lfs: flags anywhere among the
//! arguments, `--` ends them, short flags combine, `--flag=value`, and pflag's error texts.

use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum K {
    Bool,
    Str,
    /// StringSlice: also splits on commas.
    StrSlice,
    Int,
}

#[derive(Clone, Debug)]
pub struct Flag {
    pub long: &'static str,
    pub short: Option<char>,
    pub kind: K,
}

pub const fn flag(long: &'static str, short: Option<char>, kind: K) -> Flag {
    Flag { long, short, kind }
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub vals: BTreeMap<&'static str, Vec<String>>,
    pub args: Vec<String>,
    /// Flags given explicitly (cobra's Changed).
    pub changed: Vec<&'static str>,
}

impl Parsed {
    pub fn bool(&self, k: &str) -> bool {
        self.vals.get(k).and_then(|v| v.last()).map_or(false, |v| v == "true")
    }
    pub fn str(&self, k: &str) -> String {
        self.vals.get(k).and_then(|v| v.last()).cloned().unwrap_or_default()
    }
    pub fn strs(&self, k: &str) -> Vec<String> {
        self.vals.get(k).cloned().unwrap_or_default()
    }
    pub fn int(&self, k: &str, def: i64) -> i64 {
        self.vals.get(k).and_then(|v| v.last()).and_then(|v| v.parse().ok()).unwrap_or(def)
    }
    pub fn changed(&self, k: &str) -> bool {
        self.changed.iter().any(|c| *c == k)
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

fn set(p: &mut Parsed, f: &Flag, v: &str) -> Result<(), String> {
    let name = match f.short {
        Some(s) => format!("-{s}, --{}", f.long),
        None => format!("--{}", f.long),
    };
    let val = match f.kind {
        K::Bool => match parse_bool(v) {
            Some(b) => b.to_string(),
            None => return Err(format!("invalid argument \"{v}\" for \"{name}\" flag: strconv.ParseBool: parsing {}: invalid syntax", crate::tools::quote(v))),
        },
        K::Int => match v.parse::<i64>() {
            Ok(_) => v.to_string(),
            Err(_) => return Err(format!("invalid argument \"{v}\" for \"{name}\" flag: strconv.ParseInt: parsing {}: invalid syntax", crate::tools::quote(v))),
        },
        _ => v.to_string(),
    };
    if !p.changed.contains(&f.long) {
        p.changed.push(f.long);
    }
    let e = p.vals.entry(f.long).or_default();
    match f.kind {
        K::StrSlice => e.extend(val.split(',').map(str::to_string)),
        _ => *e = vec![val],
    }
    Ok(())
}

/// Parses `args` against `flags`; an error is pflag's message.
pub fn parse(args: &[String], flags: &[Flag]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            p.args.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(rest) = a.strip_prefix("--") {
            let (name, val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
                return Err(format!("bad flag syntax: {a}"));
            }
            let Some(f) = flags.iter().find(|f| f.long == name) else {
                if name == "help" {
                    p.vals.insert("help", vec!["true".into()]);
                    continue;
                }
                return Err(format!("unknown flag: --{name}"));
            };
            let v = match (val, f.kind) {
                (Some(v), _) => v,
                (None, K::Bool) => "true".into(),
                (None, _) => {
                    if i < args.len() {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        return Err(format!("flag needs an argument: --{name}"));
                    }
                }
            };
            set(&mut p, f, &v)?;
        } else if a.len() > 1 && a.starts_with('-') {
            let s: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < s.len() {
                let c = s[j];
                j += 1;
                let Some(f) = flags.iter().find(|f| f.short == Some(c)) else {
                    if c == 'h' {
                        p.vals.insert("help", vec!["true".into()]);
                        continue;
                    }
                    return Err(format!("unknown shorthand flag: '{c}' in {a}"));
                };
                if f.kind == K::Bool {
                    if j < s.len() && s[j] == '=' {
                        let v: String = s[j + 1..].iter().collect();
                        set(&mut p, f, &v)?;
                        break;
                    }
                    set(&mut p, f, "true")?;
                    continue;
                }
                let v: String = if j < s.len() {
                    let mut v: String = s[j..].iter().collect();
                    if let Some(x) = v.strip_prefix('=') {
                        v = x.to_string();
                    }
                    v
                } else if i < args.len() {
                    i += 1;
                    args[i - 1].clone()
                } else {
                    return Err(format!("flag needs an argument: '{c}' in -{c}"));
                };
                set(&mut p, f, &v)?;
                break;
            }
        } else {
            p.args.push(a.clone());
        }
    }
    Ok(p)
}
