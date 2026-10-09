//! migrate import, export and info (commands/command_migrate*.go, git/githistory,
//! git/gitattr/tree.go): history rewritten commit by commit (blobs turned into pointers, or
//! back), .gitattributes updated, refs moved; or only examined for a report.

use super::{exit, exit_with_error, setup_repository, Cmd};
use crate::attrs::{self, Attr, MacroProcessor};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::filter::{Filter, PatternType};
use crate::gitcmd::{self, Ref, RefType};
use crate::gitobj::{self, Db, TreeEntry};
use crate::tasklog::{Logger, PercentageTask, Sink};
use crate::tools;
use crate::wildmatch::Wildmatch;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::sync::Arc;

type BlobFn<'a> = &'a mut dyn FnMut(&str, Vec<u8>) -> Result<Vec<u8>>;
type TreePreFn<'a> = &'a mut dyn FnMut(&mut Db, &str, &[TreeEntry]) -> Result<()>;
type TreeFn<'a> = &'a mut dyn FnMut(&mut Db, &str, Vec<TreeEntry>) -> Result<Vec<TreeEntry>>;

pub struct Options<'a> {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub update_refs: bool,
    pub verbose: bool,
    pub object_map: String,
    pub blob_fn: Option<BlobFn<'a>>,
    pub tree_pre_fn: Option<TreePreFn<'a>>,
    pub tree_fn: Option<TreeFn<'a>>,
}

pub struct Rewriter {
    pub db: Db,
    pub filter: Filter,
    entries: HashMap<String, TreeEntry>,
    commits: HashMap<Vec<u8>, Vec<u8>>,
    logger: Arc<Logger>,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

impl Rewriter {
    pub fn new(db: Db, filter: Filter, logger: Arc<Logger>) -> Rewriter {
        Rewriter { db, filter, entries: HashMap::new(), commits: HashMap::new(), logger }
    }

    fn commits_to_migrate(&self, o: &Options) -> Result<Vec<Vec<u8>>> {
        let mut w = self.logger.waiter("Sorting commits");
        let mut args: Vec<String> = vec!["rev-list".into(), "--reverse".into(), "--topo-order".into(), "--do-walk".into(), "--stdin".into(), "--".into()];
        let mut stdin: Vec<String> = o.include.iter().filter(|s| !s.is_empty() && !gitcmd::is_zero_object_id(s)).cloned().collect();
        stdin.extend(o.exclude.iter().filter(|s| !s.is_empty() && !gitcmd::is_zero_object_id(s)).map(|x| format!("^{x}")));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::trace!("run_command: git {}", args.join(" "));
        let mut child = gitcmd::git_no_lfs_command(&argv).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(Error::from)?;
        let input = stdin.join("\n");
        let mut si = child.stdin.take().unwrap();
        let t = std::thread::spawn(move || {
            let _ = si.write_all(input.as_bytes());
        });
        let out = child.wait_with_output().map_err(Error::from)?;
        let _ = t.join();
        args.truncate(args.len());
        w.complete();
        if !out.status.success() {
            return Err(Error::new(format!("Error in `git {}`: {} {}", args.join(" "), crate::subprocess::exit_text(&out.status), String::from_utf8_lossy(&out.stderr))));
        }
        let msg = String::from_utf8_lossy(&out.stderr);
        if let Some(m) = regex::Regex::new(r"warning: refname (.*) is ambiguous").unwrap().captures(&msg) {
            return Err(Error::new(format!("ref {} is ambiguous", tools::quote(&m[1]))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim()).filter(|l| l.len() >= 40).map(|l| unhex(&l[..l.bytes().take_while(|c| c.is_ascii_hexdigit()).count()])).collect())
    }

    pub fn rewrite(&mut self, o: &mut Options) -> Result<Vec<u8>> {
        let commits = self.commits_to_migrate(o)?;
        let perc = self.logger.percentage(if o.update_refs { "Rewriting commits" } else { "Examining commits" }, commits.len() as u64);
        let mut map_file = None;
        if !o.object_map.is_empty() {
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&o.object_map) {
                Ok(f) => map_file = Some(f),
                Err(e) => {
                    perc.complete();
                    return Err(Error::new(format!("could not create object map file: {}", tools::path_err("open", &o.object_map, &e))));
                }
            }
        }
        let mut tip = vec![];
        let r: Result<()> = (|| {
            for oid in &commits {
                let original = self.db.commit(oid)?;
                let tree = self.rewrite_tree(oid, &original.tree, "", o, if o.verbose { Some(&perc) } else { None })?;
                let parents: Vec<Vec<u8>> = original.parents.iter().map(|p| self.commits.get(p).cloned().unwrap_or_else(|| p.clone())).collect();
                let new = if tree == original.tree && parents == original.parents {
                    oid.clone()
                } else {
                    let data = gitobj::rewrite_commit(&original, &tree, &parents);
                    let n = self.db.write("commit", &data)?;
                    if let Some(f) = map_file.as_mut() {
                        writeln!(f, "{},{}", tools::hex(oid), tools::hex(&n)).map_err(Error::from)?;
                    }
                    n
                };
                self.commits.insert(oid.clone(), new.clone());
                perc.count(1);
                tip = new;
            }
            Ok(())
        })();
        perc.complete();
        r?;
        if o.update_refs {
            self.update_refs().map_err(|e| e.wrap("could not update refs"))?;
        }
        Ok(tip)
    }

    fn allows(&self, e: &TreeEntry, path: &str) -> bool {
        if e.is_blob() {
            return self.filter.allows(path.trim_start_matches('/'));
        }
        true
    }

