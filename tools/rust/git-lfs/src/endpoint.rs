//! Where a remote's LFS API is (lfsapi/endpoint_finder.go, lfshttp/endpoint.go): lfs.url,
//! remote.NAME.lfsurl, or derived from the git remote URL (`.git/info/lfs`), with
//! url.*.insteadOf aliases, ssh URLs (git-lfs-authenticate or git-lfs-transfer), local paths.

use crate::config::{self, cfg};
use crate::gitcmd;
use crate::gourl;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub const URL_UNKNOWN: &str = "<unknown>";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SshMetadata {
    pub user_and_host: String,
    pub port: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub ssh: SshMetadata,
    pub operation: String,
    pub original_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    None,
    Basic,
    Negotiate,
    /// Any other lfs.<url>.access value, kept as it is (lowercased).
    Other(&'static str),
}

impl Access {
    pub fn mode(&self) -> &'static str {
        match self {
            Access::None => "none",
            Access::Basic => "basic",
            Access::Negotiate => "negotiate",
            Access::Other(s) => s,
        }
    }
}

struct Finder {
    git_protocol: String,
    git_dir: String,
    aliases: BTreeMap<String, String>,
    push_aliases: BTreeMap<String, String>,
    remote_list: Vec<String>,
    access: Mutex<BTreeMap<String, Access>>,
}

fn finder() -> &'static Finder {
    static F: OnceLock<Finder> = OnceLock::new();
    F.get_or_init(|| {
        let g = cfg().git();
        let git_dir = if cfg().in_repo() { cfg().local_git_dir() } else { gitcmd::git_dir().unwrap_or_default() };
        let mut f = Finder {
            git_protocol: g.get("lfs.gitprotocol").unwrap_or_else(|| "https".into()),
            git_dir,
            aliases: BTreeMap::new(),
            push_aliases: BTreeMap::new(),
            remote_list: gitcmd::remote_list().unwrap_or_default(),
            access: Mutex::new(BTreeMap::new()),
        };
        for (k, vals) in &g.vals {
            if vals.is_empty() || !k.starts_with("url.") {
                continue;
            }
            let (map, suffix) = if k.ends_with(".insteadof") {
                (&mut f.aliases, ".insteadof")
            } else if k.ends_with(".pushinsteadof") {
                (&mut f.push_aliases, ".pushinsteadof")
            } else {
                continue;
            };
            let url = &k[4..k.len() - suffix.len()];
            for v in vals {
                if let Some(old) = map.get(v) {
                    if old != url {
                        eprintln!("warning: Multiple 'url.*.{}' keys with the same alias: {}", suffix, crate::tools::quote(v));
                    }
                }
                map.insert(v.clone(), url.to_string());
            }
        }
        f
    })
}

fn replace_alias(map: &BTreeMap<String, String>, raw: &str) -> Option<String> {
    let mut longest = "";
    for alias in map.keys() {
        if raw.starts_with(alias.as_str()) && longest < alias.as_str() {
            longest = alias;
        }
    }
    (!longest.is_empty()).then(|| format!("{}{}", map[longest], &raw[longest.len()..]))
}

pub fn replace_url_alias(operation: &str, raw: &str) -> String {
    let f = finder();
    if operation == "upload" {
        if let Some(r) = replace_alias(&f.push_aliases, raw) {
            return r;
        }
    }
    replace_alias(&f.aliases, raw).unwrap_or_else(|| raw.to_string())
}

/// The endpoint for an operation ("download"/"upload") on a remote (or URL).
pub fn endpoint(operation: &str, remote: &str) -> Endpoint {
    let mut ep = get_endpoint(operation, remote);
    ep.operation = operation.to_string();
    ep
}

fn get_endpoint(operation: &str, remote: &str) -> Endpoint {
    let g = cfg().git();
    if operation == "upload" {
        if let Some(u) = g.get("lfs.pushurl") {
            return new_endpoint(operation, &u);
        }
    }
    if let Some(u) = g.get("lfs.url") {
        return new_endpoint(operation, &u);
    }
    if !remote.is_empty() && remote != "origin" {
        let e = remote_endpoint(operation, remote);
        if !e.url.is_empty() {
            return e;
        }
    }
    remote_endpoint(operation, "origin")
}

