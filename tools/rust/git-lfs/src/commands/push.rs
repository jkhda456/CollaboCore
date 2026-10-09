//! push and pre-push (commands/command_push.go, command_pre_push.go, uploader.go,
//! lockverifier.go): the LFS objects of the commits a push sends that the remote does not
//! have, uploaded after the remote's locks are checked.

use super::{cmd, error, exit, exit_with_code, exit_with_error, full_error, print, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Kind, Result};
use crate::gitcmd::{self, Ref, RefType};
use crate::gitscanner::{Scanner, WrappedPointer};
use crate::locking::{self, Lock};
use crate::tasklog::{Logger, Sink};
use crate::tools;
use crate::tq::{self, Direction, Manifest, Meter, TransferQueue};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::sync::Arc;

/// A ref being pushed and where it goes (git.RefUpdate).
pub struct RefUpdate {
    pub local: Ref,
    remote: Option<Ref>,
    push_remote: String,
}

impl RefUpdate {
    pub fn new(push_remote: &str, local: Ref, remote: Option<Ref>) -> RefUpdate {
        RefUpdate { local, remote, push_remote: push_remote.to_string() }
    }
    pub fn remote_ref(&mut self) -> Ref {
        if self.remote.is_none() {
            self.remote = Some(gitcmd::default_remote_ref(&self.push_remote, &self.local));
        }
        self.remote.clone().unwrap()
    }
    pub fn local_commitish(&self) -> String {
        gitcmd::ref_commitish(&self.local)
    }
}

#[derive(PartialEq, Clone, Copy)]
enum VerifyState {
    Unknown,
    Enabled,
    Disabled,
}

struct RefLock {
    path: String,
    /// (ref name, refspec, lock)
    refs: Vec<(String, String, Lock)>,
}

pub struct LockVerifier {
    endpoint_url: String,
    state: VerifyState,
    verified_refs: Vec<String>,
    ours: BTreeMap<String, RefLock>,
    theirs: BTreeMap<String, RefLock>,
    owned: Vec<String>,
    unowned: Vec<String>,
}

fn supports_locking_api(raw: &str) -> bool {
    match crate::gourl::parse(raw) {
        Ok(u) => (u.scheme == "https" || u.scheme == "ssh") && u.hostname() == "github.com",
        Err(e) => {
            crate::trace!("commands: unable to parse {} to determine locking support: {}", tools::quote(raw), e);
            false
        }
    }
}

fn verify_state_for(raw: &str) -> VerifyState {
    match crate::config::url_get("lfs", raw, "locksverify") {
        None => {
            if supports_locking_api(raw) {
                VerifyState::Enabled
            } else {
                VerifyState::Unknown
            }
        }
        Some(v) => {
            if matches!(v.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True") {
                VerifyState::Enabled
            } else {
                VerifyState::Disabled
            }
        }
    }
}

impl LockVerifier {
    pub fn new(manifest: &Manifest) -> LockVerifier {
        let ep = crate::endpoint::endpoint("upload", &cfg().push_remote());
        let state = if !manifest.upgrade().standalone_agent.is_empty() { VerifyState::Disabled } else { verify_state_for(&ep.url) };
        LockVerifier { endpoint_url: ep.url, state, verified_refs: vec![], ours: BTreeMap::new(), theirs: BTreeMap::new(), owned: vec![], unowned: vec![] }
    }

