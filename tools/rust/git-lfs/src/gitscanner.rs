//! Finding pointers in history (lfs/gitscanner*.go): `git rev-list --objects` for the blobs
//! of a range, `git cat-file --batch-check` for the small ones, `git cat-file --batch` to read
//! them as pointers; `git ls-tree`/`ls-files` for a tree, `git diff-index` for the index, and
//! `git log -p` for the versions a file had before.

use crate::errors::{Error, Result};
use crate::filter::Filter;
use crate::gitcmd;
use crate::pointer::{self, Pointer, BLOB_SIZE_CUTOFF};
use crate::tools;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

/// A pointer found, with where (src_name and status: what diff-index said, for the index).
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct WrappedPointer {
    pub sha1: String,
    pub name: String,
    pub src_name: String,
    pub status: String,
    pub p: Pointer,
}

impl WrappedPointer {
    pub fn oid(&self) -> &str {
        &self.p.oid
    }
    pub fn size(&self) -> i64 {
        self.p.size
    }
}

pub type Callback<'a> = &'a mut dyn FnMut(Result<WrappedPointer>);

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Refs,
    All,
    RangeToRemote,
}

#[derive(Default)]
pub struct Scanner<'a> {
    pub filter: Option<Filter>,
    pub remote: String,
    skipped_refs: Vec<String>,
    pub found_lockable: Option<&'a mut dyn FnMut(&str)>,
    pub potential_lockables: Option<&'a dyn Fn(&str) -> bool>,
}

fn allows(f: &Option<Filter>, name: &str) -> bool {
    f.as_ref().is_none_or(|f| f.allows(name))
}

struct RevListOptions<'a> {
    mode: Mode,
    skip_deleted_blobs: bool,
    commits_only: bool,
    remote: &'a str,
    skipped_refs: &'a [String],
}

fn non_zero(v: &[String]) -> Vec<String> {
    v.iter().filter(|s| !s.is_empty() && !gitcmd::is_zero_object_id(s)).cloned().collect()
}

fn include_exclude(include: &[String], exclude: &[String]) -> Vec<String> {
    let mut a = non_zero(include);
    a.extend(non_zero(exclude).into_iter().map(|x| format!("^{x}")));
    a
}

/// `git rev-list`: (object id, name) of each object.
fn rev_list(include: &[String], exclude: &[String], o: &RevListOptions) -> Result<Vec<(String, String)>> {
    let mut args: Vec<String> = vec!["rev-list".into()];
    if !o.commits_only {
        args.push("--objects".into());
    }
    let stdin;
    match o.mode {
        Mode::Refs => {
            args.push(if o.skip_deleted_blobs { "--no-walk" } else { "--do-walk" }.into());
            stdin = include_exclude(include, exclude).join("\n");
        }
        Mode::All => {
            args.push("--all".into());
            stdin = String::new();
        }
        Mode::RangeToRemote => {
            args.push("--ignore-missing".into());
            if o.skipped_refs.is_empty() {
                args.push("--not".into());
                args.push(format!("--remotes={}", o.remote));
                stdin = include_exclude(include, exclude).join("\n");
            } else {
                let mut v = include_exclude(include, exclude);
                v.extend(o.skipped_refs.iter().cloned());
                stdin = v.join("\n");
            }
        }
    }
    args.push("--stdin".into());
    args.push("--".into());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::trace!("run_command: git {}", args.join(" "));
    let mut child = gitcmd::git_no_lfs_command(&argv).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(Error::from)?;
    let mut si = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = si.write_all(stdin.as_bytes());
    });
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).map_err(Error::from)?;
    let mut err = Vec::new();
    child.stderr.take().unwrap().read_to_end(&mut err).ok();
    let _ = writer.join();
    let st = child.wait().map_err(Error::from)?;
    let msg = String::from_utf8_lossy(&err).into_owned();
    if !st.success() {
        return Err(Error::new(format!("Error in `git {}`: {} {}", args.join(" "), crate::subprocess::exit_text(&st), msg)));
    }
    if let Some(m) = regex::Regex::new(r"warning: refname (.*) is ambiguous").unwrap().captures(&msg) {
        return Err(Error::new(format!("ref {} is ambiguous", tools::quote(&m[1]))));
    }
    let mut res = vec![];
    for line in out.lines() {
        let line = line.trim();
        if line.len() < 40 {
            continue;
        }
        let hexlen = line.bytes().take_while(|c| c.is_ascii_hexdigit()).count();
        if hexlen != 40 && hexlen != 64 {
            return Err(Error::new(format!("missing OID in line (got {})", tools::quote(line))));
        }
        let name = if line.len() > hexlen { line[hexlen + 1..].to_string() } else { String::new() };
        res.push((line[..hexlen].to_string(), name));
    }
    Ok(res)
}

