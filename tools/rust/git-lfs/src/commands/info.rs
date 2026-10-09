//! ls-files, status and pointer (commands/command_ls_files.go, command_status.go,
//! command_pointer.go).

use super::fetch::go_indent;
use super::pull::build_filter;
use super::{cmd, error, exit, exit_with_code, exit_with_error, panic_exit, print, setup_repository, setup_working_copy, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::cfg;
use crate::errors::{Error, Result};
use crate::gitcmd::{self, RefType};
use crate::gitscanner::{self, DiffIndexEntry, ObjectReader, Scanner, WrappedPointer};
use crate::tools;
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};

fn file_exists_of_size(p: &WrappedPointer) -> bool {
    let name = String::from_utf8_lossy(&crate::fs::decode_path_bytes(p.name.as_bytes())).into_owned();
    std::fs::metadata(format!("{}/{}", cfg().local_working_dir(), name)).is_ok_and(|m| m.len() as i64 == p.size())
}

const VERSION: &str = "https://git-lfs.github.com/spec/v1";

fn ls_files(p: &Parsed) {
    setup_repository();
    let all = p.bool("all");
    let deleted = p.bool("deleted");
    let mut include_ref = String::new();
    let mut scan_range = false;
    let r: String = if let Some(a) = p.args.first() {
        if all {
            exit("Cannot use --all with explicit reference");
        } else if a == "--all" {
            exit("Did you mean `git lfs ls-files --all --` ?");
        }
        if p.args.len() > 1 {
            if deleted {
                exit("Cannot use --deleted with reference range");
            }
            include_ref = p.args[1].clone();
            scan_range = true;
        }
        a.clone()
    } else {
        match gitcmd::current_ref() {
            Ok(r) => r.sha,
            Err(_) => gitcmd::empty_tree(),
        }
    };
    let oid_len = if p.bool("long") { 64 } else { 10 };
    let (debug, json, name_only, show_size) = (p.bool("debug"), p.bool("json"), p.bool("name-only"), p.bool("size"));
    let mut seen: HashSet<String> = HashSet::new();
    let mut items: Vec<serde_json::Value> = vec![];
    let fs = cfg().filesystem();
    let mut cb = |res: Result<WrappedPointer>| {
        let p = match res {
            Ok(p) => p,
            Err(e) => exit(&format!("Could not scan for Git LFS tree: {e}")),
        };
        if p.size() == 0 {
            return;
        }
        if !all && !scan_range && seen.contains(&p.name) {
            return;
        }
        if debug {
            print(&format!(
                "filepath: {}\n    size: {}\ncheckout: {}\ndownload: {}\n     oid: sha256 {}\n version: {}\n",
                p.name,
                p.size(),
                file_exists_of_size(&p),
                fs.object_exists(p.oid(), p.size()),
                p.oid(),
                VERSION
            ));
        } else if json {
            let mut m = serde_json::Map::new();
            m.insert("name".into(), p.name.clone().into());
            m.insert("size".into(), p.size().into());
            m.insert("checkout".into(), file_exists_of_size(&p).into());
            m.insert("downloaded".into(), fs.object_exists(p.oid(), p.size()).into());
            m.insert("oid_type".into(), "sha256".into());
            m.insert("oid".into(), p.oid().into());
            m.insert("version".into(), VERSION.into());
            items.push(serde_json::Value::Object(m));
        } else {
            let mut msg = if name_only {
                vec![p.name.clone()]
            } else {
                vec![p.oid()[..oid_len.min(p.oid().len())].to_string(), if file_exists_of_size(&p) { "*" } else { "-" }.to_string(), p.name.clone()]
            };
            if show_size {
                msg.push(format!("({})", tools::format_bytes(p.size() as u64)));
            }
            print(&msg.join(" "));
        }
        seen.insert(p.name.clone());
    };
    let mut s = Scanner::new();
    s.filter = Some(build_filter(p, false));
    if p.args.is_empty() {
        if let Err(e) = s.scan_index(&r, "", &mut cb) {
            exit(&format!("Could not scan for Git LFS index: {e}"));
        }
    }
    if all {
        if let Err(e) = s.scan_all(&mut cb) {
            exit(&format!("Could not scan for Git LFS history: {e}"));
        }
    } else {
        let res = if deleted {
            s.scan_ref_with_deleted(&r, &mut cb)
        } else if scan_range {
            s.scan_ref_range(&include_ref, &r, &mut cb)
        } else {
            s.scan_tree(&r, &mut cb)
        };
        if let Err(e) = res {
            exit(&format!("Could not scan for Git LFS tree: {e}"));
        }
    }
    if json {
        let v = serde_json::json!({ "files": if items.is_empty() { serde_json::Value::Null } else { serde_json::Value::Array(items) } });
        print(&go_indent(&v, " "));
    }
}

