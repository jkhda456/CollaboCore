//! Credentials (creds/creds.go, netrc.go): the helpers asked in turn — ~/.netrc, the
//! in-process cache, GIT_ASKPASS (core.askpass, SSH_ASKPASS) when no credential.helper is
//! set, and `git credential fill/approve/reject` — with git's credential protocol.

use crate::config::{self, cfg};
use crate::errors::{Error, Result};
use crate::gourl;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

pub type Creds = BTreeMap<String, Vec<String>>;

pub fn first(c: &Creds, k: &str) -> String {
    c.get(k).and_then(|v| v.first()).cloned().unwrap_or_default()
}

pub fn is_multistage(c: &Creds) -> bool {
    matches!(first(c, "continue").as_str(), "1" | "true")
}

/// Why a helper gave nothing: it has nothing to say (credHelperNoOp), or it failed.
pub enum HelperErr {
    NoOp,
    Err(Error),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Helper {
    Netrc,
    Cache,
    AskPass,
    Command,
    Null,
}

struct Machine {
    name: String,
    login: String,
    password: String,
    default: bool,
}

struct Context {
    netrc: Option<Vec<Machine>>,
    netrc_skip: Mutex<HashSet<String>>,
    askpass: String,
    cache: Option<Mutex<HashMap<String, Creds>>>,
    skip_prompt: bool,
    www_auth: Mutex<Vec<String>>,
    state: Mutex<Vec<String>>,
}

fn ctx() -> &'static Context {
    static C: OnceLock<Context> = OnceLock::new();
    C.get_or_init(|| {
        let os = &cfg().os;
        let g = cfg().git();
        let askpass = os.get("GIT_ASKPASS").or_else(|| g.get("core.askpass")).or_else(|| os.get("SSH_ASKPASS")).unwrap_or_default();
        Context {
            netrc: parse_netrc(),
            netrc_skip: Mutex::new(HashSet::new()),
            askpass,
            cache: g.bool("lfs.cachecredentials", true).then(|| Mutex::new(HashMap::new())),
            skip_prompt: os.bool("GIT_TERMINAL_PROMPT", false),
            www_auth: Mutex::new(vec![]),
            state: Mutex::new(vec![]),
        }
    })
}

