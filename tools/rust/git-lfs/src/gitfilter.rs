//! The clean and smudge filters (lfs/gitfilter_*.go): content into the object store with a
//! pointer in its place, and back; extensions (lfs.extension.*) piped through.

use crate::config::{cfg, Extension};
use crate::errors::{Error, Kind, Result};
use crate::pointer::{self, Pointer, BLOB_SIZE_CUTOFF};
use crate::tools;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// TempFile in .git/lfs/tmp with the repository's permissions.
pub fn temp_file() -> Result<(std::fs::File, String)> {
    use std::os::unix::fs::PermissionsExt;
    let dir = cfg().temp_dir();
    for i in 0..10000u32 {
        let r: u64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() as u64 ^ ((std::process::id() as u64) << 20) ^ i as u64;
        let name = format!("{dir}/{r}");
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&name) {
            Ok(f) => {
                std::fs::set_permissions(&name, std::fs::Permissions::from_mode(cfg().repository_permissions(false)))?;
                return Ok((f, name));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(tools::path_err("open", &name, &e)),
        }
    }
    Err(Error::new("could not create a temporary file"))
}

/// GIT_LFS_PROGRESS: progress lines `event i/n written/total name`, throttled.
pub struct ProgressFile {
    file: std::fs::File,
    event: String,
    name: String,
    index: usize,
    total_files: usize,
    prev: (i64, i64),
    reached: bool,
    deadline: std::time::Instant,
}

const THROTTLE: std::time::Duration = std::time::Duration::from_millis(200);

impl ProgressFile {
    pub fn open(event: &str, name: &str, index: usize, total_files: usize) -> Result<Option<ProgressFile>> {
        let Some(path) = cfg().os.get("GIT_LFS_PROGRESS").filter(|p| !p.is_empty()) else { return Ok(None) };
        if name.is_empty() || event.is_empty() {
            return Ok(None);
        }
        if !path.starts_with('/') {
            return Err(Error::new("GIT_LFS_PROGRESS must be an absolute path"));
        }
        let wrap = |e: Error| Error::new(format!("error writing Git LFS {event} progress to {path}: {e}"));
        let dir = std::path::Path::new(&path).parent().map(|p| p.display().to_string()).unwrap_or_default();
        tools::mkdir_all(&dir, cfg().repository_permissions(false)).map_err(|e| wrap(tools::path_err("mkdir", &dir, &e)))?;
        let file = std::fs::OpenOptions::new().append(true).create(true).open(&path).map_err(|e| wrap(tools::path_err("open", &path, &e)))?;
        Ok(Some(ProgressFile { file, event: event.into(), name: name.into(), index, total_files, prev: (0, 0), reached: false, deadline: std::time::Instant::now() + THROTTLE }))
    }

    pub fn update(&mut self, total: i64, written: i64) {
        let now = std::time::Instant::now();
        if total == self.prev.0 {
            if written == self.prev.1 {
                return;
            }
        } else {
            self.reached = false;
        }
        let optional = total < 0 || written < total || self.reached;
        if optional && now < self.deadline {
            return;
        }
        let _ = writeln!(self.file, "{} {}/{} {} {}", self.event, self.index, self.total_files, tools::format_fraction(written, total), self.name);
        let _ = self.file.sync_all();
        self.prev = (total, written);
        if !optional {
            self.reached = true;
        }
        self.deadline = now + THROTTLE;
    }
}

/// Copies with a progress callback (CopyWithCallback / CallbackReader): after each read, and
/// at the end with the size read as the total when it was unknown or wrong.
pub fn copy_with_progress(w: &mut dyn Write, r: &mut dyn Read, total: i64, mut cb: Option<&mut ProgressFile>) -> std::io::Result<i64> {
    let mut buf = vec![0u8; 65536];
    let mut written: i64 = 0;
    let mut total = total;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => {
                if written != total {
                    total = written;
                    if let Some(c) = cb.as_deref_mut() {
                        c.update(total, written);
                    }
                }
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        w.write_all(&buf[..n])?;
        written += n as i64;
        if let Some(c) = cb.as_deref_mut() {
            c.update(total, written);
        }
    }
    Ok(written)
}

/// Runs extensions in order: each one's output feeds the next; (final file, results of
/// (name, oid in, oid out)).
pub fn pipe_extensions(action: &str, input: &mut dyn Read, file_name: &str, exts: &[Extension]) -> Result<(String, Vec<(String, String, String)>)> {
    let mut data = vec![];
    input.read_to_end(&mut data)?;
    let mut results = vec![];
    let n = exts.len();
    for (i, e) in exts.iter().enumerate() {
        let spec = if action == "clean" { &e.clean } else { &e.smudge };
        let pieces: Vec<&str> = spec.split(' ').collect();
        let name = pieces[0].trim();
        let args: Vec<String> = pieces[1..].iter().map(|a| a.replace("%f", file_name)).collect();
        let oid_in = tools::hex(&Sha256::digest(&data));
        let mut c = std::process::Command::new(name);
        c.args(&args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        c.env_remove("GIT_TRACE");
        if i + 1 < n {
            c.stderr(std::process::Stdio::piped());
        }
        let mut child = c.spawn().map_err(|e| Error::new(format!("exec: {}: {}", tools::quote(name), tools::io_err(&e))))?;
        let mut stdin = child.stdin.take().unwrap();
        let d = std::mem::take(&mut data);
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&d);
        });
        let out = child.wait_with_output()?;
        let _ = writer.join();
        if !out.status.success() {
            if i + 1 < n {
                return Err(Error::new(format!("extension '{}' failed with: {}", e.name, String::from_utf8_lossy(&out.stderr))));
            }
            return Err(Error::new(crate::subprocess::exit_text(&out.status)));
        }
        data = out.stdout;
        results.push((e.name.clone(), oid_in, tools::hex(&Sha256::digest(&data))));
    }
    let (mut f, path) = temp_file()?;
    f.write_all(&data)?;
    Ok((path, results))
}

