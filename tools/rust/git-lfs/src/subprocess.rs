//! Running git and other commands as git-lfs does (subprocess package): the environment
//! without GIT_TRACE and GIT_INTERNAL_SUPER_PREFIX, and its error text for failures.

use crate::errors::{Error, Result};
use std::process::{Command, Stdio};

pub fn command(name: &str, args: &[&str]) -> Command {
    crate::trace!("exec: {} {}", name, quoted_args(args));
    let mut c = Command::new(name);
    c.args(args);
    c.env_remove("GIT_TRACE");
    c.env_remove("GIT_INTERNAL_SUPER_PREFIX");
    c
}

pub fn quoted_args(args: &[&str]) -> String {
    args.iter().map(|a| format!("'{a}'")).collect::<Vec<_>>().join(" ")
}

/// Which path a command name resolves to (for messages: `error running /usr/bin/git ...`).
pub fn look_path(name: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let p = std::path::Path::new(if dir.is_empty() { "." } else { dir }).join(name);
        if let Ok(m) = std::fs::metadata(&p) {
            use std::os::unix::fs::PermissionsExt;
            if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                return p.display().to_string();
            }
        }
    }
    name.to_string()
}

/// subprocess.Output: stdout trimmed of spaces and newlines, or git-lfs's error text.
pub fn output(name: &str, args: &[&str], dir: Option<&str>) -> Result<String> {
    let mut c = command(name, args);
    if let Some(d) = dir {
        c.current_dir(d);
    }
    c.stdin(Stdio::null());
    let out = c.output().map_err(|e| Error::new(format!("exec: {}: {}", name, crate::tools::io_err(&e))))?;
    if !out.status.success() {
        let mut eo = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if eo.is_empty() {
            eo = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        let ran = if args.is_empty() { look_path(name) } else { format!("{} {}", look_path(name), quoted_args(args)) };
        let mut e = Error::new(format!("error running {}: '{}' '{}'", ran, eo, exit_text(&out.status)));
        e.exit_status = out.status.code();
        return Err(e);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_matches([' ', '\n']).to_string())
}

/// exec.ExitError's text.
pub fn exit_text(s: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(sig)) => format!("signal: {}", signal_name(sig)),
        _ => "exit status -1".into(),
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGKILL => "killed".into(),
        libc::SIGTERM => "terminated".into(),
        libc::SIGINT => "interrupt".into(),
        libc::SIGPIPE => "broken pipe".into(),
        libc::SIGSEGV => "segmentation fault".into(),
        libc::SIGABRT => "aborted".into(),
        n => format!("signal {n}"),
    }
}

/// ShellQuoteSingle.
pub fn shell_quote_single(s: &str) -> String {
    let ok = !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_@/.-".contains(&c));
    if ok {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// FormatPercentSequences: `%f` and friends replaced by shell-quoted values, `%%` by `%`.
pub fn format_percent_sequences(pattern: &str, repl: &[(char, &str)]) -> String {
    let mut s = String::new();
    let mut pct = false;
    for c in pattern.chars() {
        if !pct && c == '%' {
            pct = true;
            continue;
        }
        if pct {
            pct = false;
            if c == '%' {
                s.push('%');
            } else if let Some((_, v)) = repl.iter().find(|(k, _)| *k == c) {
                s.push_str(&shell_quote_single(v));
            }
        } else {
            s.push(c);
        }
    }
    s
}
