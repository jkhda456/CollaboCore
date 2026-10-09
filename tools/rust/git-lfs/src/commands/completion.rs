//! completion (commands/run.go): the shell completion scripts cobra generates for git-lfs
//! (kept as git-lfs 3.8.0 produces them), and the hidden __complete commands they call.

use super::{print_help, Cmd};
use crate::cli::Parsed;

const BASH: &str = include_str!("../completions/git-lfs-completion.bash");
const FISH: &str = include_str!("../completions/git-lfs-completion.fish");
const ZSH: &str = include_str!("../completions/git-lfs-completion.zsh");

fn usage_error(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    print_help("completion");
    super::exit_with_code(127);
}

fn completion(p: &Parsed) {
    if p.args.len() != 1 {
        usage_error(&format!("accepts 1 arg(s), received {}", p.args.len()));
    }
    let s = match p.args[0].as_str() {
        "bash" => BASH,
        "fish" => FISH,
        "zsh" => ZSH,
        a => usage_error(&format!("invalid argument {} for \"git-lfs completion\"", crate::tools::quote(a))),
    };
    use std::io::Write;
    let _ = std::io::stdout().write_all(s.as_bytes());
}

/// cobra's __complete: candidates for the last word, then the directive.
fn complete_impl(p: &Parsed, desc: bool) {
    let words = &p.args;
    let to = words.last().cloned().unwrap_or_default();
    let cmds = super::all();
    let mut out = vec![];
    let mut directive = 4; // ShellCompDirectiveNoFileComp
    if words.len() <= 1 {
        let mut names: Vec<&str> = cmds.iter().map(|c| c.name).filter(|n| !n.starts_with("__")).collect();
        names.push("completion");
        names.push("help");
        names.sort();
        names.dedup();
        for n in names {
            if n.starts_with(to.as_str()) {
                out.push(n.to_string());
            }
        }
    } else if let Some(c) = cmds.iter().find(|c| c.name == words[0]) {
        if to.starts_with('-') {
            for f in &c.flags {
                let l = format!("--{}", f.long);
                if l.starts_with(&to) {
                    out.push(l);
                }
            }
        } else if words.len() == 2 && !c.subs.is_empty() {
            for s in &c.subs {
                if s.name.starts_with(to.as_str()) {
                    out.push(s.name.to_string());
                }
            }
        } else {
            directive = 0;
        }
    }
    for o in out {
        if desc {
            println!("{o}");
        } else {
            println!("{o}");
        }
    }
    println!(":{directive}");
    let name = if directive == 4 { "ShellCompDirectiveNoFileComp" } else { "ShellCompDirectiveDefault" };
    eprintln!("Completion ended with directive: {name}");
}

fn complete(p: &Parsed) {
    complete_impl(p, true)
}

fn complete_no_desc(p: &Parsed) {
    complete_impl(p, false)
}

pub fn commands() -> Vec<Cmd> {
    let mut v = vec![super::cmd("completion", completion, vec![]), super::cmd("__complete", complete, vec![]), super::cmd("__completeNoDesc", complete_no_desc, vec![])];
    for c in v.iter_mut() {
        c.http_logger = false;
    }
    v
}
