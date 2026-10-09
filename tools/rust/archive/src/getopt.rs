//! Command lines the GNU way (getopt_long): short options cluster (`-dkc`), a short option's
//! argument is the rest of its word or the next word (`-T0`, `-T 0`), long options take theirs
//! after `=` or as the next word, a long option may be shortened to any unambiguous prefix,
//! options and operands mix unless POSIXLY_CORRECT is set, and `--` ends the options.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    No,
    Required,
    /// Only as `--name=value` (or glued to a short option).
    Optional,
}

pub struct Long {
    pub name: &'static str,
    pub arg: Arg,
    pub id: i32,
}

/// One option: its id (a short option's character, or a Long's id) and its argument.
#[derive(Debug)]
pub enum Item {
    Opt(i32, Option<String>),
    Operand(String),
}

pub struct Parser<'a> {
    args: Vec<String>,
    pos: usize,
    /// Within a cluster of short options: the offset of the next one.
    cluster: usize,
    shorts: &'a str,
    longs: &'a [Long],
    done: bool,
    posix: bool,
}

impl<'a> Parser<'a> {
    /// `shorts` is a getopt string: a character per option, `:` after it when it takes an
    /// argument, `::` when the argument is optional.
    pub fn new(args: Vec<String>, shorts: &'a str, longs: &'a [Long]) -> Self {
        let posix = std::env::var_os("POSIXLY_CORRECT").is_some();
        Parser { args, pos: 0, cluster: 0, shorts, longs, done: false, posix }
    }

    fn short_kind(&self, c: char) -> Option<Arg> {
        let mut it = self.shorts.char_indices().peekable();
        while let Some((i, ch)) = it.next() {
            if ch == ':' {
                continue;
            }
            if ch == c {
                let rest = &self.shorts[i + ch.len_utf8()..];
                return Some(if rest.starts_with("::") {
                    Arg::Optional
                } else if rest.starts_with(':') {
                    Arg::Required
                } else {
                    Arg::No
                });
            }
        }
        None
    }

    /// The next option or operand; Err is the message for a bad option (without the program name).
    pub fn next(&mut self) -> Option<Result<Item, String>> {
        if self.cluster > 0 {
            return Some(self.next_short());
        }
        let arg = self.args.get(self.pos)?.clone();
        if self.done || arg == "-" || !arg.starts_with('-') {
            self.pos += 1;
            if self.posix && !self.done {
                self.done = true;
            }
            return Some(Ok(Item::Operand(arg)));
        }
        if arg == "--" {
            self.pos += 1;
            self.done = true;
            return self.next();
        }
        if let Some(body) = arg.strip_prefix("--") {
            self.pos += 1;
            return Some(self.long(body));
        }
        self.cluster = 1;
        Some(self.next_short())
    }

    fn long(&mut self, body: &str) -> Result<Item, String> {
        let (name, value) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (body, None),
        };
        let exact = self.longs.iter().find(|l| l.name == name);
        let opt = match exact {
            Some(l) => l,
            None => {
                let cands: Vec<&Long> = self.longs.iter().filter(|l| l.name.starts_with(name)).collect();
                match cands.len() {
                    0 => return Err(format!("unrecognized option '--{name}'")),
                    1 => cands[0],
                    _ if cands.iter().all(|l| l.id == cands[0].id && l.arg == cands[0].arg) => cands[0],
                    _ => {
                        let list: Vec<String> = cands.iter().map(|l| format!("'--{}'", l.name)).collect();
                        return Err(format!("option '--{name}' is ambiguous; possibilities: {}", list.join(" ")));
                    }
                }
            }
        };
        match opt.arg {
            Arg::No => {
                if value.is_some() {
                    return Err(format!("option '--{}' doesn't allow an argument", opt.name));
                }
                Ok(Item::Opt(opt.id, None))
            }
            Arg::Optional => Ok(Item::Opt(opt.id, value)),
            Arg::Required => {
                if value.is_some() {
                    return Ok(Item::Opt(opt.id, value));
                }
                match self.args.get(self.pos) {
                    Some(v) => {
                        self.pos += 1;
                        Ok(Item::Opt(opt.id, Some(v.clone())))
                    }
                    None => Err(format!("option '--{}' requires an argument", opt.name)),
                }
            }
        }
    }

    fn next_short(&mut self) -> Result<Item, String> {
        let word = self.args[self.pos].clone();
        let c = word[self.cluster..].chars().next().unwrap();
        self.cluster += c.len_utf8();
        let at_end = self.cluster >= word.len();
        let rest = word[self.cluster..].to_string();
        let finish = |p: &mut Self| {
            p.cluster = 0;
            p.pos += 1;
        };
        match self.short_kind(c) {
            None => {
                if at_end {
                    finish(self);
                }
                Err(format!("invalid option -- '{c}'"))
            }
            Some(Arg::No) => {
                if at_end {
                    finish(self);
                }
                Ok(Item::Opt(c as i32, None))
            }
            Some(Arg::Optional) => {
                finish(self);
                Ok(Item::Opt(c as i32, if rest.is_empty() { None } else { Some(rest) }))
            }
            Some(Arg::Required) => {
                finish(self);
                if !rest.is_empty() {
                    return Ok(Item::Opt(c as i32, Some(rest)));
                }
                match self.args.get(self.pos) {
                    Some(v) => {
                        self.pos += 1;
                        Ok(Item::Opt(c as i32, Some(v.clone())))
                    }
                    None => Err(format!("option requires an argument -- '{c}'")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LONGS: &[Long] = &[
        Long { name: "threads", arg: Arg::Required, id: 'T' as i32 },
        Long { name: "test", arg: Arg::No, id: 't' as i32 },
        Long { name: "lzma2", arg: Arg::Optional, id: 1000 },
        Long { name: "stdout", arg: Arg::No, id: 'c' as i32 },
        Long { name: "to-stdout", arg: Arg::No, id: 'c' as i32 },
    ];
    fn all(args: &[&str]) -> Vec<String> {
        let mut p = Parser::new(args.iter().map(|s| s.to_string()).collect(), "cdT:tk9", LONGS);
        let mut out = vec![];
        while let Some(i) = p.next() {
            out.push(match i {
                Ok(Item::Opt(id, v)) => format!("{}={}", id, v.unwrap_or_default()),
                Ok(Item::Operand(s)) => format!("@{s}"),
                Err(e) => format!("!{e}"),
            });
        }
        out
    }
    #[test]
    fn clusters_and_args() {
        assert_eq!(all(&["-dkT3", "f", "-T", "4", "--", "-x"]), ["100=", "107=", "84=3", "@f", "84=4", "@-x"]);
        assert_eq!(all(&["--thr=2", "--lzma2", "--lzma2=x", "--te", "-"]), ["84=2", "1000=", "1000=x", "116=", "@-"]);
        assert_eq!(all(&["--t"])[0], "!option '--t' is ambiguous; possibilities: '--threads' '--test' '--to-stdout'");
        assert_eq!(all(&["--std", "-9"]), ["99=", "57="]);
        assert_eq!(all(&["-q"]), ["!invalid option -- 'q'"]);
    }
}
