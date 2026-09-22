//! The host's ssh-agent, lent to the guest (vsock port 1082, `collabo-sshagent` in the guest).
//!
//! ssh in the sandbox authenticates with keys that stay on this computer: the guest's
//! SSH_AUTH_SOCK is carried here, and each message goes on to the ssh-agent the user already runs
//! (OpenSSH's, 1Password's, …): `$SSH_AUTH_SOCK` on macOS and Linux, OpenSSH's named pipe on
//! Windows, or the socket the app names.
//!
//! Only two requests pass: listing the keys and signing with one. Adding, removing and locking
//! keys, and extensions, are answered with a failure here and never reach the agent. Under
//! `sshAgent: "ask"` every signature is a `permission` event (kind "ssh-agent") first; an answer
//! with remember holds for that key for the session.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Context, Result};

use crate::vsock::{Vsock, VsockStream};

pub const PORT: u32 = 1082;

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
/// Agent messages are small; a key list or a signature request is a few kilobytes.
const MAX_MESSAGE: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Off,
    Ask,
    Allow,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "off" => Some(Mode::Off),
            "ask" => Some(Mode::Ask),
            "allow" => Some(Mode::Allow),
            _ => None,
        }
    }
}

/// The app's `sshAgent` settings. Swappable while the guest runs.
#[derive(Clone)]
pub struct Policy {
    pub mode: Mode,
    /// The agent to use; None: `$SSH_AUTH_SOCK`, or OpenSSH's pipe on Windows.
    pub socket: Option<PathBuf>,
}

/// What the app hears about: a key listing or a signature, and whether it went through.
pub struct Event {
    pub op: &'static str,
    pub key: Option<String>,
    pub allowed: bool,
    pub reason: Option<String>,
}

/// Asks the app about one signature: target text → (allow, remember).
pub type Asker = Arc<dyn Fn(&str) -> Option<(bool, bool)> + Send + Sync>;

/// The app's remembered answers, by key fingerprint (cleared when the policy changes).
pub type Answers = Arc<Mutex<HashMap<String, bool>>>;

pub fn serve(
    vsock: &Vsock,
    policy: Arc<RwLock<Policy>>,
    allowed: Answers,
    asker: Asker,
    observer: Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    vsock.listen(PORT, move |stream| {
        let (policy, asker, observer, allowed) = (policy.clone(), asker.clone(), observer.clone(), allowed.clone());
        std::thread::spawn(move || {
            let mut session = Session { policy, asker, observer, allowed, agent: None, comments: HashMap::new() };
            if let Err(error) = session.run(&stream) {
                eprintln!("collabo-core: ssh-agent: {error:#}");
            }
        });
    })
}

/// The guest's connection as std::io, for the framing helpers.
struct Guest<'a>(&'a VsockStream);

impl Read for Guest<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer).map_err(std::io::Error::other)
    }
}

impl Write for Guest<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(data).map_err(std::io::Error::other)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

struct Session {
    policy: Arc<RwLock<Policy>>,
    asker: Asker,
    observer: Arc<dyn Fn(Event) + Send + Sync>,
    allowed: Answers,
    /// The host agent, opened at the first request that needs it.
    agent: Option<Box<dyn Duplex>>,
    /// Key comments from the last listing, to name a key when asking about it.
    comments: HashMap<Vec<u8>, String>,
}

impl Session {
    fn run(&mut self, stream: &VsockStream) -> Result<()> {
        let mut guest = Guest(stream);
        loop {
            let Some(request) = read_message(&mut guest)? else { return Ok(()) };
            let response = self.answer(&request);
            write_message(&mut guest, &response)?;
        }
    }

    fn answer(&mut self, request: &[u8]) -> Vec<u8> {
        let policy = self.policy.read().unwrap().clone();
        let refuse = |this: &Session, op: &'static str, key: Option<String>, reason: String| {
            (this.observer)(Event { op, key, allowed: false, reason: Some(reason) });
            vec![FAILURE]
        };
        if policy.mode == Mode::Off {
            return refuse(self, "list", None, "the app does not lend its ssh-agent (sshAgent: off)".into());
        }
        match request.first().copied() {
            Some(REQUEST_IDENTITIES) => match self.forward(&policy, request) {
                Ok(response) => {
                    self.comments = parse_identities(&response).unwrap_or_default();
                    (self.observer)(Event { op: "list", key: None, allowed: true, reason: None });
                    response
                }
                Err(error) => refuse(self, "list", None, format!("{error:#}")),
            },
            Some(SIGN_REQUEST) => {
                let Some(blob) = read_string(&request[1..]).map(|(blob, _)| blob.to_vec()) else {
                    return refuse(self, "sign", None, "a malformed signature request".into());
                };
                let key = describe_key(&blob, self.comments.get(&blob).map(String::as_str));
                if policy.mode == Mode::Ask && !self.permitted(&blob, &key) {
                    return refuse(self, "sign", Some(key), "the app did not allow this signature".into());
                }
                match self.forward(&policy, request) {
                    Ok(response) => {
                        (self.observer)(Event { op: "sign", key: Some(key), allowed: true, reason: None });
                        response
                    }
                    Err(error) => refuse(self, "sign", Some(key), format!("{error:#}")),
                }
            }
            Some(other) => refuse(self, "other", None, format!("agent request {other} is not lent to the sandbox")),
            None => vec![FAILURE],
        }
    }

    /// Under `ask`: the app's answer, or the one it gave for this key before.
    fn permitted(&self, blob: &[u8], key: &str) -> bool {
        let fingerprint = fingerprint(blob);
        if let Some(known) = self.allowed.lock().unwrap().get(&fingerprint) {
            return *known;
        }
        let (allow, remember) = (self.asker)(&format!("sign with {key}")).unwrap_or((false, false));
        if remember {
            self.allowed.lock().unwrap().insert(fingerprint, allow);
        }
        allow
    }

    /// One request to the host agent, one response back.
    fn forward(&mut self, policy: &Policy, request: &[u8]) -> Result<Vec<u8>> {
        if self.agent.is_none() {
            self.agent = Some(open_agent(policy.socket.as_ref())?);
        }
        let agent = self.agent.as_mut().unwrap();
        let exchanged = write_message(agent, request).and_then(|_| read_message(agent));
        match exchanged {
            Ok(Some(response)) => Ok(response),
            Ok(None) => {
                self.agent = None;
                bail!("the ssh-agent closed the connection")
            }
            Err(error) => {
                self.agent = None;
                Err(error)
            }
        }
    }
}

