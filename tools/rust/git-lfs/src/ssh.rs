//! SSH (ssh/ssh.go, connection.go, protocol.go): the ssh command line for git-lfs-authenticate
//! and git-lfs-transfer (GIT_SSH, GIT_SSH_COMMAND, core.sshcommand, ssh variants, control
//! master multiplexing), and the pure SSH protocol's pkt-line connections.

use crate::config::cfg;
use crate::endpoint::SshMetadata;
use crate::errors::{Error, Result};
use crate::pktline::Pktline;
use std::io::Read;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex, RwLock};

#[derive(PartialEq, Clone, Copy)]
enum Variant {
    Ssh,
    Simple,
    Putty,
    Tortoise,
}

/// tools.QuotedFields: words, or 'quoted' / "quoted" runs.
pub fn quoted_fields(s: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"'(.*)'|"(.*)"|(\S*)"#).unwrap();
    let mut out = vec![];
    for m in re.captures_iter(s) {
        if m.get(0).unwrap().as_str().is_empty() {
            continue;
        }
        let v = (1..=3).filter_map(|i| m.get(i)).map(|x| x.as_str()).find(|x| !x.is_empty()).unwrap_or("");
        out.push(v.to_string());
    }
    out
}

fn parse_shell_command(command: &str, existing: &str) -> (String, String, bool) {
    let f = quoted_fields(command);
    if !f.is_empty() {
        return (f[0].clone(), command.to_string(), true);
    }
    (existing.to_string(), String::new(), false)
}

fn find_variant(v: &str) -> (bool, Variant) {
    match v {
        "ssh" => (false, Variant::Ssh),
        "simple" => (false, Variant::Simple),
        "putty" | "plink" => (false, Variant::Putty),
        "tortoiseplink" => (false, Variant::Tortoise),
        "auto" => (true, Variant::Ssh),
        _ => (false, Variant::Ssh),
    }
}

fn get_variant(basessh: &str) -> Variant {
    let v = cfg().os.get("GIT_SSH_VARIANT").or_else(|| cfg().git().get("ssh.variant"));
    if let Some(v) = &v {
        let (auto, val) = find_variant(v);
        if !auto {
            return val;
        }
    }
    if basessh != "ssh" {
        let base = match basessh.rfind('.') {
            Some(i) if i > 0 => &basessh[..i],
            _ => basessh,
        };
        if base.eq_ignore_ascii_case("plink") {
            return Variant::Putty;
        }
        if base.eq_ignore_ascii_case("tortoiseplink") {
            return Variant::Tortoise;
        }
    }
    Variant::Ssh
}

/// GetExeAndArgs: (command, base arguments, needs a shell, multiplexing, control path).
fn exe_and_args(meta: &SshMetadata, multiplex_desired: bool, control_path_in: &str) -> (String, Vec<String>, bool, bool, String) {
    let os = &cfg().os;
    let ssh = os.get("GIT_SSH").unwrap_or_default();
    let ssh_cmd = os.get("GIT_SSH_COMMAND").unwrap_or_default();
    let (mut ssh, mut cmd, mut need_shell) = parse_shell_command(&ssh_cmd, &ssh);
    if ssh.is_empty() {
        let c = cfg().git().get("core.sshcommand").unwrap_or_default();
        (ssh, cmd, need_shell) = parse_shell_command(&c, "ssh");
    }
    if cmd.is_empty() {
        cmd = ssh.clone();
    }
    let basessh = std::path::Path::new(&ssh).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let variant = get_variant(&basessh);
    let mut args = vec![];
    if variant == Variant::Tortoise {
        args.push("-batch".to_string());
    }
    let mut multiplexing = false;
    let mut control_path = String::new();
    if variant == Variant::Ssh && multiplex_desired && cfg().git().bool("lfs.ssh.automultiplex", true) {
        let mut master = "-oControlMaster=no".to_string();
        control_path = control_path_in.to_string();
        if control_path_in.is_empty() {
            master = "-oControlMaster=yes".into();
            let base = os.get("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).unwrap_or_else(|| std::env::temp_dir().display().to_string());
            if let Some(d) = make_temp_dir(&base, "sock-") {
                control_path = format!("{d}/lfs.sock");
            }
        }
        if !control_path.is_empty() {
            multiplexing = true;
            args.push(master);
            args.push(format!("-oControlPath={control_path}"));
        }
    }
    if !meta.port.is_empty() {
        args.push(if matches!(variant, Variant::Putty | Variant::Tortoise) { "-P" } else { "-p" }.into());
        args.push(meta.port.clone());
    }
    if meta.user_and_host.starts_with('-') {
        if variant == Variant::Ssh {
            args.push("--".into());
            args.push(meta.user_and_host.clone());
        } else {
            args.push(meta.user_and_host.trim_start_matches('-').to_string());
        }
    } else {
        args.push(meta.user_and_host.clone());
    }
    let _ = Variant::Simple;
    (cmd, args, need_shell, multiplexing, control_path)
}

fn make_temp_dir(base: &str, prefix: &str) -> Option<String> {
    for i in 0..100u32 {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() ^ (std::process::id() << 8) ^ i;
        let d = format!("{base}/{prefix}{n}");
        if std::fs::create_dir(&d).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
            return Some(d);
        }
    }
    None
}

