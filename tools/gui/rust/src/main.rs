//! gui: GUI programs in the collaboCore guest, headless, one whole canvas per program window,
//! driven from the command line. The same binary is the display server (`gui server`, started on
//! demand by the first command that needs it) and every command that talks to it. Part of the
//! guest's tools image; see tools/gui/README.md, and PROTOCOL.md for the wire protocol.
mod config;
mod image;
mod keys;
mod proto;
mod server;
mod sys;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use proto::{op, status, Reader, Writer};

const HELP: &str = "\
Usage: gui COMMAND [ARGS...]

Programs draw into canvases: one whole frame per program window, always complete, never covered,
never positioned. Nothing is shown anywhere unless you ask: take a screenshot, preview it here, or
send it input. A TARGET is a program's NAME (its first canvas), NAME:N (its canvas N), or `.`
(the focused canvas, else the only program).

Programs
  run [--name N] [--size WxH] [--env K=V]... [--cwd DIR] [--wait] [--] CMD [ARGS...]
                               start a GUI program (its output: gui logs NAME)
  list [--json]                programs and their canvases (* = focus)
  info TARGET                  one program, as JSON
  logs NAME [-n LINES] [-f]    what the program printed
  close TARGET                 ask politely (the window's close button)
  kill TARGET [--signal SIG] [--grace SECONDS]
                               end it (TERM, then KILL after the grace, 3 s)
  wait TARGET [--ready|--frame|--idle MS|--exit|--sync] [--timeout SECONDS]
                               until it has drawn (default) / draws again / has not drawn for
                               MS / has exited / has handled all input sent so far

Screen
  screenshot TARGET [-o FILE|-] [--format png|ppm|raw] [--region X,Y,W,H]
                    [--scale F] [--max-width W] [--max-height H] [--idle MS]
                               save the canvas (default: ./NAME.png); --idle waits for it to
                               settle first
  view TARGET [--width COLS]   a preview in this terminal
  resize TARGET WxH [--wait]   change a canvas's size (within the limits: gui status)

Input (focuses the canvas first unless --no-focus; waits until the program has handled it
unless --no-sync). Mouse commands take --mods ctrl+shift to hold modifier keys around them.
  click TARGET [X Y] [--button left|middle|right] [--double | --count N]
  move TARGET X Y              leave TARGET (the pointer leaves the canvas: hover ends)
  mousedown TARGET [X Y] [--button B]      mouseup TARGET [X Y] [--button B]
  drag TARGET X1 Y1 X2 Y2 [--button B] [--steps N]
  scroll TARGET [--at X Y] DY [DX] [--pixels]
                               wheel notches (positive DY scrolls down); --pixels: DY, DX are
                               pixels, a smooth (touchpad) scroll
  key TARGET COMBO...          press and release: Return, Tab, ctrl+a, shift+Tab, alt+F4
  keydown TARGET COMBO...      press and hold (keyup TARGET COMBO... lets go): keydown T shift
  release TARGET               let go of every held button and modifier
  type TARGET TEXT... [--commit | --ime] [--delay MS]
                               type text (any language; TEXT - reads stdin). --commit: as one
                               input-method commit; --ime: Hangul composed as a Korean input
                               method does it (preedit ㅎ, 하, 한, then the commit)
  preedit TARGET [TEXT] [--cursor B[,E]]
                               show TEXT as the input method's composition (none: clear it)
  focus TARGET                 give it the keyboard focus

Clipboard
  clipboard get [--type MIME] [-o FILE]
  clipboard set [--type MIME] [TEXT | -]   (stdin when no TEXT)
  clipboard types | clear

Server
  status                       limits, access, what is running
  server [--detach]            run the display server (normally started for you)
  shutdown                     end every program and the server

Exit status: 0 done, 1 failed, 2 usage, 3 timed out, 4 no such program/canvas, 5 not allowed.
";

fn fail(code: u32, msg: impl AsRef<str>) -> ! {
    eprintln!("gui: {}", msg.as_ref());
    std::process::exit(code as i32);
}

/// Arguments: positionals, and options with or without a value.
struct Args {
    pos: Vec<String>,
    opts: HashMap<String, Vec<String>>,
    rest: Vec<String>,
}

impl Args {
    /// `valued` names the options that take a value. A word after `--`, or every word from the
    /// first positional on when `stop_at_first` (for `gui run`), goes to `rest`.
    fn parse(args: &[String], valued: &[&str], flags: &[&str], stop_at_first: bool) -> Args {
        let mut a = Args { pos: Vec::new(), opts: HashMap::new(), rest: Vec::new() };
        let mut i = 0;
        while i < args.len() {
            let s = &args[i];
            if s == "--" {
                a.rest = args[i + 1..].to_vec();
                break;
            }
            let numeric = s.len() > 1 && s.starts_with('-') && s[1..].chars().all(|c| c.is_ascii_digit() || c == '.');
            if s.starts_with('-') && s.len() > 1 && !numeric {
                let (name, inline) = match s.split_once('=') {
                    Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                    _ => (s.clone(), None),
                };
                if valued.contains(&name.as_str()) {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            args.get(i).cloned().unwrap_or_else(|| fail(status::USAGE, format!("{name} needs a value")))
                        }
                    };
                    a.opts.entry(name).or_default().push(v);
                } else if flags.contains(&name.as_str()) {
                    a.opts.entry(name).or_default().push(String::new());
                } else {
                    fail(status::USAGE, format!("unknown option {name} (gui help)"));
                }
            } else if stop_at_first {
                a.rest = args[i..].to_vec();
                break;
            } else {
                a.pos.push(s.clone());
            }
            i += 1;
        }
        a
    }
    fn has(&self, name: &str) -> bool {
        self.opts.contains_key(name)
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.opts.get(name).and_then(|v| v.last()).map(String::as_str)
    }
    fn all(&self, name: &str) -> Vec<String> {
        self.opts.get(name).cloned().unwrap_or_default()
    }
    fn num<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.get(name).map(|v| v.parse().unwrap_or_else(|_| fail(status::USAGE, format!("{name}: not a number: {v}"))))
    }
    fn target(&self) -> String {
        self.pos.first().cloned().unwrap_or_else(|| fail(status::USAGE, "which program? (a NAME, NAME:N, or .; gui list)"))
    }
    fn timeout_ms(&self, default_s: f64) -> u64 {
        let s: f64 = self.num("--timeout").unwrap_or(default_s);
        (s.max(0.0) * 1000.0) as u64
    }
}

