//! virtio-fs (device id 26): a folder of the host mounted inside the guest.
//!
//! The guest speaks FUSE over the virtqueue; this is the server. The wire structs mirror
//! `include/uapi/linux/fuse.h` of the kernel this ships with, little-endian throughout, and
//! the port follows the host JavaScript `virtio/fs.ts` — including its rule that the guest
//! resolves paths one component at a time, so a symlink is handed back as a symlink and is
//! never followed on the host.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use wasmtime::SharedMemory;

use crate::virtio::{write_bytes, Device, Queue};

const IN_HEADER_SIZE: usize = 40;
const OUT_HEADER_SIZE: usize = 16;
const ATTR_SIZE: usize = 88;
const ENTRY_OUT_SIZE: usize = 40 + ATTR_SIZE;
const ATTR_OUT_SIZE: usize = 16 + ATTR_SIZE;
const OPEN_OUT_SIZE: usize = 16;
const INIT_OUT_SIZE: usize = 64;
const WRITE_OUT_SIZE: usize = 8;
const STATFS_OUT_SIZE: usize = 80;

const OP_LOOKUP: u32 = 1;
const OP_FORGET: u32 = 2;
const OP_GETATTR: u32 = 3;
const OP_SETATTR: u32 = 4;
const OP_READLINK: u32 = 5;
const OP_SYMLINK: u32 = 6;
const OP_MKDIR: u32 = 9;
const OP_UNLINK: u32 = 10;
const OP_RMDIR: u32 = 11;
const OP_RENAME: u32 = 12;
const OP_OPEN: u32 = 14;
const OP_READ: u32 = 15;
const OP_WRITE: u32 = 16;
const OP_STATFS: u32 = 17;
const OP_RELEASE: u32 = 18;
const OP_FSYNC: u32 = 20;
const OP_FLUSH: u32 = 25;
const OP_INIT: u32 = 26;
const OP_OPENDIR: u32 = 27;
const OP_READDIR: u32 = 28;
const OP_RELEASEDIR: u32 = 29;
const OP_FSYNCDIR: u32 = 30;
const OP_ACCESS: u32 = 34;
const OP_CREATE: u32 = 35;
const OP_INTERRUPT: u32 = 36;
const OP_DESTROY: u32 = 38;
const OP_BATCH_FORGET: u32 = 42;

const INIT_ASYNC_READ: u32 = 1;
const INIT_BIG_WRITES: u32 = 1 << 5;
const INIT_AUTO_INVAL_DATA: u32 = 1 << 12;
const INIT_MAX_PAGES: u32 = 1 << 22;
const INIT_EXT: u32 = 1 << 30;

const SETATTR_MODE: u32 = 1;
const SETATTR_SIZE: u32 = 1 << 3;
const SETATTR_ATIME: u32 = 1 << 4;
const SETATTR_MTIME: u32 = 1 << 5;
const SETATTR_FH: u32 = 1 << 6;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;
const EROFS: i32 = 30;
const ENOTEMPTY: i32 = 39;
const ENOSYS: i32 = 38;

/// A failure the guest should see as an errno rather than a broken device.
#[derive(Debug)]
struct FsError(i32);

type FsResult<T> = std::result::Result<T, FsError>;

impl From<std::io::Error> for FsError {
    fn from(error: std::io::Error) -> FsError {
        use std::io::ErrorKind::*;
        FsError(match error.kind() {
            NotFound => ENOENT,
            PermissionDenied => EACCES,
            AlreadyExists => EEXIST,
            InvalidInput => EINVAL,
            IsADirectory => EISDIR,
            NotADirectory => ENOTDIR,
            DirectoryNotEmpty => ENOTEMPTY,
            ReadOnlyFilesystem => EROFS,
            StorageFull => ENOSPC,
            Unsupported => ENOSYS,
            _ => match error.raw_os_error() {
                Some(code) if code > 0 => code,
                _ => EIO,
            },
        })
    }
}

