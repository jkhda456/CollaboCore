//! launcher: starts the runtime folder's engine with no arguments needed.
//!
//! It sits beside `manifest.json` in a runtime folder (scripts/package-runtime.sh) and runs
//! `bin/collabo-core-engine` with the arguments the manifest's `entry` gives (the kernel, the
//! images, the add-on folder), then the options `launcher.conf` in the same folder sets, then its
//! own arguments. The engine runs in the runtime folder, so the manifest's relative paths hold.
//!
//! launcher.conf: one `key = value` per line, `#` starts a comment. A key is an engine option
//! without its dashes (`mount = work:/work` is `--mount work:/work`); a flag takes yes/no.
//! Besides those: `python = off` / `tools = off` leave out that overlay, and `command = ...`
//! runs one command (`exec -- ...`) instead of the shell. In a value `~` is the home folder and
//! `${NAME}` an environment variable; relative host paths are relative to the runtime folder, and
//! a mount's host folder there is made when it is missing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Engine options that take a value, by the name launcher.conf uses.
const VALUE_KEYS: &[&str] = &[
    "kernel", "initramfs", "python-image", "tools-image", "addon-dir", "cpus", "mount", "cwd",
    "arg", "allow", "deny", "secret", "addon", "addon-config",
];
/// Engine options that are flags.
const FLAG_KEYS: &[&str] = &["no-network", "allow-loopback", "log-requests", "no-raw"];
/// The keys whose value is a host path, relative to the runtime folder.
const PATH_KEYS: &[&str] = &["kernel", "initramfs", "python-image", "tools-image", "addon-dir"];

const USAGE: &str = "\
launcher: starts this runtime's sandbox as launcher.conf describes

Usage:
  launcher [ENGINE OPTIONS] [-- CMD...]   the arguments go after launcher.conf's; with `--`
                                          they replace its `command`
  launcher --dry-run                      print the engine command line instead (secrets hidden)
  launcher --help                         this text; bin/collabo-core-engine --help has the options

launcher.conf (beside this program): `key = value` per line, `#` for comments.
  <option> = VALUE    an engine option without its dashes, e.g. mount = work:/work (repeatable)
  <flag> = yes|no     no-network, allow-loopback, log-requests, no-raw
  python = off        leave out the CPython overlay; tools = off: curl, ssh, git
  command = CMD...    run one command in the guest instead of the shell
  ~ is the home folder, ${NAME} an environment variable; relative host paths are relative to
  this folder, and a mount's missing host folder here is made.
";

/// What launcher.conf says, as engine arguments.
#[derive(Debug, Default, PartialEq)]
struct Conf {
    args: Vec<String>,
    command: Vec<String>,
    python: bool,
    tools: bool,
    /// Host folders of the mounts, to make when missing.
    mount_dirs: Vec<PathBuf>,
}

fn parse_conf(text: &str, base: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Conf, String> {
    let mut conf = Conf { python: true, tools: true, ..Conf::default() };
    for (index, line) in text.lines().enumerate() {
        let at = |message: String| format!("launcher.conf line {}: {message}", index + 1);
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| at(format!("expected key = value: {line}")))?;
        let key = key.trim();
        let value = expand(value.trim(), env).map_err(at)?;
        match key {
            "python" => conf.python = yes(&value).map_err(at)?,
            "tools" => conf.tools = yes(&value).map_err(at)?,
            "command" => conf.command = split_words(&value).map_err(at)?,
            flag if FLAG_KEYS.contains(&flag) => {
                if yes(&value).map_err(at)? {
                    conf.args.push(format!("--{flag}"));
                }
            }
            "mount" => {
                let (host, rest) = split_mount(&value).ok_or_else(|| at(format!("expected HOST:GUEST[:ro]: {value}")))?;
                let host = resolve(host, base);
                conf.mount_dirs.push(host.clone());
                conf.args.extend(["--mount".into(), format!("{}:{rest}", host.display())]);
            }
            path if PATH_KEYS.contains(&path) => {
                conf.args.extend([format!("--{path}"), resolve(&value, base).display().to_string()]);
            }
            option if VALUE_KEYS.contains(&option) => conf.args.extend([format!("--{option}"), value]),
            other => return Err(at(format!("unknown key {other} (launcher --help)"))),
        }
    }
    Ok(conf)
}

