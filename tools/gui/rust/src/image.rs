//! Frames out of the server: PNG and PPM files, scaling, and a preview drawn in the terminal.
//! Pixels arrive as the canvas holds them: 4 bytes each, B G R X (XRGB8888, little-endian).

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// BGRX rows, no padding.
    pub px: Vec<u8>,
}

impl Image {
    fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.w + x) * 4) as usize;
        [self.px[i + 2], self.px[i + 1], self.px[i]]
    }

    /// Area-averaged to `nw` x `nh` (smaller), or nearest-neighbour (larger).
    pub fn scaled(&self, nw: u32, nh: u32) -> Image {
        let (nw, nh) = (nw.max(1), nh.max(1));
        if (nw, nh) == (self.w, self.h) {
            return Image { w: self.w, h: self.h, px: self.px.clone() };
        }
        let mut px = Vec::with_capacity(nw as usize * nh as usize * 4);
        let (w, h) = (self.w as u64, self.h as u64);
        for dy in 0..nh as u64 {
            let y0 = dy * h / nh as u64;
            let y1 = ((dy + 1) * h / nh as u64).max(y0 + 1).min(h);
            for dx in 0..nw as u64 {
                let x0 = dx * w / nw as u64;
                let x1 = ((dx + 1) * w / nw as u64).max(x0 + 1).min(w);
                let (mut b, mut g, mut r) = (0u64, 0u64, 0u64);
                for y in y0..y1 {
                    let row = (y * w) as usize * 4;
                    for x in x0..x1 {
                        let i = row + x as usize * 4;
                        b += self.px[i] as u64;
                        g += self.px[i + 1] as u64;
                        r += self.px[i + 2] as u64;
                    }
                }
                let n = (y1 - y0) * (x1 - x0);
                px.extend_from_slice(&[(b / n) as u8, (g / n) as u8, (r / n) as u8, 255]);
            }
        }
        Image { w: nw, h: nh, px }
    }

    pub fn png(&self) -> Vec<u8> {
        let (w, h) = (self.w as usize, self.h as usize);
        let stride = w * 3;
        // Each row filtered with None, Sub or Up, whichever leaves the smallest sum (the usual
        // heuristic); screens are mostly flat colour and repeated rows, which both catch.
        let mut raw = Vec::with_capacity((stride + 1) * h);
        let mut prev = vec![0u8; stride];
        let mut cur = vec![0u8; stride];
        let mut sub = vec![0u8; stride];
        let mut up = vec![0u8; stride];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                cur[x * 3] = self.px[i + 2];
                cur[x * 3 + 1] = self.px[i + 1];
                cur[x * 3 + 2] = self.px[i];
            }
            let score = |v: &[u8]| v.iter().map(|&b| (b as i8).unsigned_abs() as u64).sum::<u64>();
            for i in 0..stride {
                sub[i] = cur[i].wrapping_sub(if i >= 3 { cur[i - 3] } else { 0 });
                up[i] = cur[i].wrapping_sub(prev[i]);
            }
            let (s0, s1, s2) = (score(&cur), score(&sub), if y > 0 { score(&up) } else { u64::MAX });
            if s2 <= s0 && s2 <= s1 {
                raw.push(2);
                raw.extend_from_slice(&up);
            } else if s1 < s0 {
                raw.push(1);
                raw.extend_from_slice(&sub);
            } else {
                raw.push(0);
                raw.extend_from_slice(&cur);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 5);
        let mut out = Vec::with_capacity(z.len() + 64);
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.w.to_be_bytes());
        ihdr.extend_from_slice(&self.h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    pub fn ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.w, self.h).into_bytes();
        out.reserve(self.w as usize * self.h as usize * 3);
        for y in 0..self.h {
            for x in 0..self.w {
                out.extend_from_slice(&self.rgb(x, y));
            }
        }
        out
    }

    /// The image in `cols` terminal columns: each cell is two pixels, the upper half block in the
    /// top one's colour over the bottom one's (24-bit colour).
    pub fn ansi(&self, cols: u32) -> String {
        let cols = cols.clamp(1, self.w.max(1));
        let rows = ((self.h as u64 * cols as u64 / self.w.max(1) as u64) as u32 / 2).max(1);
        let img = self.scaled(cols, rows * 2);
        let mut out = String::with_capacity((cols * rows * 20) as usize);
        for r in 0..rows {
            let mut last: Option<([u8; 3], [u8; 3])> = None;
            for c in 0..cols {
                let (t, b) = (img.rgb(c, r * 2), img.rgb(c, r * 2 + 1));
                if last != Some((t, b)) {
                    out.push_str(&format!("\x1b[38;2;{};{};{};48;2;{};{};{}m", t[0], t[1], t[2], b[0], b[1], b[2]));
                    last = Some((t, b));
                }
                out.push('▀');
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }
}

fn crc32(parts: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xffff_ffffu32;
    for p in parts {
        for &b in *p {
            c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xffff_ffff
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Image {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let on = (x / 2 + y / 2) % 2 == 0;
                px.extend_from_slice(if on { &[0, 0, 255, 0] } else { &[255, 255, 255, 0] });
            }
        }
        Image { w, h, px }
    }

    #[test]
    fn png_layout() {
        let png = checker(9, 5).png();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 9);
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // The image data inflates back to rows of filter byte + RGB.
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&png[41..41 + idat_len]).unwrap();
        assert_eq!(raw.len(), 5 * (1 + 9 * 3));
    }

    #[test]
    fn scaling() {
        let img = checker(4, 4).scaled(2, 2);
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(&img.px[0..3], &[0, 0, 255]);
        assert_eq!(&img.px[4..7], &[255, 255, 255]);
        let half = checker(2, 2).scaled(1, 1);
        assert_eq!(&half.px[0..3], &[0, 0, 255]);
        let mixed = checker(4, 2).scaled(1, 1);
        assert_eq!(&mixed.px[0..3], &[127, 127, 255]);
        assert!(checker(8, 8).ansi(4).contains('▀'));
    }
}