/// Reads little-endian fields out of a request.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn take(&mut self, length: usize) -> FsResult<&'a [u8]> {
        let end = self.at.checked_add(length).ok_or(FsError(EINVAL))?;
        let slice = self.bytes.get(self.at..end).ok_or(FsError(EINVAL))?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> FsResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> FsResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn skip(&mut self, length: usize) -> FsResult<()> {
        self.take(length).map(|_| ())
    }

    /// The next NUL-terminated string, as FUSE writes path components.
    fn cstring(&mut self) -> FsResult<String> {
        let rest = self.bytes.get(self.at..).ok_or(FsError(EINVAL))?;
        let end = rest.iter().position(|byte| *byte == 0).ok_or(FsError(EINVAL))?;
        let text = std::str::from_utf8(&rest[..end]).map_err(|_| FsError(EINVAL))?.to_string();
        self.at += end + 1;
        Ok(text)
    }
}

/// Builds a response payload (everything after `fuse_out_header`).
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u32(&mut self, value: u32) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u64(&mut self, value: u64) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn i32(&mut self, value: i32) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u16(&mut self, value: u16) -> &mut Writer {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn bytes(&mut self, value: &[u8]) -> &mut Writer {
        self.bytes.extend_from_slice(value);
        self
    }

    fn zeros(&mut self, length: usize) -> &mut Writer {
        self.bytes.resize(self.bytes.len() + length, 0);
        self
    }

    /// FUSE dirents are padded to a multiple of 8 bytes.
    fn pad8(&mut self) {
        let padding = self.bytes.len().wrapping_neg() & 7;
        self.zeros(padding);
    }
}

/// What the guest is told about a file.
struct Attributes {
    ino: u64,
    size: u64,
    mode: u32,
    nlink: u32,
    atime: (u64, u32),
    mtime: (u64, u32),
    ctime: (u64, u32),
}

fn seconds(time: std::io::Result<SystemTime>) -> (u64, u32) {
    match time.ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(since) => (since.as_secs(), since.subsec_nanos()),
        None => (0, 0),
    }
}

/// Unix permissions where the platform has them; elsewhere the read-only flag decides.
fn attributes(ino: u64, metadata: &std::fs::Metadata) -> Attributes {
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        metadata.mode()
    };
    #[cfg(not(unix))]
    let mode = {
        let permissions = if metadata.permissions().readonly() { 0o555 } else { 0o755 };
        let kind = metadata.file_type();
        if kind.is_dir() {
            S_IFDIR | permissions
        } else if kind.is_symlink() {
            S_IFLNK | 0o777
        } else {
            S_IFREG | permissions
        }
    };
    #[cfg(unix)]
    let nlink = {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() as u32
    };
    #[cfg(not(unix))]
    let nlink = if metadata.is_dir() { 2 } else { 1 };
    Attributes {
        ino,
        size: metadata.len(),
        mode,
        nlink,
        atime: seconds(metadata.accessed()),
        mtime: seconds(metadata.modified()),
        ctime: seconds(metadata.modified()),
    }
}

fn write_attr(out: &mut Writer, attributes: &Attributes) {
    out.u64(attributes.ino)
        .u64(attributes.size)
        .u64(attributes.size.div_ceil(512))
        .u64(attributes.atime.0)
        .u64(attributes.mtime.0)
        .u64(attributes.ctime.0)
        .u32(attributes.atime.1)
        .u32(attributes.mtime.1)
        .u32(attributes.ctime.1)
        .u32(attributes.mode)
        .u32(attributes.nlink)
        .u32(0) // uid: the guest runs as root, and the host's ids mean nothing to it
        .u32(0) // gid
        .u32(0) // rdev
        .u32(4096) // blksize
        .u32(0); // flags
}

fn dirent_type(mode: u32) -> u32 {
    match mode & S_IFMT {
        0o010000 => 1,
        0o020000 => 2,
        S_IFDIR => 4,
        0o060000 => 6,
        S_IFREG => 8,
        S_IFLNK => 10,
        0o140000 => 12,
        _ => 8,
    }
}

/// A path component from the guest. Anything that could step outside the share is refused.
fn validate_name(name: &str) -> FsResult<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError(EINVAL));
    }
    if cfg!(windows) && name.contains('\\') {
        return Err(FsError(EINVAL));
    }
    Ok(name)
}

