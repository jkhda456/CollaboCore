//! pull and checkout (commands/command_pull.go, command_checkout.go, pull.go): objects
//! written into the working tree in place of their pointers, and the index refreshed.

use super::{cmd, error, exit, exit_with_error, full_error, logged_error, panic_exit, print, setup_repository, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Kind, Result};
use crate::filter::{Filter, PatternType};
use crate::gitcmd;
use crate::gitscanner::{Scanner, WrappedPointer};
use crate::tasklog::{Logger, Sink};
use crate::tools;
use crate::tq::{self, Direction, Manifest, Meter, TransferQueue};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Stdio};
use std::sync::Arc;

/// `git update-index -q --refresh --stdin`, fed as files are checked out.
#[derive(Default)]
pub struct GitIndexer {
    child: Option<Child>,
}

impl GitIndexer {
    pub fn add(&mut self, path: &str) -> Result<()> {
        if self.child.is_none() {
            let c = crate::subprocess::command("git", &["update-index", "-q", "--refresh", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(Error::from)?;
            self.child = Some(c);
        }
        let si = self.child.as_mut().unwrap().stdin.as_mut().unwrap();
        let _ = si.write_all(format!("{path}\n").as_bytes());
        Ok(())
    }

    /// The command's end: an error with its output when it failed.
    pub fn close(&mut self) -> std::result::Result<(), (Error, String)> {
        let Some(mut c) = self.child.take() else { return Ok(()) };
        c.stdin.take();
        match c.wait_with_output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
                out.push_str(&String::from_utf8_lossy(&o.stderr));
                Err((Error::new(crate::subprocess::exit_text(&o.status)), out))
            }
            Err(e) => Err((Error::from(e), String::new())),
        }
    }
}

/// singleCheckout (or the no-op one when the filters are not installed).
pub struct SingleCheckout {
    indexer: GitIndexer,
    has_work_tree: bool,
    pub remote: String,
    skip: bool,
}

impl SingleCheckout {
    pub fn new(remote: &str) -> SingleCheckout {
        let clean = cfg().git().get("filter.lfs.clean").unwrap_or_default();
        if clean.is_empty() {
            return SingleCheckout { indexer: GitIndexer::default(), has_work_tree: false, remote: remote.into(), skip: true };
        }
        let wd = cfg().local_working_dir();
        let mut has = !wd.is_empty();
        if has {
            if let Err(e) = std::env::set_current_dir(&wd) {
                full_error(&tools::path_err("chdir", &wd, &e).wrap(format!("Checkout error trying to change directory: {wd}")));
                has = false;
            }
        }
        SingleCheckout { indexer: GitIndexer::default(), has_work_tree: has, remote: remote.into(), skip: false }
    }

    pub fn manifest(&self) -> Arc<Manifest> {
        Manifest::get("download", &self.remote)
    }

    pub fn skip(&self) -> bool {
        self.skip
    }