/// GetLFSExeAndArgs: ((exe, args), multiplexing, control path), traced as run_command.
pub fn lfs_exe_and_args(meta: &SshMetadata, command: &str, operation: &str, multiplex_desired: bool, control_path: &str) -> ((String, Vec<String>), bool, String) {
    let (exe, mut args, need_shell, multiplexing, cp) = exe_and_args(meta, multiplex_desired, control_path);
    args.push(format!("{} {} {}", command, meta.path, operation));
    let (exe, args) = if need_shell {
        let quoted: Vec<String> = args.iter().map(|a| crate::subprocess::shell_quote_single(a)).collect();
        ("sh".to_string(), vec!["-c".to_string(), format!("{} {}", exe, quoted.join(" "))])
    } else {
        (exe, args)
    };
    crate::trace!("run_command: {} {}", exe, args.join(" "));
    ((exe, args), multiplexing, cp)
}

// The pure SSH protocol (git-lfs-transfer).

pub struct Connection {
    pub pl: Pktline<ChildStdout, ChildStdin>,
    child: Child,
    id: usize,
    trace_packets: bool,
}

pub fn status_line(s: &str) -> Option<i32> {
    s.strip_prefix("status ").and_then(|x| x.parse().ok())
}

impl Connection {
    fn trace_in(&self, s: &str, len: usize) {
        if self.trace_packets {
            if len <= 1 {
                crate::trace!("packet {:02x} < {:04x}", self.id, len);
            } else {
                crate::trace!("packet {:02x} < {}", self.id, s);
            }
        }
    }
    fn read_text(&mut self) -> std::io::Result<(String, usize)> {
        let r = self.pl.read_packet_text()?;
        self.trace_in(&r.0, r.1);
        Ok(r)
    }
    fn write_text(&mut self, s: &str) -> std::io::Result<()> {
        if self.trace_packets {
            crate::trace!("packet {:02x} > {}", self.id, s);
        }
        self.pl.write_packet(format!("{s}\n").as_bytes())
    }
    fn write_delim(&mut self) -> std::io::Result<()> {
        if self.trace_packets {
            crate::trace!("packet {:02x} > 0001", self.id);
        }
        use std::io::Write;
        self.pl.w.write_all(b"0001")
    }
    fn write_flush(&mut self) -> std::io::Result<()> {
        if self.trace_packets {
            crate::trace!("packet {:02x} > 0000", self.id);
        }
        self.pl.write_flush()
    }

    fn negotiate_version(&mut self) -> Result<()> {
        let mut caps = vec![];
        loop {
            let (s, l) = self.read_text().map_err(|e| Error::protocol("Unable to negotiate version with remote side (unable to read capabilities)", Some(e.into())))?;
            if l == 0 {
                break;
            }
            caps.push(s);
        }
        if !caps.iter().any(|c| c == "version=1") {
            return Err(Error::protocol("Unable to negotiate version with remote side (missing version=1)", None));
        }
        self.send_message("version 1", &[]).map_err(|e| Error::protocol("Unable to negotiate version with remote side (unable to send version)", Some(e)))?;
        let (status, args, _) = self.read_status_with_lines().map_err(|e| Error::protocol("Unable to negotiate version with remote side (unable to read status)", Some(e)))?;
        if status != 200 {
            let text = match args.first() {
                Some(a) => format!("server said: {}", crate::tools::quote(a)),
                None => "no error provided".into(),
            };
            return Err(Error::protocol(&format!("Unable to negotiate version with remote side (unexpected status {status}; {text})"), None));
        }
        Ok(())
    }

    pub fn send_message(&mut self, command: &str, args: &[String]) -> Result<()> {
        self.write_text(command)?;
        for a in args {
            self.write_text(a)?;
        }
        Ok(self.write_flush()?)
    }

