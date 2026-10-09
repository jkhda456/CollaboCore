//! git-lfs's configuration (config package): the OS environment, `git config` (with
//! .lfsconfig's safe keys below it), URL-specific keys, remotes, and the repository's paths.

use crate::errors::Result;
use crate::gitcmd::{self, Ref};
use crate::{subprocess, tools};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub fn bool_of(value: &str, def: bool) -> bool {
    if value.is_empty() {
        return def;
    }
    match value.to_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "t" => true,
        _ => false,
    }
}

pub fn int_of(value: &str, def: i64) -> i64 {
    if value.is_empty() {
        return def;
    }
    value.parse().unwrap_or(def)
}

/// The OS environment as config sees it.
pub struct OsEnv;

impl OsEnv {
    pub fn get(&self, k: &str) -> Option<String> {
        std::env::var_os(k).map(|v| v.to_string_lossy().into_owned())
    }
    pub fn bool(&self, k: &str, def: bool) -> bool {
        bool_of(&self.get(k).unwrap_or_default(), def)
    }
}

#[derive(Clone, Default, Debug)]
pub struct Extension {
    pub name: String,
    pub clean: String,
    pub smudge: String,
    pub priority: i64,
}

/// Values from `git config -l` lines (git_fetcher.go).
#[derive(Default)]
pub struct GitEnv {
    pub vals: BTreeMap<String, Vec<String>>,
    pub extensions: BTreeMap<String, Extension>,
    pub remotes: Vec<String>,
}

const SAFE_KEYS: &[&str] = &["lfs.allowincompletepush", "lfs.fetchexclude", "lfs.fetchinclude", "lfs.gitprotocol", "lfs.locksverify", "lfs.pushurl", "lfs.skipdownloaderrors", "lfs.url"];

pub struct Source {
    pub lines: Vec<String>,
    pub only_safe: bool,
}

pub fn case_fold_key(key: &str) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() < 3 {
        return key.to_lowercase();
    }
    let last = parts.len() - 1;
    format!("{}.{}.{}", parts[0].to_lowercase(), parts[1..last].join("."), parts[last].to_lowercase())
}

impl GitEnv {
    pub fn from_sources(sources: &[Source], show_warnings: bool) -> GitEnv {
        let mut g = GitEnv::default();
        let mut ignored = vec![];
        let mut uniq_remotes: Vec<String> = vec![];
        for src in sources {
            let mut uniq: BTreeMap<String, String> = BTreeMap::new();
            for line in &src.lines {
                let Some((key, val)) = line.split_once('=') else { continue };
                let mut allowed = !src.only_safe;
                if let Some(orig) = uniq.get(key) {
                    if show_warnings {
                        if let Some(prev) = g.vals.get(key).and_then(|v| v.last()) {
                            if prev != val && key.starts_with("lfs.") {
                                eprintln!("warning: These `git config` values clash:");
                                eprintln!("  git config {} = {}", tools::quote(orig), go_quote_list(&g.vals[key]));
                                eprintln!("  git config {} = {}", tools::quote(key), tools::quote(val));
                            }
                        }
                    }
                } else {
                    uniq.insert(key.to_string(), key.to_string());
                }
                let parts: Vec<&str> = key.split('.').collect();
                if parts.len() == 4 && parts[0] == "lfs" && parts[1] == "extension" {
                    let ext = g.extensions.entry(parts[2].to_string()).or_default();
                    ext.name = parts[2].to_string();
                    match parts[3] {
                        "clean" => {
                            if src.only_safe {
                                ignored.push(key.to_string());
                                continue;
                            }
                            ext.clean = val.to_string();
                        }
                        "smudge" => {
                            if src.only_safe {
                                ignored.push(key.to_string());
                                continue;
                            }
                            ext.smudge = val.to_string();
                        }
                        "priority" => {
                            allowed = true;
                            if let Ok(p) = val.parse::<i64>() {
                                if p >= 0 {
                                    ext.priority = p;
                                }
                            }
                        }
                        _ => {}
                    }
                } else if parts.len() > 1 && parts[0] == "remote" {
                    if src.only_safe && parts.len() == 3 && parts[2] != "lfsurl" {
                        ignored.push(key.to_string());
                        continue;
                    }
                    allowed = true;
                    let remote = parts[1..parts.len() - 1].join(".");
                    if !uniq_remotes.contains(&remote) {
                        uniq_remotes.push(remote);
                    }
                } else if parts.len() > 2 && parts[parts.len() - 1] == "access" {
                    allowed = true;
                }
                if !allowed && !SAFE_KEYS.contains(&key) {
                    ignored.push(key.to_string());
                    continue;
                }
                g.vals.entry(key.to_string()).or_default().push(val.to_string());
            }
        }
        if !ignored.is_empty() {
            eprint!("warning: These unsafe '.lfsconfig' keys were ignored:\n\n");
            for k in &ignored {
                eprintln!("  {k}");
            }
        }
        g.remotes = uniq_remotes;
        g
    }