fn open_agent(socket: Option<&PathBuf>) -> Result<Box<dyn Duplex>> {
    #[cfg(unix)]
    {
        let path = match socket {
            Some(path) => path.clone(),
            None => PathBuf::from(std::env::var_os("SSH_AUTH_SOCK").context("no ssh-agent: SSH_AUTH_SOCK is not set")?),
        };
        let stream = std::os::unix::net::UnixStream::connect(&path)
            .with_context(|| format!("cannot reach the ssh-agent at {}", path.display()))?;
        Ok(Box::new(stream))
    }
    #[cfg(windows)]
    {
        // OpenSSH for Windows' agent service; a pipe opens like a file.
        let path = socket.cloned().unwrap_or_else(|| PathBuf::from(r"\\.\pipe\openssh-ssh-agent"));
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot reach the ssh-agent at {} (is the ssh-agent service running?)", path.display()))?;
        Ok(Box::new(pipe))
    }
}

fn read_message(stream: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE {
        bail!("an agent message of {length} bytes");
    }
    let mut message = vec![0u8; length];
    stream.read_exact(&mut message)?;
    Ok(Some(message))
}

fn write_message(stream: &mut (impl Write + ?Sized), message: &[u8]) -> Result<()> {
    let mut framed = Vec::with_capacity(4 + message.len());
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(message);
    stream.write_all(&framed)?;
    stream.flush()?;
    Ok(())
}

/// An SSH `string`: u32 length, bytes. Returns it and what follows.
fn read_string(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let length = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let rest = &data[4..];
    (rest.len() >= length).then(|| (&rest[..length], &rest[length..]))
}

/// SSH2_AGENT_IDENTITIES_ANSWER: u32 count, then (key blob, comment) pairs.
fn parse_identities(response: &[u8]) -> Option<HashMap<Vec<u8>, String>> {
    if response.first() != Some(&IDENTITIES_ANSWER) {
        return None;
    }
    let count = u32::from_be_bytes(response.get(1..5)?.try_into().ok()?);
    let mut rest = &response[5..];
    let mut keys = HashMap::new();
    for _ in 0..count.min(1024) {
        let (blob, after) = read_string(rest)?;
        let (comment, after) = read_string(after)?;
        keys.insert(blob.to_vec(), String::from_utf8_lossy(comment).into_owned());
        rest = after;
    }
    Some(keys)
}

/// OpenSSH's fingerprint: SHA256:<base64 without padding>.
fn fingerprint(blob: &[u8]) -> String {
    format!("SHA256:{}", base64_nopad(ring::digest::digest(&ring::digest::SHA256, blob).as_ref()))
}

fn base64_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, byte)| n | (*byte as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// "ssh-ed25519 SHA256:… (comment)", as ssh-add -l says it.
fn describe_key(blob: &[u8], comment: Option<&str>) -> String {
    let kind = read_string(blob).map(|(kind, _)| String::from_utf8_lossy(kind).into_owned()).unwrap_or_else(|| "key".into());
    match comment.filter(|comment| !comment.is_empty()) {
        Some(comment) => format!("{kind} {} ({comment})", fingerprint(blob)),
        None => format!("{kind} {}", fingerprint(blob)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_read_as_openssh_prints_them() {
        // An ed25519 public key blob (key bytes 1..=32); the expected value is Python's
        // "SHA256:" + base64(sha256(blob)) without padding, which is what ssh-keygen -l prints.
        let mut blob = vec![0, 0, 0, 11];
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&[0, 0, 0, 32]);
        blob.extend(1..=32u8);
        assert_eq!(
            describe_key(&blob, Some("me@laptop")),
            "ssh-ed25519 SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4 (me@laptop)"
        );
        assert_eq!(describe_key(&blob, None), "ssh-ed25519 SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4");
    }

    #[test]
    fn base64_without_padding() {
        assert_eq!(base64_nopad(b""), "");
        assert_eq!(base64_nopad(b"f"), "Zg");
        assert_eq!(base64_nopad(b"fo"), "Zm8");
        assert_eq!(base64_nopad(b"foo"), "Zm9v");
        assert_eq!(base64_nopad(b"foobar"), "Zm9vYmFy");
    }
}
