//! The guest's HTTP request API, host side: the same wire protocol as `host/http-protocol.js`,
//! served over vsock (port 1080 by default).
//!
//! One request per connection, at the level of a browser fetch() call rather than TCP: the
//! guest names a method, an http(s) URL and a few allowed headers, and gets back a status,
//! headers and a streamed body.
//!
//!   request   METHOD URL\n  name: value\n  content-length: N\n  \n  <N bytes>
//!   response  STATUS TEXT\n name: value\n  \n  <hex-length>\n<bytes>… 0\n OK\n
//!   failure   ERROR kind: message\n    (instead of the status line, or as the trailer)
use std::io::Read;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::vsock::{Vsock, VsockStream};

pub const DEFAULT_PORT: u32 = 1080;

/// The only request headers the guest may set, unless the policy adds more.
const DEFAULT_ALLOWED_HEADERS: [&str; 4] = ["accept", "content-type", "authorization", "x-api-key"];
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
/// fetch() has already decoded the body, so its framing headers would only mislead the guest.
const DROPPED_RESPONSE_HEADERS: [&str; 4] = ["content-encoding", "content-length", "transfer-encoding", "connection"];
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;

/// A key the host adds to requests for one host, which the guest never sees.
#[derive(Clone)]
pub struct Secret {
    pub host: String,
    pub header: String,
    pub value: String,
}

/// Which hosts the guest may reach, and what the host adds to those requests.
pub struct Policy {
    /// Host patterns: "example.com", "*.example.com" (subdomains only) or "*".
    pub allow: Vec<String>,
    /// Checked first; a match always refuses.
    pub deny: Vec<String>,
    /// May the guest reach this computer (localhost, 127.0.0.0/8, ::1)?
    pub allow_loopback: bool,
    pub secrets: Vec<Secret>,
    /// Request headers the guest may set beyond the defaults.
    pub extra_headers: Vec<String>,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            allow: vec!["*".into()],
            deny: Vec::new(),
            allow_loopback: false,
            secrets: Vec::new(),
            extra_headers: Vec::new(),
        }
    }
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_lowercase();
    if pattern == "*" {
        return true;
    }
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() && host.ends_with(&format!(".{suffix}")),
        None => host == pattern,
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).trim_end_matches('.').to_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host == "::"
        || host == "0.0.0.0"
        || host.split('.').collect::<Vec<_>>().as_slice().first() == Some(&"127")
            && host.split('.').count() == 4
            && host.split('.').all(|part| part.parse::<u8>().is_ok())
}

impl Policy {
    /// Why this host is refused by the allow and deny lists alone, if it is. The packet-level
    /// stack asks this about names it resolves and addresses it is asked to reach; loopback is
    /// its own question there (`allow_loopback`).
    pub fn refuses_host(&self, host: &str) -> Option<String> {
        if let Some(rule) = self.deny.iter().find(|rule| host_matches(rule, host)) {
            return Some(format!("\"{host}\" matches the deny rule \"{rule}\""));
        }
        match self.allow.iter().any(|rule| host_matches(rule, host)) {
            true => None,
            false => Some(format!("\"{host}\" is not in the allow list")),
        }
    }

    /// Why this host is refused, if it is. The wording is the app's and the guest's, so it
    /// matches what the JavaScript runtime said.
    fn refuses(&self, host: &str) -> Option<String> {
        let reason = if is_loopback(host) && !self.allow_loopback {
            format!("\"{host}\" is this computer (allowHostLoopback is off)")
        } else if let Some(rule) = self.deny.iter().find(|rule| host_matches(rule, host)) {
            format!("\"{host}\" matches the deny rule \"{rule}\"")
        } else if !self.allow.iter().any(|rule| host_matches(rule, host)) {
            format!("\"{host}\" is not in the allow list")
        } else {
            return None;
        };
        Some(format!("blocked by the network policy: {reason}"))
    }

