//! a8m/envsubst's parser (what yq's `envsubst` operator uses): $VAR, ${VAR}, ${VAR:-default},
//! ${VAR-default}, ${VAR:=default}, ${VAR=default}, ${VAR:+alt}, ${VAR+alt}, $$.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum It {
    Zero = 0,
    Error = 1,
    Eof,
    Text,
    Plus,
    Dash,
    Equals,
    ColonEquals,
    ColonDash,
    ColonPlus,
    Variable,
    LeftDelim,
    RightDelim,
}

#[derive(Clone, Debug)]
struct Item {
    typ: It,
    val: String,
}

/// unicode.IsLetter || unicode.IsDigit, or '_'
fn is_alnum(c: Option<char>) -> bool {
    match c {
        Some(c) => c == '_' || c.is_alphabetic() || c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric() && char_is_nd(c)),
        None => false,
    }
}

fn char_is_nd(c: char) -> bool {
    let u = c as u32;
    matches!(u, 0x0660..=0x0669 | 0x06F0..=0x06F9 | 0x07C0..=0x07C9 | 0x0966..=0x096F | 0x09E6..=0x09EF | 0x0E50..=0x0E59 | 0xFF10..=0xFF19)
}

struct Lexer {
    input: Vec<char>,
    pos: usize,
    start: usize,
    last_pos: usize,
    subs_depth: i32,
    items: Vec<Item>,
}

#[derive(Clone, Copy)]
enum State {
    Text,
    Variable,
    SubOp,
    Sub,
}

impl Lexer {
    fn next(&mut self) -> Option<char> {
        if self.pos >= self.input.len() {
            self.pos += 1; // width 0 at eof in Go; keep backup symmetric with a sentinel
            return None;
        }
        let c = self.input[self.pos];
        self.pos += 1;
        Some(c)
    }
    fn backup(&mut self) {
        self.pos -= 1;
    }
    fn peek(&mut self) -> Option<char> {
        let c = self.next();
        self.backup();
        c
    }
    fn text(&self) -> String {
        self.input[self.start..self.pos.min(self.input.len())].iter().collect()
    }
    fn emit(&mut self, typ: It) {
        let val = self.text();
        self.items.push(Item { typ, val });
        self.last_pos = self.start;
        self.start = self.pos.min(self.input.len());
    }
    fn starts_with_delim_at_last(&self) -> bool {
        self.input.get(self.last_pos) == Some(&'$') && self.input.get(self.last_pos + 1) == Some(&'{')
    }
    fn errorf(&mut self, msg: &str) {
        self.items.push(Item { typ: It::Error, val: msg.into() });
    }

    fn run(&mut self) {
        let mut state = Some(State::Text);
        while let Some(s) = state {
            state = match s {
                State::Text => self.lex_text(),
                State::Variable => self.lex_variable(),
                State::SubOp => self.lex_sub_op(),
                State::Sub => self.lex_sub(),
            };
        }
    }

    fn lex_text(&mut self) -> Option<State> {
        loop {
            match self.next() {
                Some('$') => {
                    self.pos -= 1;
                    if self.pos > self.start {
                        self.emit(It::Text);
                    }
                    self.pos += 1;
                    let r = self.peek();
                    if r == Some('$') {
                        self.start = self.pos;
                        self.next();
                        self.emit(It::Text);
                    } else if r == Some('{') {
                        self.next();
                        self.subs_depth += 1;
                        self.emit(It::LeftDelim);
                        return Some(State::SubOp);
                    } else if is_alnum(r) {
                        return Some(State::Variable);
                    }
                }
                None => {
                    self.pos -= 1;
                    break;
                }
                _ => {}
            }
        }
        if self.pos > self.start {
            self.emit(It::Text);
        }
        self.emit(It::Eof);
        None
    }

    fn lex_variable(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if !is_alnum(r) {
                self.backup();
                break;
            }
        }
        let v = self.text();
        if v == "_" || v == "$_" {
            return Some(State::Text);
        }
        self.emit(It::Variable);
        if self.subs_depth > 0 {
            return Some(State::SubOp);
        }
        Some(State::Text)
    }

    fn lex_sub_op(&mut self) -> Option<State> {
        let r = self.next();
        match r {
            Some('}') => {
                self.subs_depth -= 1;
                self.emit(It::RightDelim);
                return Some(State::Text);
            }
            None | Some('\r') | Some('\n') => {
                if r.is_none() {
                    self.pos -= 1;
                }
                self.errorf("closing brace expected");
                return None;
            }
            _ if is_alnum(r) && self.starts_with_delim_at_last() => return Some(State::Variable),
            Some('+') => self.emit(It::Plus),
            Some('-') => self.emit(It::Dash),
            Some('=') => self.emit(It::Equals),
            Some(':') => match self.next() {
                Some('-') => self.emit(It::ColonDash),
                Some('=') => self.emit(It::ColonEquals),
                Some('+') => self.emit(It::ColonPlus),
                None => self.pos -= 1,
                _ => {}
            },
            _ => {}
        }
        Some(State::Sub)
    }

    fn lex_sub(&mut self) -> Option<State> {
        let r = self.next();
        match r {
            Some('}') => {
                self.subs_depth -= 1;
                self.emit(It::RightDelim);
                Some(State::Text)
            }
            None | Some('\r') | Some('\n') => {
                if r.is_none() {
                    self.pos -= 1;
                }
                self.errorf("closing brace expected");
                None
            }
            Some('$') => Some(State::Variable),
            _ if is_alnum(r) && self.starts_with_delim_at_last() => Some(State::Variable),
            _ => {
                self.emit(It::Text);
                Some(State::Sub)
            }
        }
    }
}