    pub fn run(&mut self, p: &WrappedPointer) {
        if self.skip || !self.has_work_tree {
            return;
        }
        let walk = dir_walk(&p.name, false);
        let mut not_exist = false;
        let mut file_ptr = None;
        match walk {
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    let e = Error::from(e);
                    logged_error(&e, &format!("Checkout error trying to check path for {}: {}", tools::quote(&p.name), e));
                    return;
                }
                not_exist = true;
            }
            Ok(()) => match crate::pointer::decode_from_file(&p.name) {
                Ok(fp) => file_ptr = Some(fp),
                Err(e) => {
                    let missing = std::fs::symlink_metadata(&p.name).is_err();
                    if missing {
                        not_exist = true;
                    } else if e.is(Kind::NotAPointer) || e.is(Kind::BadPointerKey) {
                        return;
                    } else {
                        logged_error(&e, &format!("Checkout error for {}: {}", tools::quote(&p.name), e));
                        return;
                    }
                }
            },
        }
        if not_exist {
            match gitcmd::git_simple(&["diff-index", "--cached", "HEAD", "--", &p.name]) {
                Err(e) => {
                    logged_error(&e, &format!("Checkout error trying to run diff-index: {e}"));
                    return;
                }
                Ok(out) => {
                    if out.starts_with(":100644 000000 ") || out.starts_with(":100755 000000 ") {
                        return;
                    }
                }
            }
        }
        if let Some(fp) = &file_ptr {
            if fp.oid != p.p.oid {
                return;
            }
        }
        if not_exist {
            if let Err(e) = dir_walk(&p.name, true) {
                let e = Error::from(e);
                logged_error(&e, &format!("Checkout error trying to create path for {}: {}", tools::quote(&p.name), e));
                return;
            }
        }
        if let Err(e) = self.run_to_path(p, &p.name) {
            if e.is(Kind::DownloadDeclined) {
                error(&format!("Skipped checkout for {}, content not local. Use fetch to download.", tools::quote(&p.name)));
            } else {
                full_error(&e.wrap(format!("could not check out {}", tools::quote(&p.name))));
            }
            return;
        }
        if let Err(e) = self.indexer.add(&p.name) {
            panic_exit(&e, "Could not update the index");
        }
    }

    pub fn run_to_path(&self, p: &WrappedPointer, path: &str) -> Result<()> {
        if self.skip {
            return Ok(());
        }
        crate::gitfilter::smudge_to_file(path, &p.p, &p.name, false, None)
    }

    pub fn close(&mut self) {
        if let Err((e, out)) = self.indexer.close() {
            logged_error(&e, &format!("Error updating the Git index:\n{out}"));
        }
    }
}

/// tools.DirWalker: each directory of the file's path must be a directory (created when
/// asked), never a symbolic link.
fn dir_walk(file: &str, create: bool) -> std::io::Result<()> {
    let Some(i) = file.rfind('/') else { return Ok(()) };
    let mut current = String::new();
    for d in file[..i].split('/') {
        if d.is_empty() || d == "." || d == ".." {
            return Err(std::io::Error::other(format!("invalid directory {} in path: {}\ninvalid directory", tools::quote(d), tools::quote(&file[..i]))));
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(d);
        match std::fs::symlink_metadata(&current) {
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound || !create {
                    return Err(e);
                }
                std::fs::create_dir(&current)?;
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&current, std::fs::Permissions::from_mode(cfg().repository_permissions(true)));
            }
            Ok(m) if !m.is_dir() => return Err(std::io::Error::other(format!("not a directory: {}\nnot a directory", tools::quote(&current)))),
            Ok(_) => {}
        }
    }
    Ok(())
}

/// pathConverterArgs: (repo dir, current dir, same).
fn converter_args() -> (String, String, bool) {
    let cur = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let cur = tools::resolve_symlinks(&cur);
    let repo = cfg().local_working_dir();
    let same = repo == cur;
    (repo, cur, same)
}

/// currentToRepoPathConverter.
pub fn current_to_repo_path(file: &str) -> String {
    let (repo, cur, same) = converter_args();
    if same {
        return file.to_string();
    }
    let abs = if file.starts_with('/') { tools::resolve_symlinks(file) } else { format!("{cur}/{file}") };
    match crate::attrs::rel(&tools::clean_str(&repo), &tools::clean_str(&abs)) {
        Some(r) => r,
        None => abs,
    }
}

/// currentToRepoPatternConverter.
pub fn current_to_repo_pattern(file: &str) -> String {
    let mut pattern = current_to_repo_path(file);
    if std::fs::metadata(file).is_ok_and(|m| m.is_dir()) {
        pattern.push('/');
    }
    if let Some(p) = pattern.strip_prefix("./") {
        pattern = if p.is_empty() { "**".into() } else { p.to_string() };
    }
    pattern
}

/// buildFilepathFilter: --include/--exclude when given, else lfs.fetchinclude/exclude when
/// fetch options apply.
pub fn build_filter(p: &Parsed, use_fetch_options: bool) -> Filter {
    let include = if p.changed("include") {
        tools::clean_paths(&p.str("include"), ",")
    } else if use_fetch_options {
        cfg().fetch_include_paths()
    } else {
        vec![]
    };
    let exclude = if p.changed("exclude") {
        tools::clean_paths(&p.str("exclude"), ",")
    } else if use_fetch_options {
        cfg().fetch_exclude_paths()
    } else {
        vec![]
    };
    Filter::new(&include, &exclude, PatternType::GitIgnore)
}