    fn rewrite_tree(&mut self, commit: &[u8], tree_oid: &[u8], path: &str, o: &mut Options, perc: Option<&PercentageTask>) -> Result<Vec<u8>> {
        let tree = self.db.tree(tree_oid)?;
        if let Some(f) = o.tree_pre_fn.as_mut() {
            f(&mut self.db, &format!("/{path}"), &tree)?;
        }
        let mut entries = Vec::with_capacity(tree.len());
        for e in &tree {
            let name = e.name_str();
            let full = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
            if !self.allows(e, &full) || e.is_link() {
                entries.push(e.clone());
                continue;
            }
            let key = format!("{}:{}", full, tools::hex(&e.oid));
            if let Some(c) = self.entries.get(&key) {
                let mut c = c.clone();
                c.mode = e.mode;
                entries.push(c);
                continue;
            }
            let oid = if e.is_blob() {
                self.rewrite_blob(commit, &e.oid, &full, o, perc)?
            } else if e.is_tree() {
                self.rewrite_tree(commit, &e.oid, &full, o, perc)?
            } else {
                e.oid.clone()
            };
            let n = TreeEntry { mode: e.mode, name: e.name.clone(), oid };
            self.entries.insert(key, n.clone());
            entries.push(n);
        }
        let rewritten = match o.tree_fn.as_mut() {
            Some(f) => f(&mut self.db, &format!("/{path}"), entries)?,
            None => entries,
        };
        if rewritten == tree {
            return Ok(tree_oid.to_vec());
        }
        self.db.write_tree(&rewritten)
    }

    fn rewrite_blob(&mut self, commit: &[u8], from: &[u8], path: &str, o: &mut Options, perc: Option<&PercentageTask>) -> Result<Vec<u8>> {
        let Some(f) = o.blob_fn.as_mut() else { return Ok(from.to_vec()) };
        let data = self.db.blob(from)?;
        let orig = data.clone();
        let new = f(path, data)?;
        if new != orig {
            let sha = self.db.write("blob", &new)?;
            if let Some(p) = perc {
                p.entry(&format!("  commit {}: {}", tools::hex(commit), path));
            }
            return Ok(sha);
        }
        Ok(from.to_vec())
    }

