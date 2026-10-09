//! Names on the 7z command line: wildcards as 7-Zip matches them (`*`, `?`, on names and paths),
//! the include/exclude switches (-i, -x with `!` and `@listfile`, recursed with `r`), and the
//! scan of the disk for `a`/`u`/`h`: what each argument selects, under which name it is stored.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

/// How names are matched in subdirectories: -r (all), -r- (none), -r0 (wildcards only), or the
/// default (none for names in the archive; for the disk, a directory brings its contents).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Recurse {
    Default,
    Yes,
    No,
    WildOnly,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    /// Path components, '/'-separated, wildcards allowed in each.
    pub parts: Vec<String>,
    pub recurse: Recurse,
}

pub fn has_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// 7-Zip's wildcard match on one name: `*` any run, `?` one character.
pub fn wild_match(pat: &str, name: &str, case: bool) -> bool {
    let p: Vec<char> = if case { pat.chars().collect() } else { pat.to_lowercase().chars().collect() };
    let n: Vec<char> = if case { name.chars().collect() } else { name.to_lowercase().chars().collect() };
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn split_path(s: &str) -> Vec<String> {
    s.split('/').filter(|c| !c.is_empty() && *c != ".").map(String::from).collect()
}

impl Pattern {
    pub fn new(s: &str, recurse: Recurse) -> Self {
        let s = if s == "*.*" { "*" } else { s };
        Pattern { parts: split_path(s), recurse }
    }

    /// Whether an archive path matches, as 7-Zip's censor does: the pattern matches the whole
    /// path or a leading part of it (then the item is inside a selected folder); with recursion,
    /// a pattern of one component may also match at any depth.
    pub fn matches(&self, path: &str, case: bool) -> bool {
        let comps = split_path(path);
        if self.parts.is_empty() {
            return true;
        }
        let at = |start: usize| -> bool {
            if comps.len() < start + self.parts.len() {
                return false;
            }
            self.parts.iter().zip(&comps[start..]).all(|(p, c)| wild_match(p, c, case))
        };
        if at(0) {
            return true;
        }
        let deep = match self.recurse {
            Recurse::Yes => true,
            Recurse::WildOnly => self.parts.iter().any(|p| has_wildcard(p)),
            _ => false,
        };
        deep && (1..comps.len()).any(at)
    }
}

/// -i / -x switch values: `[r[-|0]]{!wildcard|@listfile}`.
pub fn parse_clude(spec: &str, default_recurse: Recurse) -> Result<Vec<Pattern>, String> {
    let mut s = spec;
    let mut rec = default_recurse;
    if let Some(r) = s.strip_prefix('r') {
        s = r;
        rec = Recurse::Yes;
        if let Some(r) = s.strip_prefix('-') {
            s = r;
            rec = Recurse::No;
        } else if let Some(r) = s.strip_prefix('0') {
            s = r;
            rec = Recurse::WildOnly;
        }
    }
    // m[-|2] and w[-] modifiers: accepted, as 7-Zip's defaults.
    loop {
        if let Some(r) = s.strip_prefix("m-").or_else(|| s.strip_prefix("m2")).or_else(|| s.strip_prefix('m')) {
            s = r;
        } else if let Some(r) = s.strip_prefix("w-").or_else(|| s.strip_prefix('w')) {
            s = r;
        } else {
            break;
        }
    }
    if let Some(w) = s.strip_prefix('!') {
        return Ok(vec![Pattern::new(w, rec)]);
    }
    if let Some(f) = s.strip_prefix('@') {
        let data = std::fs::read_to_string(f).map_err(|e| format!("{f}: {}", crate::common::errstr(&e)))?;
        return Ok(data.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| Pattern::new(l, rec)).collect());
    }
    Err(format!("Incorrect wildcard type marker\n{spec}"))
}

/// One thing found on the disk to store.
pub struct DiskItem {
    pub name: String,
    pub path: PathBuf,
    pub meta: Metadata,
    pub is_dir: bool,
    pub link: Option<String>,
}

pub struct ScanResult {
    pub items: Vec<DiskItem>,
    /// Arguments that matched nothing on the disk (name, error text).
    pub missing: Vec<(String, String)>,
}

pub struct ScanOpts<'a> {
    pub recurse: Recurse,
    pub excludes: &'a [Pattern],
    pub full_paths: bool,
    pub store_links: bool,
    pub case: bool,
}

/// The name an argument is stored under: as given when it is a plain relative path; only from
/// its last component on when it is absolute or goes through `.` or `..` (7-Zip's default;
/// -spf keeps the full path).
fn stored_prefix(arg: &str, full: bool) -> (String, bool) {
    let comps: Vec<&str> = arg.split('/').filter(|c| !c.is_empty()).collect();
    if full {
        return (comps.iter().filter(|c| **c != ".").copied().collect::<Vec<_>>().join("/"), true);
    }
    let odd = arg.starts_with('/') || comps.iter().any(|c| *c == "." || *c == "..");
    if odd {
        (comps.last().map(|s| s.to_string()).unwrap_or_default(), false)
    } else {
        (comps.join("/"), true)
    }
}

fn excluded(name: &str, o: &ScanOpts) -> bool {
    o.excludes.iter().any(|p| p.matches(name, o.case))
}

fn meta_of(path: &Path, links: bool) -> std::io::Result<Metadata> {
    if links {
        std::fs::symlink_metadata(path)
    } else {
        std::fs::metadata(path)
    }
}