/// `git cat-file --batch-check` over the ids: (blob id, small enough to be a pointer).
fn cat_file_batch_check(shas: &[String]) -> Result<Vec<(String, bool)>> {
    let mut child = gitcmd::git_no_lfs_command(&["cat-file", "--batch-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(Error::from)?;
    let input: String = shas.iter().map(|s| format!("{s}\n")).collect();
    let mut si = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = si.write_all(input.as_bytes());
    });
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).map_err(Error::from)?;
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    let _ = writer.join();
    let st = child.wait().map_err(Error::from)?;
    if !st.success() {
        return Err(Error::new(format!("error in `git cat-file --batch-check`: {} {}", crate::subprocess::exit_text(&st), err)));
    }
    let mut res = vec![];
    for line in out.lines() {
        let Some(i) = line.find(' ') else { continue };
        if line.len() < i + 6 || &line[i + 1..i + 5] != "blob" {
            continue;
        }
        let Ok(size) = line[i + 6..].parse::<i64>() else { continue };
        res.push((line[..i].to_string(), size < BLOB_SIZE_CUTOFF as i64));
    }
    Ok(res)
}

/// `git cat-file --batch`: objects by id, one at a time.
pub struct ObjectReader {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
}

pub struct Object {
    pub oid: String,
    pub kind: String,
    pub data: Vec<u8>,
}