    fn update_refs(&mut self) -> Result<()> {
        let refs = all_refs().map_err(|e| e.wrap("could not find refs to update"))?;
        let refs: Vec<Ref> = refs.into_iter().filter(|r| r.typ != RefType::RemoteBranch).collect();
        let mut list = self.logger.list("Updating refs");
        let max = refs.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let mut input = String::new();
        let tx = gitcmd::is_git_version_at_least("2.27.0");
        if tx {
            input.push_str("start\0");
        }
        let mut seen: HashMap<String, Vec<u8>> = HashMap::new();
        for r in &refs {
            self.update_one_ref(&list, max, &mut seen, r, &mut input)?;
        }
        if tx {
            input.push_str("prepare\0commit\0");
        }
        let mut child = crate::subprocess::command("git", &["update-ref", "--stdin", "-z"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(Error::from)?;
        let mut si = child.stdin.take().unwrap();
        let t = std::thread::spawn(move || {
            let _ = si.write_all(input.as_bytes());
        });
        let out = child.wait_with_output().map_err(Error::from)?;
        let _ = t.join();
        list.complete();
        if !out.status.success() {
            let mut o = String::from_utf8_lossy(&out.stdout).into_owned();
            o.push_str(&String::from_utf8_lossy(&out.stderr));
            return Err(Error::new(format!("git update-ref failed: {}, output: {}", crate::subprocess::exit_text(&out.status), o)));
        }
        Ok(())
    }

    fn update_one_ref(&mut self, list: &crate::tasklog::ListTask, max: usize, seen: &mut HashMap<String, Vec<u8>>, r: &Ref, input: &mut String) -> Result<()> {
        let sha = unhex(&r.sha);
        let refspec = r.refspec();
        if seen.contains_key(&refspec) {
            return Ok(());
        }
        let mut to = self.commits.get(&sha).cloned();
        if r.typ == RefType::LocalTag {
            if let Some(tag) = self.db.tag(&sha) {
                if tag.kind == "tag" {
                    let inner = self.db.tag(&tag.object);
                    let name = format!("refs/tags/{}", inner.map(|t| t.name).unwrap_or_default());
                    if !seen.contains_key(&name) {
                        let old = gitcmd::resolve_ref(&name)?;
                        self.update_one_ref(list, max, seen, &old, input)?;
                    }
                    let Some(updated) = seen.get(&name).cloned() else { return Ok(()) };
                    to = Some(self.db.write("tag", &gitobj::rewrite_tag(&tag, &updated)).map_err(|e| e.wrap(format!("could not rewrite tag: {}", tag.name)))?);
                } else if tag.kind == "commit" {
                    let Some(obj) = self.commits.get(&tag.object).cloned() else { return Ok(()) };
                    to = Some(self.db.write("tag", &gitobj::rewrite_tag(&tag, &obj)).map_err(|e| e.wrap(format!("could not rewrite tag: {}", tag.name)))?);
                }
            }
        }
        let Some(to) = to else { return Ok(()) };
        input.push_str(&format!("update {}\0{}\0\0", refspec, tools::hex(&to)));
        list.entry(&format!("  {}{}\t{} -> {}", r.name, " ".repeat(max.saturating_sub(r.name.len())), r.sha, tools::hex(&to)));
        seen.insert(refspec, to);
        Ok(())
    }
}

/// git.AllRefsIn("").
pub fn all_refs() -> Result<Vec<Ref>> {
    let out = gitcmd::git_no_lfs_command(&["for-each-ref", "--format=%(objectname)%00%(refname)"]).output().map_err(Error::from)?;
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((sha, name)) = line.split_once('\0') else { return Err(Error::new(format!("invalid `git for-each-ref` line: {}", tools::quote(line)))) };
        let (typ, n) = gitcmd::parse_ref_to_type_and_name(name);
        v.push(Ref { name: n, typ, sha: sha.to_string() });
    }
    Ok(v)
}

// .gitattributes as a tree of directories (gitattr.Tree), for --fixup.

#[derive(Default)]
struct AttrNode {
    data: Option<Vec<u8>>,
    children: BTreeMap<String, AttrNode>,
    lines: Vec<(Wildmatch, Vec<Attr>)>,
}

#[derive(Default)]
pub struct AttrTree {
    root: AttrNode,
    system: Option<AttrNode>,
    user: Option<AttrNode>,
    repo: Option<AttrNode>,
    ready: bool,
}

fn attr_node(db: &mut Db, entries: &[TreeEntry]) -> Result<AttrNode> {
    let mut n = AttrNode::default();
    for e in entries {
        if e.name == b".gitattributes" {
            if e.is_link() {
                return Err(Error::new("expected '.gitattributes' to be a file, got a symbolic link"));
            }
            n.data = Some(db.blob(&e.oid)?);
            break;
        }
    }
    for e in entries {
        if !e.is_tree() {
            continue;
        }
        let sub = db.tree(&e.oid)?;
        let c = attr_node(db, &sub)?;
        if !c.children.is_empty() || c.data.as_ref().is_some_and(|d| !d.is_empty()) {
            n.children.insert(e.name_str(), c);
        }
    }
    Ok(n)
}

fn file_node(path: &str) -> Option<AttrNode> {
    if path.is_empty() {
        return None;
    }
    std::fs::read(path).ok().map(|d| AttrNode { data: Some(d), ..Default::default() })
}

impl AttrNode {
    fn read_macros(&self, mp: &mut MacroProcessor) {
        if let Some(d) = &self.data {
            if let Ok((l, _)) = attrs::parse_lines(d) {
                mp.process_lines(l, true);
            }
        }
    }
    fn process(&mut self, mp: &mut MacroProcessor) {
        if let Some(d) = &self.data {
            if let Ok((l, _)) = attrs::parse_lines(d) {
                self.lines = mp.process_lines(l, false);
            }
        }
        for c in self.children.values_mut() {
            c.process(mp);
        }
    }
    fn applied(&self, to: &str, out: &mut Vec<Attr>) {
        for (w, a) in &self.lines {
            if w.matches(to) {
                out.extend(a.iter().cloned());
            }
        }
        if let Some((dir, rest)) = to.split_once('/') {
            if let Some(c) = self.children.get(dir) {
                c.applied(rest, out);
            }
        }
    }
}

impl AttrTree {
    pub fn new(db: &mut Db, entries: &[TreeEntry]) -> Result<AttrTree> {
        let mut t = AttrTree { root: attr_node(db, entries)?, ..Default::default() };
        // FindSpecialAttributes: system, user (core.attributesfile) and info/attributes.
        let sys = if gitcmd::is_git_version_at_least("2.42.0") {
            gitcmd::git_no_lfs_command(&["var", "GIT_ATTR_SYSTEM"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string()).unwrap_or_default()
        } else {
            "/etc/gitattributes".into()
        };
        t.system = file_node(&sys);
        let user = cfg().git().get("core.attributesfile").unwrap_or_default();
        t.user = tools::expand_config_path(&user, "git/attributes").ok().and_then(|p| file_node(&p));
        t.repo = file_node(&format!("{}/info/attributes", cfg().local_git_dir()));
        Ok(t)
    }

    pub fn applied(&mut self, to: &str) -> Vec<Attr> {
        if !self.ready {
            let mut mp = MacroProcessor::new();
            for n in [&self.system, &self.user].into_iter().flatten() {
                n.read_macros(&mut mp);
            }
            self.root.read_macros(&mut mp);
            if let Some(r) = &self.repo {
                r.read_macros(&mut mp);
            }
            for n in [&mut self.system, &mut self.user, &mut self.repo].into_iter().flatten() {
                n.process(&mut mp);
            }
            self.root.process(&mut mp);
            self.ready = true;
        }
        let mut out = vec![];
        for n in [&self.system, &self.user].into_iter().flatten() {
            n.applied(to, &mut out);
        }
        self.root.applied(to, &mut out);
        if let Some(r) = &self.repo {
            r.applied(to, &mut out);
        }
        out
    }

    pub fn is_lfs(&mut self, path: &str) -> bool {
        let mut ok = false;
        for a in self.applied(path) {
            if a.k == "filter" {
                ok = a.v == "lfs";
            }
        }
        ok
    }
}

// Shared by the subcommands.

fn is_special_ref(refspec: &str) -> bool {
    if refspec == "refs/stash" {
        return true;
    }
    let parts: Vec<&str> = refspec.splitn(3, '/').collect();
    parts.len() >= 3 && matches!(format!("{}/{}", parts[0], parts[1]).as_str(), "refs/notes" | "refs/bisect" | "refs/replace")
}

fn include_exclude_refs(p: &Parsed, logger: &Logger, args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let inc_refs = p.strs("include-ref");
    let exc_refs = p.strs("exclude-ref");
    let everything = p.bool("everything");
    let hardcore = !inc_refs.is_empty() || !exc_refs.is_empty();
    let mut args = args.to_vec();
    if args.is_empty() && !hardcore && !everything {
        let cur = gitcmd::current_ref()?;
        if cur.typ == RefType::Other || cur.typ == RefType::RemoteBranch {
            return Err(Error::new(format!("Cannot migrate non-local ref: {}", cur.name)));
        }
        args.push(cur.name);
    }
    if everything && !args.is_empty() {
        return Err(Error::new("Cannot use --everything with explicit reference arguments"));
    }
    let (mut include, mut exclude) = (vec![], vec![]);
    for name in &args {
        // As git-lfs tests it: "^" has to start with the name.
        let (name, excluded) = if "^".starts_with(name.as_str()) { (name.get(1..).unwrap_or("").to_string(), true) } else { (name.clone(), false) };
        let r = gitcmd::resolve_ref(&name)?;
        if excluded {
            exclude.push(r.refspec());
        } else {
            include.push(r.refspec());
        }
    }
    if hardcore {
        if everything {
            return Err(Error::new("Cannot use --everything with --include-ref or --exclude-ref"));
        }
        include.extend(inc_refs);
        exclude.extend(exc_refs);
    } else if everything {
        for r in all_refs()? {
            match r.typ {
                RefType::LocalBranch | RefType::LocalTag | RefType::RemoteBranch => include.push(r.refspec()),
                RefType::Other if !is_special_ref(&r.refspec()) => include.push(r.refspec()),
                _ => {}
            }
        }
    } else {
        let bare = gitcmd::is_bare().map_err(|e| e.wrap("Unable to determine bareness"))?;
        if !bare {
            let remotes = gitcmd::remote_list()?;
            let skip = p.bool("skip-fetch");
            if !skip && !remotes.is_empty() {
                let mut w = logger.waiter("Fetching remote refs");
                let mut a: Vec<&str> = vec!["fetch"];
                if remotes.len() > 1 {
                    a.extend(["--multiple", "--"]);
                }
                a.extend(remotes.iter().map(String::as_str));
                let r = gitcmd::git_no_lfs_simple(&a);
                w.complete();
                r?;
            }
            for remote in &remotes {
                if skip {
                    for (name, _) in crate::gitscanner::cached_remote_refs(remote) {
                        exclude.push(format!("refs/remotes/{remote}/{name}"));
                    }
                } else {
                    for r in remote_refs_with_tags(remote) {
                        exclude.push(if r.typ == RefType::RemoteBranch { format!("refs/remotes/{}/{}", remote, r.name) } else { r.refspec() });
                    }
                }
            }
        }
    }
    Ok((include, exclude))
}

/// git.RemoteRefs(remote, true): heads (as remote branches) and tags.
fn remote_refs_with_tags(remote: &str) -> Vec<Ref> {
    let Ok(out) = gitcmd::git_no_lfs_command(&["ls-remote", "--heads", "-q", "--tags", remote]).stderr(std::process::Stdio::null()).output() else { return vec![] };
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((sha, r)) = line.split_once('\t') else { continue };
        let Some(rest) = r.strip_prefix("refs/") else { continue };
        let Some((ns, name)) = rest.split_once('/') else { continue };
        if name == "HEAD" {
            continue;
        }
        match ns {
            "heads" => v.push(Ref { name: name.to_string(), typ: RefType::RemoteBranch, sha: sha.to_string() }),
            "tags" => {
                if !name.ends_with("^{}") {
                    v.push(Ref { name: name.to_string(), typ: RefType::LocalTag, sha: sha.to_string() });
                }
            }
            _ => {}
        }
    }
    v
}

fn ensure_working_copy_clean(p: &Parsed) {
    let dirty = match gitcmd::is_bare() {
        Ok(true) => false,
        Ok(false) => match gitcmd::git_simple(&["status", "--porcelain"]) {
            Ok(o) => !o.is_empty(),
            Err(e) => exit_with_error(&e.wrap("Could not determine if working copy is dirty")),
        },
        Err(e) => exit_with_error(&e.wrap("Could not determine if working copy is dirty")),
    };
    if !dirty {
        return;
    }
    let mut proceed = false;
    if p.bool("yes") {
        proceed = true;
    } else {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        loop {
            eprint!("override changes in your working copy?  All uncommitted changes will be lost! [y/N] ");
            let mut s = String::new();
            match lock.read_line(&mut s) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => exit_with_error(&Error::from(e).wrap("Could not read answer")),
            }
            match s.trim() {
                "n" | "N" | "" => break,
                "y" | "Y" => {
                    proceed = true;
                    break;
                }
                _ => {}
            }
            if !s.ends_with('\n') {
                eprintln!();
            }
        }
    }
    if proceed {
        eprintln!("changes in your working copy will be overridden ...");
    } else {
        exit("working copy must not be dirty");
    }
}

fn rewriter_for(p: &Parsed, logger: &Arc<Logger>) -> Rewriter {
    let inc = if p.changed("include") { tools::clean_paths(&p.str("include"), ",") } else { vec![] };
    let exc = if p.changed("exclude") { tools::clean_paths(&p.str("exclude"), ",") } else { vec![] };
    let db = match Db::open() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    Rewriter::new(db, Filter::new(&inc, &exc, PatternType::GitAttributes), logger.clone())
}

fn tracked_from_attrs(db: &mut Db, entries: &[TreeEntry], cache: &mut HashMap<Vec<u8>, Vec<String>>) -> Result<Vec<String>> {
    let mut oid = None;
    for e in entries {
        if e.name_str().to_lowercase() == ".gitattributes" && e.is_blob() {
            if e.is_link() {
                return Err(Error::new("expected '.gitattributes' to be a file, got a symbolic link"));
            }
            oid = Some(e.oid.clone());
            break;
        }
    }
    let Some(oid) = oid else { return Ok(vec![]) };
    if let Some(s) = cache.get(&oid) {
        return Ok(s.clone());
    }
    let data = db.blob(&oid)?;
    let mut set: Vec<String> = vec![];
    for l in String::from_utf8_lossy(&data).lines() {
        let l = l.strip_suffix('\r').unwrap_or(l).to_string();
        if !set.contains(&l) {
            set.push(l);
        }
    }
    cache.insert(oid, set.clone());
    Ok(set)
}

fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut v = a.to_vec();
    for x in b {
        if !v.contains(x) {
            v.push(x.clone());
        }
    }
    v
}

