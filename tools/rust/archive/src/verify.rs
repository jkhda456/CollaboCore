//! Every compressed stream is decoded again as it is written, on a second thread, and must give
//! back what went in (length and CRC-32) before the tools report success. The codecs are young
//! pure-Rust crates (one encoder bug is patched in patches/), and an archive that does not
//! decode is the one failure a compressor must never hide: with this, it is an error instead.

use std::io::{self, Read, Write};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;

/// The error a failed check becomes, so callers can tell it from I/O and data errors.
#[derive(Debug)]
pub struct VerifyError(pub String);

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the compressed data failed the check ({}); this is a bug in the compressor", self.0)
    }
}

impl std::error::Error for VerifyError {}

pub fn failed(m: String) -> io::Error {
    io::Error::other(VerifyError(m))
}

/// The VerifyError inside an io::Error, if that is what it is.
pub fn as_verify(e: &io::Error) -> Option<&VerifyError> {
    e.get_ref().and_then(|x| x.downcast_ref::<VerifyError>())
}

#[derive(Clone, Copy)]
pub enum Codec {
    Zstd,
    Xz,
    Lzma,
    Lzip,
    Gzip,
}

/// Counts and hashes what passes through it.
pub struct Hashing<R> {
    pub inner: R,
    pub crc: crc32fast::Hasher,
    pub len: u64,
}

impl<R> Hashing<R> {
    pub fn new(inner: R) -> Self {
        Hashing { inner, crc: crc32fast::Hasher::new(), len: 0 }
    }
    pub fn sum(&self) -> (u64, u32) {
        (self.len, self.crc.clone().finalize())
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The decoding thread's input: chunks from the channel.
struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A writer that passes the compressed bytes on to `inner` and to the checking decoder.
pub struct Tee<W> {
    inner: W,
    tx: Option<SyncSender<Vec<u8>>>,
    pending: Vec<u8>,
    handle: Option<JoinHandle<Result<(u64, u32), String>>>,
}

const CHUNK: usize = 1 << 16;

impl<W: Write> Tee<W> {
    pub fn new(inner: W, codec: Codec, dict: Option<Vec<u8>>) -> Self {
        let (tx, rx) = sync_channel::<Vec<u8>>(16);
        let handle = std::thread::Builder::new()
            .name("verify".into())
            .spawn(move || decode(codec, ChannelReader { rx, buf: vec![], pos: 0 }, dict))
            .expect("thread");
        Tee { inner, tx: Some(tx), pending: Vec::with_capacity(CHUNK), handle: Some(handle) }
    }

    fn send(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let chunk = std::mem::replace(&mut self.pending, Vec::with_capacity(CHUNK));
        if let Some(tx) = &self.tx {
            // A decoder that stopped (it failed) no longer listens; finish() reports why.
            if tx.send(chunk).is_err() {
                self.tx = None;
            }
        }
    }

    /// Ends the stream; Ok when what the decoder got back has `expect`'s length and CRC.
    pub fn finish(mut self, expect: (u64, u32)) -> Result<(), String> {
        self.send();
        self.tx = None;
        let got = self.handle.take().unwrap().join().map_err(|_| "the checking decoder panicked".to_string())?;
        match got {
            Ok(g) if g == expect => Ok(()),
            Ok((len, _)) if len != expect.0 => Err(format!("it decodes to {len} bytes, not {}", expect.0)),
            Ok(_) => Err("it decodes to different data".into()),
            Err(e) => Err(format!("it does not decode: {e}")),
        }
    }
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pending.extend_from_slice(&buf[..n]);
        if self.pending.len() >= CHUNK {
            self.send();
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W> Drop for Tee<W> {
    fn drop(&mut self) {
        // Unfinished (an error on the way): let the decoder see the end and stop.
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn decode(codec: Codec, input: ChannelReader, dict: Option<Vec<u8>>) -> Result<(u64, u32), String> {
    let mut out = Hashing::new(io::sink());
    let mut input = io::BufReader::with_capacity(CHUNK, input);
    let r: io::Result<()> = match codec {
        Codec::Zstd => {
            let mut d = structured_zstd::decoding::FrameDecoder::new();
            let built = match &dict {
                Some(bytes) => match structured_zstd::decoding::DictionaryHandle::decode_dict(bytes) {
                    Ok(h) => structured_zstd::decoding::StreamingDecoder::new_with_decoder_and_dictionary_handle(&mut input, &mut d, &h)
                        .map_err(|e| format!("{e:?}"))
                        .and_then(|mut s| {
                            s.decoder_mut().set_content_checksum(structured_zstd::decoding::ContentChecksum::Verify);
                            io::copy(&mut s, &mut out).map(|_| ()).map_err(|e| e.to_string())
                        }),
                    Err(e) => Err(format!("{e:?}")),
                },
                None => structured_zstd::decoding::StreamingDecoder::new_with_decoder(&mut input, &mut d)
                    .map_err(|e| format!("{e:?}"))
                    .and_then(|mut s| {
                        s.decoder_mut().set_content_checksum(structured_zstd::decoding::ContentChecksum::Verify);
                        io::copy(&mut s, &mut out).map(|_| ()).map_err(|e| e.to_string())
                    }),
            };
            built.map_err(io::Error::other)
        }
        Codec::Xz => io::copy(&mut lzma_rust2::XzReader::new(&mut input, true), &mut out).map(|_| ()),
        Codec::Lzma => lzma_rust2::LzmaReader::new_mem_limit(&mut input, u32::MAX, None).and_then(|mut r| io::copy(&mut r, &mut out).map(|_| ())),
        Codec::Lzip => io::copy(&mut lzma_rust2::LzipReader::new(&mut input), &mut out).map(|_| ()),
        Codec::Gzip => io::copy(&mut flate2::read::MultiGzDecoder::new(&mut input), &mut out).map(|_| ()),
    };
    // Drain what is left, so the writer side never blocks on a full channel.
    let _ = io::copy(&mut input, &mut io::sink());
    r.map_err(|e| e.to_string())?;
    Ok(out.sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let mut w = lzma_rust2::XzWriter::new(&mut out, lzma_rust2::XzOptions::with_preset(1)).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap();
        out
    }

    fn check(stream: &[u8], expect: &[u8]) -> Result<(), String> {
        let mut sink = vec![];
        let mut tee = Tee::new(&mut sink, Codec::Xz, None);
        for c in stream.chunks(1000) {
            tee.write_all(c).unwrap();
        }
        let mut h = Hashing::new(expect);
        io::copy(&mut h, &mut io::sink()).unwrap();
        tee.finish(h.sum())
    }

    #[test]
    fn catches_what_does_not_come_back() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let good = xz(&data);
        assert_eq!(check(&good, &data), Ok(()));
        assert!(check(&good, &data[1..]).unwrap_err().contains("decodes to"));
        let mut other = data.clone();
        other[5] ^= 1;
        assert_eq!(check(&good, &other).unwrap_err(), "it decodes to different data");
        let mut bad = good.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x55;
        assert!(check(&bad, &data).unwrap_err().starts_with("it does not decode"));
        assert!(check(b"garbage", &data).unwrap_err().starts_with("it does not decode"));
    }
}
