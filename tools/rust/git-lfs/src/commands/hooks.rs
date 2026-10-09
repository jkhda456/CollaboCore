//! The post-checkout, post-commit and post-merge hooks: lockable files' write bits.

use super::*;

fn lock_client_ok() -> bool {
    let dir = cfg().lfs_storage_dir();
    if let Err(e) = tools::mkdir_all(&dir, cfg().repository_permissions(false)) {
        exit(&format!("Unable to create lock system: {}", tools::path_err("mkdir", &dir, &e)));
    }
    !crate::locking::lockable_patterns().is_empty()
}

/// GetFilesChanged: `git diff-tree --name-only -r FROM [TO]`.
pub fn files_changed(from: &str, to: &str) -> Result<Vec<String>, Error> {
    let mut args = vec!["-c", "core.quotepath=false", "diff-tree", "--no-commit-id", "--name-only", "-r"];
    if !from.is_empty() {
        args.push(from);
    }
    if !to.is_empty() {
        args.push(to);
    }
    args.push("--");
    let out = gitcmd::git_no_lfs_command(&args).stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::new(format!("`git diff-tree` failed: {}", crate::subprocess::exit_text(&out.status))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect())
}

fn post_checkout(p: &Parsed) {
    if p.args.len() != 3 {
        print("This should be run through Git's post-checkout hook.  Run `git lfs update` to install it.");
        exit_with_code(1);
    }
    if !cfg().set_lockable_files_read_only() {
        return;
    }
    require_git_version();
    if !lock_client_ok() {
        return;
    }
    let (pre, post) = (&p.args[0], &p.args[1]);
    if p.args[2] == "1" && pre != "0000000000000000000000000000000000000000" {
        crate::trace!("post-checkout: changes between {} and {}", pre, post);
        let files = match files_changed(pre, post) {
            Ok(f) => f,
            Err(e) => {
                logged_error(&e, &format!("Warning: post-checkout rev diff {pre}:{post} failed: {e}\nFalling back on full scan."));
                full_scan();
                vec![]
            }
        };
        if let Err(e) = crate::locking::fix_lockable_file_write_flags(&files) {
            logged_error(&e, &format!("Warning: post-checkout locked file check failed: {e}"));
        }
    } else {
        full_scan();
    }
}

fn full_scan() {
    crate::trace!("post-checkout: checking write flags for all lockable files");
    if let Err(e) = crate::locking::fix_all_lockable_file_write_flags() {
        logged_error(&e, &format!("Warning: post-checkout locked file check failed: {e}"));
    }
}

fn post_commit(_p: &Parsed) {
    if !cfg().set_lockable_files_read_only() {
        return;
    }
    require_git_version();
    if !lock_client_ok() {
        return;
    }
    crate::trace!("post-commit: checking file write flags at HEAD");
    let files = match files_changed("HEAD", "") {
        Ok(f) => f,
        Err(e) => {
            logged_error(&e, &format!("Warning: post-commit failed: {e}"));
            exit_with_code(1);
        }
    };
    if let Err(e) = crate::locking::fix_lockable_file_write_flags(&files) {
        logged_error(&e, &format!("Warning: post-commit locked file check failed: {e}"));
    }
}

fn post_merge(p: &Parsed) {
    if p.args.len() != 1 {
        print("This should be run through Git's post-merge hook.  Run `git lfs update` to install it.");
        exit_with_code(1);
    }
    if !cfg().set_lockable_files_read_only() {
        return;
    }
    require_git_version();
    if !lock_client_ok() {
        return;
    }
    crate::trace!("post-merge: checking write flags for all lockable files");
    if let Err(e) = crate::locking::fix_all_lockable_file_write_flags() {
        logged_error(&e, &format!("Warning: post-merge locked file check failed: {e}"));
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd("post-checkout", post_checkout, vec![]), cmd("post-commit", post_commit, vec![]), cmd("post-merge", post_merge, vec![])]
}
