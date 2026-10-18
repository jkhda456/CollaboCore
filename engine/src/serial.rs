//! The host's serial ports, lent to the guest (vsock port 1083, the c-sh add-on's `serial:`
//! targets): USB serial adapters (`/dev/ttyUSB*`, `/dev/ttyACM*`, `COM3`), on-board UARTs, any
//! tty device the app names.
//!
//! The app decides, as for the ssh-agent: `serial` off (the default: no device is reachable),
//! `allow` (the devices its list names, or any serial device when the list is empty), or `ask`
//! (the listed ones, and the app is asked about each other one as it is opened; an answer with
//! remember holds for that device for the session). A device is named by its path (a glob:
//! `/dev/ttyUSB*`, `COM3`) or by its USB identity, `usb:VID:PID[:SERIAL]` in hex, which stays the
//! same when the adapter is plugged in elsewhere. "Any" (an empty list, or `*`) means the serial
//! ports the system lists (ttys with hardware behind them; macOS's cu.*; Windows' COM ports), never
//! a terminal or a pty — those only when the app names them. Whatever the policy, only character
//! devices that are terminals are opened (Windows: COM ports), so the bridge reaches nothing else.
//!
//! The protocol: the guest sends one JSON line, `{"op":"list"}` (answered with the devices and
//! whether each may be opened) or `{"op":"open","device":…,"baud":…,"dataBits":8,"parity":"none",
//! "stopBits":1,"flow":"none"}` (answered `{"ok":true,"device":{…}}` or `{"ok":false,"error":…}`).
//! After an open the connection carries frames: a kind byte, a big-endian u32 length, the payload.
//! Kind 0 is data, either way; kind 1 is a JSON control message — from the guest
//! `{"op":"break","ms":N}`, `{"op":"set","dtr":B,"rts":B}`, `{"op":"config",…}`,
//! `{"op":"status"}`, each answered with one control frame, and `{"op":"close"}`, answered once
//! the host's port is closed (so the guest may open it again at once); from the host also
//! `{"event":"closed","reason":…}` when the device goes away. Other kinds are left for other
//! devices (USB) on the same port.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::vsock::{Vsock, VsockStream};

pub const PORT: u32 = 1083;

/// The bus of a device the system does not list as a serial port.
const UNLISTED: &str = "unlisted";

const FRAME_DATA: u8 = 0;
const FRAME_CONTROL: u8 = 1;
const MAX_FRAME: usize = 64 * 1024;
const MAX_REQUEST: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Off,
    Ask,
    Allow,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "off" => Some(Mode::Off),
            "ask" => Some(Mode::Ask),
            "allow" => Some(Mode::Allow),
            _ => None,
        }
    }
}

/// The app's `serial` settings. Swappable while the guest runs.
#[derive(Clone, Debug)]
pub struct Policy {
    pub mode: Mode,
    /// Device patterns: a path glob, `usb:VID:PID[:SERIAL]`, or `*`.
    pub devices: Vec<String>,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { mode: Mode::Off, devices: Vec::new() }
    }
}

/// What the app hears about: a listing, an open (allowed or not), a close.
pub struct Event {
    pub op: &'static str,
    pub device: String,
    pub allowed: bool,
    pub reason: Option<String>,
}

/// Asks the app about one device: target text → (allow, remember).
pub type Asker = Arc<dyn Fn(&str) -> Option<(bool, bool)> + Send + Sync>;

/// The app's remembered answers, by device path (cleared when the policy changes).
pub type Answers = Arc<Mutex<HashMap<String, bool>>>;

/// A USB device's identity, from the system.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usb {
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub interface: Option<u8>,
}

/// A serial device the host has.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    /// What to open: /dev/ttyUSB0, /dev/cu.usbserial-1410, COM3.
    pub path: String,
    /// The stable name some systems also give it (Linux: /dev/serial/by-id/...).
    pub by_id: Option<String>,
    /// usb, pci, platform (an on-board port), or the bus the system names.
    pub bus: String,
    /// The kernel's driver, when known (ftdi_sio, cdc_acm, cp210x, ...).
    pub driver: Option<String>,
    pub usb: Option<Usb>,
}

impl Device {
    /// One line for the app's question and the logs.
    pub fn describe(&self) -> String {
        let mut text = self.path.clone();
        if let Some(usb) = &self.usb {
            text.push_str(&format!(" (USB {:04x}:{:04x}", usb.vid, usb.pid));
            for part in [&usb.manufacturer, &usb.product].into_iter().flatten() {
                text.push(' ');
                text.push_str(part);
            }
            if let Some(serial) = &usb.serial {
                text.push_str(&format!(", serial {serial}"));
            }
            text.push(')');
        }
        if self.bus == UNLISTED {
            text.push_str(" (not a serial port this computer lists: a terminal or a pty)");
        }
        text
    }

    fn to_json(&self) -> Value {
        let mut v = json!({"path": self.path, "bus": self.bus});
        if let Some(by_id) = &self.by_id {
            v["byId"] = json!(by_id);
        }
        if let Some(driver) = &self.driver {
            v["driver"] = json!(driver);
        }
        if let Some(usb) = &self.usb {
            v["usb"] = json!({
                "vid": format!("{:04x}", usb.vid),
                "pid": format!("{:04x}", usb.pid),
                "serial": usb.serial,
                "manufacturer": usb.manufacturer,
                "product": usb.product,
                "interface": usb.interface,
            });
        }
        v
    }

    /// For a path the listing did not find (a pty, a terminal, a device without sysfs details):
    /// opened only when the app names it.
    fn bare(path: &str) -> Device {
        Device { path: path.to_string(), by_id: None, bus: UNLISTED.into(), driver: None, usb: None }
    }
}

// --- Patterns and decisions ------------------------------------------------------------------

