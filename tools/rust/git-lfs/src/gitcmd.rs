//! git helpers (git package): refs, rev-parse, remotes, version checks. Commands that read
//! objects run with the LFS filters switched off (gitConfigNoLFS).

use crate::errors::{Error, Result};
use crate::{subprocess, tools};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RefType {
    LocalBranch,
    RemoteBranch,
    LocalTag,
    Head,
    #[default]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ref {
    pub name: String,
    pub typ: RefType,
    pub sha: String,
}

impl Ref {
    pub fn refspec(&self) -> String {
        match self.typ {
            RefType::LocalBranch => format!("refs/heads/{}", self.name),
            RefType::RemoteBranch => format!("refs/remotes/{}", self.name),
            RefType::LocalTag => format!("refs/tags/{}", self.name),
            _ => self.name.clone(),
        }
    }
}

pub fn parse_ref_to_type_and_name(full: &str) -> (RefType, String) {
    if full == "HEAD" {
        (RefType::Head, full.to_string())
    } else if let Some(n) = full.strip_prefix("refs/heads/") {
        (RefType::LocalBranch, n.to_string())
    } else if let Some(n) = full.strip_prefix("refs/remotes/") {
        (RefType::RemoteBranch, n.to_string())
    } else if let Some(n) = full.strip_prefix("refs/tags/") {
        (RefType::LocalTag, n.to_string())
    } else {
        (RefType::Other, full.to_string())
    }
}

pub fn parse_ref(abs: &str, sha: &str) -> Ref {
    let (typ, name) = parse_ref_to_type_and_name(abs);
    Ref { name, typ, sha: sha.to_string() }
}

pub fn has_valid_object_id_length(s: &str) -> bool {
    s.len() == 40 || s.len() == 64
}

pub fn is_zero_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|c| c == b'0')
}

/// gitConfigNoLFS: `-c filter.lfs.*=` before the arguments.
pub fn no_lfs_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["-c", "filter.lfs.smudge=", "-c", "filter.lfs.clean=", "-c", "filter.lfs.process=", "-c", "filter.lfs.required=false"];
    a.extend_from_slice(args);
    a
}

pub fn git_no_lfs_simple(args: &[&str]) -> Result<String> {
    subprocess::output("git", &no_lfs_args(args), None)
}

pub fn git_simple(args: &[&str]) -> Result<String> {
    subprocess::output("git", args, None)
}

pub fn git_no_lfs_command(args: &[&str]) -> std::process::Command {
    subprocess::command("git", &no_lfs_args(args))
}

pub fn version() -> Result<String> {
    static V: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    V.get_or_init(|| subprocess::output("git", &["version"], None).map_err(|e| e.to_string())).clone().map_err(Error::new)
}

fn ver_number(v: &str) -> u64 {
    let re = regex::Regex::new(r"(?:git version\s+)?(\d+)(?:.(\d+))?(?:.(\d+))?.*").unwrap();
    let Some(m) = re.captures(v) else { return 0 };
    let g = |i: usize| m.get(i).and_then(|x| x.as_str().parse::<u64>().ok()).unwrap_or(0);
    g(1) * 1_000_000 + g(2) * 1000 + g(3)
}

pub fn is_version_at_least(actual: &str, desired: &str) -> bool {
    ver_number(actual) >= ver_number(desired)
}

pub fn is_git_version_at_least(v: &str) -> bool {
    match version() {
        Ok(g) => is_version_at_least(&g, v),
        Err(e) => {
            crate::trace!("Error getting git version: {}", e);
            false
        }
    }
}

pub fn resolve_ref(r: &str) -> Result<Ref> {
    let out = git_no_lfs_simple(&["rev-parse", r, "--symbolic-full-name", r]).map_err(|_| Error::new(format!("Git can't resolve ref: {}", tools::quote(r))))?;
    if out.is_empty() {
        return Err(Error::new(format!("Git can't resolve ref: {}", tools::quote(r))));
    }
    let lines: Vec<&str> = out.split('\n').collect();
    if lines.len() == 1 {
        return Ok(Ref { name: lines[0].to_string(), typ: RefType::Other, sha: lines[0].to_string() });
    }
    let (typ, name) = parse_ref_to_type_and_name(lines[1]);
    Ok(Ref { name, typ, sha: lines[0].to_string() })
}

pub fn current_ref() -> Result<Ref> {
    resolve_ref("HEAD")
}

pub fn is_bare() -> Result<bool> {
    let s = subprocess::output("git", &["rev-parse", "--is-bare-repository"], None)?;
    Ok(s == "true")
}

