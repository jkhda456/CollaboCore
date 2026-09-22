//! Host functions for the guest (vsock 1081; the guest tools `hostcall` and
//! `collabo_core.host`). This is the sandbox's only way to touch the host computer, and every
//! call is checked here. A port of `runtime/src/host-functions.mjs`.
//!
//!   list                  the built-ins plus whatever the app registered
//!   info                  { platform, arch, hostExec } — nothing identifying
//!   exec {argv, cwd?, stdin?, timeoutMs?, gui?}   run a host program, governed by `hostExec`
//!   open {target}         open a file or URL with the host's default application
//!   <name>                a function the app registered: forwarded to it as a "hostCall"
//!                         event and answered with `reply`
//!
//! One JSON line each way:
//!   request   {"fn":"NAME","args":{...}}
//!   response  {"ok":true,"result":…} | {"ok":false,"error":{"kind":…,"message":…}}
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use crate::vsock::{Vsock, VsockStream};

pub const PORT: u32 = 1081;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

/// What the sandbox may do to the host computer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostExec {
    /// Refuse every host program (the default).
    Deny,
    /// Ask the app, which answers the permission event.
    Ask,
    Allow,
}

impl HostExec {
    pub fn parse(text: &str) -> HostExec {
        match text {
            "allow" => HostExec::Allow,
            "ask" => HostExec::Ask,
            _ => HostExec::Deny,
        }
    }

    fn name(self) -> &'static str {
        match self {
            HostExec::Allow => "allow",
            HostExec::Ask => "ask",
            HostExec::Deny => "deny",
        }
    }
}

/// The part of the app's configuration these calls obey. Swappable while the guest runs.
pub struct HostPolicy {
    pub host_exec: HostExec,
    /// The names the app said it answers.
    pub functions: Vec<String>,
    pub permission_timeout: Duration,
    pub call_timeout: Duration,
}

impl Default for HostPolicy {
    fn default() -> HostPolicy {
        HostPolicy {
            host_exec: HostExec::Deny,
            functions: Vec::new(),
            permission_timeout: Duration::from_secs(120),
            call_timeout: Duration::from_secs(120),
        }
    }
}

/// What the app is asked, and how it answers.
pub enum Ask {
    /// A function the app registered.
    Call { call_id: u64, function: String, args: Value },
    /// Permission to touch the host (kind "exec" or "open"), to reach a host the network policy
    /// does not name (kind "network", target "host:port"), or to use the host's ssh-agent
    /// (kind "ssh-agent").
    Permission { request_id: u64, kind: &'static str, argv: Vec<String>, cwd: Option<String>, gui: bool, target: Option<String> },
}

/// The app's answer to one of those.
pub enum Reply {
    Result(Value),
    Error { kind: String, message: String },
    /// Whether it may, and whether the answer holds for the rest of the session.
    Allow { allow: bool, remember: bool },
}

struct CallError {
    kind: String,
    message: String,
}

fn refuse<T>(kind: &str, message: impl Into<String>) -> std::result::Result<T, CallError> {
    Err(CallError { kind: kind.into(), message: message.into() })
}

type CallResult<T> = std::result::Result<T, CallError>;

/// The bridge: listens for the guest, asks the app when it has to, and runs host programs.
#[derive(Clone)]
pub struct HostFunctions {
    policy: Arc<RwLock<HostPolicy>>,
    waiting: Arc<Mutex<HashMap<u64, SyncSender<Reply>>>>,
    next_id: Arc<AtomicU64>,
    ask: Arc<dyn Fn(Ask) + Send + Sync>,
}

impl HostFunctions {
    pub fn new(policy: Arc<RwLock<HostPolicy>>, ask: Arc<dyn Fn(Ask) + Send + Sync>) -> HostFunctions {
        HostFunctions { policy, waiting: Arc::new(Mutex::new(HashMap::new())), next_id: Arc::new(AtomicU64::new(1)), ask }
    }

    /// Serves the guest on `port`, one thread per call.
    pub fn serve(&self, vsock: &Vsock, port: u32) -> Result<()> {
        let bridge = self.clone();
        vsock.listen(port, move |stream| {
            let bridge = bridge.clone();
            std::thread::spawn(move || bridge.handle(&stream));
        })
    }