/// `*` and `?` wildcards (paths are case-sensitive except on Windows, whose COM names are not).
fn glob(pattern: &str, text: &str) -> bool {
    let fold = cfg!(windows);
    let p: Vec<char> = if fold { pattern.to_lowercase().chars().collect() } else { pattern.chars().collect() };
    let t: Vec<char> = if fold { text.to_lowercase().chars().collect() } else { text.chars().collect() };
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `usb:VID:PID[:SERIAL]` as (vid, pid, serial).
fn usb_pattern(pattern: &str) -> Option<(u16, u16, Option<&str>)> {
    let rest = pattern.strip_prefix("usb:")?;
    let mut parts = rest.splitn(3, ':');
    let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
    let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
    Some((vid, pid, parts.next().filter(|s| !s.is_empty())))
}

/// Does one pattern name this device (its path, its stable name, or its USB identity)?
pub fn matches(pattern: &str, device: &Device) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some((vid, pid, serial)) = usb_pattern(pattern) {
        return device.usb.as_ref().is_some_and(|usb| {
            usb.vid == vid && usb.pid == pid && serial.is_none_or(|s| usb.serial.as_deref() == Some(s))
        });
    }
    glob(pattern, &device.path) || device.by_id.as_deref().is_some_and(|b| glob(pattern, b))
}

enum Decision {
    Allowed,
    Ask,
    Denied(String),
}

fn decide(policy: &Policy, device: &Device) -> Decision {
    // A device the system does not list (a terminal, a pty) counts only when a pattern other than
    // "any" names it: the user's own terminal must never be "any serial port".
    let unlisted = device.bus == UNLISTED;
    let listed = policy.devices.iter().any(|p| (!unlisted || p != "*") && matches(p, device));
    let any = policy.devices.is_empty() || policy.devices.iter().any(|p| p == "*");
    match policy.mode {
        Mode::Off => Decision::Denied("the app does not share serial ports (serial: off)".into()),
        Mode::Allow if listed || (any && !unlisted) => Decision::Allowed,
        Mode::Allow if any && unlisted => Decision::Denied(format!(
            "{} is not a serial port this computer lists (a terminal or a pty is shared only when the app names it)",
            device.path
        )),
        Mode::Allow => Decision::Denied(format!("{} is not among the serial ports the app shares ({})", device.path, policy.devices.join(", "))),
        Mode::Ask if listed => Decision::Allowed,
        Mode::Ask => Decision::Ask,
    }
}

// --- Line settings ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Parity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Flow {
    None,
    RtsCts,
    XonXoff,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Line {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub flow: Flow,
}

impl Default for Line {
    fn default() -> Line {
        Line { baud: 115_200, data_bits: 8, parity: Parity::None, stop_bits: 1, flow: Flow::None }
    }
}

impl Line {
    /// The settings in a request, over `base` (what is not given stays as it was).
    fn from_json(v: &Value, base: Line) -> Result<Line> {
        let mut line = base;
        if let Some(b) = v.get("baud") {
            line.baud = b.as_u64().filter(|b| (50..=16_000_000).contains(b)).context("baud: a rate from 50 to 16000000")? as u32;
        }
        if let Some(d) = v.get("dataBits") {
            line.data_bits = d.as_u64().filter(|d| (5..=8).contains(d)).context("dataBits: 5 to 8")? as u8;
        }
        if let Some(s) = v.get("stopBits") {
            line.stop_bits = s.as_u64().filter(|s| *s == 1 || *s == 2).context("stopBits: 1 or 2")? as u8;
        }
        if let Some(p) = v.get("parity") {
            line.parity = match p.as_str() {
                Some("none") | Some("n") => Parity::None,
                Some("even") | Some("e") => Parity::Even,
                Some("odd") | Some("o") => Parity::Odd,
                Some("mark") | Some("m") => Parity::Mark,
                Some("space") | Some("s") => Parity::Space,
                _ => bail!("parity: none, even, odd, mark or space"),
            };
        }
        if let Some(f) = v.get("flow") {
            line.flow = match f.as_str() {
                Some("none") => Flow::None,
                Some("rtscts") | Some("hardware") => Flow::RtsCts,
                Some("xonxoff") | Some("software") => Flow::XonXoff,
                _ => bail!("flow: none, rtscts or xonxoff"),
            };
        }
        Ok(line)
    }

    fn to_json(self) -> Value {
        json!({
            "baud": self.baud,
            "dataBits": self.data_bits,
            "parity": match self.parity { Parity::None => "none", Parity::Even => "even", Parity::Odd => "odd", Parity::Mark => "mark", Parity::Space => "space" },
            "stopBits": self.stop_bits,
            "flow": match self.flow { Flow::None => "none", Flow::RtsCts => "rtscts", Flow::XonXoff => "xonxoff" },
        })
    }
}

/// The modem lines' state.
#[derive(Clone, Copy, Debug, Default)]
pub struct ModemStatus {
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

// --- Serving ---------------------------------------------------------------------------------

pub fn serve(
    vsock: &Vsock,
    policy: Arc<RwLock<Policy>>,
    answers: Answers,
    asker: Asker,
    observer: Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    vsock.listen(PORT, move |stream| {
        let (policy, answers, asker, observer) = (policy.clone(), answers.clone(), asker.clone(), observer.clone());
        std::thread::spawn(move || {
            let stream = Arc::new(stream);
            if let Err(error) = session(&stream, &policy, &answers, &asker, &observer) {
                let _ = send_line(&stream, &json!({"ok": false, "error": format!("{error:#}")}));
            }
            stream.close();
        });
    })
}

fn send_line(stream: &VsockStream, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line)
}

fn read_line(stream: &VsockStream) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        if byte[0] == b'\n' {
            return Ok(Some(line));
        }
        line.push(byte[0]);
        if line.len() > MAX_REQUEST {
            bail!("the request is too long");
        }
    }
}

