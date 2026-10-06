//! The display server: one process, one thread, one poll loop. It owns every canvas — a whole
//! frame of pixels per program window, kept complete whether anyone looks or not; there are no
//! window positions and nothing overlaps — and serves the programs that draw into them (apps) and
//! the command line that reads and drives them (control clients) on one Unix socket.
//!
//! It never blocks on a peer: sockets are non-blocking, output is queued per connection, and a
//! peer that stops reading is cut off once its queue passes a limit. Programs it starts (`gui
//! run`) get their own process group, their output in a log file, and are reaped through a
//! SIGCHLD self-pipe.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::proto::{self, modifier, op, status, Reader, Writer};
use crate::sys;

/// A peer whose unread output passes this (beyond one whole frame of the largest canvas, for a
/// screenshot) is disconnected.
const MAX_QUEUE: usize = 64 << 20;
/// Exited programs kept in the list (oldest go first).
const KEEP_EXITED: usize = 16;
/// A connection that has not said HELLO by then is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// A program's log (in /tmp, which is memory) past this moves to NAME.log.1 and starts again, so
/// a program's output takes at most twice this.
const LOG_LIMIT: u64 = 4 << 20;

type Id = u32;

enum Kind {
    New,
    App(Id),
    Control,
}

struct Conn {
    stream: UnixStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    kind: Kind,
    dead: bool,
    since: Instant,
}

#[derive(Clone, PartialEq)]
enum State {
    Starting,
    Running,
    Exited(String),
}

struct App {
    id: Id,
    name: String,
    token: String,
    conn: Option<Id>,
    child: Option<Child>,
    pid: u32,
    argv: Vec<String>,
    state: State,
    started: Instant,
    exited: Option<Instant>,
    log: Option<PathBuf>,
    /// The size `gui run --size` asked for; it wins over what the program asks for.
    size: Option<(u32, u32)>,
    canvases: Vec<Id>,
    next_index: u32,
    kill_at: Option<Instant>,
    pong: u32,
    /// The read end of the pipe the program's stdout and stderr go to, and the log they are
    /// copied into. The server reads it, so a program that floods it waits for the server
    /// rather than filling the memory.
    output: Option<Output>,
}

struct Output {
    pipe: File,
    log: File,
    size: u64,
}

struct Update {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    data: Vec<u8>,
}

struct Canvas {
    id: Id,
    app: Id,
    index: u32,
    /// The program's own number for it.
    handle: u32,
    title: String,
    cursor: String,
    /// The size the server asked for (CONFIGURE).
    configured: (u32, u32),
    /// The size of the last frame, which is what `pixels` holds (BGRX, 4 bytes a pixel).
    size: (u32, u32),
    pixels: Vec<u8>,
    pending: Vec<Update>,
    pending_bytes: usize,
    frames: u64,
    last_frame: Option<Instant>,
    pointer: (i32, i32),
    buttons: u32,
    mods: u32,
    /// Has the pointer entered (POINTER_ENTER sent, no POINTER_LEAVE since)?
    inside: bool,
    /// The program's text field, if one has the focus: its caret (TEXT_INPUT).
    text_input: Option<(i32, i32, u32, u32)>,
    /// The composition last sent (PREEDIT), until a commit or an empty one.
    preedit: String,
}

enum Wait {
    /// The program has a canvas with a frame in it.
    Ready(Id),
    /// A frame after the one counted.
    Frame(Id, u64),
    /// No frame for this long (and at least one frame).
    Idle(Id, Duration),
    /// A frame of this size.
    Size(Id, u32, u32),
    /// The program has exited.
    Exit(Id),
    /// The program has handled everything sent before the PING with this serial.
    Pong(Id, u32),
}

struct Waiter {
    conn: Id,
    req: u32,
    deadline: Instant,
    wait: Wait,
}

pub struct Server {
    cfg: Config,
    dir: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
    sigchld: i32,
    conns: HashMap<Id, Conn>,
    apps: Vec<App>,
    canvases: HashMap<Id, Canvas>,
    focus: Option<Id>,
    clipboard: Vec<(String, Vec<u8>)>,
    waiters: Vec<Waiter>,
    next_id: Id,
    ping_serial: u32,
    stopping: Option<Instant>,
    _lock: File,
    started: Instant,
}

fn log(msg: &str) {
    eprintln!("gui-server: {msg}");
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A name a program can be addressed by: letters, digits, `. _ -`, starting with a letter.
pub fn clean_name(raw: &str) -> String {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let mut s: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(32).collect();
    if !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        s.insert_str(0, "app");
    }
    s
}

fn describe(status: std::process::ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exited({c})"),
        (None, Some(s)) => format!("killed({s})"),
        _ => "exited".into(),
    }
}

impl Server {
    /// Binds the socket, unless another server holds the lock (then Ok(None)).
    pub fn bind(cfg: Config) -> io::Result<Option<Server>> {
        let dir = config::runtime_dir();
        std::fs::create_dir_all(dir.join("logs"))?;
        let lock = File::create(dir.join("server.lock"))?;
        if !sys::try_lock(&lock)? {
            return Ok(None);
        }
        let socket = config::socket_path();
        // We hold the lock, so a socket file left behind is a dead server's.
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let sigchld = sys::sigchld_pipe()?;
        sys::ignore_sigpipe();
        std::fs::write(dir.join("server.pid"), format!("{}\n", std::process::id()))?;
        Ok(Some(Server {
            cfg,
            dir,
            socket,
            listener,
            sigchld,
            conns: HashMap::new(),
            apps: Vec::new(),
            canvases: HashMap::new(),
            focus: None,
            clipboard: Vec::new(),
            waiters: Vec::new(),
            next_id: 1,
            ping_serial: 0,
            stopping: None,
            _lock: lock,
            started: Instant::now(),
        }))
    }