fn parse_num(s: &str, what: &str) -> i64 {
    s.parse::<f64>().map(|v| v.round() as i64).unwrap_or_else(|_| fail(status::USAGE, format!("{what}: not a number: {s}")))
}

// ── the control connection ──────────────────────────────────────────────────────────────────

struct Control {
    stream: UnixStream,
    rbuf: Vec<u8>,
    next: u32,
}

struct Answer {
    status: u32,
    text: String,
    blob: Vec<u8>,
}

impl Control {
    fn connect() -> Control {
        let path = config::socket_path();
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(_) if std::env::var_os("COLLABO_GUI_SOCKET").is_none() => start_server(&path),
            Err(e) => fail(status::FAILED, format!("{}: {e}", path.display())),
        };
        let mut c = Control { stream, rbuf: Vec::new(), next: 1 };
        let hello = Writer::new(op::HELLO).u32(proto::VERSION).u32(proto::ROLE_CONTROL).u32(std::process::id()).str("gui").str("").finish();
        c.write(&hello);
        let (o, payload) = c.read_message();
        match o {
            op::WELCOME => {}
            op::ERROR => {
                let mut r = Reader::new(&payload);
                let _ = r.u32();
                fail(status::FAILED, format!("the server refused: {}", r.str().unwrap_or_default()));
            }
            _ => fail(status::FAILED, "the server answered something unexpected"),
        }
        c
    }

    fn write(&mut self, msg: &[u8]) {
        if let Err(e) = self.stream.write_all(msg) {
            fail(status::FAILED, format!("lost the server: {e}"));
        }
    }

    fn read_message(&mut self) -> (u16, Vec<u8>) {
        loop {
            match proto::next_message(&self.rbuf, proto::MAX_RESULT) {
                Ok(Some((o, payload, used))) => {
                    let p = payload.to_vec();
                    self.rbuf.drain(..used);
                    return (o, p);
                }
                Ok(None) => {}
                Err(_) => fail(status::FAILED, "the server sent a message too large"),
            }
            let len = self.rbuf.len();
            self.rbuf.resize(len + (1 << 20), 0);
            match self.stream.read(&mut self.rbuf[len..]) {
                Ok(0) => fail(status::FAILED, "the server closed the connection"),
                Ok(n) => self.rbuf.truncate(len + n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => self.rbuf.truncate(len),
                Err(e) => fail(status::FAILED, format!("lost the server: {e}")),
            }
        }
    }

    fn call(&mut self, argv: &[String], blob: &[u8]) -> Answer {
        let req = self.next;
        self.next += 1;
        let mut w = Writer::with_capacity(op::CONTROL, blob.len() + 64).u32(req).u32(argv.len() as u32);
        for a in argv {
            w = w.str(a);
        }
        let msg = w.bytes(blob).finish();
        self.write(&msg);
        loop {
            let (o, payload) = self.read_message();
            let mut r = Reader::new(&payload);
            match o {
                op::RESULT => {
                    fn bad<T>() -> T {
                        fail(status::FAILED, "a malformed answer from the server")
                    }
                    let got = r.u32().unwrap_or_else(|_| bad());
                    if got != req {
                        continue;
                    }
                    let st = r.u32().unwrap_or_else(|_| bad());
                    let text = r.str().unwrap_or_else(|_| bad());
                    let blob = r.bytes().unwrap_or_else(|_| bad()).to_vec();
                    return Answer { status: st, text, blob };
                }
                op::ERROR => {
                    let code = r.u32().unwrap_or(status::FAILED);
                    fail(code, format!("server: {}", r.str().unwrap_or_default()));
                }
                _ => {}
            }
        }
    }

    /// A request that must succeed; its failure ends the command with the server's reason.
    fn must(&mut self, argv: &[&str], blob: &[u8]) -> Answer {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let a = self.call(&argv, blob);
        if a.status != status::OK {
            fail(a.status, a.text);
        }
        a
    }
}

