//! Writes a "stored" zip archive (method 0, no compression), the same container as
//! `host/zip.js`: UTF-8 names, the Unix mode in the external attributes and an exact mtime in
//! the "UT" extra field. No compression, no encryption, no zip64 (4 GiB / 65535 entries).
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

const SIG_LOCAL: u32 = 0x04034b50;
const SIG_CENTRAL: u32 = 0x02014b50;
const SIG_END: u32 = 0x06054b50;
const UNIX: u16 = 3;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// What goes into the archive: a file's bytes, a directory, or a symlink's target.
enum Kind {
    File(Vec<u8>),
    Directory,
    Symlink(Vec<u8>),
}

struct Entry {
    path: String,
    kind: Kind,
    mode: u32,
    mtime: u64,
}

impl Entry {
    fn data(&self) -> &[u8] {
        match &self.kind {
            Kind::File(bytes) | Kind::Symlink(bytes) => bytes,
            Kind::Directory => &[],
        }
    }

    /// The external attributes: the Unix mode in the high half, DOS "directory" in the low.
    fn external(&self) -> u32 {
        let kind = match self.kind {
            Kind::File(_) => S_IFREG,
            Kind::Directory => S_IFDIR,
            Kind::Symlink(_) => S_IFLNK,
        };
        ((kind | (self.mode & 0o7777)) << 16) | u32::from(matches!(self.kind, Kind::Directory))
    }
}

/// The zip "UT" extra field, which carries the real modification time.
fn extra(mtime: u64) -> Vec<u8> {
    let mut field = Vec::with_capacity(9);
    field.extend_from_slice(&0x5455u16.to_le_bytes()); // "UT"
    field.extend_from_slice(&5u16.to_le_bytes());
    field.push(1); // flags: mtime present
    field.extend_from_slice(&(mtime as u32).to_le_bytes());
    field
}

/// The DOS date and time (UTC), which only has two-second resolution: the "UT" extra field
/// carries the exact one.
fn dos_time(mtime: u64) -> (u16, u16) {
    let days = mtime / 86400;
    let seconds = mtime % 86400;
    let (year, month, day) = civil_from_days(days as i64);
    // Before 1980 there is no room in the field, so the zip epoch is the floor.
    let year = year.max(1980) as u64;
    let time = ((seconds / 3600) << 11) | ((seconds % 3600 / 60) << 5) | (seconds % 60 / 2);
    let date = ((year - 1980) << 9) | ((month as u64) << 5) | day as u64;
    (time as u16, date as u16)
}

/// Days since 1970-01-01 as a date, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468; // shift the epoch to 0000-03-01, where the era arithmetic works
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Collects a host folder, in a stable order, as archive entries.
fn collect(root: &Path, directory: &Path, entries: &mut Vec<Entry>) -> Result<()> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(directory)?.map(|entry| Ok(entry?.path())).collect::<Result<_>>()?;
    names.sort();
    for full in names {
        let metadata = std::fs::symlink_metadata(&full)?;
        let relative = full.strip_prefix(root).unwrap_or(&full);
        let path = relative.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs())
            .unwrap_or(0);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = if metadata.is_dir() { 0o755 } else { 0o644 };

        if metadata.is_symlink() {
            let target = std::fs::read_link(&full)?;
            entries.push(Entry { path, kind: Kind::Symlink(target.to_string_lossy().into_owned().into_bytes()), mode, mtime });
        } else if metadata.is_dir() {
            entries.push(Entry { path: format!("{path}/"), kind: Kind::Directory, mode, mtime });
            collect(root, &full, entries)?;
        } else if metadata.is_file() {
            entries.push(Entry { path, kind: Kind::File(std::fs::read(&full)?), mode, mtime });
        }
    }
    Ok(())
}

/// Writes `directory` into `out_file` as a stored zip. Returns (entries, bytes).
pub fn write_folder(directory: &Path, out_file: &Path) -> Result<(usize, u64)> {
    let mut entries = Vec::new();
    collect(directory, directory, &mut entries)?;
    if entries.len() > 0xffff {
        bail!("{} entries is more than a zip without zip64 can hold", entries.len());
    }

    let mut file = std::io::BufWriter::new(std::fs::File::create(out_file)?);
    let mut central = Vec::new();

    for entry in &entries {
        let offset = file.stream_position()?;
        let data = entry.data();
        let crc = crc32(data);
        let (time, date) = dos_time(entry.mtime);
        let name = entry.path.as_bytes();
        let extra = extra(entry.mtime);

        file.write_all(&SIG_LOCAL.to_le_bytes())?;
        file.write_all(&20u16.to_le_bytes())?; // version needed
        file.write_all(&(1u16 << 11).to_le_bytes())?; // UTF-8 names
        file.write_all(&0u16.to_le_bytes())?; // method: stored
        file.write_all(&time.to_le_bytes())?;
        file.write_all(&date.to_le_bytes())?;
        file.write_all(&crc.to_le_bytes())?;
        file.write_all(&(data.len() as u32).to_le_bytes())?;
        file.write_all(&(data.len() as u32).to_le_bytes())?;
        file.write_all(&(name.len() as u16).to_le_bytes())?;
        file.write_all(&(extra.len() as u16).to_le_bytes())?;
        file.write_all(name)?;
        file.write_all(&extra)?;
        file.write_all(data)?;

        central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        central.extend_from_slice(&((UNIX << 8) | 20).to_le_bytes()); // made by unix
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&(1u16 << 11).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&entry.external().to_le_bytes());
        central.extend_from_slice(&(offset as u32).to_le_bytes());
        central.extend_from_slice(name);
        central.extend_from_slice(&extra);
    }

    let central_at = file.stream_position()?;
    file.write_all(&central)?;
    let end_at = file.stream_position()?;

    file.write_all(&SIG_END.to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?; // this disk
    file.write_all(&0u16.to_le_bytes())?; // disk with the central directory
    file.write_all(&(entries.len() as u16).to_le_bytes())?;
    file.write_all(&(entries.len() as u16).to_le_bytes())?;
    file.write_all(&((end_at - central_at) as u32).to_le_bytes())?;
    file.write_all(&(central_at as u32).to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?; // comment length
    file.flush()?;

    // The archive is everything written plus the 22-byte end-of-directory record.
    Ok((entries.len(), end_at + 22))
}