pub fn remote_endpoint(operation: &str, remote: &str) -> Endpoint {
    let g = cfg().git();
    let remote = if remote.is_empty() { "origin" } else { remote };
    if operation == "upload" {
        if let Some(u) = g.get(&format!("remote.{remote}.lfspushurl")) {
            return new_endpoint(operation, &u);
        }
    }
    if let Some(u) = g.get(&format!("remote.{remote}.lfsurl")) {
        return new_endpoint(operation, &u);
    }
    let u = git_remote_url(remote, operation == "upload");
    if !u.is_empty() {
        return new_endpoint_from_clone_url(operation, &u);
    }
    let f = finder();
    if !f.git_dir.is_empty() && remote == "origin" {
        match parse_fetch_head(&format!("{}/FETCH_HEAD", f.git_dir)) {
            Ok(u) => return new_endpoint_from_clone_url("download", &u),
            Err(e) => crate::trace!("failed parsing FETCH_HEAD: {}", e),
        }
    }
    Endpoint::default()
}

fn parse_fetch_head(path: &str) -> Result<String, String> {
    let data = std::fs::read_to_string(path).map_err(|e| crate::tools::path_err("open", path, &e).to_string())?;
    let line = data.lines().next().ok_or_else(|| format!("Failed to read content from {path}"))?;
    let re = regex::Regex::new(r"^[a-f0-9]{40,64}\t(not-for-merge)?\t(tag |branch |)'.*' of (?P<url>[/.\-:_a-zA-Z0-9]+)$").unwrap();
    match re.captures(line) {
        Some(m) => Ok(m["url"].trim().to_string()),
        None => Err(format!("failed to extract remote URL from \"{line}\"")),
    }
}

pub fn git_remote_url(remote: &str, for_push: bool) -> String {
    let g = cfg().git();
    if for_push {
        if let Some(u) = g.get(&format!("remote.{remote}.pushurl")) {
            return u;
        }
    }
    if let Some(u) = g.get(&format!("remote.{remote}.url")) {
        return u;
    }
    let f = finder();
    if f.remote_list.iter().any(|r| r == remote) || gitcmd::validate_remote_url(remote).is_ok() {
        return remote.to_string();
    }
    String::new()
}

pub fn new_endpoint_from_clone_url(operation: &str, raw: &str) -> Endpoint {
    let mut ep = new_endpoint(operation, raw);
    if ep.url == URL_UNKNOWN {
        return ep;
    }
    if ep.url.ends_with('/') {
        ep.url.pop();
    }
    if ep.url.starts_with("file://") {
        return ep;
    }
    // path.Ext: the last element's suffix from its last '.'.
    let last = ep.url.rsplit('/').next().unwrap_or("");
    if last.rfind('.').map_or(false, |i| &last[i..] == ".git") {
        ep.url.push_str("/info/lfs");
    } else {
        ep.url.push_str(".git/info/lfs");
    }
    ep
}

pub fn new_endpoint(operation: &str, raw: &str) -> Endpoint {
    let raw = replace_url_alias(operation, raw);
    if raw.starts_with('/') {
        return from_local_path(&raw);
    }
    let u = match gourl::parse(&raw) {
        Ok(u) => u,
        Err(_) => return from_bare_ssh_url(&raw),
    };
    match u.scheme.as_str() {
        "ssh" | "git+ssh" | "ssh+git" => from_ssh_url(&u),
        "http" | "https" | "file" => Endpoint { url: u.to_string_go(), original_url: u.to_string_go(), ..Default::default() },
        "git" => {
            let mut u2 = u.clone();
            u2.scheme = finder().git_protocol.clone();
            Endpoint { url: u2.to_string_go(), ..Default::default() }
        }
        "" => {
            if std::fs::metadata(&raw).is_ok() {
                return from_local_path(&raw);
            }
            from_bare_ssh_url(&u.to_string_go())
        }
        s => {
            if raw.starts_with(&format!("{s}::")) {
                return Endpoint { url: raw.clone(), ..Default::default() };
            }
            if std::fs::metadata(&raw).is_ok() {
                return from_local_path(&raw);
            }
            from_bare_ssh_url(&u.to_string_go())
        }
    }
}

