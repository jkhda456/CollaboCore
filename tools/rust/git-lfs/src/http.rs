//! The HTTP client (lfshttp): requests to the LFS API and the storage, sent with libcurl but
//! shaped like git-lfs's Go client: its headers (User-Agent, Accept, http.<url>.extraHeader),
//! redirects followed by hand, errors from the status code and the JSON body, proxies,
//! certificates and timeouts from the git configuration, and its tracing (GIT_TRACE,
//! GIT_CURL_VERBOSE's dumps, GIT_LOG_STATS).

use crate::config::{self, cfg};
use crate::curl;
use crate::endpoint::Endpoint;
use crate::errors::{Error, Result};
use crate::gourl;
use libc::{c_char, c_void, size_t};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};

pub const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
pub const REQUEST_CONTENT_TYPE: &str = "application/vnd.git-lfs+json; charset=utf-8";

/// textproto.CanonicalMIMEHeaderKey: "content-type" → "Content-Type".
pub fn canonical_key(k: &str) -> String {
    if k.bytes().any(|c| !(c.is_ascii_alphanumeric() || c == b'-')) {
        return k.to_string();
    }
    let mut up = true;
    k.chars()
        .map(|c| {
            let r = if up { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
            up = c == '-';
            r
        })
        .collect()
}

/// http.Header: canonical keys, several values each, in the order set.
#[derive(Clone, Debug, Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn get(&self, k: &str) -> String {
        let k = canonical_key(k);
        self.0.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }
    pub fn values(&self, k: &str) -> Vec<String> {
        let k = canonical_key(k);
        self.0.iter().filter(|(a, _)| *a == k).map(|(_, v)| v.clone()).collect()
    }
    pub fn has(&self, k: &str) -> bool {
        let k = canonical_key(k);
        self.0.iter().any(|(a, _)| *a == k)
    }
    pub fn set(&mut self, k: &str, v: &str) {
        let k = canonical_key(k);
        match self.0.iter().position(|(a, _)| *a == k) {
            Some(i) => {
                self.0[i].1 = v.to_string();
                let mut n = 0;
                self.0.retain(|(a, _)| {
                    n += 1;
                    n - 1 <= i || *a != k
                });
            }
            None => self.0.push((k, v.to_string())),
        }
    }
    pub fn add(&mut self, k: &str, v: &str) {
        self.0.push((canonical_key(k), v.to_string()));
    }
    pub fn del(&mut self, k: &str) {
        let k = canonical_key(k);
        self.0.retain(|(a, _)| *a != k);
    }
    /// Keys sorted, as Header.Write prints them.
    fn sorted(&self, exclude: &[&str]) -> Vec<(String, String)> {
        let mut keys: Vec<&String> = self.0.iter().map(|(k, _)| k).filter(|k| !exclude.contains(&k.as_str())).collect();
        keys.sort();
        keys.dedup();
        let mut out = vec![];
        for k in keys {
            for (a, v) in &self.0 {
                if a == k {
                    out.push((k.clone(), v.clone()));
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub enum Body {
    Bytes(Vec<u8>),
    /// `len` bytes of a file from `offset`.
    File { path: String, offset: u64, len: u64 },
}

impl Body {
    fn len(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::File { len, .. } => *len,
        }
    }
    fn open(&self) -> std::io::Result<Box<dyn Read>> {
        Ok(match self {
            Body::Bytes(b) => Box::new(std::io::Cursor::new(b.clone())),
            Body::File { path, offset, len } => {
                let mut f = std::fs::File::open(path)?;
                if *offset > 0 {
                    use std::io::Seek;
                    f.seek(std::io::SeekFrom::Start(*offset))?;
                }
                Box::new(f.take(*len))
            }
        })
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub header: Headers,
    pub body: Option<Body>,
    /// lfshttp.WithRetries: tries again after a transport error (not after a status).
    pub retries: Option<i64>,
    /// The GIT_LOG_STATS key (LogRequest).
    pub stats_key: Option<String>,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Request {
        Request { method: method.into(), url: url.into(), header: Headers::default(), body: None, retries: None, stats_key: None }
    }
    pub fn with_json(mut self, v: &impl serde::Serialize) -> Request {
        let b = serde_json::to_vec(v).unwrap_or_default();
        self.header.set("Content-Length", &b.len().to_string());
        self.body = Some(Body::Bytes(b));
        self
    }
    /// The URL without its query, as traces print it.
    pub fn url_no_query(&self) -> &str {
        self.url.split('?').next().unwrap_or("")
    }
    pub fn parsed_url(&self) -> gourl::Url {
        gourl::parse(&self.url).unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u32,
    pub proto: String,
    pub reason: String,
    pub header: Headers,
    pub body: Vec<u8>,
    pub method: String,
    pub url: String,
    pub uncompressed: bool,
}

/// What a request does with its body and response beyond the request itself.
#[derive(Default)]
pub struct Hooks<'a> {
    /// The body of a 2xx response goes here (instead of Response::body); an error aborts it.
    pub sink: Option<&'a mut dyn FnMut(&Response, &[u8]) -> std::result::Result<(), String>>,
    /// The request body read so far changed by this many bytes (negative: reset).
    pub progress: Option<&'a mut dyn FnMut(i64)>,
    /// Before the request body's first byte is sent (startCallbackReader).
    pub on_start: Option<&'a mut dyn FnMut()>,
}

pub struct Client {
    pub skip_ssl_verify: bool,
    pub verbose: bool,
    pub debugging_verbose: bool,
    dial_timeout: i64,
    keepalive_timeout: i64,
    ssh_tries: i64,
    ssh_cache: Option<Mutex<HashMap<String, SshAuthResponse>>>,
    stats: Mutex<Option<std::fs::File>>,
}

pub fn client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| {
        let g = cfg().git();
        let os = &cfg().os;
        Client {
            skip_ssl_verify: !g.bool("http.sslverify", true) || os.bool("GIT_SSL_NO_VERIFY", false),
            verbose: os.bool("GIT_CURL_VERBOSE", false),
            debugging_verbose: os.bool("LFS_DEBUG_HTTP", false),
            dial_timeout: g.int("lfs.dialtimeout", 0),
            keepalive_timeout: g.int("lfs.keepalive", 0),
            ssh_tries: g.int("lfs.ssh.retries", 5),
            ssh_cache: g.bool("lfs.cachecredentials", true).then(|| Mutex::new(HashMap::new())),
            stats: Mutex::new(STATS_LOG.lock().unwrap().take()),
        }
    })
}

static STATS_LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// LogHTTPStats: the stats file, with its first line.
pub fn set_stats_log(mut f: std::fs::File) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let _ = writeln!(f, "concurrent={} time={} version={}", cfg().git().int("lfs.concurrenttransfers", 8), secs, crate::commands::version_desc());
    *STATS_LOG.lock().unwrap() = Some(f);
}

pub fn user_agent() -> String {
    crate::commands::version_desc()
}

fn verbose_out(s: &str) {
    let _ = std::io::stderr().write_all(s.as_bytes());
}

fn is_traceable_content(h: &Headers) -> bool {
    let ct = h.get("Content-Type");
    let ct = ct.split(';').next().unwrap_or("").to_lowercase();
    ["json", "text", "xml", "html"].iter().any(|t| ct.contains(t))
}

/// url.Error's Op: "Post", "Get", ...
fn url_error_op(method: &str) -> String {
    let mut c = method.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c.as_str().to_lowercase().chars()).collect(),
        None => String::new(),
    }
}

