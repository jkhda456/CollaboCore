//! `git lfs env`.

use super::*;
use crate::endpoint;

fn run(_p: &Parsed) {
    cfg().show_warnings.store(true, std::sync::atomic::Ordering::Relaxed);
    let gitv = gitcmd::version().unwrap_or_else(|e| format!("Error getting Git version: {e}"));
    print(&version_desc());
    print(&gitv);
    print("");
    let mut default_remote = String::new();
    if cfg().is_default_remote() {
        default_remote = cfg().remote();
        let ep = endpoint::endpoint("download", &default_remote);
        if !ep.url.is_empty() {
            let (a, _) = endpoint::access_for(&ep.url);
            print(&format!("Endpoint={} (auth={})", ep.url, a.mode()));
            if !ep.ssh.user_and_host.is_empty() {
                print(&format!("  SSH={}:{}", ep.ssh.user_and_host, ep.ssh.path));
            }
        }
    }
    for remote in cfg().remotes() {
        if remote == default_remote {
            continue;
        }
        let ep = endpoint::endpoint("download", &remote);
        let (a, _) = endpoint::access_for(&ep.url);
        print(&format!("Endpoint ({})={} (auth={})", remote, ep.url, a.mode()));
        if !ep.ssh.user_and_host.is_empty() {
            print(&format!("  SSH={}:{}", ep.ssh.user_and_host, ep.ssh.path));
        }
    }
    for e in crate::lfs::environ() {
        print(&e);
    }
    for key in ["filter.lfs.process", "filter.lfs.smudge", "filter.lfs.clean"] {
        let v = cfg().git().get(key).unwrap_or_default();
        print(&format!("git config {} = {}", key, tools::quote(&v)));
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd("env", run, vec![])]
}