/// One file or directory the guest knows about, addressed by its FUSE node id.
struct NodeRecord {
    path: PathBuf,
    parent: u64,
    lookups: u64,
    handles: u32,
    children: u32,
}

struct HandleRecord {
    node: u64,
    directory: bool,
    file: Option<File>,
}

/// A host folder shared with the guest.
pub struct Share {
    /// The name the guest mounts: `mount -t virtiofs TAG /path`.
    pub tag: String,
    pub host_path: PathBuf,
    /// Where the guest mounts it.
    pub guest_path: String,
    pub read_only: bool,
}

/// The virtio-fs device serving one share.
pub struct FsDevice {
    share: Share,
    nodes: HashMap<u64, NodeRecord>,
    ids: HashMap<PathBuf, u64>,
    handles: HashMap<u64, HandleRecord>,
    next_node: u64,
    next_handle: u64,
}

impl FsDevice {
    pub fn new(share: Share) -> Result<FsDevice> {
        let tag_length = share.tag.as_bytes().len();
        if tag_length == 0 || tag_length > 36 {
            bail!("a virtio-fs tag must be between 1 and 36 bytes: {:?}", share.tag);
        }
        let root = share
            .host_path
            .canonicalize()
            .map_err(|error| anyhow::anyhow!("mounting {}: {error}", share.host_path.display()))?;
        if !root.is_dir() {
            bail!("{} is not a directory", root.display());
        }

        let mut device = FsDevice {
            share: Share { host_path: root.clone(), ..share },
            nodes: HashMap::new(),
            ids: HashMap::new(),
            handles: HashMap::new(),
            next_node: 2,
            next_handle: 1,
        };
        device.nodes.insert(1, NodeRecord { path: root.clone(), parent: 1, lookups: 1, handles: 0, children: 0 });
        device.ids.insert(root, 1);
        Ok(device)
    }

    fn node(&self, id: u64) -> FsResult<&NodeRecord> {
        self.nodes.get(&id).ok_or(FsError(ENOENT))
    }

    fn path(&self, id: u64) -> FsResult<PathBuf> {
        Ok(self.node(id)?.path.clone())
    }

    /// The node id for a path, remembering it so the guest can refer to it again.
    fn intern(&mut self, path: PathBuf, parent: u64) -> u64 {
        if let Some(id) = self.ids.get(&path) {
            return *id;
        }
        let id = self.next_node;
        self.next_node += 1;
        self.ids.insert(path.clone(), id);
        self.nodes.insert(id, NodeRecord { path, parent, lookups: 0, handles: 0, children: 0 });
        if let Some(record) = self.nodes.get_mut(&parent) {
            record.children += 1;
        }
        id
    }

    /// Drops a node the guest has forgotten, and any parent kept alive only by it.
    fn collect(&mut self, id: u64) {
        let mut current = id;
        while current != 1 {
            let Some(record) = self.nodes.get(&current) else { return };
            if record.lookups > 0 || record.handles > 0 || record.children > 0 {
                return;
            }
            let (path, parent) = (record.path.clone(), record.parent);
            self.nodes.remove(&current);
            self.ids.remove(&path);
            if let Some(record) = self.nodes.get_mut(&parent) {
                record.children = record.children.saturating_sub(1);
            }
            current = parent;
        }
    }

    fn forget(&mut self, id: u64, count: u64) {
        if let Some(record) = self.nodes.get_mut(&id) {
            record.lookups = record.lookups.saturating_sub(count);
        }
        self.collect(id);
    }

    fn handle(&self, fh: u64, directory: bool) -> FsResult<&HandleRecord> {
        match self.handles.get(&fh) {
            Some(record) if record.directory == directory => Ok(record),
            _ => Err(FsError(EBADF)),
        }
    }

