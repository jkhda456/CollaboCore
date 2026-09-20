//! The host side of `collabo-agentd`: runs one command in the guest over a vsock connection.
//!
//! One connection per command, frames of type (1 byte) + length (u32 little endian) + payload:
//!
//!   host -> guest   'A' argument, 'E' "NAME=value", 'C' working directory, 'S' start,
//!                   'I' stdin bytes (an empty 'I' closes it), 'K' signal number (u32)
//!   guest -> host   '1' stdout, '2' stderr, 'X' exit status (i32, negative = killed by -n),
//!                   'F' the command could not be started
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::vsock::{Vsock, VsockStream};

pub const AGENT_PORT: u32 = 1024;

/// What one command asks for.
pub struct Command {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub stdin: Vec<u8>,
}

/// How it ended: an exit code, or the signal that killed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    /// The status a shell would report.
    pub fn status(self) -> i32 {
        match self {
            Exit::Code(code) => code,
            Exit::Signal(signal) => 128 + signal,
        }
    }
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(5 + payload.len());
    bytes.push(kind);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// Connects to the guest's agent, which only starts listening once the guest has booted.
pub fn connect(vsock: &Vsock, timeout: Duration) -> Result<VsockStream> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match vsock.connect(AGENT_PORT, Duration::from_millis(500)) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last = Some(error);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    match last {
        Some(error) => bail!("the guest agent did not answer on vsock port {AGENT_PORT}: {error}"),
        None => bail!("the guest agent did not answer on vsock port {AGENT_PORT}"),
    }
}

/// Runs one command to completion, handing its output to the callbacks as it arrives. When
/// `timeout` passes the command is killed and the result says so; its output up to then is
/// still delivered.
pub fn run(
    stream: &VsockStream,
    command: &Command,
    timeout: Option<Duration>,
    mut on_stdout: impl FnMut(&[u8]),
    mut on_stderr: impl FnMut(&[u8]),
) -> Result<(Exit, bool)> {
    if command.argv.is_empty() {
        bail!("a command needs at least a program name");
    }
    for argument in &command.argv {
        stream.write_all(&frame(b'A', argument.as_bytes()))?;
    }
    for (name, value) in &command.env {
        stream.write_all(&frame(b'E', format!("{name}={value}").as_bytes()))?;
    }
    if let Some(cwd) = &command.cwd {
        stream.write_all(&frame(b'C', cwd.as_bytes()))?;
    }
    stream.write_all(&frame(b'S', &[]))?;
    for chunk in command.stdin.chunks(32 * 1024) {
        stream.write_all(&frame(b'I', chunk))?;
    }
    stream.write_all(&frame(b'I', &[]))?; // end of stdin

    // After the kill the agent still reports how the command ended, so reading continues.
    let mut killed = false;
    stream.set_deadline(timeout.map(|timeout| Instant::now() + timeout));
    loop {
        let header = match stream.read_exact(5) {
            Ok(header) => header,
            Err(error) if error.downcast_ref::<crate::vsock::ReadTimeout>().is_some() && !killed => {
                killed = true;
                stream.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
                stream.write_all(&frame(b'K', &9u32.to_le_bytes()))?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if header.len() < 5 {
            bail!("the guest agent closed the connection before the command ended");
        }
        let length = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let payload = stream.read_exact(length)?;
        if payload.len() < length {
            bail!("the guest agent closed the connection mid-frame");
        }
        match header[0] {
            b'1' => on_stdout(&payload),
            b'2' => on_stderr(&payload),
            b'X' => {
                let status = i32::from_le_bytes(payload.get(..4).unwrap_or(&[0; 4]).try_into().unwrap());
                let exit = if status < 0 { Exit::Signal(-status) } else { Exit::Code(status) };
                return Ok((exit, killed));
            }
            b'F' => bail!("{}", String::from_utf8_lossy(&payload)),
            kind => bail!("unknown frame '{}' from the guest agent", kind as char),
        }
    }
}