pub fn fetch_remote_ref() -> crate::gitcmd::Ref {
    gitcmd::tracking_ref(&cfg().current_ref())
}

pub fn new_download_queue(manifest: Arc<Manifest>, remote: &str, meter: Option<Arc<Meter>>, dry_run: bool) -> TransferQueue {
    TransferQueue::new(
        Direction::Download,
        manifest,
        remote,
        tq::Options { dry_run, meter, remote_ref: Some(fetch_remote_ref()), batch_size: cfg().transfer_batch_size(), cb: None },
    )
}

fn pull_cmd(p: &Parsed) {
    super::require_git_version();
    setup_repository();
    if let Some(r) = p.args.first() {
        if let Err(e) = cfg().set_valid_remote(r) {
            exit(&format!("Invalid remote name {}: {}", tools::quote(r), e));
        }
    }
    let f = build_filter(p, true);
    pull(f);
}

pub fn pull(filter: Filter) {
    let r = match gitcmd::current_ref() {
        Ok(r) => r,
        Err(e) => panic_exit(&e, "Could not pull"),
    };
    let logger = Logger::new(Sink::Stdout, cfg().force_progress());
    let meter = Meter::new(Direction::Download, false);
    meter.logger_from_env();
    meter.enqueue(&logger);
    let remote = cfg().remote();
    let checkout = std::rc::Rc::new(std::cell::RefCell::new(SingleCheckout::new(&remote)));
    let mut q = new_download_queue(checkout.borrow().manifest(), &remote, Some(meter.clone()), false);
    let pointers: std::rc::Rc<std::cell::RefCell<HashMap<String, Vec<WrappedPointer>>>> = Default::default();
    {
        let ptrs = pointers.clone();
        let co = checkout.clone();
        q.watch(Box::new(move |t: &tq::Transfer| {
            let list = ptrs.borrow_mut().remove(&t.oid).unwrap_or_default();
            for p in list {
                co.borrow_mut().run(&p);
            }
        }));
    }
    let mut found: Vec<WrappedPointer> = vec![];
    let mut s = Scanner::new();
    s.filter = Some(filter);
    let r = s.scan_lfs_files(&r.sha, &mut |res| match res {
        Ok(p) => found.push(p),
        Err(e) => logged_error(&e, &format!("Scanner error: {e}")),
    });
    if let Err(e) = r {
        checkout.borrow_mut().close();
        exit_with_error(&e);
    }
    let fs = cfg().filesystem();
    for p in found {
        {
            let mut m = pointers.borrow_mut();
            if let Some(v) = m.get_mut(p.oid()) {
                v.push(p);
                continue;
            }
        }
        crate::gitfilter::link_or_copy_from_reference(p.oid(), p.size());
        if fs.object_exists(p.oid(), p.size()) {
            checkout.borrow_mut().run(&p);
            continue;
        }
        meter.add(p.size());
        crate::trace!("fetch {} [{}]", p.name, p.oid());
        let (name, oid, size) = (p.name.clone(), p.oid().to_string(), p.size());
        pointers.borrow_mut().entry(oid.clone()).or_default().push(p);
        match fs.object_path(&oid) {
            Ok(path) => q.add(&name, &path, &oid, size, false),
            Err(e) => q.add_error(e),
        }
    }
    meter.start();
    q.wait();
    meter.finish();
    logger.close();
    checkout.borrow_mut().close();
    let errs = q.take_errors();
    for e in &errs {
        full_error(e);
    }
    if !errs.is_empty() {
        let e = crate::endpoint::endpoint("download", &remote);
        exit(&format!("Failed to fetch some objects from '{}'", e.url));
    }
    if checkout.borrow().skip() {
        println!("Skipping object checkout, Git LFS is not installed for this repository.\nConsider installing it with 'git lfs install'.");
    }
}