    pub fn send_message_with_lines(&mut self, command: &str, args: &[String], lines: &[String]) -> Result<()> {
        self.write_text(command)?;
        for a in args {
            self.write_text(a)?;
        }
        self.write_delim()?;
        for l in lines {
            self.write_text(l)?;
        }
        Ok(self.write_flush()?)
    }

    pub fn send_message_with_data(&mut self, command: &str, args: &[String], data: &mut dyn Read) -> Result<()> {
        self.write_text(command)?;
        for a in args {
            self.write_text(a)?;
        }
        self.write_delim()?;
        let mut buf = vec![0u8; 32768];
        loop {
            match data.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.pl.write_packet(&buf[..n])?,
            }
        }
        Ok(self.write_flush()?)
    }

    fn status_of(s: &str) -> Result<i32> {
        status_line(s).ok_or_else(|| Error::protocol(&format!("expected status line, got {}", crate::tools::quote(s)), None))
    }

    pub fn read_status(&mut self) -> Result<i32> {
        let mut status = None;
        loop {
            let (s, l) = self.read_text().map_err(|e| Error::protocol("error reading packet", Some(e.into())))?;
            match (l, status) {
                (0, None) => return Err(Error::protocol("no status seen", None)),
                (0, Some(st)) => return Ok(st),
                (_, None) => status = Some(Self::status_of(&s)?),
                _ => return Err(Error::protocol(&format!("unexpected data, got {}", crate::tools::quote(&s)), None)),
            }
        }
    }

    /// The status, arguments, then the data packets up to a flush (read whole).
    pub fn read_status_with_data(&mut self) -> Result<(i32, Vec<String>, Vec<u8>)> {
        let mut args = vec![];
        let mut status = None;
        loop {
            let (s, l) = self.read_text().map_err(|e| Error::protocol("error reading packet", Some(e.into())))?;
            if l == 0 {
                return Err(Error::protocol(if status.is_none() { "no status seen" } else { "unexpected flush packet" }, None));
            } else if status.is_none() {
                status = Some(Self::status_of(&s)?);
            } else if l == 1 {
                break;
            } else {
                args.push(s);
            }
        }
        let data = self.pl.read_payload().map_err(|e| Error::protocol("error reading packet", Some(e.into())))?;
        Ok((status.unwrap(), args, data))
    }

    pub fn read_status_with_lines(&mut self) -> Result<(i32, Vec<String>, Vec<String>)> {
        let (mut args, mut lines) = (vec![], vec![]);
        let mut status = None;
        let mut delim = false;
        loop {
            let (s, l) = self.read_text().map_err(|e| Error::protocol("error reading packet", Some(e.into())))?;
            if l == 0 {
                return match status {
                    None => Err(Error::protocol("no status seen", None)),
                    Some(st) => Ok((st, args, lines)),
                };
            } else if delim {
                lines.push(s);
            } else if status.is_none() {
                status = Some(Self::status_of(&s)?);
            } else if l == 1 {
                delim = true;
            } else {
                args.push(s);
            }
        }
    }

    fn end(&mut self) -> Result<()> {
        self.send_message("quit", &[])?;
        let r = self.read_status();
        let _ = self.child.wait();
        r.map(|_| ())
    }
}

fn start_connection(id: usize, meta: &SshMetadata, operation: &str, control_path: &str) -> Result<(Connection, bool, String)> {
    crate::trace!("spawning pure SSH connection (#{})", id);
    let ((exe, args), multiplexing, cp) = lfs_exe_and_args(meta, "git-lfs-transfer", operation, true, control_path);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = crate::subprocess::command(&exe, &argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new(crate::tools::io_err(&e)))?;
    let r = child.stdout.take().unwrap();
    let w = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let errbuf = Arc::new(Mutex::new(Vec::new()));
    let eb = errbuf.clone();
    std::thread::spawn(move || {
        let mut s = stderr;
        let mut b = vec![];
        let _ = s.read_to_end(&mut b);
        eb.lock().unwrap().extend_from_slice(&b);
    });
    let mut conn = Connection { pl: Pktline::new(r, w), child, id, trace_packets: cfg().os.bool("GIT_TRACE_PACKET", false) };
    match conn.negotiate_version() {
        Ok(()) => {
            crate::trace!("pure SSH connection successful (#{})", id);
            Ok((conn, multiplexing, cp))
        }
        Err(e) => {
            drop(conn.pl);
            let _ = conn.child.wait();
            std::thread::sleep(std::time::Duration::from_millis(20));
            let stderr = String::from_utf8_lossy(&errbuf.lock().unwrap()).into_owned();
            crate::trace!("pure SSH connection unsuccessful (#{})", id);
            Err(Error::new(format!("{e}\nFailed to connect to remote SSH server: {stderr}")))
        }
    }
}