/// No server yet: start one in the background, its output in the runtime folder, and connect.
fn start_server(path: &Path) -> UnixStream {
    let dir = config::runtime_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        fail(status::FAILED, format!("{}: {e}", dir.display()));
    }
    let exe = std::env::current_exe().ok().filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("/usr/bin/gui"));
    // Two descriptors rather than a dup, which the guest's std cannot make.
    let open_log = || std::fs::OpenOptions::new().create(true).append(true).open(server::server_log());
    let (out, err) = match (open_log(), open_log()) {
        (Ok(a), Ok(b)) => (Stdio::from(a), Stdio::from(b)),
        _ => (Stdio::null(), Stdio::null()),
    };
    // Not a process group of its own: `gui server --detach` makes itself a new session, which a
    // group leader could not.
    let spawned = Command::new(&exe).arg("server").arg("--detach").stdin(Stdio::null()).stdout(out).stderr(err).spawn();
    if let Err(e) = spawned {
        fail(status::FAILED, format!("cannot start the display server ({}): {e}", exe.display()));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = UnixStream::connect(path) {
            return s;
        }
        if Instant::now() > deadline {
            fail(status::FAILED, format!("the display server did not come up (see {})", server::server_log().display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ── commands ────────────────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    let code = match cmd {
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            0
        }
        "--version" | "version" => {
            println!("gui {} (protocol {})", env!("CARGO_PKG_VERSION"), proto::VERSION);
            0
        }
        "server" => {
            let a = Args::parse(rest, &[], &["--detach"], false);
            server::main(a.has("--detach"))
        }
        "status" => simple(&["status"]),
        "shutdown" => {
            let path = config::socket_path();
            if UnixStream::connect(&path).is_err() {
                println!("no server is running");
                return;
            }
            let mut c = Control::connect();
            c.must(&["shutdown"], &[]);
            // Gone when the socket is.
            let deadline = Instant::now() + Duration::from_secs(5);
            while path.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            0
        }
        "list" | "ls" | "ps" => {
            let a = Args::parse(rest, &[], &["--json"], false);
            simple(if a.has("--json") { &["list", "json"][..] } else { &["list"][..] })
        }
        "info" => {
            let a = Args::parse(rest, &[], &[], false);
            let t = a.target();
            simple(&["info", &t])
        }
        "run" | "start" => run(rest),
        "logs" | "log" => logs(rest),
        "close" => {
            let a = Args::parse(rest, &[], &[], false);
            let t = a.target();
            simple(&["close", &t])
        }
        "kill" => {
            let a = Args::parse(rest, &["--signal", "-s", "--grace"], &[], false);
            let t = a.target();
            let sig = a.get("--signal").or(a.get("-s")).unwrap_or("TERM").to_string();
            let grace: f64 = a.num("--grace").unwrap_or(3.0);
            let mut c = Control::connect();
            let ans = c.must(&["kill", &t, &sig, &((grace * 1000.0) as u64).to_string()], &[]);
            if !ans.text.is_empty() {
                println!("{}", ans.text);
            }
            0
        }
        "wait" => wait(rest),
        "screenshot" | "shot" | "capture" => screenshot(rest),
        "view" | "preview" => view(rest),
        "resize" => resize(rest),
        "focus" => {
            let a = Args::parse(rest, &[], &[], false);
            let t = a.target();
            simple(&["focus", &t])
        }
        "click" | "move" | "mousedown" | "mouseup" | "drag" | "scroll" | "key" | "keydown" | "keyup" | "type" | "sync" | "release" | "leave"
        | "preedit" => input(cmd, rest),
        "clipboard" | "clip" => clipboard(rest),
        other => fail(status::USAGE, format!("unknown command {other} (gui help)")),
    };
    std::process::exit(code);
}

/// One request; its text printed as it is.
fn simple(argv: &[&str]) -> i32 {
    let mut c = Control::connect();
    let a = c.must(argv, &[]);
    print!("{}", a.text);
    if !a.text.is_empty() && !a.text.ends_with('\n') {
        println!();
    }
    0
}

/// PROGRAM as an absolute path, found in PATH as a shell would.
fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = Path::new(program);
        let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().ok()?.join(p) };
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into());
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| p.is_file())
}

