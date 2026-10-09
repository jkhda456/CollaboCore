//! lock, unlock and locks (commands/command_lock.go, command_unlock.go, command_locks.go).

use super::{cmd, error, exit, exit_with_code, exit_with_error, print, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::gitcmd;
use crate::locking::{self, Lock};
use crate::tools;

struct LockData {
    root: String,
    wd: String,
}

fn lock_data() -> Result<LockData> {
    let wd = std::env::current_dir().map_err(Error::from)?;
    let wd = tools::canonicalize_system_path(&wd).map_err(Error::from)?;
    Ok(LockData { root: cfg().local_working_dir(), wd: wd.display().to_string() })
}

/// lockPath: the path relative to the top of the working tree.
fn lock_path(d: &LockData, file: &str) -> std::result::Result<String, (String, Error)> {
    let abs = if file.starts_with('/') {
        match tools::canonicalize_system_path(file) {
            Ok(p) => p.display().to_string(),
            Err(e) => return Err((String::new(), Error::new(format!("unable to canonicalize path {}: {}", tools::quote(file), tools::io_err(&e))))),
        }
    } else {
        tools::clean_str(&format!("{}/{}", d.wd, file))
    };
    let path = crate::attrs::rel(&d.root, &abs).unwrap_or_default();
    if path.starts_with("../") {
        return Err((String::new(), Error::new(format!("unable to canonicalize path {}", tools::quote(&path)))));
    }
    if std::fs::metadata(&abs).is_ok_and(|m| m.is_dir()) {
        return Err((path, Error::new(format!("cannot lock directory: {file}"))));
    }
    Ok(path)
}

/// newLockClient, aimed at the ref a push of the current branch would update.
fn lock_client(remote_flag: &str) -> locking::Client {
    if !remote_flag.is_empty() {
        cfg().set_remote(remote_flag);
        cfg().set_push_remote(remote_flag);
    }
    let pr = cfg().push_remote();
    let mut c = locking::Client::new(&pr);
    if let Err(e) = tools::mkdir_all(cfg().lfs_storage_dir(), cfg().repository_permissions(true)) {
        exit(&format!("Unable to create lock system: {}", tools::io_err(&e)));
    }
    c.remote_ref = Some(gitcmd::default_remote_ref(&pr, &cfg().current_ref()));
    c
}

fn json_line(v: &impl serde::Serialize) {
    print(&serde_json::to_string(v).unwrap_or_default());
}

fn lock(p: &Parsed) {
    let d = match lock_data() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    let client = lock_client(&p.str("remote"));
    let json = p.bool("json");
    let mut ok = true;
    let mut locks: Vec<Lock> = vec![];
    for a in &p.args {
        let path = match lock_path(&d, a) {
            Ok(p) => p,
            Err((_, e)) => {
                error(&e.to_string());
                ok = false;
                continue;
            }
        };
        match client.lock_file(&path) {
            Ok(l) => {
                locks.push(l);
                if !json {
                    print(&format!("Locked {path}"));
                }
            }
            Err(e) => {
                error(&format!("Locking {} failed: {}", path, e.innermost()));
                ok = false;
            }
        }
    }
    if json {
        json_line(&locks);
    }
    if !ok {
        exit_with_code(2);
    }
}

#[derive(serde::Serialize)]
struct UnlockResponse {
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    path: String,
    unlocked: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    reason: String,
}

/// IsFileModified: `git status --porcelain` lists it.
fn is_file_modified(path: &str) -> Result<bool> {
    let out = crate::subprocess::command("git", &["-c", "core.quotepath=false", "status", "--porcelain", "--", path]).output().map_err(|e| Error::from(e).wrap("failed to find `git status`"))?;
    if !out.status.success() {
        return Err(Error::new(crate::subprocess::exit_text(&out.status)).wrap("`git status` failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().any(|l| l.len() > 3 && l[3..].trim() == path))
}

fn abort_if_modified(path: &str, force: bool) -> Result<()> {
    match is_file_modified(path) {
        Err(e) => {
            if force {
                Ok(())
            } else {
                Err(e)
            }
        }
        Ok(true) => {
            if force {
                error("warning: unlocking with uncommitted changes because --force");
                Ok(())
            } else {
                Err(Error::new("Cannot unlock file with uncommitted changes"))
            }
        }
        Ok(false) => Ok(()),
    }
}

fn unlock(p: &Parsed) {
    let id = p.str("id");
    let has_path = !p.args.is_empty();
    if has_path == !id.is_empty() {
        exit("Exactly one of --id or a set of paths must be provided");
    }
    let force = p.bool("force");
    let json = p.bool("json");
    let d = match lock_data() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    let client = lock_client(&p.str("remote"));
    let mut out: Vec<UnlockResponse> = vec![];
    let mut ok = true;
    let fail = |out: &mut Vec<UnlockResponse>, id: &str, path: &str, e: String| {
        error(&e);
        if json {
            out.push(UnlockResponse { id: id.into(), path: path.into(), unlocked: false, reason: e });
        }
    };
    if has_path {
        for spec in &p.args {
            let path = match lock_path(&d, spec) {
                Ok(x) => x,
                Err((path, e)) => {
                    if !force {
                        fail(&mut out, "", &path, format!("Unable to determine path: {e}"));
                        ok = false;
                        continue;
                    }
                    spec.clone()
                }
            };
            if let Err(e) = abort_if_modified(&path, force) {
                fail(&mut out, "", &path, e.to_string());
                ok = false;
                continue;
            }
            if let Err(e) = client.unlock_file(&path, force) {
                fail(&mut out, "", &path, e.innermost().to_string());
                ok = false;
                continue;
            }
            if !json {
                print(&format!("Unlocked {path}"));
                continue;
            }
            out.push(UnlockResponse { id: String::new(), path, unlocked: true, reason: String::new() });
        }
    } else {
        let filter = vec![("id".to_string(), id.clone())];
        let mut locks = client.search_locks(&filter, 0, true, false).unwrap_or_default();
        if locks.is_empty() {
            locks = client.search_locks(&filter, 0, false, false).unwrap_or_default();
        }
        if let Some(l) = locks.first() {
            let _ = abort_if_modified(&l.path, force);
        }
        match client.unlock_file_by_id(&id, force) {
            Err(e) => {
                fail(&mut out, &id, "", format!("Unable to unlock {}: {}", id, e.innermost()));
                ok = false;
            }
            Ok(()) => {
                if !json {
                    print(&format!("Unlocked Lock {id}"));
                } else {
                    out.push(UnlockResponse { id: id.clone(), path: String::new(), unlocked: true, reason: String::new() });
                }
            }
        }
    }
    if json {
        json_line(&out);
    }
    if !ok {
        exit_with_code(2);
    }
}

fn locks(p: &Parsed) {
    let d = match lock_data() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    let mut filters: Vec<(String, String)> = vec![];
    if !p.str("path").is_empty() {
        match lock_path(&d, &p.str("path")) {
            Ok(x) => filters.push(("path".into(), x)),
            Err((_, e)) => exit(&format!("Error building filters: {e}")),
        }
    }
    if !p.str("id").is_empty() {
        filters.push(("id".into(), p.str("id")));
    }
    let client = lock_client(&p.str("remote"));
    let limit = p.int("limit", 0).max(0) as usize;
    let (cached, local, verify, json) = (p.bool("cached"), p.bool("local"), p.bool("verify"), p.bool("json"));
    if cached {
        if limit > 0 {
            exit("--cached option can't be combined with --limit");
        }
        if !filters.is_empty() {
            exit("--cached option can't be combined with filters");
        }
        if local {
            exit("--cached option can't be combined with --local");
        }
    }
    if verify {
        if !filters.is_empty() {
            exit("--verify option can't be combined with filters");
        }
        if local {
            exit("--verify option can't be combined with --local");
        }
    }
    let (locks, owned, err): (Vec<Lock>, Option<Vec<Lock>>, Option<Error>) = if verify {
        match client.search_locks_verifiable(limit, cached) {
            Ok((ours, theirs)) => {
                if json {
                    json_line(&serde_json::json!({"ours": ours, "theirs": theirs}));
                    return;
                }
                let mut all = ours.clone();
                all.extend(theirs);
                (all, Some(ours), None)
            }
            Err(e) => {
                if json {
                    json_line(&serde_json::json!({"ours": [], "theirs": []}));
                    return;
                }
                (vec![], Some(vec![]), Some(e))
            }
        }
    } else {
        match client.search_locks(&filters, limit, local, cached) {
            Ok(l) => {
                if json {
                    json_line(&l);
                    return;
                }
                (l, None, None)
            }
            Err(e) => {
                if json {
                    json_line(&Vec::<Lock>::new());
                    return;
                }
                (vec![], None, Some(e))
            }
        }
    };
    let maxp = locks.iter().map(|l| l.path.len()).max().unwrap_or(0);
    let maxn = locks.iter().filter_map(|l| l.owner.as_ref()).map(|o| o.name.len()).max().unwrap_or(0);
    let mut by_path: std::collections::BTreeMap<String, Lock> = std::collections::BTreeMap::new();
    for l in locks {
        by_path.insert(l.path.clone(), l);
    }
    for (path, l) in &by_path {
        let name = l.owner.as_ref().map(|o| o.name.clone()).unwrap_or_default();
        let kind = match &owned {
            Some(o) => {
                if o.contains(l) {
                    "O "
                } else {
                    "  "
                }
            }
            None => "",
        };
        print(&format!("{kind}{path}{}\t{name}{}\tID:{}", " ".repeat(maxp - path.len()), " ".repeat(maxn - name.len()), l.id));
    }
    if let Some(e) = err {
        exit(&format!("Error while retrieving locks: {}", e.innermost()));
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd("lock", lock, vec![flag("remote", Some('r'), K::Str), flag("json", Some('j'), K::Bool)]),
        cmd(
            "unlock",
            unlock,
            vec![flag("remote", Some('r'), K::Str), flag("id", Some('i'), K::Str), flag("force", Some('f'), K::Bool), flag("json", Some('j'), K::Bool)],
        ),
        cmd(
            "locks",
            locks,
            vec![
                flag("remote", Some('r'), K::Str),
                flag("path", Some('p'), K::Str),
                flag("id", Some('i'), K::Str),
                flag("limit", Some('l'), K::Int),
                flag("local", None, K::Bool),
                flag("cached", None, K::Bool),
                flag("verify", None, K::Bool),
                flag("json", Some('j'), K::Bool),
            ],
        ),
    ]
}
