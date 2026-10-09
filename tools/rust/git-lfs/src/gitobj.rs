//! Git objects for history rewriting (what git-lfs uses github.com/git-lfs/gitobj for):
//! objects read through `git cat-file --batch`, new ones written as loose objects (zlib) with
//! the repository's hash (SHA-1 or SHA-256); trees and commits parsed and encoded as git does.

use crate::errors::{Error, Result};
use crate::gitscanner::ObjectReader;
use crate::tools;
use std::io::Write;

pub struct Db {
    reader: ObjectReader,
    objdir: String,
    sha256: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: Vec<u8>,
}

impl TreeEntry {
    pub fn is_tree(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub fn is_blob(&self) -> bool {
        let t = self.mode & 0o170000;
        t == 0o100000 || t == 0o120000
    }
    pub fn is_link(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
}

/// Git's tree order: names compared as bytes, a tree's as if it ended in '/'.
fn tree_order(a: &TreeEntry, b: &TreeEntry) -> std::cmp::Ordering {
    let ka: Vec<u8> = a.name.iter().copied().chain(if a.is_tree() { Some(b'/') } else { None }).collect();
    let kb: Vec<u8> = b.name.iter().copied().chain(if b.is_tree() { Some(b'/') } else { None }).collect();
    ka.cmp(&kb)
}

pub fn parse_tree(data: &[u8], oid_len: usize) -> Result<Vec<TreeEntry>> {
    let mut v = vec![];
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..].iter().position(|&c| c == b' ').ok_or_else(|| Error::new("malformed tree"))? + i;
        let nul = data[sp..].iter().position(|&c| c == 0).ok_or_else(|| Error::new("malformed tree"))? + sp;
        let mode = u32::from_str_radix(std::str::from_utf8(&data[i..sp]).unwrap_or("0"), 8).map_err(|_| Error::new("malformed tree mode"))?;
        let name = data[sp + 1..nul].to_vec();
        let oid = data.get(nul + 1..nul + 1 + oid_len).ok_or_else(|| Error::new("malformed tree"))?.to_vec();
        v.push(TreeEntry { mode, name, oid });
        i = nul + 1 + oid_len;
    }
    Ok(v)
}

pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = vec![];
    for e in entries {
        out.extend_from_slice(format!("{:o} ", e.mode).as_bytes());
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.oid);
    }
    out
}

/// Tree.Merge: entries replaced by name, or added, in git's order.
pub fn merge_tree(entries: &[TreeEntry], add: TreeEntry) -> Vec<TreeEntry> {
    let mut v: Vec<TreeEntry> = entries.iter().filter(|e| e.name != add.name).cloned().collect();
    v.push(add);
    v.sort_by(tree_order);
    v
}

pub struct Commit {
    pub tree: Vec<u8>,
    pub parents: Vec<Vec<u8>>,
    /// The commit as it is (headers after tree/parent, and the message, kept verbatim).
    pub raw: Vec<u8>,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

pub fn parse_commit(raw: &[u8]) -> Commit {
    let mut tree = vec![];
    let mut parents = vec![];
    for line in raw.split(|&c| c == b'\n') {
        if line.is_empty() {
            break;
        }
        let l = String::from_utf8_lossy(line);
        if let Some(t) = l.strip_prefix("tree ") {
            tree = unhex(t.trim());
        } else if let Some(p) = l.strip_prefix("parent ") {
            parents.push(unhex(p.trim()));
        }
    }
    Commit { tree, parents, raw: raw.to_vec() }
}

/// The commit with another tree and parents (everything else as it was).
pub fn rewrite_commit(c: &Commit, tree: &[u8], parents: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![];
    out.extend_from_slice(format!("tree {}\n", tools::hex(tree)).as_bytes());
    for p in parents {
        out.extend_from_slice(format!("parent {}\n", tools::hex(p)).as_bytes());
    }
    let mut rest = &c.raw[..];
    // Skip the original tree and parent lines.
    loop {
        if rest.starts_with(b"tree ") || rest.starts_with(b"parent ") {
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => rest = &rest[i + 1..],
                None => {
                    rest = &[];
                    break;
                }
            }
        } else {
            break;
        }
    }
    out.extend_from_slice(rest);
    out
}

pub struct Tag {
    pub object: Vec<u8>,
    pub kind: String,
    pub name: String,
    pub raw: Vec<u8>,
}

