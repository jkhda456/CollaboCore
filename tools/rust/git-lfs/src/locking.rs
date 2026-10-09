//! Locks (locking package): the local cache of the user's locks, lockable patterns and the
//! read-only flags of lockable files. The lock API itself is in commands/lock.rs.

use crate::config::cfg;
use crate::filter::{Filter, PatternType};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Owner {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Lock {
    pub id: String,
    pub path: String,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub locked_at: String,
}

fn cache_path() -> String {
    format!("{}/lockcache.db", cfg().lfs_storage_dir())
}

/// The cached locks (ours: what lock/unlock and `locks --verify` recorded).
pub fn cached_locks() -> Vec<Lock> {
    std::fs::read(cache_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

pub fn save_cached_locks(locks: &[Lock]) {
    let _ = crate::tools::mkdir_all(cfg().lfs_storage_dir(), cfg().repository_permissions(false));
    let _ = std::fs::write(cache_path(), serde_json::to_vec(locks).unwrap_or_default());
}

pub fn cache_add(l: &Lock) {
    let mut v = cached_locks();
    v.retain(|x| x.path != l.path && x.id != l.id);
    v.push(l.clone());
    save_cached_locks(&v);
}

pub fn cache_remove_by_id(id: &str) {
    let mut v = cached_locks();
    v.retain(|x| x.id != id);
    save_cached_locks(&v);
}

pub fn is_file_locked_by_current_committer(path: &str) -> bool {
    cached_locks().iter().any(|l| l.path == path)
}

pub fn lockable_patterns() -> Vec<String> {
    let c = cfg();
    crate::attrs::get_attribute_paths(&mut crate::attrs::MacroProcessor::new(), &c.local_working_dir(), &c.local_git_dir()).into_iter().filter(|p| p.lockable).map(|p| p.path).collect()
}

pub fn lockable_filter() -> Filter {
    let mut f = Filter::new(&lockable_patterns(), &[], PatternType::GitAttributes);
    f.default_value = false;
    f
}

pub fn is_file_lockable(path: &str) -> bool {
    lockable_filter().allows(path)
}

/// tools.SetFileWriteFlag.
pub fn set_file_write_flag(path: &str, write: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(path)?;
    let mode = m.permissions().mode();
    if (write && mode & 0o200 != 0) || (!write && mode & 0o222 == 0) {
        return Ok(());
    }
    let nm = if write { mode | 0o200 } else { mode & !0o222 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(nm))
}

fn fix_single(file: &str, lockable: Option<&Filter>, unlockable: Option<&Filter>) -> std::io::Result<()> {
    if let Some(l) = lockable {
        if l.allows(file) {
            return match set_file_write_flag(file, is_file_locked_by_current_committer(file)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
    }
    if let Some(u) = unlockable {
        if u.allows(file) {
            return match set_file_write_flag(file, true) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
    }
    Ok(())
}

pub fn fix_file_write_flags_in_dir(dir: &str, lockable: &[String], unlockable: &[String]) -> crate::errors::Result<()> {
    if lockable.is_empty() && unlockable.is_empty() {
        return Ok(());
    }
    let wd = cfg().local_working_dir();
    let abs = if dir.starts_with('/') { dir.to_string() } else { crate::tools::join(&[&wd, dir]) };
    let m = std::fs::metadata(&abs).map_err(|e| crate::tools::path_err("stat", &abs, &e))?;
    if !m.is_dir() {
        return Err(crate::errors::Error::new(format!("{} is not a valid directory", crate::tools::quote(dir))));
    }
    let lf = (!lockable.is_empty()).then(|| Filter::new(lockable, &[], PatternType::GitAttributes));
    let uf = (!unlockable.is_empty()).then(|| Filter::new(unlockable, &[], PatternType::GitAttributes));
    let modify_ignored = cfg().git().bool("lfs.lockignoredfiles", false);
    for f in crate::gitcmd::ls_files(&wd, !modify_ignored, false)? {
        fix_single(&f, lf.as_ref(), uf.as_ref()).map_err(|e| crate::tools::path_err("chmod", &f, &e))?;
    }
    Ok(())
}

/// FixLockableFileWriteFlags: the given files' write bits by lockability and our locks.
pub fn fix_lockable_file_write_flags(files: &[String]) -> crate::errors::Result<()> {
    if lockable_patterns().is_empty() {
        return Ok(());
    }
    let f = lockable_filter();
    let mut err = None;
    for file in files {
        if let Err(e) = fix_single(file, Some(&f), None) {
            err = Some(crate::tools::path_err("chmod", file, &e));
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub fn fix_all_lockable_file_write_flags() -> crate::errors::Result<()> {
    let wd = cfg().local_working_dir();
    let f = lockable_filter();
    let modify_ignored = cfg().git().bool("lfs.lockignoredfiles", false);
    for file in crate::gitcmd::ls_files(&wd, !modify_ignored, false)? {
        fix_single(&file, Some(&f), None).map_err(|e| crate::tools::path_err("chmod", &file, &e))?;
    }
    Ok(())
}

// The lock API (locking/api.go, ssh.go, locks.go): over HTTP, or over the pure SSH
// connection when the remote has one.

use crate::errors::{Error, Kind, Result};
use crate::gitcmd::Ref;
use crate::http::Hooks;

#[derive(Default, Deserialize)]
struct LockResponse {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    lock: Option<Lock>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    message: String,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    request_id: String,
}

#[derive(Default, Deserialize, Serialize)]
pub struct LockList {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub locks: Vec<Lock>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub next_cursor: String,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing)]
    pub request_id: String,
}

#[derive(Default, Deserialize, Serialize)]
pub struct VerifiableList {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub ours: Vec<Lock>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub theirs: Vec<Lock>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub next_cursor: String,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing)]
    pub request_id: String,
}

pub struct Client {
    pub remote: String,
    pub remote_ref: Option<Ref>,
}

fn refspec(r: &Option<Ref>) -> String {
    r.as_ref().map(|r| r.refspec()).unwrap_or_default()
}

fn ssh_for(operation: &str, remote: &str) -> Option<std::sync::Arc<crate::ssh::SshTransfer>> {
    crate::ssh::transfer_for(operation, remote)
}

fn api(method: &str, operation: &str, remote: &str, suffix: &str, body: Option<&serde_json::Value>, key: &str, query: &[(String, String)]) -> Result<(u32, crate::http::Response)> {
    let e = crate::endpoint::endpoint(operation, remote);
    let mut req = crate::http::client().new_request(method, &e, suffix, body)?;
    if !query.is_empty() {
        let mut q: Vec<(String, String)> = query.to_vec();
        q.sort();
        let enc: Vec<String> = q.iter().map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v))).collect();
        req.url = format!("{}?{}", req.url.split('?').next().unwrap_or(""), enc.join("&"));
    }
    req.stats_key = Some(key.into());
    let (res, err) = crate::lfsapi::do_api_request_with_auth(remote, &mut req, &mut Hooks::default());
    match (res, err) {
        (res, Some(e)) => {
            let mut e = e;
            if let Some(r) = &res {
                e.http_status = Some(r.status);
            }
            Err(e)
        }
        (Some(r), None) => Ok((r.status, r)),
        (None, None) => Err(Error::new("no response")),
    }
}

/// url.QueryEscape.
pub fn query_escape(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn status_of(e: &Error) -> u32 {
    let mut x = Some(e);
    while let Some(y) = x {
        if let Some(s) = y.http_status {
            return s;
        }
        x = y.cause.as_deref();
    }
    0
}

fn parse_ssh_lock(status: i32, args: &[String], lines: &[String]) -> Result<(Option<Lock>, String)> {
    let mut lock = None;
    if (200..=299).contains(&status) || status == 409 {
        let mut l = Lock::default();
        let mut seen = std::collections::HashSet::new();
        for a in args {
            if let Some(v) = a.strip_prefix("id=") {
                l.id = v.into();
                seen.insert("id");
            } else if let Some(v) = a.strip_prefix("path=") {
                l.path = v.into();
                seen.insert("path");
            } else if let Some(v) = a.strip_prefix("ownername=") {
                l.owner = Some(Owner { name: v.into() });
                seen.insert("ownername");
            } else if let Some(v) = a.strip_prefix("locked-at=") {
                if crate::tools::parse_rfc3339(v).is_none() {
                    return Err(Error::new(format!("lock response: invalid locked-at: {a}")));
                }
                l.locked_at = v.into();
                seen.insert("locked-at");
            }
        }
        if seen.len() != 4 {
            return Err(Error::new("incomplete fields for lock"));
        }
        lock = Some(l);
    }
    let msg = if status > 299 { lines.first().cloned().unwrap_or_default() } else { String::new() };
    Ok((lock, msg))
}

/// (all, ours, theirs, next cursor, message)
fn parse_ssh_list(status: i32, args: &[String], lines: &[String]) -> Result<(Vec<Lock>, Vec<Lock>, Vec<Lock>, String, String)> {
    let (mut all, mut ours, mut theirs) = (vec![], vec![], vec![]);
    let mut next = String::new();
    if (200..=299).contains(&status) {
        for a in args {
            if let Some(v) = a.strip_prefix("next-cursor=") {
                if !next.is_empty() {
                    return Err(Error::new("lock response: multiple next-cursor responses"));
                }
                next = v.into();
            }
        }
        let mut locks: Vec<(Lock, String)> = vec![];
        let incomplete = |l: &Lock| l.path.is_empty() || l.owner.is_none() || l.locked_at.is_empty();
        for e in lines {
            let v: Vec<&str> = e.splitn(3, ' ').collect();
            if v[0] == "lock" {
                if v.len() != 2 {
                    return Err(Error::new(format!("lock response: invalid response: {}", crate::tools::quote(e))));
                }
                if locks.last().is_some_and(|l| incomplete(&l.0)) {
                    return Err(Error::new("lock response: incomplete lock data"));
                }
                locks.push((Lock { id: v[1].into(), ..Default::default() }, String::new()));
            } else if v.len() != 3 {
                return Err(Error::new(format!("lock response: invalid response: {}", crate::tools::quote(e))));
            } else if locks.last().is_none_or(|l| l.0.id != v[1]) {
                return Err(Error::new(format!("lock response: interspersed response: {}", crate::tools::quote(e))));
            } else {
                let last = locks.last_mut().unwrap();
                match v[0] {
                    "path" => last.0.path = v[2].into(),
                    "owner" => last.1 = v[2].into(),
                    "ownername" => last.0.owner = Some(Owner { name: v[2].into() }),
                    "locked-at" => {
                        if crate::tools::parse_rfc3339(v[2]).is_none() {
                            return Err(Error::new(format!("lock response: invalid locked-at: {e}")));
                        }
                        last.0.locked_at = v[2].into();
                    }
                    _ => {}
                }
            }
        }
        if locks.last().is_some_and(|l| incomplete(&l.0)) {
            return Err(Error::new("lock response: incomplete lock data"));
        }
        for (l, who) in locks {
            all.push(l.clone());
            match who.as_str() {
                "ours" => ours.push(l),
                "theirs" => theirs.push(l),
                _ => {}
            }
        }
    }
    let msg = if status > 299 { lines.first().cloned().unwrap_or_default() } else { String::new() };
    Ok((all, ours, theirs, next, msg))
}

impl Client {
    pub fn new(remote: &str) -> Client {
        Client { remote: remote.to_string(), remote_ref: None }
    }

    fn raw_lock(&self, path: &str) -> Result<(LockResponse, u32)> {
        let rs = refspec(&self.remote_ref);
        if let Some(t) = ssh_for("upload", &self.remote) {
            let conn = t.connection(0)?;
            let mut c = conn.lock().unwrap();
            let args = vec![format!("path={path}"), format!("refname={rs}")];
            c.send_message("lock", &args)?;
            let (status, args, lines) = c.read_status_with_lines()?;
            let (lock, message) = parse_ssh_lock(status, &args, &lines)?;
            return Ok((LockResponse { lock, message, request_id: String::new() }, status as u32));
        }
        let mut body = serde_json::json!({"path": path});
        let mut r = serde_json::Map::new();
        if !rs.is_empty() {
            r.insert("name".into(), rs.into());
        }
        body["ref"] = serde_json::Value::Object(r);
        let (status, res) = api("POST", "upload", &self.remote, "locks", Some(&body), "lfs.locks.lock", &[])?;
        let lr: LockResponse = crate::http::decode_json(&res).map_err(|e| e.into_error())?;
        if lr.lock.is_none() && lr.message.is_empty() {
            return Err(Error::new("invalid server response"));
        }
        Ok((lr, status))
    }

    pub fn lock_file(&self, path: &str) -> Result<Lock> {
        let (res, _) = self.raw_lock(path).map_err(|e| e.wrap("locking API"))?;
        if !res.message.is_empty() {
            if !res.request_id.is_empty() {
                crate::trace!("Server Request ID: {}", res.request_id);
            }
            return Err(Error::new(format!("server unable to create lock: {}", res.message)));
        }
        let lock = res.lock.unwrap();
        cache_add(&lock);
        let abs = format!("{}/{}", cfg().local_working_dir(), path);
        if crate::tools::file_exists(&abs) {
            set_file_write_flag(&abs, true).map_err(|e| Error::new(crate::tools::io_err(&e)).wrap("set file write flag"))?;
        }
        Ok(lock)
    }

    fn raw_unlock(&self, id: &str, force: bool) -> Result<(LockResponse, u32)> {
        let rs = refspec(&self.remote_ref);
        if let Some(t) = ssh_for("upload", &self.remote) {
            let conn = t.connection(0)?;
            let mut c = conn.lock().unwrap();
            let mut args = vec![];
            if let Some(r) = &self.remote_ref {
                args.push(format!("refname={}", r.name));
            }
            c.send_message(&format!("unlock {id}"), &args)?;
            let (status, args, lines) = c.read_status_with_lines()?;
            let (lock, message) = parse_ssh_lock(status, &args, &lines)?;
            return Ok((LockResponse { lock, message, request_id: String::new() }, status as u32));
        }
        let mut r = serde_json::Map::new();
        if !rs.is_empty() {
            r.insert("name".into(), rs.into());
        }
        let body = serde_json::json!({"force": force, "ref": serde_json::Value::Object(r)});
        let (status, res) = api("POST", "upload", &self.remote, &format!("locks/{id}/unlock"), Some(&body), "lfs.locks.unlock", &[])?;
        let lr: LockResponse = crate::http::decode_json(&res).map_err(|e| e.into_error())?;
        if lr.lock.is_none() && lr.message.is_empty() {
            return Err(Error::new("invalid server response"));
        }
        Ok((lr, status))
    }

    pub fn unlock_file(&self, path: &str, force: bool) -> Result<()> {
        let id = self.lock_id_from_path(path).map_err(|e| Error::new(format!("unable to get lock ID: {e}")))?;
        self.unlock_file_by_id(&id, force)
    }

    pub fn unlock_file_by_id(&self, id: &str, force: bool) -> Result<()> {
        let (res, _) = self.raw_unlock(id, force).map_err(|e| e.wrap("locking API"))?;
        if !res.message.is_empty() {
            if !res.request_id.is_empty() {
                crate::trace!("Server Request ID: {}", res.request_id);
            }
            return Err(Error::new(format!("server unable to unlock: {}", res.message)));
        }
        cache_remove_by_id(id);
        if let Some(l) = res.lock {
            let abs = format!("{}/{}", cfg().local_working_dir(), l.path);
            if cfg().set_lockable_files_read_only() && is_file_lockable(&l.path) && crate::tools::file_exists(&abs) {
                return set_file_write_flag(&abs, false).map_err(|e| Error::new(crate::tools::io_err(&e)));
            }
        }
        Ok(())
    }

    /// One page of a search: (list, HTTP status).
    fn raw_search(&self, query: &[(String, String)]) -> Result<(LockList, u32)> {
        if let Some(t) = ssh_for("download", &self.remote) {
            let conn = t.connection(0)?;
            let mut c = conn.lock().unwrap();
            let args: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            c.send_message("list-lock", &args)?;
            let (status, args, lines) = c.read_status_with_lines()?;
            let (all, _, _, next, message) = parse_ssh_list(status, &args, &lines)?;
            return Ok((LockList { locks: all, next_cursor: next, message, request_id: String::new() }, status as u32));
        }
        let (status, res) = api("GET", "download", &self.remote, "locks", None, "lfs.locks.search", query)?;
        let list = if status == 200 { crate::http::decode_json(&res).map_err(|e| e.into_error())? } else { LockList::default() };
        Ok((list, status))
    }

    fn query(filters: &[(String, String)], cursor: &str, limit: usize, refspec: &str) -> Vec<(String, String)> {
        let mut q: Vec<(String, String)> = filters.to_vec();
        if !cursor.is_empty() {
            q.push(("cursor".into(), cursor.into()));
        }
        if limit > 0 {
            q.push(("limit".into(), limit.to_string()));
        }
        if !refspec.is_empty() {
            q.push(("refspec".into(), refspec.into()));
        }
        q
    }

    fn lock_id_from_path(&self, path: &str) -> Result<String> {
        let q = Self::query(&[("path".into(), path.into())], "", 0, &refspec(&self.remote_ref));
        let (list, _) = self.raw_search(&q)?;
        match list.locks.len() {
            0 => Err(Error::new("no matching locks found")),
            1 => Ok(list.locks[0].id.clone()),
            _ => Err(Error::new("multiple locks found; ambiguous")),
        }
    }

    pub fn search_remote_locks(&self, filters: &[(String, String)], limit: usize) -> Result<Vec<Lock>> {
        let mut locks = vec![];
        let mut cursor = String::new();
        loop {
            let q = Self::query(filters, &cursor, limit, &refspec(&self.remote_ref));
            let (list, _) = self.raw_search(&q).map_err(|e| e.wrap("locking"))?;
            if !list.message.is_empty() {
                if !list.request_id.is_empty() {
                    crate::trace!("Server Request ID: {}", list.request_id);
                }
                return Err(Error::new(format!("server error searching for locks: {}", list.message)));
            }
            for l in list.locks {
                locks.push(l);
                if limit > 0 && locks.len() >= limit {
                    return Ok(locks);
                }
            }
            if list.next_cursor.is_empty() {
                break;
            }
            cursor = list.next_cursor;
        }
        Ok(locks)
    }

    fn cache_file(&self, kind: &str) -> Result<String> {
        let mut dir = format!("{}/cache/locks", cfg().lfs_storage_dir());
        if let Some(r) = &self.remote_ref {
            dir = format!("{dir}/{}", r.refspec());
        }
        match std::fs::metadata(&dir) {
            Ok(m) if !m.is_dir() => return Err(Error::new(format!("inititalization of cache directory {dir} failed: already exists, but is no directory"))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::tools::mkdir_all(&dir, cfg().repository_permissions(true))
                    .map_err(|e| Error::new(crate::tools::io_err(&e)).wrap(format!("initiailization of cache directory {dir} failed: directory creation failed")))?;
            }
            Err(e) => return Err(Error::new(crate::tools::io_err(&e)).wrap(format!("initialization of cache directory {dir} failed"))),
        }
        Ok(format!("{dir}/{kind}"))
    }

    fn read_cache<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Result<T> {
        let f = self.cache_file(kind)?;
        if std::fs::metadata(&f).is_err() {
            return Err(Error::new("no cached locks present"));
        }
        let d = std::fs::read(&f)?;
        serde_json::from_slice(&d).map_err(|e| Error::new(e.to_string()))
    }

    fn write_cache(&self, kind: &str, v: &impl Serialize) -> Result<()> {
        let f = self.cache_file(kind)?;
        let mut s = serde_json::to_string(v).unwrap();
        s.push('\n');
        std::fs::write(&f, s).map_err(|e| crate::tools::path_err("open", &f, &e))
    }

    /// SearchLocks: from the local cache, the cached remote list, or the server.
    pub fn search_locks(&self, filters: &[(String, String)], limit: usize, local: bool, cached: bool) -> Result<Vec<Lock>> {
        if local {
            let mut out = vec![];
            for l in cached_locks() {
                if filters.iter().any(|(k, v)| (k == "path" && *v != l.path) || (k == "id" && *v != l.id)) {
                    continue;
                }
                out.push(l);
                if limit > 0 && out.len() >= limit {
                    break;
                }
            }
            return Ok(out);
        }
        if cached {
            if !filters.is_empty() || limit != 0 {
                return Err(Error::new("can't search cached locks when filter or limit is set"));
            }
            return self.read_cache("remote");
        }
        let locks = self.search_remote_locks(filters, limit)?;
        if filters.is_empty() && limit == 0 {
            self.write_cache("remote", &locks)?;
        }
        Ok(locks)
    }

    /// SearchLocksVerifiable: (ours, theirs).
    pub fn search_locks_verifiable(&self, limit: usize, cached: bool) -> Result<(Vec<Lock>, Vec<Lock>)> {
        if cached {
            if limit != 0 {
                return Err(Error::new("can't search cached locks when limit is set"));
            }
            let l: VerifiableList = self.read_cache("verifiable")?;
            return Ok((l.ours, l.theirs));
        }
        let (mut ours, mut theirs) = (vec![], vec![]);
        save_cached_locks(&[]);
        let mut cursor = String::new();
        loop {
            let (list, status) = match self.raw_verify(limit, &cursor) {
                Ok(x) => x,
                Err(e) => {
                    let st = status_of(&e);
                    if st == 404 || st == 501 {
                        return Err(e.go_wrap(Kind::NotImplemented, "Not implemented"));
                    }
                    if st == 403 {
                        return Err(e.auth());
                    }
                    return Err(e);
                }
            };
            if status == 404 || status == 501 {
                return Err(Error::new("Error").go_wrap(Kind::NotImplemented, "Not implemented"));
            }
            if status == 403 {
                return Err(Error::new("Error").auth());
            }
            if !list.message.is_empty() {
                if !list.request_id.is_empty() {
                    crate::trace!("Server Request ID: {}", list.request_id);
                }
                return Err(Error::new(format!("server error searching locks: {}", list.message)));
            }
            for l in list.ours {
                cache_add(&l);
                ours.push(l);
                if limit > 0 && ours.len() + theirs.len() >= limit {
                    return Ok((ours, theirs));
                }
            }
            for l in list.theirs {
                cache_add(&l);
                theirs.push(l);
                if limit > 0 && ours.len() + theirs.len() >= limit {
                    return Ok((ours, theirs));
                }
            }
            if list.next_cursor.is_empty() {
                break;
            }
            cursor = list.next_cursor;
        }
        if limit == 0 {
            self.write_cache("verifiable", &VerifiableList { ours: ours.clone(), theirs: theirs.clone(), ..Default::default() })?;
        }
        Ok((ours, theirs))
    }

    fn raw_verify(&self, limit: usize, cursor: &str) -> Result<(VerifiableList, u32)> {
        if let Some(t) = ssh_for("upload", &self.remote) {
            let conn = t.connection(0)?;
            let mut c = conn.lock().unwrap();
            let mut args = vec![];
            if let Some(r) = &self.remote_ref {
                args.push(format!("refname={}", r.refspec()));
            }
            if !cursor.is_empty() {
                args.push(format!("cursor={cursor}"));
            }
            if limit > 0 {
                args.push(format!("limit={limit}"));
            }
            c.send_message("list-lock", &args)?;
            let (status, args, lines) = c.read_status_with_lines()?;
            let (_, ours, theirs, next, message) = parse_ssh_list(status, &args, &lines)?;
            return Ok((VerifiableList { ours, theirs, next_cursor: next, message, request_id: String::new() }, status as u32));
        }
        let mut body = serde_json::Map::new();
        if let Some(r) = &self.remote_ref {
            let mut m = serde_json::Map::new();
            let rs = r.refspec();
            if !rs.is_empty() {
                m.insert("name".into(), rs.into());
            }
            body.insert("ref".into(), serde_json::Value::Object(m));
        }
        if !cursor.is_empty() {
            body.insert("cursor".into(), cursor.into());
        }
        if limit > 0 {
            body.insert("limit".into(), limit.into());
        }
        let (status, res) = api("POST", "upload", &self.remote, "locks/verify", Some(&serde_json::Value::Object(body)), "lfs.locks.verify", &[])?;
        let list = if status == 200 { crate::http::decode_json(&res).map_err(|e| e.into_error())? } else { VerifiableList::default() };
        Ok((list, status))
    }
}