impl ObjectReader {
    pub fn new() -> Result<ObjectReader> {
        let mut child = gitcmd::git_no_lfs_command(&["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(Error::from)?;
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        Ok(ObjectReader { child, stdin, out })
    }

    /// The object, read whole when smaller than `limit` (else only hashed: data empty, and
    /// the contents' sha256 returned).
    pub fn read(&mut self, oid: &str, limit: usize) -> Result<(Object, Option<String>)> {
        let w = self.stdin.as_mut().unwrap();
        w.write_all(format!("{oid}\n").as_bytes()).map_err(Error::from)?;
        w.flush().map_err(Error::from)?;
        let mut header = String::new();
        self.out.read_line(&mut header).map_err(Error::from)?;
        let header = header.trim_end();
        let parts: Vec<&str> = header.split(' ').collect();
        if parts.len() == 2 && parts[1] == "missing" {
            return Err(Error::new(format!("missing object: {oid}")));
        }
        if parts.len() != 3 {
            return Err(Error::new(format!("invalid `git cat-file --batch` header: {}", tools::quote(header))));
        }
        let size: usize = parts[2].parse().map_err(|_| Error::new(format!("invalid size in {}", tools::quote(header))))?;
        let mut data = vec![];
        let mut hash = None;
        if size < limit {
            data.resize(size, 0);
            self.out.read_exact(&mut data).map_err(Error::from)?;
        } else {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            let mut left = size;
            let mut buf = vec![0u8; 65536];
            while left > 0 {
                let n = left.min(buf.len());
                self.out.read_exact(&mut buf[..n]).map_err(Error::from)?;
                h.update(&buf[..n]);
                left -= n;
            }
            hash = Some(tools::hex(&h.finalize()));
        }
        let mut nl = [0u8; 1];
        self.out.read_exact(&mut nl).map_err(Error::from)?;
        Ok((Object { oid: parts[0].to_string(), kind: parts[1].to_string(), data }, hash))
    }
}

impl Drop for ObjectReader {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

/// PointerScanner.Scan: the blob as a pointer, if it is one.
pub fn read_pointer(r: &mut ObjectReader, sha: &str) -> Result<Option<WrappedPointer>> {
    let (o, _) = r.read(sha, BLOB_SIZE_CUTOFF)?;
    if o.kind != "blob" {
        return Ok(None);
    }
    match pointer::decode(&o.data) {
        Ok(p) => Ok(Some(WrappedPointer { sha1: o.oid, p, ..Default::default() })),
        Err(_) => Ok(None),
    }
}

impl<'a> Scanner<'a> {
    pub fn new() -> Scanner<'a> {
        Scanner::default()
    }

    /// NewGitScannerForPush.
    pub fn for_push(remote: &str) -> Scanner<'a> {
        Scanner { remote: remote.to_string(), skipped_refs: calc_skipped_refs(remote), ..Default::default() }
    }

    fn lockable(&mut self, name: Option<&String>, filtered: bool) {
        let Some(name) = name else { return };
        let Some(set) = self.potential_lockables else { return };
        if !set(name) {
            return;
        }
        if filtered && !allows(&self.filter, name) {
            return;
        }
        if let Some(cb) = self.found_lockable.as_mut() {
            cb(name);
        }
    }

    fn scan_refs(&mut self, include: &[String], exclude: &[String], o: RevListOptions, cb: Callback) -> Result<()> {
        let revs = rev_list(include, exclude, &o)?;
        let mut names: HashMap<String, String> = HashMap::new();
        for (sha, name) in &revs {
            if !name.is_empty() {
                names.insert(sha.clone(), name.clone());
            }
        }
        let shas: Vec<String> = revs.into_iter().map(|r| r.0).collect();
        let checked = cat_file_batch_check(&shas)?;
        let mut small = vec![];
        for (sha, is_small) in checked {
            if is_small {
                small.push(sha);
            } else {
                self.lockable(names.get(&sha), false);
            }
        }
        let mut r = ObjectReader::new()?;
        let mut later_lockables = vec![];
        let mut err = None;
        for sha in small {
            match read_pointer(&mut r, &sha) {
                Ok(Some(mut p)) => {
                    if let Some(n) = names.get(&p.sha1) {
                        p.name = n.clone();
                    }
                    if allows(&self.filter, &p.name) {
                        cb(Ok(p));
                    }
                }
                Ok(None) => later_lockables.push(sha),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        for sha in later_lockables {
            self.lockable(names.get(&sha), true);
        }
        if let Some(e) = err {
            cb(Err(e));
        }
        Ok(())
    }

    pub fn scan_multi_range_to_remote(&mut self, include: &str, exclude: &[String], cb: Callback) -> Result<()> {
        if self.remote.is_empty() {
            return Err(Error::new(format!("unable to scan starting at {}: no remote set", tools::quote(include))));
        }
        let remote = self.remote.clone();
        let skipped = self.skipped_refs.clone();
        let o = RevListOptions { mode: Mode::RangeToRemote, skip_deleted_blobs: false, commits_only: false, remote: &remote, skipped_refs: &skipped };
        self.scan_refs(&[include.to_string()], exclude, o, cb)
    }

    pub fn scan_refs_multi(&mut self, include: &[String], exclude: &[String], cb: Callback) -> Result<()> {
        let o = RevListOptions { mode: Mode::Refs, skip_deleted_blobs: false, commits_only: false, remote: "", skipped_refs: &[] };
        self.scan_refs(include, exclude, o, cb)
    }

    pub fn scan_ref_range(&mut self, include: &str, exclude: &str, cb: Callback) -> Result<()> {
        let o = RevListOptions { mode: Mode::Refs, skip_deleted_blobs: false, commits_only: false, remote: "", skipped_refs: &[] };
        self.scan_refs(&[include.to_string()], &[exclude.to_string()], o, cb)
    }

    pub fn scan_ref_with_deleted(&mut self, r: &str, cb: Callback) -> Result<()> {
        self.scan_ref_range(r, "", cb)
    }

    pub fn scan_ref(&mut self, r: &str, cb: Callback) -> Result<()> {
        let o = RevListOptions { mode: Mode::Refs, skip_deleted_blobs: true, commits_only: false, remote: "", skipped_refs: &[] };
        self.scan_refs(&[r.to_string()], &[String::new()], o, cb)
    }

    pub fn scan_all(&mut self, cb: Callback) -> Result<()> {
        let o = RevListOptions { mode: Mode::All, skip_deleted_blobs: false, commits_only: false, remote: "", skipped_refs: &[] };
        self.scan_refs(&[String::new()], &[String::new()], o, cb)
    }

    /// ScanRefByTree / ScanRefRangeByTree: every commit's tree (pointer errors included).
    pub fn scan_ref_range_by_tree(&mut self, include: &str, exclude: &str, skip_deleted: bool, cb: Callback) -> Result<()> {
        let o = RevListOptions { mode: Mode::Refs, skip_deleted_blobs: skip_deleted, commits_only: true, remote: "", skipped_refs: &[] };
        let ex: Vec<String> = if exclude.is_empty() { vec![] } else { vec![exclude.to_string()] };
        let revs = rev_list(&[include.to_string()], &ex, &o)?;
        for (rev, _) in revs {
            scan_tree_for_pointers(&rev, cb)?;
        }
        Ok(())
    }

    pub fn scan_tree(&mut self, r: &str, cb: Callback) -> Result<()> {
        let blobs = ls_blobs(&["ls-tree", "-r", "-l", "-z", "--full-tree", r], "ls-tree")?;
        self.cat_tree(blobs, cb)
    }

    pub fn scan_lfs_files(&mut self, r: &str, cb: Callback) -> Result<()> {
        let blobs = if gitcmd::is_git_version_at_least("2.42.0") {
            ls_blobs(
                &["ls-files", "--cached", "--exclude-standard", "--full-name", "--sparse", "-z", "--format=%(objectmode) %(objecttype) %(objectname) %(objectsize)\t%(path)", ":(top,attr:filter=lfs)"],
                "ls-tree",
            )?
        } else {
            ls_blobs(&["ls-tree", "-r", "-l", "-z", "--full-tree", r], "ls-tree")?
        };
        self.cat_tree(blobs, cb)
    }

    fn cat_tree(&mut self, blobs: Vec<TreeBlob>, cb: Callback) -> Result<()> {
        let mut r = ObjectReader::new()?;
        for t in blobs {
            if t.size >= BLOB_SIZE_CUTOFF as i64 || !allows(&self.filter, &t.filename) {
                continue;
            }
            match read_pointer(&mut r, &t.oid) {
                Ok(Some(mut p)) => {
                    p.name = t.filename;
                    cb(Ok(p));
                }
                Ok(None) => {}
                Err(e) => {
                    cb(Err(e));
                    break;
                }
            }
        }
        Ok(())
    }

    /// ScanIndex: what `git diff-index` (cached, then not) lists against the ref.
    pub fn scan_index(&mut self, r: &str, working_dir: &str, cb: Callback) -> Result<()> {
        let mut files: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        let mut pairs = std::collections::HashSet::new();
        let mut order: Vec<String> = vec![];
        let mut seen = std::collections::HashSet::new();
        for cached in [true, false] {
            for e in diff_index(r, cached, false, working_dir)? {
                let name = if e.dst_name.is_empty() { e.src_name.clone() } else { e.dst_name.clone() };
                if pairs.insert(format!("{}:{}", e.dst_sha, name)) {
                    files.entry(e.dst_sha.clone()).or_default().push((name, e.src_name.clone(), e.status.to_string()));
                }
                if seen.insert(e.dst_sha.clone()) {
                    order.push(e.dst_sha.clone());
                }
            }
        }
        let checked = cat_file_batch_check(&order)?;
        let mut rd = ObjectReader::new()?;
        for (sha, small) in checked {
            if !small {
                continue;
            }
            match read_pointer(&mut rd, &sha) {
                Ok(Some(p)) => {
                    for (name, src, status) in files.get(&p.sha1).cloned().unwrap_or_default() {
                        let w = WrappedPointer { sha1: p.sha1.clone(), name, src_name: src, status, p: p.p.clone() };
                        if allows(&self.filter, &w.name) {
                            cb(Ok(w));
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    cb(Err(e));
                    break;
                }
            }
        }
        Ok(())
    }

    /// ScanPreviousVersions: the pointers files had before, in the ref's history since then.
    pub fn scan_previous_versions(&mut self, r: &str, since: &str, cb: Callback) -> Result<()> {
        let mut args = vec![format!("--since={since}")];
        args.extend(LOG_SEARCH_ARGS.iter().map(|s| s.to_string()));
        args.push(r.to_string());
        parse_log(&args, '-', &self.filter, cb)
    }

    pub fn scan_unpushed(&mut self, remote: &str, cb: Callback) -> Result<()> {
        let mut args: Vec<String> = vec!["--branches".into(), "--tags".into(), "--not".into()];
        args.push(if remote.is_empty() { "--remotes".into() } else { format!("--remotes={remote}") });
        args.extend(LOG_SEARCH_ARGS.iter().map(|s| s.to_string()));
        parse_log(&args, '+', &None, cb)
    }

    pub fn scan_stashed(&mut self, cb: Callback) -> Result<()> {
        let out = gitcmd::git_no_lfs_command(&["log", "-g", "--format=%h", "refs/stash", "--"]).stderr(Stdio::null()).output().map_err(Error::from)?;
        if !out.status.success() {
            return Ok(());
        }
        let shas: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim()).filter(|l| !l.is_empty()).map(|s| format!("{s}^..{s}")).collect();
        for pre in [vec!["-m".to_string(), "--first-parent".to_string()], vec![]] {
            let mut args = pre;
            args.extend(LOG_SEARCH_ARGS.iter().map(|s| s.to_string()));
            args.extend(shas.iter().cloned());
            parse_log(&args, '+', &None, cb)?;
        }
        Ok(())
    }
}

/// calcSkippedRefs: remote-tracking refs that still exist on the remote, as exclusions.
fn calc_skipped_refs(remote: &str) -> Vec<String> {
    let cached = cached_remote_refs(remote);
    let actual = remote_refs(remote);
    cached.into_iter().filter(|(name, _)| actual.contains(name)).map(|(_, sha)| format!("^{sha}")).collect()
}

/// CachedRemoteRefs: (name, sha) of refs/remotes/REMOTE/*.
pub fn cached_remote_refs(remote: &str) -> Vec<(String, String)> {
    let Ok(out) = gitcmd::git_no_lfs_command(&["show-ref"]).stderr(Stdio::null()).output() else { return vec![] };
    let prefix = format!("refs/remotes/{remote}/");
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((sha, r)) = line.split_once(' ') else { continue };
        if let Some(name) = r.strip_prefix(&prefix) {
            let name = name.trim();
            if name != "HEAD" {
                v.push((name.to_string(), sha.to_string()));
            }
        }
    }
    v
}

/// RemoteRefs: the branch names `git ls-remote --heads` lists.
pub fn remote_refs(remote: &str) -> Vec<String> {
    let Ok(out) = gitcmd::git_no_lfs_command(&["ls-remote", "--heads", "-q", remote]).stderr(Stdio::null()).output() else { return vec![] };
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((_, r)) = line.split_once('\t') else { continue };
        let Some(rest) = r.strip_prefix("refs/") else { continue };
        let Some((_, name)) = rest.split_once('/') else { continue };
        if name != "HEAD" {
            v.push(name.to_string());
        }
    }
    v
}

pub struct TreeBlob {
    pub oid: String,
    pub size: i64,
    pub mode: u32,
    pub filename: String,
}

/// ls-tree -z (or ls-files with the same format): the blobs.
pub fn ls_blobs(args: &[&str], what: &str) -> Result<Vec<TreeBlob>> {
    let out = gitcmd::git_no_lfs_command(args).stdin(Stdio::null()).output().map_err(Error::from)?;
    if !out.status.success() {
        return Err(Error::new(format!("error in `git {what}`: {} {}", crate::subprocess::exit_text(&out.status), String::from_utf8_lossy(&out.stderr))));
    }
    let mut v = vec![];
    for rec in out.stdout.split(|&b| b == 0) {
        let line = String::from_utf8_lossy(rec);
        let Some((meta, name)) = line.split_once('\t') else { continue };
        let attrs: Vec<&str> = meta.splitn(4, ' ').collect();
        if attrs.len() < 4 || attrs[1] != "blob" {
            continue;
        }
        let Ok(mode) = u32::from_str_radix(attrs[0].trim(), 8) else { continue };
        let Ok(size) = attrs[3].trim().parse::<i64>() else { continue };
        v.push(TreeBlob { oid: attrs[2].to_string(), size, mode, filename: name.to_string() });
    }
    Ok(v)
}

/// runScanTreeForPointers: every LFS file of a tree (by its .gitattributes), a pointer or
/// an error for one that is not.
fn scan_tree_for_pointers(tree: &str, cb: Callback) -> Result<()> {
    let blobs = ls_blobs(&["ls-tree", "-r", "-l", "-z", "--full-tree", tree], "ls-tree")?;
    let mut r = ObjectReader::new()?;
    let mut pointers: Vec<(String, Option<WrappedPointer>)> = vec![];
    let mut mp = crate::attrs::MacroProcessor::new();
    let mut paths = vec![];
    for t in blobs.into_iter().filter(|t| t.mode == 0o100644 || t.mode == 0o100755) {
        let base = t.filename.rsplit('/').next().unwrap_or("");
        if base == ".gitattributes" {
            let (o, _) = r.read(&t.oid, usize::MAX)?;
            paths.extend(crate::attrs::attr_paths_from_data(&mut mp, &t.filename, "", &o.data, t.filename == ".gitattributes"));
        } else if t.size < BLOB_SIZE_CUTOFF as i64 {
            let p = read_pointer(&mut r, &t.oid)?.map(|mut p| {
                p.name = t.filename.clone();
                p
            });
            pointers.push((t.filename, p));
        } else {
            pointers.push((t.filename, None));
        }
    }
    let mut inc = vec![];
    let mut exc = vec![];
    for p in &paths {
        if p.tracked {
            inc.push(p.path.clone());
        } else {
            exc.push(p.path.clone());
        }
    }
    let f = Filter::new_default_false(&inc, &exc, crate::filter::PatternType::GitAttributes);
    for (name, p) in pointers {
        if !f.allows(&name) {
            continue;
        }
        match p {
            Some(p) => cb(Ok(p)),
            None => {
                let e = Error::not_a_pointer(Error::new("Error")).go_wrap(crate::errors::Kind::PointerScan, "Pointer error");
                let mut e = e;
                e.expected = Some(format!("{tree}\0{name}"));
                cb(Err(e));
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub struct DiffIndexEntry {
    pub src_mode: String,
    pub dst_mode: String,
    pub src_sha: String,
    pub dst_sha: String,
    pub status: char,
    pub src_name: String,
    pub dst_name: String,
}

/// `git diff-index -M [--cached] REF`.
pub fn diff_index(r: &str, cached: bool, refresh: bool, working_dir: &str) -> Result<Vec<DiffIndexEntry>> {
    if refresh {
        gitcmd::git_simple(&["update-index", "-q", "--refresh"]).map_err(|e| e.wrap("Failed to run `git update-index`"))?;
    }
    let mut args: Vec<&str> = vec![];
    if !working_dir.is_empty() {
        args.extend(["-C", working_dir]);
    }
    args.extend(["diff-index", "-M"]);
    if cached {
        args.push("--cached");
    }
    args.push(r);
    let out = crate::subprocess::command("git", &args).stdin(Stdio::null()).output().map_err(Error::from)?;
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            return Err(Error::new(format!("invalid line: {line}")).wrap("`git diff-index` scan"));
        }
        let desc: Vec<&str> = parts[0].split_whitespace().collect();
        if desc.len() < 5 {
            return Err(Error::new(format!("invalid description: {}", parts[0])).wrap("`git diff-index` scan"));
        }
        v.push(DiffIndexEntry {
            src_mode: desc[0].trim_start_matches(':').to_string(),
            dst_mode: desc[1].to_string(),
            src_sha: desc[2].to_string(),
            dst_sha: desc[3].to_string(),
            status: desc[4].chars().next().unwrap_or('X'),
            src_name: parts[1].to_string(),
            dst_name: parts.get(2).map(|s| s.to_string()).unwrap_or_default(),
        });
    }
    Ok(v)
}

const LOG_SEARCH_ARGS: [&str; 8] = ["--no-ext-diff", "--no-textconv", "--color=never", "-G", "oid sha256:", "-p", "-U12", "--format=lfs-commit-sha: %H %P"];

fn parse_log(args: &[String], dir: char, filter: &Option<Filter>, cb: Callback) -> Result<()> {
    let mut a: Vec<&str> = vec!["log"];
    a.extend(args.iter().map(String::as_str));
    let out = gitcmd::git_no_lfs_command(&a).stdin(Stdio::null()).output().map_err(Error::from)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let commit_re = regex::Regex::new(r"^lfs-commit-sha: ([0-9a-f]{40}|[0-9a-f]{64})(?: ([0-9a-f]{40}|[0-9a-f]{64}))*").unwrap();
    let file_re = regex::Regex::new(r#"^diff --git "?a/(.+?)\s+"?b/(.+)"#).unwrap();
    let merge_re = regex::Regex::new(r"^diff --cc (.+)").unwrap();
    let data_re = regex::Regex::new(r"^([\+\- ])(version https://git-lfs|oid sha256|size|ext-).*$").unwrap();
    let mut data = String::new();
    let mut current = String::new();
    let mut included = true;
    let finish = |data: &mut String, current: &str, included: bool, cb: &mut dyn FnMut(Result<WrappedPointer>)| {
        if data.is_empty() || !included {
            return;
        }
        let r = pointer::decode(data.as_bytes());
        data.clear();
        match r {
            Ok(p) => cb(Ok(WrappedPointer { name: current.to_string(), p, ..Default::default() })),
            Err(e) => crate::trace!("Unable to parse pointer from log: {}", e),
        }
    };
    let set_name = |name: &str, filter: &Option<Filter>| -> (String, bool) {
        let name = name.strip_suffix('"').unwrap_or(name);
        let name = go_unquote(name).unwrap_or_else(|| name.to_string());
        let inc = allows(filter, &name);
        (name, inc)
    };
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if commit_re.is_match(line) {
            finish(&mut data, &current, included, cb);
        } else if let Some(m) = file_re.captures(line) {
            finish(&mut data, &current, included, cb);
            let n = if dir == '+' { m[2].to_string() } else { m[1].to_string() };
            (current, included) = set_name(&n, filter);
        } else if let Some(m) = merge_re.captures(line) {
            finish(&mut data, &current, included, cb);
            (current, included) = set_name(&m[1], filter);
        } else if included {
            if let Some(m) = data_re.captures(line) {
                let c = m[1].chars().next().unwrap();
                if c == dir || c == ' ' {
                    data.push_str(&line[1..]);
                    data.push('\n');
                }
            }
        }
    }
    finish(&mut data, &current, included, cb);
    if !out.status.success() {
        cb(Err(Error::new(format!("error in `git log`: {} {}", crate::subprocess::exit_text(&out.status), String::from_utf8_lossy(&out.stderr)))));
    }
    Ok(())
}

/// strconv.Unquote of `"name"`: C-style escapes, octal bytes included.
fn go_unquote(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            return None;
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let e = *b.get(i)?;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'\'' => return None,
            b'0'..=b'7' => {
                let oct = std::str::from_utf8(b.get(i..i + 3)?).ok()?;
                out.push(u8::from_str_radix(oct, 8).ok()?);
                i += 2;
            }
            b'x' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            _ => return None,
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}
