//! The commands, and what they share (commands/commands.go): output, exits, the error log,
//! repository setup.

use crate::cli::{self, Flag, Parsed};
use crate::config::cfg;
use crate::errors::{Error, Kind};
use crate::{gitcmd, tools};
use std::io::Write;
use std::sync::Mutex;

pub mod clone;
pub mod completion;
pub mod env;
pub mod filter;
pub mod hooks;
pub mod fetch;
pub mod fsck;
pub mod info;
pub mod install;
pub mod prune;
pub mod pull;
pub mod standalone;
pub mod lock;
pub mod migrate;
pub mod misc;
pub mod push;
pub mod track;
pub mod version;

pub type Run = fn(&Parsed);

pub struct Cmd {
    pub name: &'static str,
    pub run: Run,
    pub flags: Vec<Flag>,
    pub subs: Vec<Cmd>,
    /// cobra's PreRun setupHTTPLogger (every command but version).
    pub http_logger: bool,
}

pub fn cmd(name: &'static str, run: Run, flags: Vec<Flag>) -> Cmd {
    Cmd { name, run, flags, subs: vec![], http_logger: true }
}

/// What was printed, for the error log (ErrorBuffer).
static ERROR_BUFFER: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn buffer(b: &[u8]) {
    ERROR_BUFFER.lock().unwrap().extend_from_slice(b);
}

/// Print: to stdout (and the error log's buffer).
pub fn print(s: &str) {
    let line = format!("{s}\n");
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(line.as_bytes());
    let _ = o.flush();
    buffer(line.as_bytes());
}