fn session(
    stream: &Arc<VsockStream>,
    policy: &Arc<RwLock<Policy>>,
    answers: &Answers,
    asker: &Asker,
    observer: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    let Some(line) = read_line(stream)? else { return Ok(()) };
    let request: Value = serde_json::from_slice(&line).context("the request is not JSON")?;
    let policy_now = policy.read().unwrap().clone();
    match request.get("op").and_then(Value::as_str) {
        Some("list") => {
            let devices: Vec<Value> = list_devices()
                .iter()
                .map(|d| {
                    let mut v = d.to_json();
                    v["access"] = json!(match decide(&policy_now, d) {
                        Decision::Allowed => "allowed",
                        Decision::Ask => "ask",
                        Decision::Denied(_) => "denied",
                    });
                    v
                })
                .collect();
            observer(Event { op: "list", device: String::new(), allowed: policy_now.mode != Mode::Off, reason: None });
            send_line(stream, &json!({"ok": true, "mode": format!("{:?}", policy_now.mode).to_lowercase(), "devices": devices}))
        }
        Some("open") => {
            let wanted = request.get("device").and_then(Value::as_str).context("open needs a device")?;
            let line = Line::from_json(&request, Line::default())?;
            let device = match resolve(wanted) {
                Ok(device) => device,
                Err(error) => {
                    observer(Event { op: "open", device: wanted.into(), allowed: false, reason: Some(format!("{error:#}")) });
                    return Err(error);
                }
            };
            let refuse = |reason: String| -> Result<()> {
                observer(Event { op: "open", device: device.path.clone(), allowed: false, reason: Some(reason.clone()) });
                bail!(reason)
            };
            match decide(&policy_now, &device) {
                Decision::Allowed => {}
                Decision::Denied(reason) => return refuse(reason),
                Decision::Ask => {
                    let known = answers.lock().unwrap().get(&device.path).copied();
                    let allow = match known {
                        Some(answer) => answer,
                        None => {
                            let (allow, remember) = asker(&format!("open the serial port {}", device.describe())).unwrap_or((false, false));
                            if remember {
                                answers.lock().unwrap().insert(device.path.clone(), allow);
                            }
                            allow
                        }
                    };
                    if !allow {
                        return refuse(format!("the app did not allow {}", device.path));
                    }
                }
            }
            // A port the guest just let go of may still be closing (its connection ends a moment
            // after the guest's program does): busy is retried for a second.
            let mut attempt = 0;
            let port = loop {
                match platform::Port::open(&device.path, line) {
                    Ok(port) => break Arc::new(port),
                    // (Windows says an open COM port is access denied.)
                    Err(error) if attempt < 10 && { let e = format!("{error:#}"); e.contains("busy") || (cfg!(windows) && e.contains("denied")) } => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(error) => return refuse(format!("{error:#}")),
                }
            };
            observer(Event { op: "open", device: device.path.clone(), allowed: true, reason: None });
            send_line(stream, &json!({"ok": true, "device": device.to_json(), "line": line.to_json()}))?;
            let (reason, asked_to_close) = pump(stream, &port, line);
            // The reader has ended: this is the last reference, and dropping it closes the port.
            drop(port);
            observer(Event { op: "close", device: device.path.clone(), allowed: true, reason });
            if asked_to_close {
                let _ = send_frame(stream, FRAME_CONTROL, json!({"ok": true, "closed": true}).to_string().as_bytes());
            }
            Ok(())
        }
        other => bail!("unknown op {other:?} (list, open)"),
    }
}

/// Finds the device a request names: a path (followed through links), or a USB identity.
fn resolve(wanted: &str) -> Result<Device> {
    let devices = list_devices();
    if usb_pattern(wanted).is_some() {
        let found: Vec<&Device> = devices.iter().filter(|d| matches(wanted, d)).collect();
        return match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no serial device {wanted} is plugged in"),
            many => bail!(
                "{} devices match {wanted} ({}); add the serial number",
                many.len(),
                many.iter().map(|d| d.path.as_str()).collect::<Vec<_>>().join(", ")
            ),
        };
    }
    let canonical = platform::canonical(wanted);
    if let Some(d) = devices.iter().find(|d| d.path == canonical || d.path == wanted || d.by_id.as_deref() == Some(wanted)) {
        return Ok(d.clone());
    }
    // A tty the listing does not show (a pty, a device without sysfs details): its own path.
    Ok(Device::bare(&canonical))
}

/// Carries bytes both ways until either side ends. Returns why it ended, if not by the guest, and
/// whether the guest asked to close (and waits to hear that it is closed).
fn pump(stream: &Arc<VsockStream>, port: &Arc<platform::Port>, line: Line) -> (Option<String>, bool) {
    let writer = Arc::new(Mutex::new(()));
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Device -> guest.
    let reader = {
        let (stream, port, writer, running) = (stream.clone(), port.clone(), writer.clone(), running.clone());
        std::thread::spawn(move || -> Option<String> {
            let mut buf = vec![0u8; 16 * 1024];
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                match port.read(&mut buf, 200) {
                    Ok(0) => continue,
                    Ok(n) => {
                        let _guard = writer.lock().unwrap();
                        if send_frame(&stream, FRAME_DATA, &buf[..n]).is_err() {
                            return None;
                        }
                    }
                    Err(error) => {
                        let reason = format!("the serial port went away: {error}");
                        let _guard = writer.lock().unwrap();
                        let _ = send_frame(&stream, FRAME_CONTROL, json!({"event": "closed", "reason": reason}).to_string().as_bytes());
                        stream.close();
                        return Some(reason);
                    }
                }
            }
            None
        })
    };
    // Guest -> device, and the guest's control messages.
    let mut line = line;
    let mut asked_to_close = false;
    loop {
        let Ok(Some((kind, payload))) = read_frame(stream) else { break };
        match kind {
            FRAME_DATA => {
                if port.write_all(&payload).is_err() {
                    break;
                }
            }
            FRAME_CONTROL => {
                if serde_json::from_slice::<Value>(&payload).ok().and_then(|v| v.get("op").and_then(Value::as_str).map(str::to_string)).as_deref() == Some("close") {
                    asked_to_close = true;
                    break;
                }
                let answer = control(port, &mut line, &payload);
                let _guard = writer.lock().unwrap();
                if send_frame(stream, FRAME_CONTROL, answer.to_string().as_bytes()).is_err() {
                    break;
                }
            }
            _ => {
                let _guard = writer.lock().unwrap();
                let _ = send_frame(stream, FRAME_CONTROL, json!({"ok": false, "error": format!("unknown frame kind {kind}")}).to_string().as_bytes());
            }
        }
    }
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    (reader.join().ok().flatten(), asked_to_close)
}

