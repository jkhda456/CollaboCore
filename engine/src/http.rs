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
//!
//! `x-collabo-tls-verify: false` in the head is for the host, not the server: that request's
//! https certificate is not checked (a local server's self-signed one), as `curl -k` does. The
//! app's secrets are never sent on such a request. The policy's `skip_tls_verify` (the app's
//! choice, for development) checks no certificate at all, and then the secrets do go.
//!
//! The policy's `private_networks` (the app's `privateNetworks`) refuses the private networks'
//! addresses, or everything but them, by the address a host has: here an IP in the URL, and
//! the addresses a name resolves to (`PolicyResolver`, each redirect too); in net.rs the
//! guest's DNS answers and connections. The lists' address rules (an IP, a CIDR block such as
//! 10.0.0.0/8, an IPv4 wildcard such as 10.*: `AddressRule`) are judged the same way, so a
//! deny rule's addresses are refused by whatever name. This computer is judged by address here
//! too: a name that resolves to 127.0.0.1 needs `allow_loopback` as `localhost` does.
//!
//! Redirects are followed here, not by ureq: each hop is checked as the guest's own request is
//! (the lists, `ask`, the addresses), gets the app's secrets for its own host only, and takes
//! the guest's headers along within the origin (to another origin only `accept` and
//! `content-type`). Methods change as ureq changed them; at most 10.
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::vsock::{Vsock, VsockStream};

pub const DEFAULT_PORT: u32 = 1080;

/// The only request headers the guest may set, unless the policy adds more.
const DEFAULT_ALLOWED_HEADERS: [&str; 4] = ["accept", "content-type", "authorization", "x-api-key"];
/// The head's one header for the host itself: `false` skips the certificate check.
const TLS_VERIFY_HEADER: &str = "x-collabo-tls-verify";
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
/// fetch() has already decoded the body, so its framing headers would only mislead the guest.
const DROPPED_RESPONSE_HEADERS: [&str; 4] = ["content-encoding", "content-length", "transfer-encoding", "connection"];
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Redirects followed for one request, as many as ureq followed when it did.
const MAX_REDIRECTS: u32 = 10;
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;

/// A key the host adds to requests for one host, which the guest never sees.
#[derive(Clone)]
pub struct Secret {
    pub host: String,
    pub header: String,
    pub value: String,
}

/// The private networks' addresses (`is_private`): reachable as any other (the default), never
/// (only the internet), or alone (only the local networks).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum PrivateNetworks {
    #[default]
    Allow,
    Deny,
    Only,
}

impl PrivateNetworks {
    pub fn parse(text: &str) -> Option<PrivateNetworks> {
        match text.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(PrivateNetworks::Allow),
            "deny" => Some(PrivateNetworks::Deny),
            "only" => Some(PrivateNetworks::Only),
            _ => None,
        }
    }
}

/// An address only a local network reaches: 10/8, 172.16/12, 192.168/16 (RFC 1918), 100.64/10
/// (carrier-grade NAT, RFC 6598: Tailscale and the like), 169.254/16 (link-local: a cloud's
/// metadata service), fc00::/7 (unique local), fe80::/10 (link-local), and those IPv4 ones
/// written as IPv6 (::ffff:a.b.c.d). Loopback is not one: `allow_loopback` is its own rule.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 100 && (64..=127).contains(&b)) || (a == 169 && b == 254)
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => is_private(IpAddr::V4(ip)),
            None => ip.segments()[0] & 0xfe00 == 0xfc00 || ip.segments()[0] & 0xffc0 == 0xfe80,
        },
    }
}

/// This computer by address: 127/8 and ::1, 0.0.0.0 and :: (which reach it too), and those
/// IPv4 ones written as IPv6 (::ffff:127.0.0.1).
fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_unspecified(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unspecified() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback() || ip.is_unspecified()),
    }
}

/// Which hosts the guest may reach, and what the host adds to those requests.
#[derive(Clone)]
pub struct Policy {
    /// Host patterns: "example.com", "*.example.com" (subdomains only) or "*"; and address rules
    /// (`AddressRule`): "192.168.0.22", "10.0.0.0/8", "192.168.1.*", by address. A name still needs a pattern:
    /// an address rule lets an address in, it does not make the host resolve any name for it.
    pub allow: Vec<String>,
    /// Checked first; a match always refuses. An address rule here refuses its addresses by any
    /// name that resolves to them too.
    pub deny: Vec<String>,
    /// May the guest reach this computer (localhost, 127.0.0.0/8, ::1)?
    pub allow_loopback: bool,
    pub secrets: Vec<Secret>,
    /// Request headers the guest may set beyond the defaults.
    pub extra_headers: Vec<String>,
    /// What the add-ons the sandbox booted with add (addons.rs): kept apart so that a
    /// `policy.update` replacing the lists above leaves them in place. An app secret for the
    /// same host and header wins over an add-on's.
    pub addon_headers: Vec<String>,
    pub addon_secrets: Vec<Secret>,
    /// A host neither list names is asked about (the app's `permission` event, kind "network")
    /// instead of refused.
    pub ask: bool,
    /// No https certificate is checked, on any request (and on intercept.rs's connections):
    /// for development against local servers. The app's choice, so its secrets still go.
    pub skip_tls_verify: bool,
    /// The private networks' addresses: allowed, refused, or the only ones (`refuses_address`).
    pub private_networks: PrivateNetworks,
}

/// Asks the app whether the guest may reach a host the lists do not name: (via "api" or "net",
/// target "host:port") → allowed. Blocks for as long as the user takes to answer.
pub type Asker = Arc<dyn Fn(&'static str, &str) -> bool + Send + Sync>;

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            allow: vec!["*".into()],
            deny: Vec::new(),
            allow_loopback: false,
            secrets: Vec::new(),
            extra_headers: Vec::new(),
            addon_headers: Vec::new(),
            addon_secrets: Vec::new(),
            ask: false,
            skip_tls_verify: false,
            private_networks: PrivateNetworks::Allow,
        }
    }
}

/// An address rule in the allow and deny lists: an IP address ("192.168.0.22", "::1"), a CIDR
/// block ("10.0.0.0/8", "fd00::/8"), or an IPv4 address whose last octets are * ("192.168.1.*"
/// is 192.168.1.0/24, "10.*" 10.0.0.0/8). A host pattern is about a name; this is about the
/// address a host has, whatever name reaches it (`Policy::refuses_address`). IPv4 written as
/// IPv6 (::ffff:a.b.c.d, or ::ffff:0:0/96 and longer) is IPv4.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AddressRule {
    network: IpAddr,
    prefix: u8,
}

/// An IPv4 address written as IPv6 is the IPv4 one.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// `ip` with the bits past `prefix` cleared.
fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & u32::MAX.checked_shl(32 - prefix as u32).unwrap_or(0)).into()),
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & u128::MAX.checked_shl(128 - prefix as u32).unwrap_or(0)).into()),
    }
}