// status

fn blob_info(r: &mut ObjectReader, sha: &str, name: &str) -> Result<(String, String)> {
    if !gitcmd::is_zero_object_id(sha) {
        let (o, hash) = match r.read(sha, crate::pointer::BLOB_SIZE_CUTOFF) {
            Ok(x) => x,
            Err(e) if e.to_string().starts_with("missing object") => return Ok(("<missing>".into(), "?".into())),
            Err(e) => return Err(e),
        };
        if let Some(h) = hash {
            return Ok((h[..7].to_string(), "Git".into()));
        }
        return Ok(match crate::pointer::decode(&o.data) {
            Ok(p) if o.kind == "blob" => (p.oid[..7].to_string(), "LFS".into()),
            _ => {
                let h = tools::sha256_hex(&o.data);
                (h[..7].to_string(), "Git".into())
            }
        });
    }
    let path = format!("{}/{}", cfg().local_working_dir(), name);
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(("deleted".into(), "File".into())),
        Err(e) => return Err(tools::path_err("open", &path, &e)),
    };
    if f.metadata().is_ok_and(|m| m.is_dir()) {
        return Ok(("deleted".into(), "File".into()));
    }
    let h = tools::sha256_reader(&mut f).map_err(Error::from)?;
    Ok((h[..7].to_string(), "File".into()))
}

fn info_from(r: &mut ObjectReader, e: &DiffIndexEntry) -> Result<(String, String)> {
    let sha = if gitcmd::is_zero_object_id(&e.src_sha) { &e.dst_sha } else { &e.src_sha };
    blob_info(r, sha, &e.src_name)
}

fn info_to(r: &mut ObjectReader, e: &DiffIndexEntry) -> Result<(String, String)> {
    let name = if e.dst_name.is_empty() { &e.src_name } else { &e.dst_name };
    blob_info(r, &e.dst_sha, name)
}

fn format_blob_info(r: &mut ObjectReader, e: &DiffIndexEntry) -> String {
    let (sha, src) = info_from(r, e).unwrap_or_else(|err| exit_with_error(&err));
    let from = format!("{src}: {sha}");
    if e.status == 'A' {
        return from;
    }
    let (sha, src) = info_to(r, e).unwrap_or_else(|err| exit_with_error(&err));
    format!("{from} -> {src}: {sha}")
}

fn scan_index(r: &str) -> Result<(Vec<DiffIndexEntry>, Vec<DiffIndexEntry>)> {
    let uncached = gitscanner::diff_index(r, false, true, "")?;
    let cached = gitscanner::diff_index(r, true, false, "")?;
    let mut seen = HashSet::new();
    let mut drain = |v: Vec<DiffIndexEntry>| {
        let mut out = vec![];
        for e in v {
            let name = if e.dst_name.is_empty() { e.src_name.clone() } else { e.dst_name.clone() };
            if seen.insert(format!("{}:{}:{}", e.src_sha, e.dst_sha, name)) {
                out.push(e);
            }
        }
        out
    };
    let staged = drain(cached);
    let unstaged = drain(uncached);
    Ok((staged, unstaged))
}

fn relativize(from: &str, to: &str) -> String {
    if from.is_empty() {
        return to.to_string();
    }
    let f: Vec<&str> = from.split('/').collect();
    let t: Vec<&str> = to.split('/').collect();
    let mut d = 0;
    while d < f.len().min(t.len()) && f[d] == t[d] {
        d += 1;
    }
    format!("{}{}", "../".repeat(f.len() - d), t[d..].join("/"))
}