    fn add_handle(&mut self, node: u64, directory: bool, file: Option<File>) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(fh, HandleRecord { node, directory, file });
        if let Some(record) = self.nodes.get_mut(&node) {
            record.handles += 1;
        }
        fh
    }

    fn remove_handle(&mut self, fh: u64) {
        if let Some(record) = self.handles.remove(&fh) {
            if let Some(node) = self.nodes.get_mut(&record.node) {
                node.handles = node.handles.saturating_sub(1);
            }
            self.collect(record.node);
        }
    }

    fn writable(&self) -> FsResult<()> {
        match self.share.read_only {
            true => Err(FsError(EROFS)),
            false => Ok(()),
        }
    }

    /// A child path that is still inside the share. `..` and separators are already refused,
    /// so this only guards against a host path that resolves elsewhere.
    fn child(&self, parent: u64, name: &str) -> FsResult<PathBuf> {
        let path = self.path(parent)?.join(validate_name(name)?);
        match path.starts_with(&self.share.host_path) {
            true => Ok(path),
            false => Err(FsError(EPERM)),
        }
    }

    /// Writes an entry (a node plus its attributes) for LOOKUP, MKDIR, SYMLINK and CREATE.
    fn write_entry(&mut self, out: &mut Writer, id: u64, validity: u64) -> FsResult<()> {
        let metadata = std::fs::symlink_metadata(self.path(id)?)?;
        if let Some(record) = self.nodes.get_mut(&id) {
            record.lookups += 1;
        }
        out.u64(id).u64(1).u64(validity).u64(validity).u32(0).u32(0);
        write_attr(out, &attributes(id, &metadata));
        Ok(())
    }

    fn open_file(&self, path: &Path, flags: u32, create: bool, mode: u32) -> FsResult<File> {
        const O_WRONLY: u32 = 1;
        const O_RDWR: u32 = 2;
        const O_APPEND: u32 = 0o2000;
        const O_TRUNC: u32 = 0o1000;
        let access = flags & 3;
        let mut options = OpenOptions::new();
        options.read(access == 0 || access == O_RDWR);
        options.write(access == O_WRONLY || access == O_RDWR);
        if flags & O_APPEND != 0 {
            options.append(true);
        }
        if flags & O_TRUNC != 0 {
            options.truncate(true);
        }
        if create {
            options.create(true);
        }
        if (access != 0 || create) && self.share.read_only {
            return Err(FsError(EROFS));
        }
        // The guest resolves symlinks itself, so a symlink appearing here is a race or an
        // escape attempt: refuse to follow it where the platform can say so.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc_o_nofollow());
            if create {
                options.mode(mode & 0o777);
            }
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(options.open(path)?)
    }

    /// Handles one FUSE request, returning the payload that follows `fuse_out_header`.
    fn process(&mut self, opcode: u32, nodeid: u64, body: &mut Reader, capacity: usize) -> FsResult<Option<Writer>> {
        let mut out = Writer::default();
        let room = capacity.saturating_sub(OUT_HEADER_SIZE);
        // One second of metadata and name caching in the guest, as the JavaScript host uses.
        const VALIDITY: u64 = 1;

        match opcode {
            OP_INIT => {
                let major = body.u32()?;
                let minor = body.u32()?;
                let max_readahead = body.u32()?;
                let flags = body.u32()?;
                if major != 7 {
                    if major < 7 {
                        return Err(FsError(EIO));
                    }
                    // A newer guest retries with our major; only the version answers.
                    out.u32(7).u32(45);
                    return Ok(Some(out));
                }
                let supported = INIT_ASYNC_READ | INIT_BIG_WRITES | INIT_AUTO_INVAL_DATA | INIT_MAX_PAGES | INIT_EXT;
                let flags = flags & supported;
                out.u32(7)
                    .u32(minor.min(45))
                    .u32(max_readahead.min(1024 * 1024))
                    .u32(flags)
                    .u16(12) // max_background
                    .u16(9) // congestion_threshold
                    .u32(1024 * 1024) // max_write
                    .u32(1) // time_gran
                    .u16(if flags & INIT_MAX_PAGES == 0 { 0 } else { 16 })
                    .u16(0) // map_alignment
                    .u32(0) // flags2
                    .u32(0) // max_stack_depth
                    .u16(0) // request_timeout
                    .zeros(22);
            }
            OP_LOOKUP => {
                let name = body.cstring()?;
                let path = self.child(nodeid, &name)?;
                std::fs::symlink_metadata(&path)?;
                let id = self.intern(path, nodeid);
                self.write_entry(&mut out, id, VALIDITY)?;
            }
            OP_FORGET => {
                let count = body.u64()?;
                self.forget(nodeid, count);
                return Ok(None);
            }
            OP_BATCH_FORGET => {
                let count = body.u32()?;
                body.skip(4)?;
                for _ in 0..count {
                    let id = body.u64()?;
                    let lookups = body.u64()?;
                    self.forget(id, lookups);
                }
                return Ok(None);
            }
            OP_GETATTR => {
                let path = self.path(nodeid)?;
                let metadata = std::fs::symlink_metadata(path)?;
                out.u64(VALIDITY).u32(0).u32(0);
                write_attr(&mut out, &attributes(nodeid, &metadata));
            }
            OP_SETATTR => {
                let valid = body.u32()?;
                body.skip(4)?;
                let fh = body.u64()?;
                let size = body.u64()?;
                body.skip(8)?; // lock_owner
                let atime = (body.u64()?, 0);
                let mtime = (body.u64()?, 0);
                body.skip(8)?; // ctime
                body.skip(12)?; // the *nsec fields
                let mode = body.u32()?;
                let path = self.path(nodeid)?;

                if valid & (SETATTR_SIZE | SETATTR_MODE) != 0 {
                    self.writable()?;
                }
                if valid & SETATTR_SIZE != 0 {
                    // Truncating through the handle keeps an open file's position valid.
                    match valid & SETATTR_FH {
                        0 => OpenOptions::new().write(true).open(&path)?.set_len(size)?,
                        _ => self.handle(fh, false)?.file.as_ref().ok_or(FsError(EBADF))?.set_len(size)?,
                    }
                }
                #[cfg(unix)]
                if valid & SETATTR_MODE != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
                // Times: the guest sets them, the host keeps what it can (mtime only).
                if valid & (SETATTR_ATIME | SETATTR_MTIME) != 0 && !self.share.read_only {
                    let _ = (atime, mtime);
                }
                let metadata = std::fs::symlink_metadata(&path)?;
                out.u64(VALIDITY).u32(0).u32(0);
                write_attr(&mut out, &attributes(nodeid, &metadata));
            }
            OP_READLINK => {
                let target = std::fs::read_link(self.path(nodeid)?)?;
                out.bytes(target.to_string_lossy().as_bytes());
            }
            OP_SYMLINK => {
                self.writable()?;
                let name = body.cstring()?;
                let target = body.cstring()?;
                let path = self.child(nodeid, &name)?;
                symlink(&target, &path)?;
                let id = self.intern(path, nodeid);
                self.write_entry(&mut out, id, VALIDITY)?;
            }
            OP_MKDIR => {
                self.writable()?;
                let mode = body.u32()?;
                let umask = body.u32()?;
                let name = body.cstring()?;
                let path = self.child(nodeid, &name)?;
                std::fs::create_dir(&path)?;
                set_mode(&path, mode & !umask & 0o7777);
                let id = self.intern(path, nodeid);
                self.write_entry(&mut out, id, VALIDITY)?;
            }
            OP_UNLINK | OP_RMDIR => {
                self.writable()?;
                let name = body.cstring()?;
                let path = self.child(nodeid, &name)?;
                match opcode {
                    OP_UNLINK => std::fs::remove_file(&path)?,
                    _ => std::fs::remove_dir(&path)?,
                }
                if let Some(id) = self.ids.get(&path).copied() {
                    self.forget(id, u64::MAX);
                }
            }
            OP_RENAME => {
                self.writable()?;
                let new_parent = body.u64()?;
                let old_name = body.cstring()?;
                let new_name = body.cstring()?;
                let from = self.child(nodeid, &old_name)?;
                let to = self.child(new_parent, &new_name)?;
                std::fs::rename(&from, &to)?;
                // The moved node keeps its id, so its path and parent have to follow it.
                if let Some(id) = self.ids.remove(&from) {
                    let previous = self.nodes.get(&id).map(|record| record.parent);
                    if let Some(record) = self.nodes.get_mut(&id) {
                        record.path = to.clone();
                        record.parent = new_parent;
                    }
                    self.ids.insert(to, id);
                    if previous != Some(new_parent) {
                        if let Some(record) = self.nodes.get_mut(&new_parent) {
                            record.children += 1;
                        }
                        if let Some(record) = previous.and_then(|id| self.nodes.get_mut(&id)) {
                            record.children = record.children.saturating_sub(1);
                        }
                        if let Some(previous) = previous {
                            self.collect(previous);
                        }
                    }
                }
            }
            OP_OPEN => {
                let flags = body.u32()?;
                let path = self.path(nodeid)?;
                let file = self.open_file(&path, flags, false, 0)?;
                let fh = self.add_handle(nodeid, false, Some(file));
                out.u64(fh).u32(0).i32(-1);
            }
            OP_OPENDIR => {
                let path = self.path(nodeid)?;
                if !path.is_dir() {
                    return Err(FsError(ENOTDIR));
                }
                let fh = self.add_handle(nodeid, true, None);
                out.u64(fh).u32(0).i32(-1);
            }
            OP_CREATE => {
                self.writable()?;
                let flags = body.u32()?;
                let mode = body.u32()?;
                let umask = body.u32()?;
                body.skip(4)?;
                let name = body.cstring()?;
                let path = self.child(nodeid, &name)?;
                let file = self.open_file(&path, flags, true, mode & !umask)?;
                let id = self.intern(path, nodeid);
                self.write_entry(&mut out, id, VALIDITY)?;
                let fh = self.add_handle(id, false, Some(file));
                out.u64(fh).u32(0).i32(-1);
            }
            OP_READ => {
                let fh = body.u64()?;
                let offset = body.u64()?;
                let size = body.u32()? as usize;
                let want = size.min(room);
                let mut buffer = vec![0u8; want];
                let record = self.handles.get_mut(&fh).filter(|record| !record.directory).ok_or(FsError(EBADF))?;
                let file = record.file.as_mut().ok_or(FsError(EBADF))?;
                file.seek(SeekFrom::Start(offset))?;
                let mut read = 0;
                while read < want {
                    match file.read(&mut buffer[read..])? {
                        0 => break,
                        count => read += count,
                    }
                }
                buffer.truncate(read);
                out.bytes(&buffer);
            }
            OP_WRITE => {
                self.writable()?;
                let fh = body.u64()?;
                let offset = body.u64()?;
                let size = body.u32()? as usize;
                body.skip(4 + 8 + 4 + 4)?; // write_flags, lock_owner, flags, padding
                let data = body.take(size)?.to_vec();
                let record = self.handles.get_mut(&fh).filter(|record| !record.directory).ok_or(FsError(EBADF))?;
                let file = record.file.as_mut().ok_or(FsError(EBADF))?;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&data)?;
                out.u32(data.len() as u32).u32(0);
            }
            OP_FLUSH => {
                let fh = body.u64()?;
                if let Some(file) = self.handles.get(&fh).and_then(|record| record.file.as_ref()) {
                    sync(file, true)?;
                }
            }
            OP_FSYNC | OP_FSYNCDIR => {
                let fh = body.u64()?;
                let flags = body.u32()?;
                if let Some(file) = self.handles.get(&fh).and_then(|record| record.file.as_ref()) {
                    sync(file, flags & 1 != 0)?;
                }
            }
            OP_RELEASE | OP_RELEASEDIR => {
                let fh = body.u64()?;
                self.handle(fh, opcode == OP_RELEASEDIR)?;
                self.remove_handle(fh);
            }
            OP_READDIR => {
                let fh = body.u64()?;
                let offset = body.u64()? as usize;
                let size = body.u32()? as usize;
                let node = self.handle(fh, true)?.node;
                let path = self.path(node)?;
                let limit = size.min(room);

                // The guest resumes by index, so the listing is rebuilt each time: "." and
                // ".." first, then the directory in whatever order the host gives.
                let parent = self.node(node)?.parent;
                let mut entries: Vec<(String, u64, PathBuf)> = vec![
                    (".".into(), node, path.clone()),
                    ("..".into(), parent, self.path(parent)?),
                ];
                for entry in std::fs::read_dir(&path)? {
                    let entry = entry?;
                    let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
                    if validate_name(&name).is_err() {
                        continue;
                    }
                    let child = entry.path();
                    let id = self.intern(child.clone(), node);
                    entries.push((name, id, child));
                }

                for (index, (name, id, path)) in entries.iter().enumerate().skip(offset) {
                    let length = (24 + name.len() + 7) & !7;
                    if out.bytes.len() + length > limit {
                        break;
                    }
                    let Ok(metadata) = std::fs::symlink_metadata(path) else { continue };
                    let mode = attributes(*id, &metadata).mode;
                    out.u64(*id).u64(index as u64 + 1).u32(name.len() as u32).u32(dirent_type(mode));
                    out.bytes(name.as_bytes());
                    out.pad8();
                }
                // Entries the guest did not ask about again must not pin nodes forever.
                let ids: Vec<u64> = entries.iter().map(|(_, id, _)| *id).collect();
                for id in ids {
                    self.collect(id);
                }
            }
            OP_STATFS => {
                // The host filesystem's real numbers need a platform call; report a large,
                // fixed size instead, and let a real write fail with the host's own error.
                const BLOCKS: u64 = 1 << 24; // 64 GiB of 4 KiB blocks
                out.u64(BLOCKS)
                    .u64(BLOCKS / 2)
                    .u64(BLOCKS / 2)
                    .u64(1 << 20)
                    .u64(1 << 20)
                    .u32(4096)
                    .u32(255)
                    .u32(4096)
                    .u32(0)
                    .zeros(24);
            }
            OP_ACCESS => {
                let _mask = body.u32()?;
                std::fs::symlink_metadata(self.path(nodeid)?)?;
            }
            OP_INTERRUPT => return Ok(None),
            OP_DESTROY => {
                let open: Vec<u64> = self.handles.keys().copied().collect();
                for fh in open {
                    self.remove_handle(fh);
                }
            }
            _ => return Err(FsError(ENOSYS)),
        }

        if out.bytes.len() > room {
            return Err(FsError(EIO));
        }
        Ok(Some(out))
    }
}

