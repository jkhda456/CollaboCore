//! API keys for the guest's own HTTPS clients (curl, git, python), added on this side.
//!
//! The request API (`http.rs`) has always put the app's `secrets` on requests the guest makes
//! through it. Programs that open their own TLS connections over the NIC could not get them: TLS
//! runs end to end. For hosts that have a secret, this module takes the connection instead:
//!
//!   guest ──TLS (a certificate for that host from the session CA)──▶ here
//!         here: the request head gets the secret headers (a guest's own copy of them is dropped)
//!   here ──TLS (verified against the real roots, for the host the guest named)──▶ the host
//!
//! The session CA is made at start, lives only in this process, and the guest trusts it
//! (/etc/ssl/cert.pem gets it at boot). Every other host's TLS still goes end to end untouched;
//! so does whatever is not TLS on a secret host's ports (ssh to github.com:22, plain HTTP).
//!
//! One request per connection: the request goes on with `Connection: close`, so a client that
//! wants another request opens another connection, and each one gets the headers. HTTP/1.1 only
//! (the certificate offers no h2), which curl, git and python fall back to.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::http::Policy;

const MAX_HEAD: usize = 64 * 1024;
const IDLE: Duration = Duration::from_secs(120);

/// What the app hears about an intercepted request.
pub struct Event {
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// How many secret headers went with it.
    pub secrets: usize,
}

pub type Observer = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Interceptor {
    ca_pem: String,
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    policy: Arc<RwLock<Policy>>,
    observer: Option<Observer>,
}

/// Makes (and keeps) a certificate for each host name the guest asks for.
struct Certificates {
    issuer: Issuer<'static, KeyPair>,
    ca_der: CertificateDer<'static>,
    made: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Certificates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Certificates")
    }
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_lowercase();
        if let Some(made) = self.made.lock().unwrap().get(&name) {
            return Some(made.clone());
        }
        let made = self.make(&name).map_err(|error| eprintln!("collabo-core: certificate for {name}: {error:#}")).ok()?;
        self.made.lock().unwrap().insert(name, made.clone());
        Some(made)
    }
}

impl Certificates {
    fn make(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![name.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, name);
        (params.not_before, params.not_after) = validity();
        // OpenSSL's strict verification (Python's default context) wants the issuer named.
        params.use_authority_key_identifier_extension = true;
        let certificate = params.signed_by(&key, &self.issuer)?;
        let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = rustls::crypto::ring::sign::any_supported_type(&private)?;
        Ok(Arc::new(CertifiedKey::new(vec![certificate.der().clone(), self.ca_der.clone()], signing)))
    }
}

/// From yesterday to a month from now: long enough for a session, short enough for any client.
fn validity() -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (now - time::Duration::days(1), now + time::Duration::days(30))
}