impl AddressRule {
    /// The address rule `text` is: None for a host pattern, an error for what is not a valid
    /// CIDR block (anything with a "/": a prefix out of range, bits set past it) or IPv4
    /// wildcard (`wildcard`), so that a rule written wrong is refused rather than matching
    /// nothing.
    pub fn parse(text: &str) -> std::result::Result<Option<AddressRule>, String> {
        let text = text.trim();
        if text.contains('*') {
            return AddressRule::wildcard(text);
        }
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let Ok(ip) = address.trim_matches(['[', ']']).parse::<IpAddr>() else {
            return match prefix {
                None => Ok(None),
                Some(_) => Err(format!("\"{text}\" is not a CIDR block (an address and a prefix length, such as 10.0.0.0/8 or fd00::/8)")),
            };
        };
        let bits = if ip.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= bits)
                .ok_or_else(|| format!("\"{text}\": the prefix length is 0 to {bits}"))?,
        };
        let network = masked(ip, prefix);
        if network != ip {
            return Err(format!("\"{text}\" has bits set past its /{prefix} (the block is {network}/{prefix})"));
        }
        Ok(Some(match network {
            IpAddr::V6(v6) if prefix >= 96 && v6.to_ipv4_mapped().is_some() => {
                AddressRule { network: canonical(network), prefix: prefix - 96 }
            }
            _ => AddressRule { network, prefix },
        }))
    }

    /// An IPv4 address with * for its last whole octets, as the CIDR block that makes:
    /// "192.168.1.*" /24, "192.168.*" or "192.168.*.*" /16, "10.*" /8. None for what is no
    /// address (a host pattern: "*", "*.example.com"). Refused: a * before a number
    /// ("10.*.0.1", "*.168.1.1": no CIDR block is that), inside an octet ("192.168.1*"), and in
    /// IPv6 ("fd00::*"), which takes a CIDR block.
    fn wildcard(text: &str) -> std::result::Result<Option<AddressRule>, String> {
        if text.contains(':') {
            return Err(format!("\"{text}\": IPv6 takes no *, only a CIDR block such as fd00::/8"));
        }
        if text.contains('/') {
            return Err(format!("\"{text}\": a CIDR block takes no *"));
        }
        let parts: Vec<&str> = text.split('.').collect();
        let numeric = |part: &&str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit() || byte == b'*');
        if !parts.iter().all(numeric) || !text.bytes().any(|byte| byte.is_ascii_digit()) {
            return Ok(None);
        }
        let wrong = || format!("\"{text}\": * stands for whole octets at the end of an IPv4 address (192.168.1.*, 10.*), or write a CIDR block");
        let Some(at) = parts.iter().position(|part| *part == "*").filter(|at| *at > 0 && parts.len() <= 4 && parts[*at..].iter().all(|part| *part == "*")) else {
            return Err(wrong());
        };
        let mut octets = [0u8; 4];
        for (octet, part) in octets.iter_mut().zip(&parts[..at]) {
            // 010 is 8 to some tools and 10 to others: written as neither.
            if part.len() > 1 && part.starts_with('0') {
                return Err(wrong());
            }
            *octet = part.parse().map_err(|_| wrong())?;
        }
        Ok(Some(AddressRule { network: IpAddr::V4(octets.into()), prefix: 8 * at as u8 }))
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        ip.is_ipv4() == self.network.is_ipv4() && masked(ip, self.prefix) == self.network
    }
}

/// The address rule an allow or deny list entry is, if it is one (`check_rule` has refused the
/// entries that are neither one nor a host pattern).
fn address_rule(rule: &str) -> Option<AddressRule> {
    AddressRule::parse(rule).ok().flatten()
}

/// Is this an allow or deny list entry the policy understands? A host pattern ("example.com",
/// "*.example.com", "*") or an address rule ("192.168.0.22", "10.0.0.0/8", "192.168.1.*").
/// Anything else would match nothing, which in the deny list lets through what it was meant to
/// stop.
pub fn check_rule(rule: &str) -> std::result::Result<(), String> {
    if AddressRule::parse(rule)?.is_some() {
        return Ok(());
    }
    let rule = rule.trim();
    let wildcard = rule.strip_prefix("*.").unwrap_or(rule);
    if rule.is_empty() || (rule != "*" && wildcard.contains('*')) {
        return Err(format!(
            "\"{rule}\": a host is \"example.com\", \"*.example.com\" (its subdomains) or \"*\", and addresses an IP, a CIDR block such as 192.168.1.0/24 or an IPv4 address ending in * such as 192.168.1.*"
        ));
    }
    Ok(())
}

