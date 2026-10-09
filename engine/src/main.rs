//! collaboCore engine: runs the sandbox's WebAssembly Linux kernel natively (no browser, no
//! Node.js). The command line is `USAGE` below (`collabo-core-engine --help`); `--stdio` is the
//! control protocol the Flutter/Dart package speaks (protocol.rs).
mod addons;
mod agent;
mod console;
mod devicetree;
mod fs;
mod hostfn;
mod http;
mod icmp;
mod intercept;
mod log;
mod machine;
mod protocol;
mod sshagent;
mod module_info;
mod net;
mod program_cache;
mod user;
mod virtio;
mod vsock;
mod zip;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

/// What `--help` prints. The paths in the examples are relative to a runtime folder
/// (dist/runtime/collabo-core-<platform>, scripts/package-runtime.sh).
const USAGE: &str = "\
collabo-core-engine: a Linux machine, compiled to WebAssembly, in this process

Usage:
  collabo-core-engine [OPTIONS]                  the guest's root shell in this terminal
  collabo-core-engine exec [OPTIONS] -- CMD...   run one command in the guest, then stop with
                                                 its exit status (125: it could not be run)
  collabo-core-engine --stdio [IMAGES]           the control protocol on stdin/stdout, for an app
                                                 (the app describes the sandbox in `start`)
  collabo-core-engine --help                     this text

Images:
  --kernel FILE             the kernel image, vmlinux.wasm (required)
  --initramfs FILE          a cpio archive to unpack at boot (repeatable, in order)
  --python-image FILE       the CPython overlay (with --stdio: added unless the app turns it off)
  --tools-image FILE        the tools overlay: curl, ssh, git, screen, gui (likewise)
  --addon-dir DIR           where the add-ons are: <name>.cpio + <name>.json

Machine:
  --cpus N                  virtual CPUs (default 2)
  --mount HOST:GUEST[:ro]   share a host folder at an absolute guest path (repeatable)
  --cwd PATH                the guest directory to run the command in (exec)
  --arg TEXT                an extra kernel command line argument (repeatable; a bare word
                            that is not an option counts as one too)

Network (the guest's sockets and its request API go through the same policy):
  --allow HOST              a host the guest may reach (repeatable; default: any)
  --deny HOST               a host it may not (repeatable, checked first)
  --no-network              no outbound connections at all
  --allow-loopback          let the guest reach this computer's own services
  --secret HOST:HEADER=VALUE
                            a header the host adds to https requests to HOST (repeatable);
                            the guest never sees it, and it is never logged

Logs (none by default; FILE is appended to, each line after its UTC time; - is stderr):
  --log-network FILE        the network log: every request the guest makes ([network])
  --log-exec FILE           the exec log, a history of what ran: [run] for the command exec
                            runs (when it starts, how it ended), [exec] for every program the
                            guest starts, from any shell or script (pid, uid, folder, argv, or
                            why it failed)
  --log-file FILE           both logs in one file (unless --log-network or --log-exec names
                            another place for one of them)
  --log-requests            the same as --log-network - (when no file is named)
  --log-exec-kinds KINDS    which lines the exec log keeps: run, exec or run,exec (default)
  --log-max-size SIZE       a log file grows to at most SIZE bytes (K, M, G: KiB, MiB, GiB);
                            default: no limit. A line longer than 4 KiB is cut, saying how much
  --log-rotate N            when a file reaches that size, move it to FILE.1 (FILE.1 to
                            FILE.2, ...) keeping N old files; default 0: the file is kept and
                            later lines are dropped

GUI programs (the `gui` command in the tools image; headless until asked):
  --gui-config KEY=VALUE    a setting of the display, written to /etc/collabo/gui.json
                            (repeatable): defaultSize=1024x768, maxSize=1920x1080,
                            maxCanvases=8, maxMemoryMB=256, capture=false, input=false,
                            clipboard=false

Add-ons (optional overlays; addons/README.md):
  --addon NAME              boot with that add-on (repeatable)
  --addon-config NAME:KEY=VALUE
                            one of its settings (repeatable; implies --addon NAME), e.g.
                            claude-code:provider=openai; an apiKey becomes a --secret

Terminal:
  --no-raw                  leave this terminal line-buffered (Ctrl-C then ends the engine)
  By default the shell gets this terminal raw and at its size, so keys (Ctrl-C too) reach the
  guest as they are typed and full-screen programs work. Ctrl-] then q leaves; so does
  `shutdown` (or poweroff) in the guest, which stops its programs first and syncs the mounts.