    /// The app's answer. False when nothing was waiting for it.
    pub fn reply(&self, id: u64, reply: Reply) -> bool {
        let waiter = self.waiting.lock().unwrap().remove(&id);
        match waiter {
            Some(sender) => sender.send(reply).is_ok(),
            None => false,
        }
    }

    /// Every waiting call fails: the sandbox is going away.
    pub fn close(&self) {
        self.waiting.lock().unwrap().clear();
    }

    fn handle(&self, stream: &VsockStream) {
        let response = match self.read_request(stream) {
            Ok((function, args)) => match self.dispatch(&function, &args) {
                Ok(result) => json!({"ok": true, "result": result}),
                Err(error) => json!({"ok": false, "error": {"kind": error.kind, "message": error.message}}),
            },
            Err(error) => json!({"ok": false, "error": {"kind": error.kind, "message": error.message}}),
        };
        let _ = stream.write_all(format!("{response}\n").as_bytes());
        stream.close();
    }

    fn read_request(&self, stream: &VsockStream) -> CallResult<(String, Value)> {
        let mut text = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(at) = text.iter().position(|byte| *byte == b'\n') {
                text.truncate(at);
                break;
            }
            if text.len() > MAX_REQUEST {
                return refuse("bad-request", "request too large");
            }
            match stream.read(&mut chunk) {
                Ok(0) => return refuse("bad-request", "connection closed before the request line ended"),
                Ok(read) => text.extend_from_slice(&chunk[..read]),
                Err(error) => return refuse("bad-request", error.to_string()),
            }
        }
        let request: Value = match serde_json::from_slice(&text) {
            Ok(request) => request,
            Err(error) => return refuse("bad-request", format!("the request is not valid JSON: {error}")),
        };
        match request.get("fn").and_then(Value::as_str) {
            Some(function) => Ok((function.to_string(), request.get("args").cloned().unwrap_or_else(|| json!({})))),
            None => refuse("bad-request", "expected {\"fn\": \"...\", \"args\": {...}}"),
        }
    }

    fn dispatch(&self, function: &str, args: &Value) -> CallResult<Value> {
        let (host_exec, functions) = {
            let policy = self.policy.read().unwrap();
            (policy.host_exec, policy.functions.clone())
        };
        match function {
            "list" => {
                let mut names = vec!["list".to_string(), "info".into(), "exec".into(), "open".into()];
                names.extend(functions);
                Ok(json!(names))
            }
            "info" => Ok(json!({
                "platform": host_platform(),
                "arch": host_arch(),
                "hostExec": host_exec.name(),
            })),
            "exec" => {
                let argv: Vec<String> = args
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                if argv.is_empty() {
                    return refuse("bad-request", "argv must be a non-empty list of strings");
                }
                let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);
                let gui = args.get("gui").and_then(Value::as_bool).unwrap_or(false);
                self.check_access("exec", argv.clone(), cwd.clone(), gui, None)?;
                run_host_program(
                    &argv,
                    cwd.as_deref(),
                    args.get("stdin").and_then(Value::as_str),
                    args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(60_000),
                    gui,
                )
            }
            "open" => {
                let Some(target) = args.get("target").and_then(Value::as_str).filter(|target| !target.is_empty()) else {
                    return refuse("bad-request", "open needs a target");
                };
                self.check_access("open", Vec::new(), None, true, Some(target.to_string()))?;
                run_host_program(&open_command(target), None, None, 60_000, true)
            }
            other => {
                if !functions.iter().any(|name| name == other) {
                    return refuse("unknown-function", format!("the host offers no function \"{other}\" (hostcall --list)"));
                }
                let call_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let timeout = self.policy.read().unwrap().call_timeout;
                match self.ask_app(call_id, Ask::Call { call_id, function: other.to_string(), args: args.clone() }, timeout) {
                    Some(Reply::Result(result)) => Ok(result),
                    Some(Reply::Error { kind, message }) => Err(CallError { kind, message }),
                    Some(Reply::Allow { .. }) => refuse("failed", "the app answered a call with a permission"),
                    None => refuse("timeout", "the app did not answer"),
                }
            }
        }
    }

    /// Permission to touch the host, under the `hostExec` policy.
    fn check_access(&self, kind: &'static str, argv: Vec<String>, cwd: Option<String>, gui: bool, target: Option<String>) -> CallResult<()> {
        let (mode, timeout) = {
            let policy = self.policy.read().unwrap();
            (policy.host_exec, policy.permission_timeout)
        };
        match mode {
            HostExec::Allow => Ok(()),
            HostExec::Deny => refuse("denied", "running host programs is not allowed for this sandbox (hostExec policy)"),
            HostExec::Ask => {
                let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let asked = Ask::Permission { request_id, kind, argv, cwd, gui, target };
                match self.ask_app(request_id, asked, timeout) {
                    Some(Reply::Allow { allow: true, .. }) => Ok(()),
                    Some(_) => refuse("denied", "the user did not allow it"),
                    None => refuse("denied", "nobody answered the permission request"),
                }
            }
        }
    }

    /// Asks the app about something other than a host program (kind "network", "ssh-agent"),
    /// as a `permission` event. None when nobody answered in time.
    pub fn ask_permission(&self, kind: &'static str, target: &str) -> Option<(bool, bool)> {
        let timeout = self.policy.read().unwrap().permission_timeout;
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let asked = Ask::Permission { request_id, kind, argv: Vec::new(), cwd: None, gui: false, target: Some(target.to_string()) };
        match self.ask_app(request_id, asked, timeout)? {
            Reply::Allow { allow, remember } => Some((allow, remember)),
            _ => Some((false, false)),
        }
    }

    /// Sends the app a question and waits for `reply`, or gives up.
    fn ask_app(&self, id: u64, asked: Ask, timeout: Duration) -> Option<Reply> {
        let (sender, receiver) = sync_channel(1);
        self.waiting.lock().unwrap().insert(id, sender);
        (self.ask)(asked);
        match receiver.recv_timeout(timeout) {
            Ok(reply) => Some(reply),
            Err(_) => {
                self.waiting.lock().unwrap().remove(&id);
                None
            }
        }
    }
}