impl Client {
    /// NewRequest: METHOD on the endpoint's URL (or what git-lfs-authenticate gave) + suffix.
    pub fn new_request(&self, method: &str, e: &Endpoint, suffix: &str, body: Option<&serde_json::Value>) -> Result<Request> {
        if e.url.starts_with("file://") {
            eprint!("\nhint: The remote resolves to a file:// URL, which can only work with a\nhint: standalone transfer agent.  See section \"Using a Custom Transfer Type\nhint: without the API server\" in custom-transfers.md for details.\n");
        }
        let ssh = self.ssh_resolve_with_retries(e, method)?;
        let prefix = if ssh.href.is_empty() { e.url.clone() } else { ssh.href.clone() };
        if !(prefix.starts_with("http://") || prefix.starts_with("https://")) {
            let frag = prefix.split('?').next().unwrap_or("");
            return Err(Error::new(format!("missing protocol: {}", crate::tools::quote(frag))));
        }
        let url = if prefix.ends_with('/') { format!("{prefix}{suffix}") } else { format!("{prefix}/{suffix}") };
        let mut req = Request::new(method, &url);
        for (k, v) in &ssh.header {
            req.header.set(k, v);
        }
        req.header.set("Accept", MEDIA_TYPE);
        if let Some(b) = body {
            req = req.with_json(b);
            req.header.set("Content-Type", REQUEST_CONTENT_TYPE);
        }
        Ok(req)
    }

    /// ExtraHeadersFor: http.<url>.extraHeader values the request does not have yet.
    pub fn apply_extra_headers(&self, req: &mut Request) {
        for hdr in config::url_get_all("http", &req.url, "extraHeader") {
            let Some((k, v)) = hdr.split_once(':') else { continue };
            let k = canonical_key(k);
            let v = v.trim();
            if !req.header.values(&k).iter().any(|x| x == v) {
                req.header.add(&k, v);
            }
        }
    }

    /// Do: the request with no credentials; an error for a status of 400 and up.
    pub fn do_(&self, req: &mut Request, hooks: &mut Hooks) -> (Option<Response>, Option<Error>) {
        self.apply_extra_headers(req);
        self.do_with_redirects(req.clone(), hooks)
    }

    fn do_with_redirects(&self, mut req: Request, hooks: &mut Hooks) -> (Option<Response>, Option<Error>) {
        let mut via = 0;
        loop {
            req.header.set("User-Agent", &user_agent());
            match self.do_with_redirect(&req, &mut via, hooks) {
                Redirect::Done(res, err) => return (res, err),
                Redirect::To(r) => req = r,
            }
        }
    }