    pub fn verify(&mut self, r: &Ref) {
        if self.state == VerifyState::Disabled || self.verified_refs.contains(&r.refspec()) {
            return;
        }
        crate::trace!("verifying locks for {}", r.name);
        let mut client = locking::Client::new(&cfg().push_remote());
        client.remote_ref = Some(r.clone());
        let (ours, theirs) = match client.search_locks_verifiable(0, false) {
            Ok(x) => {
                if self.state == VerifyState::Unknown {
                    error(&format!("Locking support detected on remote {}. Consider enabling it with:", tools::quote(&cfg().push_remote())));
                    error(&format!("  $ git config lfs.{}.locksverify true", self.endpoint_url));
                }
                x
            }
            Err(e) => {
                if e.is(Kind::NotImplemented) {
                    crate::trace!("commands: disabling lock verification for {}", tools::quote(&self.endpoint_url));
                    let _ = cfg().set_local(&format!("lfs.{}.locksverify", self.endpoint_url), "false");
                } else if e.is(Kind::Auth) {
                    if self.state == VerifyState::Unknown {
                        error(&format!("warning: Authentication error: {e}"));
                    } else {
                        exit(&format!("error: Authentication error: {e}"));
                    }
                } else {
                    error(&format!("Remote {} does not support the Git LFS locking API. Consider disabling it with:", tools::quote(&cfg().push_remote())));
                    error(&format!("  $ git config lfs.{}.locksverify false", self.endpoint_url));
                    if self.state == VerifyState::Enabled {
                        exit_with_error(&e);
                    }
                }
                (vec![], vec![])
            }
        };
        Self::add_locks(r, ours, &mut self.ours);
        Self::add_locks(r, theirs, &mut self.theirs);
        self.verified_refs.push(r.refspec());
        crate::trace!("verified locks for {}", r.name);
    }

    fn add_locks(r: &Ref, locks: Vec<Lock>, set: &mut BTreeMap<String, RefLock>) {
        for l in locks {
            let e = set.entry(l.path.clone()).or_insert_with(|| RefLock { path: l.path.clone(), refs: vec![] });
            e.refs.retain(|(n, _, _)| *n != r.name);
            e.refs.push((r.name.clone(), r.refspec(), l));
        }
    }

    pub fn locked_by_them(&mut self, name: &str) -> bool {
        if self.theirs.contains_key(name) {
            self.unowned.push(name.to_string());
            return true;
        }
        false
    }
    pub fn locked_by_us(&mut self, name: &str) -> bool {
        if self.ours.contains_key(name) {
            self.owned.push(name.to_string());
            return true;
        }
        false
    }
    pub fn enabled(&self) -> bool {
        self.state == VerifyState::Enabled
    }

    fn owners(&self, l: &RefLock) -> String {
        let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, _, lock) in &l.refs {
            let u = lock.owner.as_ref().map(|o| o.name.clone()).unwrap_or_default();
            users.entry(u).or_default().push(name.clone());
        }
        let mut owners = vec![];
        for (name, mut refs) in users {
            // As git-lfs compares them: ref names against the verified refspecs.
            let seen = refs.iter().filter(|r| self.verified_refs.contains(r)).count();
            if seen == self.verified_refs.len() {
                owners.push(name);
                continue;
            }
            refs.sort();
            owners.push(format!("{} (refs: {})", name, refs.join(", ")));
        }
        owners.sort();
        owners.join(", ")
    }
}

pub struct UploadContext {
    pub remote: String,
    pub dry_run: bool,
    pub manifest: Arc<Manifest>,
    uploaded: HashSet<String>,
    logger: Arc<Logger>,
    meter: Arc<Meter>,
    pub lock_verifier: LockVerifier,
    allow_missing: bool,
    scanner_err: Option<Error>,
    missing: BTreeMap<String, String>,
    corrupt: BTreeMap<String, String>,
    other_errs: Vec<Error>,
}

impl UploadContext {
    pub fn new(dry_run: bool) -> UploadContext {
        let remote = cfg().push_remote();
        let manifest = Manifest::get("upload", &remote);
        let lock_verifier = LockVerifier::new(&manifest);
        let logger = Logger::new(if dry_run { Sink::Discard } else { Sink::Stdout }, cfg().force_progress());
        let meter = Meter::build(dry_run, Direction::Upload);
        meter.enqueue(&logger);
        UploadContext {
            remote,
            dry_run,
            manifest,
            uploaded: HashSet::new(),
            logger,
            meter,
            lock_verifier,
            allow_missing: cfg().git().bool("lfs.allowincompletepush", false),
            scanner_err: None,
            missing: BTreeMap::new(),
            corrupt: BTreeMap::new(),
            other_errs: vec![],
        }
    }

