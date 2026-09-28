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
mod machine;
mod protocol;
mod sshagent;
mod module_info;
mod net;
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
  --tools-image FILE        the network tools overlay: curl, ssh, git (likewise)
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
  --log-requests            print every request the guest makes, to stderr

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
    "--python-image", "--tools-image", "--addon-dir", "--addon", "--addon-config", "--kernel",
    "--initramfs", "--cpus", "--mount", "--allow", "--deny", "--secret", "--cwd", "--arg",
];

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

    /// The terminal's size (columns, rows), if stdout is one.
    pub fn size() -> Option<(u16, u16)> {
        // SAFETY: TIOCGWINSZ fills the winsize.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            (libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0).then_some((size.ws_col, size.ws_row))
        }
    }
}

#[cfg(not(unix))]
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
    let mut mounts: Vec<fs::Share> = Vec::new();
    let mut policy = http::Policy::default();
    let mut allow: Vec<String> = Vec::new();
    let mut network = true;
    let mut log_requests = false;
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

    let vsock = vsock::Vsock::new(3);
    let policy = std::sync::Arc::new(std::sync::RwLock::new(policy));
    http::serve(
        &vsock,
        http::DEFAULT_PORT,
        policy.clone(),
        log_requests.then(|| -> std::sync::Arc<dyn Fn(http::Event) + Send + Sync> {
            std::sync::Arc::new(|event: http::Event| match (event.status, event.error) {
                (Some(status), _) => eprintln!("[network] {} {} -> {status}", event.method, event.url),
                (None, Some(error)) if event.blocked => eprintln!("[network] blocked {} {}: {error}", event.method, event.url),
                (None, Some(error)) => eprintln!("[network] {} {}: {error}", event.method, event.url),
                _ => {}
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
            log_requests.then(|| -> std::sync::Arc<dyn Fn(net::Event) + Send + Sync> {
                std::sync::Arc::new(|event: net::Event| eprintln!("[network] {event:?}"))
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
    })
    .context("booting the kernel")?;

    // Whatever is typed here goes to the guest's console.
    let input = machine.console_input();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
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
            let outcome = agent::connect(&vsock, Duration::from_secs(30)).and_then(|stream| {
                let (mut out, mut err) = (writer(false), writer(true));
                agent::run(&stream, &command, None, |bytes| out(bytes), |bytes| err(bytes))
            });
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