/// Does an allow or deny list entry match this host? A host pattern by name (`host_matches`);
/// an address rule by address, when the host is one (an IP in a URL, the guest's connection).
/// What a name resolves to is `refuses_address`'s.
fn rule_matches(rule: &str, host: &str) -> bool {
    match address_rule(rule) {
        Some(rule) => host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| rule.contains(ip)),
        None => host_matches(rule, host),
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
        if let Some(rule) = self.deny.iter().find(|rule| rule_matches(rule, host)) {
            return Some(format!("\"{host}\" matches the deny rule \"{rule}\""));
        }
        match self.allow.iter().any(|rule| rule_matches(rule, host)) {
            true => None,
            false => Some(format!("\"{host}\" is not in the allow list")),
        }
    }

    /// Why the policy refuses `ip`, the address `host` (a name, or the address itself) has, by
    /// that address, if it does: a deny list address rule (an IP, a CIDR block), whatever the
    /// name, as deny always wins; then `private_networks`. From that one, a host the allow list
    /// names exactly (no `*`) is exempt, and so is an address an allow list address rule holds:
    /// named on purpose, while `*` and `*.example.com` also let through names whose addresses
    /// whoever runs their DNS picks. Loopback is `allow_loopback`'s, not these rules'.
    pub fn refuses_address(&self, host: &str, ip: IpAddr) -> Option<String> {
        if is_loopback_ip(ip) {
            return None;
        }
        let host = host.trim_matches(['[', ']']);
        let shown = match host.parse::<IpAddr>() {
            Ok(_) => format!("{ip}"),
            Err(_) => format!("\"{host}\" is {ip}, which"),
        };
        if let Some(rule) = self.deny.iter().find(|rule| address_rule(rule).is_some_and(|rule| rule.contains(ip))) {
            return Some(format!("{shown} matches the deny rule \"{rule}\""));
        }
        if self.private_networks == PrivateNetworks::Allow {
            return None;
        }
        let named = |rule: &String| match address_rule(rule) {
            Some(rule) => rule.contains(ip),
            None => !rule.contains('*') && host_matches(rule, host),
        };
        if self.allow.iter().any(named) {
            return None;
        }
        match (self.private_networks, is_private(ip)) {
            (PrivateNetworks::Deny, true) => Some(format!("{shown} is in a private network (privateNetworks is deny)")),
            (PrivateNetworks::Only, false) => Some(format!("{shown} is not in a private network (privateNetworks is only)")),
            _ => None,
        }
    }

    /// Why the request API may not connect to `ip`, an address `host` resolved to
    /// (PolicyResolver): this computer without `allow_loopback` — a name says nothing about
    /// where it points, so `is_loopback`'s look at the name is not enough — or the rules by
    /// address (`refuses_address`).
    fn refuses_resolved(&self, host: &str, ip: IpAddr) -> Option<String> {
        if !is_loopback_ip(ip) {
            return self.refuses_address(host, ip);
        }
        let host = host.trim_matches(['[', ']']);
        (!self.allow_loopback).then(|| match host.parse::<IpAddr>() {
            Ok(_) => format!("{ip} is this computer (allowHostLoopback is off)"),
            Err(_) => format!("\"{host}\" is {ip}, this computer (allowHostLoopback is off)"),
        })
    }

    /// Neither list names this host (and it is not this computer): what `ask` asks about.
    pub fn unlisted(&self, host: &str) -> bool {
        !is_loopback(host)
            && !self.deny.iter().any(|rule| rule_matches(rule, host))
            && !self.allow.iter().any(|rule| rule_matches(rule, host))
    }

    /// Why this host is refused, if it is. The wording is the app's and the guest's, so it
    /// matches what the JavaScript runtime said.
    fn refuses(&self, host: &str) -> Option<String> {
        let reason = if is_loopback(host) && !self.allow_loopback {
            format!("\"{host}\" is this computer (allowHostLoopback is off)")
        } else if let Some(rule) = self.deny.iter().find(|rule| rule_matches(rule, host)) {
            format!("\"{host}\" matches the deny rule \"{rule}\"")
        } else if !self.allow.iter().any(|rule| rule_matches(rule, host)) {
            format!("\"{host}\" is not in the allow list")
        } else if let Some(reason) = host.trim_matches(['[', ']']).parse::<IpAddr>().ok().and_then(|ip| self.refuses_address(host, ip)) {
            // An address in the URL; a name's addresses are PolicyResolver's.
            reason
        } else {
            return None;
        };
        Some(format!("blocked by the network policy: {reason}"))
    }

    /// The secret headers for a URL. Only ever over https, so a key never travels in the clear.
    fn secrets_for(&self, https: bool, host: &str) -> Vec<&Secret> {
        match https {
            false => Vec::new(),
            true => {
                let mut matching: Vec<&Secret> = self.secrets.iter().filter(|secret| host_matches(&secret.host, host)).collect();
                let app = matching.len();
                for secret in self.addon_secrets.iter().filter(|secret| host_matches(&secret.host, host)) {
                    if !matching[..app].iter().any(|own| own.header == secret.header) {
                        matching.push(secret);
                    }
                }
                matching
            }
        }
    }

    /// The secrets for a host the guest reaches over its own TLS (intercept.rs).
    pub fn secrets_for_host(&self, host: &str) -> Vec<Secret> {
        self.secrets_for(true, host).into_iter().cloned().collect()
    }

    /// Is there a secret for this host? Its TLS connections are then taken over (intercept.rs).
    pub fn has_secret_for(&self, host: &str) -> bool {
        self.secrets.iter().chain(&self.addon_secrets).any(|secret| host_matches(&secret.host, host))
    }

    /// `redact`, for other modules.
    pub fn redact_text(&self, text: &str) -> String {
        self.redact(text)
    }

    fn allows_header(&self, name: &str) -> bool {
        DEFAULT_ALLOWED_HEADERS.contains(&name)
            || self.extra_headers.iter().chain(&self.addon_headers).any(|allowed| allowed == name)
    }

    /// Hides secret values in a message the guest will see.
    fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in self.secrets.iter().chain(&self.addon_secrets) {
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
    /// Is the server's certificate checked? (`x-collabo-tls-verify`)
    verify_tls: bool,
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
    let mut verify_tls = true;
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
        if name == TLS_VERIFY_HEADER {
            verify_tls = match value.as_str() {
                "true" => true,
                "false" => false,
                _ => return fail("bad-request", format!("{TLS_VERIFY_HEADER} is true or false, not {}", one_line(&value))),
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
    Ok(Request { method: method.to_string(), url: target.to_string(), host, https, headers, content_length, verify_tls })
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
    /// An https request whose certificate was not checked (`x-collabo-tls-verify: false`).
    pub unverified: bool,
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

/// The policy refused the addresses a name resolved to (PolicyResolver): the request is denied.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Resolves as ureq does, then keeps the addresses the URL's host may be reached at
/// (`Policy::refuses_resolved`): this computer only with `allow_loopback`, and what the deny
/// list's address rules and `private_networks` let through. A name that resolves to 127.0.0.1, or into a private network
/// or out of one, is only told apart here; every redirect handle() follows comes here too. The
/// live policy, so a `policy.update` applies at once. A proxy the host is configured with
/// (`HTTPS_PROXY`) is the host's own: its address is not checked, and the names it resolves
/// are its business.
struct PolicyResolver {
    policy: Arc<RwLock<Policy>>,
    inner: ureq::unversioned::resolver::DefaultResolver,
}

/// ureq wants one; the policy (with the secrets) stays out of it.
impl std::fmt::Debug for PolicyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PolicyResolver")
    }
}

impl ureq::unversioned::resolver::Resolver for PolicyResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let found = self.inner.resolve(uri, config, timeout)?;
        if config.proxy().is_some_and(|proxy| proxy.uri().host() == uri.host()) {
            return Ok(found);
        }
        let host = uri.host().unwrap_or("");
        let policy = self.policy.read().unwrap();
        let mut kept = self.empty();
        let mut refused = None;
        for address in found.iter() {
            match policy.refuses_resolved(host, address.ip()) {
                None => kept.push(*address),
                Some(reason) => refused = refused.or(Some(reason)),
            }
        }
        match (kept.is_empty(), refused) {
            (true, Some(reason)) => Err(ureq::Error::Other(Box::new(Refused(format!("blocked by the network policy: {reason}"))))),
            _ => Ok(kept),
        }
    }
}

/// The longest a read waits for the server (a streamed body may go on for any time while it
/// keeps arriving).
const IDLE_READ: Duration = Duration::from_secs(1800);

/// The last of the connector chain: each wait for input lasts this long at most (or the phase's
/// own deadline, when nearer). A read that waits that long fails as a timeout.
#[derive(Debug)]
struct Idle(Duration);

impl ureq::unversioned::transport::Connector<Box<dyn ureq::unversioned::transport::Transport>> for Idle {
    type Out = IdleTransport;

    fn connect(
        &self,
        _: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<Box<dyn ureq::unversioned::transport::Transport>>,
    ) -> std::result::Result<Option<IdleTransport>, ureq::Error> {
        Ok(chained.map(|inner| IdleTransport(inner, self.0)))
    }
}

#[derive(Debug)]
struct IdleTransport(Box<dyn ureq::unversioned::transport::Transport>, Duration);

impl ureq::unversioned::transport::Transport for IdleTransport {
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.0.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: ureq::unversioned::transport::NextTimeout) -> std::result::Result<(), ureq::Error> {
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: ureq::unversioned::transport::NextTimeout) -> std::result::Result<bool, ureq::Error> {
        self.0.await_input(idle_capped(timeout, self.1))
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }
}

fn idle_capped(timeout: ureq::unversioned::transport::NextTimeout, idle: Duration) -> ureq::unversioned::transport::NextTimeout {
    use ureq::unversioned::transport::time;
    match timeout.after {
        time::Duration::Exact(after) if after <= idle => timeout,
        _ => ureq::unversioned::transport::NextTimeout { after: time::Duration::Exact(idle), ..timeout },
    }
}

/// The two agents requests go through: one that checks certificates, one that does not.
#[derive(Clone)]
struct Agents {
    verified: ureq::Agent,
    unverified: ureq::Agent,
}