    fn id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    pub fn run(mut self) -> i32 {
        log(&format!("listening on {} (pid {})", self.socket.display(), std::process::id()));
        loop {
            if let Some(at) = self.stopping {
                let live = self.apps.iter().any(|a| a.child.is_some());
                if !live || Instant::now() >= at {
                    for a in &mut self.apps {
                        if let Some(c) = &a.child {
                            sys::kill_group(c.id(), libc::SIGKILL);
                        }
                    }
                    // Answers to `gui shutdown` and the like go out before we leave.
                    self.flush_all();
                    let _ = std::fs::remove_file(&self.socket);
                    let _ = std::fs::remove_file(self.dir.join("server.pid"));
                    log("stopped");
                    return 0;
                }
            }
            let mut fds = vec![(self.listener.as_raw_fd(), sys::IN), (self.sigchld, sys::IN)];
            let ids: Vec<Id> = self.conns.keys().copied().collect();
            for id in &ids {
                let c = &self.conns[id];
                let ev = if c.wpos < c.wbuf.len() { sys::IN | sys::OUT } else { sys::IN };
                fds.push((c.stream.as_raw_fd(), ev));
            }
            let outs: Vec<Id> = self.apps.iter().filter(|a| a.output.is_some()).map(|a| a.id).collect();
            for a in &self.apps {
                if let Some(o) = &a.output {
                    fds.push((o.pipe.as_raw_fd(), sys::IN));
                }
            }
            let timeout = self.next_timeout();
            let revents = match sys::poll(&fds, timeout) {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("poll: {e}"));
                    return 1;
                }
            };
            if revents[1] != 0 {
                sys::drain(self.sigchld);
            }
            self.reap();
            if revents[0] != 0 {
                self.accept();
            }
            for (i, app) in outs.iter().enumerate() {
                if revents[2 + ids.len() + i] != 0 {
                    self.copy_output(*app);
                }
            }
            for (i, id) in ids.iter().enumerate() {
                let r = revents[i + 2];
                if r == 0 {
                    continue;
                }
                if r & (sys::IN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read(*id);
                }
                if r & sys::OUT != 0 {
                    self.flush(*id);
                }
            }
            self.timers();
            self.collect_dead();
        }
    }

    fn next_timeout(&self) -> i32 {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| next = Some(next.map_or(t, |n: Instant| n.min(t)));
        for w in &self.waiters {
            consider(w.deadline);
            if let Wait::Idle(c, quiet) = w.wait {
                if let Some(at) = self.canvases.get(&c).and_then(|c| c.last_frame) {
                    consider(at + quiet);
                }
            }
        }
        for a in &self.apps {
            if let Some(t) = a.kill_at {
                consider(t);
            }
        }
        if let Some(t) = self.stopping {
            consider(t);
        }
        if self.apps.iter().any(|a| a.child.is_some()) {
            // A safety net in case a SIGCHLD was missed.
            consider(now + Duration::from_secs(2));
        }
        for c in self.conns.values() {
            if matches!(c.kind, Kind::New) {
                consider(c.since + HELLO_TIMEOUT);
            }
        }
        match next {
            None => -1,
            Some(t) => t.saturating_duration_since(now).as_millis().min(60_000) as i32 + 1,
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.id();
                    self.conns.insert(id, Conn { stream, rbuf: Vec::new(), wbuf: Vec::new(), wpos: 0, kind: Kind::New, dead: false, since: Instant::now() });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log(&format!("accept: {e}"));
                    return;
                }
            }
        }
    }

    fn read(&mut self, id: Id) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        let mut budget = 16usize << 20;
        loop {
            let len = c.rbuf.len();
            c.rbuf.resize(len + 65536, 0);
            match c.stream.read(&mut c.rbuf[len..]) {
                Ok(0) => {
                    c.rbuf.truncate(len);
                    c.dead = true;
                    break;
                }
                Ok(n) => {
                    c.rbuf.truncate(len + n);
                    budget = budget.saturating_sub(n);
                    if budget == 0 {
                        break;
                    }
                }
                Err(e) => {
                    c.rbuf.truncate(len);
                    match e.kind() {
                        io::ErrorKind::WouldBlock => break,
                        io::ErrorKind::Interrupted => continue,
                        _ => {
                            c.dead = true;
                            break;
                        }
                    }
                }
            }
        }
        let mut buf = std::mem::take(&mut c.rbuf);
        let mut off = 0;
        loop {
            match proto::next_message(&buf[off..], proto::MAX_PAYLOAD) {
                Ok(Some((o, payload, used))) => {
                    if self.handle(id, o, payload).is_err() {
                        self.error(id, status::PROTOCOL, &format!("malformed message 0x{o:04x}"));
                        self.kill_conn(id);
                    }
                    off += used;
                    if self.conns.get(&id).is_none_or(|c| c.dead) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    self.error(id, status::PROTOCOL, "message too large");
                    self.kill_conn(id);
                    break;
                }
            }
        }
        buf.drain(..off);
        // A big frame came through; do not keep its buffer for ever.
        if buf.capacity() > 4 << 20 && buf.len() < 1 << 20 {
            buf.shrink_to(1 << 20);
        }
        if let Some(c) = self.conns.get_mut(&id) {
            c.rbuf = buf;
        }
    }

    fn kill_conn(&mut self, id: Id) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.dead = true;
        }
    }

    fn send(&mut self, id: Id, msg: Vec<u8>) {
        let limit = MAX_QUEUE + self.cfg.max_size.0 as usize * self.cfg.max_size.1 as usize * 4;
        let Some(c) = self.conns.get_mut(&id) else { return };
        if c.dead {
            return;
        }
        if c.wpos == c.wbuf.len() {
            c.wbuf.clear();
            c.wpos = 0;
        }
        c.wbuf.extend_from_slice(&msg);
        if c.wbuf.len() - c.wpos > limit {
            log("a client stopped reading; disconnecting it");
            c.dead = true;
            return;
        }
        self.flush(id);
    }

    fn flush(&mut self, id: Id) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        while c.wpos < c.wbuf.len() {
            match c.stream.write(&c.wbuf[c.wpos..]) {
                Ok(0) => {
                    c.dead = true;
                    return;
                }
                Ok(n) => c.wpos += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    c.dead = true;
                    return;
                }
            }
        }
        c.wbuf.clear();
        c.wpos = 0;
        if c.wbuf.capacity() > 1 << 20 {
            c.wbuf.shrink_to(64 << 10);
        }
    }

    /// Blocks (briefly) until every queue is written, for the last answers before exiting.
    fn flush_all(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let ids: Vec<Id> = self.conns.keys().copied().collect();
        for id in ids {
            while self.conns.get(&id).is_some_and(|c| !c.dead && c.wpos < c.wbuf.len()) && Instant::now() < deadline {
                self.flush(id);
                if let Some(c) = self.conns.get(&id) {
                    let _ = sys::poll(&[(c.stream.as_raw_fd(), sys::OUT)], 50);
                }
            }
        }
    }

    fn error(&mut self, id: Id, code: u32, msg: &str) {
        self.send(id, Writer::new(op::ERROR).u32(code).str(msg).finish());
    }

    fn reply(&mut self, conn: Id, req: u32, code: u32, text: &str, blob: &[u8]) {
        let msg = Writer::with_capacity(op::RESULT, 16 + text.len() + blob.len()).u32(req).u32(code).str(text).bytes(blob).finish();
        self.send(conn, msg);
    }

    /// What the program wrote, into its log; a log past LOG_LIMIT moves to NAME.log.1.
    fn copy_output(&mut self, app: Id) {
        let Some(i) = self.app_index(app) else { return };
        let a = &mut self.apps[i];
        let Some(o) = &mut a.output else { return };
        let mut buf = [0u8; 65536];
        let mut budget = 1usize << 20;
        loop {
            match o.pipe.read(&mut buf) {
                Ok(0) => {
                    a.output = None;
                    return;
                }
                Ok(n) => {
                    let mut chunk = &buf[..n];
                    if o.size + n as u64 > LOG_LIMIT {
                        // The old file ends with a whole line where the chunk has one.
                        if let Some(nl) = chunk.iter().rposition(|&b| b == b'\n') {
                            let _ = o.log.write_all(&chunk[..=nl]);
                            chunk = &chunk[nl + 1..];
                        }
                        if let Some(path) = &a.log {
                            let old = path.with_extension("log.1");
                            if std::fs::rename(path, &old).is_ok() {
                                if let Ok(f) = File::create(path) {
                                    o.log = f;
                                    let note = format!("[gui: the earlier output is in {}]\n", old.display());
                                    let _ = o.log.write_all(note.as_bytes());
                                    o.size = note.len() as u64;
                                }
                            }
                        }
                    }
                    let _ = o.log.write_all(chunk);
                    o.size += chunk.len() as u64;
                    budget = budget.saturating_sub(n);
                    if budget == 0 {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    a.output = None;
                    return;
                }
            }
        }
    }

    fn collect_dead(&mut self) {
        let dead: Vec<Id> = self.conns.iter().filter(|(_, c)| c.dead).map(|(id, _)| *id).collect();
        for id in dead {
            let c = self.conns.remove(&id).unwrap();
            self.waiters.retain(|w| w.conn != id);
            if let Kind::App(app) = c.kind {
                self.app_disconnected(app);
            }
        }
    }

    fn app_index(&self, id: Id) -> Option<usize> {
        self.apps.iter().position(|a| a.id == id)
    }

    fn app_disconnected(&mut self, app: Id) {
        let Some(i) = self.app_index(app) else { return };
        let canvases = std::mem::take(&mut self.apps[i].canvases);
        for c in canvases {
            self.drop_canvas(c);
        }
        let a = &mut self.apps[i];
        a.conn = None;
        if a.child.is_none() && !matches!(a.state, State::Exited(_)) {
            // Not ours to wait for: gone as far as we can tell.
            a.state = State::Exited("disconnected".into());
            a.exited = Some(Instant::now());
            log(&format!("{} disconnected", a.name));
            self.resolve_exit(app);
        }
    }

    fn drop_canvas(&mut self, id: Id) {
        if self.canvases.remove(&id).is_none() {
            return;
        }
        if self.focus == Some(id) {
            self.focus = None;
        }
        let pending: Vec<(Id, u32)> = self
            .waiters
            .iter()
            .filter(|w| matches!(w.wait, Wait::Frame(c, _) | Wait::Idle(c, _) | Wait::Size(c, _, _) if c == id))
            .map(|w| (w.conn, w.req))
            .collect();
        self.waiters.retain(|w| !matches!(w.wait, Wait::Frame(c, _) | Wait::Idle(c, _) | Wait::Size(c, _, _) if c == id));
        for (conn, req) in pending {
            self.reply(conn, req, status::NOT_FOUND, "the canvas was closed", &[]);
        }
    }

    fn reap(&mut self) {
        let mut exited = Vec::new();
        for a in &mut self.apps {
            let Some(child) = &mut a.child else { continue };
            if let Ok(Some(st)) = child.try_wait() {
                a.state = State::Exited(describe(st));
                a.exited = Some(Instant::now());
                a.child = None;
                a.kill_at = None;
                log(&format!("{} {}", a.name, describe(st)));
                exited.push(a.id);
            }
        }
        for id in exited {
            self.resolve_exit(id);
        }
        // Keep the list short: the oldest exited records go.
        let mut gone: Vec<(Instant, Id)> = self.apps.iter().filter_map(|a| a.exited.filter(|_| a.conn.is_none()).map(|t| (t, a.id))).collect();
        if gone.len() > KEEP_EXITED {
            gone.sort();
            for (_, id) in &gone[..gone.len() - KEEP_EXITED] {
                self.apps.retain(|a| a.id != *id);
            }
        }
    }

    fn resolve_exit(&mut self, app: Id) {
        let text = match self.app_index(app).map(|i| &self.apps[i].state) {
            Some(State::Exited(s)) => s.clone(),
            _ => "exited".into(),
        };
        let mut answers = Vec::new();
        self.waiters.retain(|w| match w.wait {
            Wait::Exit(a) if a == app => {
                answers.push((w.conn, w.req, status::OK, text.clone()));
                false
            }
            Wait::Ready(a) | Wait::Pong(a, _) if a == app => {
                answers.push((w.conn, w.req, status::FAILED, format!("the program {text}")));
                false
            }
            _ => true,
        });
        for (conn, req, code, msg) in answers {
            self.reply(conn, req, code, &msg, &[]);
        }
    }

    fn timers(&mut self) {
        let now = Instant::now();
        for c in self.conns.values_mut() {
            if matches!(c.kind, Kind::New) && now >= c.since + HELLO_TIMEOUT {
                c.dead = true;
            }
        }

        for a in &mut self.apps {
            if let (Some(t), Some(child)) = (a.kill_at, &a.child) {
                if now >= t {
                    sys::kill_group(child.id(), libc::SIGKILL);
                    a.kill_at = None;
                }
            }
        }
        let mut answers = Vec::new();
        self.waiters.retain(|w| {
            if let Wait::Idle(c, quiet) = w.wait {
                if let Some(at) = self.canvases.get(&c).and_then(|c| c.last_frame) {
                    if now >= at + quiet {
                        answers.push((w.conn, w.req, status::OK, String::new()));
                        return false;
                    }
                }
            }
            if now >= w.deadline {
                answers.push((w.conn, w.req, status::TIMEOUT, "timed out".to_string()));
                return false;
            }
            true
        });
        for (conn, req, code, text) in answers {
            self.reply(conn, req, code, &text, &[]);
        }
    }

    // ── messages ────────────────────────────────────────────────────────────────────────────

    fn handle(&mut self, id: Id, o: u16, payload: &[u8]) -> Result<(), proto::Malformed> {
        let mut r = Reader::new(payload);
        let kind = match self.conns.get(&id).map(|c| &c.kind) {
            Some(Kind::New) => 0,
            Some(Kind::App(a)) => *a,
            Some(Kind::Control) => u32::MAX,
            None => return Ok(()),
        };
        if kind == 0 {
            if o != op::HELLO {
                self.error(id, status::PROTOCOL, "HELLO first");
                self.kill_conn(id);
                return Ok(());
            }
            let version = r.u32()?;
            let role = r.u32()?;
            let pid = r.u32()?;
            let name = r.str()?;
            let token = r.str()?;
            if version != proto::VERSION {
                self.error(id, status::PROTOCOL, &format!("protocol version {version}; this server speaks {}", proto::VERSION));
                self.kill_conn(id);
                return Ok(());
            }
            let app_name = match role {
                proto::ROLE_CONTROL => {
                    self.conns.get_mut(&id).unwrap().kind = Kind::Control;
                    String::new()
                }
                proto::ROLE_APP => {
                    let app = self.attach_app(id, pid, &name, &token);
                    self.conns.get_mut(&id).unwrap().kind = Kind::App(app);
                    self.apps[self.app_index(app).unwrap()].name.clone()
                }
                _ => {
                    self.error(id, status::PROTOCOL, &format!("unknown role {role}"));
                    self.kill_conn(id);
                    return Ok(());
                }
            };
            let welcome = Writer::new(op::WELCOME)
                .u32(proto::VERSION)
                .u32(id)
                .str(&app_name)
                .u32(self.cfg.default_size.0)
                .u32(self.cfg.default_size.1)
                .u32(self.cfg.max_size.0)
                .u32(self.cfg.max_size.1)
                .u32(self.cfg.max_canvases)
                .finish();
            self.send(id, welcome);
            return Ok(());
        }
        if kind == u32::MAX {
            if o != op::CONTROL {
                self.error(id, status::PROTOCOL, "control clients send CONTROL");
                self.kill_conn(id);
                return Ok(());
            }
            let req = r.u32()?;
            let argc = r.u32()?;
            let mut argv = Vec::with_capacity(argc.min(4096) as usize);
            for _ in 0..argc {
                argv.push(r.str()?);
            }
            let blob = r.bytes()?;
            self.control(id, req, &argv, blob);
            return Ok(());
        }
        self.app_message(id, kind, o, &mut r)
    }

    /// The program that said HELLO: the one `gui run` started with this token, else a new
    /// record named after it.
    fn attach_app(&mut self, conn: Id, pid: u32, name: &str, token: &str) -> Id {
        if !token.is_empty() {
            if let Some(a) = self.apps.iter_mut().find(|a| a.token == token && a.conn.is_none() && a.child.is_some()) {
                a.conn = Some(conn);
                a.state = State::Running;
                log(&format!("{} connected", a.name));
                return a.id;
            }
        }
        let base = clean_name(if name.is_empty() { "app" } else { name });
        let name = self.unique_name(&base);
        let id = self.id();
        log(&format!("{name} connected (pid {pid})"));
        self.apps.push(App {
            id,
            name,
            token: String::new(),
            conn: Some(conn),
            child: None,
            pid,
            argv: Vec::new(),
            state: State::Running,
            started: Instant::now(),
            exited: None,
            log: None,
            size: None,
            canvases: Vec::new(),
            next_index: 1,
            kill_at: None,
            pong: 0,
            output: None,
        });
        id
    }

    /// `base`, or `base-2`, `base-3`… if a live program has it. An exited one gives its name up.
    fn unique_name(&mut self, base: &str) -> String {
        let live = |s: &Server, n: &str| s.apps.iter().any(|a| a.name == n && !matches!(a.state, State::Exited(_)));
        let mut name = base.to_string();
        let mut n = 2;
        while live(self, &name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.apps.retain(|a| a.name != name);
        name
    }

    fn canvas_of(&self, app: Id, handle: u32) -> Option<Id> {
        let a = &self.apps[self.app_index(app)?];
        a.canvases.iter().copied().find(|c| self.canvases.get(c).is_some_and(|c| c.handle == handle))
    }

    fn memory_in_use(&self) -> usize {
        self.canvases.values().map(|c| c.pixels.len() + c.pending_bytes).sum()
    }

    fn app_message(&mut self, conn: Id, app: Id, o: u16, r: &mut Reader) -> Result<(), proto::Malformed> {
        let Some(ai) = self.app_index(app) else { return Ok(()) };
        match o {
            op::CANVAS_CREATE => {
                let handle = r.u32()?;
                let (w, h) = (r.u32()?, r.u32()?);
                let title = r.str()?;
                if self.canvas_of(app, handle).is_some() {
                    self.error(conn, status::PROTOCOL, &format!("canvas {handle} already exists"));
                    return Ok(());
                }
                if self.apps[ai].canvases.len() as u32 >= self.cfg.max_canvases {
                    self.error(conn, status::LIMIT, &format!("at most {} canvases per program", self.cfg.max_canvases));
                    return Ok(());
                }
                let (w, h) = match self.apps[ai].size {
                    Some((w, h)) => (w, h),
                    None => self.cfg.clamp(w, h),
                };
                let id = self.id();
                let a = &mut self.apps[ai];
                let index = a.next_index;
                a.next_index += 1;
                a.canvases.push(id);
                self.canvases.insert(
                    id,
                    Canvas {
                        id,
                        app,
                        index,
                        handle,
                        title,
                        cursor: String::new(),
                        configured: (w, h),
                        size: (0, 0),
                        pixels: Vec::new(),
                        pending: Vec::new(),
                        pending_bytes: 0,
                        frames: 0,
                        last_frame: None,
                        pointer: (0, 0),
                        buttons: 0,
                        mods: 0,
                        inside: false,
                        text_input: None,
                        preedit: String::new(),
                    },
                );
                self.send(conn, Writer::new(op::CONFIGURE).u32(handle).u32(w).u32(h).finish());
                if self.focus.is_none() {
                    self.set_focus(Some(id));
                }
            }
            op::CANVAS_DESTROY => {
                let handle = r.u32()?;
                if let Some(id) = self.canvas_of(app, handle) {
                    self.apps[ai].canvases.retain(|c| *c != id);
                    self.drop_canvas(id);
                }
            }
            op::CANVAS_TITLE | op::CANVAS_CURSOR => {
                let handle = r.u32()?;
                let s = r.str()?;
                if let Some(c) = self.canvas_of(app, handle).and_then(|id| self.canvases.get_mut(&id)) {
                    if o == op::CANVAS_TITLE {
                        c.title = s.chars().take(256).collect();
                    } else {
                        c.cursor = s.chars().take(64).collect();
                    }
                }
            }
            op::CANVAS_UPDATE => {
                let handle = r.u32()?;
                let (x, y, w, h) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
                let data = r.bytes()?;
                let (mx, my) = self.cfg.max_size;
                let max_bytes = mx as usize * my as usize * 4 * 2;
                let Some(id) = self.canvas_of(app, handle) else {
                    self.error(conn, status::NOT_FOUND, &format!("no canvas {handle}"));
                    return Ok(());
                };
                let ok_rect = w > 0 && h > 0 && x.checked_add(w).is_some_and(|e| e <= mx) && y.checked_add(h).is_some_and(|e| e <= my);
                if !ok_rect || data.len() != w as usize * h as usize * 4 {
                    self.error(conn, status::PROTOCOL, &format!("bad update {w}x{h}+{x}+{y} with {} bytes", data.len()));
                    return Ok(());
                }
                let in_use = self.memory_in_use();
                let c = self.canvases.get_mut(&id).unwrap();
                if c.pending_bytes + data.len() > max_bytes || in_use + data.len() > self.cfg.max_memory {
                    c.pending.clear();
                    c.pending_bytes = 0;
                    self.error(conn, status::LIMIT, "too much pixel data before a commit");
                    return Ok(());
                }
                c.pending_bytes += data.len();
                c.pending.push(Update { x, y, w, h, data: data.to_vec() });
            }
            op::CANVAS_COMMIT => {
                let handle = r.u32()?;
                let serial = r.u32()?;
                let (w, h) = (r.u32()?, r.u32()?);
                let Some(id) = self.canvas_of(app, handle) else {
                    self.error(conn, status::NOT_FOUND, &format!("no canvas {handle}"));
                    return Ok(());
                };
                self.commit(conn, id, handle, serial, w, h);
            }
            op::CANVAS_REQUEST_SIZE => {
                let handle = r.u32()?;
                let (w, h) = (r.u32()?, r.u32()?);
                if let Some(id) = self.canvas_of(app, handle) {
                    let (w, h) = self.apps[ai].size.unwrap_or_else(|| self.cfg.clamp(w, h));
                    self.canvases.get_mut(&id).unwrap().configured = (w, h);
                    self.send(conn, Writer::new(op::CONFIGURE).u32(handle).u32(w).u32(h).finish());
                }
            }
            op::TEXT_INPUT => {
                let handle = r.u32()?;
                let enabled = r.u32()? != 0;
                let (x, y, w, h) = (r.i32()?, r.i32()?, r.u32()?, r.u32()?);
                if let Some(c) = self.canvas_of(app, handle).and_then(|id| self.canvases.get_mut(&id)) {
                    c.text_input = enabled.then_some((x, y, w, h));
                    if !enabled {
                        c.preedit.clear();
                    }
                }
            }
            op::FOCUS_REQUEST => {
                let handle = r.u32()?;
                if let Some(id) = self.canvas_of(app, handle) {
                    self.set_focus(Some(id));
                }
            }
            op::CLIPBOARD_SET => {
                let n = r.u32()?;
                let mut items = Vec::new();
                let mut total = 0usize;
                for _ in 0..n.min(64) {
                    let mime = r.str()?;
                    let data = r.bytes()?;
                    total += data.len();
                    items.push((mime, data.to_vec()));
                }
                if total > 16 << 20 {
                    self.error(conn, status::LIMIT, "clipboard data over 16 MiB");
                    return Ok(());
                }
                self.set_clipboard(items);
            }
            op::CLIPBOARD_GET => {
                let req = r.u32()?;
                let mime = r.str()?;
                let found = self.clipboard_find(&mime);
                let msg = match found {
                    Some((m, d)) => Writer::with_capacity(op::CLIPBOARD_DATA, d.len() + m.len() + 16).u32(req).u32(1).str(&m).bytes(&d).finish(),
                    None => Writer::new(op::CLIPBOARD_DATA).u32(req).u32(0).str(&mime).bytes(&[]).finish(),
                };
                self.send(conn, msg);
            }
            op::PONG => {
                let serial = r.u32()?;
                self.apps[ai].pong = serial;
                let done: Vec<(Id, u32)> = self.waiters.iter().filter(|w| matches!(w.wait, Wait::Pong(a, s) if a == app && s <= serial)).map(|w| (w.conn, w.req)).collect();
                self.waiters.retain(|w| !matches!(w.wait, Wait::Pong(a, s) if a == app && s <= serial));
                for (c, req) in done {
                    self.reply(c, req, status::OK, "", &[]);
                }
            }
            _ => {
                self.error(conn, status::PROTOCOL, &format!("unknown message 0x{o:04x}"));
            }
        }
        Ok(())
    }

    fn commit(&mut self, conn: Id, id: Id, handle: u32, serial: u32, w: u32, h: u32) {
        let (mx, my) = self.cfg.max_size;
        if w == 0 || h == 0 || w > mx || h > my {
            let c = self.canvases.get_mut(&id).unwrap();
            c.pending.clear();
            c.pending_bytes = 0;
            self.error(conn, status::LIMIT, &format!("frame {w}x{h} is outside 1x1..{mx}x{my}"));
            return;
        }
        let in_use = self.memory_in_use();
        let c = self.canvases.get_mut(&id).unwrap();
        let bytes = w as usize * h as usize * 4;
        if (w, h) != c.size {
            if in_use - c.pixels.len() + bytes > self.cfg.max_memory {
                c.pending.clear();
                c.pending_bytes = 0;
                self.error(conn, status::LIMIT, &format!("a {w}x{h} frame passes the {} MiB pixel budget", self.cfg.max_memory >> 20));
                return;
            }
            // A new size: keep what overlaps, black elsewhere, until the program draws it.
            let mut fresh = vec![0u8; bytes];
            let (ow, oh) = c.size;
            let (cw, ch) = (ow.min(w) as usize, oh.min(h) as usize);
            for y in 0..ch {
                let src = y * ow as usize * 4;
                let dst = y * w as usize * 4;
                fresh[dst..dst + cw * 4].copy_from_slice(&c.pixels[src..src + cw * 4]);
            }
            c.pixels = fresh;
            c.size = (w, h);
        }
        let mut bad = None;
        for u in std::mem::take(&mut c.pending) {
            if u.x + u.w > w || u.y + u.h > h {
                bad = Some(format!("update {}x{}+{}+{} is outside the {w}x{h} frame", u.w, u.h, u.x, u.y));
                continue;
            }
            let row = u.w as usize * 4;
            for j in 0..u.h as usize {
                let dst = ((u.y as usize + j) * w as usize + u.x as usize) * 4;
                c.pixels[dst..dst + row].copy_from_slice(&u.data[j * row..(j + 1) * row]);
            }
        }
        c.pending_bytes = 0;
        c.frames += 1;
        c.last_frame = Some(Instant::now());
        let frames = c.frames;
        let app = c.app;
        if let Some(msg) = bad {
            self.error(conn, status::PROTOCOL, &msg);
        }
        self.send(conn, Writer::new(op::FRAME_DONE).u32(handle).u32(serial).finish());
        // Waiters for this canvas or its program.
        let mut done = Vec::new();
        self.waiters.retain(|wt| {
            let hit = match wt.wait {
                Wait::Ready(a) => a == app,
                Wait::Frame(cv, after) => cv == id && frames > after,
                Wait::Size(cv, ww, hh) => cv == id && (ww, hh) == (w, h),
                _ => false,
            };
            if hit {
                done.push((wt.conn, wt.req));
            }
            !hit
        });
        for (c, req) in done {
            self.reply(c, req, status::OK, "", &[]);
        }
    }

    fn set_focus(&mut self, to: Option<Id>) {
        if self.focus == to {
            return;
        }
        let from = self.focus;
        self.focus = to;
        for (cid, on) in [(from, 0u32), (to, 1u32)] {
            let Some(cid) = cid else { continue };
            let Some(c) = self.canvases.get(&cid) else { continue };
            let (handle, app) = (c.handle, c.app);
            if let Some(conn) = self.app_index(app).and_then(|i| self.apps[i].conn) {
                self.send(conn, Writer::new(op::FOCUS).u32(handle).u32(on).finish());
            }
        }
    }

    fn set_clipboard(&mut self, items: Vec<(String, Vec<u8>)>) {
        self.clipboard = items;
        let mut w = Writer::new(op::CLIPBOARD_CHANGED).u32(self.clipboard.len() as u32);
        for (m, _) in &self.clipboard {
            w = w.str(m);
        }
        let msg = w.finish();
        let conns: Vec<Id> = self.apps.iter().filter_map(|a| a.conn).collect();
        for c in conns {
            self.send(c, msg.clone());
        }
    }

    /// The clipboard item for `mime`; "text" and "text/plain" also take any text/plain flavour.
    fn clipboard_find(&self, mime: &str) -> Option<(String, Vec<u8>)> {
        if let Some((m, d)) = self.clipboard.iter().find(|(m, _)| m == mime) {
            return Some((m.clone(), d.clone()));
        }
        if matches!(mime, "text" | "text/plain" | "UTF8_STRING" | "STRING" | "TEXT") || mime.starts_with("text/plain;") {
            return self
                .clipboard
                .iter()
                .find(|(m, _)| m.starts_with("text/plain") || matches!(m.as_str(), "UTF8_STRING" | "STRING" | "TEXT"))
                .map(|(m, d)| (m.clone(), d.clone()));
        }
        None
    }

    // ── control ─────────────────────────────────────────────────────────────────────────────

    /// A target: `NAME`, `NAME:N` (its canvas number N) or `.` (the focused canvas, else the only
    /// program). Returns the program and, if it has one, the canvas.
    fn resolve(&self, target: &str) -> Result<(Id, Option<Id>), String> {
        if target == "." || target.is_empty() {
            if let Some(f) = self.focus.and_then(|f| self.canvases.get(&f)) {
                return Ok((f.app, Some(f.id)));
            }
            let live: Vec<&App> = self.apps.iter().filter(|a| !matches!(a.state, State::Exited(_))).collect();
            return match live.as_slice() {
                [a] => Ok((a.id, self.main_canvas(a))),
                [] => Err("no program is running".into()),
                _ => Err(format!(
                    "nothing has focus and several programs run ({}); name one",
                    live.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
                )),
            };
        }
        let (name, index) = match target.rsplit_once(':') {
            Some((n, i)) => (n, Some(i.parse::<u32>().map_err(|_| format!("bad canvas number in {target}"))?)),
            None => (target, None),
        };
        let a = self.apps.iter().rev().find(|a| a.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.apps.iter().map(|a| a.name.as_str()).collect();
            if names.is_empty() {
                format!("no program named {name} (none is running)")
            } else {
                format!("no program named {name} (there are: {})", names.join(", "))
            }
        })?;
        match index {
            None => Ok((a.id, self.main_canvas(a))),
            Some(i) => {
                let c = a.canvases.iter().find(|c| self.canvases.get(c).is_some_and(|c| c.index == i));
                c.map(|c| (a.id, Some(*c))).ok_or_else(|| format!("{name} has no canvas {i}"))
            }
        }
    }

    fn main_canvas(&self, a: &App) -> Option<Id> {
        a.canvases.iter().copied().filter(|c| self.canvases.contains_key(c)).min_by_key(|c| self.canvases[c].index)
    }

    fn canvas_name(&self, c: &Canvas) -> String {
        let app = self.app_index(c.app).map(|i| self.apps[i].name.as_str()).unwrap_or("?");
        format!("{app}:{}", c.index)
    }

    fn need_canvas(&self, target: &str) -> Result<Id, (u32, String)> {
        let (app, canvas) = self.resolve(target).map_err(|e| (status::NOT_FOUND, e))?;
        canvas.ok_or_else(|| {
            let a = &self.apps[self.app_index(app).unwrap()];
            let why = match &a.state {
                State::Exited(s) => format!("{} has {s}", a.name),
                _ => format!("{} has no canvas yet (gui wait {} --ready)", a.name, a.name),
            };
            (status::NOT_FOUND, why)
        })
    }

    fn control(&mut self, conn: Id, req: u32, argv: &[String], blob: &[u8]) {
        let result = self.control_inner(conn, req, argv, blob);
        match result {
            Ok(Some((text, blob))) => self.reply(conn, req, status::OK, &text, &blob),
            Ok(None) => {} // answered later
            Err((code, msg)) => self.reply(conn, req, code, &msg, &[]),
        }
    }

    #[allow(clippy::type_complexity)]
    fn control_inner(&mut self, conn: Id, req: u32, argv: &[String], blob: &[u8]) -> Result<Option<(String, Vec<u8>)>, (u32, String)> {
        let usage = |m: &str| (status::USAGE, m.to_string());
        let arg = |i: usize| argv.get(i).map(String::as_str).unwrap_or("");
        let num = |i: usize| -> Result<i64, (u32, String)> { arg(i).parse::<i64>().map_err(|_| (status::USAGE, format!("not a number: {:?}", arg(i)))) };
        let ok = |s: String| Ok(Some((s, Vec::new())));
        match arg(0) {
            "status" => ok(self.status_text()),
            "list" => ok(if arg(1) == "json" { self.list_json() } else { self.list_text() }),
            "info" => {
                let (app, canvas) = self.resolve(arg(1)).map_err(|e| (status::NOT_FOUND, e))?;
                ok(self.info_json(app, canvas))
            }
            "launch" => self.launch(argv, blob).map(|s| Some((s, Vec::new()))),
            "capture" => {
                if !self.cfg.capture {
                    return Err((status::DENIED, "reading the screen is turned off (gui setting capture=false)".into()));
                }
                let id = self.need_canvas(arg(1))?;
                let c = &self.canvases[&id];
                if c.frames == 0 {
                    return Err((status::NOT_FOUND, format!("{} has drawn nothing yet", self.canvas_name(c))));
                }
                let (w, h) = c.size;
                let (x, y, rw, rh) = if argv.len() >= 6 {
                    let (x, y, rw, rh) = (num(2)?, num(3)?, num(4)?, num(5)?);
                    let x0 = x.clamp(0, w as i64);
                    let y0 = y.clamp(0, h as i64);
                    let x1 = (x + rw).clamp(0, w as i64);
                    let y1 = (y + rh).clamp(0, h as i64);
                    if x1 <= x0 || y1 <= y0 {
                        return Err(usage(&format!("the region is outside the {w}x{h} canvas")));
                    }
                    (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32)
                } else {
                    (0, 0, w, h)
                };
                let mut out = Vec::with_capacity(rw as usize * rh as usize * 4);
                for j in y..y + rh {
                    let s = ((j * w + x) * 4) as usize;
                    out.extend_from_slice(&c.pixels[s..s + rw as usize * 4]);
                }
                Ok(Some((format!("{rw} {rh} {} {} {w} {h}", c.frames, self.canvas_name(c)), out)))
            }
            "input" => {
                if !self.cfg.input {
                    return Err((status::DENIED, "sending input is turned off (gui setting input=false)".into()));
                }
                let id = self.need_canvas(arg(1))?;
                self.input(id, &argv[2..]).map(|s| Some((s, Vec::new())))
            }
            "focus" => {
                let id = self.need_canvas(arg(1))?;
                self.set_focus(Some(id));
                ok(String::new())
            }
            "unfocus" => {
                self.set_focus(None);
                ok(String::new())
            }
            "resize" => {
                let id = self.need_canvas(arg(1))?;
                let (w, h) = (num(2)?.clamp(0, u32::MAX as i64) as u32, num(3)?.clamp(0, u32::MAX as i64) as u32);
                if w == 0 || h == 0 {
                    return Err(usage("resize needs a width and a height"));
                }
                let (w, h) = self.cfg.clamp(w, h);
                let c = self.canvases.get_mut(&id).unwrap();
                c.configured = (w, h);
                let (handle, app) = (c.handle, c.app);
                let ai = self.app_index(app).unwrap();
                if self.apps[ai].size.is_some() {
                    self.apps[ai].size = Some((w, h));
                }
                let conn_app = self.apps[ai].conn.ok_or((status::FAILED, "the program is not connected".to_string()))?;
                self.send(conn_app, Writer::new(op::CONFIGURE).u32(handle).u32(w).u32(h).finish());
                ok(format!("{w}x{h}"))
            }
            "close" => {
                let (app, canvas) = self.resolve(arg(1)).map_err(|e| (status::NOT_FOUND, e))?;
                let ai = self.app_index(app).unwrap();
                let conn_app = self.apps[ai].conn.ok_or((status::FAILED, format!("{} is not connected", self.apps[ai].name)))?;
                // NAME closes every canvas of the program, NAME:N just that one.
                let targets: Vec<u32> = if arg(1).contains(':') {
                    canvas.iter().map(|c| self.canvases[c].handle).collect()
                } else {
                    self.apps[ai].canvases.iter().filter_map(|c| self.canvases.get(c)).map(|c| c.handle).collect()
                };
                for h in targets {
                    self.send(conn_app, Writer::new(op::CLOSE).u32(h).finish());
                }
                ok(String::new())
            }
            "kill" => {
                let (app, _) = self.resolve(arg(1)).map_err(|e| (status::NOT_FOUND, e))?;
                let signal = sys::signal_number(if arg(2).is_empty() { "TERM" } else { arg(2) }).ok_or_else(|| usage("unknown signal"))?;
                let grace = Duration::from_millis(num(3).unwrap_or(3000).clamp(0, 600_000) as u64);
                let ai = self.app_index(app).unwrap();
                let a = &mut self.apps[ai];
                if let State::Exited(s) = &a.state {
                    let text = format!("{} had already {s}; removed from the list", a.name);
                    let name = a.name.clone();
                    self.apps.retain(|a| a.name != name || !matches!(a.state, State::Exited(_)));
                    return ok(text);
                }
                // Signals that do not end a program (STOP, CONT, USR1...) are sent and done.
                let ending = matches!(signal, libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGQUIT | libc::SIGKILL);
                match &a.child {
                    Some(child) => {
                        sys::kill_group(child.id(), signal);
                        if signal != libc::SIGKILL && ending && !grace.is_zero() {
                            a.kill_at = Some(Instant::now() + grace);
                        }
                    }
                    None if a.pid > 1 => {
                        // SAFETY: a plain kill(2) of the pid the program gave.
                        unsafe { libc::kill(a.pid as libc::pid_t, signal) };
                    }
                    None => return Err((status::FAILED, format!("{} gave no pid; try gui close", a.name))),
                }
                if a.child.is_none() || !ending {
                    return ok(String::new());
                }
                self.waiters.push(Waiter { conn, req, deadline: Instant::now() + grace + Duration::from_secs(3), wait: Wait::Exit(app) });
                Ok(None)
            }
            "clipboard" => {
                if !self.cfg.clipboard {
                    return Err((status::DENIED, "the clipboard is turned off (gui setting clipboard=false)".into()));
                }
                match arg(1) {
                    "types" => ok(self.clipboard.iter().map(|(m, d)| format!("{m}\t{}\n", d.len())).collect()),
                    "get" => {
                        let mime = if arg(2).is_empty() { "text/plain" } else { arg(2) };
                        match self.clipboard_find(mime) {
                            Some((m, d)) => Ok(Some((m, d))),
                            None if self.clipboard.is_empty() => Err((status::NOT_FOUND, "the clipboard is empty".into())),
                            None => Err((status::NOT_FOUND, format!("nothing of type {mime} on the clipboard (gui clipboard types)"))),
                        }
                    }
                    "set" => {
                        if blob.len() > 16 << 20 {
                            return Err((status::LIMIT, "clipboard data over 16 MiB".into()));
                        }
                        let mime = if arg(2).is_empty() { "text/plain;charset=utf-8" } else { arg(2) };
                        let mut items = vec![(mime.to_string(), blob.to_vec())];
                        if mime.starts_with("text/plain") {
                            items.push(("UTF8_STRING".into(), blob.to_vec()));
                        }
                        self.set_clipboard(items);
                        ok(String::new())
                    }
                    "clear" => {
                        self.set_clipboard(Vec::new());
                        ok(String::new())
                    }
                    _ => Err(usage("clipboard types|get|set|clear")),
                }
            }
            "wait" => {
                let timeout = Duration::from_millis(num(argv.len() - 1)?.clamp(0, 3_600_000) as u64);
                let deadline = Instant::now() + timeout;
                let (app, canvas) = self.resolve(arg(1)).map_err(|e| (status::NOT_FOUND, e))?;
                let ai = self.app_index(app).unwrap();
                let wait = match arg(2) {
                    "ready" => {
                        if self.apps[ai].canvases.iter().any(|c| self.canvases.get(c).is_some_and(|c| c.frames > 0)) {
                            return ok(String::new());
                        }
                        if let State::Exited(s) = &self.apps[ai].state {
                            return Err((status::FAILED, format!("{} has {s}", self.apps[ai].name)));
                        }
                        Wait::Ready(app)
                    }
                    "exit" => {
                        if let State::Exited(s) = &self.apps[ai].state {
                            return ok(s.clone());
                        }
                        Wait::Exit(app)
                    }
                    "frame" => {
                        let id = self.need_canvas(arg(1))?;
                        Wait::Frame(id, self.canvases[&id].frames)
                    }
                    "idle" => {
                        let id = self.need_canvas(arg(1))?;
                        Wait::Idle(id, Duration::from_millis(num(3)?.clamp(0, 600_000) as u64))
                    }
                    "size" => {
                        let id = canvas.ok_or_else(|| (status::NOT_FOUND, "no canvas".to_string()))?;
                        let (w, h) = (num(3)? as u32, num(4)? as u32);
                        if self.canvases[&id].size == (w, h) && self.canvases[&id].frames > 0 {
                            return ok(String::new());
                        }
                        Wait::Size(id, w, h)
                    }
                    "sync" => {
                        let Some(conn_app) = self.apps[ai].conn else {
                            return Err((status::FAILED, format!("{} is not connected", self.apps[ai].name)));
                        };
                        self.ping_serial = self.ping_serial.wrapping_add(1).max(1);
                        let serial = self.ping_serial;
                        self.send(conn_app, Writer::new(op::PING).u32(serial).finish());
                        Wait::Pong(app, serial)
                    }
                    other => return Err(usage(&format!("unknown wait: {other}"))),
                };
                self.waiters.push(Waiter { conn, req, deadline, wait });
                Ok(None)
            }
            "shutdown" => {
                for a in &mut self.apps {
                    if let Some(c) = &a.child {
                        sys::kill_group(c.id(), libc::SIGTERM);
                    }
                }
                self.stopping = Some(Instant::now() + Duration::from_secs(2));
                ok("stopping".into())
            }
            "" => Err(usage("empty request")),
            other => Err(usage(&format!("unknown request: {other}"))),
        }
    }

    /// `launch name=N size=WxH cwd=D env=K=V... -- PROGRAM ARGS...` (PROGRAM an absolute path).
    fn launch(&mut self, argv: &[String], _blob: &[u8]) -> Result<String, (u32, String)> {
        let sep = argv.iter().position(|a| a == "--").ok_or((status::USAGE, "launch: no -- before the command".to_string()))?;
        let cmd = &argv[sep + 1..];
        if cmd.is_empty() {
            return Err((status::USAGE, "launch: no command".into()));
        }
        let (mut name, mut size, mut cwd, mut env) = (None, None, None, Vec::new());
        for opt in &argv[1..sep] {
            let (k, v) = opt.split_once('=').unwrap_or((opt, ""));
            match k {
                "name" => name = Some(v.to_string()),
                "size" => size = Some(config::parse_size(v).ok_or((status::USAGE, format!("bad size {v}")))?),
                "cwd" => cwd = Some(v.to_string()),
                "env" => {
                    let (ek, ev) = v.split_once('=').unwrap_or((v, ""));
                    env.push((ek.to_string(), ev.to_string()));
                }
                _ => return Err((status::USAGE, format!("launch: unknown option {k}"))),
            }
        }
        let name = match name {
            Some(n) => {
                let clean = clean_name(&n);
                if clean != n {
                    return Err((status::USAGE, format!("a name is letters, digits and . _ - and starts with a letter (try {clean})")));
                }
                if self.apps.iter().any(|a| a.name == n && !matches!(a.state, State::Exited(_))) {
                    return Err((status::FAILED, format!("{n} is already running")));
                }
                self.apps.retain(|a| a.name != n);
                n
            }
            None => self.unique_name(&clean_name(&cmd[0])),
        };
        let size = size.map(|(w, h)| self.cfg.clamp(w, h));
        let (sw, sh) = size.unwrap_or(self.cfg.default_size);
        let log_path = self.dir.join("logs").join(format!("{name}.log"));
        let log_file = File::create(&log_path).map_err(|e| (status::FAILED, format!("{}: {e}", log_path.display())))?;
        let _ = std::fs::remove_file(log_path.with_extension("log.1"));
        let (pipe, out, err) = sys::output_pipe().map_err(|e| (status::FAILED, format!("a pipe for the output: {e}")))?;
        let id = self.id();
        let token = format!("{}-{id}-{}", std::process::id(), self.started.elapsed().as_nanos() % 1_000_000_007);
        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("COLLABO_GUI_SOCKET", &self.socket)
            .env("COLLABO_GUI_APP", &token)
            .env("COLLABO_GUI_NAME", &name)
            .env("COLLABO_GUI_SIZE", format!("{sw}x{sh}"))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        if let Some(d) = &cwd {
            command.current_dir(d);
        }
        let child = command.spawn().map_err(|e| (status::FAILED, format!("{}: {e}", cmd[0])))?;
        // Our copies of the write end go, so the pipe ends when the program's do.
        drop(command);
        let pid = child.id();
        log(&format!("started {name} (pid {pid}): {}", cmd.join(" ")));
        self.apps.push(App {
            id,
            name: name.clone(),
            token,
            conn: None,
            child: Some(child),
            pid,
            argv: cmd.to_vec(),
            state: State::Starting,
            started: Instant::now(),
            exited: None,
            log: Some(log_path.clone()),
            size,
            canvases: Vec::new(),
            next_index: 1,
            kill_at: None,
            pong: 0,
            output: Some(Output { pipe, log: log_file, size: 0 }),
        });
        Ok(format!("{name} {pid} {}", log_path.display()))
    }

    /// Input events in order: `motion X Y`, `button N down|up`, `wheel DX DY SX SY`,
    /// `key KEYSYM KEYCODE down|up TEXT`, `text STRING`, `preedit STRING BEGIN END`, `leave`,
    /// `release` (every held button and modifier up), and `nofocus` first to leave focus be.
    /// The pointer enters the canvas (POINTER_ENTER) with the first pointer event after a leave.
    fn input(&mut self, id: Id, ev: &[String]) -> Result<String, (u32, String)> {
        let usage = |m: String| (status::USAGE, m);
        let c = &self.canvases[&id];
        let ai = self.app_index(c.app).unwrap();
        let conn = self.apps[ai].conn.ok_or((status::FAILED, format!("{} is not connected", self.apps[ai].name)))?;
        let handle = c.handle;
        let (w, h) = if c.frames > 0 { c.size } else { c.configured };
        let mut i = 0;
        let mut focus = true;
        if ev.first().map(String::as_str) == Some("nofocus") {
            focus = false;
            i = 1;
        }
        if focus {
            self.set_focus(Some(id));
        }
        let num = |s: Option<&String>| -> Result<i64, (u32, String)> {
            s.and_then(|s| s.parse::<i64>().ok()).ok_or_else(|| (status::USAGE, format!("input: not a number: {s:?}")))
        };
        let mut msgs = Vec::new();
        let c = self.canvases.get_mut(&id).unwrap();
        let enter = |c: &mut Canvas, msgs: &mut Vec<Vec<u8>>| {
            if !c.inside {
                c.inside = true;
                msgs.push(Writer::new(op::POINTER_ENTER).u32(handle).i32(c.pointer.0).i32(c.pointer.1).u32(c.mods).u32(c.buttons).finish());
            }
        };
        while i < ev.len() {
            match ev[i].as_str() {
                "motion" => {
                    let (x, y) = (num(ev.get(i + 1))?, num(ev.get(i + 2))?);
                    if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                        return Err(usage(format!("{x},{y} is outside the {w}x{h} canvas")));
                    }
                    c.pointer = (x as i32, y as i32);
                    enter(c, &mut msgs);
                    msgs.push(Writer::new(op::POINTER_MOTION).u32(handle).i32(x as i32).i32(y as i32).u32(c.mods).u32(c.buttons).finish());
                    i += 3;
                }
                "button" => {
                    let b = num(ev.get(i + 1))?.clamp(1, 32) as u32;
                    let down = match ev.get(i + 2).map(String::as_str) {
                        Some("down") => true,
                        Some("up") => false,
                        other => return Err(usage(format!("input: button {b} {other:?}: down or up"))),
                    };
                    let mask = if b <= 5 { modifier::BUTTON1 << (b - 1) } else { 0 };
                    enter(c, &mut msgs);
                    let before = c.buttons;
                    if down {
                        c.buttons |= mask;
                    } else {
                        c.buttons &= !mask;
                    }
                    let (x, y) = c.pointer;
                    msgs.push(Writer::new(op::POINTER_BUTTON).u32(handle).i32(x).i32(y).u32(b).u32(down as u32).u32(c.mods).u32(before).finish());
                    i += 3;
                }
                "wheel" => {
                    let (dx, dy, sx, sy) = (num(ev.get(i + 1))?, num(ev.get(i + 2))?, num(ev.get(i + 3))?, num(ev.get(i + 4))?);
                    enter(c, &mut msgs);
                    let (x, y) = c.pointer;
                    msgs.push(
                        Writer::new(op::SCROLL).u32(handle).i32(x).i32(y).i32(dx as i32).i32(dy as i32).i32(sx as i32).i32(sy as i32).u32(c.mods).finish(),
                    );
                    i += 5;
                }
                "key" => {
                    let keysym = num(ev.get(i + 1))? as u32;
                    let keycode = num(ev.get(i + 2))? as u32;
                    let down = match ev.get(i + 3).map(String::as_str) {
                        Some("down") => true,
                        Some("up") => false,
                        other => return Err(usage(format!("input: key {other:?}: down or up"))),
                    };
                    let text = ev.get(i + 4).cloned().unwrap_or_default();
                    // State before the key, as X11 reports it; then the key's own effect.
                    msgs.push(Writer::new(op::KEY).u32(handle).u32(keysym).u32(keycode).u32(down as u32).u32(c.mods).str(&text).finish());
                    let bit = match keysym {
                        0xffe1 | 0xffe2 => modifier::SHIFT,
                        0xffe3 | 0xffe4 => modifier::CONTROL,
                        0xffe9 | 0xffea | 0xffe7 | 0xffe8 => modifier::ALT,
                        0xffeb | 0xffec => modifier::SUPER,
                        _ => 0,
                    };
                    if keysym == 0xffe5 && down {
                        c.mods ^= modifier::CAPS_LOCK;
                    } else if down {
                        c.mods |= bit;
                    } else {
                        c.mods &= !bit;
                    }
                    i += 5;
                }
                "text" => {
                    let text = ev.get(i + 1).cloned().unwrap_or_default();
                    msgs.push(Writer::new(op::TEXT).u32(handle).str(&text).finish());
                    c.preedit.clear();
                    i += 2;
                }
                "preedit" => {
                    let text = ev.get(i + 1).cloned().unwrap_or_default();
                    let (b, e) = (num(ev.get(i + 2))?, num(ev.get(i + 3))?);
                    let fits = |v: i64| v == -1 || (0..=text.len() as i64).contains(&v) && text.is_char_boundary(v as usize);
                    if !fits(b) || !fits(e) {
                        return Err(usage(format!("preedit: cursor {b},{e} is not a character boundary of {text:?}")));
                    }
                    msgs.push(Writer::new(op::PREEDIT).u32(handle).str(&text).i32(b as i32).i32(e as i32).finish());
                    c.preedit = text;
                    i += 4;
                }
                "leave" => {
                    if c.inside {
                        c.inside = false;
                        msgs.push(Writer::new(op::POINTER_LEAVE).u32(handle).finish());
                    }
                    i += 1;
                }
                "release" => {
                    for b in 1..=5u32 {
                        let mask = modifier::BUTTON1 << (b - 1);
                        if c.buttons & mask != 0 {
                            let before = c.buttons;
                            c.buttons &= !mask;
                            let (x, y) = c.pointer;
                            msgs.push(Writer::new(op::POINTER_BUTTON).u32(handle).i32(x).i32(y).u32(b).u32(0).u32(c.mods).u32(before).finish());
                        }
                    }
                    for (keysym, keycode) in crate::keys::mods_keys(c.mods) {
                        msgs.push(Writer::new(op::KEY).u32(handle).u32(keysym).u32(keycode).u32(0).u32(c.mods).str("").finish());
                    }
                    c.mods &= modifier::CAPS_LOCK;
                    i += 1;
                }
                other => return Err(usage(format!("input: unknown event {other}"))),
            }
        }
        let (px, py) = c.pointer;
        for m in msgs {
            self.send(conn, m);
        }
        Ok(format!("{px} {py}"))
    }

    // ── reports ─────────────────────────────────────────────────────────────────────────────

    fn state_text(a: &App) -> String {
        match &a.state {
            State::Starting => "starting".into(),
            State::Running if a.conn.is_none() => "running, not connected".into(),
            State::Running => "running".into(),
            State::Exited(s) => s.clone(),
        }
    }

    fn status_text(&self) -> String {
        let live = self.apps.iter().filter(|a| !matches!(a.state, State::Exited(_))).count();
        let bytes = self.memory_in_use();
        let yn = |b: bool| if b { "allowed" } else { "denied" };
        format!(
            "server: pid {}, up {}s, socket {}\nlimits: default {}x{}, max {}x{}, {} canvases per program, {} MiB of pixels\naccess: capture {}, input {}, clipboard {}\nnow: {} program(s), {} canvas(es), {:.1} MiB of pixels{}\n",
            std::process::id(),
            self.started.elapsed().as_secs(),
            self.socket.display(),
            self.cfg.default_size.0,
            self.cfg.default_size.1,
            self.cfg.max_size.0,
            self.cfg.max_size.1,
            self.cfg.max_canvases,
            self.cfg.max_memory >> 20,
            yn(self.cfg.capture),
            yn(self.cfg.input),
            yn(self.cfg.clipboard),
            live,
            self.canvases.len(),
            bytes as f64 / (1 << 20) as f64,
            self.focus.and_then(|f| self.canvases.get(&f)).map(|c| format!(", focus {}", self.canvas_name(c))).unwrap_or_default(),
        )
    }

    fn list_text(&self) -> String {
        let mut rows = vec![["NAME".to_string(), "PID".into(), "STATE".into(), "SIZE".into(), "FRAMES".into(), "IDLE".into(), "TITLE".into()]];
        let now = Instant::now();
        for a in &self.apps {
            let mut cs: Vec<&Canvas> = a.canvases.iter().filter_map(|c| self.canvases.get(c)).collect();
            cs.sort_by_key(|c| c.index);
            let pid = if a.pid > 0 { a.pid.to_string() } else { "-".into() };
            if cs.is_empty() {
                rows.push([a.name.clone(), pid.clone(), Self::state_text(a), "-".into(), "-".into(), "-".into(), String::new()]);
            }
            for c in cs {
                let size = if c.frames > 0 { format!("{}x{}", c.size.0, c.size.1) } else { format!("({}x{})", c.configured.0, c.configured.1) };
                let size = if c.frames > 0 && c.configured != c.size { format!("{size}→{}x{}", c.configured.0, c.configured.1) } else { size };
                let idle = c.last_frame.map(|t| format!("{:.1}s", (now - t).as_secs_f64())).unwrap_or("-".into());
                let focus = if self.focus == Some(c.id) { "* " } else { "" };
                rows.push([format!("{}:{}", a.name, c.index), pid.clone(), Self::state_text(a), size, c.frames.to_string(), idle, format!("{focus}{}", c.title)]);
            }
        }
        if rows.len() == 1 {
            return "no programs\n".into();
        }
        let mut widths = [0usize; 7];
        for r in &rows {
            for (i, cell) in r.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        for r in &rows {
            let mut line = String::new();
            for (i, cell) in r.iter().enumerate() {
                if i == 6 {
                    line.push_str(cell);
                } else {
                    let pad = widths[i] - cell.chars().count();
                    if i == 1 || i == 4 {
                        line.push_str(&" ".repeat(pad));
                        line.push_str(cell);
                    } else {
                        line.push_str(cell);
                        line.push_str(&" ".repeat(pad));
                    }
                    line.push_str("  ");
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    fn canvas_json(&self, c: &Canvas) -> String {
        let idle = c.last_frame.map(|t| t.elapsed().as_millis().to_string()).unwrap_or("null".into());
        format!(
            "{{\"id\":{},\"index\":{},\"title\":{},\"width\":{},\"height\":{},\"configuredWidth\":{},\"configuredHeight\":{},\"frames\":{},\"idleMs\":{},\"focused\":{},\"cursor\":{},\"pointer\":[{},{}],\"pointerInside\":{},\"buttons\":{},\"modifiers\":{},\"textInput\":{},\"preedit\":{}}}",
            json_str(&self.canvas_name(c)),
            c.index,
            json_str(&c.title),
            c.size.0,
            c.size.1,
            c.configured.0,
            c.configured.1,
            c.frames,
            idle,
            self.focus == Some(c.id),
            json_str(&c.cursor),
            c.pointer.0,
            c.pointer.1,
            c.inside,
            c.buttons,
            c.mods,
            match c.text_input {
                Some((x, y, w, h)) => format!("{{\"caret\":[{x},{y},{w},{h}]}}"),
                None => "null".into(),
            },
            json_str(&c.preedit)
        )
    }

    fn app_json(&self, a: &App) -> String {
        let mut cs: Vec<&Canvas> = a.canvases.iter().filter_map(|c| self.canvases.get(c)).collect();
        cs.sort_by_key(|c| c.index);
        format!(
            "{{\"name\":{},\"pid\":{},\"state\":{},\"connected\":{},\"command\":[{}],\"log\":{},\"uptimeMs\":{},\"canvases\":[{}]}}",
            json_str(&a.name),
            a.pid,
            json_str(&Self::state_text(a)),
            a.conn.is_some(),
            a.argv.iter().map(|s| json_str(s)).collect::<Vec<_>>().join(","),
            a.log.as_ref().map(|p| json_str(&p.display().to_string())).unwrap_or("null".into()),
            a.started.elapsed().as_millis(),
            cs.iter().map(|c| self.canvas_json(c)).collect::<Vec<_>>().join(",")
        )
    }

    fn list_json(&self) -> String {
        let focus = self.focus.and_then(|f| self.canvases.get(&f)).map(|c| json_str(&self.canvas_name(c))).unwrap_or("null".into());
        format!("{{\"apps\":[{}],\"focus\":{focus}}}\n", self.apps.iter().map(|a| self.app_json(a)).collect::<Vec<_>>().join(","))
    }

    fn info_json(&self, app: Id, canvas: Option<Id>) -> String {
        let a = &self.apps[self.app_index(app).unwrap()];
        let mut s = self.app_json(a);
        if let Some(c) = canvas.and_then(|c| self.canvases.get(&c)) {
            s.pop();
            s.push_str(&format!(",\"canvas\":{}}}", self.canvas_json(c)));
        }
        s.push('\n');
        s
    }
}

/// `gui server`: serve until `gui shutdown`. With `detach`, the caller has already pointed our
/// output at the server log; we just leave the caller's session.
pub fn main(detach: bool) -> i32 {
    if detach && !sys::setsid() {
        log(&format!("setsid: {}", io::Error::last_os_error()));
    }
    let (cfg, warnings) = Config::load();
    for w in warnings {
        log(&w);
    }
    match Server::bind(cfg) {
        Ok(Some(s)) => s.run(),
        Ok(None) => {
            log("another server is running");
            0
        }
        Err(e) => {
            log(&format!("cannot start in {}: {e}", config::runtime_dir().display()));
            1
        }
    }
}

pub fn log_dir() -> PathBuf {
    config::runtime_dir().join("logs")
}

pub fn server_log() -> PathBuf {
    config::runtime_dir().join("server.log")
}