    pub fn get_all(&self, key: &str) -> Vec<String> {
        self.vals.get(&case_fold_key(key)).cloned().unwrap_or_default()
    }
    pub fn get(&self, key: &str) -> Option<String> {
        self.vals.get(&case_fold_key(key)).and_then(|v| v.last().cloned())
    }
    pub fn bool(&self, k: &str, def: bool) -> bool {
        bool_of(&self.get(k).unwrap_or_default(), def)
    }
    pub fn int(&self, k: &str, def: i64) -> i64 {
        int_of(&self.get(k).unwrap_or_default(), def)
    }
}

fn go_quote_list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| tools::quote(s)).collect::<Vec<_>>().join(" "))
}

pub fn parse_lines(out: &str, only_safe: bool) -> Source {
    Source { lines: out.split('\n').map(str::to_string).collect(), only_safe }
}

pub struct Config {
    pub os: OsEnv,
    git: OnceLock<GitEnv>,
    pub show_warnings: std::sync::atomic::AtomicBool,
    dirs: OnceLock<(String, String)>,
    current_ref: OnceLock<Ref>,
    remote: Mutex<Option<String>>,
    push_remote: Mutex<Option<String>>,
    fs: OnceLock<crate::fs::Filesystem>,
    mask: OnceLock<u32>,
}

static CFG: OnceLock<Config> = OnceLock::new();

pub fn cfg() -> &'static Config {
    CFG.get_or_init(Config::new)
}

impl Config {
    fn new() -> Config {
        Config {
            os: OsEnv,
            git: OnceLock::new(),
            show_warnings: std::sync::atomic::AtomicBool::new(false),
            dirs: OnceLock::new(),
            current_ref: OnceLock::new(),
            remote: Mutex::new(None),
            push_remote: Mutex::new(None),
            fs: OnceLock::new(),
            mask: OnceLock::new(),
        }
    }

