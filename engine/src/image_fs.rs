//! The boot images over the initramfs (python, tools, add-ons, the engine's settings), served to
//! the guest read-only instead of unpacked into its memory.
//!
//! Unpacked, every file of an image stayed in the guest's ramfs for good, a whole 64 KiB page for
//! the smallest (the kernel's page size): python.cpio's 59 MB took 358 MB of the guest's memory,
//! tools.cpio's 89 MB 185 MB, and the archives themselves another 156 MB until the kernel freed
//! them (memory the host keeps resident all the same). Here the engine reads only the archives'
//! headers at boot and keeps their tree, a couple of hundred bytes a file; the guest mounts it as
//! virtio-fs (`collabo.images=TAG`) and its `/init` lays each top-level directory over the
//! initramfs's own with overlayfs, changes kept in the guest's memory. What is read comes from
//! the archive files themselves (the host's page cache, shared by every machine), and only that
//! comes into the guest.
//!
//! The tree is what the kernel's unpacking made of the same archives in the same order
//! (`init/initramfs.c`): a later file replaces an earlier one, a later directory merges into an
//! earlier one (taking its mode, owner and time), another type replaces what was there (a
//! directory only when empty), and a symlink or device node already there stays. Unlike there,
//! missing parent directories are made (the initramfs below has them), and times are the
//! archives' own (the kernel's unpacking stamped the boot time on everything).
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmtime::SharedMemory;

use crate::fs::*;
use crate::virtio::{Device, Queue};

/// The virtio-fs tag the guest mounts the images by.
pub const TAG: &str = "collabo-images";

/// How long the guest may trust what it was told: the tree never changes.
const VALIDITY: u64 = 365 * 24 * 3600;

const OP_READDIRPLUS: u32 = 44;

/// A request costs the guest a few host thread switches (some 200 µs), so it is asked for as
/// few as can be: a listing brings its entries' attributes along (READDIRPLUS), which is how
/// Python's imports find their files, so they need no lookups; opening and closing (FLUSH) cost
/// nothing, the guest keeping what it read across opens and its listings and symlinks too (what
/// a guest does when OPEN, OPENDIR and FLUSH answer ENOSYS; virtio-fs checks permissions itself,
/// so there is no ACCESS either); and what it was told stays true (VALIDITY), names that do not
/// exist too.
const INIT_FLAGS: u32 = (1 << 13) // FUSE_DO_READDIRPLUS, always (not the adaptive kind)
    | (1 << 17) // FUSE_NO_OPEN_SUPPORT
    | (1 << 18) // FUSE_PARALLEL_DIROPS
    | (1 << 23) // FUSE_CACHE_SYMLINKS
    | (1 << 24); // FUSE_NO_OPENDIR_SUPPORT

/// A cpio archive over the initramfs.
pub enum Layer {
    /// An archive file, read when the guest reads from it.
    File(PathBuf),
    /// An archive the engine made (the add-ons' and the display's settings), kept in memory.
    Bytes(Vec<u8>),
}

/// The archives a machine boots with: the initramfs, which holds `/init` and is unpacked into
/// the guest's memory, and the layers over it, served when they can be. They cannot when one is
/// not a plain newc archive (a compressed one, say) or puts something other than a directory at
/// the top (which only unpacking could lay over the initramfs's own files) or a /run (where the
/// guest mounts them), and not with
/// `COLLABO_IMAGE_SHARE=off`: then they are all unpacked after the initramfs, as before.
pub fn arrange(initramfs: Vec<u8>, layers: Vec<Layer>) -> Result<(Vec<u8>, Option<ImageDevice>)> {
    if layers.is_empty() {
        return Ok((initramfs, None));
    }
    let wanted = std::env::var_os("COLLABO_IMAGE_SHARE").is_none_or(|value| value != "off");
    if wanted {
        match Tree::build(&layers)? {
            Ok(tree) => return Ok((initramfs, Some(ImageDevice { tree }))),
            Err(why) => eprintln!("[collabo-core] the boot images are unpacked into the guest's memory: {why}"),
        }
    }
    let mut initcpio = initramfs;
    for layer in layers {
        match layer {
            Layer::File(path) => initcpio.extend(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?),
            Layer::Bytes(bytes) => initcpio.extend(bytes),
        }
    }
    Ok((initcpio, None))
}