fn agent(roots: ureq::tls::RootCerts, verify: bool, policy: Arc<RwLock<Policy>>) -> ureq::Agent {
    let resolver = PolicyResolver { policy, inner: Default::default() };
    ureq::Agent::with_parts(
        ureq::Agent::config_builder()
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).disable_verification(!verify).build())
            // Per phase rather than one deadline for the whole exchange: a model's answer streams
            // for minutes, and a local model can take minutes to start. The body has no deadline
            // of its own (ureq's would end an answer still streaming at 30 minutes): it ends when
            // nothing arrives for IDLE_READ (Idle).
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_send_body(Some(Duration::from_secs(120)))
            .timeout_recv_response(Some(Duration::from_secs(600)))
            // handle() follows redirects itself, checking each one.
            .max_redirects(0)
            // 404 is an answer, not a failure: the guest gets the status like a browser does.
            .http_status_as_error(false)
            .build(),
        ureq::unversioned::transport::Connector::chain(ureq::unversioned::transport::DefaultConnector::default(), Idle(IDLE_READ)),
        resolver,
    )
}

/// Serves the guest's HTTP API on `port`, one thread per request.
pub fn serve(vsock: &Vsock, port: u32, policy: Arc<RwLock<Policy>>, observer: Option<Observer>, asker: Option<Asker>) -> Result<()> {
    let roots = root_certificates()?;
    let agents = Agents { verified: agent(roots.clone(), true, policy.clone()), unverified: agent(roots, false, policy.clone()) };
    vsock.listen(port, move |stream| {
        let (policy, agents, observer, asker) = (policy.clone(), agents.clone(), observer.clone(), asker.clone());
        std::thread::spawn(move || {
            // A copy: the request keeps the policy it started with, and a question to the user
            // must not hold the lock policy.update needs.
            let policy = policy.read().unwrap().clone();
            if let Err(error) = handle(&stream, &policy, &agents, observer.as_ref(), asker.as_ref()) {
                eprintln!("collabo-core: http bridge: {error:#}");
            }
        });
    })
}

