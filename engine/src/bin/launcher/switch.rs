//! `launcher --addons enable|disable NAME...`: turns add-ons on and off in the conf the launcher
//! reads (launcher.conf beside it, or the `--config` file), changing as little of it as it can.
//!
//! On: an `addon = NAME` line. A commented-out one (`# addon = NAME`, as launcher.conf's example
//! is) gets its `#` taken off; else the line goes after the last line of the `# --- Add-ons ---`
//! block, or at the end of the file when it has no such block. Off: each `addon = NAME` line is
//! commented out, and so is each `addon-config = NAME:...` line (a setting turns its add-on on as
//! well), marked so that `enable` turns those settings back on; a commented-out example without
//! the mark stays as it is. Every other byte of the file stays, its line ends (CRLF) too.
use std::path::Path;

use crate::addons::Place;

/// What `disable` puts after a setting it comments out, for `enable` to find.
const MARK: &str = "# (--addons disable)";

/// A line and its end: `\n`, `\r\n`, or nothing for a last line without one.
struct Line {
    text: String,
    end: String,
}

fn lines(text: &str) -> Vec<Line> {
    text.split_inclusive('\n')
        .map(|line| {
            let text = line.trim_end_matches(['\r', '\n']);
            Line { text: text.to_string(), end: line[text.len()..].to_string() }
        })
        .collect()
}

fn joined(lines: &[Line]) -> String {
    lines.iter().map(|line| format!("{}{}", line.text, line.end)).collect()
}