fn checkout_cmd(p: &Parsed) {
    setup_repository();
    if cfg().local_working_dir().is_empty() {
        print("This operation must be run in a work tree.");
        return;
    }
    let mut seen = 0;
    let mut stage = 0;
    for (f, s) in [("base", 1), ("ours", 2), ("theirs", 3)] {
        if p.bool(f) {
            seen += 1;
            stage = s;
        }
    }
    if seen > 1 {
        exit("Error parsing args: at most one of --base, --theirs, and --ours is allowed");
    }
    let to = p.str("to");
    if !to.is_empty() && stage != 0 {
        if p.args.len() != 1 {
            exit("--to requires exactly one Git LFS object file path");
        }
        checkout_conflict(&p.args[0], stage, &to);
        return;
    } else if !to.is_empty() || stage != 0 {
        exit("--to and exactly one of --theirs, --ours, and --base must be used together");
    }
    let r = match gitcmd::current_ref() {
        Ok(r) => r,
        Err(e) => panic_exit(&e, "Could not checkout"),
    };
    let patterns: Vec<String> = p.args.iter().map(|a| current_to_repo_pattern(a)).collect();
    let mut co = SingleCheckout::new("");
    if co.skip() {
        println!("Cannot checkout LFS objects, Git LFS is not installed.");
        return;
    }
    let logger = Logger::new(Sink::Stdout, cfg().force_progress());
    let meter = Meter::new(Direction::Checkout, false);
    meter.logger_from_env();
    meter.enqueue(&logger);
    let mut total = 0i64;
    let mut pointers = vec![];
    let mut s = Scanner::new();
    s.filter = Some(Filter::new(&patterns, &[], PatternType::GitIgnore));
    let r = s.scan_lfs_files(&r.sha, &mut |res| match res {
        Ok(p) => {
            total += p.size();
            meter.add(p.size());
            meter.start_transfer(&p.name);
            pointers.push(p);
        }
        Err(e) => logged_error(&e, &format!("Scanner error: {e}")),
    });
    if let Err(e) = r {
        exit_with_error(&e);
    }
    meter.start();
    for p in &pointers {
        co.run(p);
        meter.transfer_bytes("checkout", &p.name, p.size(), total, p.size());
        meter.finish_transfer(&p.name);
    }
    meter.finish();
    logger.close();
    co.close();
}

fn checkout_conflict(file: &str, stage: u32, to: &str) {
    let to_abs = tools::abs(to).display().to_string();
    if let Some(d) = std::path::Path::new(&to_abs).parent() {
        if let Err(e) = tools::mkdir_all(d, cfg().repository_permissions(true)) {
            exit(&format!("Could not create path {}: {}", tools::quote(&to_abs), tools::io_err(&e)));
        }
    }
    let file = current_to_repo_path(file);
    let co = SingleCheckout::new("");
    if co.skip() {
        println!("Cannot checkout LFS objects, Git LFS is not installed.");
        return;
    }
    let r = match gitcmd::resolve_ref(&format!(":{stage}:{file}")) {
        Ok(r) => r,
        Err(e) => exit(&format!("Could not checkout (are you not in the middle of a merge?): {e}")),
    };
    let mut rd = match crate::gitscanner::ObjectReader::new() {
        Ok(r) => r,
        Err(e) => exit(&format!("Could not create object scanner: {e}")),
    };
    let data = match rd.read(&r.sha, usize::MAX) {
        Ok((o, _)) => o.data,
        Err(_) => exit(&format!("Could not find object {}", tools::quote(&r.sha))),
    };
    let ptr = match crate::pointer::decode(&data) {
        Ok(p) => p,
        Err(e) => exit(&format!("Could not find decoder pointer for object {}: {}", tools::quote(&r.sha), e)),
    };
    let wp = WrappedPointer { name: file, p: ptr, ..Default::default() };
    if let Err(e) = co.run_to_path(&wp, &to_abs) {
        exit(&format!("Error checking out {} to {}: {}", r.sha, tools::quote(&to_abs), e));
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd("pull", pull_cmd, vec![flag("include", Some('I'), K::Str), flag("exclude", Some('X'), K::Str)]),
        cmd("checkout", checkout_cmd, vec![flag("to", None, K::Str), flag("ours", None, K::Bool), flag("theirs", None, K::Bool), flag("base", None, K::Bool)]),
    ]
}