    pub fn git(&self) -> &GitEnv {
        self.git.get_or_init(|| {
            let sources = match self.sources() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error reading `git config`: {e}");
                    vec![]
                }
            };
            GitEnv::from_sources(&sources, self.show_warnings.load(std::sync::atomic::Ordering::Relaxed))
        })
    }

    /// git.Configuration.Sources: .lfsconfig (from the work tree, or the index, or HEAD), then
    /// `git config -l`.
    fn sources(&self) -> Result<Vec<Source>> {
        let gitconfig = parse_lines(&git_config(&["-l"])?, false);
        let mut out = vec![];
        if let Ok(bare) = gitcmd::is_bare() {
            let mut file: Option<Source> = None;
            if !bare {
                let wd = self.local_working_dir();
                let fname = if wd.is_empty() { ".lfsconfig".to_string() } else { format!("{wd}/.lfsconfig") };
                match std::fs::metadata(&fname) {
                    Ok(_) => file = Some(parse_lines(&git_config(&["-l", "-f", &fname])?, true)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        file = git_config(&["-l", "--blob", ":.lfsconfig"]).ok().map(|o| parse_lines(&o, true));
                    }
                    Err(e) => return Err(tools::path_err("stat", &fname, &e)),
                }
            }
            if file.is_none() {
                file = git_config(&["-l", "--blob", "HEAD:.lfsconfig"]).ok().map(|o| parse_lines(&o, true));
            }
            if let Some(f) = file {
                out.push(f);
            }
        }
        out.push(gitconfig);
        Ok(out)
    }

    fn dirs(&self) -> &(String, String) {
        self.dirs.get_or_init(|| match gitcmd::git_and_root_dirs() {
            Ok((g, w)) => (tools::resolve_symlinks(&g), tools::resolve_symlinks(&w)),
            Err(e) => {
                crate::trace!("Error running 'git rev-parse': {}", e);
                if e.exit_status() != Some(128) {
                    eprintln!("Error: {e}");
                }
                (String::new(), String::new())
            }
        })
    }

    pub fn local_git_dir(&self) -> String {
        self.dirs().0.clone()
    }
    pub fn local_working_dir(&self) -> String {
        self.dirs().1.clone()
    }
    pub fn in_repo(&self) -> bool {
        !self.local_git_dir().is_empty()
    }

    pub fn current_ref(&self) -> Ref {
        self.current_ref
            .get_or_init(|| match gitcmd::resolve_ref("HEAD") {
                Ok(r) => r,
                Err(e) => {
                    crate::trace!("Error loading current ref: {}", e);
                    Ref::default()
                }
            })
            .clone()
    }

    pub fn remotes(&self) -> Vec<String> {
        self.git().remotes.clone()
    }

    pub fn remote(&self) -> String {
        let r = self.current_ref();
        let mut cur = self.remote.lock().unwrap();
        if cur.is_none() {
            let g = self.git();
            *cur = Some(if let (false, Some(v)) = (r.name.is_empty(), g.get(&format!("branch.{}.remote", r.name))) {
                v
            } else if let Some(v) = g.get("remote.lfsdefault") {
                v
            } else if g.remotes.len() == 1 {
                g.remotes[0].clone()
            } else {
                "origin".to_string()
            });
        }
        cur.clone().unwrap()
    }

    pub fn push_remote(&self) -> String {
        let r = self.current_ref();
        {
            let cur = self.push_remote.lock().unwrap();
            if let Some(v) = cur.as_ref() {
                return v.clone();
            }
        }
        let g = self.git();
        let v = g.get(&format!("branch.{}.pushRemote", r.name)).or_else(|| g.get("remote.lfspushdefault")).or_else(|| g.get("remote.pushDefault")).unwrap_or_else(|| self.remote());
        *self.push_remote.lock().unwrap() = Some(v.clone());
        v
    }

    pub fn is_default_remote(&self) -> bool {
        self.remote() == "origin"
    }

    pub fn set_remote(&self, r: &str) {
        *self.remote.lock().unwrap() = Some(r.to_string());
    }
    pub fn set_push_remote(&self, r: &str) {
        *self.push_remote.lock().unwrap() = Some(r.to_string());
    }

    pub fn set_valid_remote(&self, name: &str) -> Result<()> {
        if gitcmd::validate_remote(name).is_err() {
            let n = gitcmd::rewrite_local_path_as_url(name);
            gitcmd::validate_remote(&n)?;
        }
        self.set_remote(name);
        Ok(())
    }
    pub fn set_valid_push_remote(&self, name: &str) -> Result<()> {
        if gitcmd::validate_remote(name).is_err() {
            let n = gitcmd::rewrite_local_path_as_url(name);
            gitcmd::validate_remote(&n)?;
        }
        self.set_push_remote(name);
        Ok(())
    }

    pub fn extensions(&self) -> &BTreeMap<String, Extension> {
        &self.git().extensions
    }

    pub fn sorted_extensions(&self) -> Result<Vec<Extension>> {
        let mut by: BTreeMap<i64, Extension> = BTreeMap::new();
        for (n, e) in self.extensions() {
            if by.contains_key(&e.priority) {
                return Err(crate::errors::Error::new(format!("duplicate priority {} on {}", e.priority, n)));
            }
            by.insert(e.priority, e.clone());
        }
        Ok(by.into_values().collect())
    }

    fn mask(&self) -> u32 {
        *self.mask.get_or_init(|| {
            let val = match self.git().get("core.sharedrepository") {
                None => "umask".to_string(),
                Some(v) if bool_of(&v, false) => "group".to_string(),
                Some(v) => v,
            };
            match val.to_lowercase().as_str() {
                "group" | "true" | "1" => 0o007,
                "all" | "world" | "everybody" | "2" => 0o002,
                "umask" | "false" | "0" => tools::umask(),
                v => match u32::from_str_radix(v, 8) {
                    Ok(mode) => 0o666 & !mode,
                    Err(_) => tools::umask(),
                },
            }
        })
    }

    pub fn repository_permissions(&self, executable: bool) -> u32 {
        let p = 0o666 & !self.mask();
        if executable {
            tools::executable_permissions(p)
        } else {
            p
        }
    }

    pub fn filesystem(&self) -> &crate::fs::Filesystem {
        self.fs.get_or_init(|| {
            let lfsdir = self.git().get("lfs.storage").unwrap_or_default();
            crate::fs::Filesystem::new(&self.local_git_dir(), &lfsdir, self.repository_permissions(false))
        })
    }

    pub fn lfs_storage_dir(&self) -> String {
        self.filesystem().lfs_storage_dir.clone()
    }
    pub fn lfs_object_dir(&self) -> String {
        self.filesystem().lfs_object_dir()
    }
    pub fn temp_dir(&self) -> String {
        self.filesystem().temp_dir()
    }
    pub fn local_log_dir(&self) -> String {
        self.filesystem().log_dir()
    }
    pub fn local_git_storage_dir(&self) -> String {
        self.filesystem().git_storage_dir.clone()
    }

    pub fn hook_dir(&self) -> Result<String> {
        if gitcmd::is_git_version_at_least("2.9.0") {
            if let Some(hp) = self.git().get("core.hooksPath") {
                let p = tools::expand_path(&hp)?;
                if p.starts_with('/') {
                    return Ok(p);
                }
                return Ok(format!("{}/{}", self.local_working_dir(), p));
            }
        }
        Ok(format!("{}/hooks", self.local_git_storage_dir()))
    }

    pub fn fetch_include_paths(&self) -> Vec<String> {
        tools::clean_paths(&self.git().get("lfs.fetchinclude").unwrap_or_default(), ",")
    }
    pub fn fetch_exclude_paths(&self) -> Vec<String> {
        tools::clean_paths(&self.git().get("lfs.fetchexclude").unwrap_or_default(), ",")
    }
    pub fn basic_transfers_only(&self) -> bool {
        self.git().bool("lfs.basictransfersonly", false)
    }
    pub fn tus_transfers_allowed(&self) -> bool {
        self.git().bool("lfs.tustransfers", false)
    }
    pub fn transfer_batch_size(&self) -> i64 {
        self.git().int("lfs.transfer.batchSize", 0)
    }
    pub fn skip_download_errors(&self) -> bool {
        self.os.bool("GIT_LFS_SKIP_DOWNLOAD_ERRORS", false) || self.git().bool("lfs.skipdownloaderrors", false)
    }
    pub fn set_lockable_files_read_only(&self) -> bool {
        self.os.bool("GIT_LFS_SET_LOCKABLE_READONLY", true) && self.git().bool("lfs.setlockablereadonly", true)
    }
    pub fn force_progress(&self) -> bool {
        self.os.bool("GIT_LFS_FORCE_PROGRESS", false) || self.git().bool("lfs.forceprogress", false)
    }
    pub fn auto_detect_remote_enabled(&self) -> bool {
        self.git().bool("lfs.remote.autodetect", false)
    }
    pub fn search_all_remotes_enabled(&self) -> bool {
        self.git().bool("lfs.remote.searchall", false)
    }

    /// The user (`committer`/`author`) git would use.
    pub fn user_data(&self, kind: &str) -> (String, String) {
        let up = kind.to_uppercase();
        let name = self.os.get(&format!("GIT_{up}_NAME")).or_else(|| self.git().get("user.name")).unwrap_or_default();
        let email = self.os.get(&format!("GIT_{up}_EMAIL")).or_else(|| self.git().get("user.email")).or_else(|| self.os.get("EMAIL")).unwrap_or_default();
        let f = |s: String| s.chars().filter(|c| !matches!(c, '<' | '>' | '\n')).collect::<String>();
        (f(name), f(email))
    }

    // git config reads and writes in a scope.
    pub fn find_global(&self, key: &str) -> String {
        git_config(&["--global", key]).unwrap_or_default()
    }
    pub fn find_system(&self, key: &str) -> String {
        git_config(&["--system", key]).unwrap_or_default()
    }
    pub fn find_local(&self, key: &str) -> String {
        git_config(&["--local", key]).unwrap_or_default()
    }
    pub fn find_worktree(&self, key: &str) -> String {
        git_config(&["--worktree", key]).unwrap_or_default()
    }
    pub fn find_file(&self, file: &str, key: &str) -> String {
        git_config(&["--file", file, key]).unwrap_or_default()
    }
    pub fn find(&self, key: &str) -> String {
        git_config(&[key]).unwrap_or_default()
    }
    pub fn set_local(&self, key: &str, val: &str) -> Result<String> {
        git_config(&["--replace-all", key, val])
    }
    pub fn set_global(&self, key: &str, val: &str) -> Result<String> {
        git_config(&["--global", "--replace-all", key, val])
    }
    pub fn set_system(&self, key: &str, val: &str) -> Result<String> {
        git_config(&["--system", "--replace-all", key, val])
    }
    pub fn set_worktree(&self, key: &str, val: &str) -> Result<String> {
        git_config(&["--worktree", "--replace-all", key, val])
    }
    pub fn set_file(&self, file: &str, key: &str, val: &str) -> Result<String> {
        git_config(&["--file", file, "--replace-all", key, val])
    }
    pub fn unset_local_key(&self, key: &str) -> Result<String> {
        git_config(&["--unset", key])
    }
    pub fn unset_section(&self, scope: &[&str], section: &str) -> Result<String> {
        let mut a: Vec<&str> = scope.to_vec();
        a.push("--remove-section");
        a.push(section);
        git_config(&a)
    }
}

