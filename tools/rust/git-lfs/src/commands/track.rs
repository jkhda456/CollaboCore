//! `git lfs track` and `untrack`: patterns in .gitattributes.

use super::*;
use crate::attrs::{self, AttributePath, MacroProcessor};
use crate::cli::{flag, K};

const ESCAPES: &[(&str, &str)] = &[(" ", "[[:space:]]"), ("#", "\\#")];

pub fn escape_glob(s: &str) -> String {
    let mut e = s.replace('\\', "\\\\");
    for ch in ["*", "[", "]", "?"] {
        e = e.replace(ch, &format!("\\{ch}"));
    }
    for (f, t) in ESCAPES {
        e = e.replace(f, t);
    }
    e
}

pub fn escape_attr_pattern(s: &str) -> String {
    let mut e = s.replace('\\', "\\\\");
    for (f, t) in ESCAPES {
        e = e.replace(f, t);
    }
    e
}

pub fn unescape_attr_pattern(s: &str) -> String {
    let mut u = s.to_string();
    for (t, f) in ESCAPES {
        u = u.replace(f, t);
    }
    u.replace("\\\\", "\\")
}

fn known_patterns() -> Vec<AttributePath> {
    let mut mp = MacroProcessor::new();
    let system = attrs::get_system_attribute_paths(&mut mp);
    let user = attrs::get_user_attribute_paths(&mut mp);
    let mut k = attrs::get_attribute_paths(&mut mp, &cfg().local_working_dir(), &cfg().local_git_dir());
    k.extend(user);
    k.extend(system);
    k
}

fn list_patterns(json: bool, no_excluded: bool) {
    let known = known_patterns();
    if json {
        let pats: Vec<serde_json::Value> = known.iter().map(|p| serde_json::json!({"pattern": p.path, "source": p.source, "lockable": p.lockable, "tracked": p.tracked})).collect();
        let v = serde_json::json!({ "patterns": pats });
        let mut out = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
        serde::Serialize::serialize(&v, &mut ser).unwrap();
        out.push(b'\n');
        let _ = std::io::stdout().write_all(&out);
        return;
    }
    if known.is_empty() {
        return;
    }
    print("Listing tracked patterns");
    for t in &known {
        if t.lockable {
            print(&format!("    {} [lockable] ({})", t.path, t.source));
        } else if t.tracked {
            print(&format!("    {} ({})", t.path, t.source));
        }
    }
    if no_excluded {
        return;
    }
    print("Listing excluded patterns");
    for t in &known {
        if !t.tracked && !t.lockable {
            print(&format!("    {} ({})", t.path, t.source));
        }
    }
}

fn blocklist_item(name: &str) -> Option<&'static str> {
    let base = crate::wildmatch::go_base(name);
    [".git", ".lfs"].into_iter().find(|p| base.starts_with(p))
}