    /// The secret headers for a URL. Only ever over https, so a key never travels in the clear.
    fn secrets_for(&self, https: bool, host: &str) -> Vec<&Secret> {
        match https {
            false => Vec::new(),
            true => self.secrets.iter().filter(|secret| host_matches(&secret.host, host)).collect(),
        }
    }

    fn allows_header(&self, name: &str) -> bool {
        DEFAULT_ALLOWED_HEADERS.contains(&name) || self.extra_headers.iter().any(|allowed| allowed == name)
    }

    /// Hides secret values in a message the guest will see.
    fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            if secret.value.len() >= 4 {
                out = out.replace(&secret.value, "[secret]");
            }
        }
        out
    }
}

/// A failure with the kind the protocol names: bad-request, header-not-allowed,
/// scheme-not-allowed, request-too-large, denied, network, timeout, internal.
#[derive(Debug)]
struct BridgeError {
    kind: &'static str,
    message: String,
}

fn fail<T>(kind: &'static str, message: impl Into<String>) -> std::result::Result<T, BridgeError> {
    Err(BridgeError { kind, message: message.into() })
}

type BridgeResult<T> = std::result::Result<T, BridgeError>;

/// Keeps a message to one line, so it cannot forge protocol lines.
fn one_line(text: impl AsRef<str>) -> String {
    let cleaned: String = text.as_ref().chars().map(|c| if c == '\r' || c == '\n' || c == '\0' { ' ' } else { c }).collect();
    cleaned.chars().take(500).collect()
}

fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&byte)
        })
}

/// What the guest asked for.
#[derive(Debug)]
struct Request {
    method: String,
    url: String,
    /// The URL's host, for policy decisions.
    host: String,
    https: bool,
    headers: Vec<(String, String)>,
    content_length: usize,
}

/// Splits an http(s) URL into its host and whether it is https. Anything else is refused.
fn split_url(url: &str) -> BridgeResult<(String, bool)> {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_lowercase(), rest),
        None => return fail("bad-request", format!("invalid URL: {}", one_line(url))),
    };
    let https = match scheme.as_str() {
        "https" => true,
        "http" => false,
        other => return fail("scheme-not-allowed", format!("only http: and https: URLs are allowed, not {other}:")),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        // Those become an Authorization header, which would bypass the allow list.
        return fail("bad-request", "credentials in the URL are not allowed; use the authorization header");
    }
    let host = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((inside, _)) => inside.to_string(),
            None => return fail("bad-request", "invalid IPv6 host"),
        },
        None => authority.split(':').next().unwrap_or("").to_string(),
    };
    if host.is_empty() {
        return fail("bad-request", format!("invalid URL: {}", one_line(url)));
    }
    Ok((host, https))
}

/// Parses and checks a request head. No I/O, so it is easy to test.
fn parse_head(text: &str, policy: &Policy) -> BridgeResult<Request> {
    if text.contains('\r') {
        return fail("bad-request", "lines must end with LF, not CRLF");
    }
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("");
    let Some((method, target)) = first.split_once(' ') else {
        return fail("bad-request", "first line must be: METHOD URL");
    };
    if !METHODS.contains(&method) {
        return fail("bad-request", format!("method {} is not supported ({})", one_line(method), METHODS.join(", ")));
    }
    let (host, https) = split_url(target)?;

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    let mut saw_length = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return fail("bad-request", format!("malformed header line: {}", one_line(line)));
        };
        let name = name.to_lowercase();
        let value = value.trim_matches([' ', '\t']).to_string();
        if !is_token(&name) {
            return fail("bad-request", format!("invalid header name: {}", one_line(&name)));
        }
        if value.contains('\0') {
            return fail("bad-request", format!("invalid header value for {name}"));
        }
        if name == "content-length" {
            if saw_length {
                return fail("bad-request", "duplicate content-length");
            }
            saw_length = true;
            content_length = match value.parse::<usize>() {
                Ok(length) if value.len() <= 15 => length,
                _ => return fail("bad-request", "invalid content-length"),
            };
            continue;
        }
        if !policy.allows_header(&name) {
            return fail("header-not-allowed", format!("request header \"{name}\" is not allowed"));
        }
        headers.push((name, value));
    }

    if (method == "GET" || method == "HEAD") && content_length > 0 {
        return fail("bad-request", format!("{method} requests cannot have a body"));
    }
    if content_length > MAX_REQUEST_BODY {
        return fail("request-too-large", format!("request body of {content_length} bytes exceeds the limit"));
    }
    Ok(Request { method: method.to_string(), url: target.to_string(), host, https, headers, content_length })
}

