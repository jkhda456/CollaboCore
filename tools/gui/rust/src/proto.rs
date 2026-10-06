//! The wire protocol between the display server, the programs that draw (apps) and the command
//! line (control clients). One Unix stream socket; every message is an 8-byte header — payload
//! length (u32), opcode (u16), reserved (u16) — and a payload of little-endian fields: u32/i32,
//! strings and byte strings as a u32 length and the bytes. The C client library
//! (client/collabo_gui.c) speaks the same; PROTOCOL.md describes every message.

pub const VERSION: u32 = 1;
pub const HEADER: usize = 8;
/// The largest payload the server accepts. Pixel updates bigger than this are split by the
/// sender into bands of rows.
pub const MAX_PAYLOAD: usize = 32 << 20;
/// The largest the command line accepts from the server: a whole frame of the largest canvas.
pub const MAX_RESULT: usize = 8192 * 8192 * 4 + (1 << 20);

/// HELLO's role field.
pub const ROLE_APP: u32 = 0;
pub const ROLE_CONTROL: u32 = 1;

pub mod op {
    // Both directions.
    pub const HELLO: u16 = 0x0001; // client: u32 version, u32 role, u32 pid, str name, str token
    pub const WELCOME: u16 = 0x0002; // server: u32 version, u32 client id, str app name,
    //                                   u32 default w, h, u32 max w, h, u32 max canvases
    pub const ERROR: u16 = 0x0003; // server: u32 code, str message

    // App -> server.
    pub const CANVAS_CREATE: u16 = 0x0101; // u32 canvas, u32 w, u32 h, str title
    pub const CANVAS_DESTROY: u16 = 0x0102; // u32 canvas
    pub const CANVAS_TITLE: u16 = 0x0103; // u32 canvas, str title
    pub const CANVAS_UPDATE: u16 = 0x0104; // u32 canvas, u32 x, y, w, h, bytes pixels (w*h*4)
    pub const CANVAS_COMMIT: u16 = 0x0105; // u32 canvas, u32 serial, u32 w, u32 h
    pub const CANVAS_REQUEST_SIZE: u16 = 0x0106; // u32 canvas, u32 w, u32 h
    pub const CANVAS_CURSOR: u16 = 0x0107; // u32 canvas, str cursor name
    pub const FOCUS_REQUEST: u16 = 0x0108; // u32 canvas
    pub const CLIPBOARD_SET: u16 = 0x0109; // u32 n, n * (str mime, bytes data)
    pub const CLIPBOARD_GET: u16 = 0x010a; // u32 request, str mime
    pub const PONG: u16 = 0x010b; // u32 serial
    pub const TEXT_INPUT: u16 = 0x010c; // u32 canvas, u32 enabled, i32 x, i32 y, u32 w, u32 h (the caret)

    // Server -> app.
    pub const CONFIGURE: u16 = 0x0201; // u32 canvas, u32 w, u32 h
    pub const FRAME_DONE: u16 = 0x0202; // u32 canvas, u32 serial
    pub const CLOSE: u16 = 0x0203; // u32 canvas
    pub const FOCUS: u16 = 0x0204; // u32 canvas, u32 focused
    pub const POINTER_MOTION: u16 = 0x0205; // u32 canvas, i32 x, i32 y, u32 mods, u32 buttons
    pub const POINTER_BUTTON: u16 = 0x0206; // u32 canvas, i32 x, i32 y, u32 button, u32 pressed, u32 mods, u32 buttons
    pub const SCROLL: u16 = 0x0207; // u32 canvas, i32 x, i32 y, i32 dx, i32 dy, i32 steps x, i32 steps y, u32 mods
    pub const KEY: u16 = 0x0208; // u32 canvas, u32 keysym, u32 keycode, u32 pressed, u32 mods, str text
    pub const TEXT: u16 = 0x0209; // u32 canvas, str text
    pub const CLIPBOARD_CHANGED: u16 = 0x020a; // u32 n, n * str mime
    pub const CLIPBOARD_DATA: u16 = 0x020b; // u32 request, u32 found, str mime, bytes data
    pub const PING: u16 = 0x020c; // u32 serial
    pub const POINTER_ENTER: u16 = 0x020d; // u32 canvas, i32 x, i32 y, u32 mods, u32 buttons
    pub const POINTER_LEAVE: u16 = 0x020e; // u32 canvas
    pub const PREEDIT: u16 = 0x020f; // u32 canvas, str text, i32 cursor begin, i32 cursor end (bytes; -1: none)

