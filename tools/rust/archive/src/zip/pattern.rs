//! Info-ZIP's shell patterns (util.c recmatch), for -x, -i, -R and the names -d matches in the
//! archive: `*` any run of characters (`/` included, unless -ws: then only `**` crosses it), `?`
//! one character, `[list]` with ranges and `!`/`^` to negate, `\` taking the next character
//! literally. -nw turns `*`, `[` and `\` into ordinary characters (`?` stays, as in recmatch).

#[derive(Clone, Copy, Default)]
pub struct Options {
    /// -ws
    pub stop_at_dir: bool,
    /// -nw
    pub no_wild: bool,
    /// -ic (case_map: ASCII upper case)
    pub ignore_case: bool,
}

/// Whether `s` matches the whole of `p`.
pub fn matches(p: &[u8], s: &[u8], o: Options) -> bool {
    recmatch(p, s, o) == 1
}

/// Whether `p` uses a wildcard (isshexp).
pub fn is_pattern(p: &[u8]) -> bool {
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            b'\\' if i + 1 < p.len() => i += 1,
            b'?' | b'*' | b'[' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

fn eq(a: u8, b: u8, o: Options) -> bool {
    if o.ignore_case {
        a.to_ascii_uppercase() == b.to_ascii_uppercase()
    } else {
        a == b
    }
}

/// 1: match, 0: no match, 2: no match and give up (as recmatch returns them).
fn recmatch(p: &[u8], s: &[u8], o: Options) -> u8 {
    let Some((&c, p)) = p.split_first() else {
        return s.is_empty() as u8;
    };
    if c == b'?' {
        return match s.first() {
            Some(&ch) if !(o.stop_at_dir && ch == b'/') => recmatch(p, &s[1..], o),
            _ => 0,
        };
    }
    if !o.no_wild && c == b'*' {
        let mut p = p;
        if o.stop_at_dir {
            if p.first() != Some(&b'*') {
                // A single '*': no slashes
                let mut s = s;
                while let Some(&ch) = s.first() {
                    if ch == b'/' {
                        break;
                    }
                    let r = recmatch(p, s, o);
                    if r != 0 {
                        return r;
                    }
                    s = &s[1..];
                }
                if p.is_empty() {
                    return s.is_empty() as u8;
                }
                return if p[0] == b'/' || (p[0] == b'\\' && p.get(1) == Some(&b'/')) { recmatch(p, s, o) } else { 2 };
            }
            p = &p[1..];
        }
        if p.is_empty() {
            return 1;
        }
        if !is_pattern(p) {
            // The rest is literal: compare it with the end of the string
            return (s.len() >= p.len() && s[s.len() - p.len()..].iter().zip(p).all(|(&a, &b)| eq(a, b, o))) as u8;
        }
        let mut s = s;
        while !s.is_empty() {
            let r = recmatch(p, s, o);
            if r != 0 {
                return r;
            }
            s = &s[1..];
        }
        return 2;
    }
    if !o.no_wild && c == b'[' {
        let Some(&sc) = s.first() else { return 0 };
        let reverse = matches!(p.first(), Some(b'!' | b'^'));
        let p = if reverse { &p[1..] } else { p };
        // The closing bracket
        let mut q = 0;
        let mut esc = false;
        while q < p.len() {
            if esc {
                esc = false;
            } else if p[q] == b'\\' {
                esc = true;
            } else if p[q] == b']' {
                break;
            }
            q += 1;
        }
        if q >= p.len() {
            return 0;
        }
        let rest = &p[q + 1..];
        let mut start: u8 = 0;
        let mut esc = p.first() == Some(&b'-');
        let mut i = 0;
        while i < q {
            let ch = p[i];
            if !esc && ch == b'\\' {
                esc = true;
            } else if !esc && ch == b'-' {
                start = if i > 0 { p[i - 1] } else { 0 };
            } else {
                if p.get(i + 1) != Some(&b'-') || i + 1 >= q {
                    let lo = if start != 0 { start } else { ch };
                    if lo <= ch && (lo..=ch).any(|u| eq(u, sc, o)) {
                        return if reverse { 0 } else { recmatch(rest, &s[1..], o) };
                    }
                }
                start = 0;
                esc = false;
            }
            i += 1;
        }
        return if reverse { recmatch(rest, &s[1..], o) } else { 0 };
    }
    let (c, p) = if !o.no_wild && c == b'\\' {
        match p.split_first() {
            Some((&c, p)) => (c, p),
            None => return 0,
        }
    } else {
        (c, p)
    };
    match s.first() {
        Some(&ch) if eq(c, ch, o) => recmatch(p, &s[1..], o),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        matches(p.as_bytes(), s.as_bytes(), Options::default())
    }

    #[test]
    fn shell_patterns() {
        assert!(m("*.txt", "d/a.txt"));
        assert!(m("d/*", "d/sub/b.txt"));
        assert!(!m("*.txt", "d/a.txt.bak"));
        assert!(m("d/?.txt", "d/a.txt"));
        assert!(m("[a-c].txt", "b.txt"));
        assert!(!m("[!a-c].txt", "b.txt"));
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
        assert!(m("*/sub/*", "d/sub/b.txt"));
        let ws = Options { stop_at_dir: true, ..Default::default() };
        assert!(!matches(b"*.txt", b"d/a.txt", ws));
        assert!(matches(b"**.txt", b"d/a.txt", ws));
        assert!(matches(b"d/*", b"d/a.txt", ws));
        let ic = Options { ignore_case: true, ..Default::default() };
        assert!(matches(b"*.TXT", b"d/a.txt", ic));
    }
}
