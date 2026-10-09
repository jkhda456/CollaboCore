//! fsck (commands/command_fsck.go): the objects the history (and index) refer to are
//! checked against their OIDs, and the files that should be pointers are checked to be ones.

use super::{cmd, exit_with_code, exit_with_error, panic_exit, print, setup_repository, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::Kind;
use crate::filter::{Filter, PatternType};
use crate::gitcmd;
use crate::gitscanner::Scanner;
use crate::tools;

fn fsck_object(name: &str, oid: &str, size: i64) -> std::io::Result<bool> {
    let path = cfg().filesystem().object_pathname(oid);
    crate::trace!("Examining {} ({})", name, path);
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            if size == 0 {
                return Ok(true);
            }
            print(&format!("objects: openError: {} ({}) could not be checked: {}", name, oid, tools::errno_text(e.raw_os_error().unwrap_or(0))));
            return Ok(false);
        }
    };
    if tools::sha256_reader(&mut f)? == oid {
        return Ok(true);
    }
    print(&format!("objects: corruptObject: {name} ({oid}) is corrupt"));
    Ok(false)
}

fn fsck_objects(include: &str, exclude: &str, use_index: bool) -> Vec<String> {
    let mut corrupt = vec![];
    let mut s = Scanner::new();
    s.filter = Some(Filter::new(&[], &cfg().fetch_exclude_paths(), PatternType::GitIgnore));
    let mut cb = |res: crate::errors::Result<crate::gitscanner::WrappedPointer>| {
        let r = res.and_then(|p| fsck_object(&p.name, p.oid(), p.size()).map(|ok| (ok, p.oid().to_string())).map_err(Into::into));
        match r {
            Ok((true, _)) => {}
            Ok((false, oid)) => corrupt.push(oid),
            Err(e) => panic_exit(&e, "Error checking Git LFS files"),
        }
    };
    let r = if exclude.is_empty() { s.scan_ref(include, &mut cb) } else { s.scan_ref_range(include, exclude, &mut cb) };
    if let Err(e) = r {
        exit_with_error(&e);
    }
    if use_index {
        if let Err(e) = s.scan_index("HEAD", "", &mut cb) {
            exit_with_error(&e);
        }
    }
    corrupt
}

fn fsck_pointers(include: &str, exclude: &str) -> usize {
    let mut n = 0;
    let mut s = Scanner::new();
    let mut cb = |res: crate::errors::Result<crate::gitscanner::WrappedPointer>| match res {
        Ok(p) => {
            crate::trace!("Examining {} ({})", p.oid(), p.name);
            if !p.p.canonical {
                print(&format!("pointer: nonCanonicalPointer: Pointer for {} (blob {}) was not canonical", p.oid(), p.sha1));
                n += 1;
            }
        }
        Err(e) if e.is(Kind::PointerScan) => {
            let ctx = e.expected.clone().or_else(|| e.cause.as_ref().and_then(|c| c.expected.clone())).unwrap_or_default();
            let (tree, path) = ctx.split_once('\0').unwrap_or(("", ""));
            print(&format!("pointer: unexpectedGitObject: {} (treeish {}) should have been a pointer but was not", tools::quote(path), tree));
            n += 1;
        }
        Err(e) => panic_exit(&e, "Error checking Git LFS files"),
    };
    let r = if exclude.is_empty() { s.scan_ref_range_by_tree(include, "", true, &mut cb) } else { s.scan_ref_range_by_tree(include, exclude, false, &mut cb) };
    if let Err(e) = r {
        exit_with_error(&e);
    }
    n
}

fn fsck(p: &Parsed) {
    let _ = super::install::install_hooks(false);
    setup_repository();
    let mut use_index = false;
    let mut exclude = String::new();
    let include;
    match p.args.len() {
        0 => {
            use_index = true;
            match gitcmd::current_ref() {
                Ok(r) => include = r.sha,
                Err(e) => exit_with_error(&e),
            }
        }
        _ => {
            let pieces: Vec<&str> = p.args[0].splitn(2, "..").collect();
            let mut refs = vec![];
            for pc in &pieces {
                match gitcmd::resolve_ref(pc) {
                    Ok(r) => refs.push(r),
                    Err(e) => exit_with_error(&e),
                }
            }
            if refs.len() == 2 {
                exclude = refs[0].sha.clone();
                include = refs[1].sha.clone();
            } else {
                include = refs[0].sha.clone();
            }
        }
    }
    let (mut do_pointers, mut do_objects) = (p.bool("pointers"), p.bool("objects"));
    if !do_pointers && !do_objects {
        do_pointers = true;
        do_objects = true;
    }
    let mut corrupt = vec![];
    let mut bad_pointers = 0;
    if do_objects {
        corrupt = fsck_objects(&include, &exclude, use_index);
    }
    if do_pointers {
        bad_pointers = fsck_pointers(&include, &exclude);
    }
    if corrupt.is_empty() && bad_pointers == 0 {
        print("Git LFS fsck OK");
        return;
    }
    if p.bool("dry-run") || corrupt.is_empty() {
        exit_with_code(1);
    }
    let bad = format!("{}/bad", cfg().lfs_storage_dir());
    print(&format!("objects: repair: moving corrupt objects to {bad}"));
    if let Err(e) = tools::mkdir_all(&bad, cfg().repository_permissions(true)) {
        exit_with_error(&e.into());
    }
    for oid in &corrupt {
        let src = cfg().filesystem().object_pathname(oid);
        if src == "/dev/null" {
            continue;
        }
        if let Err(e) = std::fs::rename(&src, format!("{bad}/{oid}")) {
            if e.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            exit_with_error(&tools::path_err("rename", &src, &e));
        }
    }
    exit_with_code(1);
}

pub fn commands() -> Vec<Cmd> {
    vec![cmd("fsck", fsck, vec![flag("dry-run", Some('d'), K::Bool), flag("objects", None, K::Bool), flag("pointers", None, K::Bool)])]
}