fn from_local_path(p: &str) -> Endpoint {
    let u = gitcmd::rewrite_local_path_as_url(p);
    Endpoint { url: u.clone(), original_url: u, ..Default::default() }
}

pub fn from_ssh_url(u: &gourl::Url) -> Endpoint {
    let re = regex::Regex::new(r"^([^:]+)(?::(\d+))?$").unwrap();
    let Some(m) = re.captures(&u.host) else { return Endpoint { url: URL_UNKNOWN.into(), ..Default::default() } };
    let host = m[1].to_string();
    let mut ep = Endpoint { original_url: u.to_string_go(), ..Default::default() };
    ep.ssh.user_and_host = match u.username() {
        Some(n) if !n.is_empty() => format!("{n}@{host}"),
        _ => host.clone(),
    };
    ep.ssh.port = m.get(2).map(|x| x.as_str().to_string()).unwrap_or_default();
    ep.ssh.path = u.path.clone();
    ep.url = format!("https://{}{}", host, u.path);
    ep
}

pub fn from_bare_ssh_url(raw: &str) -> Endpoint {
    let mut parts: Vec<String> = raw.split(':').map(str::to_string).collect();
    if parts.len() < 2 {
        return Endpoint { url: raw.to_string(), ..Default::default() };
    }
    let new_path = if parts.len() > 2 {
        parts[0] = parts[0].trim_start_matches('[').to_string();
        parts[1] = parts[1].trim_end_matches(']').to_string();
        format!("{}:{}", parts[0], parts[1..].join("/"))
    } else {
        parts.join("/")
    };
    let Ok(nu) = gourl::parse(&format!("ssh://{new_path}")) else { return Endpoint { url: URL_UNKNOWN.into(), ..Default::default() } };
    let mut ep = from_ssh_url(&nu);
    if let Some(p) = ep.ssh.path.strip_prefix('/') {
        ep.ssh.path = p.to_string();
    }
    ep
}

pub fn url_without_auth(raw: &str) -> String {
    if !raw.contains('@') {
        return raw.to_string();
    }
    match gourl::parse(raw) {
        Ok(mut u) => {
            u.user = None;
            u.to_string_go()
        }
        Err(e) => {
            eprint!("Error parsing URL {}: {}", crate::tools::quote(raw), e);
            raw.to_string()
        }
    }
}

/// The access mode for an endpoint URL (lfs.URL.access), cached.
pub fn access_for(raw: &str) -> (Access, String) {
    let au = url_without_auth(raw);
    let f = finder();
    let mut m = f.access.lock().unwrap();
    if let Some(a) = m.get(&au) {
        return (*a, au);
    }
    let a = match config::url_get("lfs", &au, "access") {
        Some(v) if !v.is_empty() => match v.to_lowercase().as_str() {
            "private" => Access::Basic,
            "basic" => Access::Basic,
            "negotiate" => Access::Negotiate,
            "none" => Access::None,
            o => Access::Other(Box::leak(o.to_string().into_boxed_str())),
        },
        _ => Access::None,
    };
    m.insert(au.clone(), a);
    (a, au)
}

pub fn set_access(url: &str, a: Access) {
    let key = format!("lfs.{url}.access");
    crate::trace!("setting repository access to {}", a.mode());
    let f = finder();
    let mut m = f.access.lock().unwrap();
    match a {
        Access::None => {
            let _ = cfg().unset_local_key(&key);
            m.insert(url.to_string(), Access::None);
        }
        _ => {
            let _ = cfg().set_local(&key, a.mode());
            m.insert(url.to_string(), a);
        }
    }
}

