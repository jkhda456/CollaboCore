//! git-lfs/wildmatch v2: gitignore/gitattributes-style patterns, ported as it is (its token
//! model and quirks included), since which paths match decides what LFS tracks and fetches.

#[derive(Clone, Copy, Default)]
pub struct Opts {
    pub basename: bool,
    pub case_fold: bool,
    pub gitattributes: bool,
    pub contents: bool,
}

#[derive(Clone, Debug)]
enum RuneFn {
    Any(Vec<char>),
    Between(char, char),
    Class(String),
}

impl RuneFn {
    fn test(&self, r: char) -> bool {
        match self {
            RuneFn::Any(s) => s.contains(&r),
            RuneFn::Between(a, b) => *a <= r && r <= *b,
            RuneFn::Class(n) => match n.as_str() {
                "alnum" => r.is_alphanumeric(),
                "alpha" => r.is_alphabetic(),
                "blank" => r == ' ' || r == '\t',
                "cntrl" => r.is_control(),
                "digit" => r.is_numeric() && (r.is_ascii_digit() || r.to_digit(10).is_some() || r.is_numeric()),
                "graph" => !r.is_control() && !r.is_whitespace() || r == ' ' && false,
                "lower" => r.is_lowercase(),
                "print" => !r.is_control(),
                "punct" => r.is_ascii_punctuation() || (!r.is_alphanumeric() && !r.is_whitespace() && !r.is_control() && !r.is_ascii()),
                "space" => r.is_whitespace(),
                "upper" => r.is_uppercase(),
                "xdigit" => r.is_ascii_hexdigit(),
                _ => false,
            },
        }
    }
}

#[derive(Clone, Debug)]
enum Cfn {
    Substring(String),
    /// `?` (Some(1)) or `*` (None), then the rest of the component.
    Wildcard(Option<usize>, Vec<Cfn>),
    CharClass(Vec<RuneFn>, Vec<RuneFn>),
}