    pub fn new_queue(&self, remote_ref: Option<Ref>) -> TransferQueue {
        TransferQueue::new(
            Direction::Upload,
            self.manifest.clone(),
            &self.remote,
            tq::Options { dry_run: self.dry_run, meter: Some(self.meter.clone()), remote_ref, batch_size: cfg().transfer_batch_size(), cb: None },
        )
    }

    fn prepare(&mut self, unfiltered: Vec<WrappedPointer>) -> Vec<WrappedPointer> {
        let mut uniq = HashSet::new();
        let mut out = vec![];
        for p in unfiltered {
            if uniq.contains(p.oid()) || self.uploaded.contains(p.oid()) || p.size() == 0 {
                continue;
            }
            uniq.insert(p.oid().to_string());
            let mut can = true;
            if self.lock_verifier.locked_by_them(&p.name) {
                can = !self.lock_verifier.enabled();
            }
            self.lock_verifier.locked_by_us(&p.name);
            if can {
                self.meter.add(p.size());
                out.push(p);
            }
        }
        out
    }

    pub fn upload_pointers(&mut self, q: &mut TransferQueue, unfiltered: Vec<WrappedPointer>) {
        if self.dry_run {
            for p in unfiltered {
                if self.uploaded.contains(p.oid()) {
                    continue;
                }
                print(&format!("push {} => {}", p.oid(), p.name));
                self.uploaded.insert(p.oid().to_string());
            }
            return;
        }
        for p in self.prepare(unfiltered) {
            let (path, missing) = match self.upload_transfer(&p) {
                Ok(x) => x,
                Err(e) => exit_with_error(&e),
            };
            q.add(&p.name, &path, p.oid(), p.size(), missing);
            self.uploaded.insert(p.oid().to_string());
        }
    }

    fn upload_transfer(&self, p: &WrappedPointer) -> Result<(String, bool)> {
        let fs = cfg().filesystem();
        let wrap = |e: Error| e.wrap(format!("Error uploading file {} ({})", p.name, p.oid()));
        let path = fs.object_path(p.oid()).map_err(wrap)?;
        let mut missing = false;
        if let Err(e) = std::fs::metadata(&path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing = !self.allow_missing;
            } else {
                return Err(wrap(tools::path_err("stat", &path, &e)));
            }
        }
        Ok((path, missing))
    }

    pub fn collect_errors(&mut self, q: &mut TransferQueue) {
        q.wait();
        for e in q.take_errors() {
            let msg = e.to_string();
            let re = regex::Regex::new(r"^(missing|corrupt) object: (.*) \(([0-9a-f]+)\)$").unwrap();
            if let Some(m) = re.captures(&msg) {
                if &m[1] == "missing" {
                    self.missing.insert(m[2].to_string(), m[3].to_string());
                } else {
                    self.corrupt.insert(m[2].to_string(), m[3].to_string());
                }
            } else {
                self.other_errs.push(e);
            }
        }
    }

    pub fn report_errors(&mut self) {
        self.meter.finish();
        self.logger.close();
        for e in &self.other_errs {
            full_error(e);
        }
        if !self.missing.is_empty() || !self.corrupt.is_empty() {
            let action = if self.allow_missing { "missing objects" } else { "failed" };
            print(&format!("Git LFS upload {action}:"));
            for (name, oid) in &self.missing {
                print(&format!("  (missing) {name} ({oid})"));
            }
            for (name, oid) in &self.corrupt {
                print(&format!("  (corrupt) {name} ({oid})"));
            }
            if !self.allow_missing {
                print("hint: Your push was rejected due to missing or corrupt local objects.\nhint: You can disable this check with: `git config lfs.allowincompletepush true`");
                exit_with_code(2);
            }
        }
        if !self.other_errs.is_empty() {
            exit_with_code(2);
        }
        let lv = &self.lock_verifier;
        if !lv.unowned.is_empty() {
            print("Unable to push locked files:");
            for p in &lv.unowned {
                let l = &lv.theirs[p];
                print(&format!("* {} - {}", l.path, lv.owners(l)));
            }
            if lv.enabled() {
                exit("Cannot update locked files.");
            } else {
                error("warning: The above files would have halted this push.");
            }
        } else if !lv.owned.is_empty() {
            print("Consider unlocking your own locked files: (`git lfs unlock <path>`)");
            for p in &lv.owned {
                print(&format!("* {}", lv.ours[p].path));
            }
        }
    }
}