pub fn parse_tag(raw: &[u8]) -> Tag {
    let mut t = Tag { object: vec![], kind: String::new(), name: String::new(), raw: raw.to_vec() };
    for line in raw.split(|&c| c == b'\n') {
        if line.is_empty() {
            break;
        }
        let l = String::from_utf8_lossy(line);
        if let Some(v) = l.strip_prefix("object ") {
            t.object = unhex(v.trim());
        } else if let Some(v) = l.strip_prefix("type ") {
            t.kind = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("tag ") {
            t.name = v.to_string();
        }
    }
    t
}

/// The tag pointing at another object (gitobj's Tag.Encode: object, type, tag, tagger, message).
pub fn rewrite_tag(t: &Tag, object: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(&t.raw).into_owned();
    let (head, msg) = text.split_once("\n\n").unwrap_or((&text, ""));
    let tagger = head.lines().find_map(|l| l.strip_prefix("tagger ")).unwrap_or("");
    format!("object {}\ntype {}\ntag {}\ntagger {}\n\n{}", tools::hex(object), t.kind, t.name, tagger, msg).into_bytes()
}

impl Db {
    pub fn open() -> Result<Db> {
        let objdir = crate::gitcmd::git_no_lfs_simple(&["rev-parse", "--git-path", "objects"]).map_err(|e| e.wrap("cannot open root"))?;
        let objdir = if objdir.starts_with('/') { objdir } else { tools::abs(&objdir).display().to_string() };
        let fmt = crate::gitcmd::git_no_lfs_simple(&["rev-parse", "--show-object-format"]).unwrap_or_default();
        Ok(Db { reader: ObjectReader::new()?, objdir, sha256: fmt.trim() == "sha256" })
    }

    pub fn oid_len(&self) -> usize {
        if self.sha256 {
            32
        } else {
            20
        }
    }

    /// (type, contents)
    pub fn read(&mut self, oid: &[u8]) -> Result<(String, Vec<u8>)> {
        let (o, _) = self.reader.read(&tools::hex(oid), usize::MAX)?;
        Ok((o.kind, o.data))
    }

    pub fn tree(&mut self, oid: &[u8]) -> Result<Vec<TreeEntry>> {
        let (k, d) = self.read(oid)?;
        if k != "tree" {
            return Err(Error::new(format!("expected tree object, got {k}")));
        }
        parse_tree(&d, self.oid_len())
    }

    pub fn commit(&mut self, oid: &[u8]) -> Result<Commit> {
        let (k, d) = self.read(oid)?;
        if k != "commit" {
            return Err(Error::new(format!("expected commit object, got {k}")));
        }
        Ok(parse_commit(&d))
    }

    pub fn blob(&mut self, oid: &[u8]) -> Result<Vec<u8>> {
        let (_, d) = self.read(oid)?;
        Ok(d)
    }

    pub fn tag(&mut self, oid: &[u8]) -> Option<Tag> {
        match self.read(oid) {
            Ok((k, d)) if k == "tag" => Some(parse_tag(&d)),
            _ => None,
        }
    }

    pub fn hash(&self, kind: &str, data: &[u8]) -> Vec<u8> {
        let header = format!("{} {}\0", kind, data.len());
        if self.sha256 {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(header.as_bytes());
            h.update(data);
            h.finalize().to_vec()
        } else {
            use sha1::Digest;
            let mut h = sha1::Sha1::new();
            h.update(header.as_bytes());
            h.update(data);
            h.finalize().to_vec()
        }
    }

    /// A loose object (unless it exists already).
    pub fn write(&mut self, kind: &str, data: &[u8]) -> Result<Vec<u8>> {
        let oid = self.hash(kind, data);
        let hex = tools::hex(&oid);
        let dir = format!("{}/{}", self.objdir, &hex[..2]);
        let path = format!("{dir}/{}", &hex[2..]);
        if std::fs::metadata(&path).is_ok() {
            return Ok(oid);
        }
        std::fs::create_dir_all(&dir).map_err(|e| tools::path_err("mkdir", &dir, &e))?;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(format!("{} {}\0", kind, data.len()).as_bytes())?;
        enc.write_all(data)?;
        let z = enc.finish()?;
        let tmp = format!("{dir}/tmp_obj_{}_{}", std::process::id(), &hex[2..10]);
        std::fs::write(&tmp, &z).map_err(|e| tools::path_err("open", &tmp, &e))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444));
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(tools::path_err("rename", &tmp, &e));
        }
        Ok(oid)
    }

    pub fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Vec<u8>> {
        self.write("tree", &encode_tree(entries))
    }
}
