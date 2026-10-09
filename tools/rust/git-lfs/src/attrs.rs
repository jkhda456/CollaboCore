//! .gitattributes (git/gitattr): the lines, macros (`[attr]`), and which patterns set
//! filter=lfs or lockable, with the file they come from.

use crate::wildmatch::{Opts, Wildmatch};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Attr {
    pub k: String,
    pub v: String,
    pub unspecified: bool,
}

pub enum Line {
    Pattern(Wildmatch, Vec<Attr>),
    Macro(String, Vec<Attr>),
}

/// ParseLines: the lines and the file's line ending ("" when it has none).
pub fn parse_lines(data: &[u8]) -> Result<(Vec<Line>, String), String> {
    let mut lines = vec![];
    let (mut lf, mut crlf) = (0, 0);
    let text = String::from_utf8_lossy(data);
    let mut rest: &str = &text;
    while !rest.is_empty() {
        let (raw, next) = match rest.find('\n') {
            Some(i) => {
                let l = &rest[..i];
                if l.ends_with('\r') {
                    crlf += 1;
                    (&l[..l.len() - 1], &rest[i + 1..])
                } else {
                    lf += 1;
                    (l, &rest[i + 1..])
                }
            }
            None => (rest, ""),
        };
        rest = next;
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (pattern, applied, macro_name);
        if t.starts_with('"') {
            let last = t.rfind('"').unwrap();
            if last == 0 {
                return Err(format!("unbalanced quote: {t}"));
            }
            pattern = unquote(&t[..=last]).ok_or_else(|| format!("unable to unquote: {}: invalid syntax", &t[..=last]))?;
            applied = t[last + 1..].trim().to_string();
            macro_name = String::new();
        } else {
            let mut sp = t.splitn(2, ' ');
            let first = sp.next().unwrap();
            if let Some(m) = first.strip_prefix("[attr]") {
                macro_name = m.to_string();
                pattern = String::new();
            } else {
                pattern = first.to_string();
                macro_name = String::new();
            }
            applied = sp.next().unwrap_or("").to_string();
        }
        let mut attrs = vec![];
        for s in applied.split(' ') {
            if s.is_empty() {
                continue;
            }
            let a = if let Some(k) = s.strip_prefix('-') {
                Attr { k: k.to_string(), v: "false".into(), unspecified: false }
            } else if let Some(k) = s.strip_prefix('!') {
                Attr { k: k.to_string(), v: String::new(), unspecified: true }
            } else if let Some((k, v)) = s.split_once('=') {
                Attr { k: k.to_string(), v: v.to_string(), unspecified: false }
            } else {
                Attr { k: s.to_string(), v: "true".into(), unspecified: false }
            };
            attrs.push(a);
        }
        if !pattern.is_empty() {
            let w = Wildmatch::new(&pattern, Opts { basename: true, gitattributes: true, ..Default::default() }).map_err(|e| format!("invalid attribute pattern {}: {}", crate::tools::quote(&pattern), e))?;
            lines.push(Line::Pattern(w, attrs));
        } else {
            lines.push(Line::Macro(macro_name, attrs));
        }
    }
    let eol = if crlf > lf {
        "\r\n"
    } else if lf == 0 {
        ""
    } else {
        "\n"
    };
    Ok((lines, eol.to_string()))
}

fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if c == '"' {
            return None;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            c @ '0'..='7' => {
                let mut v = c.to_digit(8)?;
                for _ in 0..2 {
                    v = v * 8 + it.next()?.to_digit(8)?;
                }
                out.push(char::from_u32(v)?);
            }
            'x' => {
                let h: String = [it.next()?, it.next()?].iter().collect();
                out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

pub struct MacroProcessor {
    macros: BTreeMap<String, Vec<Attr>>,
}

impl MacroProcessor {
    pub fn new() -> MacroProcessor {
        let mut macros = BTreeMap::new();
        macros.insert(
            "binary".to_string(),
            vec![Attr { k: "diff".into(), v: "false".into(), unspecified: false }, Attr { k: "merge".into(), v: "false".into(), unspecified: false }, Attr { k: "text".into(), v: "false".into(), unspecified: false }],
        );
        MacroProcessor { macros }
    }

    /// The pattern lines with macros expanded (reading macro definitions when asked).
    pub fn process_lines(&mut self, lines: Vec<Line>, read_macros: bool) -> Vec<(Wildmatch, Vec<Attr>)> {
        let mut out = vec![];
        for l in lines {
            match l {
                Line::Pattern(w, attrs) => {
                    let mut res = vec![];
                    for a in attrs {
                        if let Some(m) = self.macros.get(&a.k) {
                            if a.v == "true" {
                                res.extend(m.iter().cloned());
                            } else if a.unspecified {
                                for x in m {
                                    res.push(Attr { k: x.k.clone(), v: String::new(), unspecified: true });
                                }
                            }
                        }
                        res.push(a);
                    }
                    out.push((w, res));
                }
                Line::Macro(name, attrs) => {
                    if read_macros {
                        self.macros.insert(name, attrs);
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct AttributePath {
    pub path: String,
    pub source: String,
    pub line_ending: String,
    pub lockable: bool,
    pub tracked: bool,
}

/// filepath.Rel for clean absolute paths (or relative ones against "").
pub fn rel(base: &str, target: &str) -> Option<String> {
    if base.is_empty() {
        return Some(target.to_string());
    }
    let b: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    if base.starts_with('/') != target.starts_with('/') {
        return None;
    }
    let common = b.iter().zip(t.iter()).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; b.len() - common];
    parts.extend_from_slice(&t[common..]);
    if parts.is_empty() {
        return Some(".".into());
    }
    Some(parts.join("/"))
}

pub fn attr_paths_from_data(mp: &mut MacroProcessor, fpath: &str, working_dir: &str, data: &[u8], read_macros: bool) -> Vec<AttributePath> {
    let relfile = rel(working_dir, fpath).unwrap_or_default();
    let dir = std::path::Path::new(&relfile).parent().map(|p| p.display().to_string()).unwrap_or_default();
    let mut reldir = crate::tools::trim_current_prefix(if dir.is_empty() { "." } else { &dir }).to_string();
    if reldir == "." {
        reldir.clear();
    }
    let (lines, eol) = match parse_lines(data) {
        Ok(x) => x,
        Err(e) => {
            crate::trace!("Error parsing attributes from {}: {}", fpath, e);
            return vec![];
        }
    };
    let mut out = vec![];
    for (w, attrs) in mp.process_lines(lines, read_macros) {
        let (mut lockable, mut tracked, mut has_filter) = (false, false, false);
        for a in &attrs {
            if a.k == "filter" {
                has_filter = true;
                tracked = a.v == "lfs";
            } else if a.k == "lockable" && a.v == "true" {
                lockable = true;
            }
        }
        if !has_filter && !lockable {
            continue;
        }
        let mut pattern = w.as_str().to_string();
        if !reldir.is_empty() {
            pattern = crate::tools::clean_str(&format!("{reldir}/{pattern}"));
        }
        out.push(AttributePath { path: pattern, source: relfile.clone(), line_ending: eol.clone(), lockable, tracked });
    }
    out
}

fn attr_paths_from_file(mp: &mut MacroProcessor, path: &str, working_dir: &str, read_macros: bool) -> Vec<AttributePath> {
    match std::fs::read(path) {
        Ok(d) => attr_paths_from_data(mp, path, working_dir, &d, read_macros),
        Err(_) => vec![],
    }
}

/// The repository's attribute files: .git/info/attributes and every .gitattributes git
/// knows of, deepest first.
fn find_attribute_files(working_dir: &str, git_dir: &str) -> Vec<(String, bool)> {
    let mut paths = vec![];
    let repo = format!("{git_dir}/info/attributes");
    if std::fs::metadata(&repo).map(|m| !m.is_dir()).unwrap_or(false) {
        paths.push((repo, true));
    }
    match crate::gitcmd::ls_files(working_dir, true, true) {
        Ok(files) => {
            for f in files {
                if crate::wildmatch::go_base(&f) == ".gitattributes" {
                    crate::trace!("findAttributeFiles: located {}", f);
                    paths.push((crate::tools::join(&[working_dir, &f]), f == ".gitattributes"));
                }
            }
        }
        Err(e) => crate::trace!("Error finding .gitattributes: {}", e),
    }
    paths.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    paths
}

pub fn get_attribute_paths(mp: &mut MacroProcessor, working_dir: &str, git_dir: &str) -> Vec<AttributePath> {
    let mut out = vec![];
    for (p, m) in find_attribute_files(working_dir, git_dir) {
        out.extend(attr_paths_from_file(mp, &p, working_dir, m));
    }
    out
}

pub fn get_user_attribute_paths(mp: &mut MacroProcessor) -> Vec<AttributePath> {
    let p = crate::config::cfg().git().get("core.attributesfile").unwrap_or_default();
    let Ok(p) = crate::tools::expand_config_path(&p, "git/attributes") else { return vec![] };
    if std::fs::metadata(&p).is_err() {
        return vec![];
    }
    attr_paths_from_file(mp, &p, "", true)
}

pub fn get_system_attribute_paths(mp: &mut MacroProcessor) -> Vec<AttributePath> {
    let path = if crate::gitcmd::is_git_version_at_least("2.42.0") {
        match crate::gitcmd::git_no_lfs_command(&["var", "GIT_ATTR_SYSTEM"]).output() {
            Ok(o) => String::from_utf8_lossy(&o.stdout).split('\n').next().unwrap_or("").to_string(),
            Err(_) => return vec![],
        }
    } else {
        let prefix = std::env::var("PREFIX").unwrap_or_default();
        crate::tools::join(&[if prefix.is_empty() { "/" } else { &prefix }, "etc", "gitattributes"])
    };
    if std::fs::metadata(&path).is_err() {
        return vec![];
    }
    attr_paths_from_file(mp, &path, "", true)
}

/// GetAttributeFilter: the LFS-filtered patterns as a gitattributes filter.
pub fn lfs_attribute_filter() -> crate::filter::Filter {
    let c = crate::config::cfg();
    let paths = get_attribute_paths(&mut MacroProcessor::new(), &c.local_working_dir(), &c.local_git_dir());
    let pats: Vec<String> = paths.iter().map(|p| p.path.clone()).collect();
    crate::filter::Filter::new(&pats, &[], crate::filter::PatternType::GitAttributes)
}