fn run(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["--name", "-n", "--size", "-s", "--env", "-e", "--cwd", "-C", "--timeout"], &["--wait", "-w"], true);
    if a.rest.is_empty() {
        fail(status::USAGE, "gui run [--name N] [--size WxH] [--wait] [--] CMD [ARGS...]");
    }
    let program = which(&a.rest[0]).unwrap_or_else(|| fail(status::NOT_FOUND, format!("{}: command not found", a.rest[0])));
    let mut argv: Vec<String> = vec!["launch".into()];
    if let Some(n) = a.get("--name").or(a.get("-n")) {
        argv.push(format!("name={n}"));
    }
    if let Some(s) = a.get("--size").or(a.get("-s")) {
        config::parse_size(s).unwrap_or_else(|| fail(status::USAGE, format!("--size {s}: WIDTHxHEIGHT")));
        argv.push(format!("size={s}"));
    }
    let cwd = a.get("--cwd").or(a.get("-C")).map(PathBuf::from).or_else(|| std::env::current_dir().ok());
    if let Some(d) = cwd {
        argv.push(format!("cwd={}", d.display()));
    }
    // The program gets this shell's environment, as if run from it, plus --env.
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !k.starts_with("COLLABO_GUI_")).collect();
    for kv in a.all("--env").into_iter().chain(a.all("-e")) {
        let (k, v) = kv.split_once('=').unwrap_or_else(|| fail(status::USAGE, format!("--env {kv}: KEY=VALUE")));
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v.to_string()));
    }
    for (k, v) in env {
        argv.push(format!("env={k}={v}"));
    }
    argv.push("--".into());
    argv.push(program.display().to_string());
    argv.extend(a.rest[1..].iter().cloned());
    let mut c = Control::connect();
    let ans = c.call(&argv, &[]);
    if ans.status != status::OK {
        fail(ans.status, ans.text);
    }
    let mut parts = ans.text.splitn(3, ' ');
    let (name, pid, log) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or(""), parts.next().unwrap_or("").to_string());
    if a.has("--wait") || a.has("-w") {
        let timeout = a.timeout_ms(30.0);
        let ans = c.call(&["wait".into(), name.clone(), "ready".into(), timeout.to_string()], &[]);
        if ans.status != status::OK {
            let tail = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<&str> = tail.lines().collect();
            let tail = lines[lines.len().saturating_sub(15)..].join("\n");
            let why = if ans.status == status::TIMEOUT { format!("{name} drew nothing within {:.0} s", timeout as f64 / 1000.0) } else { format!("{name}: {}", ans.text) };
            fail(ans.status, if tail.is_empty() { why } else { format!("{why}; its last output:\n{tail}") });
        }
        println!("{name} is up (pid {pid})");
    } else {
        println!("{name} started (pid {pid}); output: gui logs {name}");
    }
    0
}

fn logs(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["-n", "--lines"], &["-f", "--follow"], false);
    let t = a.target();
    let name = t.split(':').next().unwrap_or(&t).to_string();
    let path = server::log_dir().join(format!("{name}.log"));
    let mut f = std::fs::File::open(&path).unwrap_or_else(|_| fail(status::NOT_FOUND, format!("no log for {name} (only programs started with gui run have one)")));
    let mut text = Vec::new();
    let _ = f.read_to_end(&mut text);
    let out = match a.get("-n").or(a.get("--lines")) {
        Some(n) => {
            let n: usize = n.parse().unwrap_or_else(|_| fail(status::USAGE, "-n LINES"));
            let s = String::from_utf8_lossy(&text);
            let lines: Vec<&str> = s.lines().collect();
            let mut o = lines[lines.len().saturating_sub(n)..].join("\n");
            if !o.is_empty() {
                o.push('\n');
            }
            o.into_bytes()
        }
        None => text,
    };
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
    if a.has("-f") || a.has("--follow") {
        let mut buf = vec![0u8; 65536];
        loop {
            match f.read(&mut buf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(200)),
                Ok(n) => {
                    let _ = stdout.write_all(&buf[..n]);
                    let _ = stdout.flush();
                }
                Err(_) => return 1,
            }
        }
    }
    0
}