pub fn host_platform() -> &'static str {
    // The names Node reports, which the guest tools and the app already know.
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn open_command(target: &str) -> Vec<String> {
    let program = match host_platform() {
        "darwin" => "open",
        "win32" => "explorer.exe",
        _ => "xdg-open",
    };
    vec![program.to_string(), target.to_string()]
}

/// Runs a program on the host. `gui` starts it detached and captures nothing.
fn run_host_program(argv: &[String], cwd: Option<&str>, stdin: Option<&str>, timeout_ms: u64, gui: bool) -> CallResult<Value> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        command.current_dir(cwd);
    } else if let Some(home) = home_directory() {
        command.current_dir(home);
    }

    if gui {
        // Detached: the application outlives this call.
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        return match command.spawn() {
            Ok(child) => Ok(json!({"pid": child.id()})),
            Err(error) => refuse("failed", format!("{}: {error}", argv[0])),
        };
    }

    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return refuse("failed", format!("{}: {error}", argv[0])),
    };

    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin.unwrap_or("").as_bytes());
    }
    // Each stream on its own thread, so a full pipe cannot deadlock the other.
    let read = |mut pipe: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe.as_mut() {
                let _ = pipe.take(MAX_OUTPUT as u64).read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(child.stdout.take());
    let stderr = {
        let mut pipe = child.stderr.take();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe.as_mut() {
                let _ = pipe.take(MAX_OUTPUT as u64).read_to_end(&mut bytes);
            }
            bytes
        })
    };

    let limit = Duration::from_millis(timeout_ms.min(10 * 60_000));
    let deadline = std::time::Instant::now() + limit;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().map_err(|error| CallError { kind: "failed".into(), message: error.to_string() })?;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return refuse("failed", error.to_string()),
        }
    };

    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    // The order matters to the C client: exitCode first, stdout last.
    Ok(json!({
        "exitCode": status.code().unwrap_or(if timed_out { 137 } else { 1 }),
        "timedOut": timed_out,
        "stderr": String::from_utf8_lossy(&err),
        "stdout": String::from_utf8_lossy(&out),
    }))
}

fn home_directory() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()).filter(|home| !home.is_empty())
}