fn track(p: &Parsed) {
    require_git_version();
    setup_working_copy();
    let lockable = p.bool("lockable");
    let not_lockable = p.bool("not-lockable");
    let verbose = p.bool("verbose");
    let dry_run = p.bool("dry-run");
    let no_modify = p.bool("no-modify-attrs") || dry_run;
    if !cfg().os.bool("GIT_LFS_TRACK_NO_INSTALL_HOOKS", false) {
        let _ = super::install::install_hooks(false);
    }
    if p.args.is_empty() {
        list_patterns(p.bool("json"), p.bool("no-excluded"));
        return;
    }
    if p.bool("json") {
        exit("--json option can't be combined with arguments");
    }
    let mut mp = MacroProcessor::new();
    attrs::get_system_attribute_paths(&mut mp);
    attrs::get_user_attribute_paths(&mut mp);
    let known = attrs::get_attribute_paths(&mut mp, &cfg().local_working_dir(), &cfg().local_git_dir());
    let mut line_end = known.iter().find(|a| a.source == ".gitattributes").map(|a| a.line_ending.clone()).unwrap_or_default();
    if line_end.is_empty() {
        line_end = crate::lfs::git_line_ending();
    }
    let wd = tools::resolve_symlinks(&std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default());
    let relpath = match attrs::rel(&cfg().local_working_dir(), &wd) {
        Some(r) => r,
        None => exit(&format!("Current directory {} outside of Git working directory {}.", tools::quote(&wd), tools::quote(&cfg().local_working_dir()))),
    };
    let mut changed: Vec<(String, String)> = vec![];
    let mut readonly = vec![];
    let mut writeable = vec![];
    'args: for unsanitized in &p.args {
        let mut pattern = tools::trim_current_prefix(unsanitized).to_string();
        let encoded = if p.bool("filename") {
            pattern = escape_glob(&pattern);
            pattern.clone()
        } else {
            escape_attr_pattern(&pattern)
        };
        if !no_modify {
            let joined = tools::clean_str(&format!("{relpath}/{pattern}"));
            for k in &known {
                if unescape_attr_pattern(&k.path) == joined && ((lockable && k.lockable) || (not_lockable && !k.lockable) || (!lockable && !not_lockable)) {
                    print(&format!("{} already supported", tools::quote(&pattern)));
                    continue 'args;
                }
            }
        }
        let lock_arg = if lockable { " lockable" } else { "" };
        let line = format!("{encoded} filter=lfs diff=lfs merge=lfs -text{lock_arg}{line_end}");
        match changed.iter_mut().find(|(k, _)| *k == pattern) {
            Some(x) => x.1 = line,
            None => changed.push((pattern.clone(), line)),
        }
        if lockable {
            readonly.push(pattern.clone());
        } else {
            writeable.push(pattern.clone());
        }
        print(&format!("Tracking {}", tools::quote(&unescape_attr_pattern(&encoded))));
    }
    let mut out: Option<std::fs::File> = None;
    if !no_modify {
        let contents = match std::fs::read(".gitattributes") {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(_) => {
                print("Error reading '.gitattributes' file");
                return;
            }
        };
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = match std::fs::OpenOptions::new().write(true).truncate(true).create(true).mode(0o660).open(".gitattributes") {
            Ok(f) => f,
            Err(_) => {
                print("Error opening '.gitattributes' file");
                return;
            }
        };
        for line in String::from_utf8_lossy(&contents).split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.is_empty() {
                continue;
            }
            let pat = unescape_attr_pattern(fields[0]);
            if let Some(i) = changed.iter().position(|(k, _)| *k == pat) {
                let (_, nl) = changed.remove(i);
                let _ = f.write_all(nl.as_bytes());
            } else {
                let _ = f.write_all(format!("{line}{line_end}").as_bytes());
            }
        }
        out = Some(f);
    }
    let mut modified = false;
    let mut saw_error = false;
    let n_changed = changed.len();
    for (pattern, newline) in &changed {
        if verbose {
            print(&format!("Searching for files matching pattern: {pattern}"));
        }
        let tracked = match gitcmd::get_tracked_files(pattern) {
            Ok(t) => t,
            Err(e) => exit(&format!("Error getting tracked files for {}: {}", tools::quote(pattern), e)),
        };
        if verbose {
            print(&format!("Found {} files previously added to Git matching pattern: {}", tracked.len(), pattern));
        }
        let mut blocked = false;
        for f in &tracked {
            if blocklist_item(f).is_some() {
                print(&format!("Pattern '{pattern}' matches forbidden file '{f}'. If you would like to track {f}, modify '.gitattributes' manually."));
                blocked = true;
            }
        }
        if blocked {
            continue;
        }
        if let Some(f) = out.as_mut() {
            let _ = f.write_all(newline.as_bytes());
        }
        modified = true;
        for f in &tracked {
            if verbose || dry_run {
                print(&format!("Touching {}", tools::quote(f)));
            }
            if !dry_run {
                // zeroed + fields: the guest's ILP32 timespec has a private padding field.
                let mut now: libc::timespec = unsafe { std::mem::zeroed() };
                now.tv_sec = 0;
                now.tv_nsec = libc::UTIME_NOW;
                let c = std::ffi::CString::new(f.as_str()).unwrap();
                if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [now, now].as_ptr(), 0) } != 0 {
                    let e = std::io::Error::last_os_error();
                    let msg = format!("Error marking {} modified: {}", tools::quote(f), tools::path_err("chtimes", f, &e));
                    logged_error(&Error::new(msg.clone()), &msg);
                    saw_error = true;
                }
            }
        }
    }
    drop(out);
    if let Err(e) = crate::locking::fix_file_write_flags_in_dir(&relpath, &readonly, &writeable) {
        let msg = format!("Error changing lockable file permissions: {e}");
        logged_error(&e, &msg);
        saw_error = true;
    }
    if saw_error {
        exit_with_code(2);
    }
    if !modified && n_changed > 0 {
        exit_with_code(1);
    }
}

fn untrack(p: &Parsed) {
    setup_working_copy();
    let _ = super::install::install_hooks(false);
    if p.args.is_empty() {
        print("git lfs untrack <path> [path]*");
        return;
    }
    let Ok(data) = std::fs::read(".gitattributes") else { return };
    let mut f = match std::fs::File::create(".gitattributes") {
        Ok(f) => f,
        Err(_) => {
            print("Error opening '.gitattributes' for writing");
            return;
        }
    };
    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    for line in lines {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.contains("filter=lfs") {
            let _ = f.write_all(format!("{line}\n").as_bytes());
            continue;
        }
        let path = line.split_whitespace().next().unwrap_or("");
        let without = tools::trim_current_prefix(path);
        if p.args.iter().any(|t| without == escape_attr_pattern(tools::trim_current_prefix(t))) {
            print(&format!("Untracking {}", tools::quote(&unescape_attr_pattern(path))));
        } else {
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }
}

pub fn commands() -> Vec<Cmd> {
    vec![
        cmd(
            "track",
            track,
            vec![
                flag("lockable", Some('l'), K::Bool),
                flag("not-lockable", None, K::Bool),
                flag("verbose", Some('v'), K::Bool),
                flag("dry-run", Some('d'), K::Bool),
                flag("no-modify-attrs", None, K::Bool),
                flag("no-excluded", None, K::Bool),
                flag("filename", None, K::Bool),
                flag("json", Some('j'), K::Bool),
            ],
        ),
        cmd("untrack", untrack, vec![]),
    ]
}
