//! The parts of Go's net/url that git-lfs relies on (Parse, String, Hostname, Port, user
//! info): its rules, not a browser's, so that URLs match and print as they do in git-lfs.

use crate::errors::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub opaque: String,
    pub user: Option<(String, Option<String>)>,
    pub host: String,
    pub path: String,
    pub raw_query: Option<String>,
    pub fragment: Option<String>,
}

fn get_scheme(raw: &str) -> Result<(String, &str)> {
    for (i, c) in raw.char_indices() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || c == '+' || c == '-' || c == '.' {
            if i == 0 {
                return Ok((String::new(), raw));
            }
            continue;
        }
        if c == ':' {
            if i == 0 {
                return Err(Error::new("missing protocol scheme"));
            }
            return Ok((raw[..i].to_string(), &raw[i + 1..]));
        }
        return Ok((String::new(), raw));
    }
    Ok((String::new(), raw))
}

fn unescape(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(Error::new(format!("invalid URL escape {}", crate::tools::quote(&s[i..]))));
            }
            let h = |c: u8| (c as char).to_digit(16);
            match (h(b[i + 1]), h(b[i + 2])) {
                (Some(x), Some(y)) => out.push((x * 16 + y) as u8),
                _ => return Err(Error::new(format!("invalid URL escape {}", crate::tools::quote(&s[i..i + 3])))),
            }
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// url.Parse; errors read `parse "RAW": what`.
pub fn parse(raw: &str) -> Result<Url> {
    parse_inner(raw).map_err(|e| Error::new(format!("parse {}: {}", crate::tools::quote(raw), e)))
}

fn parse_inner(raw: &str) -> Result<Url> {
    if raw.bytes().any(|c| c < 0x20 || c == 0x7f) {
        return Err(Error::new("net/url: invalid control character in URL"));
    }
    let mut u = Url::default();
    let (rest, frag) = match raw.find('#') {
        Some(i) => (&raw[..i], Some(raw[i + 1..].to_string())),
        None => (raw, None),
    };
    u.fragment = frag;
    if rest == "*" {
        u.path = "*".into();
        return Ok(u);
    }
    let (scheme, mut rest) = get_scheme(rest)?;
    u.scheme = scheme.to_lowercase();
    if let Some(i) = rest.find('?') {
        u.raw_query = Some(rest[i + 1..].to_string());
        rest = &rest[..i];
    }
    if !rest.starts_with('/') {
        if !u.scheme.is_empty() {
            u.opaque = rest.to_string();
            return Ok(u);
        }
        let seg = rest.split('/').next().unwrap_or("");
        if seg.contains(':') {
            return Err(Error::new("first path segment in URL cannot contain colon"));
        }
    }
    if (!u.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let r = &rest[2..];
        let (auth, p) = match r.find('/') {
            Some(i) => (&r[..i], &r[i..]),
            None => (r, ""),
        };
        let (user, host) = match auth.rfind('@') {
            Some(i) => (Some(&auth[..i]), &auth[i + 1..]),
            None => (None, auth),
        };
        if let Some(ui) = user {
            let (n, p) = match ui.find(':') {
                Some(i) => (unescape(&ui[..i])?, Some(unescape(&ui[i + 1..])?)),
                None => (unescape(ui)?, None),
            };
            u.user = Some((n, p));
        }
        if let Some(i) = host.rfind(':') {
            let port = &host[i + 1..];
            if !host.starts_with('[') || host[..i].ends_with(']') {
                if !port.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(Error::new(format!("invalid port {} after host", crate::tools::quote(&host[i..]))));
                }
            }
        }
        u.host = unescape(host)?;
        rest = p;
    }
    u.path = unescape(rest)?;
    Ok(u)
}

impl Url {
    pub fn hostname(&self) -> &str {
        let h = &self.host;
        if let Some(rest) = h.strip_prefix('[') {
            return rest.split(']').next().unwrap_or("");
        }
        match h.rfind(':') {
            Some(i) => &h[..i],
            None => h,
        }
    }
    pub fn port(&self) -> &str {
        let h = &self.host;
        let after = if h.starts_with('[') { h.rfind(']').map(|i| &h[i + 1..]).unwrap_or("") } else { h.rfind(':').map(|i| &h[i..]).unwrap_or("") };
        after.strip_prefix(':').unwrap_or("")
    }
    pub fn username(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.0.as_str())
    }
    /// URL.EscapedPath().
    pub fn escaped_path(&self) -> String {
        escape_path(&self.path)
    }
    /// URL.String().
    pub fn to_string_go(&self) -> String {
        let mut s = String::new();
        if !self.scheme.is_empty() {
            s.push_str(&self.scheme);
            s.push(':');
        }
        if !self.opaque.is_empty() {
            s.push_str(&self.opaque);
        } else {
            if !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some() {
                if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                    s.push_str("//");
                }
                if let Some((n, p)) = &self.user {
                    s.push_str(&escape_userinfo(n));
                    if let Some(p) = p {
                        s.push(':');
                        s.push_str(&escape_userinfo(p));
                    }
                    s.push('@');
                }
                s.push_str(&self.host);
            }
            if !self.path.is_empty() && !self.path.starts_with('/') && !self.host.is_empty() {
                s.push('/');
            }
            s.push_str(&escape_path(&self.path));
        }
        if let Some(q) = &self.raw_query {
            s.push('?');
            s.push_str(q);
        }
        if let Some(f) = &self.fragment {
            s.push('#');
            s.push_str(f);
        }
        s
    }
}

fn should_escape(c: u8, userinfo: bool) -> bool {
    if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
        return false;
    }
    if userinfo {
        return !b"$&+,;=".contains(&c) && !(c == b'!' || c == b'\'' || c == b'(' || c == b')' || c == b'*');
    }
    !b"$&+,/:;=@!'()*".contains(&c)
}

fn escape_userinfo(s: &str) -> String {
    s.bytes().map(|c| if should_escape(c, true) { format!("%{c:02X}") } else { (c as char).to_string() }).collect()
}

fn escape_path(s: &str) -> String {
    s.bytes().map(|c| if should_escape(c, false) { format!("%{c:02X}") } else { (c as char).to_string() }).collect()
}