pub fn git_dir() -> Result<String> {
    let out = git_no_lfs_command(&["rev-parse", "--git-dir"]).stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        let mut e = Error::new(format!(
            "failed to call `git rev-parse --git-dir`: {} {}: {}",
            subprocess::exit_text(&out.status),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
        e.exit_status = out.status.code();
        return Err(e);
    }
    tools::canonicalize_path(String::from_utf8_lossy(&out.stdout).trim(), false)
}

/// (git dir, work tree root); the root is empty in a bare repository or the git dir.
pub fn git_and_root_dirs() -> Result<(String, String)> {
    let out = git_no_lfs_command(&["rev-parse", "--git-dir", "--show-toplevel"]).stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        if out.status.code() == Some(128) {
            let g = git_dir();
            return match g {
                Ok(g) => Ok((g, String::new())),
                Err(e) => Err(e),
            };
        }
        return Err(Error::new(format!("failed to call `git rev-parse --git-dir --show-toplevel`: {}", tools::quote(&String::from_utf8_lossy(&out.stderr)))));
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    let paths: Vec<&str> = s.split('\n').collect();
    let g = tools::canonicalize_path(paths[0], false)?;
    if paths.len() == 1 || paths[1].is_empty() {
        return Ok((g, String::new()));
    }
    Ok((g, tools::canonicalize_path(paths[1], false)?))
}

pub fn remote_list() -> Result<Vec<String>> {
    let out = git_no_lfs_command(&["remote"]).stdin(std::process::Stdio::null()).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect())
}

/// `git remote -v`: name → fetch (or push) URLs.
pub fn remote_urls(push: bool) -> Result<Vec<(String, Vec<String>)>> {
    let out = git_no_lfs_command(&["remote", "-v"]).stdin(std::process::Stdio::null()).output()?;
    let text = if push { "(push)" } else { "(fetch)" };
    let mut ret: Vec<(String, Vec<String>)> = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let pair: Vec<&str> = line.trim().split('\t').collect();
        if pair.len() != 2 {
            continue;
        }
        let up: Vec<&str> = pair[1].split(' ').collect();
        if up.len() != 2 || up[1] != text {
            continue;
        }
        match ret.iter_mut().find(|(n, _)| n == pair[0]) {
            Some((_, v)) => v.push(up[0].to_string()),
            None => ret.push((pair[0].to_string(), vec![up[0].to_string()])),
        }
    }
    Ok(ret)
}

pub fn map_remote_url(url: &str, push: bool) -> (String, bool) {
    if let Ok(urls) = remote_urls(push) {
        for (name, rs) in urls {
            if rs.len() == 1 && rs[0] == url {
                return (name, true);
            }
        }
    }
    (url.to_string(), false)
}

pub fn validate_remote(remote: &str) -> Result<()> {
    let remotes = remote_list()?;
    if remotes.iter().any(|r| r == remote) {
        return Ok(());
    }
    if validate_remote_url(remote).is_ok() {
        return Ok(());
    }
    Err(Error::new(format!("invalid remote name: {}", tools::quote(remote))))
}

pub fn validate_remote_url(remote: &str) -> Result<()> {
    match crate::gourl::parse(remote) {
        Ok(u) if !u.scheme.is_empty() => match u.scheme.as_str() {
            "ssh" | "http" | "https" | "git" | "file" => Ok(()),
            s => Err(Error::new(format!("invalid remote URL protocol {} in {}", tools::quote(s), tools::quote(remote)))),
        },
        _ => {
            if remote.contains(':') {
                Ok(())
            } else {
                Err(Error::new(format!("invalid remote name: {}", tools::quote(remote))))
            }
        }
    }
}

pub fn rewrite_local_path_as_url(path: &str) -> String {
    let mut path = tools::abs(path).display().to_string();
    let gitpath;
    if std::path::Path::new(&path).file_name().map_or(false, |n| n == ".git") {
        gitpath = path.clone();
        path = std::path::Path::new(&path).parent().map(|p| p.display().to_string()).unwrap_or_default();
    } else {
        gitpath = format!("{path}/.git");
    }
    if std::fs::metadata(&gitpath).is_ok() {
        path = gitpath;
    } else if std::fs::metadata(&path).is_err() {
        return path;
    }
    format!("file://{path}")
}

pub fn local_refs() -> Result<Vec<Ref>> {
    let out = git_no_lfs_command(&["show-ref"]).stdin(std::process::Stdio::null()).output()?;
    let mut refs = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        let Some((sha, name)) = line.split_once(' ') else { continue };
        if !has_valid_object_id_length(sha) || name.is_empty() {
            continue;
        }
        let (typ, n) = parse_ref_to_type_and_name(name);
        if typ != RefType::LocalBranch && typ != RefType::LocalTag {
            continue;
        }
        refs.push(Ref { name: n, typ, sha: sha.to_string() });
    }
    Ok(refs)
}

pub fn remote_for_branch(branch: &str) -> String {
    crate::config::cfg().find(&format!("branch.{branch}.remote"))
}