fn status(p: &Parsed) {
    setup_working_copy();
    let cur = gitcmd::current_ref().ok();
    let at = if cur.is_none() { gitcmd::empty_tree() } else { "HEAD".to_string() };
    let mut rd = match ObjectReader::new() {
        Ok(r) => r,
        Err(e) => exit_with_error(&e),
    };
    if p.bool("porcelain") {
        let (staged, unstaged) = scan_index(&at).unwrap_or_else(|e| exit_with_error(&e));
        let mut seen = HashSet::new();
        for e in unstaged.iter().chain(staged.iter()) {
            let name = if e.dst_name.is_empty() { &e.src_name } else { &e.dst_name };
            if seen.insert(name.clone()) {
                print(&match e.status {
                    'R' | 'C' => format!("{}  {} -> {}", e.status, e.src_name, e.dst_name),
                    'M' => format!(" {} {}", e.status, e.src_name),
                    s => format!("{}  {}", s, e.src_name),
                });
            }
        }
        return;
    }
    if p.bool("json") {
        let (staged, unstaged) = scan_index(&at).unwrap_or_else(|e| exit_with_error(&e));
        let mut files: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for e in unstaged.iter().chain(staged.iter()) {
            let (_, src) = info_from(&mut rd, e).unwrap_or_else(|err| exit_with_error(&err));
            if src != "LFS" {
                continue;
            }
            match e.status {
                'R' | 'C' => {
                    files.insert(e.dst_name.clone(), serde_json::json!({"status": e.status.to_string(), "from": e.src_name}));
                }
                s => {
                    files.insert(e.src_name.clone(), serde_json::json!({"status": s.to_string()}));
                }
            }
        }
        print(&serde_json::to_string(&serde_json::json!({ "files": files })).unwrap());
        return;
    }
    if let Some(r) = &cur {
        print(&format!("On branch {}", r.name));
        if let Some(remote_ref) = current_remote_ref(r) {
            print(&format!("Objects to be pushed to {}:\n", remote_ref.name));
            let mut s = Scanner::new();
            let res = s.scan_ref_range(&r.sha, &remote_ref.sha, &mut |res| match res {
                Ok(p) => print(&format!("\t{} ({})", p.name, p.oid())),
                Err(e) => panic_exit(&e, "Could not scan for Git LFS objects"),
            });
            if let Err(e) = res {
                panic_exit(&e, "Could not scan for Git LFS objects");
            }
        }
    }
    let (staged, unstaged) = scan_index(&at).unwrap_or_else(|e| exit_with_error(&e));
    let wd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let wd = tools::resolve_symlinks(&wd);
    let repo = cfg().local_working_dir();
    print("\nObjects to be committed:\n");
    for e in &staged {
        let src = relativize(&wd, &tools::clean_str(&format!("{repo}/{}", e.src_name)));
        let dst = relativize(&wd, &tools::clean_str(&format!("{repo}/{}", e.dst_name)));
        let info = format_blob_info(&mut rd, e);
        match e.status {
            'R' | 'C' => print(&format!("\t{src} -> {dst} ({info})")),
            _ => print(&format!("\t{src} ({info})")),
        }
    }
    print("\nObjects not staged for commit:\n");
    for e in &unstaged {
        let src = relativize(&wd, &tools::clean_str(&format!("{repo}/{}", e.src_name)));
        let info = format_blob_info(&mut rd, e);
        print(&format!("\t{src} ({info})"));
    }
    print("");
}

/// The remote-tracking ref of the current branch (CurrentRemoteRef).
fn current_remote_ref(r: &gitcmd::Ref) -> Option<gitcmd::Ref> {
    if r.typ == RefType::Head || r.typ == RefType::Other {
        return None;
    }
    let remote = gitcmd::remote_for_branch(&r.name);
    if remote.is_empty() {
        return None;
    }
    let branch = gitcmd::remote_branch_for_local_branch(&r.name);
    gitcmd::resolve_ref(&format!("refs/remotes/{remote}/{branch}")).ok()
}

// pointer