/// uploadForRefUpdates.
pub fn upload_for_ref_updates(ctx: &mut UploadContext, updates: &mut [RefUpdate], push_all: bool) -> Result<()> {
    let r = upload_refs(ctx, updates, push_all);
    ctx.report_errors();
    r
}

fn upload_refs(ctx: &mut UploadContext, updates: &mut [RefUpdate], push_all: bool) -> Result<()> {
    for u in updates.iter_mut() {
        let r = u.remote_ref();
        ctx.lock_verifier.verify(&r);
    }
    let mut exclude = vec![];
    for u in updates.iter_mut() {
        let sha = u.remote_ref().sha;
        if u.local_commitish() != sha {
            exclude.push(sha);
        }
    }
    for u in updates.iter_mut() {
        let mut q = ctx.new_queue(Some(u.remote_ref()));
        let r = upload_range_or_all(ctx, &mut q, &exclude, u, push_all);
        ctx.collect_errors(&mut q);
        if let Err(e) = r {
            return Err(e.wrap(format!("ref {}:", tools::quote(&u.local.name))));
        }
    }
    Ok(())
}

fn upload_range_or_all(ctx: &mut UploadContext, q: &mut TransferQueue, exclude: &[String], u: &mut RefUpdate, push_all: bool) -> Result<()> {
    let mut found: Vec<Result<WrappedPointer>> = vec![];
    let mut lockables: Vec<String> = vec![];
    {
        let theirs: HashSet<String> = ctx.lock_verifier.theirs.keys().cloned().collect();
        let contains = move |n: &str| theirs.contains(n);
        let mut lock_cb = |n: &str| lockables.push(n.to_string());
        let mut s = Scanner::for_push(&ctx.remote);
        s.potential_lockables = Some(&contains);
        s.found_lockable = Some(&mut lock_cb);
        let mut cb = |r: Result<WrappedPointer>| found.push(r);
        if push_all {
            s.scan_ref_with_deleted(&u.local_commitish(), &mut cb)?;
        } else {
            s.scan_multi_range_to_remote(&u.local_commitish(), exclude, &mut cb)?;
        }
    }
    for n in lockables {
        ctx.lock_verifier.locked_by_them(&n);
    }
    let mut ptrs = vec![];
    for r in found {
        match r {
            Ok(p) => ptrs.push(p),
            Err(e) => {
                ctx.scanner_err = Some(match ctx.scanner_err.take() {
                    None => e,
                    Some(prev) => Error::new(format!("{prev}\n{e}")),
                })
            }
        }
    }
    // One at a time, as the scanner's callback does, so that batches fill as in git-lfs.
    for p in ptrs {
        ctx.upload_pointers(q, vec![p]);
    }
    match ctx.scanner_err.clone() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn pre_push(p: &Parsed) {
    if p.args.is_empty() {
        print("This should be run through Git's pre-push hook.  Run `git lfs update` to install it.");
        exit_with_code(1);
    }
    if cfg().os.bool("GIT_LFS_SKIP_PUSH", false) {
        return;
    }
    super::require_git_version();
    let (remote, _) = gitcmd::map_remote_url(&p.args[0], true);
    if let Err(e) = cfg().set_valid_push_remote(&remote) {
        exit(&format!("Invalid remote name {}: {}", tools::quote(&p.args[0]), e));
    }
    let mut ctx = UploadContext::new(p.bool("dry-run"));
    let mut updates = vec![];
    let stdin = std::io::stdin();
    for line in stdin.lock().lines().map_while(|l| l.ok()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        crate::trace!("pre-push: {}", line);
        let mut refs: Vec<&str> = line.split(' ').collect();
        while refs.len() < 4 {
            refs.push("");
        }
        let local = gitcmd::parse_ref(refs[0], refs[1]);
        let remote_ref = gitcmd::parse_ref(refs[2], refs[3]);
        if gitcmd::is_zero_object_id(&local.sha) {
            continue;
        }
        updates.push(RefUpdate::new(&cfg().push_remote(), local, Some(remote_ref)));
    }
    if let Err(e) = upload_for_ref_updates(&mut ctx, &mut updates, false) {
        exit_with_error(&e);
    }
}

fn push(p: &Parsed) {
    if p.args.is_empty() {
        exit("Specify a remote and a remote branch name (`git lfs push origin main`)");
    }
    super::require_git_version();
    if let Err(e) = cfg().set_valid_push_remote(&p.args[0]) {
        exit(&format!("Invalid remote name {}: {}", tools::quote(&p.args[0]), e));
    }
    let mut ctx = UploadContext::new(p.bool("dry-run"));
    let use_stdin = p.bool("stdin");
    let push_all = p.bool("all");
    let list: Vec<String> = if use_stdin {
        if p.args.len() > 1 {
            exit("Further command line arguments are ignored with --stdin");
        }
        std::io::stdin().lock().lines().map_while(|l| l.ok()).filter(|l| !l.is_empty()).collect()
    } else {
        p.args[1..].to_vec()
    };
    if p.bool("object-id") {
        if !use_stdin && list.is_empty() {
            print("At least one object ID must be supplied with --object-id");
            exit_with_code(1);
        }
        let fs = cfg().filesystem();
        let mut ptrs = vec![];
        for oid in &list {
            let mp = match fs.object_path(oid) {
                Ok(m) => m,
                Err(e) => exit_with_error(&e.wrap("Unable to find local media path:")),
            };
            let size = match std::fs::metadata(&mp) {
                Ok(m) => m.len() as i64,
                Err(e) => exit_with_error(&tools::path_err("stat", &mp, &e).wrap("Unable to stat local media path")),
            };
            ptrs.push(WrappedPointer { name: mp, p: crate::pointer::Pointer::new(oid, size, vec![]), ..Default::default() });
        }
        let rr = gitcmd::default_remote_ref(&cfg().push_remote(), &cfg().current_ref());
        let mut q = ctx.new_queue(Some(rr));
        ctx.upload_pointers(&mut q, ptrs);
        ctx.collect_errors(&mut q);
        ctx.report_errors();
        return;
    }
    if !use_stdin && !push_all && list.is_empty() {
        print("At least one ref must be supplied without --all");
        exit_with_code(1);
    }
    crate::trace!("Upload refs [{}] to remote {}", list.join(" "), ctx.remote);
    let mut updates = match push_refs(&list, push_all) {
        Ok(u) => u,
        Err(e) => {
            error(&e.to_string());
            exit("Error getting local refs.");
        }
    };
    if let Err(e) = upload_for_ref_updates(&mut ctx, &mut updates, push_all) {
        exit_with_error(&e);
    }
}

fn push_refs(names: &[String], all: bool) -> Result<Vec<RefUpdate>> {
    let local = gitcmd::local_refs()?;
    let pr = cfg().push_remote();
    if all && names.is_empty() {
        return Ok(local.into_iter().map(|r| RefUpdate::new(&pr, r, None)).collect());
    }
    let lookup: HashMap<String, Ref> = local.into_iter().map(|r| (r.name.clone(), r)).collect();
    let mut out = vec![];
    for n in names {
        match lookup.get(n) {
            Some(r) => out.push(RefUpdate::new(&pr, r.clone(), None)),
            None => {
                if gitcmd::resolve_ref(n).is_err() {
                    return Err(Error::new(format!("Invalid ref argument: {n}")));
                }
                out.push(RefUpdate::new(&pr, Ref { name: n.clone(), typ: RefType::Other, sha: n.clone() }, None));
            }
        }
    }
    Ok(out)
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd("pre-push", pre_push, vec![flag("dry-run", Some('d'), K::Bool)]),
        cmd(
            "push",
            push,
            vec![flag("dry-run", Some('d'), K::Bool), flag("object-id", Some('o'), K::Bool), flag("stdin", None, K::Bool), flag("all", Some('a'), K::Bool)],
        ),
    ]
}
