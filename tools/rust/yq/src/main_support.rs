//! Process plumbing: Go-style I/O error texts and running a command (system operator).

/// The text Go's os package uses for an I/O error (syscall.Errno's string).
pub fn io_err(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let s = std::io::Error::from_raw_os_error(code).to_string();
            // Rust: "No such file or directory (os error 2)"; Go: "no such file or directory"
            let s = s.split(" (os error").next().unwrap_or(&s).to_string();
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                None => s,
            }
        }
        None => e.to_string(),
    }
}

/// exec.Command(cmd, args...).Output() with stdin: stdout, or (error text, stderr).
pub fn run_command(cmd: &str, args: &[String], stdin: &[u8]) -> Result<Vec<u8>, (String, Vec<u8>)> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (format!("exec: \"{cmd}\": {}", spawn_err(&e)), Vec::new()))?;
    let mut si = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = si.write_all(&data);
    });
    let out = child.wait_with_output().map_err(|e| (e.to_string(), Vec::new()))?;
    let _ = writer.join();
    if !out.status.success() {
        let msg = match out.status.code() {
            Some(c) => format!("exit status {c}"),
            None => "signal: killed".to_string(),
        };
        return Err((msg, out.stderr));
    }
    Ok(out.stdout)
}

fn spawn_err(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "executable file not found in $PATH".into()
    } else {
        io_err(e)
    }
}