fn attrs_blob(db: &mut Db, patterns: &[String]) -> Result<Vec<u8>> {
    let mut s = String::new();
    for p in patterns {
        s.push_str(p);
        s.push('\n');
    }
    db.write("blob", s.as_bytes())
}

fn checkout_non_bare(logger: &Logger) -> Result<()> {
    if gitcmd::is_bare().unwrap_or(false) {
        return Ok(());
    }
    let mut t = logger.waiter("Checkout");
    let r = gitcmd::git_no_lfs_simple(&["checkout", "--force"]);
    t.complete();
    r.map(|_| ())
}

/// The clean filter into a buffer (commands' clean()).
fn clean_blob(data: &[u8], path: &str) -> Result<Vec<u8>> {
    let mut out = vec![];
    let mut r: &[u8] = data;
    super::filter::do_clean(&mut out, &mut r, path, data.len() as i64)?;
    Ok(out)
}

// import

fn import(p: &Parsed) {
    ensure_working_copy_clean(p);
    let logger = Logger::new(Sink::Stderr, cfg().force_progress());
    let _ = super::install::install_hooks(false);
    let fixup = p.bool("fixup");
    if p.bool("no-rewrite") {
        if fixup {
            exit_with_error(&Error::new("--no-rewrite and --fixup cannot be combined"));
        }
        import_no_rewrite(p, &logger);
        logger.close();
        return;
    }
    if fixup && (p.changed("include") || p.changed("exclude")) {
        exit_with_error(&Error::new("Cannot use --fixup with --include, --exclude"));
    }
    let mut rw = rewriter_for(p, &logger);
    let tracked: Vec<String> = {
        let mut v = vec![];
        for i in rw.filter.include_strs() {
            let l = format!("{} filter=lfs diff=lfs merge=lfs -text", super::track::escape_attr_pattern(&i));
            if !v.contains(&l) {
                v.push(l);
            }
        }
        for e in rw.filter.exclude_strs() {
            let l = format!("{} !text -filter -merge -diff", super::track::escape_attr_pattern(&e));
            if !v.contains(&l) {
                v.push(l);
            }
        }
        v
    };
    let above = match tools::parse_bytes(&p.str("above")) {
        Ok(a) => a,
        Err(e) => exit_with_error(&e.wrap("Cannot parse --above=<n>")),
    };
    if above > 0 && (p.changed("include") || p.changed("exclude") || fixup) {
        exit_with_error(&Error::new("Cannot use --above with --include, --exclude, --fixup"));
    }
    setup_repository();
    let (include, exclude) = include_exclude_refs(p, &logger, &p.args).unwrap_or_else(|e| exit_with_error(&e));
    let exts: std::cell::RefCell<Vec<String>> = Default::default();
    let fixups: std::cell::RefCell<Option<AttrTree>> = Default::default();
    let mut cache: HashMap<Vec<u8>, Vec<String>> = HashMap::new();
    let mut blob_fn = |path: &str, data: Vec<u8>| -> Result<Vec<u8>> {
        let base = path.rsplit('/').next().unwrap_or(path);
        if base == ".gitattributes" {
            return Ok(data);
        }
        if above > 0 && (data.len() as u64) < above {
            return Ok(data);
        }
        if fixup && !fixups.borrow_mut().as_mut().is_some_and(|f| f.is_lfs(path)) {
            return Ok(data);
        }
        let out = clean_blob(&data, path)?;
        let ext = base.rfind('.').map(|i| &base[i..]).unwrap_or("");
        let line = if !ext.is_empty() && above == 0 {
            format!("*{ext} filter=lfs diff=lfs merge=lfs -text")
        } else {
            format!("/{} filter=lfs diff=lfs merge=lfs -text", super::track::escape_glob(path))
        };
        let mut e = exts.borrow_mut();
        if !e.contains(&line) {
            e.push(line);
        }
        Ok(out)
    };
    let mut pre_fn = |db: &mut Db, path: &str, t: &[TreeEntry]| -> Result<()> {
        if fixup && path == "/" {
            *fixups.borrow_mut() = Some(AttrTree::new(db, t)?);
        }
        Ok(())
    };
    let mut tree_fn = |db: &mut Db, path: &str, t: Vec<TreeEntry>| -> Result<Vec<TreeEntry>> {
        if path != "/" || fixup {
            return Ok(t);
        }
        let ours = if tracked.is_empty() { exts.borrow().clone() } else { tracked.clone() };
        if ours.is_empty() {
            return Ok(t);
        }
        let theirs = tracked_from_attrs(db, &t, &mut cache)?;
        let blob = attrs_blob(db, &union(&theirs, &ours))?;
        Ok(gitobj::merge_tree(&t, TreeEntry { mode: 0o100644, name: b".gitattributes".to_vec(), oid: blob }))
    };
    let mut o = Options {
        include,
        exclude,
        update_refs: true,
        verbose: p.bool("verbose"),
        object_map: p.str("object-map"),
        blob_fn: Some(&mut blob_fn),
        tree_pre_fn: Some(&mut pre_fn),
        tree_fn: Some(&mut tree_fn),
    };
    if let Err(e) = rw.rewrite(&mut o) {
        logger.close();
        exit_with_error(&e);
    }
    if let Err(e) = checkout_non_bare(&logger) {
        logger.close();
        exit_with_error(&e.wrap("Could not checkout"));
    }
    logger.close();
}