/// Where a file's bytes are.
enum Source {
    File(File),
    Bytes(Vec<u8>),
}

impl Source {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        match self {
            Source::Bytes(bytes) => {
                let start = (offset as usize).min(bytes.len());
                let count = buffer.len().min(bytes.len() - start);
                buffer[..count].copy_from_slice(&bytes[start..start + count]);
                Ok(count)
            }
            #[cfg(unix)]
            Source::File(file) => std::os::unix::fs::FileExt::read_at(file, buffer, offset),
            #[cfg(windows)]
            Source::File(file) => std::os::windows::fs::FileExt::seek_read(file, buffer, offset),
        }
    }
}

enum Content {
    Directory(BTreeMap<String, u64>),
    File { source: usize, offset: u64 },
    Symlink(String),
    /// A device node, fifo or socket.
    Special,
}

struct Node {
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    mtime: u64,
    size: u64,
    nlink: u32,
    parent: u64,
    content: Content,
}

impl Node {
    fn directory(parent: u64, mode: u32, mtime: u64) -> Node {
        Node { mode: S_IFDIR | (mode & 0o7777), uid: 0, gid: 0, rdev: 0, mtime, size: 0, nlink: 2, parent, content: Content::Directory(BTreeMap::new()) }
    }
}

/// One entry of an archive, as its header says.
struct Entry {
    name: String,
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u64,
    size: u64,
    /// The device the archived file was on: with `ino`, what makes two names one file.
    dev: u32,
    /// A device node's own number, as FUSE has it (the kernel's new_encode_dev).
    rdev: u32,
    /// Where its data starts in the archive.
    offset: u64,
}

/// The merged archives; a node's id is its index plus one (FUSE's root is 1).
struct Tree {
    sources: Vec<Source>,
    nodes: Vec<Node>,
}

impl Tree {
    /// The tree of `layers`, or why they cannot be served (`Ok(Err)`); an archive that cannot be
    /// read at all is an error, as it was when it was read whole.
    fn build(layers: &[Layer]) -> Result<std::result::Result<Tree, String>> {
        let mut tree = Tree { sources: Vec::new(), nodes: vec![Node::directory(1, 0o755, 0)] };
        for layer in layers {
            let (source, label) = match layer {
                Layer::File(path) => {
                    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
                    (Source::File(file), path.display().to_string())
                }
                Layer::Bytes(bytes) => (Source::Bytes(bytes.clone()), "the engine's settings".to_string()),
            };
            let index = tree.sources.len();
            let entries = match &source {
                Source::File(file) => entries(BufReader::new(file.try_clone()?)),
                Source::Bytes(bytes) => entries(std::io::Cursor::new(bytes.as_slice())),
            };
            let entries = match entries {
                Ok(entries) => entries,
                Err(why) => return Ok(Err(format!("{label}: {why}"))),
            };
            tree.sources.push(source);
            if let Err(why) = tree.add(index, &entries) {
                return Ok(Err(format!("{label}: {why}")));
            }
        }
        if let Content::Directory(children) = &tree.nodes[0].content {
            for (name, id) in children {
                if tree.nodes[*id as usize - 1].mode & S_IFMT != S_IFDIR {
                    return Ok(Err(format!("/{name} is not a directory")));
                }
                // where /init mounts the images
                if name == "run" {
                    return Ok(Err("it has a /run".into()));
                }
            }
        }
        tree.count_links();
        Ok(Ok(tree))
    }

    fn node(&self, id: u64) -> FsResult<&Node> {
        match id {
            1.. => self.nodes.get(id as usize - 1).ok_or(FsError(ENOENT)),
            0 => Err(FsError(ENOENT)),
        }
    }

    fn children(&self, id: u64) -> FsResult<&BTreeMap<String, u64>> {
        match &self.node(id)?.content {
            Content::Directory(children) => Ok(children),
            _ => Err(FsError(ENOTDIR)),
        }
    }