impl Cfn {
    fn apply<'a>(&self, s: &'a str) -> Option<&'a str> {
        match self {
            Cfn::Substring(sub) => s.strip_prefix(sub.as_str()),
            Cfn::Wildcard(n, fns) => {
                let until = |s: &str| -> Option<&'static str> {
                    let mut head = s;
                    for f in fns {
                        head = f.apply(head)?;
                    }
                    if !head.is_empty() {
                        return None;
                    }
                    Some("")
                };
                match n {
                    Some(n) => {
                        if *n > s.len() || !s.is_char_boundary(*n) {
                            return None;
                        }
                        until(&s[*n..])
                    }
                    None => {
                        let mut i = s.len();
                        while i > 0 {
                            if s.is_char_boundary(i) {
                                if let Some(r) = until(&s[i..]) {
                                    return Some(r);
                                }
                            }
                            i -= 1;
                        }
                        until(s)
                    }
                }
            }
            Cfn::CharClass(inc, exc) => {
                let r = s.chars().next()?;
                if !inc.is_empty() && !inc.iter().any(|f| f.test(r)) {
                    return None;
                }
                if exc.iter().any(|f| f.test(r)) {
                    return None;
                }
                Some(&s[r.len_utf8()..])
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Token {
    Component(Vec<Cfn>),
    DoubleStar(Option<Box<Token>>, bool),
    Unanchored(Box<Token>),
    Trailing,
}

impl Token {
    fn consume(&self, path: &[String], is_dir: bool) -> (Vec<String>, bool) {
        match self {
            Token::DoubleStar(until, empty) => {
                if path.is_empty() {
                    return (path.to_vec(), *empty);
                }
                let Some(u) = until else { return (vec![], true) };
                let mut i = path.len();
                while i > 0 {
                    let (rest, ok) = u.consume(&path[i..], false);
                    if ok {
                        return (rest, ok);
                    }
                    i -= 1;
                }
                u.consume(path, is_dir)
            }
            Token::Unanchored(u) => Token::DoubleStar(Some(u.clone()), false).consume(path, is_dir),
            Token::Trailing => Token::DoubleStar(None, true).consume(path, is_dir),
            Token::Component(fns) => {
                if path.is_empty() {
                    return (path.to_vec(), false);
                }
                let mut head: &str = &path[0];
                for f in fns {
                    match f.apply(head) {
                        Some(h) => head = h,
                        None => return (path.to_vec(), false),
                    }
                }
                if !head.is_empty() {
                    let mut v = vec![head.to_string()];
                    v.extend_from_slice(&path[1..]);
                    return (v, false);
                }
                (path[1..].to_vec(), true)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Wildmatch {
    ts: Vec<Token>,
    p: String,
    basename: bool,
    case_fold: bool,
    gitattributes: bool,
}

const ESCAPES: &[u8] = b"\\[]*?#";

fn slash_escape(p: &str) -> String {
    let b = p.as_bytes();
    let mut out: Vec<u8> = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            if i + 1 < b.len() && ESCAPES.contains(&b[i + 1]) {
                out.push(b'\\');
                out.push(b[i + 1]);
                i += 2;
            } else {
                out.push(b'/');
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_component(s: &str) -> Result<Vec<Cfn>, String> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    let b = s.as_bytes();
    match b[0] {
        b'\\' => {
            if b.len() < 2 {
                return Err("wildmatch: unclosed escape sequence".into());
            }
            let c = s[1..].chars().next().unwrap();
            let mut v = vec![Cfn::Substring((b[1] as char).to_string())];
            let _ = c;
            if b.len() > 2 {
                v.extend(parse_component(&s[2..])?);
            }
            Ok(v)
        }
        b'[' => {
            let mut i = 1;
            let mut inc = vec![];
            let mut exc = vec![];
            let mut run: Vec<u8> = vec![];
            let mut neg = false;
            let push = |yes: bool, inc: &mut Vec<RuneFn>, exc: &mut Vec<RuneFn>, f: RuneFn| if yes { inc.push(f) } else { exc.push(f) };
            while i < b.len() {
                if b[i] == b'^' || b[i] == b'!' {
                    neg = !neg;
                    i += 1;
                } else if s[i..].starts_with("[:") {
                    let Some(close) = s[i..].find(":]") else { return Err("unclosed character class".into()) };
                    if close == 1 {
                        run.extend_from_slice(b"[:]");
                        i += 2;
                        continue;
                    }
                    let name = s[i..i + close].to_lowercase().trim_start_matches("[:").to_string();
                    if !["alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space", "upper", "xdigit"].contains(&name.as_str()) {
                        return Err(format!("wildmatch: unknown class: {}", crate::tools::quote(&name)));
                    }
                    push(!neg, &mut inc, &mut exc, RuneFn::Class(name));
                    i += close + 2;
                } else if b[i] == b'-' {
                    let mut start = 0u8;
                    if let Some(l) = run.pop() {
                        start = l;
                    }
                    let Some(&end) = b.get(i + 1) else { return Err("runtime error: index out of range".into()) };
                    if !run.is_empty() {
                        push(!neg, &mut inc, &mut exc, RuneFn::Any(String::from_utf8_lossy(&run).chars().collect()));
                        run.clear();
                    }
                    let (a, z) = if end < start { (end, start) } else { (start, end) };
                    push(!neg, &mut inc, &mut exc, RuneFn::Between(a as char, z as char));
                    i += 2;
                } else if b[i] == b'\\' {
                    if i + 1 >= b.len() {
                        return Err("wildmatch: unclosed escape".into());
                    }
                    run.push(b[i + 1]);
                    i += 2;
                } else if b[i] == b']' {
                    break;
                } else {
                    run.push(b[i]);
                    i += 1;
                }
            }
            if !run.is_empty() {
                push(!neg, &mut inc, &mut exc, RuneFn::Any(String::from_utf8_lossy(&run).chars().collect()));
            }
            let rest = if i + 1 < b.len() { &s[i + 1..] } else { "" };
            let mut v = vec![Cfn::CharClass(inc, exc)];
            v.extend(parse_component(rest)?);
            Ok(v)
        }
        b'?' => Ok(vec![Cfn::Wildcard(Some(1), parse_component(&s[1..])?)]),
        b'*' => Ok(vec![Cfn::Wildcard(None, parse_component(&s[1..])?)]),
        _ => {
            let i = b.iter().position(|c| matches!(c, b'[' | b'*' | b'?' | b'\\')).unwrap_or(b.len());
            let mut v = vec![Cfn::Substring(s[..i].to_string())];
            v.extend(parse_component(&s[i..])?);
            Ok(v)
        }
    }
}

impl Wildmatch {
    pub fn new(p: &str, o: Opts) -> Result<Wildmatch, String> {
        let mut w = Wildmatch { ts: vec![], p: slash_escape(p), basename: o.basename, case_fold: o.case_fold, gitattributes: o.gitattributes };
        if w.case_fold {
            w.p = w.p.to_lowercase();
        }
        let parts: Vec<String> = w.p.split('/').map(str::to_string).collect();
        if parts.len() > 1 {
            w.basename = false;
        }
        w.ts = w.parse_tokens(parts, o.contents)?;
        Ok(w)
    }

    fn parse_tokens(&self, mut dirs: Vec<String>, contents: bool) -> Result<Vec<Token>, String> {
        if dirs.is_empty() {
            return Ok(vec![]);
        }
        let mut finals: Option<Vec<Token>> = None;
        if !self.gitattributes {
            let trailing_empty = dirs.len() > 1 && dirs[dirs.len() - 1].is_empty();
            let mut n = dirs.len();
            if trailing_empty {
                n -= 1;
            }
            if contents {
                finals = Some(vec![Token::Trailing]);
                if trailing_empty {
                    dirs.truncate(n);
                }
            }
            if n == 1 && (trailing_empty || contents) {
                let rest = parse_simple(&dirs)?;
                let mut tokens = vec![Token::Unanchored(Box::new(rest[0].clone()))];
                if finals.is_none() && rest.len() > 1 {
                    finals = Some(rest[1..].to_vec());
                }
                tokens.extend(finals.unwrap_or_default());
                return Ok(tokens);
            }
        }
        let mut comps = parse_simple(&dirs)?;
        comps.extend(finals.unwrap_or_default());
        Ok(comps)
    }

    pub fn matches(&self, t: &str) -> bool {
        self.matches_opts(t, false)
    }

    pub fn matches_opts(&self, t: &str, is_directory: bool) -> bool {
        match self.consume(t, is_directory) {
            Some(d) => d.is_empty(),
            None => false,
        }
    }

    fn consume(&self, t: &str, is_directory: bool) -> Option<Vec<String>> {
        let mut t = t.to_string();
        if self.basename {
            t = go_base(&t);
        }
        if self.case_fold {
            t = t.to_lowercase();
        }
        let is_dir;
        if is_directory {
            is_dir = true;
            if !t.ends_with('/') {
                t.push('/');
            }
        } else {
            is_dir = t.ends_with('/');
        }
        let mut dirs: Vec<String> = t.split('/').map(str::to_string).collect();
        if self.gitattributes && is_dir {
            return None;
        }
        for tok in &self.ts {
            let (d, ok) = tok.consume(&dirs, is_dir);
            if !ok {
                return None;
            }
            dirs = d;
        }
        if is_dir && dirs.len() == 1 && dirs[0].is_empty() {
            return Some(vec![]);
        }
        Some(dirs)
    }

    pub fn as_str(&self) -> &str {
        &self.p
    }
}

fn parse_simple(dirs: &[String]) -> Result<Vec<Token>, String> {
    if dirs.is_empty() {
        return Ok(vec![]);
    }
    match dirs[0].as_str() {
        "" => {
            if dirs.len() == 1 {
                return Ok(vec![Token::Component(vec![Cfn::Substring(String::new())])]);
            }
            parse_simple(&dirs[1..])
        }
        "**" => {
            let rest = parse_simple(&dirs[1..])?;
            if rest.is_empty() {
                return Ok(vec![Token::DoubleStar(None, false)]);
            }
            let mut v = vec![Token::DoubleStar(Some(Box::new(rest[0].clone())), false)];
            v.extend_from_slice(&rest[1..]);
            Ok(v)
        }
        d => {
            let mut v = vec![Token::Component(parse_component(d)?)];
            v.extend(parse_simple(&dirs[1..])?);
            Ok(v)
        }
    }
}

/// filepath.Base.
pub fn go_base(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return "/".into();
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}