pub struct Restrictions {
    pub no_unset: bool,
    pub no_empty: bool,
}

enum Node {
    Text(String),
    Var(String),
    Subst { exp: It, var: String, default: Option<Box<Node>> },
}

struct P<'a> {
    items: Vec<Item>,
    idx: usize,
    env: &'a dyn Fn(&str) -> Option<String>,
    r: &'a Restrictions,
}

impl<'a> P<'a> {
    fn next(&mut self) -> Item {
        let it = self.items.get(self.idx).cloned().unwrap_or(Item { typ: It::Zero, val: String::new() });
        self.idx += 1;
        it
    }
    fn peek(&self) -> It {
        self.items.get(self.idx).map(|i| i.typ).unwrap_or(It::Zero)
    }

    fn var_string(&self, ident: &str) -> Result<String, String> {
        let v = (self.env)(ident);
        if self.r.no_unset && v.is_none() {
            return Err(format!("variable ${{{ident}}} not set"));
        }
        let value = v.clone().unwrap_or_default();
        if self.r.no_empty && value.is_empty() && v.is_some() {
            return Err(format!("variable ${{{ident}}} set but empty"));
        }
        Ok(value)
    }

    fn node_string(&self, n: &Node) -> Result<String, String> {
        match n {
            Node::Text(t) => Ok(t.clone()),
            Node::Var(v) => self.var_string(v),
            Node::Subst { exp, var, default } => {
                if *exp >= It::Plus {
                    if let Some(d) = default {
                        match exp {
                            It::ColonDash | It::ColonEquals => {
                                if let Ok(s) = self.var_string(var) {
                                    if !s.is_empty() {
                                        return Ok(s);
                                    }
                                }
                                return self.node_string(d);
                            }
                            It::Plus | It::ColonPlus => {
                                if (self.env)(var).is_some() {
                                    return self.node_string(d);
                                }
                                return Ok(String::new());
                            }
                            _ => {
                                if (self.env)(var).is_none() {
                                    return self.node_string(d);
                                }
                            }
                        }
                    }
                }
                self.var_string(var)
            }
        }
    }
}

/// parse.New(...).Parse(text) with the restrictions and mode yq sets.
pub fn substitute(text: &str, env: &dyn Fn(&str) -> Option<String>, r: &Restrictions, fail_fast: bool) -> Result<String, String> {
    let mut lx = Lexer { input: text.chars().collect(), pos: 0, start: 0, last_pos: 0, subs_depth: 0, items: Vec::new() };
    lx.run();
    let mut p = P { items: lx.items, idx: 0, env, r };
    let mut errs: Vec<String> = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    // parse
    let parse_err: Option<String> = loop {
        let t = p.next();
        match t.typ {
            It::Eof => break None,
            It::Error => break Some(t.val),
            It::Variable => nodes.push(Node::Var(t.val.trim_start_matches('$').to_string())),
            It::LeftDelim if p.peek() == It::Variable => {
                let var = p.next().val;
                let mut exp = It::Zero;
                let mut default: Option<Box<Node>> = None;
                let mut err = None;
                loop {
                    let t = p.next();
                    match t.typ {
                        It::RightDelim => break,
                        It::Error => {
                            err = Some(t.val);
                            break;
                        }
                        It::Variable => default = Some(Box::new(Node::Var(t.val.trim_start_matches('$').to_string()))),
                        It::Text => {
                            let mut txt = t.val;
                            loop {
                                match p.peek() {
                                    It::RightDelim | It::Error | It::Eof => break,
                                    _ => txt.push_str(&p.next().val),
                                }
                            }
                            default = Some(Box::new(Node::Text(txt)));
                        }
                        It::Zero => break,
                        other => exp = other,
                    }
                }
                if let Some(e) = err {
                    break Some(e);
                }
                nodes.push(Node::Subst { exp, var, default });
            }
            It::Zero => break None,
            _ => nodes.push(Node::Text(t.val)),
        }
    };
    if let Some(e) = parse_err {
        if fail_fast {
            return Err(e);
        }
        errs.push(e);
    }
    let mut out = String::new();
    for n in &nodes {
        match p.node_string(n) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                if fail_fast {
                    return Err(e);
                }
                errs.push(e);
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(n: &str) -> Option<String> {
        match n {
            "A" => Some("apple".into()),
            "E" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn subst() {
        let r = Restrictions { no_unset: false, no_empty: false };
        assert_eq!(substitute("x $A ${A} ${B:-dflt} ${E:-e} ${A:+alt} $$A", &env, &r, false).unwrap(), "x apple apple dflt e alt $A");
        let r2 = Restrictions { no_unset: true, no_empty: false };
        assert_eq!(substitute("${B}", &env, &r2, false).unwrap_err(), "variable ${B} not set");
        assert_eq!(substitute("${A", &env, &r, false).unwrap_err(), "closing brace expected");
    }
}