Examples (in a runtime folder):
  bin/collabo-core-engine --kernel app/images/vmlinux.wasm \\
      --initramfs app/images/initramfs.cpio --initramfs app/images/python.cpio \\
      --mount \"$PWD:/work\"
  bin/collabo-core-engine exec --kernel app/images/vmlinux.wasm \\
      --initramfs app/images/initramfs.cpio --no-network -- uname -a
  manifest.json's `entry` holds the arguments an app starts it with, before --stdio.
";

/// The options that take a value, so `--help` can be told from an option's value.
const TAKES_VALUE: &[&str] = &[
    "--python-image", "--tools-image", "--addon-dir", "--addon", "--addon-config", "--gui-config", "--kernel",
    "--initramfs", "--cpus", "--mount", "--allow", "--deny", "--secret", "--cwd", "--arg",
    "--log-network", "--log-exec", "--log-file", "--log-exec-kinds", "--log-max-size", "--log-rotate",
];

/// Bytes of a URL a network log line keeps.
const URL_LIMIT: usize = 2048;

/// Which lines --log-exec keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandKinds {
    /// The command `exec` runs, when it starts and when it ends.
    run: bool,
    /// Every program the guest starts (the kernel reports them).
    exec: bool,
}

impl Default for CommandKinds {
    fn default() -> Self {
        CommandKinds { run: true, exec: true }
    }
}

fn parse_command_kinds(text: &str) -> Result<CommandKinds> {
    let mut kinds = CommandKinds { run: false, exec: false };
    for word in text.split(',').map(str::trim) {
        match word {
            "run" => kinds.run = true,
            "exec" => kinds.exec = true,
            "all" => kinds = CommandKinds { run: true, exec: true },
            _ => anyhow::bail!("--log-exec-kinds {text}: the kinds are run, exec (or run,exec)"),
        }
    }
    Ok(kinds)
}

/// One program the guest started, as a command log line.
fn exec_line(event: &machine::ExecEvent) -> String {
    let mut line = format!(
        "[exec] pid={} uid={} cwd={} file={}",
        event.pid,
        event.uid,
        log::quote(&event.cwd),
        log::quote(&event.file)
    );
    if event.errno != 0 {
        line += &format!(" failed: {}", linux_errno(event.errno));
    } else {
        line += &format!(": {}", log::quote_all(&event.argv));
    }
    log::clean_cut(&line, event.cut)
}

/// A guest errno by its Linux name (the host's own numbers differ on Windows and macOS): the ones
/// an exec can fail with once the file is a wasm program.
fn linux_errno(errno: i32) -> String {
    let name = match errno {
        5 => "EIO (I/O error)",
        7 => "E2BIG (argument list too long)",
        8 => "ENOEXEC (not a program this kernel runs)",
        12 => "ENOMEM (out of memory)",
        13 => "EACCES (permission denied)",
        14 => "EFAULT (bad address)",
        22 => "EINVAL (invalid argument)",
        27 => "EFBIG (file too large)",
        _ => return format!("errno {errno}"),
    };
    name.to_string()
}

/// Whether the command line asks for help: `-h`, `--help` or `help` as an option (not as an
/// option's value, and not in the command after `--`).
fn wants_help(argv: &[String]) -> bool {
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-h" | "--help" | "help" => return true,
            "--" => return false,
            option if TAKES_VALUE.contains(&option) => {
                words.next();
            }
            _ => {}
        }
    }
    false
}