impl Interceptor {
    pub fn new(policy: Arc<RwLock<Policy>>, observer: Option<Observer>) -> Result<Arc<Interceptor>> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(DnType::CommonName, "collaboCore sandbox session CA");
        params.distinguished_name.push(DnType::OrganizationName, "collaboCore");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        (params.not_before, params.not_after) = validity();
        let ca = params.self_signed(&key)?;
        let certificates = Certificates {
            ca_der: ca.der().clone(),
            issuer: Issuer::new(params, key),
            made: Mutex::new(HashMap::new()),
        };

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(certificates));
        server.alpn_protocols = vec![b"http/1.1".to_vec()];

        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_store()?)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Arc::new(Interceptor {
            ca_pem: ca.pem(),
            server: Arc::new(server),
            client: Arc::new(client),
            policy,
            observer,
        }))
    }

    /// The session CA, for the guest's trust store.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Takes a connection the guest opened to `destination`: returns the socket the packet
    /// stack relays the guest's bytes to, with this module on its other end.
    pub fn attach(self: &Arc<Interceptor>, destination: SocketAddr) -> std::io::Result<TcpStream> {
        // A pair of connected sockets (std has no socketpair on Windows): a one-off listener.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let near = TcpStream::connect(listener.local_addr()?)?;
        let (far, _) = listener.accept()?;
        let this = self.clone();
        std::thread::spawn(move || {
            if let Err(error) = this.serve(far, destination) {
                eprintln!("collabo-core: intercept {destination}: {error:#}");
            }
        });
        Ok(near)
    }

    fn serve(&self, guest: TcpStream, destination: SocketAddr) -> Result<()> {
        // TLS starts with the client's handshake record (0x16). Anything else, or a protocol
        // where the server speaks first, goes through untouched.
        guest.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut first = [0u8; 1];
        let tls = matches!(guest.peek(&mut first), Ok(1) if first[0] == 0x16);
        guest.set_read_timeout(Some(IDLE))?;
        if !tls {
            return pass_through(guest, destination);
        }

        let mut guest = rustls::StreamOwned::new(rustls::ServerConnection::new(self.server.clone())?, guest);
        while guest.conn.is_handshaking() {
            guest.conn.complete_io(&mut guest.sock)?;
        }
        let Some(host) = guest.conn.server_name().map(str::to_lowercase) else {
            bail!("a TLS connection without a server name");
        };
        // The guest's connection was allowed for the address; the name it now claims must be
        // allowed too, since that is where this goes and whose keys it gets.
        let policy = self.policy.read().unwrap().clone();
        let loopback = destination.ip().is_loopback();
        if !loopback || !policy.allow_loopback {
            if let Some(reason) = policy.refuses_host(&host) {
                bail!("{host}: {reason}");
            }
        }
        let secrets = policy.secrets_for_host(&host);

        // The request head, with the secrets in.
        let mut reader = BufReader::new(&mut guest);
        let head = read_head(&mut reader)?;
        let Some((request_line, headers)) = head.split_once("\r\n") else { bail!("no request line") };
        let mut parts = request_line.split(' ');
        let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
        let url = format!("https://{host}{}{target}", if destination.port() == 443 { String::new() } else { format!(":{}", destination.port()) });
        let mut out_head = format!("{request_line}\r\n");
        let (mut length, mut chunked) = (None::<u64>, false);
        for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
            let Some((name, value)) = line.split_once(':') else { continue };
            let name_lower = name.trim().to_lowercase();
            match name_lower.as_str() {
                "content-length" => length = value.trim().parse().ok(),
                "transfer-encoding" => chunked = value.to_lowercase().contains("chunked"),
                _ => {}
            }
            // Framing of the connection is ours; a 100-continue wait would stall this one-way
            // relay (clients send the body after a second anyway).
            if ["connection", "keep-alive", "proxy-connection", "expect"].contains(&name_lower.as_str())
                || secrets.iter().any(|secret| secret.header == name_lower)
            {
                continue;
            }
            out_head.push_str(line);
            out_head.push_str("\r\n");
        }
        for secret in &secrets {
            out_head.push_str(&format!("{}: {}\r\n", secret.header, secret.value));
        }
        out_head.push_str("Connection: close\r\n\r\n");

        let report = |status: Option<u16>, error: Option<String>| {
            if let Some(observer) = &self.observer {
                observer(Event { method: method.clone(), url: url.clone(), status, error: error.map(|e| policy.redact_text(&e)), secrets: secrets.len() });
            }
        };

        // To the real host, verified for the name the guest used.
        let upstream = (|| -> Result<_> {
            let socket = TcpStream::connect_timeout(&destination, Duration::from_secs(10))?;
            socket.set_read_timeout(Some(IDLE))?;
            let name = ServerName::try_from(host.clone())?;
            let mut upstream = rustls::StreamOwned::new(rustls::ClientConnection::new(self.client.clone(), name)?, socket);
            upstream.write_all(out_head.as_bytes())?;
            copy_body(&mut reader, &mut upstream, if chunked { Body::Chunked } else { Body::Length(length.unwrap_or(0)) })?;
            upstream.flush()?;
            Ok(upstream)
        })();
        let mut upstream = match upstream {
            Ok(upstream) => upstream,
            Err(error) => {
                report(None, Some(format!("{error:#}")));
                let _ = guest.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                return Err(error);
            }
        };

        // The response, as it comes, with our framing of the connection.
        let mut reader = BufReader::new(&mut upstream);
        let head = read_head(&mut reader)?;
        let (status_line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
        let status: u16 = status_line.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
        let mut out_head = format!("{status_line}\r\n");
        let (mut length, mut chunked) = (None::<u64>, false);
        for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
            let Some((name, value)) = line.split_once(':') else { continue };
            match name.trim().to_lowercase().as_str() {
                "content-length" => length = value.trim().parse().ok(),
                "transfer-encoding" => chunked = value.to_lowercase().contains("chunked"),
                "connection" | "keep-alive" => continue,
                _ => {}
            }
            out_head.push_str(line);
            out_head.push_str("\r\n");
        }
        out_head.push_str("Connection: close\r\n\r\n");
        guest.write_all(out_head.as_bytes())?;
        let body = if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
            Body::Length(0)
        } else if chunked {
            Body::Chunked
        } else {
            length.map(Body::Length).unwrap_or(Body::UntilEnd)
        };
        let copied = copy_body(&mut reader, &mut guest, body);
        report(Some(status), copied.as_ref().err().map(|error| format!("{error:#}")));
        guest.flush()?;
        guest.conn.send_close_notify();
        let _ = guest.flush();
        copied
    }
}