/// `git config --includes ARGS` (in the git dir when one is known, as git-lfs does).
pub fn git_config(args: &[&str]) -> Result<String> {
    let mut a = vec!["config", "--includes"];
    a.extend_from_slice(args);
    subprocess::output("git", &a, None)
}

/// URL-specific configuration (`http.<url>.key`), as url_config.go matches it.
pub fn url_get(prefix: &str, rawurl: &str, key: &str) -> Option<String> {
    url_get_all(prefix, rawurl, key).last().cloned()
}

pub fn url_bool(prefix: &str, rawurl: &str, key: &str, def: bool) -> bool {
    bool_of(&url_get(prefix, rawurl, key).unwrap_or_default(), def)
}

pub fn url_get_all(prefix: &str, rawurl: &str, key: &str) -> Vec<String> {
    let key = key.to_lowercase();
    let prefix = prefix.to_lowercase();
    let g = cfg().git();
    let v = url_match_all(g, &prefix, rawurl, &key);
    if !v.is_empty() {
        return v;
    }
    g.get_all(&format!("{prefix}.{key}"))
}

fn url_match_all(g: &GitEnv, prefix: &str, rawurl: &str, key: &str) -> Vec<String> {
    let Ok(search) = crate::gourl::parse(rawurl) else { return vec![] };
    let mut best: (Option<String>, usize, usize, usize) = (None, 0, 0, 0);
    let pre = format!("{prefix}.");
    let suf = format!(".{key}");
    for k in g.vals.keys() {
        let Some(mid) = k.strip_prefix(&pre).and_then(|r| r.strip_suffix(&suf)) else { continue };
        if mid.is_empty() || mid.contains(char::is_whitespace) {
            continue;
        }
        let Ok(cu) = crate::gourl::parse(mid) else { continue };
        if search.scheme != cu.scheme {
            continue;
        }
        let host = compare_hosts(search.hostname(), cu.hostname());
        if host == 0 || host < best.1 {
            continue;
        }
        if port_for(&search) != port_for(&cu) {
            continue;
        }
        let path = compare_paths(&search.path, &cu.path);
        if path == 0 {
            continue;
        }
        let mut user = 0;
        if let Some(cn) = cu.username() {
            match search.username() {
                Some(sn) if sn == cn => user = 1,
                _ => continue,
            }
        }
        if host > best.1 || path > best.2 || (path == best.2 && user > best.3) {
            best = (Some(k.clone()), host, path, user);
        }
    }
    match best.0 {
        Some(k) => g.vals.get(&k).cloned().unwrap_or_default(),
        None => vec![],
    }
}

