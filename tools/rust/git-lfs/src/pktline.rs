//! git's pkt-line framing (github.com/git-lfs/pktline), for the long-running filter process.

use std::io::{Read, Write};

pub const MAX_PACKET_LENGTH: usize = 65516;

pub struct Pktline<R: Read, W: Write> {
    pub r: std::io::BufReader<R>,
    pub w: W,
}

impl<R: Read, W: Write> Pktline<R, W> {
    pub fn new(r: R, w: W) -> Self {
        Pktline { r: std::io::BufReader::with_capacity(65536, r), w }
    }

    /// (payload, length): length 0 is a flush packet, 1 a delimiter.
    pub fn read_packet_with_length(&mut self) -> std::io::Result<(Vec<u8>, usize)> {
        let mut h = [0u8; 4];
        self.r.read_exact(&mut h)?;
        let s = std::str::from_utf8(&h).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "strconv.ParseInt: invalid syntax"))?;
        let len = usize::from_str_radix(s, 16).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("strconv.ParseInt: parsing {}: invalid syntax", crate::tools::quote(s))))?;
        if len == 0 || len == 1 {
            return Ok((vec![], len));
        }
        if len < 4 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid packet length."));
        }
        let mut buf = vec![0u8; len - 4];
        self.r.read_exact(&mut buf)?;
        Ok((buf, len))
    }

    pub fn read_packet_text(&mut self) -> std::io::Result<(String, usize)> {
        let (d, l) = self.read_packet_with_length()?;
        let s = String::from_utf8_lossy(&d).into_owned();
        Ok((s.strip_suffix('\n').map(str::to_string).unwrap_or(s), l))
    }

    pub fn read_packet_list(&mut self) -> std::io::Result<Vec<String>> {
        let mut list = vec![];
        loop {
            let (d, l) = self.read_packet_text()?;
            if l == 0 {
                return Ok(list);
            }
            list.push(d);
        }
    }

    pub fn write_packet(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.len() > MAX_PACKET_LENGTH {
            return Err(std::io::Error::other("Packet length exceeds maximal length"));
        }
        self.w.write_all(format!("{:04x}", data.len() + 4).as_bytes())?;
        self.w.write_all(data)
    }

    pub fn write_flush(&mut self) -> std::io::Result<()> {
        self.w.write_all(b"0000")?;
        self.w.flush()
    }

    pub fn write_packet_list(&mut self, list: &[String]) -> std::io::Result<()> {
        for i in list {
            self.write_packet(format!("{i}\n").as_bytes())?;
        }
        self.write_flush()
    }

    /// The content packets up to a flush (a request's payload).
    pub fn read_payload(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = vec![];
        loop {
            let (d, l) = self.read_packet_with_length()?;
            if l == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&d);
        }
    }

    /// Content as packets of at most `cap` bytes, then a flush.
    pub fn write_payload(&mut self, data: &[u8], cap: usize) -> std::io::Result<()> {
        for c in data.chunks(cap.min(MAX_PACKET_LENGTH)) {
            self.write_packet(c)?;
        }
        self.write_flush()
    }
}