/// The guest's own trust for the real hosts: Mozilla's roots, or COLLABO_CORE_CA_FILE instead
/// (as for the request API).
fn root_store() -> Result<rustls::RootCertStore> {
    let mut store = rustls::RootCertStore::empty();
    match std::env::var_os("COLLABO_CORE_CA_FILE") {
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(path) => {
            use rustls::pki_types::pem::PemObject;
            for certificate in CertificateDer::pem_file_iter(&path).with_context(|| format!("reading {}", path.to_string_lossy()))? {
                store.add(certificate?)?;
            }
        }
    }
    Ok(store)
}

fn pass_through(guest: TcpStream, destination: SocketAddr) -> Result<()> {
    let host = TcpStream::connect_timeout(&destination, Duration::from_secs(10))?;
    let (mut guest_in, mut host_out) = (guest.try_clone()?, host.try_clone()?);
    let upward = std::thread::spawn(move || {
        let _ = std::io::copy(&mut guest_in, &mut host_out);
        let _ = host_out.shutdown(std::net::Shutdown::Write);
    });
    let (mut host_in, mut guest_out) = (host, guest);
    let _ = std::io::copy(&mut host_in, &mut guest_out);
    let _ = guest_out.shutdown(std::net::Shutdown::Write);
    let _ = upward.join();
    Ok(())
}

fn read_head(reader: &mut impl BufRead) -> Result<String> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        reader.read_until(b'\n', &mut head)?;
        if head.len() == before {
            bail!("the connection closed before the head ended");
        }
        if head.ends_with(b"\r\n\r\n") || head == b"\r\n" {
            break;
        }
        if head.len() > MAX_HEAD {
            bail!("a head larger than {MAX_HEAD} bytes");
        }
    }
    Ok(String::from_utf8_lossy(&head[..head.len() - 2]).into_owned())
}

enum Body {
    Length(u64),
    Chunked,
    UntilEnd,
}

/// Copies one message body as its framing says, framing included.
fn copy_body(from: &mut impl BufRead, to: &mut impl Write, body: Body) -> Result<()> {
    match body {
        Body::Length(length) => {
            let copied = std::io::copy(&mut from.take(length), to)?;
            if copied != length {
                bail!("the body ended after {copied} of {length} bytes");
            }
        }
        Body::UntilEnd => {
            // A server that ends the body by closing may skip TLS close_notify; what arrived is
            // the body either way.
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match from.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => to.write_all(&buffer[..read])?,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Body::Chunked => loop {
            let mut line = Vec::new();
            from.read_until(b'\n', &mut line)?;
            to.write_all(&line)?;
            let size = std::str::from_utf8(&line)
                .ok()
                .and_then(|text| u64::from_str_radix(text.trim().split(';').next().unwrap_or(""), 16).ok())
                .context("a malformed chunk size")?;
            if size == 0 {
                // Trailers, then the empty line.
                loop {
                    let mut trailer = Vec::new();
                    from.read_until(b'\n', &mut trailer)?;
                    to.write_all(&trailer)?;
                    if trailer == b"\r\n" || trailer.is_empty() {
                        return Ok(());
                    }
                }
            }
            std::io::copy(&mut from.take(size + 2), to)?;
        },
    }
    Ok(())
}