    /// DoWithRedirect: one request (tried again after transport errors as its retries
    /// allow); a redirect gives the request to follow.
    pub fn do_with_redirect(&self, req: &Request, via: &mut usize, hooks: &mut Hooks) -> Redirect {
        let retries = req.retries.unwrap_or(0).max(0);
        // traceRequest: once, whatever the retries.
        crate::trace!("HTTP: {} {}", req.method, req.url_no_query());
        if self.verbose {
            let u = req.parsed_url();
            let gzip = !req.header.has("Accept-Encoding") && !req.header.has("Range") && req.method != "HEAD";
            self.dump_request(req, &u, gzip, req.header.get("Transfer-Encoding").eq_ignore_ascii_case("chunked"));
        }
        let mut last = None;
        for _ in 0..=retries {
            match self.perform(req, hooks) {
                Ok(res) => {
                    last = Some(Ok(res));
                    break;
                }
                Err(e) => last = Some(Err(e)),
            }
        }
        let res = match last.unwrap() {
            Ok(r) => r,
            Err(e) => return Redirect::Done(None, Some(e)),
        };
        if ![301, 302, 303, 307, 308].contains(&res.status) {
            let err = handle_response(&res);
            return Redirect::Done(Some(res), err);
        }
        let mut to = res.header.get("Location");
        if let Ok(loc) = gourl::parse(&to) {
            if loc.scheme.is_empty() {
                to = resolve_reference(&req.url, &to);
            }
        }
        *via += 1;
        if *via >= 3 {
            return Redirect::Done(Some(res), Some(Error::new("too many redirects")));
        }
        match new_request_for_retry(req, &to) {
            Ok(r) => Redirect::To(r),
            Err(e) => Redirect::Done(Some(res), Some(e)),
        }
    }

    /// One exchange through libcurl, traced as the Go client traces it.
    fn perform(&self, req: &Request, hooks: &mut Hooks) -> Result<Response> {
        let u = req.parsed_url();
        let host = u.host.clone();
        let gzip = !req.header.has("Accept-Encoding") && !req.header.has("Range") && req.method != "HEAD";
        let chunked = req.header.get("Transfer-Encoding").eq_ignore_ascii_case("chunked");
        let mut st = State {
            res: Response { method: req.method.clone(), url: req.url.clone(), ..Default::default() },
            head_done: false,
            sink: hooks.sink.as_deref_mut(),
            progress: hooks.progress.as_deref_mut(),
            on_start: hooks.on_start.as_deref_mut(),
            reader: None,
            sink_err: None,
            read_err: None,
            verbose: self.verbose,
            debugging_verbose: self.debugging_verbose,
            trace_req_body: self.verbose && is_traceable_content(&req.header),
            trace_body: false,
            verbose_body: false,
            sent: 0,
            received: 0,
            gzip_requested: gzip,
        };
        if let Some(b) = &req.body {
            st.reader = Some(b.open().map_err(|e| Error::new(crate::tools::io_err(&e)))?);
        }
        let started = std::time::Instant::now();
        let (code, times) = curl::with_handle(|h| {
            h.str(curl::URL, &req.url);
            let mut hdrs: Vec<String> = vec![];
            for (k, v) in &req.header.0 {
                if k == "Content-Length" || k == "Transfer-Encoding" {
                    continue;
                }
                hdrs.push(format!("{k}: {v}"));
            }
            for k in ["Accept", "Expect", "Content-Type"] {
                if !req.header.has(k) {
                    hdrs.push(format!("{k}:"));
                }
            }
            if chunked {
                hdrs.push("Transfer-Encoding: chunked".into());
            }
            h.list(curl::HTTPHEADER, &hdrs);
            match (&req.body, req.method.as_str()) {
                (Some(b), m) => {
                    h.long(curl::UPLOAD, 1);
                    if !chunked {
                        h.off(curl::INFILESIZE_LARGE, b.len() as i64);
                    }
                    if m != "PUT" {
                        h.str(curl::CUSTOMREQUEST, m);
                    }
                }
                (None, "GET") => h.long(curl::HTTPGET, 1),
                (None, "HEAD") => h.long(curl::NOBODY, 1),
                (None, m) => h.str(curl::CUSTOMREQUEST, m),
            }
            if gzip {
                h.str(curl::ACCEPT_ENCODING, "gzip");
            } else {
                h.long(curl::HTTP_CONTENT_DECODING, 0);
            }
            if let Err(e) = self.configure(h, &u) {
                return (Err(e), [0i64; 4]);
            }
            let sp = &mut st as *mut State as *mut c_void;
            h.write_function(write_cb, sp);
            h.header_function(header_cb, sp);
            h.read_function(read_cb, sp);
            let code = h.perform();
            let times = [
                h.info_off(curl::INFO_NAMELOOKUP_TIME_T),
                h.info_off(curl::INFO_CONNECT_TIME_T),
                h.info_off(curl::INFO_APPCONNECT_TIME_T),
                h.info_off(curl::INFO_STARTTRANSFER_TIME_T),
            ];
            (Ok(code), times)
        });
        let code = code?;
        let elapsed = started.elapsed().as_nanos() as i64;
        self.log_stats(req, st.sent, None, times, elapsed);
        if code != 0 {
            if let Some(e) = st.sink_err.take() {
                return Err(Error::new(e));
            }
            if let Some(e) = st.read_err.take() {
                return Err(Error::new(format!("{} {}: {}", url_error_op(&req.method), crate::tools::quote(&req.url), e)));
            }
            return Err(Error::new(format!("{} {}: {}", url_error_op(&req.method), crate::tools::quote(&req.url), transport_error(code, &u, &host))));
        }
        if !st.head_done {
            st.finish_head();
        }
        let mut res = st.res;
        self.log_stats(req, st.sent, Some((res.status, st.received)), times, elapsed);
        res.method = req.method.clone();
        res.url = req.url.clone();
        Ok(res)
    }

