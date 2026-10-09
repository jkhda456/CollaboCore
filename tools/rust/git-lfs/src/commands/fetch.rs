//! fetch (commands/command_fetch.go): the objects of refs (their trees), of recent refs and
//! commits, or of all history, downloaded into the local store.

use super::pull::{build_filter, new_download_queue};
use super::{cmd, error, exit, exit_with_error, full_error, panic_exit, print, setup_repository, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::filter::Filter;
use crate::gitcmd::{self, Ref, RefType};
use crate::gitscanner::{Scanner, WrappedPointer};
use crate::tasklog::{Logger, Sink};
use crate::tools;
use crate::tq::{self, Direction, Manifest, Meter};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::BufRead;
use std::rc::Rc;

#[derive(Default)]
struct Watcher {
    json: bool,
    dry_run: bool,
    refetch: bool,
    transfers: Vec<tq::Transfer>,
    observed: HashSet<String>,
}

impl Watcher {
    fn register(&mut self, t: &tq::Transfer) {
        if self.json {
            self.transfers.push(t.clone());
        }
        if self.dry_run || self.refetch {
            self.observed.insert(t.oid.clone());
        }
        if self.dry_run {
            error(&format!("fetch {} => {}", t.oid, t.name));
        }
    }
}

struct Ctx {
    watcher: Rc<RefCell<Watcher>>,
    dry_run: bool,
    refetch: bool,
}

/// The Go JSON of a transfer in `fetch --json` (tq.Transfer, indented by one space).
fn dump_json(w: &Watcher) {
    let list: Vec<serde_json::Value> = w
        .transfers
        .iter()
        .map(|t| {
            let mut m = serde_json::Map::new();
            if !t.name.is_empty() {
                m.insert("name".into(), t.name.clone().into());
            }
            if !t.oid.is_empty() {
                m.insert("oid".into(), t.oid.clone().into());
            }
            m.insert("size".into(), t.size.into());
            if !t.actions.is_empty() {
                let mut a = serde_json::Map::new();
                for (k, v) in &t.actions {
                    a.insert(k.clone(), v.to_go_json());
                }
                m.insert("actions".into(), serde_json::Value::Object(a));
            }
            if let Some(l) = &t.links {
                if !l.is_empty() {
                    let mut a = serde_json::Map::new();
                    for (k, v) in l {
                        a.insert(k.clone(), v.to_go_json());
                    }
                    m.insert("_links".into(), serde_json::Value::Object(a));
                }
            }
            if !t.path.is_empty() {
                m.insert("path".into(), t.path.clone().into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let v = serde_json::json!({ "transfers": list });
    print(&go_indent(&v, " "));
}

/// json.Encoder with SetIndent("", indent).
pub fn go_indent(v: &serde_json::Value, indent: &str) -> String {
    let mut buf = vec![];
    let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    use serde::Serialize;
    let _ = v.serialize(&mut ser);
    let s = String::from_utf8(buf).unwrap_or_default();
    // Go escapes <, > and & in strings.
    s.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

impl Ctx {
    fn fetch(&self, all: Vec<WrappedPointer>) -> bool {
        let w = self.watcher.borrow();
        let print_transfers = w.json || w.dry_run;
        drop(w);
        let logger = Logger::new(Sink::Stdout, cfg().force_progress());
        let meter = Meter::build(print_transfers, Direction::Download);
        meter.enqueue(&logger);
        let fs = cfg().filesystem();
        let mut todo = vec![];
        for p in all {
            if self.watcher.borrow().observed.contains(p.oid()) || p.size() == 0 {
                continue;
            }
            crate::gitfilter::link_or_copy_from_reference(p.oid(), p.size());
            if !self.refetch && fs.object_exists(p.oid(), p.size()) {
                continue;
            }
            meter.add(p.size());
            todo.push(p);
        }
        let remote = cfg().remote();
        let mut q = new_download_queue(Manifest::get("download", &remote), &remote, Some(meter.clone()), self.dry_run);
        let w = self.watcher.clone();
        q.watch(Box::new(move |t: &tq::Transfer| w.borrow_mut().register(t)));
        for p in &todo {
            crate::trace!("fetch {} [{}]", p.name, p.oid());
            match fs.object_path(p.oid()) {
                Ok(path) => q.add(&p.name, &path, p.oid(), p.size(), false),
                Err(e) => q.add_error(e),
            }
        }
        q.wait();
        let mut ok = true;
        for e in q.take_errors() {
            ok = false;
            full_error(&e);
        }
        // git-lfs never ends this meter's task (no ", done." line).
        std::mem::forget(meter);
        drop(logger);
        ok
    }

    fn fetch_ref(&self, sha: &str, filter: &Filter) -> bool {
        let mut ptrs = vec![];
        let mut errs: Vec<Error> = vec![];
        let mut s = Scanner::new();
        s.filter = Some(clone_filter(filter));
        let r = s.scan_tree(sha, &mut |res| match res {
            Ok(p) => ptrs.push(p),
            Err(e) => errs.push(e),
        });
        if let Err(e) = r {
            panic_exit(&e, "Could not scan for Git LFS files");
        }
        if !errs.is_empty() {
            panic_exit(&tq::join_errors(&errs), "Could not scan for Git LFS files");
        }
        self.fetch(ptrs)
    }

    fn scan_with_count(&self, f: impl FnOnce(&mut Scanner, &mut dyn FnMut(Result<WrappedPointer>)) -> Result<()>) -> Vec<WrappedPointer> {
        let logger = Logger::new(Sink::Stdout, cfg().force_progress());
        let mut task = logger.simple();
        let mut ptrs = vec![];
        let mut errs: Vec<Error> = vec![];
        let mut n = 0;
        let mut s = Scanner::new();
        let r = f(&mut s, &mut |res| match res {
            Ok(p) => {
                n += 1;
                task.log(&format!("{} object{} found", n, if n == 1 { "" } else { "s" }));
                ptrs.push(p);
            }
            Err(e) => errs.push(e),
        });
        task.complete();
        logger.close();
        if let Err(e) = r {
            panic_exit(&e, "Could not scan for Git LFS files");
        }
        if !errs.is_empty() {
            panic_exit(&tq::join_errors(&errs), "Could not scan for Git LFS files");
        }
        ptrs
    }

    fn fetch_refs(&self, shas: &[String]) -> bool {
        let shas = shas.to_vec();
        let ptrs = self.scan_with_count(|s, cb| s.scan_refs_multi(&shas, &[], cb));
        self.fetch(ptrs)
    }

    fn fetch_all(&self) -> bool {
        let ptrs = self.scan_with_count(|s, cb| s.scan_all(cb));
        error("Fetching all references...");
        self.fetch(ptrs)
    }

    fn fetch_previous(&self, sha: &str, since_secs: i64, filter: &Filter) -> bool {
        let mut ptrs = vec![];
        let mut s = Scanner::new();
        s.filter = Some(clone_filter(filter));
        let since = crate::lfs::time_format(since_secs, "%Y-%m-%d %H:%M:%S %z", false);
        let r = s.scan_previous_versions(sha, &since, &mut |res| match res {
            Ok(p) => ptrs.push(p),
            Err(e) => panic_exit(&e, "Could not scan for Git LFS previous versions"),
        });
        if let Err(e) = r {
            exit_with_error(&e);
        }
        self.fetch(ptrs)
    }

    fn fetch_recent(&self, fp: &crate::lfs::FetchPruneConfig, already: &[Ref], filter: &Filter) -> bool {
        if fp.fetch_recent_refs_days == 0 && fp.fetch_recent_commits_days == 0 {
            return true;
        }
        let mut ok = true;
        let mut uniq: Vec<(String, String)> = vec![];
        for r in already {
            if !uniq.iter().any(|(s, _)| *s == r.sha) {
                uniq.push((r.sha.clone(), r.name.clone()));
            }
        }
        let now = tools::unix_secs(std::time::SystemTime::now());
        if fp.fetch_recent_refs_days > 0 {
            let d = fp.fetch_recent_refs_days;
            error(&format!("Fetching recent branches within {} day{}", d, if d == 1 { "" } else { "s" }));
            let since = now - d * 86400;
            let refs = match recent_branches(since, fp.fetch_recent_refs_include_remotes, &cfg().remote()) {
                Ok(r) => r,
                Err(e) => panic_exit(&e, "Could not scan for recent refs"),
            };
            for r in refs {
                if let Some((_, prev)) = uniq.iter().find(|(s, _)| *s == r.sha) {
                    if r.name != *prev {
                        crate::trace!("Skipping fetch for {}, already fetched via {}", r.name, prev);
                    }
                } else {
                    uniq.push((r.sha.clone(), r.name.clone()));
                    error(&format!("Fetching reference {}", r.name));
                    ok = self.fetch_ref(&r.sha, filter) && ok;
                }
            }
        }
        if fp.fetch_recent_commits_days > 0 {
            for (commit, name) in &uniq {
                let date = match commit_date(commit) {
                    Ok(d) => d,
                    Err(e) => {
                        error(&format!("Couldn't scan commits at {name}: {e}"));
                        continue;
                    }
                };
                let d = fp.fetch_recent_commits_days;
                error(&format!("Fetching changes within {} day{} of {}", d, if d == 1 { "" } else { "s" }, name));
                ok = self.fetch_previous(commit, date - d * 86400, filter) && ok;
            }
        }
        ok
    }
}

fn clone_filter(f: &Filter) -> Filter {
    let mut n = Filter::new(&f.include_strs(), &f.exclude_strs(), crate::filter::PatternType::GitIgnore);
    n.default_value = f.default_value;
    n
}

/// git.ParseGitDate: "2006-01-02 15:04:05 -0700" as seconds since the epoch.
pub fn parse_git_date(s: &str) -> Option<i64> {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2}):(\d{2})\s+([+-])(\d{2})(\d{2})").unwrap();
    let m = re.captures(s.trim())?;
    let n = |i: usize| m[i].parse::<i64>().unwrap();
    let secs = tools::days_from_civil(n(1), n(2), n(3)) * 86400 + n(4) * 3600 + n(5) * 60 + n(6);
    let off = n(8) * 3600 + n(9) * 60;
    Some(if &m[7] == "+" { secs - off } else { secs + off })
}

/// git.RecentBranches: refs with commits since then, newest first.
pub fn recent_branches(since: i64, include_remotes: bool, only_remote: &str) -> Result<Vec<Ref>> {
    let out = gitcmd::git_no_lfs_command(&["for-each-ref", "--sort=-committerdate", "--format=%(refname) %(objectname) %(committerdate:iso)", "refs"]).output().map_err(Error::from)?;
    let re = regex::Regex::new(r"^(refs/[^/]+/\S+)\s+([0-9a-f]{40}|[0-9a-f]{64})\s+(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\s+[\+\-]\d{4})").unwrap();
    crate::trace!("RECENT: Getting refs >= {}", crate::lfs::time_format(since, "%Y-%m-%d %H:%M:%S %z", false));
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(m) = re.captures(line) else { continue };
        let (typ, name) = gitcmd::parse_ref_to_type_and_name(&m[1]);
        if typ == RefType::RemoteBranch {
            if !include_remotes {
                continue;
            }
            if !only_remote.is_empty() && !name.starts_with(&format!("{only_remote}/")) {
                continue;
            }
        }
        let Some(d) = parse_git_date(&m[3]) else { return Err(Error::new(format!("invalid date {}", &m[3]))) };
        if d < since {
            break;
        }
        crate::trace!("RECENT: {} ({})", name, &m[3]);
        v.push(Ref { name, typ, sha: m[2].to_string() });
    }
    Ok(v)
}

/// The committer date of a commit (GetCommitSummary's CommitDate).
pub fn commit_date(commit: &str) -> Result<i64> {
    let out = gitcmd::git_no_lfs_command(&["show", "-s", "--format=%H|%h|%P|%ai|%ci|%ae|%an|%ce|%cn|%s", commit]).output().map_err(Error::from)?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(Error::new(format!("failed to call `git show`: {} {}", crate::subprocess::exit_text(&out.status), text)));
    }
    let f: Vec<&str> = text.splitn(10, '|').collect();
    if f.len() < 9 {
        return Err(Error::new(format!("Unexpected output from `git show`: {text}")));
    }
    Ok(parse_git_date(f[4]).unwrap_or(0))
}

fn fetch_cmd(p: &Parsed) {
    setup_repository();
    let mut refs: Vec<Ref> = vec![];
    if let Some(r) = p.args.first() {
        if let Err(e) = cfg().set_valid_remote(r) {
            exit(&format!("Invalid remote name {}: {}", tools::quote(r), e));
        }
    }
    let (all, recent, prune, json, dry_run, refetch) = (p.bool("all"), p.bool("recent"), p.bool("prune"), p.bool("json"), p.bool("dry-run"), p.bool("refetch"));
    if p.bool("stdin") {
        if p.args.len() > 1 {
            exit("Further command line arguments are ignored with --stdin");
        }
        for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
            if line.is_empty() {
                continue;
            }
            match gitcmd::resolve_ref(&line) {
                Ok(r) => refs.push(r),
                Err(e) => panic_exit(&e, &format!("Invalid ref argument: {line}")),
            }
        }
    } else if p.args.len() > 1 {
        for a in &p.args[1..] {
            match gitcmd::resolve_ref(a) {
                Ok(r) => refs.push(r),
                Err(e) => panic_exit(&e, &format!("Invalid ref argument: [{}]", p.args[1..].join(" "))),
            }
        }
    } else if !all {
        match gitcmd::current_ref() {
            Ok(r) => refs.push(r),
            Err(e) => panic_exit(&e, "Could not fetch"),
        }
    }
    if json && prune {
        exit("Cannot combine --json with --prune");
    }
    let fp = crate::lfs::fetch_prune_config();
    let ctx = Ctx { watcher: Rc::new(RefCell::new(Watcher { json, dry_run, refetch, ..Default::default() })), dry_run, refetch };
    let mut ok = true;
    if all {
        if recent {
            exit("Cannot combine --all with --recent");
        }
        if p.changed("include") || p.changed("exclude") {
            exit("Cannot combine --all with --include or --exclude");
        }
        if !cfg().fetch_include_paths().is_empty() || !cfg().fetch_exclude_paths().is_empty() {
            error("Ignoring global include / exclude paths to fulfil --all");
        }
        if !refs.is_empty() {
            let shas: Vec<String> = refs.iter().map(|r| r.sha.clone()).collect();
            ok = ctx.fetch_refs(&shas);
        } else {
            ok = ctx.fetch_all();
        }
    } else {
        let filter = build_filter(p, true);
        for r in &refs {
            error(&format!("Fetching reference {}", r.refspec()));
            ok = ctx.fetch_ref(&r.sha, &filter) && ok;
        }
        if recent || fp.fetch_recent_always {
            ok = ctx.fetch_recent(&fp, &refs, &filter) && ok;
        }
    }
    if prune {
        super::prune::prune(&fp, fp.prune_verify_remote_always, fp.prune_verify_unreachable_always, false, dry_run, dry_run);
    }
    if !ok {
        let e = crate::endpoint::endpoint("download", &cfg().remote());
        exit(&format!("error: failed to fetch some objects from '{}'", e.url));
    }
    if json {
        dump_json(&ctx.watcher.borrow());
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd(
        "fetch",
        fetch_cmd,
        vec![
            flag("include", Some('I'), K::Str),
            flag("exclude", Some('X'), K::Str),
            flag("recent", Some('r'), K::Bool),
            flag("all", Some('a'), K::Bool),
            flag("prune", Some('p'), K::Bool),
            flag("refetch", None, K::Bool),
            flag("dry-run", Some('d'), K::Bool),
            flag("json", Some('j'), K::Bool),
            flag("stdin", None, K::Bool),
        ],
    )]
}

/// fetchRef without a watcher (what `git lfs clone --no-checkout` does).
pub fn fetch_ref_plain(sha: &str, filter: &Filter) -> bool {
    let ctx = Ctx { watcher: Rc::new(RefCell::new(Watcher::default())), dry_run: false, refetch: false };
    ctx.fetch_ref(sha, filter)
}