/// `HOST:GUEST[:ro]`. The host path may contain ':' (a Windows drive letter), so the guest
/// path and the read-only marker are taken from the right.
fn parse_mount(spec: &str, index: usize) -> Result<fs::Share> {
    let (body, read_only) = match spec.strip_suffix(":ro") {
        Some(body) => (body, true),
        None => (spec, false),
    };
    let cut = body.rfind(':').filter(|cut| *cut > 0).context("--mount expects HOST:GUEST[:ro]")?;
    let guest_path = body[cut + 1..].to_string();
    if !guest_path.starts_with('/') {
        anyhow::bail!("the guest path of --mount must be absolute: {guest_path}");
    }
    Ok(fs::Share {
        // The tag only has to be unique and free of spaces: the guest mounts by it.
        tag: format!("mount{index}"),
        host_path: body[..cut].into(),
        guest_path,
        read_only,
    })
}

/// This process's terminal in raw mode while the guest's console owns it; restored on exit.
#[cfg(unix)]
mod host_tty {
    use std::sync::Mutex;

    static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

    /// Raw mode for stdin's terminal, if stdin is one. True when it was switched.
    pub fn enter() -> bool {
        // SAFETY: termios calls on fd 0 with a zeroed struct that tcgetattr fills.
        unsafe {
            if libc::isatty(0) == 0 {
                return false;
            }
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut termios) != 0 {
                return false;
            }
            *SAVED.lock().unwrap() = Some(termios);
            let mut raw = termios;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw) == 0
        }
    }

    pub fn restore() {
        if let Some(termios) = SAVED.lock().unwrap().take() {
            // SAFETY: the settings tcgetattr gave back.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) };
        }
    }

    pub fn active() -> bool {
        SAVED.lock().unwrap().is_some()
    }

    /// What is typed.
    pub fn input() -> Box<dyn std::io::Read + Send> {
        Box::new(std::io::stdin())
    }

    /// The terminal's size (columns, rows), if stdout is one.
    pub fn size() -> Option<(u16, u16)> {
        // SAFETY: TIOCGWINSZ fills the winsize.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            (libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0).then_some((size.ws_col, size.ws_row))
        }
    }
}

/// The console in raw mode: no line editing or echo of its own, Ctrl-C a key (0x03) rather than
/// a signal to this process, keys as VT sequences, and VT output; restored on exit.
#[cfg(windows)]
mod host_tty {
    use std::sync::Mutex;
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleW,
        SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO,
        DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    /// The modes and code pages to put back: (input mode, output mode if it is a console, input
    /// code page, output code page).
    static SAVED: Mutex<Option<(CONSOLE_MODE, Option<CONSOLE_MODE>, u32, u32)>> = Mutex::new(None);

    /// Raw mode for stdin's console, if stdin is one. True when it was switched.
    pub fn enter() -> bool {
        // SAFETY: console calls on this process's standard handles.
        unsafe {
            let (input, output) = (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE));
            let mut in_mode: CONSOLE_MODE = 0;
            if GetConsoleMode(input, &mut in_mode) == 0 {
                return false;
            }
            let mut out_mode: CONSOLE_MODE = 0;
            let out_mode = (GetConsoleMode(output, &mut out_mode) != 0).then_some(out_mode);
            let raw = (in_mode & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if SetConsoleMode(input, raw) == 0 {
                return false;
            }
            *SAVED.lock().unwrap() = Some((in_mode, out_mode, GetConsoleCP(), GetConsoleOutputCP()));
            if let Some(mode) = out_mode {
                // Without DISABLE_NEWLINE_AUTO_RETURN where the console does not know it.
                let vt = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                if SetConsoleMode(output, vt | DISABLE_NEWLINE_AUTO_RETURN) == 0 {
                    SetConsoleMode(output, vt);
                }
            }
            SetConsoleCP(65001);
            SetConsoleOutputCP(65001);
            true
        }
    }