fn port_for(u: &crate::gourl::Url) -> String {
    let p = u.port();
    if !p.is_empty() {
        return p.to_string();
    }
    match u.scheme.as_str() {
        "http" => "80",
        "https" => "443",
        "ssh" => "22",
        _ => "",
    }
    .to_string()
}

fn compare_hosts(search: &str, config: &str) -> usize {
    let s: Vec<&str> = search.split('.').collect();
    let c: Vec<&str> = config.split('.').collect();
    if s.len() != c.len() {
        return 0;
    }
    let mut score = s.len() + 1;
    for (i, sub) in s.iter().enumerate() {
        if c[i] == "*" {
            score -= 1;
            continue;
        }
        if *sub != c[i] {
            return 0;
        }
    }
    score
}

fn compare_paths(search: &str, config: &str) -> usize {
    let s: Vec<&str> = search.split('/').filter(|x| !x.is_empty()).collect();
    let c: Vec<&str> = config.split('/').filter(|x| !x.is_empty()).collect();
    if s.len() < c.len() {
        return 0;
    }
    let mut score = 1;
    for (i, el) in c.iter().enumerate() {
        let se = s[i];
        if *el == se {
            score += 2;
            continue;
        }
        if is_default_lfs_url(se, &s, i + 1) && &se[..se.len() - 4] == *el {
            score += 1;
            continue;
        }
        return 0;
    }
    score
}

fn is_default_lfs_url(path: &str, parts: &[&str], index: usize) -> bool {
    if path.len() < 5 || !path.ends_with(".git") {
        return false;
    }
    if index + 2 > parts.len() {
        return false;
    }
    parts[index] == "info" && parts[index + 1] == "lfs"
}