/// Error: to stderr (and the buffer).
pub fn error(s: &str) {
    let line = format!("{s}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    buffer(line.as_bytes());
}

pub fn exit_with_code(code: i32) -> ! {
    cleanup();
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Exit: the message on stderr, status 2.
pub fn exit(msg: &str) -> ! {
    error(msg);
    exit_with_code(2);
}

pub fn exit_with_error(e: &Error) -> ! {
    if e.is(Kind::Fatal) {
        panic_exit(e, &e.to_string());
    }
    exit(&e.to_string());
}

pub fn full_error(e: &Error) {
    if e.is(Kind::Fatal) {
        logged_error(e, &e.to_string());
    } else {
        error(&e.to_string());
    }
}

pub fn panic_exit(e: &Error, msg: &str) -> ! {
    logged_error(e, msg);
    exit_with_code(2);
}

pub fn logged_error(e: &Error, msg: &str) {
    if !msg.is_empty() {
        error(msg);
    }
    let file = log_panic(e);
    if !file.is_empty() {
        eprintln!("\nErrors logged to '{file}'.\nUse `git lfs logs last` to view the log.");
    }
}

static CLEANED: std::sync::Once = std::sync::Once::new();

pub fn cleanup() {
    CLEANED.call_once(|| {
        crate::ssh::shutdown_all();
        tools::remove_exit_files();
        if cfg().in_repo() {
            cfg().filesystem().cleanup();
        }
    });
}

pub fn version_desc() -> String {
    format!("git-lfs/{} (CollaboCore; linux {}; rust)", crate::VERSION, std::env::consts::ARCH)
}

fn log_panic(e: &Error) -> String {
    let dir = cfg().local_log_dir();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let name = format!("{}.{:09}", crate::lfs::time_format(now.as_secs() as i64, "%Y%m%dT%H%M%S", false), now.subsec_nanos());
    let name = name.trim_end_matches('0').trim_end_matches('.').to_string();
    let full = format!("{dir}/{name}.log");
    let le = crate::lfs::git_line_ending();
    let mut body = String::new();
    body.push_str(&version_desc());
    body.push_str(&le);
    body.push_str(&gitcmd::version().unwrap_or_else(|e| format!("Error getting Git version: {e}")));
    body.push_str(&le);
    body.push_str(&le);
    let args: Vec<String> = std::env::args().collect();
    body.push_str(&format!("$ {}", std::path::Path::new(&args[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    if args.len() > 1 {
        body.push_str(&format!(" {}", args[1..].join(" ")));
    }
    body.push_str(&le);
    body.push_str(&String::from_utf8_lossy(&ERROR_BUFFER.lock().unwrap()));
    body.push_str(&le);
    body.push_str(&format!("{e}{le}"));
    body.push_str(&format!("{le}Current time in UTC:{le}"));
    body.push_str(&crate::lfs::time_format(now.as_secs() as i64, "%Y-%m-%d %H:%M:%S", true));
    body.push_str(&le);
    body.push_str(&format!("{le}Environment:{le}"));
    for env in crate::lfs::environ() {
        body.push_str(&env);
        body.push_str(&le);
    }
    body.push_str(&format!("{le}Client IP addresses:{le}"));
    if tools::mkdir_all(&dir, cfg().repository_permissions(false)).is_err() {
        eprintln!("Unable to log panic to '{dir}'\n");
        eprint!("{body}");
        return String::new();
    }
    match std::fs::write(&full, body.as_bytes()) {
        Ok(()) => full,
        Err(_) => {
            eprintln!("Unable to log panic to '{full}'\n");
            eprint!("{body}");
            String::new()
        }
    }
}

pub fn require_in_repo() {
    if !cfg().in_repo() {
        print("Not in a Git repository.");
        exit_with_code(128);
    }
}

pub fn require_working_copy() {
    if cfg().local_working_dir().is_empty() {
        print("This operation must be run in a work tree.");
        exit_with_code(128);
    }
}

pub fn verify_repository_version() {
    let key = "lfs.repositoryformatversion";
    let val = cfg().find_local(key);
    if val.is_empty() {
        let _ = cfg().set_local(key, "0");
    } else if val != "0" {
        print(&format!("Unknown repository format version: {val}"));
        exit_with_code(128);
    }
}

pub fn setup_repository() {
    require_in_repo();
    let bare = match gitcmd::is_bare() {
        Ok(b) => b,
        Err(e) => exit_with_error(&e.wrap("Could not determine bareness")),
    };
    verify_repository_version();
    if !bare {
        change_to_working_copy();
    }
}

pub fn setup_working_copy() {
    require_in_repo();
    require_working_copy();
    verify_repository_version();
    change_to_working_copy();
}

pub fn change_to_working_copy() {
    let wd = cfg().local_working_dir();
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => exit_with_error(&Error::from(e).wrap("Could not determine current working directory")),
    };
    let cwd = match tools::canonicalize_system_path(&cwd) {
        Ok(c) => c.display().to_string(),
        Err(e) => exit_with_error(&Error::from(e).wrap("Could not canonicalize current working directory")),
    };
    if !(cwd.starts_with(&wd) && (cwd == wd || cwd.as_bytes().get(wd.len()) == Some(&b'/'))) {
        let _ = std::env::set_current_dir(&wd);
    }
}

pub fn require_git_version() {
    if !gitcmd::is_git_version_at_least("2.0.0") {
        match gitcmd::version() {
            Ok(v) => exit(&format!("Git version 2.0.0 or higher is required for Git LFS; your version: {v}")),
            Err(e) => exit(&format!("Error getting Git version: {e}")),
        }
    }
}

pub fn require_stdin(msg: &str) {
    if tools::is_tty(0) {
        error(&format!("Cannot read from STDIN: {msg}"));
        exit_with_code(1);
    }
}

pub fn man_page(name: &str) -> Option<&'static str> {
    crate::mancontent::MAN_PAGES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

pub fn print_help(name: &str) {
    let name = if name == "--help" || name == "-h" { "git-lfs" } else { name };
    match man_page(name) {
        Some(t) => print(t.trim()),
        None => print(&format!("Sorry, no usage text found for {}", tools::quote(name))),
    }
}

/// canonicalizeEnvironment: the git path variables made absolute and canonical.
pub fn canonicalize_environment() {
    for v in ["GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
        if let Ok(val) = std::env::var(v) {
            if let Ok(p) = tools::canonicalize_path(&val, true) {
                crate::lfs::OLD_ENV.lock().unwrap().push((v.to_string(), val));
                std::env::set_var(v, p);
            }
        }
    }
}

pub fn all() -> Vec<Cmd> {
    let mut v = vec![];
    v.extend(version::commands());
    v.extend(install::commands());
    v.extend(env::commands());
    v.extend(track::commands());
    v.extend(filter::commands());
    v.extend(hooks::commands());
    v.extend(push::commands());
    v.extend(lock::commands());
    v.extend(standalone::commands());
    v.extend(fetch::commands());
    v.extend(pull::commands());
    v.extend(prune::commands());
    v.extend(fsck::commands());
    v.extend(info::commands());
    v.extend(misc::commands());
    v.extend(clone::commands());
    v.extend(completion::commands());
    v.extend(migrate::commands());
    v
}

pub fn run_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    canonicalize_environment();
    let cmds = all();
    // The root: `--version`/`-v`, or no command: version and usage.
    let first = args.iter().position(|a| !a.starts_with('-'));
    let Some(fi) = first else {
        match cli::parse(&args, &[cli::flag("version", Some('v'), cli::K::Bool)]) {
            Ok(p) => {
                if p.vals.contains_key("help") {
                    print_help("git-lfs");
                    return 0;
                }
                version::version_line();
                if !p.bool("version") {
                    print_help("git-lfs");
                }
                return 0;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                print_help("git-lfs");
                return 127;
            }
        }
    };
    let name = args[fi].as_str();
    let mut rest: Vec<String> = args[..fi].to_vec();
    rest.extend(args[fi + 1..].iter().cloned());
    if name == "help" {
        let topic = rest.iter().find(|a| !a.starts_with('-')).cloned();
        match topic {
            None => print_help("git-lfs"),
            Some(t) => {
                if cmds.iter().any(|c| c.name == t) || t == "config" || t == "faq" || t == "help" || t == "completion" {
                    print_help(&t);
                } else {
                    print(&format!("Unknown help topic [`{t}`]"));
                    print_help("git-lfs");
                }
            }
        }
        return 0;
    }
    let Some(c) = cmds.iter().find(|c| c.name == name) else {
        eprintln!("Error: unknown command {} for \"git-lfs\"\nRun 'git-lfs --help' for usage.", tools::quote(name));
        return 127;
    };
    // A subcommand (`install hooks`)?
    let (c, rest) = match rest.iter().position(|a| !a.starts_with('-')) {
        Some(si) if c.subs.iter().any(|s| s.name == rest[si]) => {
            let sub = c.subs.iter().find(|s| s.name == rest[si]).unwrap();
            let mut r = rest[..si].to_vec();
            r.extend(rest[si + 1..].iter().cloned());
            (sub, r)
        }
        _ => (c, rest),
    };
    let p = match cli::parse(&rest, &c.flags) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            print_help(name);
            return 127;
        }
    };
    if p.vals.contains_key("help") {
        print_help(name);
        return 0;
    }
    if c.http_logger {
        crate::lfs::setup_http_logger();
    }
    (c.run)(&p);
    0
}
