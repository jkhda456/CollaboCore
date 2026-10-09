//! standalone-file (commands/command_standalone_file.go, lfshttp/standalone): the custom
//! transfer agent for file:// remotes, copying (or linking) objects between this repository's
//! store and the other repository's.

use super::{cmd, exit_with_error, Cmd};
use crate::cli::Parsed;
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::fs::Filesystem;
use crate::tools;
use std::io::{BufRead, Write};

#[derive(serde::Deserialize, Default)]
struct Input {
    #[serde(default)]
    event: String,
    #[serde(default)]
    operation: String,
    #[serde(default)]
    remote: String,
    #[serde(default)]
    oid: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    path: String,
}

struct Handler {
    remote_path: String,
    remote_fs: Filesystem,
    tempdir: String,
}

fn file_url_from_remote(name: &str, direction: &str) -> Option<crate::gourl::Url> {
    if name.starts_with("file://") {
        if let Ok(u) = crate::gourl::parse(name) {
            return Some(u);
        }
    }
    for r in cfg().remotes() {
        if r != name {
            continue;
        }
        let ep = crate::endpoint::endpoint(direction, &r);
        if !ep.url.starts_with("file://") {
            return None;
        }
        return crate::gourl::parse(&ep.url).ok();
    }
    None
}

/// The git dir of the repository at a path (with no GIT_* variables in the way).
fn git_dir_at(path: &str) -> Result<String> {
    let path = if path.rsplit('/').next() == Some(".git") { std::path::Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default() } else { path.to_string() };
    let mut c = crate::subprocess::command("git", &["rev-parse", "--git-dir"]);
    c.current_dir(&path);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    let out = c.output().map_err(|e| Error::from(e).wrap("failed to find `git rev-parse --git-dir`"))?;
    if !out.status.success() {
        if !out.stderr.is_empty() {
            return Err(Error::new(format!("failed to call `git rev-parse --git-dir`: {}", String::from_utf8_lossy(&out.stderr))));
        }
        return Err(Error::new(crate::subprocess::exit_text(&out.status)).wrap("failed to call `git rev-parse --git-dir`"));
    }
    let gd = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
    let abs = if gd.starts_with('/') { gd } else { tools::clean_str(&format!("{path}/{gd}")) };
    let abs = if abs.starts_with('/') { abs } else { tools::abs(&abs).display().to_string() };
    tools::canonicalize_system_path(&abs).map(|p| p.display().to_string()).map_err(Error::from)
}

fn new_handler(msg: &Input) -> Result<Handler> {
    let Some(u) = file_url_from_remote(&msg.remote, &msg.operation) else { return Err(Error::new("no valid file:// URLs found")) };
    let path = u.path.clone();
    let gitdir = git_dir_at(&path)?;
    let storage = crate::subprocess::command("git", &["config", "lfs.storage"]).current_dir(&gitdir).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let remote_fs = Filesystem::new(&gitdir, &storage, cfg().repository_permissions(false));
    let base = cfg().temp_dir();
    let mut tempdir = String::new();
    for i in 0..1000u32 {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() ^ (std::process::id() << 10) ^ i;
        let d = format!("{base}/lfs-standalone-file-{n}");
        if std::fs::create_dir(&d).is_ok() {
            tempdir = d;
            break;
        }
    }
    crate::trace!("using {} as remote git directory", tools::quote(&gitdir));
    Ok(Handler { remote_path: path, remote_fs, tempdir })
}

#[derive(serde::Serialize)]
struct Complete {
    event: &'static str,
    oid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

impl Handler {
    fn upload(&self, oid: &str, size: i64, path: &str) -> Result<String> {
        if self.remote_fs.object_exists(oid, size) {
            return Ok(String::new());
        }
        let dest = self.remote_fs.object_path(oid)?;
        crate::gitfilter::link_or_copy(path, &dest)?;
        Ok(String::new())
    }

    fn download(&self, oid: &str, size: i64) -> Result<String> {
        if !self.remote_fs.object_exists(oid, size) {
            crate::trace!("missing object in {} ({})", tools::quote(&self.remote_path), oid);
            return Err(Error::new(format!("remote missing object {oid}")));
        }
        let src = self.remote_fs.object_path(oid)?;
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        let path = format!("{}/download{}", self.tempdir, n);
        crate::gitfilter::link_or_copy(&src, &path)?;
        Ok(path)
    }
}

fn respond(out: &mut impl Write, oid: &str, r: Result<String>) {
    let (path, error) = match r {
        Ok(p) => (p, None),
        Err(e) => (String::new(), Some(serde_json::json!({"message": e.to_string()}))),
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&Complete { event: "complete", oid: oid.to_string(), path, error }).unwrap());
    let _ = out.flush();
}

fn process() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut handler: Option<Handler> = None;
    let mut event_err = None;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Error::from(e).wrap("error reading input"))?;
        let msg: Input = serde_json::from_str(&line).map_err(|e| Error::new(e.to_string()).wrap("error decoding JSON"))?;
        if handler.is_none() {
            match new_handler(&msg) {
                Ok(h) => handler = Some(h),
                Err(e) => {
                    let e = e.wrap("error creating handler");
                    let _ = writeln!(out, "{}", serde_json::json!({"error": {"message": e.to_string()}}));
                    return Err(e);
                }
            }
        }
        let h = handler.as_ref().unwrap();
        match msg.event.as_str() {
            "init" => {
                let _ = writeln!(out, "{{}}");
                let _ = out.flush();
            }
            "upload" => respond(&mut out, &msg.oid, h.upload(&msg.oid, msg.size, &msg.path)),
            "download" => respond(&mut out, &msg.oid, h.download(&msg.oid, msg.size)),
            "terminate" => break,
            e => {
                event_err = Some(Error::new(format!("unknown event {}", tools::quote(e))));
                break;
            }
        }
    }
    if let Some(h) = handler {
        let _ = std::fs::remove_dir_all(&h.tempdir);
    }
    match event_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn standalone_file(_p: &Parsed) {
    if let Err(e) = process() {
        exit_with_error(&e);
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd("standalone-file", standalone_file, vec![])]
}
