//! clone (commands/command_clone.go, deprecated): `git clone` without the LFS filters, then
//! a pull (or a fetch, for a bare or not checked out clone).

use super::{exit, exit_with_error, setup_repository, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::gitcmd;
use crate::tools;

const BOOLS: [(&str, Option<char>); 20] = [
    ("local", Some('l')),
    ("shared", Some('s')),
    ("no-hardlinks", None),
    ("quiet", Some('q')),
    ("no-checkout", Some('n')),
    ("progress", None),
    ("bare", None),
    ("mirror", None),
    ("dissociate", None),
    ("recursive", None),
    ("recurse-submodules", None),
    ("single-branch", None),
    ("no-single-branch", None),
    ("verbose", Some('v')),
    ("ipv4", None),
    ("ipv6", None),
    ("shallow-submodules", None),
    ("no-shallow-submodules", None),
    ("skip-repo", None),
    ("help-unused", None),
];

const STRS: [(&str, Option<char>); 13] = [
    ("template", None),
    ("origin", Some('o')),
    ("branch", Some('b')),
    ("upload-pack", Some('u')),
    ("reference", None),
    ("reference-if-able", None),
    ("separate-git-dir", None),
    ("depth", None),
    ("config", Some('c')),
    ("shallow-since", None),
    ("shallow-exclude", None),
    ("include", Some('I')),
    ("exclude", Some('X')),
];

fn clone(p: &Parsed) {
    super::require_git_version();
    if gitcmd::is_git_version_at_least("2.15.0") {
        eprintln!("WARNING: `git lfs clone` is deprecated and will not be updated\n          with new flags from `git clone`\n\n`git clone` has been updated in upstream Git to have comparable\nspeeds to `git lfs clone`.");
    }
    // CloneWithoutFilters: the flags in git-lfs's order.
    let mut a: Vec<String> = vec!["clone".into()];
    let b = |n: &str| p.bool(n);
    let s = |n: &str| p.str(n);
    let push_s = |a: &mut Vec<String>, flag: &str, v: String| {
        if !v.is_empty() {
            a.push(flag.into());
            a.push(v);
        }
    };
    if b("bare") {
        a.push("--bare".into());
    }
    push_s(&mut a, "--branch", s("branch"));
    push_s(&mut a, "--config", s("config"));
    push_s(&mut a, "--depth", s("depth"));
    for (f, n) in [("dissociate", "--dissociate"), ("ipv4", "--ipv4"), ("ipv6", "--ipv6"), ("local", "--local"), ("mirror", "--mirror"), ("no-checkout", "--no-checkout"), ("no-hardlinks", "--no-hardlinks"), ("no-single-branch", "--no-single-branch")] {
        if b(f) {
            a.push(n.into());
        }
    }
    push_s(&mut a, "--origin", s("origin"));
    for (f, n) in [("progress", "--progress"), ("quiet", "--quiet"), ("recursive", "--recursive"), ("recurse-submodules", "--recurse-submodules")] {
        if b(f) {
            a.push(n.into());
        }
    }
    push_s(&mut a, "--reference", s("reference"));
    push_s(&mut a, "--reference-if-able", s("reference-if-able"));
    push_s(&mut a, "--separate-git-dir", s("separate-git-dir"));
    if b("shared") {
        a.push("--shared".into());
    }
    if b("single-branch") {
        a.push("--single-branch".into());
    }
    push_s(&mut a, "--template", s("template"));
    push_s(&mut a, "--upload-pack", s("upload-pack"));
    if b("verbose") {
        a.push("--verbose".into());
    }
    push_s(&mut a, "--shallow-since", s("shallow-since"));
    push_s(&mut a, "--shallow-exclude", s("shallow-exclude"));
    if b("shallow-submodules") {
        a.push("--shallow-submodules".into());
    }
    if b("no-shallow-submodules") {
        a.push("--no-shallow-submodules".into());
    }
    let jobs = p.int("jobs", -1);
    if jobs > -1 {
        a.push("--jobs".into());
        a.push(jobs.to_string());
    }
    a.extend(p.args.iter().cloned());
    let argv: Vec<&str> = a.iter().map(String::as_str).collect();
    let st = gitcmd::git_no_lfs_command(&argv).status();
    match st {
        Err(e) => exit(&format!("Error(s) during clone:\nfailed to start `git clone`: {}", tools::io_err(&e))),
        Ok(s) if !s.success() => exit(&format!("Error(s) during clone:\n`git clone` failed: {}", crate::subprocess::exit_text(&s))),
        Ok(_) => {}
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let last = p.args.last().cloned().unwrap_or_default();
    let mut dir = tools::abs(&last).display().to_string();
    if !tools::dir_exists(&dir) {
        let mut base = last.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
        if let Some(b) = base.strip_suffix(".git") {
            base = b.to_string();
        }
        dir = tools::abs(&base).display().to_string();
        if !tools::dir_exists(&dir) {
            exit(&format!("Unable to find clone dir at {}", tools::quote(&dir)));
        }
    }
    if let Err(e) = std::env::set_current_dir(&dir) {
        exit(&format!("Unable to change directory to clone dir {}: {}", tools::quote(&dir), tools::io_err(&e)));
    }
    setup_repository();
    if !s("origin").is_empty() {
        cfg().set_remote(&s("origin"));
    }
    if let Ok(r) = gitcmd::current_ref() {
        let filter = super::pull::build_filter(p, true);
        if b("no-checkout") || b("bare") {
            super::fetch::fetch_ref_plain(&r.name, &filter);
        } else {
            super::pull::pull(filter);
            if gitcmd::is_git_version_at_least("2.9.0") && (b("recursive") || b("recurse-submodules")) {
                let st = crate::subprocess::command("git", &["submodule", "foreach", "--recursive", "git lfs pull"]).status();
                match st {
                    Ok(s) if s.success() => {}
                    Ok(s) => exit(&format!("Error performing `git lfs pull` for submodules: {}", crate::subprocess::exit_text(&s))),
                    Err(e) => exit(&format!("Error performing `git lfs pull` for submodules: {}", tools::io_err(&e))),
                }
            }
        }
    }
    if !b("skip-repo") {
        if let Err(e) = super::install::install_hooks(false) {
            exit_with_error(&e);
        }
    }
    let _ = std::env::set_current_dir(cwd);
}

pub fn commands() -> Vec<Cmd> {
    let mut flags: Vec<crate::cli::Flag> = BOOLS.iter().filter(|(n, _)| *n != "help-unused").map(|(n, s)| flag(n, *s, K::Bool)).collect();
    flags.extend(STRS.iter().map(|(n, s)| flag(n, *s, K::Str)));
    flags.push(flag("jobs", Some('j'), K::Int));
    let mut c = super::cmd("clone", clone, flags);
    c.http_logger = false;
    vec![c]
}