fn push(out: &mut Vec<DiskItem>, name: String, path: PathBuf, meta: Metadata) {
    let is_dir = meta.is_dir();
    let link = if meta.file_type().is_symlink() { std::fs::read_link(&path).ok().map(|t| t.to_string_lossy().into_owned()) } else { None };
    out.push(DiskItem { name, path, meta, is_dir, link });
}

/// A directory's entries as 7-Zip enumerates them: all of them in readdir order, then each
/// subdirectory's in turn.
fn walk(dir: &Path, name: &str, o: &ScanOpts, out: &mut Vec<DiskItem>, filter: Option<&str>, errors: &mut Vec<(String, String)>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            errors.push((dir.to_string_lossy().into_owned(), crate::common::errstr(&e)));
            return;
        }
    };
    let mut subdirs: Vec<(PathBuf, String, Option<&str>)> = vec![];
    for e in rd.filter_map(|e| e.ok()) {
        let fname = e.file_name().to_string_lossy().into_owned();
        let child_name = if name.is_empty() { fname.clone() } else { format!("{name}/{fname}") };
        let path = e.path();
        let meta = match meta_of(&path, o.store_links) {
            Ok(m) => m,
            Err(err) => {
                errors.push((path.to_string_lossy().into_owned(), crate::common::errstr(&err)));
                continue;
            }
        };
        if excluded(&child_name, o) {
            continue;
        }
        let is_dir = meta.is_dir();
        match filter {
            // A wildcard searched for in subdirectories (-r): only matching names are stored,
            // but every directory is gone through.
            Some(pat) => {
                if wild_match(pat, &fname, o.case) {
                    push(out, child_name.clone(), path.clone(), meta);
                    if is_dir {
                        subdirs.push((path, child_name, None));
                    }
                } else if is_dir {
                    subdirs.push((path, child_name, Some(pat)));
                }
            }
            None => {
                push(out, child_name.clone(), path.clone(), meta);
                if is_dir {
                    subdirs.push((path, child_name, None));
                }
            }
        }
    }
    for (p, n, f) in subdirs {
        walk(&p, &n, o, out, f, errors);
    }
}

/// What the file name arguments select on the disk, in 7-Zip's order (arguments in turn,
/// each directory's entries in readdir order, then its subdirectories').
pub fn scan(args: &[String], o: &ScanOpts) -> ScanResult {
    let mut items = vec![];
    let mut missing = vec![];
    for arg in args {
        let arg = if arg == "*.*" { "*" } else { arg.as_str() };
        let (dir_part, last) = match arg.rfind('/') {
            Some(i) => (&arg[..i + 1], &arg[i + 1..]),
            None => ("", arg),
        };
        if has_wildcard(last) {
            let base = if dir_part.is_empty() { PathBuf::from(".") } else { PathBuf::from(dir_part) };
            let (prefix, _) = stored_prefix(dir_part, o.full_paths);
            let mut found = false;
            let rd = std::fs::read_dir(&base);
            if let Ok(rd) = rd {
                let mut subdirs: Vec<(PathBuf, String, Option<&str>)> = vec![];
                for e in rd.filter_map(|e| e.ok()) {
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let name = if prefix.is_empty() { fname.clone() } else { format!("{prefix}/{fname}") };
                    let path = e.path();
                    let Ok(meta) = meta_of(&path, o.store_links) else { continue };
                    let is_dir = meta.is_dir();
                    if wild_match(last, &fname, o.case) {
                        if excluded(&name, o) {
                            continue;
                        }
                        found = true;
                        push(&mut items, name.clone(), path.clone(), meta);
                        if is_dir && o.recurse != Recurse::No {
                            subdirs.push((path, name, None));
                        }
                    } else if is_dir && matches!(o.recurse, Recurse::Yes | Recurse::WildOnly) {
                        subdirs.push((path, name, Some(last)));
                    }
                }
                for (p, n, f) in subdirs {
                    walk(&p, &n, o, &mut items, f, &mut missing);
                }
            }
            let _ = found;
            continue;
        }
        let path = PathBuf::from(arg);
        let (name, _) = stored_prefix(arg, o.full_paths);
        match meta_of(&path, o.store_links) {
            Ok(meta) => {
                if excluded(&name, o) {
                    continue;
                }
                let is_dir = meta.is_dir();
                push(&mut items, name.clone(), path.clone(), meta);
                if is_dir && o.recurse != Recurse::No {
                    walk(&path, &name, o, &mut items, None, &mut missing);
                }
            }
            Err(e) => missing.push((arg.to_string(), crate::common::errstr(&e))),
        }
    }
    // A name given twice (or found twice) is stored once.
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.name.clone()));
    ScanResult { items, missing }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcards() {
        assert!(wild_match("*.txt", "a.txt", true));
        assert!(!wild_match("*.txt", "a.txt.gz", true));
        assert!(wild_match("a?c*", "abcdef", true));
        assert!(wild_match("*", "", true));
        assert!(wild_match("*.TXT", "a.txt", false));
    }
    #[test]
    fn patterns() {
        let p = Pattern::new("*.txt", Recurse::Default);
        assert!(p.matches("a.txt", true));
        assert!(!p.matches("d/a.txt", true));
        assert!(Pattern::new("*.txt", Recurse::Yes).matches("d/a.txt", true));
        assert!(Pattern::new("d", Recurse::Default).matches("d/x/y", true));
        assert!(Pattern::new("d/sub", Recurse::Default).matches("d/sub/s.txt", true));
        assert_eq!(stored_prefix("./d/h.txt", false).0, "h.txt");
        assert_eq!(stored_prefix("/abs/s.txt", false).0, "s.txt");
        assert_eq!(stored_prefix("d/sub/", false).0, "d/sub");
    }
}