pub fn remote_branch_for_local_branch(branch: &str) -> String {
    let merge = crate::config::cfg().find(&format!("branch.{branch}.merge"));
    match merge.strip_prefix("refs/heads/") {
        Some(m) => m.to_string(),
        None => branch.to_string(),
    }
}

pub fn empty_tree() -> String {
    static T: OnceLock<String> = OnceLock::new();
    T.get_or_init(|| git_no_lfs_simple(&["hash-object", "-t", "tree", "/dev/null"]).unwrap_or_default()).clone()
}

/// git.TrackingRef: the ref a branch merges from (`branch.NAME.merge`), or the ref itself.
pub fn tracking_ref(r: &Ref) -> Ref {
    match crate::config::cfg().git().get(&format!("branch.{}.merge", r.name)) {
        Some(m) => parse_ref(&m, ""),
        None => r.clone(),
    }
}

/// The remote ref a push of `local` to `remote` updates (RefUpdate.RemoteRef with no
/// explicit remote ref): by push.default.
pub fn default_remote_ref(remote: &str, local: &Ref) -> Ref {
    let g = crate::config::cfg().git();
    match g.get("push.default").unwrap_or_default().as_str() {
        "" | "simple" => {
            if g.get(&format!("branch.{}.remote", local.name)).as_deref().unwrap_or("") == remote {
                tracking_ref(local)
            } else {
                local.clone()
            }
        }
        "upstream" | "tracking" => tracking_ref(local),
        "current" => local.clone(),
        m => {
            crate::trace!("WARNING: {} push mode not supported", tools::quote(m));
            local.clone()
        }
    }
}

pub fn ref_commitish(r: &Ref) -> String {
    if r.sha.is_empty() {
        r.name.clone()
    } else {
        r.sha.clone()
    }
}

/// git.NewLsFiles: the paths `git ls-files` lists (cached, and optionally untracked).
pub fn ls_files(working_dir: &str, standard_exclude: bool, untracked: bool) -> Result<Vec<String>> {
    let mut args = vec!["ls-files", "-z", "--cached"];
    if is_git_version_at_least("2.35.0") {
        args.push("--sparse");
    }
    if standard_exclude {
        args.push("--exclude-standard");
    }
    if untracked {
        args.push("--others");
    }
    crate::trace!("NewLsFiles: running in {} git {}", working_dir, args.join(" "));
    let mut c = git_no_lfs_command(&args);
    if !working_dir.is_empty() {
        c.current_dir(working_dir);
    }
    let out = c.stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::new(format!("Error in `git {}`: {} {}", args.join(" "), subprocess::exit_text(&out.status), String::from_utf8_lossy(&out.stderr))));
    }
    Ok(out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

/// GetTrackedFiles: files git tracks that match a pattern (via `ls-files --ignored -x`).
pub fn get_tracked_files(pattern: &str) -> Result<Vec<String>> {
    let safe = pattern.strip_prefix('/').unwrap_or(pattern);
    let root_wildcard = safe.len() < pattern.len() && safe.contains('*');
    let out = git_no_lfs_command(&["ls-files", "--ignored", "--cached", "-z", "-x", safe]).stdin(std::process::Stdio::null()).output()?;
    let mut ret = vec![];
    for line in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let line = String::from_utf8_lossy(line).into_owned();
        if root_wildcard && line.contains('/') {
            continue;
        }
        ret.push(line.trim().to_string());
    }
    if !out.status.success() {
        return Err(Error::new(subprocess::exit_text(&out.status)));
    }
    Ok(ret)
}

/// FirstRemoteForTreeish: the remote of the first remote-tracking branch containing it.
pub fn first_remote_for_treeish(treeish: &str) -> String {
    let out = if treeish.is_empty() {
        crate::trace!("git: treeish: not provided");
        git_no_lfs_simple(&["branch", "-r", "--contains", "HEAD"])
    } else {
        crate::trace!("git: treeish: {}", tools::quote(treeish));
        git_no_lfs_simple(&["branch", "-r", "--contains", treeish])
    };
    let refs: Vec<String> = match out {
        Ok(o) if !o.is_empty() => o.split('\n').map(str::to_string).collect(),
        _ => {
            crate::trace!("git: symbolic name: can't resolve symbolic name for ref: {}", tools::quote(treeish));
            vec![]
        }
    };
    let Some(name) = refs.into_iter().find(|r| !r.is_empty()) else {
        crate::trace!("git: remote treeish: no valid remote refs parsed for {}", tools::quote(treeish));
        return String::new();
    };
    crate::trace!("git: working ref: {}", name);
    let Ok(remotes) = remote_list() else { return String::new() };
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() < 2 {
        return String::new();
    }
    for r in &remotes {
        if r.contains('/') {
            crate::trace!("git: ref remote: cannot determine remote for ref {} since remote {} contains a slash", name, r);
            return String::new();
        }
    }
    crate::trace!("git: working remote {}", parts[0]);
    parts[0].to_string()
}