fn hash_object(data: &[u8]) -> Result<String> {
    let mut c = gitcmd::git_no_lfs_command(&["hash-object", "--stdin"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| Error::new(format!("failed to find `git hash-object`: {}", tools::io_err(&e))))?;
    c.stdin.take().unwrap().write_all(data).map_err(Error::from)?;
    let out = c.wait_with_output().map_err(Error::from)?;
    if !out.status.success() {
        return Err(Error::new(format!("error building Git blob OID: {}", crate::subprocess::exit_text(&out.status))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pointer(p: &Parsed) {
    let file = p.str("file");
    let compare = p.str("pointer");
    let stdin = p.bool("stdin");
    if p.bool("check") {
        if p.bool("strict") && p.bool("no-strict") {
            exit_with_error(&Error::new("Cannot combine --strict with --no-strict"));
        }
        if !compare.is_empty() {
            exit_with_error(&Error::new("Cannot combine --check with --pointer"));
        }
        let mut data = vec![];
        if !file.is_empty() {
            if stdin {
                exit_with_error(&Error::new("With --check, --file cannot be combined with --stdin"));
            }
            match std::fs::File::open(&file) {
                Ok(mut f) => {
                    let _ = f.read_to_end(&mut data);
                }
                Err(e) => exit_with_error(&tools::path_err("open", &file, &e)),
            }
        } else if stdin {
            let _ = std::io::stdin().read_to_end(&mut data);
        } else {
            exit_with_error(&Error::new("Must specify either --file or --stdin with --check"));
        }
        match crate::pointer::decode(&data) {
            Err(_) => exit_with_code(1),
            Ok(ptr) => {
                if p.bool("strict") && !ptr.canonical {
                    exit_with_code(2);
                }
            }
        }
        return;
    }
    let mut comparing = !compare.is_empty() || stdin;
    let mut something = false;
    let mut has_ext = false;
    let mut build_oid = String::new();
    let mut compare_oid = String::new();
    let mut compare_exts = 0;
    if !file.is_empty() {
        something = true;
        let mut f = match std::fs::File::open(&file) {
            Ok(f) => f,
            Err(e) => {
                error(&tools::path_err("open", &file, &e).to_string());
                exit_with_code(1);
            }
        };
        let ptr = if p.bool("no-extensions") || !cfg().in_repo() {
            let mut data = vec![];
            if let Err(e) = f.read_to_end(&mut data) {
                error(&tools::io_err(&e));
                exit_with_code(1);
            }
            crate::pointer::Pointer::new(&tools::sha256_hex(&data), data.len() as i64, vec![])
        } else {
            has_ext = !cfg().extensions().is_empty();
            match crate::gitfilter::clean(&mut f, &file, -1, None) {
                Ok(crate::gitfilter::Cleaned::Object { pointer, tmp }) => {
                    let _ = std::fs::remove_file(tmp);
                    pointer
                }
                Ok(crate::gitfilter::Cleaned::Passthrough(d)) => match crate::pointer::decode(&d) {
                    Ok(p) => p,
                    Err(e) => {
                        error(&e.to_string());
                        exit_with_code(1);
                    }
                },
                Err(e) => {
                    error(&e.to_string());
                    exit_with_code(1);
                }
            }
        };
        eprintln!("Git LFS pointer for {file}");
        if !p.bool("no-extensions") && has_ext {
            eprintln!("warning: Using LFS extensions, use --no-extensions for a plain pointer.");
        }
        eprintln!();
        let enc = ptr.encoded();
        print!("{enc}");
        let _ = std::io::stdout().flush();
        if comparing {
            build_oid = hash_object(enc.as_bytes()).unwrap_or_else(|e| {
                error(&e.to_string());
                exit_with_code(1)
            });
            eprint!("\nGit blob OID: {build_oid}\n\n");
        }
    } else {
        comparing = false;
    }
    if !compare.is_empty() || stdin {
        something = true;
        let data = if !compare.is_empty() {
            if stdin {
                error("cannot read from STDIN and --pointer");
                exit_with_code(1);
            }
            match std::fs::read(&compare) {
                Ok(d) => d,
                Err(e) => {
                    error(&tools::path_err("open", &compare, &e).to_string());
                    exit_with_code(1);
                }
            }
        } else {
            super::require_stdin("The --stdin flag expects a pointer file from STDIN.");
            let mut d = vec![];
            let _ = std::io::stdin().read_to_end(&mut d);
            d
        };
        let parsed = crate::pointer::decode(&data);
        let name = if stdin { "STDIN".to_string() } else { compare.clone() };
        eprint!("Pointer from {name}\n\n");
        let ptr = match parsed {
            Ok(p) => p,
            Err(e) => {
                error(&e.to_string());
                exit_with_code(1);
            }
        };
        compare_exts = ptr.extensions.len();
        eprint!("{}", String::from_utf8_lossy(&data));
        if comparing {
            compare_oid = hash_object(&data).unwrap_or_else(|e| {
                error(&e.to_string());
                exit_with_code(1)
            });
            eprintln!("\nGit blob OID: {compare_oid}");
        }
    }
    if comparing && build_oid != compare_oid {
        eprintln!("\nPointers do not match");
        if has_ext || compare_exts > 0 {
            eprintln!("note: Mismatch may be due to differing LFS extensions.");
        }
        exit_with_code(1);
    }
    if !something {
        error("Nothing to do!");
        exit_with_code(1);
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd(
            "ls-files",
            ls_files,
            vec![
                flag("long", Some('l'), K::Bool),
                flag("size", Some('s'), K::Bool),
                flag("name-only", Some('n'), K::Bool),
                flag("debug", Some('d'), K::Bool),
                flag("all", Some('a'), K::Bool),
                flag("deleted", None, K::Bool),
                flag("include", Some('I'), K::Str),
                flag("exclude", Some('X'), K::Str),
                flag("json", Some('j'), K::Bool),
            ],
        ),
        cmd("status", status, vec![flag("porcelain", Some('p'), K::Bool), flag("json", Some('j'), K::Bool)]),
        cmd(
            "pointer",
            pointer,
            vec![
                flag("file", Some('f'), K::Str),
                flag("pointer", Some('p'), K::Str),
                flag("stdin", None, K::Bool),
                flag("check", None, K::Bool),
                flag("strict", None, K::Bool),
                flag("no-strict", None, K::Bool),
                flag("no-extensions", None, K::Bool),
            ],
        ),
    ]
}