    fn configure(&self, h: &mut curl::Easy, u: &gourl::Url) -> Result<()> {
        let url = u.to_string_go();
        let host = &u.host;
        // Timeouts: the dial timeout, and the activity timeout as a lowest speed.
        let dial = if self.dial_timeout < 1 { 30 } else { self.dial_timeout };
        h.long(curl::CONNECTTIMEOUT, dial as _);
        let keepalive = if self.keepalive_timeout < 1 { 1800 } else { self.keepalive_timeout };
        h.long(curl::TCP_KEEPALIVE, 1);
        h.long(curl::TCP_KEEPIDLE, keepalive as _);
        let activity = match config::url_get("lfs", &url, "activitytimeout") {
            Some(v) => v.trim().parse::<i64>().unwrap_or(0),
            None => 30,
        };
        if activity > 0 {
            h.long(curl::LOW_SPEED_LIMIT, 1);
            h.long(curl::LOW_SPEED_TIME, activity as _);
        }
        // Proxies (getProxyServers): never for loopback hosts.
        let (https_proxy, http_proxy, no_proxy) = proxy_servers(u);
        let mut proxy = if u.scheme == "https" { https_proxy } else { String::new() };
        if proxy.is_empty() {
            proxy = http_proxy;
        }
        let hn = u.hostname().to_string();
        let loopback = hn == "localhost" || hn.starts_with("127.") || hn == "::1";
        if proxy.is_empty() || loopback {
            h.str(curl::PROXY, "");
        } else {
            let proxy = proxy.replacen("socks5h://", "socks5://", 1);
            let proxy = if proxy.contains("://") { proxy } else { format!("http://{proxy}") };
            h.str(curl::PROXY, &proxy);
            if !no_proxy.is_empty() {
                h.str(curl::NOPROXY, &no_proxy);
            }
        }
        // TLS.
        if u.scheme == "https" {
            let verify_off = config::url_get("http", &format!("https://{host}"), "sslverify").as_deref() == Some("false") || self.skip_ssl_verify;
            if verify_off {
                h.long(curl::SSL_VERIFYPEER, 0);
                h.long(curl::SSL_VERIFYHOST, 0);
            } else {
                self.configure_cas(h, host);
            }
        }
        // A client certificate (configured by host, whatever the scheme).
        let hostu = format!("https://{host}/");
        if let (Some(key), Some(cert)) = (config::url_get("http", &hostu, "sslKey"), config::url_get("http", &hostu, "sslCert")) {
            crate::trace!("http: client cert for {}", host);
            let key = crate::tools::expand_path(&key).map_err(|e| e.wrap(format!("Error resolving key path {}", crate::tools::quote(&key))))?;
            let cert = crate::tools::expand_path(&cert).map_err(|e| e.wrap(format!("Error resolving cert path {}", crate::tools::quote(&cert))))?;
            if let Err(e) = std::fs::read(&cert) {
                crate::trace!("Error reading client cert file {}: {}", crate::tools::quote(&cert), e);
                return Err(crate::tools::path_err("open", &cert, &e).wrap(format!("Error reading client cert file {}", crate::tools::quote(&cert))));
            }
            let keydata = match std::fs::read_to_string(&key) {
                Ok(k) => k,
                Err(e) => {
                    crate::trace!("Error reading client key file {}: {}", crate::tools::quote(&key), e);
                    return Err(crate::tools::path_err("open", &key, &e).wrap(format!("Error reading client key file {}", crate::tools::quote(&key))));
                }
            };
            if !keydata.contains("-----BEGIN ") {
                return Err(Error::new(format!("Error decoding PEM block from {}", crate::tools::quote(&key))));
            }
            h.str(curl::SSLCERT, &cert);
            h.str(curl::SSLKEY, &key);
            if keydata.contains("ENCRYPTED") {
                let pass = crate::creds::cert_password(&key).map_err(|e| {
                    crate::trace!("Unable to decrypt client key file {}: {}", crate::tools::quote(&key), e);
                    e.wrap(format!("Error reading client key file {} (not a PKCS#1 file?)", crate::tools::quote(&key)))
                })?;
                h.str(curl::KEYPASSWD, &pass);
            }
        }
        // HTTP version.
        match config::url_get("http", &url, "version").unwrap_or_default().as_str() {
            "HTTP/1.1" => h.long(curl::HTTP_VERSION, curl::HTTP_VERSION_1_1),
            "HTTP/2" => {
                if u.scheme != "https" {
                    return Err(Error::new("HTTP/2 cannot be used except with TLS"));
                }
                h.long(curl::HTTP_VERSION, curl::HTTP_VERSION_2TLS)
            }
            "" => {}
            v => return Err(Error::new(format!("Unknown HTTP version {}", crate::tools::quote(v)))),
        }
        // Cookies.
        if let Some(cf) = config::url_get("http", &format!("https://{host}"), "cookieFile") {
            crate::trace!("http: cookieFile for {}", host);
            match crate::tools::expand_path(&cf) {
                Ok(p) => h.str(curl::COOKIEFILE, &p),
                Err(e) => crate::trace!("http: error while reading cookieFile: {}", e),
            }
        }
        Ok(())
    }

