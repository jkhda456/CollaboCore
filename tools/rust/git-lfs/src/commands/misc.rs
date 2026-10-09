//! logs, ext, dedup and merge-driver (commands/command_logs.go, command_ext.go,
//! command_dedup.go, command_merge_driver.go).

use super::{cmd, error, exit, exit_with_code, exit_with_error, panic_exit, print, setup_repository, Cmd};
use crate::cli::{flag, Parsed, K};
use crate::config::{cfg, Extension};
use crate::errors::{Error, Kind, Result};
use crate::gitscanner::Scanner;
use crate::tools;
use std::collections::BTreeMap;
use std::io::Write;

// logs

fn sorted_logs() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(cfg().local_log_dir()) else { return vec![] };
    let mut v: Vec<String> = rd.flatten().filter(|e| !e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn logs(_p: &Parsed) {
    for l in sorted_logs() {
        print(&l);
    }
}

fn logs_show_name(name: &str) {
    let path = format!("{}/{}", cfg().local_log_dir(), name);
    match std::fs::read(&path) {
        Ok(b) => {
            crate::trace!("Reading log: {}", name);
            let _ = std::io::stdout().write_all(&b);
        }
        Err(_) => exit(&format!("Error reading log: {name}")),
    }
}

fn logs_last(_p: &Parsed) {
    let l = sorted_logs();
    match l.last() {
        None => print("No logs to show"),
        Some(n) => logs_show_name(n),
    }
}

fn logs_show(p: &Parsed) {
    match p.args.first() {
        None => print("Supply a log name."),
        Some(n) => logs_show_name(n),
    }
}

fn logs_clear(_p: &Parsed) {
    let d = cfg().local_log_dir();
    if let Err(e) = std::fs::remove_dir_all(&d) {
        if e.kind() != std::io::ErrorKind::NotFound {
            panic_exit(&Error::from(e), &format!("Error clearing {d}"));
        }
    }
    print(&format!("Cleared {d}"));
}

fn logs_boomtown(_p: &Parsed) {
    crate::trace!("Sample trace message");
    let e = Error::new("Sample wrapped error message").wrap("Sample error message");
    panic_exit(&e, "Sample panic message");
}

// ext

fn print_ext(e: &Extension) {
    print(&format!("Extension: {}", e.name));
    print(&format!("    clean = {}\n    smudge = {}\n    priority = {}", e.clean, e.smudge, e.priority));
}

fn print_all_exts() {
    match cfg().sorted_extensions() {
        Ok(v) => v.iter().for_each(print_ext),
        Err(e) => println!("{e}"),
    }
}

fn ext(_p: &Parsed) {
    print_all_exts();
}

fn ext_list(p: &Parsed) {
    if p.args.is_empty() {
        print_all_exts();
        return;
    }
    for k in &p.args {
        let e = cfg().extensions().get(k).cloned().unwrap_or(Extension { name: String::new(), clean: String::new(), smudge: String::new(), priority: 0 });
        print_ext(&e);
    }
}

// dedup

const FICLONE: libc::c_ulong = 0x40049409;

fn clone_file(dst: &std::fs::File, src: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let r = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn check_clone_supported(dir: &str) -> std::io::Result<()> {
    let n = std::process::id();
    let s = format!("{dir}/src{n}");
    let d = format!("{dir}/dst{n}");
    let sf = std::fs::File::create(&s)?;
    let df = std::fs::File::create(&d);
    let r = df.and_then(|df| clone_file(&df, &sf));
    let _ = std::fs::remove_file(&s);
    let _ = std::fs::remove_file(&d);
    r
}

fn dedup(p: &Parsed) {
    setup_repository();
    let ext_msg = "This platform supports file de-duplication, however, Git LFS extensions are configured and therefore de-duplication can not be used.";
    if p.bool("test") {
        if let Err(e) = check_clone_supported(&cfg().temp_dir()) {
            exit(&format!("This system does not support de-duplication: {}", tools::io_err(&e)));
        }
        if !cfg().extensions().is_empty() {
            exit(ext_msg);
        }
        print("OK: This platform and repository support file de-duplication.");
        return;
    }
    let gd = match crate::gitcmd::git_dir() {
        Ok(d) => d,
        Err(e) => exit_with_error(&e),
    };
    if check_clone_supported(&gd).is_err() {
        exit("This system does not support de-duplication.");
    }
    if !cfg().extensions().is_empty() {
        exit(ext_msg);
    }
    match crate::gitcmd::is_bare() {
        Ok(false) => match crate::gitcmd::git_simple(&["status", "--porcelain"]) {
            Ok(o) if !o.is_empty() => exit("Working tree is dirty. Please commit or reset your change."),
            Ok(_) => {}
            Err(e) => exit_with_error(&e),
        },
        Ok(true) => {}
        Err(e) => exit_with_error(&e),
    }
    let (mut count, mut size) = (0i64, 0i64);
    let fs = cfg().filesystem();
    let mut s = Scanner::new();
    let r = s.scan_tree("HEAD", &mut |res| {
        let p = match res {
            Ok(p) => p,
            Err(e) => exit(&format!("Could not scan for Git LFS tree: {e}")),
        };
        let r: Result<()> = (|| {
            if !fs.object_exists(p.oid(), p.size()) {
                return Err(Error::new("Git LFS object file does not exist"));
            }
            let orig = std::fs::metadata(&p.name).map_err(|e| tools::path_err("stat", &p.name, &e))?;
            let src = fs.object_pathname(p.oid());
            if src == "/dev/null" {
                return Ok(());
            }
            let dst = format!("{}/{}", cfg().local_working_dir(), p.name);
            let sf = std::fs::File::open(&src).map_err(|e| tools::path_err("open", &src, &e))?;
            let df = std::fs::File::create(&dst).map_err(|e| tools::path_err("open", &dst, &e))?;
            clone_file(&df, &sf).map_err(|e| Error::new(tools::io_err(&e)))?;
            std::fs::set_permissions(&dst, orig.permissions()).map_err(|e| tools::path_err("chmod", &dst, &e))?;
            Ok(())
        })();
        match r {
            Err(e) => error(&format!("Skipped: {} (Size: {})\n          {}", p.name, p.size(), e)),
            Ok(()) => {
                print(&format!("Success: {} (Size: {})", p.name, p.size()));
                count += 1;
                size += p.size();
            }
        }
    });
    if let Err(e) = r {
        exit_with_error(&e);
    }
    print(&format!(
        "\n\nFinished successfully.\n  De-duplicated  size: {} byte{}\n                count: {}",
        size,
        if size == 1 { "" } else { "s" },
        count
    ));
}

// merge-driver

fn merge_input(file: &str, specs: &mut BTreeMap<char, String>, tag: char) {
    let (f, tmp) = match crate::gitfilter::temp_file() {
        Ok(x) => x,
        Err(e) => exit(&format!("could not create temporary file when merging: {e}")),
    };
    specs.insert(tag, tmp.clone());
    if file.is_empty() {
        return;
    }
    let mut f = f;
    match crate::pointer::decode_from_file(file) {
        Err(e) if e.is(Kind::NotAPointer) => {
            drop(f);
            if let Err(e) = std::fs::copy(file, &tmp) {
                let _ = std::fs::remove_file(&tmp);
                exit(&format!("could not copy non-LFS content when merging: {}", tools::io_err(&e)));
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            exit(&format!("could not decode pointer when merging: {e}"));
        }
        Ok(ptr) => {
            let mut pf = crate::gitfilter::ProgressFile::open("download", &tmp, 1, 1).ok().flatten();
            let _ = crate::gitfilter::smudge(&mut f, &ptr, &tmp, true, pf.as_mut());
        }
    }
}

fn merge_driver(p: &Parsed) {
    let (anc, cur, oth, out) = (p.str("ancestor"), p.str("current"), p.str("other"), p.str("output"));
    if anc.is_empty() || cur.is_empty() || oth.is_empty() || out.is_empty() {
        exit("the --ancestor, --current, --other, and --output options are mandatory");
    }
    let mut specs: BTreeMap<char, String> = BTreeMap::new();
    merge_input(&anc, &mut specs, 'O');
    merge_input(&cur, &mut specs, 'A');
    merge_input(&oth, &mut specs, 'B');
    merge_input("", &mut specs, 'D');
    let marker = p.int("marker-size", 12);
    let marker_s = marker.to_string();
    let mut program = p.str("program");
    if program.is_empty() {
        program = "git merge-file --stdout --marker-size=%L %A %O %B >%D".into();
    }
    let repl: Vec<(char, &str)> = vec![('A', &specs[&'A']), ('O', &specs[&'O']), ('B', &specs[&'B']), ('D', &specs[&'D']), ('L', &marker_s)];
    let formatted = crate::subprocess::format_percent_sequences(&program, &repl);
    let cleanup = |specs: &BTreeMap<char, String>| {
        for k in ['A', 'O', 'B', 'D'] {
            let _ = std::fs::remove_file(&specs[&k]);
        }
    };
    let status = match crate::subprocess::command("sh", &["-c", &formatted]).status() {
        Ok(s) => s.code().unwrap_or(-1),
        Err(e) => {
            cleanup(&specs);
            exit_with_error(&Error::new(format!("failed to run merge program {}: {}", tools::quote(&formatted), tools::io_err(&e))));
        }
    };
    let r: Result<()> = (|| {
        use std::os::unix::fs::OpenOptionsExt;
        let mut outf = std::fs::OpenOptions::new().write(true).create(true).mode(0o600).open(&out).map_err(|e| tools::path_err("open", &out, &e))?;
        let d = &specs[&'D'];
        let mut inf = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(d).map_err(|e| tools::path_err("open", d, &e))?;
        super::filter::do_clean(&mut outf, &mut inf, d, -1)?;
        Ok(())
    })();
    cleanup(&specs);
    if let Err(e) = r {
        exit_with_error(&e);
    }
    exit_with_code(status);
}

pub fn commands() -> Vec<Cmd> {
    let mut logs_cmd = cmd("logs", logs, vec![]);
    logs_cmd.subs = vec![cmd("last", logs_last, vec![]), cmd("show", logs_show, vec![]), cmd("clear", logs_clear, vec![]), cmd("boomtown", logs_boomtown, vec![])];
    let mut ext_cmd = cmd("ext", ext, vec![]);
    ext_cmd.subs = vec![cmd("list", ext_list, vec![])];
    vec![
        logs_cmd,
        ext_cmd,
        cmd("dedup", dedup, vec![flag("test", Some('t'), K::Bool)]),
        cmd(
            "merge-driver",
            merge_driver,
            vec![
                flag("ancestor", None, K::Str),
                flag("current", None, K::Str),
                flag("other", None, K::Str),
                flag("output", None, K::Str),
                flag("program", None, K::Str),
                flag("marker-size", None, K::Int),
            ],
        ),
    ]
}