    fn children_mut(&mut self, id: u64) -> &mut BTreeMap<String, u64> {
        match &mut self.nodes[id as usize - 1].content {
            Content::Directory(children) => children,
            _ => unreachable!("only directories are parents"),
        }
    }

    fn push(&mut self, node: Node) -> u64 {
        self.nodes.push(node);
        self.nodes.len() as u64
    }

    /// The directory at `path`, made (0755) where it is missing; `None` if something else is
    /// in the way.
    fn directory_at(&mut self, path: &[&str], mtime: u64) -> Option<u64> {
        let mut id = 1;
        for name in path {
            id = match self.children_mut(id).get(*name).copied() {
                Some(child) if self.nodes[child as usize - 1].mode & S_IFMT == S_IFDIR => child,
                Some(_) => return None,
                None => {
                    let child = self.push(Node::directory(id, 0o755, mtime));
                    self.children_mut(id).insert(name.to_string(), child);
                    child
                }
            };
        }
        Some(id)
    }

    /// Adds one archive's entries, in order.
    fn add(&mut self, source: usize, entries: &[Entry]) -> std::result::Result<(), String> {
        // hard links: the entries of one archive that share an inode number
        let mut links: std::collections::HashMap<(u32, u32), u64> = std::collections::HashMap::new();
        for entry in entries {
            let mut path: Vec<&str> = Vec::new();
            for part in entry.name.split('/') {
                match part {
                    "" | "." => {}
                    ".." => return Err(format!("{}: a name with ..", entry.name)),
                    part => path.push(part),
                }
            }
            let kind = entry.mode & S_IFMT;
            let Some((name, parents)) = path.split_last() else {
                // the archive's root: the kernel's mkdir of "." fails, its chmod applies
                if kind == S_IFDIR {
                    self.nodes[0].mode = S_IFDIR | (entry.mode & 0o7777);
                }
                continue;
            };
            let Some(parent) = self.directory_at(parents, entry.mtime) else { continue };
            let existing = self.children_mut(parent).get(*name).copied();
            let existing_kind = existing.map(|id| self.nodes[id as usize - 1].mode & S_IFMT);

            if kind == S_IFDIR {
                if existing_kind == Some(S_IFDIR) {
                    let node = &mut self.nodes[existing.unwrap() as usize - 1];
                    (node.mode, node.uid, node.gid, node.mtime) = (entry.mode, entry.uid, entry.gid, entry.mtime);
                    continue;
                }
                let mut node = Node::directory(parent, entry.mode, entry.mtime);
                (node.uid, node.gid) = (entry.uid, entry.gid);
                let id = self.push(node);
                self.children_mut(parent).insert(name.to_string(), id);
                continue;
            }
            match existing {
                // what the kernel's clean_path() would not remove: a directory with something in
                // it, and anything of the same type that is not a regular file
                Some(id) if existing_kind == Some(S_IFDIR) && !self.children(id).map_or(true, BTreeMap::is_empty) => continue,
                Some(_) if existing_kind == Some(kind) && kind != S_IFREG => continue,
                _ => {}
            }
            let content = match kind {
                S_IFREG => Content::File { source, offset: entry.offset },
                S_IFLNK => {
                    let mut target = vec![0u8; entry.size as usize];
                    let read = self.sources[source].read_at(&mut target, entry.offset).map_err(|e| e.to_string())?;
                    target.truncate(read);
                    let target = String::from_utf8(target).map_err(|_| format!("{}: a symlink target that is not UTF-8", entry.name))?;
                    Content::Symlink(target.trim_end_matches('\0').to_string())
                }
                0o010000 | 0o020000 | 0o060000 | 0o140000 => Content::Special,
                _ => continue,
            };
            if kind == S_IFREG && entry.nlink > 1 {
                if let Some(&id) = links.get(&(entry.ino, entry.dev)) {
                    // another name for a file of this archive (the data comes with one of them)
                    if entry.size > 0 {
                        let node = &mut self.nodes[id as usize - 1];
                        (node.size, node.content) = (entry.size, Content::File { source, offset: entry.offset });
                    }
                    self.children_mut(parent).insert(name.to_string(), id);
                    continue;
                }
            }
            let size = if kind == S_IFREG || kind == S_IFLNK { entry.size } else { 0 };
            let rdev = if matches!(content, Content::Special) { entry.rdev } else { 0 };
            let id = self.push(Node { mode: entry.mode, uid: entry.uid, gid: entry.gid, rdev, mtime: entry.mtime, size, nlink: 1, parent, content });
            self.children_mut(parent).insert(name.to_string(), id);
            if kind == S_IFREG && entry.nlink > 1 {
                links.insert((entry.ino, entry.dev), id);
            }
        }
        Ok(())
    }