fn wait(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["--timeout", "-t", "--idle"], &["--ready", "--frame", "--exit", "--sync"], false);
    let t = a.target();
    let timeout = a.timeout_ms(30.0).to_string();
    let argv: Vec<String> = if let Some(ms) = a.get("--idle") {
        vec!["wait".into(), t, "idle".into(), ms.into(), timeout]
    } else if a.has("--frame") {
        vec!["wait".into(), t, "frame".into(), timeout]
    } else if a.has("--exit") {
        vec!["wait".into(), t, "exit".into(), timeout]
    } else if a.has("--sync") {
        vec!["wait".into(), t, "sync".into(), timeout]
    } else {
        vec!["wait".into(), t, "ready".into(), timeout]
    };
    let mut c = Control::connect();
    let ans = c.call(&argv, &[]);
    if ans.status != status::OK {
        fail(ans.status, if ans.text.is_empty() { "timed out".into() } else { ans.text });
    }
    if a.has("--exit") && !ans.text.is_empty() {
        println!("{}", ans.text);
    }
    0
}

/// The canvas's pixels, after it has been still for `idle` ms if asked.
fn capture(c: &mut Control, target: &str, region: Option<&str>, idle: Option<&str>, timeout: u64) -> (image::Image, String, (u32, u32)) {
    if let Some(ms) = idle {
        let ans = c.call(&["wait".into(), target.into(), "idle".into(), ms.into(), timeout.to_string()], &[]);
        if ans.status != status::OK && ans.status != status::TIMEOUT {
            fail(ans.status, ans.text);
        }
    }
    let mut argv = vec!["capture".to_string(), target.to_string()];
    if let Some(r) = region {
        let v: Vec<&str> = r.split(',').collect();
        if v.len() != 4 {
            fail(status::USAGE, "--region X,Y,WIDTH,HEIGHT");
        }
        argv.extend(v.iter().map(|s| parse_num(s.trim(), "--region").to_string()));
    }
    let ans = c.call(&argv, &[]);
    if ans.status != status::OK {
        fail(ans.status, ans.text);
    }
    let f: Vec<&str> = ans.text.split(' ').collect();
    let (w, h): (u32, u32) = (f[0].parse().unwrap_or(0), f[1].parse().unwrap_or(0));
    let full = (f.get(4).and_then(|v| v.parse().ok()).unwrap_or(w), f.get(5).and_then(|v| v.parse().ok()).unwrap_or(h));
    let name = f.get(3).unwrap_or(&"canvas").to_string();
    if ans.blob.len() != w as usize * h as usize * 4 {
        fail(status::FAILED, "the server sent a short frame");
    }
    (image::Image { w, h, px: ans.blob }, name, full)
}

fn screenshot(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["-o", "--output", "--format", "-f", "--region", "--scale", "--max-width", "--max-height", "--idle", "--timeout"], &[], false);
    let t = a.target();
    let mut c = Control::connect();
    let (mut img, name, full) = capture(&mut c, &t, a.get("--region"), a.get("--idle"), a.timeout_ms(10.0));
    let (ow, oh) = (img.w, img.h);
    let mut scale: f64 = a.num("--scale").unwrap_or(1.0);
    if let Some(mw) = a.num::<u32>("--max-width") {
        scale = scale.min(mw as f64 / img.w as f64);
    }
    if let Some(mh) = a.num::<u32>("--max-height") {
        scale = scale.min(mh as f64 / img.h as f64);
    }
    if !(scale > 0.0) || scale > 8.0 {
        fail(status::USAGE, "--scale between 0 and 8");
    }
    if (scale - 1.0).abs() > 1e-9 {
        img = img.scaled(((img.w as f64 * scale).round() as u32).max(1), ((img.h as f64 * scale).round() as u32).max(1));
    }
    let out = a.get("-o").or(a.get("--output")).map(str::to_string).unwrap_or_else(|| format!("{}.png", name.replace(':', "-")));
    let format = a.get("--format").or(a.get("-f")).map(str::to_string).unwrap_or_else(|| {
        if out.ends_with(".ppm") {
            "ppm".into()
        } else if out.ends_with(".raw") || out.ends_with(".bgrx") {
            "raw".into()
        } else {
            "png".into()
        }
    });
    let bytes = match format.as_str() {
        "png" => img.png(),
        "ppm" => img.ppm(),
        "raw" | "bgrx" => img.px.clone(),
        other => fail(status::USAGE, format!("--format {other}: png, ppm or raw")),
    };
    if out == "-" {
        if std::io::stdout().is_terminal() {
            fail(status::USAGE, "not writing an image to a terminal (-o FILE, or pipe it)");
        }
        let mut so = std::io::stdout();
        let _ = so.write_all(&bytes);
        let _ = so.flush();
        eprintln!("{name} {}x{} ({} bytes {format})", img.w, img.h, bytes.len());
    } else {
        std::fs::write(&out, &bytes).unwrap_or_else(|e| fail(status::FAILED, format!("{out}: {e}")));
        let region = if (ow, oh) != full { format!(" (region of {}x{})", full.0, full.1) } else { String::new() };
        println!("{out}: {name} {}x{}{region}, {} bytes", img.w, img.h, bytes.len());
    }
    0
}