fn control(port: &platform::Port, line: &mut Line, payload: &[u8]) -> Value {
    let request: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return json!({"ok": false, "error": "a control message is JSON"}),
    };
    let result = (|| -> Result<Value> {
        match request.get("op").and_then(Value::as_str) {
            Some("break") => {
                let ms = request.get("ms").and_then(Value::as_u64).unwrap_or(250).clamp(1, 10_000);
                port.send_break(ms as u32)?;
                Ok(json!({"ok": true}))
            }
            Some("set") => {
                if let Some(dtr) = request.get("dtr").and_then(Value::as_bool) {
                    port.set_dtr(dtr)?;
                }
                if let Some(rts) = request.get("rts").and_then(Value::as_bool) {
                    port.set_rts(rts)?;
                }
                Ok(json!({"ok": true}))
            }
            Some("config") => {
                let new = Line::from_json(&request, *line)?;
                port.configure(new)?;
                *line = new;
                Ok(json!({"ok": true, "line": line.to_json()}))
            }
            Some("status") => {
                let s = port.modem_status()?;
                Ok(json!({"ok": true, "line": line.to_json(), "modem": {"cts": s.cts, "dsr": s.dsr, "dcd": s.dcd, "ri": s.ri}}))
            }
            other => bail!("unknown control op {other:?} (break, set, config, status)"),
        }
    })();
    result.unwrap_or_else(|error| json!({"ok": false, "error": format!("{error:#}")}))
}

fn send_frame(stream: &VsockStream, kind: u8, payload: &[u8]) -> Result<()> {
    for chunk in payload.chunks(MAX_FRAME).chain(payload.is_empty().then_some(&[][..])) {
        let mut frame = Vec::with_capacity(5 + chunk.len());
        frame.push(kind);
        frame.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        frame.extend_from_slice(chunk);
        stream.write_all(&frame)?;
    }
    Ok(())
}

fn read_frame(stream: &VsockStream) -> Result<Option<(u8, Vec<u8>)>> {
    let head = stream.read_exact(5)?;
    if head.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        bail!("a {len}-byte frame");
    }
    let payload = stream.read_exact(len)?;
    if payload.len() < len {
        return Ok(None);
    }
    Ok(Some((head[0], payload)))
}

// --- Listing the devices ---------------------------------------------------------------------

/// The serial devices this computer has now.
pub fn list_devices() -> Vec<Device> {
    platform::list()
}

/// Linux: /sys/class/tty, with each device's bus, driver and USB identity, and the stable names
/// in /dev/serial/by-id. `sys` and `dev` are the roots (the tests give a tree of their own).
#[cfg(any(target_os = "linux", test))]
pub fn list_linux(sys: &std::path::Path, dev: &std::path::Path) -> Vec<Device> {
    let read = |p: &std::path::Path| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut by_id: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dev.join("serial/by-id")) {
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                    by_id.insert(name.to_string(), dev.join("serial/by-id").join(entry.file_name()).to_string_lossy().into_owned());
                }
            }
        }
    }
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sys.join("class/tty")) else { return out };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let tty = entry.path();
        // Only ttys with hardware behind them; virtual consoles and ptys have no `device`.
        let Ok(device_dir) = std::fs::canonicalize(tty.join("device")) else { continue };
        // A legacy 8250 port the board does not have reports UART type 0.
        if name.starts_with("ttyS") && read(&tty.join("type")).as_deref() == Some("0") {
            continue;
        }
        let driver = std::fs::read_link(device_dir.join("driver")).ok().and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()));
        let subsystem = std::fs::read_link(device_dir.join("subsystem")).ok().and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()));
        // The USB device is an ancestor with idVendor / idProduct; the interface is in between.
        let mut usb = None;
        let mut interface = None;
        let mut dir = Some(device_dir.as_path());
        while let Some(d) = dir {
            if interface.is_none() {
                if let Some(n) = read(&d.join("bInterfaceNumber")).and_then(|n| u8::from_str_radix(&n, 16).ok()) {
                    interface = Some(n);
                }
            }
            if let (Some(v), Some(p)) = (read(&d.join("idVendor")), read(&d.join("idProduct"))) {
                if let (Ok(vid), Ok(pid)) = (u16::from_str_radix(&v, 16), u16::from_str_radix(&p, 16)) {
                    usb = Some(Usb {
                        vid,
                        pid,
                        serial: read(&d.join("serial")),
                        manufacturer: read(&d.join("manufacturer")),
                        product: read(&d.join("product")),
                        interface,
                    });
                }
                break;
            }
            if d == sys {
                break;
            }
            dir = d.parent();
        }
        let bus = if usb.is_some() {
            "usb".to_string()
        } else {
            subsystem.clone().unwrap_or_else(|| "unknown".into())
        };
        out.push(Device { path: dev.join(&name).to_string_lossy().into_owned(), by_id: by_id.get(&name).cloned(), bus, driver, usb });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(unix)]