pub enum Cleaned {
    /// The input was already a pointer: written back as it was.
    Passthrough(Vec<u8>),
    Object { pointer: Pointer, tmp: String },
}

/// Clean: hashes the content into a temp file (or passes a pointer through).
pub fn clean(input: &mut dyn Read, file_name: &str, file_size: i64, cb: Option<&mut ProgressFile>) -> Result<Cleaned> {
    let exts = cfg().sorted_extensions()?;
    if !exts.is_empty() {
        let (tmp, results) = pipe_extensions("clean", input, file_name, &exts)?;
        let oid = results.last().unwrap().2.clone();
        let size = std::fs::metadata(&tmp)?.len() as i64;
        let mut pexts = vec![];
        for (name, oin, oout) in &results {
            if oin != oout {
                let pr = pexts.len() as i64;
                pexts.push(pointer::Extension { name: name.clone(), priority: pr, oid: oin.clone() });
            }
        }
        return Ok(Cleaned::Object { pointer: Pointer::new(&oid, size, pexts), tmp });
    }
    // The first 1024 bytes: a pointer (or nothing) passes through unchanged.
    let mut head = vec![0u8; BLOB_SIZE_CUTOFF];
    let mut got = 0;
    while got < head.len() {
        match input.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    head.truncate(got);
    let parsed = pointer::decode(&head).is_ok();
    if head.is_empty() || (parsed && head.len() < BLOB_SIZE_CUTOFF) {
        return Ok(Cleaned::Passthrough(head));
    }
    let (mut f, tmp) = temp_file()?;
    let mut hasher = Sha256::new();
    struct Tee<'a>(&'a mut std::fs::File, &'a mut Sha256);
    impl Write for Tee<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.1.update(b);
            self.0.write_all(b)?;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut tee = Tee(&mut f, &mut hasher);
    let mut chained = std::io::Cursor::new(head).chain(input);
    let size = copy_with_progress(&mut tee, &mut chained, file_size, cb)?;
    Ok(Cleaned::Object { pointer: Pointer::new(&tools::hex(&hasher.finalize()), size, vec![]), tmp })
}

/// LinkOrCopyFromReference: an object from an alternate's store, if it has it.
pub fn link_or_copy_from_reference(oid: &str, size: i64) {
    let fs = cfg().filesystem();
    if fs.object_exists(oid, size) {
        return;
    }
    let Ok(media) = fs.object_path(oid) else { return };
    for alt in fs.object_reference_paths(oid) {
        crate::trace!("altMediafile: {}", alt);
        if tools::file_exists_of_size(&alt, size) && link_or_copy(&alt, &media).is_ok() {
            break;
        }
    }
}

pub fn link_or_copy(src: &str, dst: &str) -> Result<()> {
    if src == dst {
        return Ok(());
    }
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    let (mut f, tmp) = temp_file()?;
    let mut s = std::fs::File::open(src).map_err(|e| tools::path_err("open", src, &e))?;
    std::io::copy(&mut s, &mut f)?;
    drop(f);
    std::fs::rename(&tmp, dst).map_err(|e| tools::path_err("rename", &tmp, &e))?;
    Ok(())
}

/// Smudge: the object's content (downloading it when allowed); a missing object without
/// download is a DownloadDeclined error.
pub fn smudge(w: &mut dyn Write, ptr: &Pointer, working_file: &str, download: bool, cb: Option<&mut ProgressFile>) -> Result<i64> {
    let fs = cfg().filesystem();
    let media = fs.object_path(&ptr.oid)?;
    link_or_copy_from_reference(&ptr.oid, ptr.size);
    let mut exists = false;
    if let Ok(m) = std::fs::metadata(&media) {
        if m.len() as i64 != ptr.size {
            crate::trace!("Removing {}, size {} is invalid", media, m.len());
            let _ = std::fs::remove_file(&media);
        } else {
            exists = true;
        }
    }
    if ptr.size == 0 {
        return Ok(0);
    }
    let r = if !exists {
        if download {
            let mut r = download_file(w, ptr, working_file, &media, cb);
            if r.is_err() && cfg().search_all_remotes_enabled() {
                crate::trace!("git: smudge: default remote failed. searching alternate remotes");
                r = crate::tq::download_fallback(ptr, working_file, &media).and_then(|_| read_local_file(w, ptr, &media, working_file, None));
            }
            r
        } else {
            let e = Error::new(format!("stat {}: no such file or directory", media)).wrap("smudge filter").with_kind(Kind::DownloadDeclined);
            return Err(e);
        }
    } else {
        read_local_file(w, ptr, &media, working_file, cb)
    };
    r.map_err(|e| e.wrap("Smudge error").with_kind(Kind::Smudge))
}

fn download_file(w: &mut dyn Write, ptr: &Pointer, working_file: &str, media: &str, cb: Option<&mut ProgressFile>) -> Result<i64> {
    eprintln!("Downloading {} ({})", working_file, tools::format_bytes(ptr.size as u64));
    // The GIT_LFS_PROGRESS log follows the transfer.
    let pcb: Option<crate::tq::ProgressCb> = match cb {
        Some(_) => ProgressFile::open("download", working_file, 1, 1).ok().flatten().map(|pf| {
            let pf = std::sync::Mutex::new(pf);
            std::sync::Arc::new(move |_: &str, total: i64, read: i64, _: i64| pf.lock().unwrap().update(total, read)) as crate::tq::ProgressCb
        }),
        None => None,
    };
    crate::tq::download_one(ptr, working_file, media, &cfg().remote(), pcb)?;
    read_local_file(w, ptr, media, working_file, None)
}

pub fn read_local_file(w: &mut dyn Write, ptr: &Pointer, media: &str, working_file: &str, cb: Option<&mut ProgressFile>) -> Result<i64> {
    let mut reader = std::fs::File::open(media).map_err(|e| Error::from(tools::path_err("open", media, &e)).wrap("error opening media file"))?;
    let mut size = ptr.size;
    if size == 0 {
        if let Ok(m) = std::fs::metadata(media) {
            size = m.len() as i64;
        }
    }
    let smudged;
    if !ptr.extensions.is_empty() {
        let registered = cfg().extensions();
        let mut exts: Vec<Extension> = vec![];
        for pe in &ptr.extensions {
            let Some(e) = registered.get(&pe.name) else {
                return Err(Error::new(format!("extension '{}' is not configured", pe.name)).wrap("smudge filter"));
            };
            let mut e = e.clone();
            e.priority = pe.priority;
            exts.push(e);
        }
        exts.sort_by_key(|e| e.priority);
        for w2 in exts.windows(2) {
            if w2[0].priority == w2[1].priority {
                return Err(Error::new(format!("duplicate priority {} on {}", w2[1].priority, w2[1].name)).wrap("smudge filter"));
            }
        }
        exts.reverse();
        let (file, results) = pipe_extensions("smudge", &mut reader, working_file, &exts).map_err(|e| e.wrap("smudge filter"))?;
        let oid = &results[0].1;
        if *oid != ptr.oid {
            return Err(Error::new(format!("actual OID {} during smudge does not match expected {}", oid, ptr.oid)).wrap("smudge filter"));
        }
        for exp in &ptr.extensions {
            let Some(actual) = results.iter().find(|r| r.0 == exp.name) else {
                return Err(Error::new(format!("actual extension name '' does not match expected '{}'", exp.name)).wrap("smudge filter"));
            };
            if actual.2 != exp.oid {
                return Err(Error::new(format!("actual OID {} for extension '{}' does not match expected {}", actual.2, exp.name, exp.oid)).wrap("smudge filter"));
            }
        }
        reader = std::fs::File::open(&file).map_err(|e| Error::new(format!("Error opening smudged file: {}", tools::path_err("open", &file, &e))))?;
        smudged = Some(file);
    } else {
        smudged = None;
    }
    let n = copy_with_progress(w, &mut reader, size, cb).map_err(|e| Error::new(format!("Error reading from media file: {}", tools::io_err(&e))));
    if let Some(f) = smudged {
        let _ = std::fs::remove_file(f);
    }
    n
}

/// SmudgeToFile: the object into a working tree file (the pointer when not downloaded).
pub fn smudge_to_file(path: &str, ptr: &Pointer, name: &str, download: bool, cb: Option<&mut ProgressFile>) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut mode = 0o666;
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.is_file() {
            if ptr.size == 0 && m.len() == 0 {
                return Ok(());
            }
            mode = m.permissions().mode() & 0o777;
        }
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::from(tools::path_err("remove", path, &e)).wrap(format!("could not remove working directory file {}", tools::quote(path))));
        }
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path).map_err(|e| Error::from(tools::path_err("open", path, &e)).wrap(format!("could not create working directory file {}", tools::quote(path))))?;
    match smudge(&mut f, ptr, name, download, cb) {
        Ok(_) => Ok(()),
        Err(e) if e.is(Kind::DownloadDeclined) => {
            use std::io::Seek;
            let _ = f.seek(std::io::SeekFrom::Start(0));
            let _ = f.write_all(ptr.encoded().as_bytes());
            Err(e)
        }
        Err(e) => Err(Error::new(format!("could not write working directory file: {e}"))),
    }
}