    // Control <-> server.
    pub const CONTROL: u16 = 0x0301; // u32 request, u32 argc, argc * str, bytes blob
    pub const RESULT: u16 = 0x0302; // u32 request, u32 status, str text, bytes blob
}

/// Modifier bits, as X11's state mask.
pub mod modifier {
    pub const SHIFT: u32 = 1;
    pub const CAPS_LOCK: u32 = 2;
    pub const CONTROL: u32 = 4;
    pub const ALT: u32 = 8;
    pub const SUPER: u32 = 0x40;
    pub const BUTTON1: u32 = 0x100;
}

/// Error codes in ERROR and RESULT.
pub mod status {
    pub const OK: u32 = 0;
    pub const FAILED: u32 = 1;
    pub const USAGE: u32 = 2;
    pub const TIMEOUT: u32 = 3;
    pub const NOT_FOUND: u32 = 4;
    pub const DENIED: u32 = 5;
    pub const PROTOCOL: u32 = 6;
    pub const LIMIT: u32 = 7;
}

#[derive(Debug)]
pub struct Malformed;

pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new(op: u16) -> Writer {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend_from_slice(&op.to_le_bytes());
        buf.extend_from_slice(&[0, 0]);
        Writer { buf }
    }
    pub fn with_capacity(op: u16, payload: usize) -> Writer {
        let mut w = Writer { buf: Vec::with_capacity(HEADER + payload) };
        w.buf.extend_from_slice(&[0, 0, 0, 0]);
        w.buf.extend_from_slice(&op.to_le_bytes());
        w.buf.extend_from_slice(&[0, 0]);
        w
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i32(mut self, v: i32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(mut self, v: &[u8]) -> Self {
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v);
        self
    }
    pub fn str(self, v: &str) -> Self {
        self.bytes(v.as_bytes())
    }
    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.buf.len() - HEADER) as u32;
        self.buf[0..4].copy_from_slice(&len.to_le_bytes());
        self.buf
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(payload: &'a [u8]) -> Reader<'a> {
        Reader { buf: payload, pos: 0 }
    }
    pub fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.buf.get(self.pos..self.pos + 4).ok_or(Malformed)?;
        self.pos += 4;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i32(&mut self) -> Result<i32, Malformed> {
        self.u32().map(|v| v as i32)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let n = self.u32()? as usize;
        let b = self.buf.get(self.pos..self.pos.checked_add(n).ok_or(Malformed)?).ok_or(Malformed)?;
        self.pos += n;
        Ok(b)
    }
    pub fn str(&mut self) -> Result<String, Malformed> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
}

/// Splits a buffer of received bytes into whole messages: `(op, payload)` for each one complete
/// at the front, and how many bytes they took. Err when a header names a payload over `max`.
pub fn next_message(buf: &[u8], max: usize) -> Result<Option<(u16, &[u8], usize)>, Malformed> {
    if buf.len() < HEADER {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > max {
        return Err(Malformed);
    }
    let op = u16::from_le_bytes([buf[4], buf[5]]);
    if buf.len() < HEADER + len {
        return Ok(None);
    }
    Ok(Some((op, &buf[HEADER..HEADER + len], HEADER + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = Writer::new(op::KEY).u32(7).u32(0x61).i32(-3).str("가").bytes(&[1, 2]).finish();
        let (o, payload, used) = next_message(&m, MAX_PAYLOAD).unwrap().unwrap();
        assert_eq!((o, used), (op::KEY, m.len()));
        let mut r = Reader::new(payload);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 0x61);
        assert_eq!(r.i32().unwrap(), -3);
        assert_eq!(r.str().unwrap(), "가");
        assert_eq!(r.bytes().unwrap(), &[1, 2]);
        assert!(r.u32().is_err());
    }

    #[test]
    fn partial_and_oversized() {
        let m = Writer::new(op::PING).u32(1).finish();
        assert!(next_message(&m[..m.len() - 1], MAX_PAYLOAD).unwrap().is_none());
        let mut big = m.clone();
        big[0..4].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert!(next_message(&big, MAX_PAYLOAD).is_err());
        let mut r = Reader::new(&[5, 0, 0, 0, 1]);
        assert!(r.bytes().is_err());
    }
}