fn import_no_rewrite(p: &Parsed, logger: &Logger) {
    if p.args.is_empty() {
        exit_with_error(&Error::new("Expected one or more files with --no-rewrite"));
    }
    let r = gitcmd::current_ref().unwrap_or_else(|e| exit_with_error(&e.wrap("Unable to find current reference")));
    let mut db = Db::open().unwrap_or_else(|e| exit_with_error(&e));
    let sha = unhex(&r.sha);
    let commit = db.commit(&sha).unwrap_or_else(|e| exit_with_error(&e.wrap("Unable to load commit")));
    let mut root = commit.tree.clone();
    let filter = attrs::lfs_attribute_filter();
    if filter.include_strs().is_empty() {
        exit_with_error(&Error::new("No Git LFS filters found in '.gitattributes'"));
    }
    for f in &p.args {
        if !filter.allows(f) {
            exit_with_error(&Error::new(format!("File {f} did not match any Git LFS filters in '.gitattributes'")));
        }
    }
    for f in &p.args {
        root = rewrite_path(&mut db, &root, f).unwrap_or_else(|e| exit_with_error(&e.wrap(format!("Could not rewrite {}", tools::quote(f)))));
    }
    let sig = |kind: &str| -> String {
        let (name, email) = cfg().user_data(kind);
        let up = kind.to_uppercase();
        let when = std::env::var(format!("GIT_{up}_DATE")).ok().and_then(|d| parse_git_env_date(&d)).unwrap_or_else(|| {
            let now = tools::unix_secs(std::time::SystemTime::now());
            (now, local_offset(now))
        });
        format!("{} <{}> {} {}", name, email, when.0, format_offset(when.1))
    };
    let msg = if p.changed("message") { p.str("message") } else { format!("{}: convert to Git LFS", p.args.join(",")) };
    let mut data = format!("tree {}\nparent {}\n", tools::hex(&root), r.sha);
    data.push_str(&format!("author {}\ncommitter {}\n\n{}\n", sig("author"), sig("committer"), msg));
    let oid = db.write("commit", data.as_bytes()).unwrap_or_else(|e| exit_with_error(&e.wrap("Unable to write commit")));
    let refname = if r.typ == RefType::Head || r.typ == RefType::Other { "HEAD".to_string() } else { r.refspec() };
    if let Err(e) = gitcmd::git_simple(&["update-ref", "-m", "git lfs migrate import --no-rewrite", &refname, &tools::hex(&oid), &r.sha]) {
        exit_with_error(&e.wrap("Unable to update ref"));
    }
    if let Err(e) = checkout_non_bare(logger) {
        exit_with_error(&e.wrap("Could not checkout"));
    }
}