/// One request on its way: the guest's, or a redirect handle() follows.
struct Hop {
    method: String,
    url: String,
    host: String,
    https: bool,
    /// The guest's headers that go with it.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// One event for the app (and the network log) about a hop.
fn notify(observer: Option<&Observer>, hop: &Hop, status: Option<u16>, error: Option<&str>, blocked: bool, unverified: bool) {
    if let Some(observer) = observer {
        observer(Event { method: hop.method.clone(), url: hop.url.clone(), status, error: error.map(str::to_string), blocked, unverified });
    }
}

/// The method a redirect goes on with, as ureq (and curl) do: 307 and 308 keep it, but are not
/// followed for a method with a body or DELETE (None: the 3xx goes back to the guest); the
/// others turn what is not GET or HEAD into GET.
fn redirect_method(method: &str, status: u16) -> Option<String> {
    match status {
        307 | 308 if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") => None,
        307 | 308 => Some(method.to_string()),
        _ if method == "GET" || method == "HEAD" => Some(method.to_string()),
        _ => Some("GET".to_string()),
    }
}

/// A Location against the URL it came from (RFC 3986 5.2): absolute, `//authority…`,
/// `/path`, `?query` or a relative path, without its fragment. None when it is not one.
fn join_location(base: &str, location: &str) -> Option<String> {
    let location = location.trim().split('#').next().unwrap_or("");
    if location.is_empty() {
        return None;
    }
    let scheme_end = location.find(':').filter(|&end| {
        location[..end].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && location[..end].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if scheme_end.is_some() {
        return Some(location.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(network_path) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{network_path}"));
    }
    let (authority, path_and_query) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let base_path = match path_and_query.split(['?', '#']).next().unwrap_or("") {
        "" => "/",
        path => path,
    };
    let (path, query) = location.split_at(location.find('?').unwrap_or(location.len()));
    let path = match path {
        "" => base_path.to_string(),
        absolute if absolute.starts_with('/') => absolute.to_string(),
        relative => format!("{}{relative}", &base_path[..=base_path.rfind('/').unwrap_or(0)]),
    };
    Some(format!("{scheme}://{authority}{}{query}", remove_dot_segments(&path)))
}

/// `.` and `..` out of a path (RFC 3986 5.2.4).
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let last = index == segments.len() - 1;
        match *segment {
            "." | ".." => {
                if *segment == ".." && out.len() > 1 {
                    out.pop();
                }
                if last {
                    out.push("");
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match joined.starts_with('/') {
        true => joined,
        false => format!("/{joined}"),
    }
}

/// scheme://host:port of an http(s) URL, the port always written: what a redirect leaves or
/// stays in.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let has_port = authority.rsplit_once(':').is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !port.contains(']'));
    match has_port {
        true => format!("{scheme}://{authority}"),
        false => format!("{scheme}://{authority}:{}", if scheme == "https" { 443 } else { 80 }),
    }
}

/// Serves one connection. Every failure is reported to the guest in the protocol's own words.
fn handle(stream: &VsockStream, policy: &Policy, agents: &Agents, observer: Option<&Observer>, asker: Option<&Asker>) -> Result<()> {
    let mut head_sent = false;
    let mut blocked = false;
    let mut unverified = false;
    let mut seen: Option<(String, String)> = None;
    let outcome = (|| -> BridgeResult<()> {
        let (text, rest) = read_head(stream)?;
        let request = parse_head(&text, policy)?;
        seen = Some((request.method.clone(), request.url.clone()));
        unverified = request.https && (policy.skip_tls_verify || !request.verify_tls);
        let body = read_body(stream, rest, request.content_length)?;

        // A hop the lists refuse, or the app (`ask`) does not allow.
        let refusal = |host: &str, https: bool| -> Option<String> {
            match (policy.refuses(host), asker) {
                (Some(_), Some(ask)) if policy.ask && policy.unlisted(host) => {
                    let target = format!("{host}:{}", if https { 443 } else { 80 });
                    match ask("api", &target) {
                        true => None,
                        false => Some(format!("blocked by the network policy: the app did not allow \"{target}\"")),
                    }
                }
                (reason, _) => reason,
            }
        };

        // The guest's request, then each redirect: followed here rather than by ureq
        // (`max_redirects(0)`), so every hop is checked as the first is and carries only its
        // own host's secrets.
        let mut hop = Hop { method: request.method.clone(), url: request.url.clone(), host: request.host.clone(), https: request.https, headers: request.headers.clone(), body };
        let mut from: Option<String> = None;
        let mut redirects = 0;
        let (mut response, status) = loop {
            seen = Some((hop.method.clone(), hop.url.clone()));
            unverified = hop.https && (policy.skip_tls_verify || !request.verify_tls);
            let after = |reason: String| match &from {
                Some(from) => format!("{reason} (a redirect from {from})"),
                None => reason,
            };
            if let Some(reason) = refusal(&hop.host, hop.https).map(after) {
                notify(observer, &hop, None, Some(&reason), true, unverified);
                blocked = true;
                return fail("denied", reason);
            }

            // The host's secrets replace a header of the same name the guest set: a key the
            // app configured always wins over anything from inside the sandbox.
            let secrets = policy.secrets_for(hop.https, &hop.host);
            // Only the guest asked: the app did not agree to send its keys unchecked.
            if hop.https && !request.verify_tls && !policy.skip_tls_verify && !secrets.is_empty() {
                return fail(
                    "denied",
                    after(format!(
                        "the app's key for \"{}\" is only sent to a server whose certificate is checked; \
                         for a private CA give the engine COLLABO_CORE_CA_FILE instead of {TLS_VERIFY_HEADER}: false",
                        hop.host
                    )),
                );
            }
            let build = || {
                let mut builder = ureq::http::Request::builder().method(hop.method.as_str()).uri(&hop.url);
                for (name, value) in &hop.headers {
                    if secrets.iter().any(|secret| secret.header == *name) {
                        continue;
                    }
                    builder = builder.header(name, value);
                }
                // The guest never sees these, and they are never logged.
                for secret in &secrets {
                    builder = builder.header(secret.header.as_str(), secret.value.as_str());
                }
                builder.body(hop.body.as_slice()).map_err(|error| BridgeError { kind: "bad-request", message: policy.redact(&error.to_string()) })
            };

            let agent = match unverified {
                true => &agents.unverified,
                false => &agents.verified,
            };
            let mut repeated = false;
            let response = loop {
                match agent.run(build()?) {
                    Ok(response) => break response,
                    // A connection the pool kept that the server had just closed: an HTTP/1.0
                    // server closes after every response, and ureq keeps one unless it says
                    // `connection: close`, so the next request can find it gone. A request that
                    // may be repeated goes once more, on a new connection.
                    Err(ureq::Error::Io(error))
                        if !repeated
                            && matches!(hop.method.as_str(), "GET" | "HEAD" | "OPTIONS")
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe
                            ) =>
                    {
                        repeated = true;
                    }
                    Err(ureq::Error::Timeout(phase)) => return fail("timeout", format!("timed out ({phase})")),
                    Err(ureq::Error::Other(error)) if error.is::<Refused>() => {
                        let reason = after(error.to_string());
                        notify(observer, &hop, None, Some(&reason), true, unverified);
                        blocked = true;
                        return fail("denied", reason);
                    }
                    Err(error) => return fail("network", policy.redact(&error.to_string())),
                }
            };
            let status = response.status();
            notify(observer, &hop, Some(status.as_u16()), None, false, unverified);

            // A redirect ureq would have followed: any 3xx but 304 with a Location.
            let location = response.headers().get("location").and_then(|value| value.to_str().ok()).map(str::to_string);
            let (Some(location), true) = (location, status.is_redirection() && status.as_u16() != 304) else { break (response, status) };
            let Some(method) = redirect_method(&hop.method, status.as_u16()) else { break (response, status) };
            // Read to its end (a redirect's body is short), so ureq knows whether the connection
            // can carry the next request: one dropped unread could go back to the pool after the
            // server closed it (an HTTP/1.0 server), and the next hop would find it gone.
            let mut response = response;
            let _ = std::io::copy(&mut response.body_mut().as_reader().take(64 * 1024), &mut std::io::sink());
            drop(response);
            if redirects == MAX_REDIRECTS {
                return fail("network", format!("too many redirects (more than {MAX_REDIRECTS})"));
            }
            redirects += 1;
            let Some(url) = join_location(&hop.url, &location) else {
                return fail("network", format!("invalid redirect location {} (from {})", one_line(&location), hop.url));
            };
            let (host, https) = split_url(&url).map_err(|error| BridgeError { kind: error.kind, message: format!("{} (a redirect from {})", error.message, hop.url) })?;
            let mut headers = std::mem::take(&mut hop.headers);
            let mut body = std::mem::take(&mut hop.body);
            if method != hop.method {
                body.clear();
                headers.retain(|(name, _)| name != "content-type");
            }
            // The guest's own keys stay with the origin it gave them for.
            if origin(&url) != origin(&hop.url) {
                headers.retain(|(name, _)| name == "accept" || name == "content-type");
            }
            from = Some(std::mem::take(&mut hop.url));
            hop = Hop { method, url, host, https, headers, body };
        };

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

        if hop.method != "HEAD" {
            let mut reader = response.body_mut().as_reader();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if matches!(error.get_ref().and_then(|inner| inner.downcast_ref::<ureq::Error>()), Some(ureq::Error::Timeout(_))) => {
                        return fail("timeout", format!("the server sent nothing for {} minutes", IDLE_READ.as_secs() / 60))
                    }
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
            observer(Event { method, url, status: None, error: Some(message.clone()), blocked: false, unverified });
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

/// Parses `NAME[,NAME...]`, the command line spelling of extra allowed headers.
pub fn parse_header_names(spec: &str) -> Result<Vec<String>> {
    let names: Vec<String> = spec.split(',').map(|name| name.trim().to_lowercase()).filter(|name| !name.is_empty()).collect();
    if names.is_empty() {
        bail!("--allow-header needs a header name");
    }
    if let Some(name) = names.iter().find(|name| !is_token(name)) {
        bail!("{name} is not a valid header name");
    }
    Ok(names)
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

    /// A local server that sends its head, then `pieces` of the body `gap` apart (then closes).
    fn slow_server(pieces: usize, gap: Duration, stall: Duration) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = [0u8; 4096];
            let _ = stream.read(&mut head);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n");
            std::thread::sleep(stall);
            for _ in 0..pieces {
                let _ = stream.write_all(b"piece\n");
                std::thread::sleep(gap);
            }
        });
        format!("http://{address}/")
    }

    fn idle_agent(idle: Duration) -> ureq::Agent {
        ureq::Agent::with_parts(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
            ureq::unversioned::transport::Connector::chain(ureq::unversioned::transport::DefaultConnector::default(), Idle(idle)),
            ureq::unversioned::resolver::DefaultResolver::default(),
        )
    }

    #[test]
    fn a_body_goes_on_while_it_arrives_and_ends_when_it_stops() {
        // 12 pieces 100 ms apart: 1.2 s in all, four times the 300 ms a read may wait
        let mut response = idle_agent(Duration::from_millis(300)).get(&slow_server(12, Duration::from_millis(100), Duration::ZERO)).call().unwrap();
        let text = response.body_mut().read_to_string().unwrap();
        assert_eq!(text.matches("piece").count(), 12);
        // nothing for 1 s after the head
        let mut response = idle_agent(Duration::from_millis(300)).get(&slow_server(1, Duration::ZERO, Duration::from_secs(1))).call().unwrap();
        let mut body = Vec::new();
        let error = response.body_mut().as_reader().read_to_end(&mut body).unwrap_err();
        assert!(matches!(error.get_ref().and_then(|inner| inner.downcast_ref::<ureq::Error>()), Some(ureq::Error::Timeout(_))), "{error:?}");
    }

    #[test]
    fn a_read_waits_for_the_nearer_of_its_phase_and_the_idle_limit() {
        use ureq::unversioned::transport::{time, NextTimeout};
        let next = |after| NextTimeout { after, reason: ureq::Timeout::RecvBody };
        let idle = Duration::from_secs(1800);
        assert_eq!(idle_capped(next(time::Duration::NotHappening), idle), next(time::Duration::Exact(idle)));
        assert_eq!(idle_capped(next(time::Duration::from_secs(600)), idle), next(time::Duration::from_secs(600)));
        assert_eq!(idle_capped(next(time::Duration::from_secs(7200)), idle), next(time::Duration::Exact(idle)));
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
    fn the_private_networks_are_the_local_ones() {
        let private = |text: &str| is_private(text.parse().unwrap());
        for ip in ["10.0.0.1", "10.255.255.255", "172.16.0.1", "172.31.255.254", "192.168.0.22", "100.64.0.1", "100.127.255.1",
                   "169.254.169.254", "fd7d:a3ce:f5bb:53b5::1", "fc00::1", "fe80::1", "::ffff:192.168.1.1"] {
            assert!(private(ip), "{ip}");
        }
        for ip in ["8.8.8.8", "172.15.0.1", "172.32.0.1", "192.169.0.1", "100.63.0.1", "100.128.0.1", "11.0.0.1",
                   "127.0.0.1", "192.0.2.1", "2606:4700::6810:84e5", "::1", "::ffff:1.1.1.1"] {
            assert!(!private(ip), "{ip}");
        }
    }

    #[test]
    fn private_networks_denied_or_alone_by_address() {
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        let deny = Policy { private_networks: PrivateNetworks::Deny, ..Policy::default() };
        let only = Policy { private_networks: PrivateNetworks::Only, ..Policy::default() };
        assert_eq!(Policy::default().refuses_address("nas.lan", ip("192.168.0.22")), None);
        assert_eq!(deny.refuses_address("192.168.0.22", ip("192.168.0.22")).unwrap(), "192.168.0.22 is in a private network (privateNetworks is deny)");
        assert_eq!(
            deny.refuses_address("nas.example.com", ip("10.0.0.5")).unwrap(),
            "\"nas.example.com\" is 10.0.0.5, which is in a private network (privateNetworks is deny)"
        );
        assert_eq!(deny.refuses_address("example.com", ip("93.184.215.14")), None);
        assert!(only.refuses_address("example.com", ip("93.184.215.14")).unwrap().contains("is not in a private network (privateNetworks is only)"));
        assert_eq!(only.refuses_address("nas.lan", ip("192.168.0.22")), None);
        // Loopback is allow_loopback's question in either mode.
        assert_eq!(deny.refuses_address("localhost", ip("127.0.0.1")), None);
        assert_eq!(only.refuses_address("localhost", ip("::1")), None);
        // A host the allow list names exactly is exempt; one a wildcard lets through is not.
        let named = |mode| Policy { allow: vec!["*".into(), "*.corp.test".into(), "nas.corp.test".into(), "api.example.com".into(), "10.0.0.7".into()], private_networks: mode, ..Policy::default() };
        assert_eq!(named(PrivateNetworks::Deny).refuses_address("nas.corp.test", ip("10.0.0.5")), None);
        assert_eq!(named(PrivateNetworks::Deny).refuses_address("10.0.0.7", ip("10.0.0.7")), None);
        assert!(named(PrivateNetworks::Deny).refuses_address("git.corp.test", ip("10.0.0.6")).is_some());
        assert_eq!(named(PrivateNetworks::Only).refuses_address("API.example.com.", ip("93.184.215.14")), None);
        assert!(named(PrivateNetworks::Only).refuses_address("other.example.com", ip("93.184.215.14")).is_some());
    }

    #[test]
    fn an_address_in_the_url_is_checked_with_the_lists() {
        let deny = Policy { private_networks: PrivateNetworks::Deny, ..Policy::default() };
        assert!(deny.refuses("192.168.0.22").unwrap().starts_with("blocked by the network policy: 192.168.0.22 is in a private network"));
        assert!(deny.refuses("[fd00::1]").unwrap().contains("private network"));
        assert_eq!(deny.refuses("example.com"), None);
        let only = Policy { private_networks: PrivateNetworks::Only, ..Policy::default() };
        assert!(only.refuses("1.1.1.1").unwrap().contains("not in a private network"));
        // The lists come first: a deny rule says so itself.
        let both = Policy { deny: vec!["10.0.0.1".into()], ..only.clone() };
        assert!(both.refuses("10.0.0.1").unwrap().contains("deny rule"));
        assert!(PrivateNetworks::parse(" DENY ") == Some(PrivateNetworks::Deny) && PrivateNetworks::parse("block").is_none());
    }

    #[test]
    fn address_rules_are_ips_and_cidr_blocks() {
        let rule = |text: &str| AddressRule::parse(text).unwrap().unwrap();
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        assert!(rule("10.0.0.0/8").contains(ip("10.255.0.1")) && !rule("10.0.0.0/8").contains(ip("11.0.0.1")));
        assert!(rule("192.168.1.0/24").contains(ip("192.168.1.200")) && !rule("192.168.1.0/24").contains(ip("192.168.2.1")));
        assert!(rule("192.168.1.7").contains(ip("192.168.1.7")) && !rule("192.168.1.7").contains(ip("192.168.1.8")));
        assert!(rule("0.0.0.0/0").contains(ip("8.8.8.8")) && !rule("0.0.0.0/0").contains(ip("2606:4700::1")));
        assert!(rule("fd00::/8").contains(ip("fd12::1")) && !rule("fd00::/8").contains(ip("fe80::1")));
        assert!(rule("[::1]").contains(ip("::1")) && rule("2001:db8::/32").contains(ip("2001:db8:ffff::1")));
        // IPv4 written as IPv6 is IPv4, in the address and in the rule.
        assert!(rule("10.0.0.0/8").contains(ip("::ffff:10.1.2.3")));
        assert_eq!(rule("::ffff:10.0.0.0/104"), rule("10.0.0.0/8"));
        assert!(rule("::ffff:192.168.0.22").contains(ip("192.168.0.22")));
        // Host patterns are not address rules; what has a "/" must be a CIDR block.
        for pattern in ["example.com", "*.example.com", "*", "10.0.0.1.nip.io"] {
            assert_eq!(AddressRule::parse(pattern), Ok(None), "{pattern}");
        }
        assert!(AddressRule::parse("10.0.0.0/33").unwrap_err().contains("0 to 32"));
        assert!(AddressRule::parse("fd00::/129").unwrap_err().contains("0 to 128"));
        assert!(AddressRule::parse("10.0.0.0/x").is_err() && AddressRule::parse("10.0.0.0/").is_err());
        assert_eq!(AddressRule::parse("10.1.2.3/8").unwrap_err(), "\"10.1.2.3/8\" has bits set past its /8 (the block is 10.0.0.0/8)");
        assert!(AddressRule::parse("example.com/24").unwrap_err().contains("not a CIDR block"));
        // The lists' entries: these or host patterns, nothing that would match nothing.
        for good in ["example.com", "*.example.com", "*", "10.0.0.0/8", "::1", "192.168.0.22", "192.168.1.*"] {
            assert_eq!(check_rule(good), Ok(()), "{good}");
        }
        for bad in ["*.*.example.com", "api*.example.com", "", "10.0.0.0/40", "*.*.*.*"] {
            assert!(check_rule(bad).is_err(), "{bad}");
        }
        assert!(check_rule("api*.example.com").unwrap_err().contains("CIDR block such as 192.168.1.0/24 or an IPv4 address ending in *"));
    }

    #[test]
    fn ipv4_wildcards_are_the_cidr_blocks_of_their_last_octets() {
        let rule = |text: &str| AddressRule::parse(text).unwrap().unwrap();
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        assert_eq!(rule("192.168.1.*"), rule("192.168.1.0/24"));
        assert_eq!(rule("192.168.*"), rule("192.168.0.0/16"));
        assert_eq!(rule("192.168.*.*"), rule("192.168.0.0/16"));
        assert_eq!(rule("10.*"), rule("10.0.0.0/8"));
        assert_eq!(rule("10.*.*.*"), rule("10.0.0.0/8"));
        assert_eq!(rule(" 0.* "), rule("0.0.0.0/8"));
        assert!(rule("192.168.1.*").contains(ip("192.168.1.254")) && !rule("192.168.1.*").contains(ip("192.168.2.1")));
        assert!(rule("10.*").contains(ip("::ffff:10.9.9.9")));
        // Host patterns stay host patterns.
        for pattern in ["*", "*.example.com", "*.10.in-addr.arpa"] {
            assert_eq!(AddressRule::parse(pattern), Ok(None), "{pattern}");
        }
        // No CIDR block is a * before a number, part of an octet, or more than four octets.
        for wrong in ["10.*.0.1", "*.168.1.1", "*.10", "192.168.1*", "192.168.1.1*", "192.168.1.1.*", "256.*", "010.*", "192..*"] {
            let error = check_rule(wrong).unwrap_err();
            assert!(error.contains("192.168.1.*") || error.contains("a host is"), "{wrong}: {error}");
        }
        assert!(AddressRule::parse("10.*.0.1").unwrap_err().contains("* stands for whole octets at the end of an IPv4 address"));
        // IPv6 takes a CIDR block; a CIDR block takes no *.
        for wrong in ["fd00::*", "[fd00::*]", "fe80:*::1", "*::1"] {
            assert!(AddressRule::parse(wrong).unwrap_err().contains("IPv6 takes no *, only a CIDR block"), "{wrong}");
        }
        assert!(AddressRule::parse("192.168.*/16").unwrap_err().contains("a CIDR block takes no *"));
        // By address in the lists, as any address rule: a name into the block is refused, and
        // an allow wildcard is exempt from privateNetworks.
        let deny = Policy { deny: vec!["10.*".into()], ..Policy::default() };
        assert_eq!(deny.refuses_host("10.1.2.3").unwrap(), "\"10.1.2.3\" matches the deny rule \"10.*\"");
        assert_eq!(deny.refuses_address("x.nip.io", ip("10.1.2.3")).unwrap(), "\"x.nip.io\" is 10.1.2.3, which matches the deny rule \"10.*\"");
        assert_eq!(deny.refuses_host("11.1.2.3"), None);
        let subnet = Policy { allow: vec!["*".into(), "192.168.1.*".into()], private_networks: PrivateNetworks::Deny, ..Policy::default() };
        assert_eq!(subnet.refuses_address("nas.lan", ip("192.168.1.40")), None);
        assert!(subnet.refuses_address("nas.lan", ip("192.168.2.40")).is_some());
    }

    #[test]
    fn address_rules_in_the_lists_go_by_address() {
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        // By address, whether the host is written as one or a name resolves to it.
        let deny = Policy { deny: vec!["10.0.0.0/8".into(), "192.168.0.22".into()], ..Policy::default() };
        assert_eq!(deny.refuses_host("10.9.8.7").unwrap(), "\"10.9.8.7\" matches the deny rule \"10.0.0.0/8\"");
        assert_eq!(deny.refuses_host("10.9.8.7.nip.io"), None); // a name: its address decides
        assert_eq!(
            deny.refuses_address("10.9.8.7.nip.io", ip("10.9.8.7")).unwrap(),
            "\"10.9.8.7.nip.io\" is 10.9.8.7, which matches the deny rule \"10.0.0.0/8\""
        );
        assert_eq!(deny.refuses_address("10.9.8.7", ip("10.9.8.7")).unwrap(), "10.9.8.7 matches the deny rule \"10.0.0.0/8\"");
        assert!(deny.refuses_address("nas.lan", ip("192.168.0.22")).unwrap().contains("deny rule \"192.168.0.22\""));
        assert_eq!(deny.refuses_address("nas.lan", ip("192.168.0.23")), None);
        assert!(deny.refuses("[::ffff:10.0.0.1]").unwrap().contains("deny rule \"10.0.0.0/8\""));
        assert!(deny.refuses_resolved("x.test", ip("::ffff:10.0.0.1")).unwrap().contains("deny rule"));
        // Deny wins over an exact allow too, as it does for names.
        let both = Policy { allow: vec!["*".into(), "nas.corp.test".into()], ..deny.clone() };
        assert!(both.refuses_address("nas.corp.test", ip("10.0.0.5")).is_some());
        // Loopback stays allow_loopback's.
        let all = Policy { deny: vec!["0.0.0.0/0".into(), "::/0".into()], allow_loopback: true, ..Policy::default() };
        assert_eq!(all.refuses_resolved("localhost", ip("127.0.0.1")), None);
        assert!(all.refuses_resolved("example.com", ip("93.184.215.14")).is_some());
        // In the allow list: the addresses in it, by address only; a name still needs a pattern.
        let allow = Policy { allow: vec!["192.168.1.0/24".into(), "fd00::/8".into()], ..Policy::default() };
        assert_eq!(allow.refuses_host("192.168.1.40"), None);
        assert_eq!(allow.refuses("[fd00::9]"), None);
        assert!(allow.refuses_host("192.168.2.40").unwrap().contains("not in the allow list"));
        assert!(allow.refuses_host("nas.lan").unwrap().contains("not in the allow list"));
        assert!(!allow.unlisted("192.168.1.40") && allow.unlisted("192.168.2.40") && allow.unlisted("nas.lan"));
        // ...and they are exempt from privateNetworks, by any name: the LAN's one subnet.
        let subnet = Policy { allow: vec!["*".into(), "192.168.1.0/24".into()], private_networks: PrivateNetworks::Deny, ..Policy::default() };
        assert_eq!(subnet.refuses_address("nas.lan", ip("192.168.1.40")), None);
        assert_eq!(subnet.refuses_address("192.168.1.40", ip("192.168.1.40")), None);
        assert!(subnet.refuses_address("192.168.2.40", ip("192.168.2.40")).unwrap().contains("privateNetworks is deny"));
        assert!(subnet.refuses_address("metadata", ip("169.254.169.254")).is_some());
        let public = Policy { allow: vec!["*".into(), "1.1.1.0/24".into()], private_networks: PrivateNetworks::Only, ..Policy::default() };
        assert_eq!(public.refuses_address("one.one.one.one", ip("1.1.1.1")), None);
        assert!(public.refuses_address("dns.google", ip("8.8.8.8")).is_some());
    }

    #[test]
    fn the_resolver_keeps_the_addresses_the_rule_lets_through() {
        use ureq::unversioned::resolver::Resolver;
        let resolver = |mode| PolicyResolver { policy: Arc::new(RwLock::new(Policy { private_networks: mode, ..Policy::default() })), inner: Default::default() };
        let config = ureq::config::Config::builder().proxy(None).build();
        let resolve = |resolver: &PolicyResolver, url: &str| resolver.resolve(&url.parse().unwrap(), &config, ureq::unversioned::transport::NextTimeout { after: ureq::unversioned::transport::time::Duration::NotHappening, reason: ureq::Timeout::Resolve });
        let found = resolve(&resolver(PrivateNetworks::Allow), "http://192.168.0.22:8080/").unwrap();
        assert_eq!(found.iter().map(|a| a.to_string()).collect::<Vec<_>>(), ["192.168.0.22:8080"]);
        // What handle() tells the guest: the refusal itself (ureq's own text adds "other: ").
        let reason = |error: ureq::Error| match error {
            ureq::Error::Other(error) if error.is::<Refused>() => error.to_string(),
            other => panic!("not a refusal: {other}"),
        };
        let refused = reason(resolve(&resolver(PrivateNetworks::Deny), "http://192.168.0.22:8080/").unwrap_err());
        assert_eq!(refused, "blocked by the network policy: 192.168.0.22 is in a private network (privateNetworks is deny)");
        assert!(reason(resolve(&resolver(PrivateNetworks::Only), "https://8.8.8.8/").unwrap_err()).contains("not in a private network"));
        assert_eq!(resolve(&resolver(PrivateNetworks::Only), "http://10.1.2.3/").unwrap().len(), 1);
        // A live policy: an update applies to the next resolve.
        let live = resolver(PrivateNetworks::Deny);
        live.policy.write().unwrap().private_networks = PrivateNetworks::Allow;
        assert_eq!(resolve(&live, "http://10.1.2.3/").unwrap().len(), 1);
        // A deny list CIDR block, on what was resolved (here a decimal 10.1.2.3, which only the
        // resolver reads as an address).
        live.policy.write().unwrap().deny = vec!["10.0.0.0/8".into()];
        assert_eq!(reason(resolve(&live, "http://167838211/").unwrap_err()), "blocked by the network policy: \"167838211\" is 10.1.2.3, which matches the deny rule \"10.0.0.0/8\"");
    }

    #[test]
    fn this_computer_by_the_address_a_name_resolves_to() {
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        let policy = Policy::default();
        assert_eq!(policy.refuses_resolved("127.0.0.1.nip.io", ip("127.0.0.1")).unwrap(), "\"127.0.0.1.nip.io\" is 127.0.0.1, this computer (allowHostLoopback is off)");
        assert_eq!(policy.refuses_resolved("::ffff:127.0.0.1", ip("::ffff:127.0.0.1")).unwrap(), "::ffff:127.0.0.1 is this computer (allowHostLoopback is off)");
        for address in ["127.1.2.3", "::1", "0.0.0.0", "::", "::ffff:0.0.0.0"] {
            assert!(policy.refuses_resolved("x.test", ip(address)).is_some(), "{address}");
        }
        assert_eq!(policy.refuses_resolved("example.com", ip("93.184.215.14")), None);
        assert_eq!(Policy { allow_loopback: true, ..Policy::default() }.refuses_resolved("x.test", ip("127.0.0.1")), None);
        // Loopback is not a private network: only allow_loopback decides it.
        let only = Policy { allow_loopback: true, private_networks: PrivateNetworks::Only, ..Policy::default() };
        assert_eq!(only.refuses_resolved("x.test", ip("::ffff:127.0.0.1")), None);
        assert!(only.refuses_resolved("x.test", ip("8.8.8.8")).is_some());
    }

    #[test]
    fn a_location_is_joined_to_the_url_it_came_from() {
        let join = |base: &str, location: &str| join_location(base, location);
        let base = "https://a.test:8443/v1/models/list?page=2";
        assert_eq!(join(base, "https://b.test/x").as_deref(), Some("https://b.test/x"));
        assert_eq!(join(base, "//b.test/x?y").as_deref(), Some("https://b.test/x?y"));
        assert_eq!(join(base, "/root?q=1#frag").as_deref(), Some("https://a.test:8443/root?q=1"));
        assert_eq!(join(base, "other").as_deref(), Some("https://a.test:8443/v1/models/other"));
        assert_eq!(join(base, "../up/./here").as_deref(), Some("https://a.test:8443/v1/up/here"));
        assert_eq!(join(base, "../../../../top").as_deref(), Some("https://a.test:8443/top"));
        assert_eq!(join(base, "?page=3").as_deref(), Some("https://a.test:8443/v1/models/list?page=3"));
        assert_eq!(join("http://a.test", "x").as_deref(), Some("http://a.test/x"));
        assert_eq!(join("http://a.test/dir/", "..").as_deref(), Some("http://a.test/"));
        assert_eq!(join(base, "ftp://b.test/").as_deref(), Some("ftp://b.test/")); // split_url refuses it
        assert_eq!(join(base, "  "), None);
    }

    #[test]
    fn origins_and_the_methods_redirects_go_on_with() {
        assert_eq!(origin("https://A.test/x"), "https://a.test:443");
        assert_eq!(origin("https://a.test:443/y?z"), "https://a.test:443");
        assert_eq!(origin("http://[::1]/"), "http://[::1]:80");
        assert_eq!(origin("http://[::1]:8080/"), "http://[::1]:8080");
        assert_ne!(origin("http://a.test/"), origin("https://a.test/"));
        assert_ne!(origin("http://a.test:1/"), origin("http://a.test:2/"));
        assert_eq!(redirect_method("GET", 301).as_deref(), Some("GET"));
        assert_eq!(redirect_method("HEAD", 303).as_deref(), Some("HEAD"));
        assert_eq!(redirect_method("POST", 302).as_deref(), Some("GET"));
        assert_eq!(redirect_method("DELETE", 303).as_deref(), Some("GET"));
        assert_eq!(redirect_method("GET", 307).as_deref(), Some("GET"));
        assert_eq!(redirect_method("POST", 307), None);
        assert_eq!(redirect_method("PUT", 308), None);
        assert_eq!(redirect_method("DELETE", 307), None);
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
    fn extra_headers_from_the_command_line() {
        assert_eq!(parse_header_names("X-Auth-Token, cf-access-client-id").unwrap(), ["x-auth-token", "cf-access-client-id"]);
        assert!(parse_header_names(" , ").is_err());
        assert!(parse_header_names("x auth").is_err());
        let policy = Policy { extra_headers: parse_header_names("x-auth-token").unwrap(), ..Policy::default() };
        assert_eq!(parse_head("POST https://mcp.test/mcp\nX-Auth-Token: t\n", &policy).unwrap().headers, vec![("x-auth-token".to_string(), "t".to_string())]);
        assert_eq!(parse_head("POST https://mcp.test/mcp\nx-other: t\n", &policy).unwrap_err().kind, "header-not-allowed");
    }

    #[test]
    fn the_certificate_check_is_skipped_only_when_asked() {
        let policy = Policy::default();
        assert!(parse_head("GET https://example.com/\n", &policy).unwrap().verify_tls);
        // For the host, not the server: taken out of the headers, and allowed without the list.
        let request = parse_head("POST https://localhost:8443/mcp\nx-collabo-tls-verify: false\naccept: */*\n", &policy).unwrap();
        assert!(!request.verify_tls);
        assert_eq!(request.headers, vec![("accept".to_string(), "*/*".to_string())]);
        assert!(parse_head("GET https://example.com/\nX-Collabo-TLS-Verify: true\n", &policy).unwrap().verify_tls);
        assert_eq!(parse_head("GET https://example.com/\nx-collabo-tls-verify: no\n", &policy).unwrap_err().kind, "bad-request");
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