/// ~/.netrc's machines (None when it cannot be parsed: no netrc helper then).
fn parse_netrc() -> Option<Vec<Machine>> {
    let home = cfg().os.get("HOME").unwrap_or_default();
    if home.is_empty() {
        return Some(vec![]);
    }
    let path = format!("{home}/.netrc");
    let Ok(text) = std::fs::read_to_string(&path) else { return Some(vec![]) };
    let mut machines: Vec<Machine> = vec![];
    let mut in_macro = false;
    let mut pending: Option<&str> = None;
    for line in text.lines() {
        if in_macro {
            // A macro definition runs to the next blank line.
            if line.trim().is_empty() {
                in_macro = false;
            }
            continue;
        }
        for t in line.split_whitespace() {
            if let Some(k) = pending.take() {
                match k {
                    "machine" => machines.push(Machine { name: t.into(), login: String::new(), password: String::new(), default: false }),
                    "login" => {
                        if let Some(m) = machines.last_mut() {
                            m.login = t.into();
                        }
                    }
                    "password" => {
                        if let Some(m) = machines.last_mut() {
                            m.password = t.into();
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match t {
                "machine" | "login" | "password" | "account" => pending = Some(t),
                "default" => machines.push(Machine { name: String::new(), login: String::new(), password: String::new(), default: true }),
                "macdef" => {
                    in_macro = true;
                    break;
                }
                _ => {}
            }
        }
    }
    if pending == Some("machine") {
        crate::trace!("bad netrc file {}: {}", path, "syntax error");
        return None;
    }
    Some(machines)
}

fn netrc_find<'a>(ms: &'a [Machine], host: &str, login: &str) -> Option<&'a Machine> {
    let mut def = None;
    for m in ms {
        if !m.default && m.name == host {
            if !login.is_empty() && m.login != login {
                continue;
            }
            return Some(m);
        }
        if m.default {
            def = Some(m);
        }
    }
    def
}

fn netrc_host(h: &str) -> std::result::Result<String, ()> {
    if h.contains(':') {
        // net.SplitHostPort: one colon, or a bracketed IPv6 host.
        let (host, _) = h.rsplit_once(':').unwrap();
        let ok = if host.starts_with('[') { host.ends_with(']') } else { !host.contains(':') };
        if !ok {
            crate::trace!("netrc: error parsing {}: address {}: too many colons in address", crate::tools::quote(h), h);
            return Err(());
        }
        Ok(host.trim_start_matches('[').trim_end_matches(']').to_string())
    } else {
        Ok(h.to_string())
    }
}

fn cache_key(c: &Creds) -> String {
    [first(c, "protocol"), first(c, "host"), first(c, "path")].join("//")
}

/// A credential helper chain for one URL, with the input the helpers are asked with.
pub struct Wrapper {
    pub helpers: Vec<Helper>,
    pub input: Creds,
    pub url: String,
    pub creds: Option<Creds>,
    pub protect_protocol: bool,
    skipped: Mutex<HashSet<Helper>>,
}

/// GetCredentialHelper.
pub fn wrapper_for(u: &gourl::Url) -> Wrapper {
    let c = ctx();
    let rawurl = format!("{}://{}{}", u.scheme, u.host, u.path);
    let mut input = Creds::new();
    input.insert("protocol".into(), vec![u.scheme.clone()]);
    input.insert("host".into(), vec![u.host.clone()]);
    if let Some(n) = u.username().filter(|n| !n.is_empty()) {
        input.insert("username".into(), vec![n.to_string()]);
    }
    if u.scheme == "cert" || config::url_bool("credential", &rawurl, "usehttppath", false) {
        input.insert("path".into(), vec![u.path.strip_prefix('/').unwrap_or(&u.path).to_string()]);
    }
    let www = c.www_auth.lock().unwrap().clone();
    if !www.is_empty() && !config::url_bool("credential", &rawurl, "skipwwwauth", false) {
        input.insert("wwwauth[]".into(), www);
    }
    let st = c.state.lock().unwrap().clone();
    if !st.is_empty() {
        input.insert("state[]".into(), st);
    }
    let mut helpers = vec![];
    if c.netrc.is_some() {
        helpers.push(Helper::Netrc);
    }
    if c.cache.is_some() {
        helpers.push(Helper::Cache);
    }
    if !c.askpass.is_empty() && config::url_get("credential", &rawurl, "helper").unwrap_or_default().is_empty() {
        helpers.push(Helper::AskPass);
    }
    helpers.push(Helper::Command);
    Wrapper { helpers, input, url: u.to_string_go(), creds: None, protect_protocol: config::url_bool("credential", &rawurl, "protectProtocol", true), skipped: Mutex::new(HashSet::new()) }
}

pub fn null_wrapper() -> Wrapper {
    Wrapper { helpers: vec![Helper::Null], input: Creds::new(), url: String::new(), creds: None, protect_protocol: true, skipped: Mutex::new(HashSet::new()) }
}

pub fn set_www_auth_headers(h: Vec<String>) {
    *ctx().www_auth.lock().unwrap() = h;
}

pub fn set_state_fields(f: Vec<String>) {
    *ctx().state.lock().unwrap() = f;
}

impl Wrapper {
    fn skipped(&self, h: Helper) -> bool {
        self.skipped.lock().unwrap().contains(&h)
    }
    fn skip(&self, h: Helper) {
        self.skipped.lock().unwrap().insert(h);
    }

    /// FillCreds: the first helper's credentials, else an error naming the URL.
    pub fn fill_creds(&mut self) -> Result<()> {
        let r = self.fill(&self.input.clone());
        match r {
            Ok(Some(c)) if !c.is_empty() => {
                self.creds = Some(c);
                Ok(())
            }
            other => {
                let mut msg = format!("Git credentials for {} not found", self.url);
                match other {
                    Err(e) => msg = format!("{msg}:\n{e}"),
                    _ => msg.push('.'),
                }
                self.creds = None;
                Err(Error::new(msg))
            }
        }
    }

    pub fn fill(&self, what: &Creds) -> Result<Option<Creds>> {
        if self.helpers == [Helper::Null] {
            return Err(Error::new("No credential helper configured"));
        }
        let mut errs: Vec<String> = vec![];
        for &h in &self.helpers {
            if self.skipped(h) {
                continue;
            }
            match helper_fill(h, what, self.protect_protocol) {
                Ok(Some(c)) => return Ok(Some(c)),
                Ok(None) => {}
                Err(HelperErr::NoOp) => {}
                Err(HelperErr::Err(e)) => {
                    self.skip(h);
                    crate::trace!("credential fill error: {}", e);
                    errs.push(e.to_string());
                }
            }
        }
        if errs.is_empty() {
            return Ok(None);
        }
        Err(Error::new(format!("credential fill errors:\n{}", errs.join("\n"))))
    }

    pub fn approve(&self) {
        let Some(c) = &self.creds else { return };
        if self.helpers == [Helper::Null] {
            return;
        }
        for (i, &h) in self.helpers.iter().enumerate() {
            if self.skipped(h) {
                continue;
            }
            match helper_approve(h, c, self.protect_protocol) {
                Err(HelperErr::NoOp) => continue,
                Err(HelperErr::Err(_)) if i > 0 => {
                    for &j in &self.helpers[..i] {
                        if !self.skipped(j) {
                            let _ = helper_reject(j, c, self.protect_protocol);
                        }
                    }
                    return;
                }
                _ => return,
            }
        }
    }

    pub fn reject(&self) {
        let Some(c) = &self.creds else { return };
        if self.helpers == [Helper::Null] {
            return;
        }
        for &h in &self.helpers {
            if self.skipped(h) {
                continue;
            }
            match helper_reject(h, c, self.protect_protocol) {
                Err(HelperErr::NoOp) => continue,
                _ => return,
            }
        }
    }
}

fn helper_fill(h: Helper, what: &Creds, protect: bool) -> std::result::Result<Option<Creds>, HelperErr> {
    let c = ctx();
    match h {
        Helper::Null => Err(HelperErr::Err(Error::new("No credential helper configured"))),
        Helper::Netrc => {
            let Ok(host) = netrc_host(&first(what, "host")) else { return Err(HelperErr::NoOp) };
            if c.netrc_skip.lock().unwrap().contains(&host) {
                return Err(HelperErr::NoOp);
            }
            let ms = c.netrc.as_ref().unwrap();
            match netrc_find(ms, &host, &first(what, "username")) {
                Some(m) => {
                    let mut cr = Creds::new();
                    cr.insert("username".into(), vec![m.login.clone()]);
                    cr.insert("password".into(), vec![m.password.clone()]);
                    for k in ["protocol", "host", "scheme", "path"] {
                        if let Some(v) = what.get(k) {
                            cr.insert(k.into(), v.clone());
                        }
                    }
                    cr.insert("source".into(), vec!["netrc".into()]);
                    crate::trace!(
                        "netrc: git credential fill ({}, {}, {}, {})",
                        crate::tools::quote(&first(what, "protocol")),
                        crate::tools::quote(&first(what, "host")),
                        crate::tools::quote(&m.login),
                        crate::tools::quote(&first(what, "path"))
                    );
                    Ok(Some(cr))
                }
                None => Err(HelperErr::NoOp),
            }
        }
        Helper::Cache => {
            let cache = c.cache.as_ref().unwrap().lock().unwrap();
            match cache.get(&cache_key(what)) {
                Some(cr) => {
                    crate::trace!(
                        "creds: git credential cache ({}, {}, {})",
                        crate::tools::quote(&first(what, "protocol")),
                        crate::tools::quote(&first(what, "host")),
                        crate::tools::quote(&first(what, "path"))
                    );
                    Ok(Some(cr.clone()))
                }
                None => Err(HelperErr::NoOp),
            }
        }
        Helper::AskPass => askpass_fill(what).map(Some).map_err(HelperErr::Err),
        Helper::Command => {
            crate::trace!(
                "creds: git credential fill ({}, {}, {})",
                crate::tools::quote(&first(what, "protocol")),
                crate::tools::quote(&first(what, "host")),
                crate::tools::quote(&first(what, "path"))
            );
            command_exec("fill", what, protect).map_err(HelperErr::Err)
        }
    }
}

fn helper_approve(h: Helper, what: &Creds, protect: bool) -> std::result::Result<(), HelperErr> {
    let c = ctx();
    match h {
        Helper::Null => Ok(()),
        Helper::Netrc => {
            if first(what, "source") != "netrc" {
                return Err(HelperErr::NoOp);
            }
            let Ok(host) = netrc_host(&first(what, "host")) else { return Err(HelperErr::NoOp) };
            crate::trace!(
                "netrc: git credential approve ({}, {}, {})",
                crate::tools::quote(&first(what, "protocol")),
                crate::tools::quote(&first(what, "host")),
                crate::tools::quote(&first(what, "path"))
            );
            c.netrc_skip.lock().unwrap().remove(&host);
            Ok(())
        }
        Helper::Cache => {
            let mut cache = c.cache.as_ref().unwrap().lock().unwrap();
            let k = cache_key(what);
            if cache.contains_key(&k) {
                return Ok(());
            }
            cache.insert(k, what.clone());
            Err(HelperErr::NoOp)
        }
        Helper::AskPass => Ok(()),
        Helper::Command => {
            crate::trace!(
                "creds: git credential approve ({}, {}, {})",
                crate::tools::quote(&first(what, "protocol")),
                crate::tools::quote(&first(what, "host")),
                crate::tools::quote(&first(what, "path"))
            );
            command_exec("approve", what, protect).map(|_| ()).map_err(HelperErr::Err)
        }
    }
}

fn helper_reject(h: Helper, what: &Creds, protect: bool) -> std::result::Result<(), HelperErr> {
    let c = ctx();
    match h {
        Helper::Null => Ok(()),
        Helper::Netrc => {
            if first(what, "source") != "netrc" {
                return Err(HelperErr::NoOp);
            }
            let Ok(host) = netrc_host(&first(what, "host")) else { return Err(HelperErr::NoOp) };
            crate::trace!(
                "netrc: git credential reject ({:?}, {:?}, {:?})",
                what.get("protocol").cloned().unwrap_or_default(),
                what.get("host").cloned().unwrap_or_default(),
                what.get("path").cloned().unwrap_or_default()
            );
            c.netrc_skip.lock().unwrap().insert(host);
            Ok(())
        }
        Helper::Cache => {
            c.cache.as_ref().unwrap().lock().unwrap().remove(&cache_key(what));
            Err(HelperErr::NoOp)
        }
        Helper::AskPass => Ok(()),
        Helper::Command => command_exec("reject", what, protect).map(|_| ()).map_err(HelperErr::Err),
    }
}

fn askpass_fill(what: &Creds) -> Result<Creds> {
    let mut u = gourl::Url { scheme: first(what, "protocol"), host: first(what, "host"), path: first(what, "path"), ..Default::default() };
    let mut cr = Creds::new();
    let username = match what.get("username").and_then(|v| v.first()) {
        Some(n) => n.clone(),
        None => askpass_program("Username", &u)?,
    };
    cr.insert("username".into(), vec![username.clone()]);
    if !username.is_empty() {
        u.user = Some((username, None));
    }
    let password = match what.get("password").and_then(|v| v.first()) {
        Some(p) => p.clone(),
        None => askpass_program("Password", &u)?,
    };
    cr.insert("password".into(), vec![password]);
    Ok(cr)
}

fn askpass_program(what: &str, u: &gourl::Url) -> Result<String> {
    let prog = &ctx().askpass;
    let prompt = format!("{what} for {}", crate::tools::quote(&u.to_string_go()));
    crate::trace!("creds: filling with GIT_ASKPASS: {} {}", prog, prompt);
    let out = crate::subprocess::command(prog, &[&prompt]).stdin(std::process::Stdio::null()).output().map_err(|e| {
        crate::trace!("creds: failed to find GIT_ASKPASS command: {}", prog);
        Error::new(crate::tools::io_err(&e))
    })?;
    if !out.status.success() {
        return Err(Error::new(crate::subprocess::exit_text(&out.status)));
    }
    if !out.stderr.is_empty() {
        return Err(Error::new(String::from_utf8_lossy(&out.stderr).into_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn creds_buffer(c: &Creds, protect: bool) -> Result<String> {
    let mut s = String::from("capability[]=authtype\ncapability[]=state\n");
    for (k, vs) in c {
        for v in vs {
            if v.contains('\n') {
                return Err(Error::new(format!("credential value for {k} contains newline: {}", crate::tools::quote(v))));
            }
            if protect && v.contains('\r') {
                return Err(Error::new(format!(
                    "credential value for {k} contains carriage return: {}\nIf this is intended, set `credential.protectProtocol=false`",
                    crate::tools::quote(v)
                )));
            }
            if v.contains('\0') {
                return Err(Error::new(format!("credential value for {k} contains null byte: {}", crate::tools::quote(v))));
            }
            s.push_str(&format!("{k}={v}\n"));
        }
    }
    Ok(s)
}

fn command_exec(sub: &str, input: &Creds, protect: bool) -> Result<Option<Creds>> {
    let buf = creds_buffer(input, protect).map_err(|e| Error::new(format!("invalid input to `git credential {sub}`: {e}")))?;
    let mut child = crate::subprocess::command("git", &["credential", sub])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| Error::new(format!("failed to find `git credential {sub}`: {}", crate::tools::io_err(&e))))?;
    {
        let mut si = child.stdin.take().unwrap();
        let _ = si.write_all(buf.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| Error::new(format!("`git credential {sub}` error: {}", crate::tools::io_err(&e))))?;
    if !out.status.success() {
        if ctx().skip_prompt {
            return Err(Error::new(format!(
                "change the GIT_TERMINAL_PROMPT env var to be prompted to enter your credentials for {}://{}",
                first(input, "protocol"),
                first(input, "host")
            )));
        }
        if sub == "fill" && out.status.code() == Some(128) {
            return Ok(None);
        }
        return Err(Error::new(format!("`git credential {sub}` error: {}", crate::subprocess::exit_text(&out.status))));
    }
    let mut c = Creds::new();
    for line in String::from_utf8_lossy(&out.stdout).split('\n') {
        let Some((k, v)) = line.split_once('=') else { continue };
        if v.is_empty() {
            continue;
        }
        c.entry(k.to_string()).or_default().push(v.to_string());
    }
    Ok(Some(c))
}

/// The password of an encrypted client key (decryptPEMBlock), from the helpers for
/// cert:///PATH.
pub fn cert_password(path: &str) -> Result<String> {
    let fileurl = format!("cert:///{path}");
    let u = gourl::parse(&fileurl).map_err(|e| Error::new(e.to_string()))?;
    let mut w = wrapper_for(&u);
    w.input.insert("username".into(), vec![String::new()]);
    let input = w.input.clone();
    match w.fill(&input) {
        Ok(Some(c)) => {
            let p = first(&c, "password");
            w.creds = Some(c);
            w.approve();
            Ok(p)
        }
        Ok(None) => Err(Error::new("no password")),
        Err(e) => {
            crate::trace!("Error filling credentials for {}: {}", crate::tools::quote(&fileurl), e);
            Err(e)
        }
    }
}
