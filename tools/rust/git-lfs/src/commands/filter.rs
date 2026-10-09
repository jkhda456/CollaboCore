//! `git lfs clean`, `smudge` and `filter-process` (the filters git runs).

use super::*;
use crate::cli::{flag, K};
use crate::errors::Kind;
use crate::filter::{Filter, PatternType};
use crate::gitfilter::{self, Cleaned, ProgressFile};
use crate::pktline::Pktline;
use crate::pointer::{self, Pointer};
use std::io::Read;

fn possibly_malformed(n: i64) -> bool {
    n >= 4 * (1i64 << 30)
}

/// clean(): the pointer (or the input passed through) written to `to`.
pub(super) fn do_clean(to: &mut dyn Write, from: &mut dyn Read, file_name: &str, size: i64) -> Result<Option<Pointer>, Error> {
    let mut cb = match ProgressFile::open("clean", file_name, 1, 1) {
        Ok(c) => c,
        Err(e) => {
            error(&e.to_string());
            None
        }
    };
    let cleaned = match gitfilter::clean(from, file_name, size, cb.as_mut()) {
        Ok(c) => c,
        Err(e) => exit_with_error(&e.wrap("Error cleaning Git LFS object")),
    };
    let (ptr, tmp) = match cleaned {
        Cleaned::Passthrough(b) => {
            to.write_all(&b)?;
            return Ok(None);
        }
        Cleaned::Object { pointer, tmp } => (pointer, tmp),
    };
    let media = match cfg().filesystem().object_path(&ptr.oid) {
        Ok(m) => m,
        Err(e) => panic_exit(&e, "Unable to get local media path."),
    };
    match std::fs::metadata(&media) {
        Ok(m) => {
            if m.len() as i64 != ptr.size && ptr.extensions.is_empty() {
                exit(&format!("Files don't match:\n{media}\n{tmp}"));
            }
            crate::trace!("{} exists", media);
            let _ = std::fs::remove_file(&tmp);
        }
        Err(_) => {
            if let Err(e) = std::fs::rename(&tmp, &media) {
                panic_exit(&tools::path_err("rename", &tmp, &e), &format!("Unable to move {tmp} to {media}"));
            }
            crate::trace!("Writing {}", media);
        }
    }
    to.write_all(ptr.encoded().as_bytes())?;
    Ok(Some(ptr))
}

fn clean_cmd(p: &Parsed) {
    require_stdin("This command should be run by the Git 'clean' filter");
    setup_repository();
    let _ = super::install::install_hooks(false);
    let name = p.args.first().cloned().unwrap_or_default();
    let mut out = std::io::stdout().lock();
    match do_clean(&mut out, &mut std::io::stdin().lock(), &name, -1) {
        Err(e) => error(&e.to_string()),
        Ok(Some(ptr)) if possibly_malformed(ptr.size) => error("Possibly malformed conversion on Windows, see `git lfs help smudge` for more details."),
        _ => {}
    }
    let _ = out.flush();
}

fn fetch_filter() -> Filter {
    Filter::new(&cfg().fetch_include_paths(), &cfg().fetch_exclude_paths(), PatternType::GitIgnore)
}