/// SSHTransfer: connections to git-lfs-transfer, the first opened at once, the others when
/// a worker first needs one.
pub struct SshTransfer {
    conns: RwLock<Vec<Option<Arc<Mutex<Connection>>>>>,
    meta: SshMetadata,
    operation: String,
    multiplexing: bool,
    control_path: Mutex<String>,
}

impl SshTransfer {
    pub fn new(meta: &SshMetadata, operation: &str) -> Result<SshTransfer> {
        let (c, multiplexing, cp) = start_connection(0, meta, operation, "")?;
        Ok(SshTransfer {
            conns: RwLock::new(vec![Some(Arc::new(Mutex::new(c)))]),
            meta: meta.clone(),
            operation: operation.to_string(),
            multiplexing,
            control_path: Mutex::new(cp),
        })
    }

    pub fn is_multiplexing_enabled(&self) -> bool {
        self.multiplexing
    }

    pub fn connection(&self, n: usize) -> Result<Arc<Mutex<Connection>>> {
        {
            let c = self.conns.read().unwrap();
            if n >= c.len() {
                return Err(Error::new(format!("pure SSH connection unavailable (#{n})")));
            }
            if let Some(x) = &c[n] {
                return Ok(x.clone());
            }
        }
        let mut c = self.conns.write().unwrap();
        if let Some(x) = &c[n] {
            return Ok(x.clone());
        }
        let cp = self.control_path.lock().unwrap().clone();
        let conn = match start_connection(n, &self.meta, &self.operation, &cp) {
            Ok((conn, _, _)) => conn,
            Err(e) => {
                crate::trace!("failed to spawn pure SSH connection (#{}): {}", n, e);
                return Err(e);
            }
        };
        let a = Arc::new(Mutex::new(conn));
        c[n] = Some(a.clone());
        Ok(a)
    }

    pub fn set_connection_count_at_least(&self, n: usize) {
        let mut c = self.conns.write().unwrap();
        while c.len() < n {
            c.push(None);
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        crate::trace!("shutting down pure SSH connections");
        let mut c = self.conns.write().unwrap();
        let count = c.len();
        for (i, item) in c.iter().enumerate().skip(1) {
            match item {
                None => crate::trace!("skipping uninitialized lazy pure SSH connection (#{}) (resetting total from {} to 0)", i, count),
                Some(x) => {
                    crate::trace!("terminating pure SSH connection (#{}) (resetting total from {} to 0)", i, count);
                    x.lock().unwrap().end()?;
                }
            }
        }
        if let Some(Some(x)) = c.first() {
            crate::trace!("terminating pure SSH connection (#0) (resetting total from {} to 0)", count);
            x.lock().unwrap().end()?;
        }
        c.clear();
        Ok(())
    }
}

type TransferCache = Mutex<Vec<(String, String, Option<Arc<SshTransfer>>)>>;

fn transfers() -> &'static TransferCache {
    static T: std::sync::OnceLock<TransferCache> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(vec![]))
}

/// lfsapi.Client.SSHTransfer: the pure SSH connection for (operation, remote), opened once
/// (and not again after it failed).
pub fn transfer_for(operation: &str, remote: &str) -> Option<Arc<SshTransfer>> {
    if operation.is_empty() {
        return None;
    }
    let mut all = transfers().lock().unwrap();
    if let Some(t) = all.iter().find(|(o, r, _)| o == operation && r == remote) {
        return t.2.clone();
    }
    let ep = crate::endpoint::endpoint(operation, remote);
    if ep.ssh.user_and_host.is_empty() {
        return None;
    }
    if let Some(v) = crate::config::url_get("lfs", &ep.original_url, "sshtransfer") {
        if v != "negotiate" && v != "always" {
            crate::trace!("skipping pure SSH protocol connection by request ({}, {})", operation, remote);
            return None;
        }
    }
    crate::trace!("attempting pure SSH protocol connection ({}, {})", operation, remote);
    match SshTransfer::new(&ep.ssh, operation) {
        Ok(t) => {
            let t = Arc::new(t);
            all.push((operation.into(), remote.into(), Some(t.clone())));
            Some(t)
        }
        Err(e) => {
            crate::trace!("pure SSH protocol connection failed ({}, {}): {}", operation, remote, e);
            None
        }
    }
}

/// closeSSHTransfers.
pub fn shutdown_all() {
    let all: Vec<_> = transfers().lock().unwrap().drain(..).collect();
    for (_, _, t) in all {
        if let Some(t) = t {
            let _ = t.shutdown();
        }
    }
}
