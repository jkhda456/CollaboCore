//! collaboCore engine: runs the sandbox's WebAssembly Linux kernel natively (no browser, no
//! Node.js).
//!
//!   collabo-core-engine [options]                 the guest's root shell in this terminal
//!   collabo-core-engine exec [options] -- CMD...  run one command in the guest, then stop
//!   collabo-core-engine --stdio [options]         the control protocol on stdin/stdout, for
//!                                                 the Flutter/Dart package (protocol.rs)
//!
//! options: --kernel FILE      the kernel image (vmlinux.wasm)
//!          --initramfs FILE   a cpio archive to unpack at boot (repeatable)
//!          --cpus N           virtual CPUs (default 2)
//!          --mount HOST:GUEST[:ro]  share a host folder with the guest (repeatable)
//!          --allow HOST       a host the guest may reach (repeatable; default: any)
//!          --deny HOST        a host it may not (repeatable, checked first)
//!          --no-network       no outbound requests at all
//!          --allow-loopback   let the guest reach this computer's own services
//!          --secret HOST:HEADER=VALUE  a key the host adds to https requests (never seen
//!                             by the guest, and never logged)
//!          --log-requests     print every request the guest makes
//!          --cwd PATH         the guest directory to run the command in (exec)
//!          --python-image FILE  the CPython overlay, added unless the app turns it off
//!          --tools-image FILE   the network tools overlay (curl, ssh, git), likewise
//!          --arg TEXT         an extra kernel command line argument (repeatable)
mod agent;
mod console;
mod devicetree;
mod fs;
mod hostfn;
mod http;
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

/// Writes to a stream of this process, flushing each chunk so output appears as it happens.
fn writer(to_stderr: bool) -> Box<dyn FnMut(&[u8]) + Send> {
    Box::new(move |bytes: &[u8]| {
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
    let mut mounts: Vec<fs::Share> = Vec::new();
    let mut policy = http::Policy::default();
    let mut allow: Vec<String> = Vec::new();
    let mut network = true;
    let mut log_requests = false;

    let mut argv = std::env::args().skip(1);
    while let Some(argument) = argv.next() {
        match argument.as_str() {
            "exec" => exec = true,
            "--stdio" => stdio = true,
            "--python-image" => python_image = argv.next(),
            "--tools-image" => tools_image = argv.next(),
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
            "--cwd" => cwd = argv.next(),
            "--arg" => args.extend(argv.next()),
            "--" => command.extend(argv.by_ref()),
            other => args.push(other.to_string()),
        }
    }
    if exec && command.is_empty() {
        anyhow::bail!("exec needs a command after --");
    }

    let kernel_path = kernel_path.context("--kernel is required")?;
    if stdio {
        // The app describes the sandbox in its `start` request; the command line only says
        // where the images are.
        return protocol::Server::new(protocol::Images {
            kernel: kernel_path.into(),
            initramfs: initramfs_paths.iter().map(Into::into).collect(),
            python: python_image.map(Into::into),
            tools: tools_image.map(Into::into),
        })
        .serve();
    }
    let kernel = std::fs::read(&kernel_path).with_context(|| format!("reading {kernel_path}"))?;
    let mut initcpio = Vec::new();
    for path in &initramfs_paths {
        initcpio.extend(std::fs::read(path).with_context(|| format!("reading {path}"))?);
    }

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
    let console = console::Console::new(100, 30, writer(console_to_stderr));
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
        while let Ok(read) = stdin.read(&mut buffer) {
            if read == 0 || input.send(buffer[..read].to_vec()).is_err() {
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
        Some(Ok((exit, _))) => std::process::exit(exit.status()),
        Some(Err(error)) => {
            eprintln!("collabo-core: {error:#}");
            std::process::exit(125);
        }
        None => {
            if termination == machine::Termination::Panic {
                eprintln!("[engine] the guest panicked");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