    /// Directories: 2 and one for each subdirectory; files: one for each name.
    fn count_links(&mut self) {
        let mut counts = vec![0u32; self.nodes.len()];
        for node in &self.nodes {
            if let Content::Directory(children) = &node.content {
                for id in children.values() {
                    counts[*id as usize - 1] += 1;
                }
            }
        }
        let mut subdirectories = vec![0u32; self.nodes.len()];
        for node in &self.nodes {
            if node.mode & S_IFMT == S_IFDIR && !std::ptr::eq(node, &self.nodes[0]) {
                subdirectories[node.parent as usize - 1] += 1;
            }
        }
        for (index, node) in self.nodes.iter_mut().enumerate() {
            node.nlink = match node.mode & S_IFMT {
                S_IFDIR => 2 + subdirectories[index],
                _ => counts[index].max(1),
            };
        }
    }

    fn attributes(&self, id: u64) -> FsResult<Attributes> {
        let node = self.node(id)?;
        Ok(Attributes {
            ino: id,
            size: node.size,
            mode: node.mode,
            nlink: node.nlink,
            uid: node.uid,
            gid: node.gid,
            rdev: node.rdev,
            atime: (node.mtime, 0),
            mtime: (node.mtime, 0),
            ctime: (node.mtime, 0),
        })
    }
}

