//! `git lfs install`, `uninstall`, `update`: the filter configuration and the hooks.

use super::*;
use crate::cli::{flag, K};
use crate::errors::Result;

struct FilterOptions {
    force: bool,
    file: String,
    local: bool,
    worktree: bool,
    system: bool,
    skip_smudge: bool,
}

fn filter_attribute(skip: bool) -> (Vec<(&'static str, &'static str)>, Vec<(&'static str, Vec<&'static str>)>) {
    if skip {
        (
            vec![("clean", "git-lfs clean -- %f"), ("smudge", "git-lfs smudge --skip -- %f"), ("process", "git-lfs filter-process --skip"), ("required", "true")],
            vec![
                ("clean", vec!["git-lfs clean -- %f"]),
                ("smudge", vec!["git-lfs smudge %f", "git-lfs smudge --skip %f", "git-lfs smudge -- %f"]),
                ("process", vec!["git-lfs filter", "git-lfs filter --skip", "git-lfs filter-process"]),
            ],
        )
    } else {
        (
            vec![("clean", "git-lfs clean -- %f"), ("smudge", "git-lfs smudge -- %f"), ("process", "git-lfs filter-process"), ("required", "true")],
            vec![
                ("clean", vec!["git-lfs clean %f"]),
                ("smudge", vec!["git-lfs smudge %f", "git-lfs smudge --skip %f", "git-lfs smudge --skip -- %f"]),
                ("process", vec!["git-lfs filter", "git-lfs filter --skip", "git-lfs filter-process --skip"]),
            ],
        )
    }
}

impl FilterOptions {
    fn find(&self, key: &str) -> String {
        let c = cfg();
        if self.local {
            c.find_local(key)
        } else if self.worktree {
            c.find_worktree(key)
        } else if self.system {
            c.find_system(key)
        } else if !self.file.is_empty() {
            c.find_file(&self.file, key)
        } else {
            c.find_global(key)
        }
    }
    fn set(&self, key: &str, val: &str) -> Result<String> {
        let c = cfg();
        if self.local {
            c.set_local(key, val)
        } else if self.worktree {
            c.set_worktree(key, val)
        } else if self.system {
            c.set_system(key, val)
        } else if !self.file.is_empty() {
            c.set_file(&self.file, key, val)
        } else {
            c.set_global(key, val)
        }
    }
    fn install(&self) -> Result<()> {
        let (props, upgradeables) = filter_attribute(self.skip_smudge);
        for (k, v) in props {
            let key = format!("filter.lfs.{k}");
            let ups = upgradeables.iter().find(|(n, _)| *n == k).map(|(_, u)| u.clone()).unwrap_or_default();
            let current = self.find(&key);
            if self.force || current.is_empty() || ups.contains(&current.as_str()) {
                self.set(&key, v)?;
            } else if current != v {
                return Err(Error::new(format!("the {} attribute should be {} but is {}", tools::quote(&key), tools::quote(v), tools::quote(&current))));
            }
        }
        Ok(())
    }
    fn uninstall(&self) -> Result<()> {
        let c = cfg();
        let scope: Vec<&str> = if self.local {
            vec!["--local"]
        } else if self.worktree {
            vec!["--worktree"]
        } else if self.system {
            vec!["--system"]
        } else if !self.file.is_empty() {
            vec!["--file", &self.file]
        } else {
            vec!["--global"]
        };
        c.unset_section(&scope, "filter.lfs")?;
        for k in ["clean", "smudge", "process", "required"] {
            let name = format!("filter.lfs.{k}");
            if !c.find(&name).is_empty() {
                return Err(Error::new(format!("some filter configuration was not removed (found {name})")));
            }
        }
        Ok(())
    }
}

fn options(p: &Parsed) -> FilterOptions {
    require_git_version();
    let local = p.bool("local");
    let worktree = p.bool("worktree");
    if local || worktree {
        setup_repository();
    }
    let n = [local, worktree, p.bool("system"), !p.str("file").is_empty()].iter().filter(|b| **b).count();
    if n > 1 {
        exit("Only one of the --local, --system, --worktree, and --file options can be specified.");
    }
    let uid = unsafe { libc::geteuid() };
    if p.bool("system") && uid != 0 {
        print("warning: current user is not root/admin, system install is likely to fail.");
    }
    FilterOptions { force: p.bool("force"), file: p.str("file"), local, worktree, system: p.bool("system"), skip_smudge: p.bool("skip-smudge") }
}

// Hooks (lfs/hook.go)