    /// getRootCAsForHostFromGitconfig: GIT_SSL_CAINFO, http.<url>.sslcainfo, GIT_SSL_CAPATH,
    /// http.sslcapath (else the system's).
    fn configure_cas(&self, h: &mut curl::Easy, host: &str) {
        let url = format!("https://{host}/");
        let os = &cfg().os;
        if let Some(f) = os.get("GIT_SSL_CAINFO").filter(|s| !s.is_empty()) {
            h.str(curl::CAINFO, &f);
        } else if let Some(f) = config::url_get("http", &url, "sslcainfo") {
            h.str(curl::CAINFO, &f);
        } else if let Some(d) = os.get("GIT_SSL_CAPATH").filter(|s| !s.is_empty()).or_else(|| cfg().git().get("http.sslcapath")) {
            // Go reads every file of the directory, hashed names or not: so a bundle of them.
            let mut bundle = String::new();
            if let Ok(rd) = std::fs::read_dir(&d) {
                let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
                names.sort();
                for p in names {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        bundle.push_str(&s);
                        bundle.push('\n');
                    }
                }
            }
            let path = format!("{}/git-lfs-capath-{}.pem", std::env::temp_dir().display(), std::process::id());
            if std::fs::write(&path, bundle).is_ok() {
                h.str(curl::CAINFO, &path);
                crate::tools::remove_at_exit(&path);
            }
        }
    }

    fn dump_request(&self, req: &Request, u: &gourl::Url, gzip: bool, chunked: bool) {
        let mut path = if u.opaque.is_empty() { u.escaped_path() } else { u.opaque.clone() };
        if path.is_empty() {
            path = "/".into();
        }
        if let Some(q) = &u.raw_query {
            path.push('?');
            path.push_str(q);
        }
        let mut lines = vec![format!("{} {} HTTP/1.1", req.method, path), format!("Host: {}", u.host)];
        let ua = req.header.get("User-Agent");
        lines.push(format!("User-Agent: {}", if ua.is_empty() { "Go-http-client/1.1".into() } else { ua }));
        if chunked {
            lines.push("Transfer-Encoding: chunked".into());
        } else if let Some(b) = &req.body {
            lines.push(format!("Content-Length: {}", b.len()));
        } else if req.method == "POST" || req.method == "PUT" || req.method == "PATCH" {
            lines.push("Content-Length: 0".into());
        }
        for (k, v) in req.header.sorted(&["Host", "User-Agent", "Content-Length", "Transfer-Encoding", "Trailer"]) {
            lines.push(format!("{k}: {v}"));
        }
        if gzip {
            lines.push("Accept-Encoding: gzip".into());
        }
        lines.push(String::new());
        self.dump(">", &lines);
    }

    fn dump(&self, dir: &str, lines: &[String]) {
        let mut out = String::new();
        for l in lines {
            if !self.debugging_verbose && l.to_lowercase().starts_with("authorization: basic") {
                out.push_str(&format!("{dir} Authorization: Basic * * * * *\n"));
            } else {
                out.push_str(&format!("{dir} {l}\n"));
            }
        }
        verbose_out(&out);
    }

    fn log_stats(&self, req: &Request, sent: u64, res: Option<(u32, u64)>, t: [i64; 4], elapsed: i64) {
        let Some(key) = &req.stats_key else { return };
        let mut f = self.stats.lock().unwrap();
        let Some(f) = f.as_mut() else { return };
        let head = format!("key={} event={} url={} method={}", key, if res.is_some() { "response" } else { "request" }, req.url_no_query(), req.method);
        let line = match res {
            None => format!("{head} body={sent}\n"),
            Some((status, body)) => {
                let us = |x: i64| x.max(0) * 1000;
                format!(
                    "{head} status={status} body={body} conntime={} dnstime={} tlstime={} restime={} time={}\n",
                    us(t[1] - t[0]),
                    us(t[0]),
                    us(if t[2] > 0 { t[2] - t[1] } else { 0 }),
                    (elapsed - us(t[3])).max(0),
                    elapsed
                )
            }
        };
        let _ = f.write_all(line.as_bytes());
    }

    /// sshResolveWithRetries: git-lfs-authenticate for an ssh remote's endpoint.
    fn ssh_resolve_with_retries(&self, e: &Endpoint, method: &str) -> Result<SshAuthResponse> {
        if let Some(v) = config::url_get("lfs", &e.original_url, "sshtransfer") {
            if v != "negotiate" && v != "never" {
                crate::trace!("skipping SSH-HTTPS hybrid protocol connection by request");
                return Err(Error::new("git-lfs-authenticate has been disabled by request"));
            }
        }
        let requests = self.ssh_tries.max(0) + 1;
        let mut last = (SshAuthResponse::default(), None);
        for i in 0..requests {
            match self.ssh_resolve_cached(e, method) {
                Ok(r) => return Ok(r),
                Err((res, err, unavailable)) => {
                    if unavailable {
                        crate::trace!("ssh: {} does not provide git-lfs-authenticate, falling back to guessed LFS endpoint", e.ssh.user_and_host);
                        return Ok(SshAuthResponse::default());
                    }
                    crate::trace!("ssh: {} failed, error: {}, message: {} (try: {}/{})", e.ssh.user_and_host, err, res.message, i, requests);
                    last = (res, Some(err));
                }
            }
        }
        let err = last.1.unwrap();
        if !last.0.message.is_empty() {
            return Err(err.wrap(last.0.message));
        }
        Err(err)
    }

    fn ssh_resolve_cached(&self, e: &Endpoint, method: &str) -> std::result::Result<SshAuthResponse, (SshAuthResponse, Error, bool)> {
        if e.ssh.user_and_host.is_empty() {
            return Ok(SshAuthResponse::default());
        }
        let op = endpoint_operation(e, method);
        let key = [e.ssh.user_and_host.as_str(), &e.ssh.port, &e.ssh.path, method].join("//");
        if let Some(c) = &self.ssh_cache {
            if let Some(r) = c.lock().unwrap().get(&key) {
                if !r.expired_within(5) {
                    crate::trace!("ssh cache: {} git-lfs-authenticate {} {}", e.ssh.user_and_host, e.ssh.path, op);
                    return Ok(r.clone());
                }
                crate::trace!("ssh cache expired: {} git-lfs-authenticate {} {}", e.ssh.user_and_host, e.ssh.path, op);
            }
        }
        let r = ssh_authenticate(e, &op)?;
        if let Some(c) = &self.ssh_cache {
            c.lock().unwrap().insert(key, r.clone());
        }
        Ok(r)
    }
}