/// A `#` starts a comment, except inside quotes (a command may hold one).
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (c, Some(q)) if c == q => quote = None,
            ('#', None) => return &line[..i],
            _ => {}
        }
    }
    line
}

fn yes(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "on" | "true" | "1" => Ok(true),
        "no" | "off" | "false" | "0" => Ok(false),
        _ => Err(format!("expected yes or no: {value}")),
    }
}

/// `~` at the start is the home folder; `${NAME}` is that environment variable.
fn expand(value: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    if rest == "~" || rest.starts_with("~/") || rest.starts_with("~\\") {
        out.push_str(&env("HOME").or_else(|| env("USERPROFILE")).ok_or("no home folder for ~")?);
        rest = &rest[1..];
    }
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').ok_or_else(|| format!("unclosed ${{ in {value}"))? + start;
        let name = &rest[start + 2..end];
        out.push_str(&env(name).ok_or_else(|| format!("environment variable {name} is not set"))?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `HOST:GUEST[:ro]` → (HOST, `GUEST[:ro]`); as the engine reads it, from the right, so a
/// Windows drive letter stays with the host path.
fn split_mount(spec: &str) -> Option<(&str, &str)> {
    let body = spec.strip_suffix(":ro").unwrap_or(spec);
    let cut = body.rfind(':').filter(|cut| *cut > 0)?;
    Some((&spec[..cut], &spec[cut + 1..]))
}

fn resolve(path: &str, base: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}

/// Words split at spaces; '…' and "…" keep theirs.
fn split_words(text: &str) -> Result<Vec<String>, String> {
    let (mut words, mut word, mut quote, mut started) = (Vec::new(), String::new(), None, false);
    for c in text.chars() {
        match (c, quote) {
            (c, Some(q)) if c == q => quote = None,
            (c, Some(_)) => word.push(c),
            ('"' | '\'', None) => (quote, started) = (Some(c), true),
            (c, None) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (c, None) => (word, started) = (word + &c.to_string(), true),
        }
    }
    if quote.is_some() {
        return Err(format!("unclosed quote in {text}"));
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// The manifest's `entry` without the program, less the overlays launcher.conf turns off.
fn entry_args(manifest: &str, python: bool, tools: bool) -> Result<Vec<String>, String> {
    let manifest: serde_json::Value = serde_json::from_str(manifest).map_err(|e| format!("manifest.json: {e}"))?;
    let entry: Vec<String> = manifest["entry"]
        .as_array()
        .ok_or("manifest.json has no entry")?
        .iter()
        .map(|word| word.as_str().map(str::to_string).ok_or("manifest.json: entry holds a non-string"))
        .collect::<Result<_, _>>()?;
    let mut args = Vec::new();
    let mut words = entry.into_iter().skip(1);
    while let Some(word) = words.next() {
        let drop = (word == "--python-image" && !python) || (word == "--tools-image" && !tools);
        if drop {
            words.next();
        } else {
            args.push(word);
        }
    }
    Ok(args)
}

/// The engine's arguments: the manifest's, launcher.conf's, then the launcher's own. Arguments
/// with a `--` bring their own command; otherwise launcher.conf's `command` runs, through exec.
fn engine_args(entry: Vec<String>, conf: &Conf, extra: &[String]) -> Vec<String> {
    let mut args = entry;
    args.extend(conf.args.iter().cloned());
    args.extend(extra.iter().cloned());
    if !conf.command.is_empty() && !extra.iter().any(|a| a == "--") {
        if !args.iter().any(|a| a == "exec") {
            args.insert(0, "exec".into());
        }
        args.push("--".into());
        args.extend(conf.command.iter().cloned());
    }
    args
}

/// For --dry-run: a --secret's value, and anything in `--addon-config NAME:apiKey=`, hidden.
fn shown(args: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for arg in args {
        let secret = match out.last().map(String::as_str) {
            Some("--secret") => true,
            Some("--addon-config") => arg.to_ascii_lowercase().contains(":apikey="),
            _ => false,
        };
        let text = match arg.split_once('=') {
            Some((name, _)) if secret => format!("{name}=…"),
            _ => arg.clone(),
        };
        out.push(if text.contains(' ') { format!("\"{text}\"") } else { text });
    }
    out.join(" ")
}

fn run() -> Result<i32, String> {
    let extra: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = extra.first().is_some_and(|a| a == "--dry-run");
    let extra = if dry_run { extra[1..].to_vec() } else { extra };
    if matches!(extra.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        print!("{USAGE}");
        return Ok(0);
    }

    let program = std::env::current_exe().map_err(|e| format!("where is this program: {e}"))?;
    let program = program.canonicalize().unwrap_or(program);
    let base = program.parent().ok_or("this program has no folder")?.to_path_buf();
    let manifest = std::fs::read_to_string(base.join("manifest.json"))
        .map_err(|e| format!("{}: {e} (the launcher belongs in a runtime folder)", base.join("manifest.json").display()))?;

    let conf_path = base.join("launcher.conf");
    let conf = match std::fs::read_to_string(&conf_path) {
        Ok(text) => parse_conf(&text, &base, &|name| std::env::var(name).ok())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Conf { python: true, tools: true, ..Conf::default() },
        Err(e) => return Err(format!("{}: {e}", conf_path.display())),
    };
    let args = engine_args(entry_args(&manifest, conf.python, conf.tools)?, &conf, &extra);
    let engine = base.join("bin").join(if cfg!(windows) { "collabo-core-engine.exe" } else { "collabo-core-engine" });

    if dry_run {
        println!("{} {}", engine.display(), shown(&args));
        return Ok(0);
    }
    // A mount inside the runtime folder (work/) may not have survived an archive that drops
    // empty folders.
    for dir in &conf.mount_dirs {
        if dir.starts_with(&base) && !dir.exists() {
            std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
        }
    }

    let mut command = Command::new(&engine);
    command.args(&args).current_dir(&base);
    #[cfg(unix)]
    {
        // The engine takes this process's place: the terminal, signals and exit status are its.
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(format!("{}: {error}", engine.display()))
    }
    #[cfg(windows)]
    {
        // The engine shares this console. Ctrl-C there is a key the engine hands the guest (or,
        // with no-raw, what ends the engine); either way this process waits for the engine
        // rather than dying and leaving it behind. The console modes the engine changes are put
        // back even when it did not get to.
        let saved = console::save();
        console::ignore_ctrl_c();
        let status = command.status().map_err(|e| format!("{}: {e}", engine.display()));
        console::restore(saved);
        Ok(status?.code().unwrap_or(1))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let status = command.status().map_err(|e| format!("{}: {e}", engine.display()))?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(windows)]
mod console {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetStdHandle, SetConsoleCP, SetConsoleCtrlHandler,
        SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE, CTRL_BREAK_EVENT, CTRL_C_EVENT, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    /// The input mode, output mode and code pages, where there is a console.
    pub struct Saved(Option<CONSOLE_MODE>, Option<CONSOLE_MODE>, u32, u32);

    pub fn save() -> Saved {
        // SAFETY: console queries on this process's standard handles.
        unsafe {
            let mode = |handle| {
                let mut mode: CONSOLE_MODE = 0;
                (GetConsoleMode(handle, &mut mode) != 0).then_some(mode)
            };
            Saved(mode(GetStdHandle(STD_INPUT_HANDLE)), mode(GetStdHandle(STD_OUTPUT_HANDLE)), GetConsoleCP(), GetConsoleOutputCP())
        }
    }

    pub fn restore(saved: Saved) {
        // SAFETY: the settings save() read.
        unsafe {
            if let Some(mode) = saved.0 {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
            }
            if let Some(mode) = saved.1 {
                SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), mode);
            }
            if saved.2 != 0 {
                SetConsoleCP(saved.2);
                SetConsoleOutputCP(saved.3);
            }
        }
    }

    /// Handles Ctrl-C and Ctrl-Break by doing nothing. Unlike ignoring them outright, a handler
    /// is not inherited, so the engine still gets them.
    pub fn ignore_ctrl_c() {
        unsafe extern "system" fn handler(event: u32) -> BOOL {
            (event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT) as BOOL
        }
        // SAFETY: the handler is a plain function that lives as long as the process.
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("launcher: {message}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "HOME" => Some("/home/me".into()),
            "KEY" => Some("sk-1".into()),
            _ => None,
        }
    }

    #[test]
    fn conf_lines_become_engine_options() {
        let conf = parse_conf(
            "# comment\n\
             mount = work:/work\n\
             mount = ~/proj:/proj:ro   # read-only\n\
             cpus = 4\n\
             no-network = yes\n\
             log-requests = no\n\
             python = off\n\
             addon-config = claude-code:apiKey=${KEY}\n\
             command = sh -c 'echo # not a comment'\n",
            Path::new("/rt"),
            &env,
        )
        .unwrap();
        assert_eq!(
            conf.args,
            [
                "--mount", "/rt/work:/work", "--mount", "/home/me/proj:/proj:ro", "--cpus", "4",
                "--no-network", "--addon-config", "claude-code:apiKey=sk-1",
            ]
        );
        assert_eq!(conf.command, ["sh", "-c", "echo # not a comment"]);
        assert!(!conf.python && conf.tools);
        assert_eq!(conf.mount_dirs, [PathBuf::from("/rt/work"), PathBuf::from("/home/me/proj")]);
    }

    #[test]
    fn conf_mistakes_name_their_line() {
        let base = Path::new("/rt");
        assert!(parse_conf("x = 1\n", base, &env).unwrap_err().contains("line 1: unknown key x"));
        assert!(parse_conf("\nmount\n", base, &env).unwrap_err().contains("line 2: expected key = value"));
        assert!(parse_conf("no-raw = maybe\n", base, &env).unwrap_err().contains("expected yes or no"));
        assert!(parse_conf("secret = a:b=${NOPE}\n", base, &env).unwrap_err().contains("NOPE is not set"));
        assert!(parse_conf("mount = /work\n", base, &env).unwrap_err().contains("HOST:GUEST"));
    }

    #[test]
    fn entry_loses_the_overlays_turned_off() {
        let manifest = r#"{"entry": ["bin/e", "--kernel", "k", "--python-image", "p", "--tools-image", "t", "--addon-dir", "a"]}"#;
        assert_eq!(entry_args(manifest, true, true).unwrap(), ["--kernel", "k", "--python-image", "p", "--tools-image", "t", "--addon-dir", "a"]);
        assert_eq!(entry_args(manifest, false, false).unwrap(), ["--kernel", "k", "--addon-dir", "a"]);
    }

    #[test]
    fn a_command_runs_through_exec_unless_the_arguments_bring_one() {
        let conf = Conf { args: vec!["--cpus".into(), "1".into()], command: vec!["ls".into()], ..Conf::default() };
        let entry = || vec!["--kernel".to_string(), "k".into()];
        assert_eq!(engine_args(entry(), &conf, &[]), ["exec", "--kernel", "k", "--cpus", "1", "--", "ls"]);
        let own = ["exec".to_string(), "--".into(), "pwd".into()];
        assert_eq!(engine_args(entry(), &conf, &own), ["--kernel", "k", "--cpus", "1", "exec", "--", "pwd"]);
        let shell = Conf { command: vec![], ..conf };
        assert_eq!(engine_args(entry(), &shell, &[]), ["--kernel", "k", "--cpus", "1"]);
    }

    #[test]
    fn dry_run_hides_secrets() {
        let args: Vec<String> = ["--secret", "api.x:authorization=Bearer k", "--addon-config", "claude-code:apiKey=k", "--addon-config", "claude-code:model=m"]
            .map(String::from)
            .into();
        assert_eq!(shown(&args), "--secret api.x:authorization=… --addon-config claude-code:apiKey=… --addon-config claude-code:model=m");
    }

    #[test]
    fn mounts_keep_a_windows_drive_letter() {
        assert_eq!(split_mount(r"C:\data:/data:ro"), Some((r"C:\data", "/data:ro")));
        assert_eq!(split_mount("work:/work"), Some(("work", "/work")));
    }

    /// Every option the engine takes (engine/src/main.rs) has its launcher.conf key.
    #[test]
    fn every_engine_option_has_a_key() {
        for line in include_str!("../main.rs").lines().map(str::trim) {
            let Some(rest) = line.strip_prefix("\"--").filter(|_| line.contains("\" => ")) else { continue };
            let option = &rest[..rest.find('"').unwrap()];
            if option.is_empty() || option == "stdio" {
                continue;
            }
            let takes_value = line.contains("argv.next()") || line.ends_with("=> {");
            let keys = if takes_value { VALUE_KEYS } else { FLAG_KEYS };
            assert!(keys.contains(&option), "--{option} has no launcher.conf key");
        }
    }
}