mod platform {
    use super::{Device, Flow, Line, ModemStatus, Parity};
    use anyhow::{bail, Context, Result};
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub fn list() -> Vec<Device> {
        #[cfg(target_os = "linux")]
        {
            super::list_linux(std::path::Path::new("/sys"), std::path::Path::new("/dev"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS and the BSDs: the call-out devices (cu.*), which do not wait for carrier.
            let mut out: Vec<Device> = std::fs::read_dir("/dev")
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.file_name().to_str().map(str::to_string))
                        .filter(|n| n.starts_with("cu.") && n != "cu.Bluetooth-Incoming-Port")
                        .map(|n| Device {
                            path: format!("/dev/{n}"),
                            by_id: None,
                            bus: if n.contains("usb") { "usb".into() } else { "unknown".into() },
                            driver: None,
                            usb: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.path.cmp(&b.path));
            out
        }
    }

    pub fn canonical(path: &str) -> String {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
    }

    pub struct Port {
        fd: OwnedFd,
    }

    fn os_error(what: &str) -> anyhow::Error {
        anyhow::anyhow!("{what}: {}", std::io::Error::last_os_error())
    }

    impl Port {
        pub fn open(path: &str, line: Line) -> Result<Port> {
            let c = CString::new(path).context("a NUL in the device path")?;
            // SAFETY: open with O_NOCTTY (never our controlling terminal) and O_NONBLOCK (no
            // wait for carrier); blocking is restored below.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(os_error(&format!("cannot open {path}")));
            }
            // SAFETY: fd is a valid descriptor we now own.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // Only a terminal device: the bridge must not become a way to any other file.
            // SAFETY: fstat into a zeroed stat; isatty on an open fd.
            unsafe {
                let mut st: libc::stat = std::mem::zeroed();
                if libc::fstat(fd.as_raw_fd(), &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFCHR || libc::isatty(fd.as_raw_fd()) != 1 {
                    bail!("{path} is not a serial device");
                }
                // Exclusive: a second open (another program here) fails while the guest has it.
                libc::ioctl(fd.as_raw_fd(), libc::TIOCEXCL as _);
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
            let port = Port { fd };
            port.configure(line)?;
            Ok(port)
        }

        pub fn configure(&self, line: Line) -> Result<()> {
            // SAFETY: tcgetattr/tcsetattr on our tty, the struct filled by the first.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(self.fd.as_raw_fd(), &mut t) != 0 {
                    return Err(os_error("tcgetattr"));
                }
                libc::cfmakeraw(&mut t);
                t.c_cflag |= libc::CLOCAL | libc::CREAD;
                t.c_cflag &= !libc::CSIZE;
                t.c_cflag |= match line.data_bits {
                    5 => libc::CS5,
                    6 => libc::CS6,
                    7 => libc::CS7,
                    _ => libc::CS8,
                };
                if line.stop_bits == 2 {
                    t.c_cflag |= libc::CSTOPB;
                } else {
                    t.c_cflag &= !libc::CSTOPB;
                }
                t.c_cflag &= !(libc::PARENB | libc::PARODD);
                #[cfg(target_os = "linux")]
                {
                    t.c_cflag &= !libc::CMSPAR;
                }
                match line.parity {
                    Parity::None => {}
                    Parity::Even => t.c_cflag |= libc::PARENB,
                    Parity::Odd => t.c_cflag |= libc::PARENB | libc::PARODD,
                    #[cfg(target_os = "linux")]
                    Parity::Mark => t.c_cflag |= libc::PARENB | libc::PARODD | libc::CMSPAR,
                    #[cfg(target_os = "linux")]
                    Parity::Space => t.c_cflag |= libc::PARENB | libc::CMSPAR,
                    #[cfg(not(target_os = "linux"))]
                    Parity::Mark | Parity::Space => bail!("mark and space parity are not available on this system"),
                }
                t.c_cflag &= !libc::CRTSCTS;
                t.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
                match line.flow {
                    Flow::None => {}
                    Flow::RtsCts => t.c_cflag |= libc::CRTSCTS,
                    Flow::XonXoff => t.c_iflag |= libc::IXON | libc::IXOFF,
                }
                t.c_cc[libc::VMIN] = 1;
                t.c_cc[libc::VTIME] = 0;
                set_speed(&mut t, line.baud)?;
                if libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &t) != 0 {
                    return Err(os_error("configuring the serial port"));
                }
            }
            Ok(())
        }

        /// Reads what arrives within `timeout_ms` (0 bytes: nothing yet).
        pub fn read(&self, buf: &mut [u8], timeout_ms: i32) -> std::io::Result<usize> {
            let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd; read into buf within its length.
            unsafe {
                let n = libc::poll(&mut p, 1, timeout_ms);
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    return if e.kind() == std::io::ErrorKind::Interrupted { Ok(0) } else { Err(e) };
                }
                if n == 0 {
                    return Ok(0);
                }
                if p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && p.revents & libc::POLLIN == 0 {
                    return Err(std::io::Error::other("hung up"));
                }
                let r = libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                if r < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if r == 0 {
                    return Err(std::io::Error::other("end of file"));
                }
                Ok(r as usize)
            }
        }