const HOOK_BASE: &str = "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { printf >&2 \"\\n%s\\n\\n\" \"This repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting the '{{Command}}' file in the hooks directory (set by 'core.hookspath'; usually '.git/hooks').\"; exit 2; }\ngit lfs {{Command}} \"$@\"";
const HOOK_OLD: &str = "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"\\nThis repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting the '{{Command}}' file in the hooks directory (set by 'core.hookspath'; usually '.git/hooks').\\n\"; exit 2; }\ngit lfs {{Command}} \"$@\"";
const HOOK_OLD2: &str = "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"\\nThis repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting '.git/hooks/{{Command}}'.\\n\"; exit 2; }\ngit lfs {{Command}} \"$@\"";
const HOOK_OLD3: &str = "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"\\nThis repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting .git/hooks/{{Command}}.\\n\"; exit 2; }\ngit lfs {{Command}} \"$@\"";

pub struct Hook {
    pub typ: &'static str,
    pub contents: String,
    pub dir: String,
    upgradeables: Vec<String>,
}

pub fn load_hooks(dir: &str) -> Vec<Hook> {
    let mk = |t: &'static str, ups: Vec<&str>| Hook {
        typ: t,
        contents: HOOK_BASE.replace("{{Command}}", t),
        dir: dir.to_string(),
        upgradeables: ups.iter().map(|u| u.replace("{{Command}}", t)).collect(),
    };
    vec![
        mk(
            "pre-push",
            vec![
                "#!/bin/sh\ngit lfs push --stdin $*",
                "#!/bin/sh\ngit lfs push --stdin \"$@\"",
                "#!/bin/sh\ngit lfs pre-push \"$@\"",
                "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"\\nThis repository has been set up with Git LFS but Git LFS is not installed.\\n\"; exit 0; }\ngit lfs pre-push \"$@\"",
                "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"\\nThis repository has been set up with Git LFS but Git LFS is not installed.\\n\"; exit 2; }\ngit lfs pre-push \"$@\"",
                HOOK_OLD,
                HOOK_OLD2,
                HOOK_OLD3,
            ],
        ),
        mk("post-checkout", vec![HOOK_OLD, HOOK_OLD2, HOOK_OLD3]),
        mk("post-commit", vec![HOOK_OLD, HOOK_OLD2, HOOK_OLD3]),
        mk("post-merge", vec![HOOK_OLD, HOOK_OLD2, HOOK_OLD3]),
    ]
}

impl Hook {
    pub fn path(&self) -> String {
        format!("{}/{}", self.dir, self.typ)
    }
    fn write(&self) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let p = self.path();
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(&p).map_err(|e| tools::path_err("open", &p, &e))?;
        f.write_all(format!("{}\n", self.contents).as_bytes())?;
        Ok(())
    }
    /// (upgradable, matches): an error when it is someone else's hook.
    fn matches_current(&self) -> Result<(bool, bool)> {
        let p = self.path();
        let data = std::fs::read(&p).map_err(|e| tools::path_err("open", &p, &e))?;
        let data = &data[..data.len().min(1024)];
        let contents = tools::undent(&String::from_utf8_lossy(data)).trim().to_string();
        if contents == self.contents {
            return Ok((true, true));
        }
        if contents.is_empty() || self.upgradeables.contains(&contents) {
            return Ok((true, false));
        }
        Err(Error::new(format!("Hook already exists: {}\n\n{}\n", self.typ, tools::indent(&contents))))
    }
    pub fn install(&self, force: bool) -> Result<()> {
        crate::trace!("Install hook: {}, force={}, path={}", self.typ, force, self.path());
        tools::mkdir_all(&self.dir, cfg().repository_permissions(false)).map_err(|e| tools::path_err("mkdir", &self.dir, &e))?;
        if std::fs::metadata(self.path()).is_ok() && !force {
            let (up, m) = self.matches_current()?;
            if !up || m {
                return Ok(());
            }
        }
        self.write()
    }
    pub fn uninstall(&self) -> Result<()> {
        let (up, _) = match self.matches_current() {
            Ok(x) => x,
            Err(e) => {
                if std::fs::metadata(self.path()).is_err() {
                    return Err(e);
                }
                return Err(e);
            }
        };
        if !up {
            return Ok(());
        }
        let _ = std::fs::remove_file(self.path());
        Ok(())
    }
}

pub fn install_hooks(force: bool) -> Result<()> {
    let dir = cfg().hook_dir()?;
    for h in load_hooks(&dir) {
        h.install(force)?;
    }
    Ok(())
}