fn local_offset(secs: i64) -> i64 {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm.tm_gmtoff as i64
}

fn format_offset(off: i64) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{}{:02}{:02}", sign, a / 3600, a / 60 % 60)
}

/// GIT_AUTHOR_DATE / GIT_COMMITTER_DATE: "@secs +zzzz", "secs +zzzz", or RFC 2822 / ISO forms.
fn parse_git_env_date(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let re = regex::Regex::new(r"^@?(\d+)(?:\s+([+-])(\d{2})(\d{2}))?$").unwrap();
    if let Some(m) = re.captures(s) {
        let secs: i64 = m[1].parse().ok()?;
        let off = match m.get(2) {
            Some(sg) => {
                let o = m[3].parse::<i64>().ok()? * 3600 + m[4].parse::<i64>().ok()? * 60;
                if sg.as_str() == "-" {
                    -o
                } else {
                    o
                }
            }
            None => 0,
        };
        return Some((secs, off));
    }
    let iso = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})\s*(Z|[+-]\d{2}:?\d{2})?$").unwrap();
    if let Some(m) = iso.captures(s) {
        let n = |i: usize| m[i].parse::<i64>().unwrap();
        let local = tools::days_from_civil(n(1), n(2), n(3)) * 86400 + n(4) * 3600 + n(5) * 60 + n(6);
        let off = match m.get(7).map(|x| x.as_str()) {
            None | Some("Z") => 0,
            Some(z) => {
                let d: String = z.chars().filter(|c| c.is_ascii_digit()).collect();
                let o = d[..2].parse::<i64>().ok()? * 3600 + d[2..4].parse::<i64>().ok()? * 60;
                if z.starts_with('-') {
                    -o
                } else {
                    o
                }
            }
        };
        return Some((local - off, off));
    }
    let rfc = regex::Regex::new(r"^(?:\w{3},\s*)?(\d{1,2})\s+(\w{3})\s+(\d{4})\s+(\d{2}):(\d{2}):(\d{2})\s+([+-])(\d{2})(\d{2})$").unwrap();
    if let Some(m) = rfc.captures(s) {
        const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let mon = M.iter().position(|x| *x == &m[2])? as i64 + 1;
        let n = |i: usize| m[i].parse::<i64>().unwrap();
        let local = tools::days_from_civil(n(3), mon, n(1)) * 86400 + n(4) * 3600 + n(5) * 60 + n(6);
        let o = n(8) * 3600 + n(9) * 60;
        let off = if &m[7] == "-" { -o } else { o };
        return Some((local - off, off));
    }
    None
}

/// rewriteTree of import --no-rewrite: the file's blob cleaned, the trees above rewritten.
fn rewrite_path(db: &mut Db, root: &[u8], path: &str) -> Result<Vec<u8>> {
    let tree = db.tree(root)?;
    match path.split_once('/') {
        None => {
            let Some(e) = tree.iter().find(|e| e.name == path.as_bytes()) else { return Err(Error::new(format!("unable to find entry {path} in tree"))) };
            let data = db.blob(&e.oid)?;
            let out = clean_blob(&data, &e.name_str())?;
            let oid = db.write("blob", &out)?;
            let t = gitobj::merge_tree(&tree, TreeEntry { mode: e.mode, name: e.name.clone(), oid });
            db.write_tree(&t)
        }
        Some((head, tail)) => {
            let Some(e) = tree.iter().find(|e| e.name == head.as_bytes()).cloned() else { return Err(Error::new(format!("unable to find entry {head} in tree"))) };
            if !e.is_tree() {
                return Err(Error::new(format!("expected {head} to be a tree, got {}", if e.is_blob() { "blob" } else { "commit" })));
            }
            let sub = rewrite_path(db, &e.oid, tail)?;
            let t = gitobj::merge_tree(&tree, TreeEntry { mode: e.mode, name: e.name.clone(), oid: sub });
            db.write_tree(&t)
        }
    }
}

// export