/// Reads until the blank line that ends the head, returning it and whatever followed.
fn read_head(stream: &VsockStream) -> BridgeResult<(String, Vec<u8>)> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(at) = buffer.windows(2).position(|pair| pair == b"\n\n") {
            let text = match std::str::from_utf8(&buffer[..at]) {
                Ok(text) => text.to_string(),
                Err(_) => return fail("bad-request", "request head is not valid UTF-8"),
            };
            return Ok((text, buffer[at + 2..].to_vec()));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return fail("bad-request", "request head too large");
        }
        match stream.read(&mut chunk) {
            Ok(0) => return fail("bad-request", "connection closed before the request head ended"),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) => return fail("network", format!("reading the request: {error}")),
        }
    }
}

fn read_body(stream: &VsockStream, rest: Vec<u8>, length: usize) -> BridgeResult<Vec<u8>> {
    if rest.len() > length {
        return fail("bad-request", "data after the request body");
    }
    let mut body = rest;
    while body.len() < length {
        let mut chunk = vec![0u8; (length - body.len()).min(64 * 1024)];
        match stream.read(&mut chunk) {
            Ok(0) => return fail("bad-request", "connection closed before the request body ended"),
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(error) => return fail("network", format!("reading the request body: {error}")),
        }
    }
    Ok(body)
}

/// What the host tells its caller about a request, for a log or a UI.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// The policy refused it: nothing was sent.
    pub blocked: bool,
}

type Observer = Arc<dyn Fn(Event) + Send + Sync>;

/// The certificates that verify https servers. By default Mozilla's roots; `COLLABO_CORE_CA_FILE`
/// names a PEM file that **replaces** them, as curl's `--cacert` does — for a private CA or a
/// company proxy that terminates TLS.
fn root_certificates() -> Result<ureq::tls::RootCerts> {
    let Some(path) = std::env::var_os("COLLABO_CORE_CA_FILE") else {
        return Ok(ureq::tls::RootCerts::WebPki);
    };
    let pem = std::fs::read(&path).with_context(|| format!("reading {}", path.to_string_lossy()))?;
    let certificates: Vec<ureq::tls::Certificate<'static>> = pem
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>()
        .split_inclusive(|line| line.starts_with(b"-----END "))
        .filter_map(|block| {
            let bytes: Vec<u8> = block.concat();
            ureq::tls::Certificate::from_pem(&bytes).ok().map(|certificate| certificate.to_owned())
        })
        .collect();
    if certificates.is_empty() {
        bail!("no certificate in {}", path.to_string_lossy());
    }
    Ok(ureq::tls::RootCerts::from(certificates))
}

/// Serves the guest's HTTP API on `port`, one thread per request.
pub fn serve(vsock: &Vsock, port: u32, policy: Arc<RwLock<Policy>>, observer: Option<Observer>) -> Result<()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(root_certificates()?).build())
            .timeout_global(Some(Duration::from_secs(60)))
            .max_redirects(10)
            // 404 is an answer, not a failure: the guest gets the status like a browser does.
            .http_status_as_error(false)
            .build(),
    );
    vsock.listen(port, move |stream| {
        let (policy, agent, observer) = (policy.clone(), agent.clone(), observer.clone());
        std::thread::spawn(move || {
            let policy = policy.read().unwrap();
            if let Err(error) = handle(&stream, &policy, &agent, observer.as_ref()) {
                eprintln!("collabo-core: http bridge: {error:#}");
            }
        });
    })
}