fn uninstall_hooks() -> Result<()> {
    if !cfg().in_repo() {
        return Err(Error::new("Not in a Git repository"));
    }
    let dir = cfg().hook_dir()?;
    for h in load_hooks(&dir) {
        h.uninstall()?;
    }
    Ok(())
}

fn hook_install_steps() -> String {
    let dir = match cfg().hook_dir() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    let wd = format!("{}/", cfg().local_working_dir());
    let shown = dir.strip_prefix(&wd).unwrap_or(&dir).to_string();
    load_hooks(&dir).iter().map(|h| format!("Add the following to '{}/{}':\n\n{}", shown, h.typ, tools::indent(&h.contents))).collect::<Vec<_>>().join("\n\n")
}

pub fn update(force: bool, manual: bool) {
    require_git_version();
    setup_repository();
    let g = cfg().git();
    let re = regex::Regex::new(r"\Alfs\.(.*)\.access\z").unwrap();
    for key in g.vals.keys() {
        let Some(m) = re.captures(key) else { continue };
        let value = g.get(key).unwrap_or_default();
        match value.as_str() {
            "basic" => {}
            "private" => {
                let _ = cfg().set_local(key, "basic");
                print(&format!("Updated {} access from {} to {}.", &m[1], value, "basic"));
            }
            _ => {
                let _ = cfg().unset_local_key(key);
                print(&format!("Removed invalid {} access of {}.", &m[1], value));
            }
        }
    }
    if force && manual {
        exit("You cannot use --force and --manual options together");
    }
    if manual {
        print(&hook_install_steps());
    } else if let Err(e) = install_hooks(force) {
        error(&e.to_string());
        exit("To resolve this, either:\n  1: run `git lfs update --manual` for instructions on how to merge hooks.\n  2: run `git lfs update --force` to overwrite your hook.");
    } else {
        print("Updated Git hooks.");
    }
}

fn install_run(p: &Parsed) {
    let o = options(p);
    if let Err(e) = o.install() {
        print(&format!("warning: {e}"));
        print("Run `git lfs install --force` to reset Git configuration.");
        exit_with_code(2);
    }
    if !p.bool("skip-repo") && (o.local || o.worktree || cfg().in_repo()) {
        update(p.bool("force"), p.bool("manual"));
    }
    print("Git LFS initialized.");
}

fn install_hooks_run(p: &Parsed) {
    update(p.bool("force"), p.bool("manual"));
}

fn uninstall_run(p: &Parsed) {
    let o = options(p);
    if let Err(e) = o.uninstall() {
        print(&format!("warning: {e}"));
    }
    if !p.bool("skip-repo") && (o.local || o.worktree || cfg().in_repo()) {
        uninstall_hooks_run(p);
    }
    if o.system {
        print("System Git LFS configuration has been removed.");
    } else if !(o.local || o.worktree) {
        print("Global Git LFS configuration has been removed.");
    }
}

fn uninstall_hooks_run(_p: &Parsed) {
    if let Err(e) = uninstall_hooks() {
        error(&e.to_string());
    }
    print("Hooks for this repository have been removed.");
}

fn update_run(p: &Parsed) {
    update(p.bool("force"), p.bool("manual"));
}

pub fn commands() -> Vec<Cmd> {
    let worktree = gitcmd::is_git_version_at_least("2.20.0");
    let mut iflags = vec![
        flag("force", Some('f'), K::Bool),
        flag("local", Some('l'), K::Bool),
        flag("file", None, K::Str),
        flag("system", None, K::Bool),
        flag("skip-smudge", Some('s'), K::Bool),
        flag("skip-repo", None, K::Bool),
        flag("manual", Some('m'), K::Bool),
    ];
    let mut uflags = vec![flag("local", Some('l'), K::Bool), flag("file", None, K::Str), flag("system", None, K::Bool), flag("skip-repo", None, K::Bool)];
    if worktree {
        iflags.push(flag("worktree", Some('w'), K::Bool));
        uflags.push(flag("worktree", Some('w'), K::Bool));
    }
    let mut install = cmd("install", install_run, iflags.clone());
    install.subs.push(cmd("hooks", install_hooks_run, iflags));
    let mut uninstall = cmd("uninstall", uninstall_run, uflags.clone());
    uninstall.subs.push(cmd("hooks", uninstall_hooks_run, uflags));
    vec![install, uninstall, cmd("update", update_run, vec![flag("force", Some('f'), K::Bool), flag("manual", Some('m'), K::Bool)])]
}