/// A `key = value` line's key and value, its comment cut off.
fn setting(line: &str) -> Option<(&str, &str)> {
    let (key, value) = crate::strip_comment(line).trim().split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// What a commented-out line says after its `#`s.
fn commented(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix('#').map(|rest| rest.trim_start_matches('#'))
}

fn is_addon(line: &str, name: &str) -> bool {
    setting(line) == Some(("addon", name))
}

fn is_config(line: &str, name: &str) -> bool {
    matches!(setting(line), Some(("addon-config", value)) if value.strip_prefix(name).is_some_and(|rest| rest.starts_with(':')))
}

/// A setting of `name` that `disable` commented out, as it was.
fn restored(line: &str, name: &str) -> Option<String> {
    let setting = commented(line)?.trim_end().strip_suffix(MARK)?.trim();
    is_config(setting, name).then(|| setting.to_string())
}

/// Where a line goes at the end of the Add-ons block: after its last line that is not blank.
fn addons_block_end(lines: &[Line]) -> Option<usize> {
    let header = |line: &Line| commented(&line.text).is_some_and(|rest| rest.trim_start().starts_with("---"));
    let start = lines.iter().position(|line| header(line) && line.text.to_ascii_lowercase().contains("add-ons"))?;
    let end = lines[start + 1..].iter().position(header).map_or(lines.len(), |i| start + 1 + i);
    let last = (start..end).rev().find(|&i| !lines[i].text.trim().is_empty()).unwrap_or(start);
    Some(last + 1)
}

/// The conf with `name` on, and what changed (nothing: it was on).
fn turn_on(text: &str, name: &str) -> (String, Vec<String>) {
    let mut lines = lines(text);
    let mut done = Vec::new();
    for (i, line) in lines.iter_mut().enumerate() {
        if let Some(setting) = restored(&line.text, name) {
            line.text = setting;
            done.push(format!("line {}: its addon-config back on", i + 1));
        }
    }
    if !lines.iter().any(|line| is_addon(&line.text, name)) {
        if let Some(i) = lines.iter().position(|line| commented(&line.text).is_some_and(|rest| is_addon(rest, name))) {
            lines[i].text = commented(&lines[i].text).unwrap().trim_start().to_string();
            done.push(format!("line {}: uncommented {}", i + 1, lines[i].text.trim_end()));
        } else {
            let end = if lines.iter().any(|line| line.end == "\r\n") { "\r\n" } else { "\n" };
            let (at, place) = match addons_block_end(&lines) {
                Some(at) => (at, "at the end of the Add-ons block"),
                None => (lines.len(), "at the end"),
            };
            if at > 0 && lines[at - 1].end.is_empty() {
                lines[at - 1].end = end.to_string();
            }
            lines.insert(at, Line { text: format!("addon = {name}"), end: end.to_string() });
            done.push(format!("line {}: added addon = {name} ({place})", at + 1));
        }
    }
    (joined(&lines), done)
}

/// The conf with `name` off, and what changed (nothing: it was off).
fn turn_off(text: &str, name: &str) -> (String, Vec<String>) {
    let mut lines = lines(text);
    let mut done = Vec::new();
    for (i, line) in lines.iter_mut().enumerate() {
        if is_addon(&line.text, name) {
            line.text = format!("# {}", line.text);
            done.push(format!("line {}: commented out addon = {name}", i + 1));
        } else if is_config(&line.text, name) {
            // Not shown: a setting may be a key.
            line.text = format!("# {}  {MARK}", line.text.trim_end());
            done.push(format!("line {}: commented out its addon-config (enable turns it back on)", i + 1));
        }
    }
    (joined(&lines), done)
}

/// An add-on's name as the add-on folder has it (`<name>.cpio`).
fn valid(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// `launcher --addons enable|disable NAME...`.
pub fn switch(names: &[String], on: bool, place: &Place) -> Result<i32, String> {
    let conf = &place.conf;
    for name in names {
        if !valid(name) {
            return Err(format!("{name:?} is not an add-on name"));
        }
        if on && !place.dir.join(format!("{name}.cpio")).exists() {
            return Err(format!("{name} is not in {} (launcher --addons get {name} downloads it)", place.dir.display()));
        }
    }
    // launcher.conf beside the launcher may be missing (the defaults); `enable` makes it.
    let mut text = match std::fs::read_to_string(conf) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", conf.display())),
    };
    let mut report = Vec::new();
    for name in names {
        let (changed, done) = if on { turn_on(&text, name) } else { turn_off(&text, name) };
        if done.is_empty() {
            report.push(format!("  {name}: {} already", if on { "on" } else { "off" }));
        } else {
            report.push(format!("  {name}: {}", if on { "on" } else { "off" }));
            report.extend(done.iter().map(|what| format!("    {what}")));
        }
        text = changed;
    }
    let before = std::fs::read_to_string(conf).unwrap_or_default();
    if text != before {
        // What the launcher reads must still read (it did before).
        let dir = conf.parent().unwrap_or(Path::new("."));
        crate::parse_conf(&text, dir, &|name| std::env::var(name).ok()).map_err(|e| format!("{} would not read: {e}", conf.display()))?;
        // In place, so the file keeps its permissions (it may hold a key).
        std::fs::write(conf, &text).map_err(|e| format!("{}: {e}", conf.display()))?;
    }
    println!("{}:", conf.display());
    for line in report {
        println!("{line}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = include_str!("../../../launcher.conf");

    fn args(text: &str) -> Vec<String> {
        crate::parse_conf(text, Path::new("/rt"), &|_| Some("k".into())).unwrap().args
    }

    fn has_addon(text: &str, name: &str) -> bool {
        let args = args(text);
        args.windows(2).any(|pair| pair[0] == "--addon" && pair[1] == name)
            || args.windows(2).any(|pair| pair[0] == "--addon-config" && pair[1].starts_with(&format!("{name}:")))
    }

    /// The lines that differ, as (line number, before, after); lines added show "" before.
    fn diff(before: &str, after: &str) -> Vec<(usize, String, String)> {
        let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
        if a.len() == b.len() {
            return (0..a.len()).filter(|&i| a[i] != b[i]).map(|i| (i + 1, a[i].into(), b[i].into())).collect();
        }
        let at = (0..b.len()).find(|&i| a.get(i) != Some(&b[i])).unwrap();
        assert_eq!(a[at..], b[at + 1..], "more than one line added");
        vec![(at + 1, String::new(), b[at].into())]
    }

    #[test]
    fn the_example_line_is_uncommented_and_commented_again() {
        assert!(!has_addon(DEFAULT, "agent"));
        let (on, done) = turn_on(DEFAULT, "agent");
        let line = DEFAULT.lines().position(|line| line == "# addon = agent").unwrap() + 1;
        assert_eq!(diff(DEFAULT, &on), [(line, "# addon = agent".into(), "addon = agent".into())]);
        assert_eq!(done, [format!("line {line}: uncommented addon = agent")]);
        assert!(has_addon(&on, "agent"));
        assert_eq!(turn_on(&on, "agent"), (on.clone(), vec![]));
        let (off, done) = turn_off(&on, "agent");
        assert_eq!(off, DEFAULT);
        assert_eq!(done, [format!("line {line}: commented out addon = agent")]);
        assert_eq!(turn_off(&off, "agent"), (off.clone(), vec![]));
    }

    #[test]
    fn a_new_line_goes_at_the_end_of_the_add_ons_block() {
        let (on, _) = turn_on(DEFAULT, "nmap");
        let changes = diff(DEFAULT, &on);
        assert_eq!(changes.len(), 1);
        let at = changes[0].0;
        let lines: Vec<&str> = on.lines().collect();
        assert_eq!(lines[at - 1], "addon = nmap");
        assert!(lines[at - 2].starts_with("# addon-config = agent:"), "{}", lines[at - 2]);
        assert_eq!((lines[at], lines[at + 1]), ("", "# --- Updates ---"));
        assert!(has_addon(&on, "nmap") && !has_addon(&on, "agent"));
        // and again after the lines added there
        let (both, _) = turn_on(&on, "tcpdump");
        assert_eq!(both.lines().nth(at).unwrap(), "addon = tcpdump");
    }

    #[test]
    fn without_an_add_ons_block_it_goes_at_the_end() {
        assert_eq!(turn_on("cpus = 2", "nmap").0, "cpus = 2\naddon = nmap\n");
        assert_eq!(turn_on("cpus = 2\n\n# --- Network ---\n# deny = x\n", "nmap").0, "cpus = 2\n\n# --- Network ---\n# deny = x\naddon = nmap\n");
        assert_eq!(turn_on("", "nmap").0, "addon = nmap\n");
        assert_eq!(turn_on("cpus = 2\r\nno-raw = yes\r\n", "nmap").0, "cpus = 2\r\nno-raw = yes\r\naddon = nmap\r\n");
        // the block's end is the next header; blank lines before it stay after the new line
        let conf = "# --- Add-ons ---\naddon = agent\n\n\n# --- Command ---\n";
        assert_eq!(turn_on(conf, "nmap").0, "# --- Add-ons ---\naddon = agent\naddon = nmap\n\n\n# --- Command ---\n");
    }

    #[test]
    fn its_settings_go_off_with_it_and_come_back() {
        let conf = "addon = agent   # mine\r\naddon-config = agent:apiKey=${KEY}\r\naddon-config = agentx:a=1\r\n# addon-config = agent:model=example\r\naddon = agent\r\n";
        let (off, done) = turn_off(conf, "agent");
        assert_eq!(
            off,
            "# addon = agent   # mine\r\n# addon-config = agent:apiKey=${KEY}  # (--addons disable)\r\naddon-config = agentx:a=1\r\n\
             # addon-config = agent:model=example\r\n# addon = agent\r\n"
        );
        assert_eq!(done.len(), 3);
        assert!(done.iter().all(|line| !line.contains("KEY")), "{done:?}");
        assert!(!has_addon(&off, "agent") && has_addon(&off, "agentx"));
        let (on, done) = turn_on(&off, "agent");
        // the setting as it was, and the first addon line; the example stays an example
        assert_eq!(
            on,
            "addon = agent   # mine\r\naddon-config = agent:apiKey=${KEY}\r\naddon-config = agentx:a=1\r\n\
             # addon-config = agent:model=example\r\n# addon = agent\r\n"
        );
        assert_eq!(done, ["line 2: its addon-config back on", "line 1: uncommented addon = agent   # mine"]);
    }

    #[test]
    fn names_are_add_on_file_names() {
        assert!(valid("agent") && valid("c-sh") && valid("python-extension") && valid("a.b_c"));
        for bad in ["", ".hidden", "a b", "a:b", "a/b", "a#b", "a=b"] {
            assert!(!valid(bad), "{bad}");
        }
    }
}