/// The first pointer-sized part of the input, and the input with it put back.
fn decode_from(from: &mut dyn Read) -> (std::result::Result<Pointer, Error>, Vec<u8>) {
    let mut head = vec![0u8; pointer::BLOB_SIZE_CUTOFF];
    let mut got = 0;
    while got < head.len() {
        match from.read(&mut head[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    head.truncate(got);
    (pointer::decode(&head), head)
}

/// smudge(): the object content (or pointer) to `to`.
fn do_smudge(to: &mut dyn Write, from: &mut dyn Read, filename: &str, skip: bool, filter: &Filter) -> Result<i64, Error> {
    let (ptr, head) = decode_from(from);
    let ptr = match ptr {
        Ok(p) => p,
        Err(perr) => {
            to.write_all(&head).map_err(|e| Error::from(e).wrap(perr.to_string()))?;
            let n = std::io::copy(from, to).map_err(|e| Error::from(e).wrap(perr.to_string()))? + head.len() as u64;
            if n != 0 {
                return Err(Error::not_a_pointer(Error::new(format!("Unable to parse pointer at: {}", tools::quote(filename)))));
            }
            return Ok(0);
        }
    };
    gitfilter::link_or_copy_from_reference(&ptr.oid, ptr.size);
    let mut cb = ProgressFile::open("download", filename, 1, 1)?;
    if skip || !filter.allows(filename) {
        to.write_all(ptr.encoded().as_bytes())?;
        return Ok(ptr.encoded().len() as i64);
    }
    match gitfilter::smudge(to, &ptr, filename, true, cb.as_mut()) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = to.write_all(ptr.encoded().as_bytes());
            let oid = &ptr.oid[..ptr.oid.len().min(7)];
            logged_error(&e, &format!("Error downloading object: {filename} ({oid}): {e}"));
            if !cfg().skip_download_errors() {
                exit_with_code(2);
            }
            Ok(0)
        }
    }
}

fn smudge_cmd(p: &Parsed) {
    require_stdin("This command should be run by the Git 'smudge' filter");
    setup_repository();
    let _ = super::install::install_hooks(false);
    let skip = p.bool("skip") || cfg().os.bool("GIT_LFS_SKIP_SMUDGE", false);
    let filter = fetch_filter();
    let name = p.args.first().cloned().unwrap_or_else(|| "<unknown file>".into());
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    match do_smudge(&mut out, &mut std::io::stdin().lock(), &name, skip, &filter) {
        Err(e) => {
            if e.is(Kind::NotAPointer) {
                let _ = out.flush();
                eprintln!("{e}");
            } else {
                error(&e.to_string());
            }
        }
        Ok(n) if possibly_malformed(n) => eprintln!("Possibly malformed smudge on Windows: see `git lfs help smudge` for more info."),
        _ => {}
    }
    let _ = out.flush();
}

struct Delayed {
    path: String,
    ptr: Pointer,
}

fn filter_process(p: &Parsed) {
    require_stdin("This command should be run by the Git filter process");
    setup_repository();
    let _ = super::install::install_hooks(false);
    let stdout = std::io::stdout();
    let mut pl = Pktline::new(std::io::stdin().lock(), stdout.lock());
    crate::trace!("Initialize filter-process");
    let fail = |e: Error| -> ! { exit_with_error(&e) };
    match pl.read_packet_text() {
        Ok((m, _)) if m == "git-filter-client" => {}
        Ok((m, _)) => fail(Error::new(format!("invalid filter-process pkt-line welcome message: {m}"))),
        Err(e) => fail(Error::from(e).wrap("reading filter-process initialization")),
    }
    let vers = pl.read_packet_list().unwrap_or_else(|e| fail(Error::from(e).wrap("reading filter-process versions")));
    if !vers.iter().any(|v| v == "version=2") {
        fail(Error::new(format!("filter 'version=2' not supported (your Git supports: [{}])", vers.join(" "))));
    }
    if let Err(e) = pl.write_packet_list(&["git-filter-server".into(), "version=2".into()]) {
        fail(Error::from(e).wrap("writing filter-process initialization failed"));
    }
    let caps = pl.read_packet_list().unwrap_or_else(|e| fail(Error::new(format!("reading filter-process capabilities failed with {}", tools::io_err(&e)))));
    let mut req_caps = vec!["capability=clean".to_string(), "capability=smudge".to_string()];
    let supports_delay = caps.iter().any(|c| c == "capability=delay");
    if supports_delay {
        req_caps.push("capability=delay".into());
    }
    for rc in &req_caps {
        if !caps.contains(rc) {
            fail(Error::new(format!("filter '{}' not supported (your Git supports: [{}])", rc, caps.join(" "))));
        }
    }
    if let Err(e) = pl.write_packet_list(&req_caps) {
        fail(Error::new(format!("writing filter-process capabilities failed with {}", tools::io_err(&e))));
    }
    let skip = p.bool("skip") || cfg().os.bool("GIT_LFS_SKIP_SMUDGE", false);
    let filter = fetch_filter();
    let mut malformed: Vec<String> = vec![];
    let mut malformed_windows: Vec<String> = vec![];
    // Delayed smudges: queued until git asks which are available; then done.
    let mut queued: Vec<Delayed> = vec![];
    let mut ready: Vec<Delayed> = vec![];
    let mut cached: std::collections::BTreeMap<String, Pointer> = Default::default();
    let mut queue_started = false;
    let status = |pl: &mut Pktline<_, _>, ok: bool| {
        let _ = pl.write_packet_list(&[format!("status={}", if ok { "success" } else { "error" })]);
    };
    loop {
        let list = match pl.read_packet_list() {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => exit_with_error(&e.into()),
        };
        let mut header: std::collections::BTreeMap<String, String> = Default::default();
        for pair in &list {
            if let Some((k, v)) = pair.split_once('=') {
                header.insert(k.to_string(), v.to_string());
            }
        }
        let command = header.get("command").cloned().unwrap_or_default();
        let pathname = header.get("pathname").cloned().unwrap_or_default();
        let payload = if command == "list_available_blobs" { vec![] } else { pl.read_payload().unwrap_or_default() };
        let mut out: Vec<u8> = vec![];
        let mut err: Option<Error> = None;
        let mut n: i64 = 0;
        let mut delayed = false;
        let cap: usize;
        match command.as_str() {
            "clean" => {
                status(&mut pl, true);
                cap = 512;
                match do_clean(&mut out, &mut std::io::Cursor::new(payload), &pathname, -1) {
                    Ok(Some(ptr)) => n = ptr.size,
                    Ok(None) => {}
                    Err(e) => err = Some(e),
                }
            }
            "smudge" => {
                cap = crate::pktline::MAX_PACKET_LENGTH;
                if supports_delay && !queue_started {
                    queue_started = true;
                    if cfg().auto_detect_remote_enabled() {
                        let r = crate::tq::first_remote_for_treeish(header.get("treeish").map(String::as_str).unwrap_or(""));
                        if !r.is_empty() {
                            cfg().set_remote(&r);
                        }
                    }
                }
                if header.get("can-delay").map(String::as_str) == Some("1") {
                    let (ptr, head) = decode_from(&mut std::io::Cursor::new(&payload[..]));
                    match ptr {
                        Err(_) => {
                            status(&mut pl, true);
                            out.extend_from_slice(&payload);
                            if !payload.is_empty() {
                                err = Some(Error::not_a_pointer(Error::new(format!("Unable to parse pointer at: {}", tools::quote(&pathname)))));
                            }
                            let _ = head;
                        }
                        Ok(ptr) => {
                            gitfilter::link_or_copy_from_reference(&ptr.oid, ptr.size);
                            match cfg().filesystem().object_path(&ptr.oid) {
                                Err(e) => err = Some(e),
                                Ok(path) => {
                                    if !skip && filter.allows(&pathname) {
                                        if std::fs::metadata(&path).is_err() && ptr.size != 0 {
                                            queued.push(Delayed { path: pathname.clone(), ptr: ptr.clone() });
                                            cached.insert(pathname.clone(), ptr.clone());
                                            delayed = true;
                                        } else {
                                            status(&mut pl, true);
                                            match gitfilter::smudge(&mut out, &ptr, &pathname, false, None) {
                                                Ok(x) => n = x,
                                                Err(e) => err = Some(e),
                                            }
                                        }
                                    } else {
                                        status(&mut pl, true);
                                        out.extend_from_slice(ptr.encoded().as_bytes());
                                        n = ptr.encoded().len() as i64;
                                    }
                                }
                            }
                        }
                    }
                } else {
                    status(&mut pl, true);
                    let input: Vec<u8> = if payload.is_empty() { cached.get(&pathname).map(|p| p.encoded().into_bytes()).unwrap_or_default() } else { payload };
                    match do_smudge(&mut out, &mut std::io::Cursor::new(input), &pathname, skip, &filter) {
                        Ok(x) => {
                            n = x;
                            cached.remove(&pathname);
                        }
                        Err(e) => err = Some(e),
                    }
                }
            }
            "list_available_blobs" => {
                // Everything queued is downloaded now; the next list call reports the
                // rest (none), which ends git's polling.
                if !queued.is_empty() {
                    let batch: Vec<Delayed> = std::mem::take(&mut queued);
                    let items: Vec<(String, Pointer)> = batch.iter().map(|d| (d.path.clone(), d.ptr.clone())).collect();
                    let results = crate::tq::download_many(&items, &cfg().remote());
                    for (d, r) in batch.into_iter().zip(results) {
                        if let Err(e) = r {
                            crate::trace!("delayed download of {} failed: {}", d.path, e);
                        }
                        ready.push(d);
                    }
                }
                let mut paths: Vec<String> = ready.drain(..).map(|d| format!("pathname={}", d.path)).collect();
                if paths.is_empty() {
                    paths = cached.keys().map(|k| format!("pathname={k}")).collect();
                    queue_started = false;
                }
                let _ = pl.write_packet_list(&paths);
                status(&mut pl, true);
                continue;
            }
            other => exit_with_error(&Error::new(format!("unknown command {}", tools::quote(other)))),
        }
        if let Some(e) = &err {
            if e.is(Kind::NotAPointer) {
                malformed.push(pathname.clone());
                err = None;
            }
        } else if possibly_malformed(n) {
            malformed_windows.push(pathname.clone());
        }
        if delayed {
            let _ = pl.write_packet_list(&[format!("status={}", if err.is_some() { "error" } else { "delayed" })]);
            continue;
        }
        let ok = pl.write_payload(&out, cap).is_ok() && err.is_none();
        status(&mut pl, ok);
    }
    if !malformed.is_empty() {
        if malformed.len() == 1 {
            eprintln!("Encountered 1 file that should have been a pointer, but wasn't:");
        } else {
            eprintln!("Encountered {} files that should have been pointers, but weren't:", malformed.len());
        }
        for m in &malformed {
            eprintln!("\t{m}");
        }
    }
    if !malformed_windows.is_empty() && cfg().git().bool("lfs.largefilewarning", !gitcmd::is_git_version_at_least("2.34.0")) {
        if malformed_windows.len() == 1 {
            eprintln!("Encountered 1 file that may not have been copied correctly on Windows:");
        } else {
            eprintln!("Encountered {} files that may not have been copied correctly on Windows:", malformed_windows.len());
        }
        for m in &malformed_windows {
            eprintln!("\t{m}");
        }
        eprint!("\nSee: `git lfs help smudge` for more details.\n");
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd("clean", clean_cmd, vec![]),
        cmd("smudge", smudge_cmd, vec![flag("skip", Some('s'), K::Bool)]),
        cmd("filter-process", filter_process, vec![flag("skip", Some('s'), K::Bool)]),
    ]
}