pub enum Redirect {
    Done(Option<Response>, Option<Error>),
    To(Request),
}

/// newRequestForRetry: the request again at a redirect's location (no Authorization for
/// another host; never from HTTPS to HTTP).
fn new_request_for_retry(req: &Request, location: &str) -> Result<Request> {
    let old = req.parsed_url();
    let new = gourl::parse(location).map_err(|e| Error::new(e.to_string()))?;
    if old.scheme == "https" && new.scheme == "http" {
        return Err(Error::new("refusing insecure redirect: HTTPS to HTTP"));
    }
    let same_host = old.host == new.host;
    let mut r = Request::new(&req.method, location);
    for (k, v) in &req.header.0 {
        if k == "Authorization" && !same_host {
            continue;
        }
        if !r.header.has(k) {
            r.header.set(k, v);
        }
    }
    crate::trace!("api: redirect {} {} to {}", req.method, req.url_no_query(), location.split('?').next().unwrap_or(""));
    r.body = req.body.clone();
    r.retries = req.retries;
    r.stats_key = req.stats_key.clone();
    Ok(r)
}

/// url.URL.ResolveReference for a relative Location.
fn resolve_reference(base: &str, rel: &str) -> String {
    let Ok(b) = gourl::parse(base) else { return rel.to_string() };
    let origin = format!("{}://{}", b.scheme, b.host);
    if rel.starts_with("//") {
        return format!("{}:{}", b.scheme, rel);
    }
    if rel.starts_with('/') {
        return format!("{origin}{rel}");
    }
    let dir = match b.path.rfind('/') {
        Some(i) => &b.path[..=i],
        None => "/",
    };
    format!("{origin}{}", crate::tools::clean_str(&format!("{dir}{rel}")))
}

fn transport_error(code: i32, u: &gourl::Url, host: &str) -> String {
    let port = if u.port().is_empty() { if u.scheme == "https" { "443" } else { "80" } } else { u.port() };
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:{port}") };
    match code {
        curl::E_COULDNT_CONNECT => format!("dial tcp {addr}: connect: connection refused"),
        curl::E_COULDNT_RESOLVE_HOST => format!("dial tcp: lookup {}: no such host", u.hostname()),
        curl::E_OPERATION_TIMEDOUT => format!("read tcp {addr}: i/o timeout"),
        curl::E_PEER_FAILED_VERIFICATION => "tls: failed to verify certificate: x509: certificate signed by unknown authority".into(),
        c => curl::strerror(c),
    }
}

fn proxy_servers(u: &gourl::Url) -> (String, String, String) {
    let os = &cfg().os;
    let get = |k: &str| os.get(k).unwrap_or_default();
    let mut https = get("HTTPS_PROXY");
    if https.is_empty() {
        https = get("https_proxy");
    }
    let mut http = get("HTTP_PROXY");
    if http.is_empty() {
        http = get("http_proxy");
    }
    if let Some(p) = config::url_get("http", &u.to_string_go(), "proxy").filter(|p| !p.is_empty()) {
        if u.scheme == "https" {
            https = p.clone();
        }
        http = p;
    }
    let mut no = get("NO_PROXY");
    if no.is_empty() {
        no = get("no_proxy");
    }
    (https, http, no)
}

struct State<'a, 'b> {
    res: Response,
    head_done: bool,
    sink: Option<&'a mut (dyn FnMut(&Response, &[u8]) -> std::result::Result<(), String> + 'b)>,
    progress: Option<&'a mut (dyn FnMut(i64) + 'b)>,
    on_start: Option<&'a mut (dyn FnMut() + 'b)>,
    reader: Option<Box<dyn Read>>,
    sink_err: Option<String>,
    read_err: Option<String>,
    verbose: bool,
    debugging_verbose: bool,
    trace_req_body: bool,
    trace_body: bool,
    verbose_body: bool,
    sent: u64,
    received: u64,
    gzip_requested: bool,
}