/// Serves one connection. Every failure is reported to the guest in the protocol's own words.
fn handle(stream: &VsockStream, policy: &Policy, agent: &ureq::Agent, observer: Option<&Observer>) -> Result<()> {
    let mut head_sent = false;
    let mut blocked = false;
    let mut seen: Option<(String, String)> = None;
    let outcome = (|| -> BridgeResult<()> {
        let (text, rest) = read_head(stream)?;
        let request = parse_head(&text, policy)?;
        seen = Some((request.method.clone(), request.url.clone()));
        let body = read_body(stream, rest, request.content_length)?;

        if let Some(reason) = policy.refuses(&request.host) {
            if let Some(observer) = observer {
                observer(Event {
                    method: request.method.clone(),
                    url: request.url.clone(),
                    status: None,
                    error: Some(reason.clone()),
                    blocked: true,
                });
            }
            blocked = true;
            return fail("denied", reason);
        }

        // The host's secrets replace a header of the same name the guest set: a key the app
        // configured always wins over anything from inside the sandbox.
        let secrets = policy.secrets_for(request.https, &request.host);
        let mut builder = ureq::http::Request::builder().method(request.method.as_str()).uri(&request.url);
        for (name, value) in &request.headers {
            if secrets.iter().any(|secret| secret.header == *name) {
                continue;
            }
            builder = builder.header(name, value);
        }
        // The guest never sees these, and they are never logged.
        for secret in secrets {
            builder = builder.header(secret.header.as_str(), secret.value.as_str());
        }
        let built = match builder.body(body.as_slice()) {
            Ok(built) => built,
            Err(error) => return fail("bad-request", policy.redact(&error.to_string())),
        };

        let mut response = match agent.run(built) {
            Ok(response) => response,
            Err(ureq::Error::Timeout(_)) => return fail("timeout", "no response within 60 s"),
            Err(error) => return fail("network", policy.redact(&error.to_string())),
        };

        let status = response.status();
        if let Some(observer) = observer {
            observer(Event {
                method: request.method.clone(),
                url: request.url.clone(),
                status: Some(status.as_u16()),
                error: None,
                blocked: false,
            });
        }

        let mut head = format!("{} {}\n", status.as_u16(), status.canonical_reason().unwrap_or(""));
        for (name, value) in response.headers() {
            let name = name.as_str().to_lowercase();
            if DROPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
                continue;
            }
            head.push_str(&format!("{name}: {}\n", one_line(value.to_str().unwrap_or(""))));
        }
        head.push('\n');
        write(stream, head.as_bytes())?;
        head_sent = true;

        if request.method != "HEAD" {
            let mut reader = response.body_mut().as_reader();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) => return fail("network", format!("response body interrupted: {error}")),
                };
                write(stream, format!("{read:x}\n").as_bytes())?;
                write(stream, &chunk[..read])?;
            }
        }
        write(stream, b"0\nOK\n")?;
        Ok(())
    })();

    if let Err(error) = outcome {
        let message = policy.redact(&one_line(&error.message));
        // A refusal was already reported with its reason.
        if let (Some(observer), Some((method, url)), false) = (observer, seen, blocked) {
            observer(Event { method, url, status: None, error: Some(message.clone()), blocked: false });
        }
        let line = format!("ERROR {}: {message}\n", error.kind);
        // Before the head the error replaces the status line; after it, it is the trailer.
        let reply = match head_sent {
            true => format!("0\n{line}"),
            false => line,
        };
        let _ = stream.write_all(reply.as_bytes());
    }
    stream.close();
    Ok(())
}

fn write(stream: &VsockStream, bytes: &[u8]) -> BridgeResult<()> {
    match stream.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(error) => fail("network", format!("writing to the guest: {error}")),
    }
}