    pub fn restore() {
        if let Some((in_mode, out_mode, in_cp, out_cp)) = SAVED.lock().unwrap().take() {
            // SAFETY: the settings the console gave back.
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), in_mode);
                if let Some(mode) = out_mode {
                    SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), mode);
                }
                SetConsoleCP(in_cp);
                SetConsoleOutputCP(out_cp);
            }
        }
    }

    pub fn active() -> bool {
        SAVED.lock().unwrap().is_some()
    }

    /// The console window's size (columns, rows), if stdout is one.
    pub fn size() -> Option<(u16, u16)> {
        // SAFETY: the call fills the zeroed struct.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
                return None;
            }
            let window = info.srWindow;
            let (columns, rows) = (window.Right - window.Left + 1, window.Bottom - window.Top + 1);
            (columns > 0 && rows > 0).then_some((columns as u16, rows as u16))
        }
    }

    /// What is typed, as UTF-8. In raw mode the console is read directly: std's stdin reads a
    /// lone Ctrl-Z as the end of input, which would stop the guest's keyboard.
    pub fn input() -> Box<dyn std::io::Read + Send> {
        if active() {
            Box::new(Console { pending: Vec::new(), high: None })
        } else {
            Box::new(std::io::stdin())
        }
    }

    struct Console {
        pending: Vec<u8>,
        /// A high surrogate whose pair is in the next read.
        high: Option<u16>,
    }

    impl std::io::Read for Console {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            while self.pending.is_empty() {
                let mut units = [0u16; 512];
                let mut read = 0u32;
                // SAFETY: the buffer holds as many units as asked for.
                let ok = unsafe {
                    ReadConsoleW(
                        GetStdHandle(STD_INPUT_HANDLE),
                        units.as_mut_ptr().cast(),
                        units.len() as u32,
                        &mut read,
                        std::ptr::null(),
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut units: Vec<u16> = self.high.take().into_iter().chain(units[..read as usize].iter().copied()).collect();
                if units.last().is_some_and(|unit| (0xd800..0xdc00).contains(unit)) {
                    self.high = units.pop();
                }
                let text: String = char::decode_utf16(units).map(|c| c.unwrap_or('\u{fffd}')).collect();
                self.pending = text.into_bytes();
            }
            let count = buffer.len().min(self.pending.len());
            buffer[..count].copy_from_slice(&self.pending[..count]);
            self.pending.drain(..count);
            Ok(count)
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod host_tty {
    pub fn enter() -> bool {
        false
    }
    pub fn restore() {}
    pub fn active() -> bool {
        false
    }
    pub fn size() -> Option<(u16, u16)> {
        None
    }
    pub fn input() -> Box<dyn std::io::Read + Send> {
        Box::new(std::io::stdin())
    }
}

/// Leaves with the terminal as it was.
fn exit(code: i32) -> ! {
    host_tty::restore();
    std::process::exit(code)
}

/// Writes to a stream of this process, flushing each chunk so output appears as it happens.
/// A raw terminal does not turn \n into \r\n; the kernel's early messages need it done here
/// (the guest's tty already does it for what programs write).
fn writer(to_stderr: bool) -> Box<dyn FnMut(&[u8]) + Send> {
    let mut previous = 0u8;
    Box::new(move |bytes: &[u8]| {
        let converted;
        let bytes = if host_tty::active() && bytes.contains(&b'\n') {
            let mut out = Vec::with_capacity(bytes.len() + 16);
            for &byte in bytes {
                if byte == b'\n' && previous != b'\r' {
                    out.push(b'\r');
                }
                out.push(byte);
                previous = byte;
            }
            converted = out;
            &converted[..]
        } else {
            if let Some(&last) = bytes.last() {
                previous = last;
            }
            bytes
        };
        if to_stderr {
            let mut out = std::io::stderr().lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        } else {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        }
    })
}

fn main() -> Result<()> {
    let mut kernel_path = None;
    let mut initramfs_paths: Vec<String> = Vec::new();
    let mut cpus = 2u32;
    let mut args: Vec<String> = Vec::new();
    let mut command: Vec<String> = Vec::new();
    let mut cwd = None;
    let mut exec = false;
    let mut stdio = false;
    let mut python_image: Option<String> = None;
    let mut tools_image: Option<String> = None;
    let mut addon_dir: Option<String> = None;
    let mut addon_settings = serde_json::Map::new();
    let mut gui_settings = serde_json::Map::new();
    let mut mounts: Vec<fs::Share> = Vec::new();
    let mut policy = http::Policy::default();
    let mut allow: Vec<String> = Vec::new();
    let mut network = true;
    let mut log_requests = false;
    let mut log_network: Option<String> = None;
    let mut log_exec: Option<String> = None;
    let mut log_file: Option<String> = None;
    let mut command_kinds = CommandKinds::default();
    let mut log_limits = log::Limits::default();
    let mut raw = true;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&argv) {
        print!("{USAGE}");
        return Ok(());
    }
    if argv.is_empty() {
        eprint!("{USAGE}");
        exit(2);
    }
    let mut argv = argv.into_iter();
    while let Some(argument) = argv.next() {
        match argument.as_str() {
            "exec" => exec = true,
            "--stdio" => stdio = true,
            "--python-image" => python_image = argv.next(),
            "--tools-image" => tools_image = argv.next(),
            "--addon-dir" => addon_dir = argv.next(),
            "--addon" => {
                let name = argv.next().context("--addon needs a name")?;
                addon_settings.entry(name).or_insert(serde_json::json!(true));
            }
            "--addon-config" => {
                let spec = argv.next().context("--addon-config needs NAME:KEY=VALUE")?;
                addons::parse_setting(&spec, &mut addon_settings).map_err(anyhow::Error::msg)?;
            }
            "--gui-config" => {
                let spec = argv.next().context("--gui-config needs KEY=VALUE")?;
                if !spec.contains('=') {
                    anyhow::bail!("--gui-config needs KEY=VALUE");
                }
                // The add-on settings' parser, for the same typing of numbers and true/false.
                addons::parse_setting(&format!("gui:{spec}"), &mut gui_settings).map_err(anyhow::Error::msg)?;
            }
            "--kernel" => kernel_path = argv.next(),
            "--initramfs" => initramfs_paths.extend(argv.next()),
            "--cpus" => cpus = argv.next().context("--cpus needs a number")?.parse()?,
            "--mount" => {
                let spec = argv.next().context("--mount needs HOST:GUEST[:ro]")?;
                mounts.push(parse_mount(&spec, mounts.len())?);
            }
            "--allow" => allow.extend(argv.next()),
            "--deny" => policy.deny.extend(argv.next()),
            "--no-network" => network = false,
            "--allow-loopback" => policy.allow_loopback = true,
            "--secret" => {
                let spec = argv.next().context("--secret needs HOST:HEADER=VALUE")?;
                policy.secrets.push(http::parse_secret(&spec)?);
            }
            "--log-requests" => log_requests = true,
            "--log-network" => log_network = Some(argv.next().context("--log-network needs a file (- for stderr)")?),
            "--log-exec" => log_exec = Some(argv.next().context("--log-exec needs a file (- for stderr)")?),
            "--log-file" => log_file = Some(argv.next().context("--log-file needs a file (- for stderr)")?),
            "--log-exec-kinds" => command_kinds = parse_command_kinds(&argv.next().context("--log-exec-kinds needs run, exec or run,exec")?)?,
            "--log-max-size" => log_limits.max_size = Some(log::parse_size(&argv.next().context("--log-max-size needs a size")?)?),
            "--log-rotate" => {
                let count = argv.next().context("--log-rotate needs a number")?;
                log_limits.rotate = count.parse().with_context(|| format!("--log-rotate {count}: a number of files"))?;
            }
            "--no-raw" => raw = false,
            "--cwd" => cwd = argv.next(),
            "--arg" => args.extend(argv.next()),
            "--" => command.extend(argv.by_ref()),
            option if option.starts_with('-') => {
                anyhow::bail!("unknown option {option} (collabo-core-engine --help lists them)")
            }
            other => args.push(other.to_string()),
        }
    }
    if exec && command.is_empty() {
        anyhow::bail!("exec needs a command after --");
    }

    let kernel_path = kernel_path.context("--kernel is required (collabo-core-engine --help)")?;
    if stdio {
        // The app describes the sandbox in its `start` request; the command line only says
        // where the images are.
        return protocol::Server::new(protocol::Images {
            kernel: kernel_path.into(),
            initramfs: initramfs_paths.iter().map(Into::into).collect(),
            python: python_image.map(Into::into),
            tools: tools_image.map(Into::into),
            addon_dir: addon_dir.map(Into::into),
        })
        .serve();
    }
    let kernel = std::fs::read(&kernel_path).with_context(|| format!("reading {kernel_path}"))?;
    let mut initcpio = Vec::new();
    for path in &initramfs_paths {
        initcpio.extend(std::fs::read(path).with_context(|| format!("reading {path}"))?);
    }
    // The overlays --stdio adds unless the app turns them off, here only when named.
    for path in python_image.iter().chain(&tools_image) {
        initcpio.extend(std::fs::read(path).with_context(|| format!("reading {path}"))?);
    }
    let addons = match addon_settings.is_empty() {
        true => addons::Resolved::default(),
        false => addons::resolve(addon_dir.as_deref().map(std::path::Path::new), Some(&serde_json::Value::Object(addon_settings)))
            .map_err(anyhow::Error::msg)?,
    };
    initcpio.extend_from_slice(&addons.initcpio);
    if let Some(serde_json::Value::Object(settings)) = gui_settings.get("gui") {
        initcpio.extend(protocol::gui_settings(settings));
    }
    policy.addon_headers = addons.headers;
    policy.addon_secrets = addons.secrets;

    // The guest's own output stays out of the command's stdout, which belongs to the command.
    let console_to_stderr = exec;
    if exec {
        args.push("collabo.agent=1".into());
        args.push("collabo.quiet=1".into());
    }

    // --allow replaces the default "anything"; --no-network refuses everything.
    if !allow.is_empty() {
        policy.allow = allow;
    }
    if !network {
        policy.allow.clear();
    }

    // Each log goes where its own option says, else to --log-file; one place is opened once, so
    // two logs in one file share its size limit.
    let network_to = log_network.or(log_file.clone()).or(log_requests.then(|| "-".to_string()));
    let exec_to = log_exec.or(log_file);
    let mut opened: Vec<(String, log::Log)> = Vec::new();
    let mut open = |to: &String| -> Result<log::Log> {
        if let Some((_, log)) = opened.iter().find(|(place, _)| place == to) {
            return Ok(log.clone());
        }
        let log = if to == "-" { log::to_stderr() } else { log::to_file(to, log_limits)? };
        opened.push((to.clone(), log.clone()));
        Ok(log)
    };
    let request_log = network_to.as_ref().map(&mut open).transpose()?;
    let command_log = exec_to.as_ref().map(&mut open).transpose()?;
    let exec_log = command_log.clone().filter(|_| command_kinds.exec).map(|log| -> machine::ExecObserver {
        Arc::new(move |event: machine::ExecEvent| log(&exec_line(&event)))
    });
    let run_log = command_log.filter(|_| command_kinds.run);

    let vsock = vsock::Vsock::new(3);
    let policy = std::sync::Arc::new(std::sync::RwLock::new(policy));
    http::serve(
        &vsock,
        http::DEFAULT_PORT,
        policy.clone(),
        request_log.clone().map(|log| -> std::sync::Arc<dyn Fn(http::Event) + Send + Sync> {
            std::sync::Arc::new(move |event: http::Event| {
                // The status or the error after it stays on the line however long the URL is.
                let url = log::shorten(&event.url, URL_LIMIT);
                match (event.status, event.error) {
                    (Some(status), _) => log(&format!("[network] {} {url} -> {status}", event.method)),
                    (None, Some(error)) if event.blocked => log(&format!("[network] blocked {} {url}: {error}", event.method)),
                    (None, Some(error)) => log(&format!("[network] {} {url}: {error}", event.method)),
                    _ => {}
                }
            })
        }),
        None,
    )?;
    // The guest's shell gets this terminal, raw and at its size (not for one command: its
    // output is this process's output, and Ctrl-C ends it).
    let raw = raw && !exec && host_tty::enter();
    let (columns, rows) = if raw { host_tty::size().unwrap_or((100, 30)) } else { (100, 30) };
    if raw {
        // The guest's agent sets its tty to this terminal's size when the window changes.
        args.push("collabo.agent=1".into());
        let vsock = vsock.clone();
        std::thread::spawn(move || {
            let mut last = (columns, rows);
            loop {
                std::thread::sleep(Duration::from_millis(400));
                if let Some(now) = host_tty::size() {
                    if now != last && agent::resize_console(&vsock, now.0, now.1).is_ok() {
                        last = now;
                    }
                }
            }
        });
    }
    let console = console::Console::new(columns, rows, writer(console_to_stderr));
    let mut devices: Vec<Box<dyn virtio::Device>> = vec![Box::new(console), vsock.device()];

    // The guest's own sockets, through a NIC whose gateway is this process.
    if network {
        let stack = net::Stack::new(
            policy,
            request_log.clone().map(|log| -> std::sync::Arc<dyn Fn(net::Event) + Send + Sync> {
                std::sync::Arc::new(move |event: net::Event| log(&format!("[network] {event:?}")))
            }),
            None,
            None,
        );
        devices.push(stack.device());
        args.extend(net::Stack::kernel_arguments());
    }
    for share in mounts {
        // The guest's /etc/rc mounts what the kernel command line names.
        let option = if share.read_only { ":ro" } else { "" };
        args.push(format!("collabo.mount={}:{}{option}", share.tag, share.guest_path));
        devices.push(Box::new(fs::FsDevice::new(share)?));
    }

    let mut machine = machine::Machine::boot(machine::BootOptions {
        kernel,
        devices,
        args,
        cpus,
        initcpio,
        boot_console: writer(console_to_stderr),
        exec_log,
    })
    .context("booting the kernel")?;

    // Whatever is typed here goes to the guest's console.
    let input = machine.console_input();
    std::thread::spawn(move || {
        let mut stdin = host_tty::input();
        let mut buffer = [0u8; 1024];
        // Ctrl-] then q leaves; Ctrl-] then anything else sends both on.
        let mut escaped = false;
        while let Ok(read) = stdin.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let mut bytes = Vec::with_capacity(read + 1);
            for &byte in &buffer[..read] {
                match (raw, escaped, byte) {
                    (true, false, 0x1d) => escaped = true,
                    (true, true, b'q' | b'Q' | b'.') => {
                        eprint!("\r\n[collabo-core] left the guest\r\n");
                        exit(0);
                    }
                    (true, true, other) => {
                        escaped = false;
                        bytes.extend_from_slice(&[0x1d, other]);
                    }
                    _ => bytes.push(byte),
                }
            }
            if !bytes.is_empty() && input.send(bytes).is_err() {
                break;
            }
        }
    });

    // One command: run it on a thread of its own, then stop the machine.
    let result: Arc<Mutex<Option<Result<(agent::Exit, bool)>>>> = Arc::new(Mutex::new(None));
    if exec {
        let (vsock, stopper, result) = (vsock.clone(), machine.stopper(), result.clone());
        let command = agent::Command { argv: command, env: Vec::new(), cwd, stdin: Vec::new() };
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            if let Some(log) = &run_log {
                let cwd = command.cwd.as_deref().map(|cwd| format!(" cwd={}", log::quote(cwd))).unwrap_or_default();
                log(&format!("[run]{cwd}: {}", log::quote_all(&command.argv)));
            }
            let outcome = agent::connect(&vsock, Duration::from_secs(30)).and_then(|stream| {
                let (mut out, mut err) = (writer(false), writer(true));
                agent::run(&stream, &command, None, |bytes| out(bytes), |bytes| err(bytes))
            });
            if let Some(log) = &run_log {
                let seconds = started.elapsed().as_secs_f64();
                match &outcome {
                    Ok((status, _)) => log(&format!("[run] exit={} after {seconds:.1}s", status.status())),
                    Err(error) => log(&format!("[run] failed after {seconds:.1}s: {error:#}")),
                }
            }
            *result.lock().unwrap() = Some(outcome);
            stopper.stop();
        });
    }

    let termination = machine.run()?;
    let outcome = result.lock().unwrap().take();
    match outcome {
        Some(Ok((status, _))) => exit(status.status()),
        Some(Err(error)) => {
            eprintln!("collabo-core: {error:#}");
            exit(125);
        }
        None => {
            if termination == machine::Termination::Panic {
                eprintln!("[engine] the guest panicked");
                exit(1);
            }
            if termination == machine::Termination::PowerOff && !exec {
                eprint!("\r\n[collabo-core] the guest shut down\r\n");
            }
            host_tty::restore();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_records_become_command_lines() {
        let event = machine::ExecEvent::parse(42, 0, 0, b"/work/a b\0/bin/ls\0ls\0-la\0it's\0", 26);
        assert_eq!(exec_line(&event), "[exec] pid=42 uid=0 cwd='/work/a b' file=/bin/ls: ls -la 'it'\\''s'");
        // Cut by the kernel: the last argument is partial, and the line says how much is missing.
        let event = machine::ExecEvent::parse(7, 1000, 0, b"/\0/bin/echo\0echo\0aaa", 5000);
        assert_eq!(event.argv, ["echo", "aaa"]);
        assert_eq!(exec_line(&event), "[exec] pid=7 uid=1000 cwd=/ file=/bin/echo: echo aaa …[+4980 bytes]");
        let event = machine::ExecEvent::parse(9, 0, -7, b"/\0/bin/big\0", 13);
        assert!(exec_line(&event).starts_with("[exec] pid=9 uid=0 cwd=/ file=/bin/big failed: E2BIG (argument list too long)"));
        assert_eq!(parse_command_kinds("run, exec").unwrap(), CommandKinds::default());
        assert_eq!(parse_command_kinds("exec").unwrap(), CommandKinds { run: false, exec: true });
        assert!(parse_command_kinds("shell").is_err());
    }

    #[test]
    fn help_is_an_option_not_a_value_or_the_command() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert!(wants_help(&argv(&["--help"])));
        assert!(wants_help(&argv(&["--kernel", "k.wasm", "-h"])));
        assert!(wants_help(&argv(&["exec", "help"])));
        assert!(!wants_help(&argv(&["exec", "--kernel", "k.wasm", "--", "ls", "--help"])));
        assert!(!wants_help(&argv(&["--arg", "help", "--kernel", "k.wasm"])));
        assert!(!wants_help(&argv(&[])));
        // Every option main() matches is in --help, and the ones that read a value are in
        // TAKES_VALUE (they are the arms that call argv.next()).
        for line in include_str!("main.rs").lines().map(str::trim) {
            let Some(rest) = line.strip_prefix("\"--").filter(|_| line.contains("\" => ")) else { continue };
            let option = format!("--{}", &rest[..rest.find('"').unwrap()]);
            if option == "--" {
                continue;
            }
            assert!(USAGE.contains(&format!("{option} ")), "{option} is not in USAGE");
            let takes_value = line.contains("argv.next()") || line.ends_with("=> {");
            assert_eq!(TAKES_VALUE.contains(&option.as_str()), takes_value, "{option} and TAKES_VALUE");
        }
    }

    #[test]
    fn mounts_take_unix_and_windows_paths() {
        let unix = parse_mount("/home/me/proj:/work", 0).unwrap();
        assert_eq!(unix.host_path.to_string_lossy(), "/home/me/proj");
        assert_eq!(unix.guest_path, "/work");
        assert!(!unix.read_only);
        assert_eq!(unix.tag, "mount0");

        // A Windows host path carries a drive letter, so the guest path is taken from the right.
        let windows = parse_mount(r"C:\Users\me\docs:/docs:ro", 1).unwrap();
        assert_eq!(windows.host_path.to_string_lossy(), r"C:\Users\me\docs");
        assert_eq!(windows.guest_path, "/docs");
        assert!(windows.read_only);
        assert_eq!(windows.tag, "mount1");

        assert!(parse_mount("/only-a-host-path", 0).is_err());
        assert!(parse_mount("/host:relative", 0).is_err());
    }

    #[test]
    fn secrets_are_host_header_value() {
        let secret = http::parse_secret("api.example.com:X-Api-Key=sk-123").unwrap();
        assert_eq!((secret.host.as_str(), secret.header.as_str(), secret.value.as_str()),
                   ("api.example.com", "x-api-key", "sk-123"));
        assert!(http::parse_secret("nothing-to-split").is_err());
        assert!(http::parse_secret("host:bad header=value").is_err());
    }
}
