//! The LFS storage under the git dir (fs package): objects/aa/bb/OID, tmp, logs, and the
//! object directories of alternates (reference clones).

use crate::errors::{Error, Result};
use crate::tools;
use std::sync::OnceLock;

pub const EMPTY_OBJECT_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub struct Filesystem {
    pub git_storage_dir: String,
    pub lfs_storage_dir: String,
    pub reference_dirs: Vec<String>,
    perms: u32,
    objdir: OnceLock<String>,
    tmpdir: OnceLock<String>,
    logdir: OnceLock<String>,
}

impl Filesystem {
    pub fn new(gitdir: &str, lfsdir: &str, perms: u32) -> Filesystem {
        let git_storage_dir = resolve_git_storage_dir(gitdir);
        let reference_dirs = resolve_reference_dirs(&git_storage_dir);
        let lfsdir = if lfsdir.is_empty() { "lfs" } else { lfsdir };
        let lfs_storage_dir = if lfsdir.starts_with('/') { lfsdir.to_string() } else { tools::join(&[&git_storage_dir, lfsdir]) };
        Filesystem { git_storage_dir, lfs_storage_dir, reference_dirs, perms, objdir: OnceLock::new(), tmpdir: OnceLock::new(), logdir: OnceLock::new() }
    }

    fn sub(&self, cell: &OnceLock<String>, name: &str) -> String {
        cell.get_or_init(|| {
            let d = tools::join(&[&self.lfs_storage_dir, name]);
            let _ = tools::mkdir_all(&d, self.perms);
            d
        })
        .clone()
    }
    pub fn lfs_object_dir(&self) -> String {
        self.sub(&self.objdir, "objects")
    }
    pub fn temp_dir(&self) -> String {
        self.sub(&self.tmpdir, "tmp")
    }
    pub fn log_dir(&self) -> String {
        self.sub(&self.logdir, "logs")
    }

    fn local_object_dir(&self, oid: &str) -> String {
        tools::join(&[&self.lfs_object_dir(), &oid[0..2], &oid[2..4]])
    }

    pub fn object_pathname(&self, oid: &str) -> String {
        if oid == EMPTY_OBJECT_SHA256 {
            return "/dev/null".into();
        }
        format!("{}/{}", self.local_object_dir(oid), oid)
    }

    /// The object's path, its directory created.
    pub fn object_path(&self, oid: &str) -> Result<String> {
        if oid.len() < 4 {
            return Err(Error::new(format!("too short object ID: {}", tools::quote(oid))));
        }
        if oid == EMPTY_OBJECT_SHA256 {
            return Ok("/dev/null".into());
        }
        let dir = self.local_object_dir(oid);
        tools::mkdir_all(&dir, self.perms).map_err(|e| Error::new(format!("error trying to create local storage directory in {}: {}", tools::quote(&dir), tools::path_err("mkdir", &dir, &e))))?;
        Ok(format!("{dir}/{oid}"))
    }

    pub fn object_exists(&self, oid: &str, size: i64) -> bool {
        if size == 0 {
            return true;
        }
        tools::file_exists_of_size(self.object_pathname(oid), size)
    }

    pub fn object_reference_paths(&self, oid: &str) -> Vec<String> {
        self.reference_dirs.iter().map(|r| format!("{r}/{}/{}/{oid}", &oid[0..2], &oid[2..4])).collect()
    }

    /// Every object file: (oid, size).
    pub fn each_object(&self) -> Vec<(String, i64)> {
        let mut out = vec![];
        walk(std::path::Path::new(&self.lfs_object_dir()), &mut |p, m| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.len() >= 64 && name[..64].bytes().all(|c| c.is_ascii_alphanumeric()) {
                out.push((name, m.len() as i64));
            }
        });
        out
    }

    /// Removes tmp files of objects now stored, and ones older than an hour.
    pub fn cleanup(&self) {
        let tmp = format!("{}/tmp", self.lfs_storage_dir);
        if !tools::dir_exists(&tmp) {
            return;
        }
        let hour = std::time::Duration::from_secs(3600);
        walk(std::path::Path::new(&tmp), &mut |p, m| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parts: Vec<&str> = name.splitn(2, '-').collect();
            if parts.len() == 2 && parts[0].len() == 64 {
                if std::fs::metadata(self.object_pathname(parts[0])).map(|x| !x.is_dir()).unwrap_or(false) {
                    crate::trace!("Removing existing tmp object file: {}", p.display());
                    let _ = std::fs::remove_file(p);
                    return;
                }
            }
            if let Some(parent) = p.parent() {
                if parent != std::path::Path::new(&tmp) {
                    if let Ok(pm) = std::fs::metadata(parent) {
                        if pm.modified().ok().and_then(|t| t.elapsed().ok()).map_or(false, |e| e <= hour) {
                            return;
                        }
                    }
                }
            }
            if m.modified().ok().and_then(|t| t.elapsed().ok()).map_or(false, |e| e > hour) {
                crate::trace!("Removing old tmp object file: {}", p.display());
                let _ = std::fs::remove_file(p);
            }
        });
    }
}

pub fn walk(dir: &std::path::Path, f: &mut dyn FnMut(&std::path::Path, &std::fs::Metadata)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if m.is_dir() {
            walk(&p, f);
        } else {
            f(&p, &m);
        }
    }
}

fn resolve_git_storage_dir(gitdir: &str) -> String {
    let common = format!("{gitdir}/commondir");
    if tools::file_exists(&common) && !tools::dir_exists(format!("{gitdir}/objects")) {
        if let Ok(data) = std::fs::read_to_string(&common) {
            let d = data.trim();
            return if d.starts_with('/') { d.to_string() } else { tools::clean_str(&format!("{gitdir}/{d}")) };
        }
    }
    gitdir.to_string()
}

fn exists_alternate(objs: &str) -> Option<String> {
    let mut objs = objs.trim().to_string();
    if objs.starts_with('"') {
        let end = objs.rfind('"')?;
        if end == 0 {
            return None;
        }
        objs = unquote_go(&objs[..=end])?;
    }
    let parent = std::path::Path::new(&objs).parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into());
    let storage = tools::clean_str(&format!("{parent}/lfs/objects"));
    tools::dir_exists(&storage).then_some(storage)
}

fn unquote_go(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                c => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn resolve_reference_dirs(storage: &str) -> Vec<String> {
    let mut refs = vec![];
    if let Ok(env) = std::env::var("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        for s in env.split(':') {
            if let Some(d) = exists_alternate(s) {
                refs.push(d);
            }
        }
    }
    let path = format!("{storage}/objects/info/alternates");
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if let Some(d) = exists_alternate(t) {
                refs.push(d);
            }
        }
    }
    refs
}

/// DecodePathBytes: git's quoted paths with \NNN octal escapes, back to bytes.
pub fn decode_path_bytes(p: &[u8]) -> Vec<u8> {
    let mut p = p;
    if p.len() > 2 && p[0] == b'"' && p[p.len() - 1] == b'"' {
        p = &p[1..p.len() - 1];
    }
    let mut out = vec![];
    let mut i = 0;
    while i < p.len() {
        if p[i] == b'\\' && i + 4 <= p.len() && p[i + 1..i + 4].iter().all(|c| c.is_ascii_digit()) {
            let s = std::str::from_utf8(&p[i + 1..i + 4]).unwrap();
            match u64::from_str_radix(s, 8) {
                Ok(k) => {
                    out.push(k as u8);
                    i += 4;
                    continue;
                }
                Err(_) => return p.to_vec(),
            }
        }
        out.push(p[i]);
        i += 1;
    }
    out
}