impl State<'_, '_> {
    /// The response's head is complete: trace it (traceResponse).
    fn finish_head(&mut self) {
        self.head_done = true;
        if self.gzip_requested && self.res.header.get("Content-Encoding").eq_ignore_ascii_case("gzip") {
            self.res.header.del("Content-Encoding");
            self.res.header.del("Content-Length");
            self.res.uncompressed = true;
            crate::trace!("http: decompressed gzipped response");
        }
        crate::trace!("HTTP: {}", self.res.status);
        let traceable = is_traceable_content(&self.res.header);
        let redirect = [301, 302, 303, 307, 308].contains(&self.res.status);
        self.trace_body = traceable && !redirect;
        self.verbose_body = traceable && self.verbose && !redirect;
        if !self.verbose {
            return;
        }
        verbose_out(if traceable { "\n\n" } else { "\n" });
        let mut lines = vec![format!("{} {} {}", self.res.proto, self.res.status, self.res.reason).trim_end().to_string()];
        let cl = self.res.header.get("Content-Length");
        if self.res.header.get("Transfer-Encoding").eq_ignore_ascii_case("chunked") {
            lines.push("Transfer-Encoding: chunked".into());
        } else if !cl.is_empty() && cl != "0" {
            lines.push(format!("Content-Length: {cl}"));
        }
        for (k, v) in self.res.header.sorted(&["Content-Length", "Transfer-Encoding", "Trailer"]) {
            lines.push(format!("{k}: {v}"));
        }
        lines.push(String::new());
        let mut out = String::new();
        for l in lines {
            if !self.debugging_verbose && l.to_lowercase().starts_with("authorization: basic") {
                out.push_str("< Authorization: Basic * * * * *\n");
            } else {
                out.push_str(&format!("< {l}\n"));
            }
        }
        verbose_out(&out);
    }
}

extern "C" fn header_cb(ptr: *mut c_char, size: size_t, n: size_t, data: *mut c_void) -> size_t {
    let st = unsafe { &mut *(data as *mut State) };
    let len = size * n;
    let line = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\r', '\n']);
    if line.starts_with("HTTP/") {
        // A new response (after 100 Continue, or a proxy's CONNECT).
        let mut parts = line.splitn(3, ' ');
        let proto = parts.next().unwrap_or("");
        st.res.proto = match proto {
            "HTTP/2" => "HTTP/2.0".into(),
            p => p.to_string(),
        };
        st.res.status = parts.next().unwrap_or("0").parse().unwrap_or(0);
        st.res.reason = parts.next().unwrap_or("").to_string();
        if st.res.reason.is_empty() {
            st.res.reason = status_text(st.res.status).to_string();
        }
        st.res.header = Headers::default();
    } else if let Some((k, v)) = line.split_once(':') {
        st.res.header.add(k.trim(), v.trim());
    }
    len
}

extern "C" fn write_cb(ptr: *mut c_char, size: size_t, n: size_t, data: *mut c_void) -> size_t {
    let st = unsafe { &mut *(data as *mut State) };
    let len = size * n;
    let chunk = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    if !st.head_done {
        st.finish_head();
    }
    st.received += len as u64;
    if st.trace_body {
        crate::trace!("HTTP: {}", String::from_utf8_lossy(chunk));
    }
    if st.verbose_body {
        verbose_out(&String::from_utf8_lossy(chunk));
    }
    if (200..300).contains(&st.res.status) {
        if let Some(s) = st.sink.as_mut() {
            if let Err(e) = s(&st.res, chunk) {
                st.sink_err = Some(e);
                return 0;
            }
            return len;
        }
    }
    st.res.body.extend_from_slice(chunk);
    len
}

extern "C" fn read_cb(ptr: *mut c_char, size: size_t, n: size_t, data: *mut c_void) -> size_t {
    let st = unsafe { &mut *(data as *mut State) };
    if let Some(cb) = st.on_start.take() {
        cb();
    }
    let Some(r) = st.reader.as_mut() else { return 0 };
    let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size * n) };
    match r.read(buf) {
        Ok(k) => {
            st.sent += k as u64;
            if k > 0 {
                if let Some(p) = st.progress.as_mut() {
                    p(k as i64);
                }
                if st.trace_req_body {
                    verbose_out(&String::from_utf8_lossy(&buf[..k]));
                }
            }
            k
        }
        Err(e) => {
            st.read_err = Some(crate::tools::io_err(&e));
            curl::READFUNC_ABORT
        }
    }
}

fn status_text(code: u32) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        416 => "Requested Range Not Satisfiable",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        509 => "Bandwidth Limit Exceeded",
        _ => "",
    }
}

/// handleResponse: None below 400; else the server's message (JSON) or a default one, of
/// the kind the status calls for.
pub fn handle_response(res: &Response) -> Option<Error> {
    if res.status < 400 {
        return None;
    }
    let base = match decode_json::<ClientError>(res) {
        Ok(ce) if !ce.message.is_empty() => {
            let mut e = Error::new(ce.message);
            e.http_status = Some(res.status);
            e
        }
        Ok(_) | Err(DecodeError::Type(_)) => default_error(res),
        Err(DecodeError::Other(e)) => e,
    };
    Some(match res.status {
        401 => base.auth(),
        422 => base.unprocessable(),
        429 => {
            let h = res.header.get("Retry-After");
            match base.clone().retriable_later(&h) {
                Some(e) => e,
                None => base.retriable(),
            }
        }
        s if s > 499 && s != 501 && s != 507 && s != 509 => base.go_fatal(),
        _ => base,
    })
}

#[derive(serde::Deserialize, Default)]
struct ClientError {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    message: String,
}

fn default_error(res: &Response) -> Error {
    let u = &res.url;
    let msg = match res.status {
        400 => format!("Client error: {u}"),
        401 | 403 => format!("Authorization error: {u}\nCheck that you have proper access to the repository"),
        404 => format!("Repository or object not found: {u}\nCheck that it exists and that you have proper access to it"),
        422 => format!("Unprocessable entity: {u}"),
        429 => format!("Rate limit exceeded: {u}"),
        500 => format!("Server error: {u}"),
        501 => format!("Not Implemented: {u}"),
        507 => format!("Insufficient server storage: {u}"),
        509 => format!("Bandwidth limit exceeded: {u}"),
        s if s < 500 => format!("Client error {u} from HTTP {s}"),
        s => format!("Server error {u} from HTTP {s}"),
    };
    Error::new(msg)
}