/// Parses `HOST:HEADER=VALUE`, the command line spelling of a secret.
pub fn parse_secret(spec: &str) -> Result<Secret> {
    let Some((host, rest)) = spec.split_once(':') else {
        bail!("a secret looks like HOST:HEADER=VALUE, got {spec}");
    };
    let Some((header, value)) = rest.split_once('=') else {
        bail!("a secret looks like HOST:HEADER=VALUE, got {spec}");
    };
    let header = header.to_lowercase();
    if !is_token(&header) || value.contains(['\r', '\n']) {
        bail!("{header} is not a valid header name for a secret");
    }
    Ok(Secret { host: host.to_lowercase(), header, value: value.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_policy() -> Policy {
        Policy {
            allow: vec!["example.com".into(), "*.allowed.test".into()],
            deny: vec!["blocked.allowed.test".into()],
            ..Policy::default()
        }
    }

    #[test]
    fn patterns_match_the_host_exactly_or_by_subdomain() {
        assert!(host_matches("*", "anything.example"));
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("example.com", "EXAMPLE.com."));
        assert!(!host_matches("example.com", "www.example.com"));
        assert!(host_matches("*.allowed.test", "www.allowed.test"));
        // A wildcard covers subdomains, not the domain itself.
        assert!(!host_matches("*.allowed.test", "allowed.test"));
    }

    #[test]
    fn the_policy_says_why_a_host_is_refused() {
        let policy = a_policy();
        assert_eq!(policy.refuses_host("example.com"), None);
        assert!(policy.refuses_host("other.test").unwrap().contains("not in the allow list"));
        assert!(policy.refuses_host("blocked.allowed.test").unwrap().contains("deny rule"));
        // Loopback is its own rule, and off by default.
        assert!(policy.refuses("localhost").unwrap().contains("allowHostLoopback is off"));
        // With loopback allowed the address itself still has to be in the allow list.
        assert!(Policy { allow_loopback: true, ..a_policy() }.refuses("127.0.0.1").is_some());
    }

    #[test]
    fn loopback_is_recognised_by_name_and_address() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("app.localhost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.13.0.9"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("example.com"));
        assert!(!is_loopback("128.0.0.1"));
    }

    #[test]
    fn a_request_head_is_parsed_and_checked() {
        let policy = Policy::default();
        let request = parse_head("GET https://example.com/v1?q=1\naccept: application/json\n", &policy).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.host, "example.com");
        assert!(request.https);
        assert_eq!(request.headers, vec![("accept".to_string(), "application/json".to_string())]);

        // Only a few headers may be set, and only http(s) URLs are carried.
        assert_eq!(parse_head("GET https://example.com/\ncookie: a=b\n", &policy).unwrap_err().kind, "header-not-allowed");
        assert_eq!(parse_head("GET file:///etc/passwd\n", &policy).unwrap_err().kind, "scheme-not-allowed");
        assert_eq!(parse_head("GET https://user:pw@example.com/\n", &policy).unwrap_err().kind, "bad-request");
        assert_eq!(parse_head("GET https://example.com/\ncontent-length: 5\n", &policy).unwrap_err().kind, "bad-request");
        assert_eq!(parse_head("SING https://example.com/\n", &policy).unwrap_err().kind, "bad-request");
    }

    #[test]
    fn secrets_only_travel_over_https() {
        let policy = Policy {
            secrets: vec![Secret { host: "example.com".into(), header: "x-api-key".into(), value: "sk-1".into() }],
            ..Policy::default()
        };
        assert_eq!(policy.secrets_for(true, "example.com").len(), 1);
        assert_eq!(policy.secrets_for(false, "example.com").len(), 0);
        assert_eq!(policy.secrets_for(true, "other.test").len(), 0);
        assert_eq!(policy.redact("the key is sk-1 today"), "the key is [secret] today");
    }
}
