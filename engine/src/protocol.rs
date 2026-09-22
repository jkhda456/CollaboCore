//! The control protocol between an app (the Flutter/Dart package) and this process:
//! newline-delimited JSON on stdin/stdout, stderr free-form. The same protocol the Node
//! runtime served (`runtime/src/protocol.mjs`), so the Dart package needs no change.
//!
//!   app -> engine   {"id": 1, "method": "exec", "params": {...}}
//!   engine -> app   {"id": 1, "result": {...}}  or  {"id": 1, "error": {"kind", "message"}}
//!   engine -> app   {"event": "console", ...}
//!
//! Methods: start, exec, readFile, writeFile, console.write, console.resize, policy.update,
//! reply, exportZip, stop. Events: ready, console, network, execOutput, hostCall, permission,
//! sshAgent, exit.
//!
//! start.config: cpus, python, tools, mounts, network {allow, deny, allowHostLoopback, secrets,
//! extraAllowedHeaders, ask}, hostExec, hostFunctions, permissionTimeoutMs, sshAgent ("off",
//! "ask", "allow"), sshAgentSocket, quiet, consoleSize. policy.update takes network, hostExec,
//! hostFunctions, sshAgent and sshAgentSocket.
//!
//! permission events (answered with reply {id, allow, remember?}): kind "exec" / "open" (hostExec:
//! ask), "network" (target "host:port" that network.ask leaves to the app), "ssh-agent" (target
//! "sign with <key>"). remember (default true) keeps a network or ssh-agent answer for the session.
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

use crate::{agent, console, fs, hostfn, http, intercept, machine, net, sshagent, virtio, vsock, zip};

pub const PROTOCOL_VERSION: u64 = 1;

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| value | (*byte as u32) << (16 - 8 * index));
        for index in 0..4 {
            out.push(match index <= chunk.len() {
                true => ALPHABET[(triple >> (18 - 6 * index) & 0x3f) as usize] as char,
                false => '=',
            });
        }
    }
    out
}

fn unbase64(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(anyhow!("invalid base64")),
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

/// Where the engine finds what it boots.
pub struct Images {
    pub kernel: PathBuf,
    pub initramfs: Vec<PathBuf>,
    /// The CPython overlay, added unless the app asks for `python: false`.
    pub python: Option<PathBuf>,
    /// The network tools overlay (curl, ssh, git), added unless the app asks for `tools: false`.
    pub tools: Option<PathBuf>,
}

struct Mount {
    host_path: PathBuf,
    guest_path: String,
    read_only: bool,
}

/// A failure the app should see with a kind it can match on.
struct Failure {
    kind: &'static str,
    message: String,
}

fn failed<T>(kind: &'static str, message: impl Into<String>) -> std::result::Result<T, Failure> {
    Err(Failure { kind, message: message.into() })
}

type Answer = std::result::Result<Value, Failure>;

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Failure {
        Failure { kind: "failed", message: format!("{error:#}") }
    }
}

/// Writes one JSON message per line to the app, from any thread.
#[derive(Clone)]
struct Out(Arc<Mutex<()>>);

impl Out {
    fn send(&self, message: Value) {
        let _guard = self.0.lock().unwrap();
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{message}");
        let _ = stdout.flush();
    }

    fn event(&self, name: &str, mut fields: Map<String, Value>) {
        fields.insert("event".into(), json!(name));
        self.send(Value::Object(fields));
    }
}

/// The running sandbox, once `start` has booted it.
struct Running {
    vsock: vsock::Vsock,
    stopper: machine::Stopper,
    console: std::sync::mpsc::Sender<Vec<u8>>,
    mounts: Vec<Mount>,
    /// Both policies can be replaced while the guest runs (`policy.update`).
    network: Arc<RwLock<http::Policy>>,
    /// The app's answers to network questions, forgotten when the policy changes.
    network_answers: Arc<Mutex<HashMap<String, bool>>>,
    ssh_agent: Arc<RwLock<sshagent::Policy>>,
    ssh_answers: sshagent::Answers,
    host_policy: Arc<RwLock<hostfn::HostPolicy>>,
    host_functions: hostfn::HostFunctions,
}

pub struct Server {
    images: Images,
    out: Out,
    running: RwLock<Option<Arc<Running>>>,
}