fn export(p: &Parsed) {
    ensure_working_copy_clean(p);
    let logger = Logger::new(Sink::Stderr, cfg().force_progress());
    let mut rw = rewriter_for(p, &logger);
    if rw.filter.include_strs().is_empty() {
        exit_with_error(&Error::new("One or more files must be specified with --include"));
    }
    let mut tracked: Vec<String> = vec![];
    for i in rw.filter.include_strs() {
        let l = format!("{} !text !filter !merge !diff", super::track::escape_attr_pattern(&i));
        if !tracked.contains(&l) {
            tracked.push(l);
        }
    }
    for e in rw.filter.exclude_strs() {
        let l = format!("{} filter=lfs diff=lfs merge=lfs -text", super::track::escape_attr_pattern(&e));
        if !tracked.contains(&l) {
            tracked.push(l);
        }
    }
    setup_repository();
    let (include, exclude) = include_exclude_refs(p, &logger, &p.args).unwrap_or_else(|e| exit_with_error(&e));
    let remote = if p.changed("remote") { p.str("remote") } else { cfg().remote() };
    let url = crate::endpoint::remote_endpoint("download", &remote).url;
    if url.is_empty() && p.changed("remote") {
        exit_with_error(&Error::new(format!("Invalid remote {remote} provided")));
    }
    let fs = cfg().filesystem();
    if !url.is_empty() {
        let mut q = super::pull::new_download_queue(crate::tq::Manifest::get("download", &remote), &remote, None, false);
        let mut s = crate::gitscanner::Scanner::new();
        let filter = &rw.filter;
        let mut found = vec![];
        let _ = s.scan_refs_multi(&include, &exclude, &mut |res| {
            if let Ok(wp) = res {
                if filter.allows(&wp.name) {
                    found.push(wp);
                }
            }
        });
        for wp in found {
            let Ok(path) = fs.object_path(wp.oid()) else { continue };
            if std::fs::metadata(&path).is_err() {
                q.add(&wp.name, &path, wp.oid(), wp.size(), false);
            }
        }
        q.wait();
        if let Some(e) = q.take_errors().into_iter().next() {
            exit_with_error(&e);
        }
    }
    let mut cache: HashMap<Vec<u8>, Vec<String>> = HashMap::new();
    let mut blob_fn = |path: &str, data: Vec<u8>| -> Result<Vec<u8>> {
        if path.rsplit('/').next() == Some(".gitattributes") {
            return Ok(data);
        }
        let ptr = match crate::pointer::decode(&data) {
            Ok(p) => p,
            Err(e) if e.is(crate::errors::Kind::NotAPointer) => return Ok(data),
            Err(e) => return Err(e),
        };
        let path = fs.object_path(&ptr.oid)?;
        std::fs::read(&path).map_err(|e| tools::path_err("open", &path, &e))
    };
    let mut tree_fn = |db: &mut Db, path: &str, t: Vec<TreeEntry>| -> Result<Vec<TreeEntry>> {
        if path != "/" {
            return Ok(t);
        }
        let theirs = tracked_from_attrs(db, &t, &mut cache)?;
        let blob = attrs_blob(db, &union(&theirs, &tracked))?;
        Ok(gitobj::merge_tree(&t, TreeEntry { mode: 0o100644, name: b".gitattributes".to_vec(), oid: blob }))
    };
    let mut o = Options {
        include,
        exclude,
        update_refs: true,
        verbose: p.bool("verbose"),
        object_map: p.str("object-map"),
        blob_fn: Some(&mut blob_fn),
        tree_pre_fn: None,
        tree_fn: Some(&mut tree_fn),
    };
    if let Err(e) = rw.rewrite(&mut o) {
        logger.close();
        exit_with_error(&e);
    }
    if !gitcmd::is_bare().unwrap_or(false) {
        let mut t = logger.waiter("Checkout");
        let r = gitcmd::git_no_lfs_simple(&["checkout", "--force"]);
        t.complete();
        if let Err(e) = r {
            logger.close();
            exit_with_error(&e);
        }
    }
    logger.close();
    let mut fp = crate::lfs::fetch_prune_config();
    fp.fetch_recent_refs_days = 0;
    super::prune::prune(&fp, false, false, false, false, true);
}

// info

struct InfoEntry {
    qualifier: String,
    separate: bool,
    bytes_above: i64,
    total_above: i64,
    total: i64,
}

fn info(p: &Parsed) {
    let logger = Logger::new(Sink::Stderr, cfg().force_progress());
    let mut rw = rewriter_for(p, &logger);
    let above = match tools::parse_bytes(&p.str("above")) {
        Ok(a) => a,
        Err(e) => exit_with_error(&e.wrap("cannot parse --above=<n>")),
    };
    let mut unit = 0u64;
    if p.changed("unit") {
        unit = match parse_byte_unit(&p.str("unit")) {
            Ok(u) => u,
            Err(e) => exit_with_error(&e.wrap("cannot parse --unit=<unit>")),
        };
    }
    // 0 follow, 1 no-follow, 2 ignore
    let mut mode = 0;
    if p.changed("pointers") {
        mode = match p.str("pointers").as_str() {
            "follow" => 0,
            "no-follow" => 1,
            "ignore" => 2,
            _ => exit_with_error(&Error::new("Unsupported --pointers option value")),
        };
    }
    let fixup = p.bool("fixup");
    if fixup {
        if p.changed("include") || p.changed("exclude") {
            exit_with_error(&Error::new("Cannot use --fixup with --include, --exclude"));
        }
        if p.changed("pointers") && mode != 2 {
            exit_with_error(&Error::new(format!("Cannot use --fixup with --pointers={}", p.str("pointers"))));
        }
        mode = 2;
    }
    let exts: std::cell::RefCell<BTreeMap<String, InfoEntry>> = Default::default();
    let pointers: std::cell::RefCell<InfoEntry> = std::cell::RefCell::new(InfoEntry { qualifier: "LFS Objects".into(), separate: true, bytes_above: 0, total_above: 0, total: 0 });
    let fixups: std::cell::RefCell<Option<AttrTree>> = Default::default();
    let mut blob_fn = |path: &str, data: Vec<u8>| -> Result<Vec<u8>> {
        if fixup {
            if path.rsplit('/').next() == Some(".gitattributes") {
                return Ok(data);
            }
            if !fixups.borrow_mut().as_mut().is_some_and(|f| f.is_lfs(path)) {
                return Ok(data);
            }
        }
        let ptr = if mode != 1 && data.len() < crate::pointer::BLOB_SIZE_CUTOFF { crate::pointer::decode(&data).ok() } else { None };
        let size;
        let mut ex = exts.borrow_mut();
        let mut pe = pointers.borrow_mut();
        let entry: &mut InfoEntry = match ptr {
            Some(p) => {
                if mode == 2 {
                    return Ok(data);
                }
                size = p.size;
                &mut pe
            }
            None => {
                size = data.len() as i64;
                let base = path.rsplit('/').next().unwrap_or(path);
                let ext = base.rfind('.').map(|i| &base[i..]).unwrap_or("");
                let group = if !ext.is_empty() { format!("*{ext}") } else { base.to_string() };
                ex.entry(group.clone()).or_insert(InfoEntry { qualifier: group, separate: false, bytes_above: 0, total_above: 0, total: 0 })
            }
        };
        entry.total += 1;
        if size > above as i64 {
            entry.total_above += 1;
            entry.bytes_above += size;
        }
        Ok(data)
    };
    let mut pre_fn = |db: &mut Db, path: &str, t: &[TreeEntry]| -> Result<()> {
        if fixup {
            if path == "/" {
                *fixups.borrow_mut() = Some(AttrTree::new(db, t)?);
            }
            return Ok(());
        }
        for e in t {
            if e.name_str().to_lowercase() == ".gitattributes" && e.is_blob() {
                if e.is_link() {
                    return Err(Error::new("expected '.gitattributes' to be a file, got a symbolic link"));
                }
                break;
            }
        }
        Ok(())
    };
    setup_repository();
    let (include, exclude) = include_exclude_refs(p, &logger, &p.args).unwrap_or_else(|e| exit_with_error(&e));
    let mut o = Options { include, exclude, update_refs: false, verbose: false, object_map: String::new(), blob_fn: Some(&mut blob_fn), tree_pre_fn: Some(&mut pre_fn), tree_fn: None };
    if let Err(e) = rw.rewrite(&mut o) {
        logger.close();
        exit_with_error(&e);
    }
    logger.close();
    let mut entries: Vec<InfoEntry> = std::mem::take(&mut *exts.borrow_mut()).into_values().filter(|e| e.total_above > 0).collect();
    // sort.Reverse(EntriesBySize): bytes descending, then qualifier ascending.
    entries.sort_by(|a, b| b.bytes_above.cmp(&a.bytes_above).then_with(|| a.qualifier.cmp(&b.qualifier)));
    let top = p.int("top", 5).clamp(0, entries.len() as i64) as usize;
    entries.truncate(top);
    let pe = pointers.into_inner();
    if pe.total > 0 {
        entries.push(pe);
    }
    print_info(&entries, unit);
}