        pub fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
            while !data.is_empty() {
                // SAFETY: data is readable for its length.
                let n = unsafe { libc::write(self.fd.as_raw_fd(), data.as_ptr() as *const libc::c_void, data.len()) };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                data = &data[n as usize..];
            }
            Ok(())
        }

        pub fn send_break(&self, ms: u32) -> Result<()> {
            // SAFETY: break on our tty for the time asked, then clear it.
            unsafe {
                if libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSBRK as _) != 0 {
                    // Not every tty can hold a break; tcsendbreak sends the default one.
                    if libc::tcsendbreak(self.fd.as_raw_fd(), 0) != 0 {
                        return Err(os_error("sending a break"));
                    }
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                libc::ioctl(self.fd.as_raw_fd(), libc::TIOCCBRK as _);
            }
            Ok(())
        }

        fn modem_bits(&self, bits: libc::c_int, on: bool) -> Result<()> {
            let request = if on { libc::TIOCMBIS } else { libc::TIOCMBIC };
            // SAFETY: the modem bits by reference, as the ioctl wants.
            if unsafe { libc::ioctl(self.fd.as_raw_fd(), request as _, &bits) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ENOTTY) || e.raw_os_error() == Some(libc::EINVAL) {
                    bail!("this device has no modem lines (DTR, RTS)");
                }
                return Err(os_error("setting the modem lines"));
            }
            Ok(())
        }

        pub fn set_dtr(&self, on: bool) -> Result<()> {
            self.modem_bits(libc::TIOCM_DTR, on)
        }

        pub fn set_rts(&self, on: bool) -> Result<()> {
            self.modem_bits(libc::TIOCM_RTS, on)
        }

        pub fn modem_status(&self) -> Result<ModemStatus> {
            let mut bits: libc::c_int = 0;
            // SAFETY: TIOCMGET writes the bits.
            if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCMGET as _, &mut bits) } != 0 {
                // A pty has no modem lines.
                return Ok(ModemStatus::default());
            }
            Ok(ModemStatus {
                cts: bits & libc::TIOCM_CTS != 0,
                dsr: bits & libc::TIOCM_DSR != 0,
                dcd: bits & libc::TIOCM_CAR != 0,
                ri: bits & libc::TIOCM_RNG != 0,
            })
        }
    }

    impl Drop for Port {
        fn drop(&mut self) {
            // The exclusive mode is a flag of the tty, which outlives this descriptor while any
            // other program has the device open: give it back, or no one could open it after us.
            // SAFETY: an ioctl on our own descriptor, closed right after (OwnedFd).
            unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCNXCL as _) };
        }
    }

    /// The line speed: the standard rates on Linux (termios' B constants); macOS and the BSDs
    /// take the number itself.
    fn set_speed(t: &mut libc::termios, baud: u32) -> Result<()> {
        #[cfg(target_os = "linux")]
        let speed = match baud {
            50 => libc::B50,
            75 => libc::B75,
            110 => libc::B110,
            134 => libc::B134,
            150 => libc::B150,
            200 => libc::B200,
            300 => libc::B300,
            600 => libc::B600,
            1200 => libc::B1200,
            1800 => libc::B1800,
            2400 => libc::B2400,
            4800 => libc::B4800,
            9600 => libc::B9600,
            19200 => libc::B19200,
            38400 => libc::B38400,
            57600 => libc::B57600,
            115200 => libc::B115200,
            230400 => libc::B230400,
            460800 => libc::B460800,
            500000 => libc::B500000,
            576000 => libc::B576000,
            921600 => libc::B921600,
            1000000 => libc::B1000000,
            1152000 => libc::B1152000,
            1500000 => libc::B1500000,
            2000000 => libc::B2000000,
            2500000 => libc::B2500000,
            3000000 => libc::B3000000,
            3500000 => libc::B3500000,
            4000000 => libc::B4000000,
            other => bail!("{other} baud is not a standard rate here (115200, 230400, 460800, 921600, ...)"),
        };
        #[cfg(not(target_os = "linux"))]
        let speed = baud as libc::speed_t;
        // SAFETY: setting the speed fields of a valid termios.
        unsafe {
            if libc::cfsetispeed(t, speed) != 0 || libc::cfsetospeed(t, speed) != 0 {
                bail!("{baud} baud is not supported");
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::{Device, Flow, Line, ModemStatus, Parity};
    use anyhow::{bail, Result};
    use windows_sys::Win32::Devices::Communication::*;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The COM ports Windows has: HKLM\HARDWARE\DEVICEMAP\SERIALCOMM.
    pub fn list() -> Vec<Device> {
        let mut out = Vec::new();
        // SAFETY: a registry key opened for reading, its values enumerated into our buffers.
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide("HARDWARE\\DEVICEMAP\\SERIALCOMM").as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return out;
            }
            for index in 0.. {
                let mut name = [0u16; 256];
                let mut name_len = name.len() as u32;
                let mut data = [0u16; 64];
                let mut data_len = (data.len() * 2) as u32;
                let mut kind = 0u32;
                let r = RegEnumValueW(key, index, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind, data.as_mut_ptr() as *mut u8, &mut data_len);
                if r != 0 {
                    break;
                }
                let port = String::from_utf16_lossy(&data[..(data_len as usize / 2)]).trim_end_matches('\0').to_string();
                let device_name = String::from_utf16_lossy(&name[..name_len as usize]);
                let bus = if device_name.to_lowercase().contains("usb") || device_name.to_lowercase().contains("vcp") { "usb" } else { "unknown" };
                out.push(Device { path: port, by_id: None, bus: bus.into(), driver: Some(device_name), usb: None });
            }
            RegCloseKey(key);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    pub fn canonical(path: &str) -> String {
        path.trim_start_matches("\\\\.\\").to_uppercase()
    }

    pub struct Port {
        handle: HANDLE,
    }

    // SAFETY: a COM handle may be used from any thread; overlapped I/O keeps reads and writes apart.
    unsafe impl Send for Port {}
    unsafe impl Sync for Port {}

    fn os_error(what: &str) -> anyhow::Error {
        anyhow::anyhow!("{what}: {}", std::io::Error::last_os_error())
    }

    impl Port {
        pub fn open(path: &str, line: Line) -> Result<Port> {
            let name = canonical(path);
            if !name.starts_with("COM") || !name[3..].chars().all(|c| c.is_ascii_digit()) || name.len() < 4 {
                bail!("{path} is not a COM port");
            }
            // SAFETY: CreateFileW on \\.\COMn, exclusive (no sharing), overlapped.
            let handle = unsafe {
                CreateFileW(
                    wide(&format!("\\\\.\\{name}")).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(os_error(&format!("cannot open {name}")));
            }
            let port = Port { handle };
            port.configure(line)?;
            // SAFETY: timeouts on our handle: a read returns what has arrived at once.
            unsafe {
                let timeouts = COMMTIMEOUTS { ReadIntervalTimeout: u32::MAX, ReadTotalTimeoutMultiplier: 0, ReadTotalTimeoutConstant: 0, WriteTotalTimeoutMultiplier: 0, WriteTotalTimeoutConstant: 0 };
                SetCommTimeouts(handle, &timeouts);
            }
            Ok(port)
        }

        pub fn configure(&self, line: Line) -> Result<()> {
            // SAFETY: the DCB read, changed and written back on our handle.
            unsafe {
                let mut dcb: DCB = std::mem::zeroed();
                dcb.DCBlength = std::mem::size_of::<DCB>() as u32;
                if GetCommState(self.handle, &mut dcb) == 0 {
                    return Err(os_error("GetCommState"));
                }
                dcb.BaudRate = line.baud;
                dcb.ByteSize = line.data_bits;
                dcb.Parity = match line.parity {
                    Parity::None => NOPARITY,
                    Parity::Odd => ODDPARITY,
                    Parity::Even => EVENPARITY,
                    Parity::Mark => MARKPARITY,
                    Parity::Space => SPACEPARITY,
                };
                dcb.StopBits = if line.stop_bits == 2 { TWOSTOPBITS } else { ONESTOPBIT };
                // The DCB's flags are a bitfield: fBinary(0) fParity(1) fOutxCtsFlow(2)
                // fOutxDsrFlow(3) fDtrControl(4-5) fDsrSensitivity(6) fTXContinueOnXoff(7)
                // fOutX(8) fInX(9) fErrorChar(10) fNull(11) fRtsControl(12-13) fAbortOnError(14).
                let mut flags: u32 = 1; // fBinary
                if line.parity != Parity::None {
                    flags |= 1 << 1;
                }
                flags |= 1 << 4; // DTR_CONTROL_ENABLE
                match line.flow {
                    Flow::None => flags |= 1 << 12, // RTS_CONTROL_ENABLE
                    Flow::RtsCts => flags |= (1 << 2) | (2 << 12), // CTS out, RTS_CONTROL_HANDSHAKE
                    Flow::XonXoff => flags |= (1 << 8) | (1 << 9) | (1 << 12),
                }
                dcb._bitfield = flags;
                dcb.XonChar = 0x11;
                dcb.XoffChar = 0x13;
                if SetCommState(self.handle, &dcb) == 0 {
                    return Err(os_error("configuring the serial port"));
                }
            }
            Ok(())
        }

        /// One overlapped transfer, waited for up to `timeout_ms` (u32::MAX: until done).
        fn transfer(&self, write: bool, buf: *mut u8, len: u32, timeout_ms: u32) -> std::io::Result<usize> {
            // SAFETY: an OVERLAPPED with its own event, alive until the transfer is done or cancelled.
            unsafe {
                let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
                let mut ov: OVERLAPPED = std::mem::zeroed();
                ov.hEvent = event;
                let mut done = 0u32;
                let ok = if write { WriteFile(self.handle, buf, len, &mut done, &mut ov) } else { ReadFile(self.handle, buf, len, &mut done, &mut ov) };
                let result = if ok != 0 {
                    Ok(done as usize)
                } else if GetLastError() == ERROR_IO_PENDING {
                    if WaitForSingleObject(event, timeout_ms) != 0 {
                        CancelIo(self.handle);
                        GetOverlappedResult(self.handle, &ov, &mut done, 1);
                        Ok(done as usize)
                    } else if GetOverlappedResult(self.handle, &ov, &mut done, 0) != 0 {
                        Ok(done as usize)
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                } else {
                    Err(std::io::Error::last_os_error())
                };
                CloseHandle(event);
                result
            }
        }

        pub fn read(&self, buf: &mut [u8], timeout_ms: i32) -> std::io::Result<usize> {
            // With ReadIntervalTimeout MAXDWORD a read returns at once; wait for a character
            // first by polling the comm events would be finer, a short sleep keeps it simple.
            let n = self.transfer(false, buf.as_mut_ptr(), buf.len() as u32, timeout_ms.max(0) as u32)?;
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms.clamp(1, 20) as u64));
            }
            Ok(n)
        }

        pub fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
            while !data.is_empty() {
                let n = self.transfer(true, data.as_ptr() as *mut u8, data.len() as u32, u32::MAX)?;
                if n == 0 {
                    return Err(std::io::Error::other("the serial port took nothing"));
                }
                data = &data[n..];
            }
            Ok(())
        }

        pub fn send_break(&self, ms: u32) -> Result<()> {
            // SAFETY: break set and cleared on our handle.
            unsafe {
                if SetCommBreak(self.handle) == 0 {
                    return Err(os_error("sending a break"));
                }
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                ClearCommBreak(self.handle);
            }
            Ok(())
        }

        fn escape(&self, function: ESCAPE_COMM_FUNCTION) -> Result<()> {
            // SAFETY: a modem-line function on our handle.
            if unsafe { EscapeCommFunction(self.handle, function) } == 0 {
                return Err(os_error("setting the modem lines"));
            }
            Ok(())
        }

        pub fn set_dtr(&self, on: bool) -> Result<()> {
            self.escape(if on { SETDTR } else { CLRDTR })
        }

        pub fn set_rts(&self, on: bool) -> Result<()> {
            self.escape(if on { SETRTS } else { CLRRTS })
        }

        pub fn modem_status(&self) -> Result<ModemStatus> {
            let mut bits = 0u32;
            // SAFETY: the status written into bits.
            if unsafe { GetCommModemStatus(self.handle, &mut bits) } == 0 {
                return Err(os_error("reading the modem lines"));
            }
            Ok(ModemStatus { cts: bits & MS_CTS_ON != 0, dsr: bits & MS_DSR_ON != 0, dcd: bits & MS_RLSD_ON != 0, ri: bits & MS_RING_ON != 0 })
        }
    }

    impl Drop for Port {
        fn drop(&mut self) {
            // SAFETY: ours, closed once.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn usb_device() -> Device {
        Device {
            path: "/dev/ttyUSB0".into(),
            by_id: Some("/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0".into()),
            bus: "usb".into(),
            driver: Some("ftdi_sio".into()),
            usb: Some(Usb { vid: 0x0403, pid: 0x6001, serial: Some("A50285BI".into()), manufacturer: Some("FTDI".into()), product: Some("FT232R USB UART".into()), interface: Some(0) }),
        }
    }

    #[test]
    fn patterns() {
        let d = usb_device();
        assert!(matches("*", &d));
        assert!(matches("/dev/ttyUSB*", &d));
        assert!(!matches("/dev/ttyACM*", &d));
        assert!(matches("usb:0403:6001", &d));
        assert!(matches("usb:0403:6001:A50285BI", &d));
        assert!(!matches("usb:0403:6001:OTHER", &d));
        assert!(!matches("usb:10c4:ea60", &d));
        assert!(matches("/dev/serial/by-id/usb-FTDI*", &d));
        assert!(!matches("/dev/ttyS0", &Device::bare("/dev/ttyS1")));
    }

    #[test]
    fn decisions() {
        let d = usb_device();
        let off = Policy::default();
        assert!(matches!(decide(&off, &d), Decision::Denied(_)));
        let any = Policy { mode: Mode::Allow, devices: vec![] };
        assert!(matches!(decide(&any, &d), Decision::Allowed));
        let listed = Policy { mode: Mode::Allow, devices: vec!["usb:0403:6001".into()] };
        assert!(matches!(decide(&listed, &d), Decision::Allowed));
        assert!(matches!(decide(&listed, &Device::bare("/dev/ttyS0")), Decision::Denied(_)));
        let ask = Policy { mode: Mode::Ask, devices: vec!["/dev/ttyACM0".into()] };
        assert!(matches!(decide(&ask, &d), Decision::Ask));
        assert!(matches!(decide(&ask, &Device::bare("/dev/ttyACM0")), Decision::Allowed));
        // "Any" is the listed serial ports, never a terminal the system does not list as one.
        for any in [Policy { mode: Mode::Allow, devices: vec![] }, Policy { mode: Mode::Allow, devices: vec!["*".into()] }] {
            assert!(matches!(decide(&any, &d), Decision::Allowed));
            assert!(matches!(decide(&any, &Device::bare("/dev/pts/3")), Decision::Denied(_)));
            assert!(matches!(decide(&any, &Device::bare("/dev/tty")), Decision::Denied(_)));
        }
        let named = Policy { mode: Mode::Allow, devices: vec!["/dev/pts/7".into()] };
        assert!(matches!(decide(&named, &Device::bare("/dev/pts/7")), Decision::Allowed));
        assert!(matches!(decide(&named, &Device::bare("/dev/pts/8")), Decision::Denied(_)));
    }

    #[test]
    fn line_settings() {
        let l = Line::from_json(&json!({"baud": 9600, "dataBits": 7, "parity": "even", "stopBits": 2, "flow": "rtscts"}), Line::default()).unwrap();
        assert_eq!(l, Line { baud: 9600, data_bits: 7, parity: Parity::Even, stop_bits: 2, flow: Flow::RtsCts });
        assert!(Line::from_json(&json!({"dataBits": 9}), Line::default()).is_err());
        let partial = Line::from_json(&json!({"baud": 57600}), l).unwrap();
        assert_eq!(partial.data_bits, 7);
    }

    /// A sysfs and /dev of our own: an FTDI adapter, a CDC-ACM board, an absent 8250 port and a
    /// real one, a virtual console.
    #[test]
    fn linux_listing() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("serial-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sys, dev) = (root.join("sys"), root.join("dev"));
        let w = |p: PathBuf, s: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, s).unwrap();
        };
        // USB device 1-1 with an FTDI interface 1-1:1.0 and its ttyUSB0
        let usb = sys.join("devices/pci0000:00/usb1/1-1");
        w(usb.join("idVendor"), "0403\n");
        w(usb.join("idProduct"), "6001\n");
        w(usb.join("serial"), "A50285BI\n");
        w(usb.join("manufacturer"), "FTDI\n");
        w(usb.join("product"), "FT232R USB UART\n");
        let intf = usb.join("1-1:1.0");
        w(intf.join("bInterfaceNumber"), "00\n");
        let port = intf.join("ttyUSB0");
        std::fs::create_dir_all(&port).unwrap();
        std::fs::create_dir_all(sys.join("bus/usb-serial/drivers/ftdi_sio")).unwrap();
        symlink(sys.join("bus/usb-serial/drivers/ftdi_sio"), port.join("driver")).unwrap();
        std::fs::create_dir_all(sys.join("bus/usb-serial")).unwrap();
        symlink(sys.join("bus/usb-serial"), port.join("subsystem")).unwrap();
        // CDC-ACM: the tty's device is the interface itself
        let usb2 = sys.join("devices/pci0000:00/usb1/1-2");
        w(usb2.join("idVendor"), "2341\n");
        w(usb2.join("idProduct"), "0043\n");
        let acm = usb2.join("1-2:1.0");
        w(acm.join("bInterfaceNumber"), "00\n");
        // platform 8250 ports: ttyS0 present (type 4), ttyS1 absent (type 0)
        let platform = sys.join("devices/platform/serial8250");
        std::fs::create_dir_all(&platform).unwrap();
        let class = sys.join("class/tty");
        std::fs::create_dir_all(&class).unwrap();
        for (name, target) in [("ttyUSB0", port.clone()), ("ttyACM0", acm.clone()), ("ttyS0", platform.clone()), ("ttyS1", platform.clone())] {
            let d = class.join(name);
            std::fs::create_dir_all(&d).unwrap();
            symlink(&target, d.join("device")).unwrap();
        }
        w(class.join("ttyS0/type"), "4\n");
        w(class.join("ttyS1/type"), "0\n");
        std::fs::create_dir_all(class.join("tty1")).unwrap(); // no device: a virtual console
        std::fs::create_dir_all(dev.join("serial/by-id")).unwrap();
        symlink("../../ttyUSB0", dev.join("serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0")).unwrap();

        let list = list_linux(&sys, &dev);
        let paths: Vec<String> = list.iter().map(|d| d.path.rsplit('/').next().unwrap().to_string()).collect();
        assert_eq!(paths, ["ttyACM0", "ttyS0", "ttyUSB0"]);
        let ftdi = list.iter().find(|d| d.path.ends_with("ttyUSB0")).unwrap();
        assert_eq!(ftdi.bus, "usb");
        assert_eq!(ftdi.driver.as_deref(), Some("ftdi_sio"));
        let u = ftdi.usb.as_ref().unwrap();
        assert_eq!((u.vid, u.pid, u.serial.as_deref(), u.interface), (0x0403, 0x6001, Some("A50285BI"), Some(0)));
        assert!(ftdi.by_id.as_deref().unwrap().ends_with("A50285BI-if00-port0"));
        assert!(matches("usb:0403:6001:A50285BI", ftdi));
        let acm = list.iter().find(|d| d.path.ends_with("ttyACM0")).unwrap();
        assert_eq!(acm.usb.as_ref().map(|u| (u.vid, u.pid)), Some((0x2341, 0x0043)));
        assert!(ftdi.describe().contains("USB 0403:6001 FTDI FT232R USB UART, serial A50285BI"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