impl Server {
    pub fn new(images: Images) -> Server {
        Server { images, out: Out(Arc::new(Mutex::new(()))), running: RwLock::new(None) }
    }

    /// Serves until stdin ends or `stop` arrives.
    ///
    /// Every request runs on its own thread: a command in the guest can take minutes, and
    /// while it runs the app still has to be able to answer the host calls it makes.
    pub fn serve(self) -> Result<()> {
        let server = Arc::new(self);
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(_) => {
                    server.out.send(json!({"id": null, "error": {"kind": "bad-request", "message": "not valid JSON"}}));
                    continue;
                }
            };
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

            let stop = method == "stop";
            let worker = server.clone();
            let answering = std::thread::spawn(move || {
                let answer = worker.dispatch(&method, &params, &id);
                match answer {
                    Ok(result) => worker.out.send(json!({"id": id, "result": result})),
                    Err(failure) => {
                        worker.out.send(json!({"id": id, "error": {"kind": failure.kind, "message": failure.message}}))
                    }
                }
            });
            if stop {
                let _ = answering.join();
                break;
            }
        }
        // The app went away, or asked to stop: do not leave a guest running without an owner.
        if let Some(running) = server.running.read().unwrap().as_ref() {
            running.host_functions.close();
            running.stopper.stop();
        }
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn dispatch(&self, method: &str, params: &Value, id: &Value) -> Answer {
        match method {
            "start" => self.start(params),
            "exec" => self.exec_method(params, id),
            "readFile" => self.read_file(params),
            "writeFile" => self.write_file(params),
            "console.write" => self.console_write(params),
            "console.resize" => Ok(json!({})),
            "policy.update" => self.update_policy(params),
            "reply" => self.reply(params),
            "exportZip" => self.export_zip(params),
            "stop" => {
                if let Some(running) = self.running.read().unwrap().as_ref() {
                    running.stopper.stop();
                }
                Ok(json!({}))
            }
            other => failed("unknown-method", format!("no method \"{other}\"")),
        }
    }

    fn running(&self) -> std::result::Result<Arc<Running>, Failure> {
        match self.running.read().unwrap().as_ref() {
            Some(running) => Ok(running.clone()),
            None => failed("not-started", "call start first"),
        }
    }

    fn start(&self, params: &Value) -> Answer {
        if self.running.read().unwrap().is_some() {
            return failed("already-started", "the sandbox is already running");
        }
        let config = params.get("config").cloned().unwrap_or_else(|| json!({}));
        let cpus = config.get("cpus").and_then(Value::as_u64).unwrap_or(2).clamp(1, 32) as u32;
        let python = config.get("python").and_then(Value::as_bool).unwrap_or(true);
        let tools = config.get("tools").and_then(Value::as_bool).unwrap_or(true);
        let quiet = config.get("quiet").and_then(Value::as_bool).unwrap_or(false);

        if let Some(mode) = config.get("hostExec").and_then(Value::as_str) {
            if !["deny", "ask", "allow"].contains(&mode) {
                return failed("bad-request", "hostExec must be one of deny, ask, allow");
            }
        }
        let ssh_agent = match ssh_agent_policy(&config, sshagent::Policy { mode: sshagent::Mode::Off, socket: None }) {
            Ok(policy) => policy,
            Err(message) => return failed("bad-request", message),
        };

        // Mounts: a host folder each, mounted by the guest's /etc/rc from the command line.
        let mut mounts: Vec<Mount> = Vec::new();
        for (index, entry) in config.get("mounts").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
            let host_path = entry.get("hostPath").and_then(Value::as_str).unwrap_or("");
            let guest_path = entry.get("guestPath").and_then(Value::as_str).unwrap_or("").trim_end_matches('/').to_string();
            if host_path.is_empty() || guest_path.is_empty() {
                return failed("bad-request", format!("mounts[{index}] needs hostPath and guestPath"));
            }
            let host_path = std::path::PathBuf::from(host_path);
            if !host_path.is_dir() {
                return failed("bad-request", format!("mounts[{index}]: {} is not a directory", host_path.display()));
            }
            if !safe_guest_path(&guest_path) {
                return failed(
                    "bad-request",
                    format!(
                        "mounts[{index}]: guestPath \"{guest_path}\" must be an absolute path of letters, \
                         digits, . _ - / outside system directories"
                    ),
                );
            }
            if mounts.iter().any(|mount| mount.guest_path == guest_path) {
                return failed("bad-request", format!("two mounts at {guest_path}"));
            }
            mounts.push(Mount {
                host_path,
                guest_path,
                read_only: entry.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
            });
        }

        let network = Arc::new(RwLock::new(network_policy(config.get("network"))));
        let vsock = vsock::Vsock::new(3);
        // Host functions: what the app answers itself, and whether it lets the guest run
        // programs on this computer.
        let host_policy = Arc::new(RwLock::new(hostfn::HostPolicy {
            host_exec: hostfn::HostExec::parse(config.get("hostExec").and_then(Value::as_str).unwrap_or("deny")),
            functions: config
                .get("hostFunctions")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            permission_timeout: Duration::from_millis(
                config.get("permissionTimeoutMs").and_then(Value::as_u64).unwrap_or(120_000),
            ),
            ..Default::default()
        }));
        let ask_out = self.out.clone();
        let host_functions = hostfn::HostFunctions::new(
            host_policy.clone(),
            Arc::new(move |asked: hostfn::Ask| {
                let mut fields = Map::new();
                match asked {
                    hostfn::Ask::Call { call_id, function, args } => {
                        fields.insert("callId".into(), json!(call_id));
                        fields.insert("fn".into(), json!(function));
                        fields.insert("args".into(), args);
                        ask_out.event("hostCall", fields);
                    }
                    hostfn::Ask::Permission { request_id, kind, argv, cwd, gui, target } => {
                        fields.insert("requestId".into(), json!(request_id));
                        fields.insert("kind".into(), json!(kind));
                        fields.insert("argv".into(), json!(argv));
                        fields.insert("cwd".into(), json!(cwd));
                        fields.insert("gui".into(), json!(gui));
                        if let Some(target) = target {
                            fields.insert("target".into(), json!(target));
                        }
                        ask_out.event("permission", fields);
                    }
                }
            }),
        );
        host_functions.serve(&vsock, hostfn::PORT)?;

        // The host's ssh-agent, when the app lends it: the guest's SSH_AUTH_SOCK comes here.
        let ssh_lent = ssh_agent.mode != sshagent::Mode::Off;
        let ssh_agent = Arc::new(RwLock::new(ssh_agent));
        let ssh_answers: sshagent::Answers = Arc::new(Mutex::new(HashMap::new()));
        {
            let (asker_functions, observer_out) = (host_functions.clone(), self.out.clone());
            sshagent::serve(
                &vsock,
                ssh_agent.clone(),
                ssh_answers.clone(),
                Arc::new(move |target: &str| asker_functions.ask_permission("ssh-agent", target)),
                Arc::new(move |event: sshagent::Event| {
                    let mut fields = Map::new();
                    fields.insert("op".into(), json!(event.op));
                    if let Some(key) = event.key {
                        fields.insert("key".into(), json!(key));
                    }
                    fields.insert("allowed".into(), json!(event.allowed));
                    if let Some(reason) = event.reason {
                        fields.insert("reason".into(), json!(reason));
                    }
                    observer_out.event("sshAgent", fields);
                }),
            )?;
        }

        // Hosts the network policy does not name, under `ask`: one question per host:port,
        // remembered for the session unless the app answers with remember: false. policy.update
        // forgets the answers.
        let answers: Arc<Mutex<HashMap<String, bool>>> = Arc::new(Mutex::new(HashMap::new()));
        let asker: http::Asker = {
            let (answers, host_functions) = (answers.clone(), host_functions.clone());
            Arc::new(move |_via: &'static str, target: &str| {
                if let Some(known) = answers.lock().unwrap().get(target) {
                    return *known;
                }
                let (allow, remember) = host_functions.ask_permission("network", target).unwrap_or((false, false));
                if remember {
                    answers.lock().unwrap().insert(target.to_string(), allow);
                }
                allow
            })
        };
        let out = self.out.clone();
        http::serve(
            &vsock,
            http::DEFAULT_PORT,
            network.clone(),
            Some(Arc::new(move |event: http::Event| {
                let mut fields = Map::new();
                fields.insert("via".into(), json!("api"));
                fields.insert("method".into(), json!(event.method));
                fields.insert("url".into(), json!(event.url));
                fields.insert("kind".into(), json!("request"));
                if event.blocked {
                    fields.insert("blocked".into(), json!(true));
                    fields.insert("errorKind".into(), json!("denied"));
                }
                match (event.status, event.error) {
                    (Some(status), _) => {
                        fields.insert("phase".into(), json!("response"));
                        fields.insert("status".into(), json!(status));
                    }
                    (None, Some(error)) => {
                        fields.insert("phase".into(), json!("failed"));
                        fields.insert("error".into(), json!(error.clone()));
                        fields.insert("reason".into(), json!(error));
                    }
                    _ => {}
                }
                out.event("network", fields);
            })),
            Some(asker.clone()),
        )?;


        // Packet level: a NIC whose gateway is this process, unless the app turned the
        // network off entirely.
        let mut args: Vec<String> = vec!["collabo.agent=1".into()];
        let networked = !matches!(config.get("network"), Some(Value::Bool(false)));
        // The guest's own HTTPS clients get the app's secrets too: TLS to those hosts ends here
        // (intercept.rs), with a session CA the guest trusts.
        let interceptor = match networked {
            false => None,
            true => {
                let out = self.out.clone();
                Some(intercept::Interceptor::new(
                    network.clone(),
                    Some(Arc::new(move |event: intercept::Event| {
                        let mut fields = Map::new();
                        fields.insert("via".into(), json!("net"));
                        fields.insert("kind".into(), json!("request"));
                        fields.insert("method".into(), json!(event.method));
                        fields.insert("url".into(), json!(event.url));
                        fields.insert("secrets".into(), json!(event.secrets));
                        match (event.status, event.error) {
                            (_, Some(error)) => {
                                fields.insert("phase".into(), json!("failed"));
                                fields.insert("reason".into(), json!(error));
                            }
                            (Some(status), None) => {
                                fields.insert("phase".into(), json!("response"));
                                fields.insert("status".into(), json!(status));
                            }
                            _ => {}
                        }
                        out.event("network", fields);
                    })),
                )?)
            }
        };
        let session_ca = interceptor.as_ref().map(|interceptor| interceptor.ca_pem().to_string());
        let stack = networked.then(|| {
            let out = self.out.clone();
            net::Stack::new(
                network.clone(),
                Some(Arc::new(move |event: net::Event| {
                    let mut fields = Map::new();
                    fields.insert("via".into(), json!("net"));
                    match event {
                        net::Event::Dns { host, addresses, blocked, reason } => {
                            fields.insert("kind".into(), json!("dns"));
                            fields.insert("host".into(), json!(host));
                            if !addresses.is_empty() {
                                fields.insert("addresses".into(), json!(addresses));
                            }
                            if blocked {
                                fields.insert("blocked".into(), json!(true));
                            }
                            if let Some(reason) = reason {
                                fields.insert("reason".into(), json!(reason));
                            }
                        }
                        net::Event::Connect { id, ip, port, phase, blocked, reason } => {
                            fields.insert("kind".into(), json!("connect"));
                            fields.insert("id".into(), json!(id));
                            fields.insert("ip".into(), json!(ip));
                            fields.insert("port".into(), json!(port));
                            fields.insert("phase".into(), json!(phase));
                            if blocked {
                                fields.insert("blocked".into(), json!(true));
                            }
                            if let Some(reason) = reason {
                                fields.insert("reason".into(), json!(reason));
                            }
                        }
                    }
                    out.event("network", fields);
                })),
                Some(asker.clone()),
                interceptor.clone(),
            )
        });
        if quiet {
            args.push("collabo.quiet=1".into());
        }
        if ssh_lent {
            args.push("collabo.sshagent=1".into());
        }
        let console_out = self.out.clone();
        let console = console::Console::new(
            config.get("consoleSize").and_then(|size| size.get("cols")).and_then(Value::as_u64).unwrap_or(100) as u16,
            config.get("consoleSize").and_then(|size| size.get("rows")).and_then(Value::as_u64).unwrap_or(30) as u16,
            Box::new(move |bytes| {
                let mut fields = Map::new();
                fields.insert("dataBase64".into(), json!(base64(bytes)));
                console_out.event("console", fields);
            }),
        );

        let mut devices: Vec<Box<dyn virtio::Device>> = vec![Box::new(console), vsock.device()];
        if let Some(stack) = &stack {
            devices.push(stack.device());
            args.extend(net::Stack::kernel_arguments());
        }
        for (index, mount) in mounts.iter().enumerate() {
            let tag = format!("mount{index}");
            let option = if mount.read_only { ":ro" } else { "" };
            args.push(format!("collabo.mount={tag}:{}{option}", mount.guest_path));
            devices.push(Box::new(fs::FsDevice::new(fs::Share {
                tag,
                host_path: mount.host_path.clone(),
                guest_path: mount.guest_path.clone(),
                read_only: mount.read_only,
            })?));
        }

        let mut initcpio = Vec::new();
        for path in &self.images.initramfs {
            initcpio.extend(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        }
        if python {
            match &self.images.python {
                Some(path) => initcpio.extend(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?),
                None => return failed("bad-request", "this runtime has no Python image; start with python: false"),
            }
        }
        if tools {
            match &self.images.tools {
                Some(path) => initcpio.extend(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?),
                None => return failed("bad-request", "this runtime has no network tools image; start with tools: false"),
            }
        }
        if let Some(pem) = &session_ca {
            initcpio.extend(cpio_overlay(&[("etc", None), ("etc/ssl", None), ("etc/ssl/collabo-ca.pem", Some(pem.as_bytes()))]));
        }
        let kernel = std::fs::read(&self.images.kernel)
            .with_context(|| format!("reading {}", self.images.kernel.display()))?;

        let boot_out = self.out.clone();
        let mut machine = machine::Machine::boot(machine::BootOptions {
            kernel,
            devices,
            args,
            cpus,
            initcpio,
            boot_console: Box::new(move |bytes| {
                let mut fields = Map::new();
                fields.insert("dataBase64".into(), json!(base64(bytes)));
                boot_out.event("console", fields);
            }),
        })
        .context("booting the kernel")?;

        let (stopper, console_input) = (machine.stopper(), machine.console_input());
        let exit_out = self.out.clone();
        std::thread::spawn(move || {
            let termination = machine.run();
            let mut fields = Map::new();
            match termination {
                Ok(machine::Termination::Clean) => {
                    fields.insert("reason".into(), json!("stopped"));
                }
                Ok(machine::Termination::Panic) => {
                    fields.insert("reason".into(), json!("panicked"));
                }
                Err(error) => {
                    fields.insert("reason".into(), json!("failed"));
                    fields.insert("message".into(), json!(format!("{error:#}")));
                }
            }
            exit_out.event("exit", fields);
            // The app has its answer; the process leaves with the guest.
            std::thread::sleep(Duration::from_millis(50));
            std::process::exit(0);
        });

        // The guest is ready when its agent answers.
        agent::connect(&vsock, Duration::from_secs(60)).map_err(Failure::from)?;

        let reported = json!({
            "cpus": cpus,
            "hostExec": config.get("hostExec").cloned().unwrap_or_else(|| json!("deny")),
            "hostFunctions": config.get("hostFunctions").cloned().unwrap_or_else(|| json!([])),
            "python": python,
            "tools": tools,
            "sshAgent": config.get("sshAgent").cloned().unwrap_or_else(|| json!("off")),
            "quiet": quiet,
            "mounts": mounts.iter().map(|mount| json!({
                "hostPath": mount.host_path.to_string_lossy(),
                "guestPath": mount.guest_path,
                "readOnly": mount.read_only,
            })).collect::<Vec<_>>(),
            "network": redacted_network(config.get("network")),
        });
        *self.running.write().unwrap() =
            Some(Arc::new(Running {
                vsock,
                stopper,
                console: console_input,
                mounts,
                network,
                network_answers: answers,
                ssh_agent,
                ssh_answers,
                host_policy,
                host_functions,
            }));
        self.out.event("ready", Map::new());
        Ok(json!({"version": PROTOCOL_VERSION, "config": reported}))
    }

    /// Runs one command in the guest and answers with its output.
    fn exec(&self, params: &Value, id: Option<&Value>) -> std::result::Result<ExecResult, Failure> {
        let running = self.running()?;
        let argv = match (params.get("argv").and_then(Value::as_array), params.get("command").and_then(Value::as_str)) {
            (_, Some(command)) => vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            (Some(argv), None) => argv.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            (None, None) => return failed("bad-request", "exec needs argv or command"),
        };
        if argv.is_empty() {
            return failed("bad-request", "exec needs a command");
        }
        let stdin = match (params.get("stdinBase64").and_then(Value::as_str), params.get("stdin").and_then(Value::as_str)) {
            (Some(text), _) => unbase64(text).map_err(Failure::from)?,
            (None, Some(text)) => text.as_bytes().to_vec(),
            (None, None) => Vec::new(),
        };
        let env = params
            .get("env")
            .and_then(Value::as_object)
            .map(|env| env.iter().map(|(name, value)| (name.clone(), value.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        let timeout = params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(120_000);
        let streaming = params.get("stream").and_then(Value::as_bool).unwrap_or(false);

        let command = agent::Command {
            argv,
            env,
            cwd: params.get("cwd").and_then(Value::as_str).map(str::to_string),
            stdin,
        };
        let stream = agent::connect(&running.vsock, Duration::from_secs(30)).map_err(Failure::from)?;
        let started = Instant::now();

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let (out, execution) = (self.out.clone(), id.cloned());
        let emit = |name: &str, bytes: &[u8]| {
            if let (true, Some(id)) = (streaming, &execution) {
                let mut fields = Map::new();
                fields.insert("execId".into(), id.clone());
                fields.insert("stream".into(), json!(name));
                fields.insert("dataBase64".into(), json!(base64(bytes)));
                out.event("execOutput", fields);
            }
        };
        let (exit, timed_out) = agent::run(
            &stream,
            &command,
            (timeout > 0).then(|| Duration::from_millis(timeout)),
            |bytes| {
                stdout.extend_from_slice(bytes);
                emit("stdout", bytes);
            },
            |bytes| {
                stderr.extend_from_slice(bytes);
                emit("stderr", bytes);
            },
        )
        .map_err(Failure::from)?;

        Ok(ExecResult { exit, stdout, stderr, timed_out, duration: started.elapsed() })
    }

    fn exec_method(&self, params: &Value, id: &Value) -> Answer {
        let result = self.exec(params, Some(id))?;
        Ok(json!({
            "exitCode": match result.exit { agent::Exit::Code(code) => json!(code), _ => Value::Null },
            "signal": match result.exit { agent::Exit::Signal(signal) => json!(signal), _ => Value::Null },
            "timedOut": result.timed_out,
            "truncated": false,
            "durationMs": result.duration.as_millis() as u64,
            "stdoutBase64": base64(&result.stdout),
            "stderrBase64": base64(&result.stderr),
        }))
    }

    fn read_file(&self, params: &Value) -> Answer {
        let Some(path) = params.get("path").and_then(Value::as_str) else {
            return failed("bad-request", "path is required");
        };
        let result = self.exec(&json!({"argv": ["cat", "--", path]}), None)?;
        match result.exit {
            agent::Exit::Code(0) => Ok(json!({"dataBase64": base64(&result.stdout)})),
            _ => failed("failed", String::from_utf8_lossy(&result.stderr).trim().to_string()),
        }
    }

    fn write_file(&self, params: &Value) -> Answer {
        let Some(path) = params.get("path").and_then(Value::as_str) else {
            return failed("bad-request", "path is required");
        };
        let data = match (params.get("dataBase64").and_then(Value::as_str), params.get("data").and_then(Value::as_str)) {
            (Some(text), _) => text.to_string(),
            (None, Some(text)) => base64(text.as_bytes()),
            (None, None) => base64(b""),
        };
        let mode = match params.get("mode").and_then(Value::as_u64) {
            Some(mode) => format!("chmod {mode:o} \"$1\" && "),
            None => String::new(),
        };
        let script = format!("mkdir -p \"$(dirname \"$1\")\" && cat > \"$1\" && {mode}true");
        let result = self.exec(
            &json!({"argv": ["/bin/sh", "-c", script, "sh", path], "stdinBase64": data}),
            None,
        )?;
        match result.exit {
            agent::Exit::Code(0) => Ok(json!({})),
            _ => failed("failed", String::from_utf8_lossy(&result.stderr).trim().to_string()),
        }
    }

    fn console_write(&self, params: &Value) -> Answer {
        let running = self.running()?;
        let bytes = match (params.get("dataBase64").and_then(Value::as_str), params.get("data").and_then(Value::as_str)) {
            (Some(text), _) => unbase64(text).map_err(Failure::from)?,
            (None, Some(text)) => text.as_bytes().to_vec(),
            (None, None) => Vec::new(),
        };
        let _ = running.console.send(bytes);
        Ok(json!({}))
    }

    /// Replaces the policies the app set at start. Requests already in flight keep the old one.
    fn update_policy(&self, params: &Value) -> Answer {
        let running = self.running()?;
        if let Some(network) = params.get("network") {
            apply_network_policy(&mut running.network.write().unwrap(), Some(network));
            running.network_answers.lock().unwrap().clear();
        }
        if params.get("sshAgent").is_some() || params.get("sshAgentSocket").is_some() {
            let current = running.ssh_agent.read().unwrap().clone();
            match ssh_agent_policy(params, current) {
                Ok(policy) => *running.ssh_agent.write().unwrap() = policy,
                Err(message) => return failed("bad-request", message),
            }
            running.ssh_answers.lock().unwrap().clear();
        }
        {
            let mut policy = running.host_policy.write().unwrap();
            if let Some(mode) = params.get("hostExec").and_then(Value::as_str) {
                policy.host_exec = hostfn::HostExec::parse(mode);
            }
            if let Some(functions) = params.get("hostFunctions").and_then(Value::as_array) {
                policy.functions = functions.iter().filter_map(Value::as_str).map(str::to_string).collect();
            }
        }
        Ok(json!({}))
    }

    /// The app's answer to a hostCall or a permission request.
    fn reply(&self, params: &Value) -> Answer {
        let running = self.running()?;
        let Some(id) = params.get("id").and_then(Value::as_u64) else {
            return failed("bad-request", "reply needs the id of the question");
        };
        let reply = match (params.get("error"), params.get("allow")) {
            (Some(error), _) => hostfn::Reply::Error {
                kind: error.get("kind").and_then(Value::as_str).unwrap_or("failed").to_string(),
                message: match error.get("message").and_then(Value::as_str) {
                    Some(message) => message.to_string(),
                    None => error.as_str().unwrap_or("the app refused").to_string(),
                },
            },
            (None, Some(allow)) => hostfn::Reply::Allow {
                allow: allow.as_bool().unwrap_or(false),
                remember: params.get("remember").and_then(Value::as_bool).unwrap_or(true),
            },
            (None, None) => hostfn::Reply::Result(params.get("result").cloned().unwrap_or(Value::Null)),
        };
        match running.host_functions.reply(id, reply) {
            true => Ok(json!({})),
            false => failed("unknown-id", format!("nothing is waiting for an answer with id {id}")),
        }
    }

    fn export_zip(&self, params: &Value) -> Answer {
        let running = self.running()?;
        let guest_path = params.get("guestPath").and_then(Value::as_str).unwrap_or("");
        let Some(mount) = running.mounts.iter().find(|mount| mount.guest_path == guest_path) else {
            let mounted: Vec<&str> = running.mounts.iter().map(|mount| mount.guest_path.as_str()).collect();
            return failed("bad-request", format!("{guest_path} is not a mounted folder ({})", mounted.join(", ")));
        };
        let Some(out_file) = params.get("outFile").and_then(Value::as_str) else {
            return failed("bad-request", "outFile is required");
        };
        let (entries, bytes) = zip::write_folder(&mount.host_path, std::path::Path::new(out_file)).map_err(Failure::from)?;
        Ok(json!({"entries": entries, "bytes": bytes}))
    }
}

struct ExecResult {
    exit: agent::Exit,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    duration: Duration,
}

/// Where a mount may appear in the guest: an absolute path of plain characters, not the root
/// and not over the system directories the guest boots from.
fn safe_guest_path(path: &str) -> bool {
    const SYSTEM: [&str; 8] = ["proc", "sys", "dev", "bin", "sbin", "usr", "etc", "lib"];
    if !path.starts_with('/') || path == "/" || path.contains("//") || path.contains("..") {
        return false;
    }
    if !path.chars().all(|c| c.is_ascii_alphanumeric() || ".-_/".contains(c)) {
        return false;
    }
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    !SYSTEM.contains(&first)
}

/// A newc cpio archive of a few directories and files (root-owned, 755 / 644), to append after
/// the images: the kernel unpacks archives placed back to back.
fn cpio_overlay(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut add = |ino: usize, name: &str, mode: u32, data: &[u8]| {
        let header = format!(
            "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            0, 0, 1, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    };
    for (index, (name, data)) in entries.iter().enumerate() {
        match data {
            None => add(index + 1, name, 0o040755, &[]),
            Some(data) => add(index + 1, name, 0o100644, data),
        }
    }
    add(0, "TRAILER!!!", 0, &[]);
    out
}

/// `sshAgent` ("off", "ask", "allow") and `sshAgentSocket` from a start or policy.update request,
/// over what was there before.
fn ssh_agent_policy(fields: &Value, mut policy: sshagent::Policy) -> std::result::Result<sshagent::Policy, String> {
    if let Some(mode) = fields.get("sshAgent") {
        policy.mode = mode
            .as_str()
            .and_then(sshagent::Mode::parse)
            .ok_or_else(|| "sshAgent must be one of off, ask, allow".to_string())?;
    }
    match fields.get("sshAgentSocket") {
        Some(Value::String(path)) if !path.is_empty() => policy.socket = Some(PathBuf::from(path)),
        Some(Value::Null) | Some(Value::String(_)) => policy.socket = None,
        Some(_) => return Err("sshAgentSocket must be a path".into()),
        None => {}
    }
    Ok(policy)
}

/// The app's `network` field: `false`, or the policy object.
fn network_policy(value: Option<&Value>) -> http::Policy {
    let mut policy = http::Policy::default();
    apply_network_policy(&mut policy, value);
    policy
}

/// Applies what the app sent onto a policy, leaving out what it did not mention. `policy.update`
/// is a change to the policy the sandbox started with, not a replacement for it.
fn apply_network_policy(policy: &mut http::Policy, value: Option<&Value>) {
    let fields = match value {
        Some(Value::Bool(false)) => {
            // No network at all: nothing is allowed, and the keys go with it.
            policy.allow.clear();
            policy.secrets.clear();
            return;
        }
        Some(Value::Object(fields)) => fields,
        _ => return,
    };
    let list = |name: &str| -> Option<Vec<String>> {
        fields
            .get(name)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(|text| text.trim().to_lowercase()).collect())
    };
    if let Some(allow) = list("allow") {
        policy.allow = allow;
    }
    if let Some(deny) = list("deny") {
        policy.deny = deny;
    }
    if let Some(headers) = list("extraAllowedHeaders") {
        policy.extra_headers = headers;
    }
    if let Some(loopback) = fields.get("allowHostLoopback").and_then(Value::as_bool) {
        policy.allow_loopback = loopback;
    }
    if let Some(ask) = fields.get("ask").and_then(Value::as_bool) {
        policy.ask = ask;
    }
    if let Some(secrets) = fields.get("secrets").and_then(Value::as_array) {
        policy.secrets = secrets
            .iter()
            .filter_map(|secret| {
                Some(http::Secret {
                    host: secret.get("host")?.as_str()?.to_lowercase(),
                    header: secret.get("header")?.as_str()?.to_lowercase(),
                    value: secret.get("value")?.as_str()?.to_string(),
                })
            })
            .collect();
    }
}

/// The policy echoed back to the app, without the secret values.
fn redacted_network(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(fields)) => {
            let mut fields = fields.clone();
            if let Some(Value::Array(secrets)) = fields.get("secrets").cloned() {
                let stripped: Vec<Value> = secrets
                    .iter()
                    .map(|secret| json!({"host": secret.get("host"), "header": secret.get("header")}))
                    .collect();
                fields.insert("secrets".into(), Value::Array(stripped));
            }
            Value::Object(fields)
        }
        Some(other) => other.clone(),
        None => json!({}),
    }
}