fn view(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["--width", "-w", "--region", "--idle", "--timeout"], &[], false);
    let t = a.target();
    let mut c = Control::connect();
    let (img, name, _) = capture(&mut c, &t, a.get("--region"), a.get("--idle"), a.timeout_ms(10.0));
    let cols = a.num::<u32>("--width").or(a.num("-w")).unwrap_or_else(|| sys::terminal_size().map(|(c, _)| c as u32).unwrap_or(80));
    print!("{}", img.ansi(cols));
    println!("{name} {}x{}", img.w, img.h);
    0
}

fn resize(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["--timeout"], &["--wait", "-w"], false);
    let t = a.target();
    let size = a.pos.get(1).unwrap_or_else(|| fail(status::USAGE, "gui resize TARGET WIDTHxHEIGHT"));
    let (w, h) = config::parse_size(size).unwrap_or_else(|| fail(status::USAGE, format!("{size}: WIDTHxHEIGHT")));
    let mut c = Control::connect();
    let ans = c.must(&["resize", &t, &w.to_string(), &h.to_string()], &[]);
    let got = ans.text.clone();
    if got != format!("{w}x{h}") {
        eprintln!("gui: {w}x{h} is outside the limits; asked for {got} (gui status)");
    }
    if a.has("--wait") || a.has("-w") {
        let (gw, gh) = config::parse_size(&got).unwrap_or((w, h));
        let ans = c.call(&["wait".into(), t.clone(), "size".into(), gw.to_string(), gh.to_string(), a.timeout_ms(10.0).to_string()], &[]);
        if ans.status != status::OK {
            fail(ans.status, format!("{t} did not redraw at {got}: {}", ans.text));
        }
    }
    println!("{got}");
    0
}

fn button_number(s: Option<&str>) -> u32 {
    match s.unwrap_or("left") {
        "left" | "l" | "1" => 1,
        "middle" | "m" | "2" => 2,
        "right" | "r" | "3" => 3,
        "back" | "8" => 8,
        "forward" | "9" => 9,
        other => other.parse().ok().filter(|n| (1..=32).contains(n)).unwrap_or_else(|| fail(status::USAGE, format!("--button {other}: left, middle, right or a number"))),
    }
}