pub enum DecodeError {
    /// Not a JSON media type (decodeTypeError).
    Type(String),
    Other(Error),
}

impl DecodeError {
    pub fn into_error(self) -> Error {
        match self {
            DecodeError::Type(t) => Error::new(format!("Expected JSON type, got: {}", crate::tools::quote(&t))),
            DecodeError::Other(e) => e,
        }
    }
}

/// DecodeJSON: the body, for a JSON (or LFS JSON) content type.
pub fn decode_json<T: serde::de::DeserializeOwned>(res: &Response) -> std::result::Result<T, DecodeError> {
    let ct = res.header.get("Content-Type");
    let media_ok = |m: &str| ct == m || ct.starts_with(&format!("{m};"));
    if !(media_ok(MEDIA_TYPE) || media_ok("application/json")) {
        return Err(DecodeError::Type(ct));
    }
    // json.Decoder reads one value: what follows it does not matter.
    let mut de = serde_json::Deserializer::from_slice(&res.body).into_iter::<T>();
    match de.next() {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(DecodeError::Other(Error::new(go_json_error(&e)).wrap(format!("Unable to parse HTTP response for {} {}", res.method, res.url)))),
        None => Err(DecodeError::Other(Error::new("EOF").wrap(format!("Unable to parse HTTP response for {} {}", res.method, res.url)))),
    }
}

fn go_json_error(e: &serde_json::Error) -> String {
    if e.is_eof() {
        return "unexpected EOF".into();
    }
    e.to_string()
}

/// The error for an unexpected status (statusCodeError).
pub fn status_code_error(res: &Response) -> Error {
    let mut e = Error::new(format!("Invalid HTTP status for {} {}: {}", res.method, res.url.split('?').next().unwrap_or(""), res.status));
    e.http_status = Some(res.status);
    e
}

// git-lfs-authenticate (lfshttp/ssh.go).

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct SshAuthResponse {
    #[serde(skip)]
    pub message: String,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub href: String,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub header: std::collections::BTreeMap<String, String>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub expires_at: Option<String>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub expires_in: Option<i64>,
    #[serde(skip)]
    pub created_at: Option<std::time::SystemTime>,
}

impl SshAuthResponse {
    fn expiration(&self) -> Option<std::time::SystemTime> {
        match self.expires_in {
            Some(n) if n != 0 => Some(self.created_at.unwrap_or_else(std::time::SystemTime::now) + std::time::Duration::from_secs(n.max(0) as u64)),
            _ => self.expires_at.as_deref().and_then(crate::tools::parse_rfc3339).filter(|t| crate::tools::unix_secs(*t) > -62135596800),
        }
    }
    pub fn expired_within(&self, secs: u64) -> bool {
        match self.expiration() {
            None => false,
            Some(t) => t < std::time::SystemTime::now() + std::time::Duration::from_secs(secs),
        }
    }
}

pub fn endpoint_operation(e: &Endpoint, method: &str) -> String {
    if !e.operation.is_empty() {
        return e.operation.clone();
    }
    match method {
        "GET" | "HEAD" => "download".into(),
        _ => "upload".into(),
    }
}

const SSH_AUTH_NOT_FOUND: [&str; 4] = [
    "git-lfs-authenticate: not found",
    "git-lfs-authenticate: command not found",
    "git-lfs-authenticate: no such file",
    "command not found: git-lfs-authenticate",
];

fn ssh_authenticate(e: &Endpoint, op: &str) -> std::result::Result<SshAuthResponse, (SshAuthResponse, Error, bool)> {
    let (exe, args) = crate::ssh::lfs_exe_and_args(&e.ssh, "git-lfs-authenticate", op, false, "").0;
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let now = std::time::SystemTime::now();
    let out = crate::subprocess::command(&exe, &argv).stdin(std::process::Stdio::null()).output();
    let out = match out {
        Ok(o) => o,
        Err(err) => return Err((SshAuthResponse::default(), Error::new(crate::tools::io_err(&err)), false)),
    };
    if !out.status.success() {
        let mut res = SshAuthResponse::default();
        res.message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().unwrap_or(-1);
        let msg = res.message.to_lowercase();
        let unavailable = code == 127 || SSH_AUTH_NOT_FOUND.iter().any(|n| msg.contains(n));
        let err = Error::new(crate::subprocess::exit_text(&out.status));
        return Err((res, err, unavailable));
    }
    let mut res: SshAuthResponse = match serde_json::from_slice(&out.stdout) {
        Ok(r) => r,
        Err(err) => return Err((SshAuthResponse::default(), Error::new(err.to_string()), false)),
    };
    if res.expires_in.unwrap_or(0) == 0 && res.expires_at.as_deref().and_then(crate::tools::parse_rfc3339).is_none() {
        let ttl = cfg().git().int("lfs.defaulttokenttl", 0).max(0);
        res.expires_in = Some(ttl);
    }
    res.created_at = Some(now);
    Ok(res)
}

/// For fields Go's json leaves at their zero value when the JSON says null.
pub fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