/// The entries of a newc ("070701", or "070702" with checksums) archive, or several of them one
/// after another, with only their headers and symlink targets read.
fn entries(mut archive: impl Read + Seek) -> std::result::Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut at: u64 = 0;
    let mut header = [0u8; 110];
    loop {
        // between archives (and after the last one): NUL padding, as the kernel skips it
        let mut first = [0u8; 1];
        loop {
            match archive.read(&mut first) {
                Ok(0) => return Ok(entries),
                Ok(_) if first[0] == 0 => at += 1,
                Ok(_) => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        if at % 4 != 0 {
            return Err(format!("an entry at {at}, not on a 4-byte boundary"));
        }
        header[0] = first[0];
        archive.read_exact(&mut header[1..]).map_err(|_| format!("a truncated header at {at}"))?;
        if &header[..6] != b"070701" && &header[..6] != b"070702" {
            return Err(format!("not a newc cpio archive (at {at})"));
        }
        let field = |index: usize| -> std::result::Result<u32, String> {
            let text = std::str::from_utf8(&header[6 + 8 * index..14 + 8 * index]).map_err(|_| "a bad header".to_string())?;
            u32::from_str_radix(text, 16).map_err(|_| format!("a bad header field at {at}"))
        };
        let (ino, mode, uid, gid, nlink, mtime, size) = (field(0)?, field(1)?, field(2)?, field(3)?, field(4)?, field(5)?, field(6)?);
        let (dev_major, dev_minor, rdev_major, rdev_minor, name_size) = (field(7)?, field(8)?, field(9)?, field(10)?, field(11)?);
        let mut name = vec![0u8; name_size as usize];
        archive.read_exact(&mut name).map_err(|_| format!("a truncated name at {at}"))?;
        let name = String::from_utf8(name).map_err(|_| format!("a name that is not UTF-8 at {at}"))?;
        let name = name.trim_end_matches('\0').to_string();
        let data = (at + 110 + name_size as u64 + 3) & !3;
        let next = (data + size as u64 + 3) & !3;
        // past the padding and the data, keeping what is buffered
        let skip = |archive: &mut dyn Seek| archive.seek_relative((next - (at + 110 + name_size as u64)) as i64).map_err(|error| error.to_string());
        if name == "TRAILER!!!" {
            skip(&mut archive)?;
            at = next;
            continue;
        }
        entries.push(Entry {
            name,
            ino,
            mode,
            uid,
            gid,
            nlink,
            mtime: mtime as u64,
            size: size as u64,
            dev: (dev_major << 20) | (dev_minor & 0xfffff),
            rdev: (rdev_minor & 0xff) | (rdev_major << 8) | ((rdev_minor & !0xff) << 12),
            offset: data,
        });
        skip(&mut archive)?;
        at = next;
    }
}

/// The virtio-fs device the guest mounts the images from.
pub struct ImageDevice {
    tree: Tree,
}

impl ImageDevice {
    fn write_entry(&self, out: &mut Writer, id: u64) -> FsResult<()> {
        out.u64(id).u64(1).u64(VALIDITY).u64(VALIDITY).u32(0).u32(0);
        write_attr(out, &self.tree.attributes(id)?);
        Ok(())
    }
}

impl Device for ImageDevice {
    fn device_id(&self) -> u32 {
        26
    }

    fn config(&self) -> Vec<u8> {
        config(TAG)
    }

    fn notify(&mut self, _queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        serve(self, queue, memory)
    }
}

impl Server for ImageDevice {
    fn process(&mut self, opcode: u32, nodeid: u64, body: &mut Reader, capacity: usize) -> FsResult<Option<Writer>> {
        let mut out = Writer::default();
        let room = capacity.saturating_sub(OUT_HEADER_SIZE);
        match opcode {
            OP_INIT => out = init(body, INIT_FLAGS)?,
            OP_LOOKUP => {
                let name = body.cstring()?;
                match self.tree.children(nodeid)?.get(&name).copied() {
                    Some(id) => self.write_entry(&mut out, id)?,
                    // node 0: "no such name", which the guest may remember as long as the rest
                    None => {
                        out.u64(0).u64(0).u64(VALIDITY).u64(0).u32(0).u32(0).zeros(ATTR_SIZE);
                    }
                }
            }
            // nodes are never dropped: the tree is the device's for as long as it lives
            OP_FORGET | OP_BATCH_FORGET | OP_INTERRUPT => return Ok(None),
            OP_GETATTR => {
                out.u64(VALIDITY).u32(0).u32(0);
                write_attr(&mut out, &self.tree.attributes(nodeid)?);
            }
            OP_READLINK => match &self.tree.node(nodeid)?.content {
                Content::Symlink(target) => {
                    out.bytes(target.as_bytes());
                }
                _ => return Err(FsError(EINVAL)),
            },
            // no handles: reads name the node (and the mount is read-only, so nothing is opened
            // to write)
            OP_OPEN | OP_OPENDIR | OP_FLUSH | OP_FSYNC | OP_FSYNCDIR | OP_ACCESS => return Err(FsError(ENOSYS)),
            OP_READ => {
                body.skip(8)?; // fh
                let offset = body.u64()?;
                let size = body.u32()? as usize;
                let node = self.tree.node(nodeid)?;
                let Content::File { source, offset: start } = node.content else { return Err(FsError(EBADF)) };
                let want = (size.min(room) as u64).min(node.size.saturating_sub(offset)) as usize;
                let mut buffer = vec![0u8; want];
                let mut read = 0;
                while read < want {
                    match self.tree.sources[source].read_at(&mut buffer[read..], start + offset + read as u64)? {
                        0 => break,
                        count => read += count,
                    }
                }
                buffer.truncate(read);
                out.bytes(&buffer);
            }
            OP_RELEASE | OP_RELEASEDIR => {}
            OP_READDIR | OP_READDIRPLUS => {
                body.skip(8)?; // fh
                let offset = body.u64()? as usize;
                let size = body.u32()? as usize;
                let limit = size.min(room);
                let plus = opcode == OP_READDIRPLUS;
                let parent = self.tree.node(nodeid)?.parent;
                let listing = [(".", nodeid), ("..", parent)]
                    .into_iter()
                    .chain(self.tree.children(nodeid)?.iter().map(|(name, id)| (name.as_str(), *id)));
                for (index, (name, id)) in listing.enumerate().skip(offset) {
                    let length = (if plus { ENTRY_OUT_SIZE } else { 0 } + 24 + name.len() + 7) & !7;
                    if out.bytes.len() + length > limit {
                        break;
                    }
                    if plus {
                        match index {
                            // the guest takes nothing for these from a listing
                            0 | 1 => {
                                out.zeros(ENTRY_OUT_SIZE);
                            }
                            _ => self.write_entry(&mut out, id)?,
                        }
                    }
                    out.u64(id).u64(index as u64 + 1).u32(name.len() as u32).u32(dirent_type(self.tree.node(id)?.mode));
                    out.bytes(name.as_bytes());
                    out.pad8();
                }
            }
            OP_STATFS => {
                let bytes: u64 = self.tree.nodes.iter().map(|node| node.size).sum();
                out.u64(bytes.div_ceil(4096))
                    .u64(0)
                    .u64(0)
                    .u64(self.tree.nodes.len() as u64)
                    .u64(0)
                    .u32(4096)
                    .u32(255)
                    .u32(4096)
                    .u32(0)
                    .zeros(24);
            }
            OP_SETATTR | OP_SYMLINK | OP_MKDIR | OP_UNLINK | OP_RMDIR | OP_RENAME | OP_CREATE | OP_WRITE => {
                return Err(FsError(EROFS))
            }
            OP_DESTROY => {}
            _ => return Err(FsError(ENOSYS)),
        }
        if out.bytes.len() > room {
            return Err(FsError(EIO));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut add = |ino: usize, name: &str, mode: u32, data: &[u8]| {
            let header = format!(
                "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                0, 0, 1, 1_700_000_000u32, data.len(), 0, 0, 0, 0, name.len() + 1, 0
            );
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            while out.len() % 4 != 0 {
                out.push(0);
            }
            out.extend_from_slice(data);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        };
        for (index, (name, mode, data)) in entries.iter().enumerate() {
            add(index + 1, name, *mode, data);
        }
        add(0, "TRAILER!!!", 0, &[]);
        out
    }

    fn lookup(tree: &Tree, path: &str) -> Option<u64> {
        let mut id = 1;
        for name in path.split('/') {
            id = *tree.children(id).ok()?.get(name)?;
        }
        Some(id)
    }

    fn read(tree: &Tree, id: u64) -> Vec<u8> {
        let node = tree.node(id).unwrap();
        let Content::File { source, offset } = node.content else { panic!("not a file") };
        let mut buffer = vec![0u8; node.size as usize];
        tree.sources[source].read_at(&mut buffer, offset).unwrap();
        buffer
    }

    #[test]
    fn layers_merge_as_the_kernel_unpacks_them() {
        let first = archive(&[
            ("usr", 0o040755, b""),
            ("usr/bin", 0o040755, b""),
            ("usr/bin/tool", 0o100755, b"first"),
            ("usr/lib/link", 0o120777, b"target-one"),
            ("etc/full", 0o040755, b""),
            ("etc/full/inside", 0o100644, b"x"),
            ("etc/empty", 0o040755, b""),
        ]);
        let mut second = archive(&[
            ("./usr/bin/tool", 0o100700, b"second, longer"),
            ("usr/lib/link", 0o120777, b"target-two"),
            ("etc/full", 0o100644, b"a file over a full directory"),
            ("etc/empty", 0o100644, b"a file over an empty one"),
            ("usr/bin", 0o040700, b""),
        ]);
        // two archives in one file, NUL-padded between, as concatenated images are
        second.extend_from_slice(&[0; 512]);
        second.extend(archive(&[("usr/share/doc", 0o100644, b"third")]));
        let tree = Tree::build(&[Layer::Bytes(first), Layer::Bytes(second)]).unwrap().unwrap();

        let tool = lookup(&tree, "usr/bin/tool").unwrap();
        assert_eq!(read(&tree, tool), b"second, longer");
        assert_eq!(tree.node(tool).unwrap().mode, 0o100700);
        assert_eq!(tree.node(tool).unwrap().mtime, 1_700_000_000);
        match &tree.node(lookup(&tree, "usr/lib/link").unwrap()).unwrap().content {
            Content::Symlink(target) => assert_eq!(target, "target-one"),
            _ => panic!("not a symlink"),
        }
        assert_eq!(tree.node(lookup(&tree, "etc/full").unwrap()).unwrap().mode & S_IFMT, S_IFDIR);
        assert!(lookup(&tree, "etc/full/inside").is_some());
        assert_eq!(read(&tree, lookup(&tree, "etc/empty").unwrap()), b"a file over an empty one");
        assert_eq!(tree.node(lookup(&tree, "usr/bin").unwrap()).unwrap().mode, 0o040700);
        // a parent the archive does not list
        assert_eq!(tree.node(lookup(&tree, "usr/share").unwrap()).unwrap().mode, 0o040755);
        assert_eq!(read(&tree, lookup(&tree, "usr/share/doc").unwrap()), b"third");
        assert_eq!(tree.node(lookup(&tree, "usr").unwrap()).unwrap().nlink, 5);
    }

    #[test]
    fn what_cannot_be_served_is_said() {
        let at_top = archive(&[("init", 0o100755, b"#!/bin/sh")]);
        assert!(Tree::build(&[Layer::Bytes(at_top)]).unwrap().is_err());
        let compressed = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0];
        assert!(Tree::build(&[Layer::Bytes(compressed)]).unwrap().is_err());
        let escaping = archive(&[("usr/../../etc", 0o040755, b"")]);
        assert!(Tree::build(&[Layer::Bytes(escaping)]).unwrap().is_err());
    }

    #[test]
    fn answers_lookups_reads_and_listings() {
        let tree = Tree::build(&[Layer::Bytes(archive(&[("usr/bin/tool", 0o100755, b"0123456789")]))]).unwrap().unwrap();
        let mut device = ImageDevice { tree };
        let bin = lookup(&device.tree, "usr/bin").unwrap();
        let request = |name: &str| {
            let mut bytes = name.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let answer = |device: &mut ImageDevice, opcode: u32, nodeid: u64, body: &[u8]| {
            device.process(opcode, nodeid, &mut Reader::new(body), 1 << 20).map(|out| out.unwrap().bytes)
        };

        let found = answer(&mut device, OP_LOOKUP, bin, &request("tool")).unwrap();
        let tool = u64::from_le_bytes(found[..8].try_into().unwrap());
        assert_eq!(tool, lookup(&device.tree, "usr/bin/tool").unwrap());
        let missing = answer(&mut device, OP_LOOKUP, bin, &request("nothing")).unwrap();
        assert_eq!(u64::from_le_bytes(missing[..8].try_into().unwrap()), 0, "a negative entry");

        let mut read = Vec::new();
        read.extend_from_slice(&0u64.to_le_bytes()); // no handle: the header names the node
        read.extend_from_slice(&3u64.to_le_bytes());
        read.extend_from_slice(&100u32.to_le_bytes());
        assert_eq!(answer(&mut device, OP_READ, tool, &read).unwrap(), b"3456789");

        assert_eq!(answer(&mut device, OP_OPEN, tool, &0u32.to_le_bytes()).unwrap_err().0, ENOSYS);
        assert_eq!(answer(&mut device, OP_MKDIR, bin, &[0; 16]).unwrap_err().0, EROFS);

        let mut listing = Vec::new();
        listing.extend_from_slice(&0u64.to_le_bytes());
        listing.extend_from_slice(&0u64.to_le_bytes());
        listing.extend_from_slice(&4096u32.to_le_bytes());
        let names = answer(&mut device, OP_READDIR, bin, &listing).unwrap();
        let text = String::from_utf8_lossy(&names);
        assert!(text.contains("tool") && text.contains(".."));
        // with the entry of each name: ".", ".." (empty), then "tool" with its node
        let plus = answer(&mut device, OP_READDIRPLUS, bin, &listing).unwrap();
        let entry = |name_length: usize| (ENTRY_OUT_SIZE + 24 + name_length + 7) & !7;
        let third = entry(1) + entry(2);
        assert_eq!(u64::from_le_bytes(plus[third..third + 8].try_into().unwrap()), tool);
        assert!(String::from_utf8_lossy(&plus).contains("tool"));
    }
}
