//! Traditional PKWARE encryption (ZipCrypto), as Info-ZIP's crypt.c does it for `zip -e`/`-P`:
//! three keys updated by each plaintext byte, a 12-byte header (10 random bytes and two check
//! bytes) before the data.

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

pub struct Keys([u32; 3]);

impl Keys {
    pub fn new(password: &[u8]) -> Keys {
        let mut k = Keys([0x1234_5678, 0x2345_6789, 0x3456_7890]);
        for &b in password {
            k.update(b);
        }
        k
    }

    fn update(&mut self, b: u8) {
        let t = crc_table();
        self.0[0] = t[((self.0[0] ^ b as u32) & 0xff) as usize] ^ (self.0[0] >> 8);
        self.0[1] = self.0[1].wrapping_add(self.0[0] & 0xff).wrapping_mul(134_775_813).wrapping_add(1);
        self.0[2] = t[((self.0[2] ^ (self.0[1] >> 24)) & 0xff) as usize] ^ (self.0[2] >> 8);
    }

    fn stream_byte(&self) -> u8 {
        let t = (self.0[2] | 2) as u16;
        (t.wrapping_mul(t ^ 1) >> 8) as u8
    }

    pub fn encrypt(&mut self, buf: &mut [u8]) {
        for b in buf {
            let c = *b;
            *b = c ^ self.stream_byte();
            self.update(c);
        }
    }
}

/// The encryption header for an entry whose check value is `check` (the CRC, or the DOS time
/// shifted left 16 when the data descriptor carries the CRC): crypthead() makes its ten random
/// bytes by encrypting random ones under a first key schedule, then encrypts the whole header
/// under a fresh one, which `keys` continues into the data.
pub fn header(password: &[u8], check: u32) -> ([u8; 12], Keys) {
    let mut random = [0u8; 10];
    fill_random(&mut random);
    let mut first = Keys::new(password);
    first.encrypt(&mut random);
    let mut head = [0u8; 12];
    head[..10].copy_from_slice(&random);
    head[10] = (check >> 16) as u8;
    head[11] = (check >> 24) as u8;
    let mut keys = Keys::new(password);
    keys.encrypt(&mut head);
    (head, keys)
}

fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf)).is_ok() {
        return;
    }
    // No urandom: the clock, stirred (the header only has to differ between entries).
    let mut x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
        ^ (std::process::id() as u64) << 32;
    for b in buf {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decrypt(password: &[u8], data: &[u8]) -> Vec<u8> {
        let mut k = Keys::new(password);
        data.iter()
            .map(|&c| {
                let p = c ^ k.stream_byte();
                k.update(p);
                p
            })
            .collect()
    }

    #[test]
    fn round_trip_and_check_byte() {
        let (head, mut keys) = header(b"secret", 0xabcd_1234);
        let mut data = b"hello, world".to_vec();
        keys.encrypt(&mut data);
        let mut all = head.to_vec();
        all.extend_from_slice(&data);
        let plain = decrypt(b"secret", &all);
        assert_eq!(plain[11], 0xab);
        assert_eq!(plain[10], 0xcd);
        assert_eq!(&plain[12..], b"hello, world");
    }
}