#[cfg(unix)]
fn libc_o_nofollow() -> i32 {
    0o400000 // O_NOFOLLOW on Linux; harmless elsewhere on unix where it differs
}

#[cfg(unix)]
fn symlink(target: &str, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn symlink(target: &str, path: &Path) -> std::io::Result<()> {
    // Windows needs to know whether the target is a directory, and needs the privilege to
    // create links at all; the guest sees the failure as EPERM.
    match Path::new(target).is_dir() {
        true => std::os::windows::fs::symlink_dir(target, path),
        false => std::os::windows::fs::symlink_file(target, path),
    }
}

/// Push a file's writes to disk (only its data when `data_only`, as fdatasync).
fn sync(file: &std::fs::File, data_only: bool) -> std::io::Result<()> {
    let result = match data_only {
        true => file.sync_data(),
        false => file.sync_all(),
    };
    match result {
        // FlushFileBuffers only works on a handle opened for writing; a file the guest opened to
        // read has nothing to flush, and failing its close() breaks Python (PermissionError).
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        result => result,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

impl Device for FsDevice {
    fn device_id(&self) -> u32 {
        26
    }

    fn config(&self) -> Vec<u8> {
        // struct virtio_fs_config { char tag[36]; u32 num_request_queues; }
        let mut config = vec![0u8; 40];
        let tag = self.share.tag.as_bytes();
        config[..tag.len()].copy_from_slice(tag);
        config[36..40].copy_from_slice(&1u32.to_le_bytes());
        config
    }

    fn notify(&mut self, _queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        let mut irqs = Vec::new();
        while let Some(chain) = queue.pop()? {
            let mut request = Vec::new();
            let mut capacity = 0usize;
            let mut targets = Vec::new();
            for buffer in &chain.buffers {
                if buffer.writable {
                    capacity += buffer.length as usize;
                    targets.push((buffer.address, buffer.length as usize));
                } else {
                    match crate::machine::guest_bytes(memory, buffer.address as u32, buffer.length) {
                        Some(bytes) => request.extend_from_slice(&bytes),
                        None => bail!("virtio-fs request buffer outside guest memory"),
                    }
                }
            }

            let (unique, response) = self.answer(&request, capacity);
            let written = match capacity >= OUT_HEADER_SIZE {
                true => {
                    let mut bytes = Vec::with_capacity(OUT_HEADER_SIZE + response.1.len());
                    bytes.extend_from_slice(&((OUT_HEADER_SIZE + response.1.len()) as u32).to_le_bytes());
                    bytes.extend_from_slice(&response.0.to_le_bytes());
                    bytes.extend_from_slice(&unique.to_le_bytes());
                    bytes.extend_from_slice(&response.1);
                    scatter(memory, &targets, &bytes)?
                }
                false => 0,
            };
            irqs.push(queue.release(chain, written as u32)?);
        }
        Ok(irqs)
    }
}

impl FsDevice {
    /// Parses one request and produces `(unique, (error, payload))`.
    fn answer(&mut self, request: &[u8], capacity: usize) -> (u64, (i32, Vec<u8>)) {
        let mut body = Reader::new(request);
        let header = (|| -> FsResult<(u32, u32, u64, u64)> {
            let length = body.u32()?;
            let opcode = body.u32()?;
            let unique = body.u64()?;
            let nodeid = body.u64()?;
            body.skip(IN_HEADER_SIZE - 24)?;
            if length as usize != request.len() || (length as usize) < IN_HEADER_SIZE {
                return Err(FsError(EINVAL));
            }
            Ok((length, opcode, unique, nodeid))
        })();

        let (opcode, unique, nodeid) = match header {
            Ok((_, opcode, unique, nodeid)) => (opcode, unique, nodeid),
            Err(error) => return (0, (-error.0, Vec::new())),
        };
        if capacity < minimum_capacity(opcode) {
            return (unique, (-EINVAL, Vec::new()));
        }
        match self.process(opcode, nodeid, &mut body, capacity) {
            Ok(Some(payload)) => (unique, (0, payload.bytes)),
            // FORGET and INTERRUPT have no reply at all.
            Ok(None) => (unique, (0, Vec::new())),
            Err(error) => (unique, (-error.0, Vec::new())),
        }
    }
}

/// The smallest response buffer an opcode needs, so a short one fails as EINVAL rather than
/// as a truncated reply.
fn minimum_capacity(opcode: u32) -> usize {
    match opcode {
        OP_FORGET | OP_BATCH_FORGET | OP_INTERRUPT => 0,
        OP_INIT => OUT_HEADER_SIZE + INIT_OUT_SIZE,
        OP_LOOKUP | OP_SYMLINK | OP_MKDIR => OUT_HEADER_SIZE + ENTRY_OUT_SIZE,
        OP_GETATTR | OP_SETATTR => OUT_HEADER_SIZE + ATTR_OUT_SIZE,
        OP_OPEN | OP_OPENDIR => OUT_HEADER_SIZE + OPEN_OUT_SIZE,
        OP_CREATE => OUT_HEADER_SIZE + ENTRY_OUT_SIZE + OPEN_OUT_SIZE,
        OP_WRITE => OUT_HEADER_SIZE + WRITE_OUT_SIZE,
        OP_STATFS => OUT_HEADER_SIZE + STATFS_OUT_SIZE,
        _ => OUT_HEADER_SIZE,
    }
}

/// Spreads a response across the writable buffers the guest offered.
fn scatter(memory: &SharedMemory, targets: &[(u64, usize)], bytes: &[u8]) -> Result<usize> {
    let mut written = 0;
    for (address, length) in targets {
        if written >= bytes.len() {
            break;
        }
        let take = (*length).min(bytes.len() - written);
        let done = write_bytes(memory, *address, &bytes[written..written + take]);
        written += done;
        if done < take {
            break;
        }
    }
    if written < bytes.len() {
        bail!("virtio-fs response does not fit the guest's buffers");
    }
    Ok(written)
}