fn parse_byte_unit(s: &str) -> Result<u64> {
    let u = s.trim().to_lowercase();
    match u.as_str() {
        "" | "b" => Ok(1),
        "kib" => Ok(1 << 10),
        "mib" => Ok(1 << 20),
        "gib" => Ok(1 << 30),
        "tib" => Ok(1 << 40),
        "pib" => Ok(1 << 50),
        "kb" => Ok(1000),
        "mb" => Ok(1_000_000),
        "gb" => Ok(1_000_000_000),
        "tb" => Ok(1_000_000_000_000),
        "pb" => Ok(1_000_000_000_000_000),
        _ => Err(Error::new(format!("unknown unit: {}", tools::quote(&u)))),
    }
}

fn ljust(v: &[String]) -> Vec<String> {
    let w = v.iter().map(|s| s.len()).max().unwrap_or(0);
    v.iter().map(|s| format!("{s}{}", " ".repeat(w - s.len()))).collect()
}

fn rjust(v: &[String]) -> Vec<String> {
    let w = v.iter().map(|s| s.len()).max().unwrap_or(0);
    v.iter().map(|s| format!("{}{s}", " ".repeat(w - s.len()))).collect()
}

fn print_info(e: &[InfoEntry], unit: u64) {
    if e.is_empty() {
        return;
    }
    let mut exts = vec![];
    let mut sizes = vec![];
    let mut stats = vec![];
    let mut pcts = vec![];
    for x in e {
        let pct = 100.0 * (x.total_above as f64 / x.total as f64);
        sizes.push(if unit > 0 { tools::format_bytes_unit(x.bytes_above as u64, unit) } else { tools::format_bytes(x.bytes_above as u64) });
        stats.push(if x.total == 1 { format!("{}/{} file ", x.total_above, x.total) } else { format!("{}/{} files", x.total_above, x.total) });
        pcts.push(format!("{pct:.0}%"));
        exts.push(x.qualifier.clone());
    }
    let (exts, sizes, stats, pcts) = (ljust(&exts), ljust(&sizes), rjust(&stats), rjust(&pcts));
    let mut out = vec![];
    for i in 0..e.len() {
        if i > 0 && e[i].separate {
            out.push(String::new());
        }
        out.push([exts[i].as_str(), &sizes[i], &stats[i], &pcts[i]].join("\t"));
    }
    println!("{}", out.join("\n"));
}

pub fn commands() -> Vec<Cmd> {
    let common = || {
        vec![
            flag("include", Some('I'), K::Str),
            flag("exclude", Some('X'), K::Str),
            flag("include-ref", None, K::StrSlice),
            flag("exclude-ref", None, K::StrSlice),
            flag("everything", None, K::Bool),
            flag("skip-fetch", None, K::Bool),
            flag("yes", Some('y'), K::Bool),
        ]
    };
    let mut f_import = common();
    f_import.extend([
        flag("above", None, K::Str),
        flag("verbose", None, K::Bool),
        flag("object-map", None, K::Str),
        flag("no-rewrite", None, K::Bool),
        flag("message", Some('m'), K::Str),
        flag("fixup", None, K::Bool),
    ]);
    let mut f_export = common();
    f_export.extend([flag("verbose", None, K::Bool), flag("object-map", None, K::Str), flag("remote", None, K::Str)]);
    let mut f_info = common();
    f_info.extend([flag("top", None, K::Int), flag("above", None, K::Str), flag("unit", None, K::Str), flag("pointers", None, K::Str), flag("fixup", None, K::Bool)]);
    let mut m = super::cmd("migrate", migrate_root, common());
    m.subs = vec![super::cmd("import", import, f_import), super::cmd("export", export, f_export), super::cmd("info", info, f_info)];
    vec![m]
}

fn migrate_root(_p: &Parsed) {
    super::print_help("migrate");
}