fn input(cmd: &str, rest: &[String]) -> i32 {
    let a = Args::parse(
        rest,
        &["--button", "-b", "--count", "--steps", "--at", "--timeout", "--delay", "--mods", "-m", "--cursor"],
        &["--double", "--no-focus", "--no-sync", "--commit", "--ime", "--pixels"],
        false,
    );
    let t = a.target();
    // Words after `--` are positional too: `gui type T -- -text-`.
    let words: Vec<String> = a.pos[1..].iter().chain(a.rest.iter()).cloned().collect();
    let p = &words[..];
    let mut ev: Vec<String> = Vec::new();
    let s = |v: &str| v.to_string();
    let xy = |i: usize| -> Option<(i64, i64)> {
        match (p.get(i), p.get(i + 1)) {
            (Some(x), Some(y)) => Some((parse_num(x, "X"), parse_num(y, "Y"))),
            (Some(_), None) => fail(status::USAGE, "X needs a Y"),
            _ => None,
        }
    };
    let button = button_number(a.get("--button").or(a.get("-b"))).to_string();
    let mut delays: Vec<usize> = Vec::new(); // event batch boundaries for --delay
    match cmd {
        "move" => {
            let (x, y) = xy(0).unwrap_or_else(|| fail(status::USAGE, "gui move TARGET X Y"));
            ev.extend([s("motion"), x.to_string(), y.to_string()]);
        }
        "click" | "mousedown" | "mouseup" => {
            if let Some((x, y)) = xy(0) {
                ev.extend([s("motion"), x.to_string(), y.to_string()]);
            }
            let count = if a.has("--double") { 2 } else { a.num::<u32>("--count").unwrap_or(1).clamp(1, 10) };
            for _ in 0..count {
                if cmd != "mouseup" {
                    ev.extend([s("button"), button.clone(), s("down")]);
                }
                if cmd != "mousedown" {
                    ev.extend([s("button"), button.clone(), s("up")]);
                }
            }
        }
        "drag" => {
            let (x1, y1) = xy(0).unwrap_or_else(|| fail(status::USAGE, "gui drag TARGET X1 Y1 X2 Y2"));
            let (x2, y2) = xy(2).unwrap_or_else(|| fail(status::USAGE, "gui drag TARGET X1 Y1 X2 Y2"));
            let steps = a.num::<i64>("--steps").unwrap_or(10).clamp(1, 1000);
            ev.extend([s("motion"), x1.to_string(), y1.to_string(), s("button"), button.clone(), s("down")]);
            for i in 1..=steps {
                ev.extend([s("motion"), (x1 + (x2 - x1) * i / steps).to_string(), (y1 + (y2 - y1) * i / steps).to_string()]);
            }
            ev.extend([s("button"), button.clone(), s("up")]);
        }
        "scroll" => {
            if let Some(at) = a.opts.get("--at") {
                // --at X Y: Y is the next positional word.
                let x = parse_num(&at[0], "--at X");
                let y = parse_num(p.first().unwrap_or_else(|| fail(status::USAGE, "--at X Y")), "--at Y");
                ev.extend([s("motion"), x.to_string(), y.to_string()]);
            }
            let q = if a.has("--at") { &p[1.min(p.len())..] } else { p };
            let dy = q.first().map(|v| parse_num(v, "DY")).unwrap_or_else(|| fail(status::USAGE, "gui scroll TARGET [--at X Y] DY [DX]"));
            let dx = q.get(1).map(|v| parse_num(v, "DX")).unwrap_or(0);
            if a.has("--pixels") {
                ev.extend([s("wheel"), dx.to_string(), dy.to_string(), s("0"), s("0")]);
            } else {
                ev.extend([s("wheel"), (dx * 40).to_string(), (dy * 40).to_string(), dx.to_string(), dy.to_string()]);
            }
        }
        "key" | "keydown" | "keyup" => {
            if p.is_empty() {
                fail(status::USAGE, format!("gui {cmd} TARGET COMBO... (Return, ctrl+a, shift+Tab, shift, ...)"));
            }
            for combo in p {
                let k = keys::parse_combo(combo).unwrap_or_else(|e| fail(status::USAGE, e));
                match cmd {
                    "key" => push_stroke(&mut ev, &k),
                    "keydown" => {
                        for (sym, code) in &k.mods {
                            push_key(&mut ev, *sym, *code, true, "");
                        }
                        push_key(&mut ev, k.keysym, k.keycode, true, &k.text);
                    }
                    _ => {
                        push_key(&mut ev, k.keysym, k.keycode, false, "");
                        for (sym, code) in k.mods.iter().rev() {
                            push_key(&mut ev, *sym, *code, false, "");
                        }
                    }
                }
                delays.push(ev.len());
            }
        }
        "release" => ev.push(s("release")),
        "leave" => ev.push(s("leave")),
        "preedit" => {
            let text = p.join(" ");
            let (b, e) = match a.get("--cursor") {
                None => (text.len() as i64, text.len() as i64),
                Some(c) => {
                    let v: Vec<i64> = c.split(',').map(|x| parse_num(x.trim(), "--cursor")).collect();
                    (v[0], *v.get(1).unwrap_or(&v[0]))
                }
            };
            ev.extend([s("preedit"), text, b.to_string(), e.to_string()]);
        }
        "type" => {
            let text = if p.len() == 1 && p[0] == "-" {
                let mut s = String::new();
                let _ = std::io::stdin().read_to_string(&mut s);
                s
            } else {
                p.join(" ")
            };
            if text.is_empty() {
                fail(status::USAGE, "gui type TARGET TEXT");
            }
            if a.has("--commit") {
                ev.extend([s("text"), text]);
            } else if a.has("--ime") {
                // A Korean input method: each syllable composed in the preedit, then committed;
                // what it does not compose (Latin, digits, space, Return) typed as keys.
                for ch in text.chars() {
                    let jamo = ('\u{3131}'..='\u{318e}').contains(&ch);
                    match keys::hangul_steps(ch) {
                        Some(steps) => {
                            for step in &steps[..steps.len() - 1] {
                                let t = step.to_string();
                                ev.extend([s("preedit"), t.clone(), t.len().to_string(), t.len().to_string()]);
                            }
                            ev.extend([s("text"), ch.to_string()]);
                        }
                        None if jamo => {
                            let t = ch.to_string();
                            ev.extend([s("preedit"), t.clone(), t.len().to_string(), t.len().to_string(), s("text"), t]);
                        }
                        None => push_stroke(&mut ev, &keys::char_stroke(ch)),
                    }
                    delays.push(ev.len());
                }
            } else {
                for ch in text.chars() {
                    push_stroke(&mut ev, &keys::char_stroke(ch));
                    delays.push(ev.len());
                }
            }
        }
        "sync" => {}
        _ => unreachable!(),
    }
    // --mods: the modifier keys held around the mouse input.
    if let Some(m) = a.get("--mods").or(a.get("-m")) {
        let mods = keys::parse_mods(m).unwrap_or_else(|e| fail(status::USAGE, e));
        let mut wrapped = Vec::new();
        for (sym, code) in &mods {
            push_key(&mut wrapped, *sym, *code, true, "");
        }
        wrapped.append(&mut ev);
        for (sym, code) in mods.iter().rev() {
            push_key(&mut wrapped, *sym, *code, false, "");
        }
        ev = wrapped;
        delays.clear();
    }
    let mut c = Control::connect();
    let nofocus = a.has("--no-focus");
    let delay = a.num::<u64>("--delay").unwrap_or(0);
    let batches: Vec<&[String]> = if delay > 0 && !delays.is_empty() {
        let mut out = Vec::new();
        let mut start = 0;
        for end in delays {
            out.push(&ev[start..end]);
            start = end;
        }
        out
    } else {
        vec![&ev[..]]
    };
    for (i, batch) in batches.iter().enumerate() {
        if batch.is_empty() && cmd != "sync" {
            continue;
        }
        if !batch.is_empty() {
            let mut argv = vec![s("input"), t.clone()];
            if nofocus {
                argv.push(s("nofocus"));
            }
            argv.extend(batch.iter().cloned());
            let ans = c.call(&argv, &[]);
            if ans.status != status::OK {
                fail(ans.status, ans.text);
            }
        }
        if delay > 0 && i + 1 < batches.len() {
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    if !a.has("--no-sync") || cmd == "sync" {
        let timeout = a.timeout_ms(5.0);
        let ans = c.call(&[s("wait"), t.clone(), s("sync"), timeout.to_string()], &[]);
        if ans.status == status::TIMEOUT {
            fail(status::TIMEOUT, format!("{t} has not handled the input within {:.1} s (busy or hung?)", timeout as f64 / 1000.0));
        } else if ans.status != status::OK {
            fail(ans.status, ans.text);
        }
    }
    0
}

fn push_key(ev: &mut Vec<String>, keysym: u32, keycode: u32, down: bool, text: &str) {
    ev.extend(["key".into(), keysym.to_string(), keycode.to_string(), if down { "down" } else { "up" }.into(), text.to_string()]);
}

fn push_stroke(ev: &mut Vec<String>, k: &keys::Stroke) {
    for (sym, code) in &k.mods {
        push_key(ev, *sym, *code, true, "");
    }
    push_key(ev, k.keysym, k.keycode, true, &k.text);
    push_key(ev, k.keysym, k.keycode, false, "");
    for (sym, code) in k.mods.iter().rev() {
        push_key(ev, *sym, *code, false, "");
    }
}

fn clipboard(rest: &[String]) -> i32 {
    let a = Args::parse(rest, &["--type", "-t", "-o", "--output"], &[], false);
    let sub = a.pos.first().map(String::as_str).unwrap_or("get");
    let mime = a.get("--type").or(a.get("-t")).unwrap_or("").to_string();
    let mut c = Control::connect();
    match sub {
        "get" | "paste" => {
            let ans = c.must(&["clipboard", "get", &mime], &[]);
            match a.get("-o").or(a.get("--output")) {
                Some(f) => std::fs::write(f, &ans.blob).unwrap_or_else(|e| fail(status::FAILED, format!("{f}: {e}"))),
                None => {
                    let mut so = std::io::stdout();
                    let _ = so.write_all(&ans.blob);
                    if so.is_terminal() && !ans.blob.ends_with(b"\n") && ans.text.starts_with("text/") {
                        let _ = so.write_all(b"\n");
                    }
                    let _ = so.flush();
                }
            }
        }
        "set" | "copy" => {
            let data = match a.pos.get(1) {
                Some(t) if t != "-" => a.pos[1..].join(" ").into_bytes(),
                _ => {
                    let mut v = Vec::new();
                    let _ = std::io::stdin().read_to_end(&mut v);
                    v
                }
            };
            c.must(&["clipboard", "set", &mime], &data);
        }
        "types" => {
            let ans = c.must(&["clipboard", "types"], &[]);
            print!("{}", ans.text);
        }
        "clear" => {
            c.must(&["clipboard", "clear"], &[]);
        }
        other => fail(status::USAGE, format!("gui clipboard {other}: get, set, types or clear")),
    }
    0
}
