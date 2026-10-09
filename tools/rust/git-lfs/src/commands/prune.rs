//! prune (commands/command_prune.go): local objects no longer referenced by the current and
//! recent refs and commits, unpushed commits, worktrees, their indexes and stashes are
//! deleted (after asking the remote whether it has them, with --verify-remote).

use super::{cmd, exit, logged_error, panic_exit, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::filter::{Filter, PatternType};
use crate::gitcmd::{self, Ref};
use crate::gitscanner::Scanner;
use crate::lfs::FetchPruneConfig;
use crate::tasklog::{Logger, Sink};
use crate::tools;
use crate::tq::{self, Direction, Manifest, TransferQueue};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

pub struct Worktree {
    pub sha: String,
    pub dir: String,
    pub prunable: bool,
}

/// git.GetAllWorktrees (`git worktree list --porcelain -z`).
pub fn all_worktrees() -> Result<Vec<Worktree>> {
    let out = gitcmd::git_no_lfs_command(&["worktree", "list", "--porcelain", "-z"]).output().map_err(Error::from)?;
    if !out.status.success() {
        return Err(Error::new(format!("error in `git worktree`: {}: {}", crate::subprocess::exit_text(&out.status), String::from_utf8_lossy(&out.stderr))));
    }
    let mut v = vec![];
    let (mut dir, mut sha, mut prunable, mut have) = (String::new(), String::new(), false, false);
    for rec in String::from_utf8_lossy(&out.stdout).split('\0') {
        if rec.is_empty() {
            if !dir.is_empty() && have && !sha.is_empty() {
                v.push(Worktree { sha: sha.clone(), dir: dir.clone(), prunable });
            }
            dir.clear();
            have = false;
            sha.clear();
            continue;
        }
        let (k, val) = rec.split_once(' ').map(|(a, b)| (a, Some(b))).unwrap_or((rec, None));
        match (k, val) {
            ("worktree", Some(d)) if dir.is_empty() => {
                dir = tools::clean_str(d);
                have = true;
                prunable = false;
            }
            ("HEAD", Some(s)) if have => sha = s.to_string(),
            ("bare", _) => {
                dir.clear();
                have = false;
            }
            ("prunable", _) => prunable = true,
            _ => {}
        }
    }
    Ok(v)
}

#[derive(Default)]
struct Progress {
    local: usize,
    retained: usize,
    verified: usize,
    not_remote: usize,
}

impl Progress {
    fn msg(&self) -> String {
        let mut m = format!("{} local object{}, {} retained", self.local, if self.local == 1 { "" } else { "s" }, self.retained);
        if self.verified > 0 {
            m.push_str(&format!(", {} verified with remote", self.verified));
        }
        if self.not_remote > 0 {
            m.push_str(&format!(", {} not on remote", self.not_remote));
        }
        m
    }
}

pub fn prune(fp: &FetchPruneConfig, verify_remote: bool, verify_unreachable: bool, continue_unverified: bool, dry_run: bool, verbose: bool) {
    prune_with(fp, verify_remote, verify_unreachable, continue_unverified, dry_run, verbose, false, false)
}

#[allow(clippy::too_many_arguments)]
fn prune_with(fp: &FetchPruneConfig, verify_remote: bool, verify_unreachable: bool, continue_unverified: bool, dry_run: bool, verbose: bool, prune_recent: bool, prune_force: bool) {
    let logger = Logger::new(Sink::Stdout, cfg().force_progress());
    let mut task = logger.simple();
    let mut prog = Progress::default();
    let local = cfg().filesystem().each_object();
    for _ in &local {
        prog.local += 1;
        task.log(&prog.msg());
    }
    let mut retained: HashSet<String> = HashSet::new();
    let mut errors: Vec<Error> = vec![];
    {
        let mut retain = |oid: &str, why: String| {
            crate::trace!("RETAIN: {} {}", oid, why);
            if retained.insert(oid.to_string()) {
                prog.retained += 1;
                task.log(&prog.msg());
            }
        };
        let new_scanner = || {
            let mut s = Scanner::new();
            s.filter = Some(Filter::new(&[], &cfg().fetch_exclude_paths(), PatternType::GitIgnore));
            s
        };
        let at_ref = |sha: &str, errors: &mut Vec<Error>, retain: &mut dyn FnMut(&str, String)| {
            let mut s = new_scanner();
            let r = s.scan_tree(sha, &mut |res| match res {
                Ok(p) => retain(p.oid(), format!("via ref {sha}")),
                Err(e) => errors.push(e),
            });
            if let Err(e) = r {
                errors.push(e);
            }
        };
        // Current and recent refs (and their recent commits).
        let mut commits: Vec<String> = vec![];
        match gitcmd::current_ref() {
            Err(e) => errors.push(e),
            Ok(head) => {
                commits.push(head.sha.clone());
                if !prune_force {
                    at_ref(&head.sha, &mut errors, &mut retain);
                }
                if !prune_recent && fp.fetch_recent_refs_days > 0 {
                    let days = fp.fetch_recent_refs_days + fp.prune_offset_days;
                    crate::trace!("PRUNE: Retaining non-HEAD refs within {} ({}+{}) days", days, fp.fetch_recent_refs_days, fp.prune_offset_days);
                    let since = tools::unix_secs(std::time::SystemTime::now()) - days * 86400;
                    let refs: Vec<Ref> = match super::fetch::recent_branches(since, fp.fetch_recent_refs_include_remotes, "") {
                        Ok(r) => r,
                        Err(e) => panic_exit(&e, "Could not scan for recent refs"),
                    };
                    for r in refs {
                        if !commits.contains(&r.sha) {
                            commits.push(r.sha.clone());
                            at_ref(&r.sha, &mut errors, &mut retain);
                        }
                    }
                }
                if !prune_recent && fp.fetch_recent_commits_days > 0 {
                    let days = fp.fetch_recent_commits_days + fp.prune_offset_days;
                    for c in &commits {
                        let date = match super::fetch::commit_date(c) {
                            Ok(d) => d,
                            Err(e) => {
                                errors.push(Error::new(format!("couldn't scan commits at {c}: {e}")));
                                continue;
                            }
                        };
                        let since = crate::lfs::time_format(date - days * 86400, "%Y-%m-%d %H:%M:%S %z", false);
                        let mut s = new_scanner();
                        let r = s.scan_previous_versions(c, &since, &mut |res| match res {
                            Ok(p) => retain(p.oid(), format!("via ref {c} >= {since}")),
                            Err(e) => errors.push(e),
                        });
                        if let Err(e) = r {
                            errors.push(e);
                        }
                    }
                }
            }
        }
        // Unpushed commits.
        {
            let mut s = new_scanner();
            let r = s.scan_unpushed(&fp.prune_remote_name, &mut |res| match res {
                Ok(p) => retain(p.oid(), "unpushed".into()),
                Err(e) => errors.push(e),
            });
            if let Err(e) = r {
                errors.push(e);
            }
        }
        // Worktrees and their indexes.
        match all_worktrees() {
            Err(e) => errors.push(e),
            Ok(wts) => {
                let mut seen: Vec<String> = vec![];
                if !prune_force {
                    match gitcmd::current_ref() {
                        Ok(h) => seen.push(h.sha),
                        Err(e) => errors.push(e),
                    }
                }
                for w in wts {
                    if !prune_force && !seen.contains(&w.sha) {
                        seen.push(w.sha.clone());
                        at_ref(&w.sha, &mut errors, &mut retain);
                    }
                    if !w.prunable {
                        let mut s = new_scanner();
                        let r = s.scan_index(&w.sha, &w.dir, &mut |res| match res {
                            Ok(p) => retain(p.oid(), "index".into()),
                            Err(e) => errors.push(e),
                        });
                        if let Err(e) = r {
                            errors.push(e);
                        }
                    }
                }
            }
        }
        // Stashes.
        {
            let mut s = new_scanner();
            let r = s.scan_stashed(&mut |res| match res {
                Ok(p) => retain(p.oid(), "stashed".into()),
                Err(e) => errors.push(e),
            });
            if let Err(e) = r {
                errors.push(e);
            }
        }
    }
    let mut reachable: HashSet<String> = HashSet::new();
    if verify_remote && !verify_unreachable {
        let mut s = Scanner::new();
        s.filter = Some(Filter::new(&[], &cfg().fetch_exclude_paths(), PatternType::GitIgnore));
        let r = s.scan_all(&mut |res| match res {
            Ok(p) => {
                reachable.insert(p.oid().to_string());
            }
            Err(e) => errors.push(e),
        });
        if let Err(e) = r {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        task.complete();
        logger.close();
        for e in &errors {
            logged_error(e, &format!("Prune error: {e}"));
        }
        exit("Prune sub-tasks failed, cannot continue");
    }
    let mut prunable: Vec<String> = vec![];
    let mut total = 0i64;
    let mut verbose_out = vec![];
    let verified: Rc<RefCell<HashSet<String>>> = Default::default();
    let mut q: Option<TransferQueue> = None;
    if verify_remote {
        let mut queue = TransferQueue::new(
            Direction::Download,
            Manifest::get("download", &fp.prune_remote_name),
            &fp.prune_remote_name,
            tq::Options { dry_run: true, meter: None, remote_ref: Some(super::pull::fetch_remote_ref()), batch_size: cfg().transfer_batch_size(), cb: None },
        );
        let v = verified.clone();
        queue.watch(Box::new(move |t: &tq::Transfer| {
            v.borrow_mut().insert(t.oid.clone());
            crate::trace!("VERIFIED: {}", t.oid);
        }));
        q = Some(queue);
    }
    let fs = cfg().filesystem();
    for (oid, size) in &local {
        if retained.contains(oid) {
            continue;
        }
        prunable.push(oid.clone());
        total += size;
        if verbose {
            verbose_out.push(format!("{} ({})", oid, tools::format_bytes(*size as u64)));
        }
        if let Some(q) = q.as_mut() {
            let path = fs.object_path(oid).unwrap_or_default();
            q.add("", &path, oid, *size, false);
        }
    }
    if let Some(mut q) = q {
        q.wait();
        prog.verified += verified.borrow().len();
        task.log(&prog.msg());
        let before = prunable.len();
        let mut problems = String::new();
        let v = verified.borrow();
        let mut kept = vec![];
        for oid in prunable {
            if v.contains(&oid) {
                kept.push(oid);
            } else if verify_unreachable {
                crate::trace!("UNVERIFIED: {}", oid);
                problems.push_str(&format!(" * {oid}\n"));
            } else if reachable.contains(&oid) {
                problems.push_str(&format!(" * {oid}\n"));
            } else {
                crate::trace!("UNREACHABLE: {}", oid);
                kept.push(oid);
            }
        }
        prunable = kept;
        if before != prunable.len() {
            prog.not_remote += before - prunable.len();
            task.log(&prog.msg());
        }
        task.complete();
        if !continue_unverified && !problems.is_empty() {
            logger.close();
            exit(&format!("These objects to be pruned are missing on remote:\n{problems}"));
        }
    } else {
        task.complete();
    }
    if prunable.is_empty() {
        logger.close();
        return;
    }
    {
        let mut info = logger.simple();
        if dry_run {
            let n = prunable.len();
            info.log(&format!("{} file{} would be pruned ({})", n, if n == 1 { "" } else { "s" }, tools::format_bytes(total as u64)));
            for item in &verbose_out {
                info.log(&format!("\n * {item}"));
            }
        } else {
            for item in &verbose_out {
                info.log(&format!("\n{item}"));
            }
        }
        info.complete();
    }
    if !dry_run {
        let task = logger.percentage("Deleting objects", prunable.len() as u64);
        let mut problems = String::new();
        for oid in &prunable {
            let media = match fs.object_path(oid) {
                Ok(m) => m,
                Err(e) => {
                    problems.push_str(&format!("Unable to find media path for {oid}: {e}\n"));
                    continue;
                }
            };
            if media == "/dev/null" {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&media) {
                problems.push_str(&format!("Failed to remove file {}: {}\n", media, tools::path_err("remove", &media, &e)));
                continue;
            }
            task.count(1);
        }
        task.complete();
        logger.close();
        if !problems.is_empty() {
            logged_error(&Error::new("failed to delete some files"), &problems);
            exit("Prune failed, see errors above");
        }
    }
    logger.close();
}

fn prune_cmd(p: &Parsed) {
    if p.bool("verify-remote") && p.bool("no-verify-remote") {
        exit("Cannot specify both --verify-remote and --no-verify-remote");
    }
    let fp = crate::lfs::fetch_prune_config();
    let verify = !p.bool("no-verify-remote") && (fp.prune_verify_remote_always || p.bool("verify-remote"));
    let verify_unreachable = !p.bool("no-verify-unreachable") && (p.bool("verify-unreachable") || fp.prune_verify_unreachable_always);
    let when = if p.changed("when-unverified") { p.str("when-unverified") } else { "halt".to_string() };
    let cont = match when.as_str() {
        "halt" => false,
        "continue" => true,
        w => exit(&format!("Invalid value for --when-unverified: {w}")),
    };
    let force = p.bool("force");
    prune_with(&fp, verify, verify_unreachable, cont, p.bool("dry-run"), p.bool("verbose"), p.bool("recent") || force, force);
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd(
        "prune",
        prune_cmd,
        vec![
            flag("dry-run", Some('d'), K::Bool),
            flag("verbose", Some('v'), K::Bool),
            flag("recent", None, K::Bool),
            flag("force", Some('f'), K::Bool),
            flag("verify-remote", Some('c'), K::Bool),
            flag("no-verify-remote", None, K::Bool),
            flag("verify-unreachable", None, K::Bool),
            flag("no-verify-unreachable", None, K::Bool),
            flag("when-unverified", None, K::Str),
        ],
    )]
}
