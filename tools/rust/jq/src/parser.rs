//! jq's grammar (parser.y) by precedence climbing, into an AST. Precedence, lowest first:
//! `|` (right), `,`, `//` (right), the assignments (non-assoc), `or`, `and`, the comparisons
//! (non-assoc), `+ -`, `* / %`; then unary minus and the postfix forms of a term. `def`,
//! `label`, and `E as $x | body` take the rest of the query; `try` and `catch` bodies are
//! terms (they bind tighter than any operator), `reduce`/`foreach` sources expressions.

use crate::lexer::{Lexer, StrPart, Tok, Token};
use crate::value::Value;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
pub enum Ast {
    Identity,
    RecurseDefault,
    Lit(Value),
    /// A string with interpolations, under a format (@text when none).
    Str(Option<String>, Vec<StrAst>),
    Format(String),
    Index(Box<Ast>, Box<Ast>),
    Slice(Box<Ast>, Option<Box<Ast>>, Option<Box<Ast>>),
    Iterate(Box<Ast>),
    /// An Index, Slice or Iterate followed by `?`: its own failure gives no output.
    IndexOpt(Box<Ast>),
    Try(Box<Ast>, Option<Box<Ast>>),
    Neg(Box<Ast>),
    Array(Option<Box<Ast>>),
    Object(Vec<(Ast, Ast)>),
    Pipe(Box<Ast>, Box<Ast>),
    Comma(Box<Ast>, Box<Ast>),
    Bin(BinOp, Box<Ast>, Box<Ast>),
    And(Box<Ast>, Box<Ast>),
    Or(Box<Ast>, Box<Ast>),
    Alt(Box<Ast>, Box<Ast>),
    /// `=`, `|=`, `op=`, `//=`: the operator ("=", "|=", "+", ..., "//").
    Assign(&'static str, Box<Ast>, Box<Ast>),
    If(Box<Ast>, Box<Ast>, Option<Box<Ast>>),
    Reduce(Box<Ast>, Rc<Vec<Pattern>>, Box<Ast>, Box<Ast>),
    Foreach(Box<Ast>, Rc<Vec<Pattern>>, Box<Ast>, Box<Ast>, Option<Box<Ast>>),
    Defs(Vec<Rc<FuncDef>>, Box<Ast>),
    Call(String, Vec<Ast>, Span),
    Var(String, Span),
    As(Box<Ast>, Rc<Vec<Pattern>>, Box<Ast>),
    Label(String, Box<Ast>),
    Break(String, Span),
}

#[derive(Clone, Debug)]
pub enum StrAst {
    Text(String),
    Interp(Ast),
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Var(String),
    Array(Vec<Pattern>),
    /// Keys: `$name` (key "name", bound), or an expression; with a sub-pattern or not.
    Object(Vec<(ObjKey, Option<Pattern>)>),
}

#[derive(Clone, Debug)]
pub enum ObjKey {
    Var(String),
    Expr(Ast),
}

#[derive(Clone, Debug)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Ast,
}

#[derive(Clone, Debug)]
pub enum Param {
    Filter(String),
    Value(String),
}

pub type Span = (usize, usize);

/// A parsed program or module: its metadata, imports, definitions and main query.
pub struct Program {
    pub module: Option<Value>,
    pub imports: Vec<Import>,
    pub defs: Vec<Rc<FuncDef>>,
    pub main: Option<Ast>,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub path: String,
    /// `as name` (a library), `as $name` (data), or None (include).
    pub alias: Option<String>,
    pub data: bool,
    pub meta: Option<Value>,
}

/// A compile error: message and span (byte offsets) in the source.
#[derive(Debug, Clone)]
pub struct PErr {
    pub msg: String,
    pub span: Span,
    /// The errors bison's recovery adds ("Possibly unterminated 'if' statement").
    pub notes: Vec<(String, Span)>,
    /// The token the syntax error is at.
    pub tok: Option<usize>,
}

impl PErr {
    fn is_syntax(&self) -> bool {
        self.msg.starts_with("syntax error")
    }
}

/// Where bison lists a token among the expected ones (its token numbers' order).
fn tok_rank(t: &str) -> usize {
    match t {
        "then" => 20,
        "end" => 25,
        "QQSTRING_INTERP_END" => 44,
        "'|'" => 47,
        "','" => 48,
        "';'" => 61,
        "':'" => 62,
        "')'" => 64,
        "']'" => 65,
        "'}'" => 67,
        _ => 100,
    }
}

type P<T> = Result<T, PErr>;

pub struct Parser {
    toks: Vec<Token>,
    i: usize,
    /// For $__loc__: the file name and where each line starts.
    file: Rc<str>,
    lines: Rc<Vec<usize>>,
}

fn tok_desc(t: &Tok) -> String {
    match t {
        Tok::Ident(_) => "IDENT".into(),
        Tok::Field(_) => "FIELD".into(),
        Tok::Binding(_) => "BINDING".into(),
        Tok::Format(_) => "FORMAT".into(),
        Tok::Literal(_) => "LITERAL".into(),
        Tok::Str(_) => "QQSTRING_START".into(),
        Tok::Kw(k) => k.to_string(),
        Tok::Op(o) if o.len() == 1 => format!("'{o}'"),
        Tok::Op(o) => o.to_string(),
        Tok::Loc => "\"$__loc__\"".into(),
        Tok::Eof => "end of file".into(),
    }
}

impl Parser {
    pub fn new(src: &str, file: &str) -> P<Parser> {
        let toks = Lexer::new(src).tokens(false).map_err(|(msg, a, b)| PErr { notes: vec![], tok: None, msg, span: (a, b) })?;
        let mut lines = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                lines.push(i + 1);
            }
        }
        Ok(Parser { toks, i: 0, file: Rc::from(file), lines: Rc::new(lines) })
    }

    fn sub(&self, mut toks: Vec<Token>, end: usize) -> Parser {
        toks.push(Token { tok: Tok::Eof, start: end, end });
        Parser { toks, i: 0, file: self.file.clone(), lines: self.lines.clone() }
    }

    fn loc(&self, at: usize) -> Ast {
        let line = match self.lines.binary_search(&at) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        let mut m = crate::value::Map::new();
        m.insert(Rc::from("file"), Value::Str(self.file.clone()));
        m.insert(Rc::from("line"), Value::num(line as f64));
        Ast::Lit(Value::obj(m))
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.i.min(self.toks.len() - 1)].tok
    }
    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.i + k).min(self.toks.len() - 1)].tok
    }
    fn span(&self) -> Span {
        let t = &self.toks[self.i.min(self.toks.len() - 1)];
        (t.start, t.end)
    }
    fn bump(&mut self) -> Token {
        // Past the end, peek() stays at the end-of-file token (and `i -= 1` undoes a bump).
        let t = self.toks[self.i.min(self.toks.len() - 1)].clone();
        self.i += 1;
        t
    }
    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Tok::Op(x) if *x == o)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Kw(x) if *x == k)
    }
    fn unexpected<T>(&self) -> P<T> {
        self.unexpected_expecting(&[])
    }

    /// bison's "syntax error, unexpected X, expecting A or B" (the expected tokens in its order).
    fn unexpected_expecting<T>(&self, expected: &[&str]) -> P<T> {
        let t = self.peek();
        let (a, mut b) = self.span();
        if matches!(t, Tok::Str(_)) {
            // The token is the opening quote (QQSTRING_START).
            b = a + 1;
        }
        let mut msg = format!("syntax error, unexpected {}", tok_desc(t));
        // After a bare `.` bison has no default reduction, and lists nothing.
        let after_dot = self.i > 0 && matches!(self.toks.get(self.i - 1).map(|t| &t.tok), Some(Tok::Op(".")));
        if !expected.is_empty() && !after_dot {
            let mut e: Vec<&str> = expected.to_vec();
            e.sort_by_key(|x| tok_rank(x));
            msg.push_str(", expecting ");
            msg.push_str(&e.join(" or "));
        }
        Err(PErr { msg, span: (a, b), notes: vec![], tok: Some(self.i) })
    }

    /// After a query: the closing token, or bison's error listing it with `|` and `,`.
    fn close_query(&mut self, closer: &str) -> P<()> {
        if self.is_op(closer) || self.is_kw(closer) {
            self.bump();
            return Ok(());
        }
        let c = if closer.chars().all(|c| c.is_ascii_alphabetic()) { closer.to_string() } else { format!("'{closer}'") };
        self.unexpected_expecting(&[&c, "'|'", "','"])
    }
    /// reduce/foreach: the `(` after the patterns (bison names it, or `?//` before it).
    fn open_paren_after_patterns(&mut self) -> P<()> {
        if self.is_op("(") {
            self.bump();
            return Ok(());
        }
        self.unexpected_expecting(&["'('"])
    }

    fn prev_end(&self) -> usize {
        self.toks[self.i.saturating_sub(1).min(self.toks.len() - 1)].end
    }

    fn can_start_query(&self) -> bool {
        match self.peek() {
            Tok::Op(o) => matches!(*o, "." | ".." | "(" | "[" | "{" | "-" | "$"),
            Tok::Kw(k) => matches!(*k, "if" | "try" | "reduce" | "foreach" | "def" | "label" | "break"),
            Tok::Eof => false,
            _ => true,
        }
    }

    fn expect_op(&mut self, o: &str) -> P<()> {
        if self.is_op(o) {
            self.bump();
            Ok(())
        } else {
            self.unexpected()
        }
    }
    fn expect_kw(&mut self, k: &str) -> P<()> {
        if self.is_kw(k) {
            self.bump();
            Ok(())
        } else {
            self.unexpected()
        }
    }

    pub fn program(&mut self) -> P<Program> {
        let mut prog = Program { module: None, imports: vec![], defs: vec![], main: None };
        if self.is_kw("module") {
            self.bump();
            let at = self.span();
            let q = self.pipe()?;
            let end = self.prev_end();
            let span = (at.0, end);
            match const_value(&q) {
                None => return Err(PErr { notes: vec![], tok: None, msg: "Module metadata must be constant".into(), span }),
                Some(Value::Obj(_)) => prog.module = Some(const_value(&q).unwrap()),
                Some(_) => return Err(PErr { notes: vec![], tok: None, msg: "Module metadata must be an object".into(), span }),
            }
            self.expect_op(";")?;
        }
        while self.is_kw("import") || self.is_kw("include") {
            let include = self.is_kw("include");
            self.bump();
            let pspan = self.span();
            let path = match self.peek().clone() {
                Tok::Str(parts) => {
                    self.bump();
                    match parts.as_slice() {
                        [StrPart::Text(t)] => t.clone(),
                        [] => String::new(),
                        _ => return Err(PErr { notes: vec![], tok: None, msg: "Import path must be constant".into(), span: (pspan.0, pspan.1) }),
                    }
                }
                _ => return self.unexpected(),
            };
            let mut imp = Import { path, alias: None, data: false, meta: None };
            if !include {
                self.expect_kw("as")?;
                match self.bump().tok {
                    Tok::Binding(b) => {
                        imp.alias = Some(b);
                        imp.data = true;
                    }
                    Tok::Ident(n) => imp.alias = Some(n),
                    _ => {
                        self.i -= 1;
                        return self.unexpected();
                    }
                }
            }
            if !self.is_op(";") {
                let at = self.span();
                let q = self.pipe()?;
                let end = self.prev_end();
                match const_value(&q) {
                    None => return Err(PErr { notes: vec![], tok: None, msg: "Module metadata must be constant".into(), span: (at.0, end) }),
                    Some(v @ Value::Obj(_)) => imp.meta = Some(v),
                    Some(_) => return Err(PErr { notes: vec![], tok: None, msg: "Module metadata must be an object".into(), span: (at.0, end) }),
                }
            }
            self.expect_op(";")?;
            prog.imports.push(imp);
        }
        // Definitions, then (in a program) the main query.
        while self.is_kw("def") {
            let save = self.i;
            let d = self.funcdef()?;
            if matches!(self.peek(), Tok::Eof) {
                prog.defs.push(Rc::new(d));
                return Ok(prog);
            }
            // A definition followed by more: part of the main query, unless only defs follow.
            prog.defs.push(Rc::new(d));
            let _ = save;
        }
        if matches!(self.peek(), Tok::Eof) {
            return Ok(prog);
        }
        // bison: a program that cannot start reduces to an empty one, which wants the end.
        if prog.defs.is_empty() && !self.can_start_query() {
            let t = self.peek().clone();
            let (a, b) = self.span();
            let what = if matches!(t, Tok::Op(")") | Tok::Op("]") | Tok::Op("}")) { "INVALID_CHARACTER".into() } else { tok_desc(&t) };
            return Err(PErr { notes: vec![], tok: None, msg: format!("syntax error, unexpected {what}, expecting end of file"), span: (a, b) });
        }
        let q = self.pipe()?;
        if !matches!(self.peek(), Tok::Eof) {
            let t = self.peek().clone();
            let (a, b) = self.span();
            return Err(PErr { notes: vec![], tok: None, msg: format!("syntax error, unexpected {}, expecting end of file", if matches!(t, Tok::Op(")") | Tok::Op("]") | Tok::Op("}")) { "INVALID_CHARACTER".into() } else { tok_desc(&t) }), span: (a, b) });
        }
        prog.main = Some(q);
        Ok(prog)
    }

    fn funcdef(&mut self) -> P<FuncDef> {
        self.expect_kw("def")?;
        let name = match self.bump().tok {
            Tok::Ident(n) => n,
            Tok::Kw(k) => k.to_string(),
            _ => {
                self.i -= 1;
                return self.unexpected();
            }
        };
        let mut params = vec![];
        if self.is_op("(") {
            self.bump();
            loop {
                match self.bump().tok {
                    Tok::Ident(n) => params.push(Param::Filter(n)),
                    Tok::Binding(n) => params.push(Param::Value(n)),
                    _ => {
                        self.i -= 1;
                        return self.unexpected();
                    }
                }
                if self.is_op(";") {
                    self.bump();
                    continue;
                }
                self.expect_op(")")?;
                break;
            }
        }
        self.expect_op(":")?;
        let body = self.pipe()?;
        self.close_query(";")?;
        Ok(FuncDef { name, params, body })
    }

    /// Query: the pipe level, with def/label/as forms.
    pub fn pipe(&mut self) -> P<Ast> {
        if self.is_kw("def") {
            let mut defs = vec![];
            while self.is_kw("def") {
                defs.push(Rc::new(self.funcdef()?));
            }
            let rest = self.pipe()?;
            return Ok(Ast::Defs(defs, Box::new(rest)));
        }
        if self.is_kw("label") {
            self.bump();
            let name = match self.bump().tok {
                Tok::Binding(n) => n,
                _ => {
                    self.i -= 1;
                    return self.unexpected();
                }
            };
            if !self.is_op("|") {
                return self.unexpected_expecting(&["'|'"]);
            }
            self.bump();
            let body = self.pipe()?;
            return Ok(Ast::Label(name, Box::new(body)));
        }
        let left = self.comma()?;
        if let Ast::As(..) = left {
            return Ok(left);
        }
        if self.is_op("|") {
            self.bump();
            let right = self.pipe()?;
            return Ok(Ast::Pipe(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn comma(&mut self) -> P<Ast> {
        let mut left = self.expr_or_as()?;
        while self.is_op(",") {
            if matches!(left, Ast::As(..)) {
                break;
            }
            self.bump();
            // `def` and `label` start a query (`Query ',' Query`): it takes the rest.
            if self.is_kw("def") || self.is_kw("label") {
                let right = self.pipe()?;
                return Ok(Ast::Comma(Box::new(left), Box::new(right)));
            }
            let right = self.expr_or_as()?;
            let done = matches!(right, Ast::As(..));
            left = Ast::Comma(Box::new(left), Box::new(right));
            if done {
                break;
            }
        }
        Ok(left)
    }

    fn expr_or_as(&mut self) -> P<Ast> {
        let e = self.expr(0)?;
        if self.is_kw("as") {
            self.bump();
            let pats = self.patterns()?;
            if !self.is_op("|") {
                return self.unexpected_expecting(&["'|'"]);
            }
            self.bump();
            let body = self.pipe()?;
            return Ok(Ast::As(Box::new(e), Rc::new(pats), Box::new(body)));
        }
        Ok(e)
    }

    fn patterns(&mut self) -> P<Vec<Pattern>> {
        let mut v = vec![self.pattern()?];
        while self.is_op("?") && matches!(self.peek_at(1), Tok::Op("//")) {
            self.bump();
            self.bump();
            v.push(self.pattern()?);
        }
        Ok(v)
    }

    fn pattern(&mut self) -> P<Pattern> {
        match self.peek().clone() {
            Tok::Binding(n) => {
                self.bump();
                Ok(Pattern::Var(n))
            }
            Tok::Op("[") => {
                self.bump();
                if self.is_op("]") {
                    let (a, b) = self.span();
                    return Err(PErr { notes: vec![], tok: None, msg: "syntax error, unexpected ']', expecting BINDING or '[' or '{'".into(), span: (a, b) });
                }
                let mut v = vec![self.pattern()?];
                while self.is_op(",") {
                    self.bump();
                    v.push(self.pattern()?);
                }
                if !self.is_op("]") {
                    return self.unexpected_expecting(&["','", "']'"]);
                }
                self.bump();
                Ok(Pattern::Array(v))
            }
            Tok::Op("{") => {
                self.bump();
                let mut v = vec![];
                loop {
                    let at = self.span();
                    let key = match self.peek().clone() {
                        Tok::Binding(n) => {
                            self.bump();
                            if self.is_op(":") {
                                self.bump();
                                let p = self.pattern()?;
                                v.push((ObjKey::Var(n), Some(p)));
                            } else {
                                v.push((ObjKey::Var(n), None));
                            }
                            None
                        }
                        Tok::Ident(n) => {
                            self.bump();
                            Some(Ast::Lit(Value::str(&n)))
                        }
                        Tok::Kw(k) => {
                            self.bump();
                            Some(Ast::Lit(Value::str(k)))
                        }
                        Tok::Str(_) => Some(self.term_primary()?),
                        Tok::Op("(") => {
                            self.bump();
                            let q = self.pipe()?;
                            self.expect_op(")")?;
                            if let Some(v) = const_value(&q) {
                                if !matches!(v, Value::Str(_)) {
                                    return Err(PErr { notes: vec![], tok: None,
                                        msg: format!("Cannot use {} ({}) as object key", v.kind(), crate::value::dump_trunc(&v, 30)),
                                        span: (at.1, self.toks[self.i - 2].end),
                                    });
                                }
                            }
                            Some(q)
                        }
                        _ => return self.unexpected(),
                    };
                    if let Some(k) = key {
                        self.expect_op(":")?;
                        let p = self.pattern()?;
                        v.push((ObjKey::Expr(k), Some(p)));
                    }
                    if self.is_op(",") {
                        self.bump();
                        continue;
                    }
                    self.expect_op("}")?;
                    break;
                }
                Ok(Pattern::Object(v))
            }
            _ => {
                let (a, b) = self.span();
                Err(PErr { notes: vec![], tok: None, msg: format!("syntax error, unexpected {}, expecting BINDING or '[' or '{{'", tok_desc(self.peek())), span: (a, b) })
            }
        }
    }

    /// Binary operators by precedence: 0 `//` (right), 1 assignments (non-assoc), 2 or,
    /// 3 and, 4 comparisons (non-assoc), 5 + -, 6 * / %.
    fn expr(&mut self, min: u8) -> P<Ast> {
        let mut left = self.unary()?;
        loop {
            let (prec, op): (u8, &'static str) = match self.peek() {
                Tok::Op(o @ "//") => (0, o),
                Tok::Op(o @ ("=" | "|=" | "+=" | "-=" | "*=" | "/=" | "%=" | "//=")) => (1, o),
                Tok::Kw("or") => (2, "or"),
                Tok::Kw("and") => (3, "and"),
                Tok::Op(o @ ("==" | "!=" | "<" | "<=" | ">" | ">=")) => (4, o),
                Tok::Op(o @ ("+" | "-")) => (5, o),
                Tok::Op(o @ ("*" | "/" | "%")) => (6, o),
                _ => break,
            };
            if prec < min {
                break;
            }
            self.bump();
            let next_min = match prec {
                0 => 0,     // right-assoc
                1 | 4 => prec + 1, // non-assoc
                _ => prec + 1,
            };
            let right = self.expr(next_min)?;
            left = match op {
                "//" => Ast::Alt(Box::new(left), Box::new(right)),
                "or" => Ast::Or(Box::new(left), Box::new(right)),
                "and" => Ast::And(Box::new(left), Box::new(right)),
                "=" => Ast::Assign("=", Box::new(left), Box::new(right)),
                "|=" => Ast::Assign("|=", Box::new(left), Box::new(right)),
                "+=" => Ast::Assign("+", Box::new(left), Box::new(right)),
                "-=" => Ast::Assign("-", Box::new(left), Box::new(right)),
                "*=" => Ast::Assign("*", Box::new(left), Box::new(right)),
                "/=" => Ast::Assign("/", Box::new(left), Box::new(right)),
                "%=" => Ast::Assign("%", Box::new(left), Box::new(right)),
                "//=" => Ast::Assign("//", Box::new(left), Box::new(right)),
                o => {
                    let b = match o {
                        "+" => BinOp::Add,
                        "-" => BinOp::Sub,
                        "*" => BinOp::Mul,
                        "/" => BinOp::Div,
                        "%" => BinOp::Mod,
                        "==" => BinOp::Eq,
                        "!=" => BinOp::Ne,
                        "<" => BinOp::Lt,
                        "<=" => BinOp::Le,
                        ">" => BinOp::Gt,
                        _ => BinOp::Ge,
                    };
                    Ast::Bin(b, Box::new(left), Box::new(right))
                }
            };
            if prec == 1 || prec == 4 {
                // Non-associative: another operator of the same level is an error.
                let again = matches!(self.peek(), Tok::Op("=" | "|=" | "+=" | "-=" | "*=" | "/=" | "%=" | "//=")) && prec == 1
                    || matches!(self.peek(), Tok::Op("==" | "!=" | "<" | "<=" | ">" | ">=")) && prec == 4;
                if again {
                    return self.unexpected();
                }
            }
        }
        Ok(left)
    }

    fn unary(&mut self) -> P<Ast> {
        if self.is_op("-") {
            self.bump();
            let t = self.postfix()?;
            return Ok(Ast::Neg(Box::new(t)));
        }
        self.postfix()
    }

    /// A term with its postfix forms: .field, ."str", [q], [], [a:b], ?, and their dotted forms.
    fn postfix(&mut self) -> P<Ast> {
        let paren = self.is_op("(");
        let mut t = self.term_primary()?;
        // Whether `t` ends in an indexing that a `?` makes optional (not `(.a)?`, a try).
        let mut indexed = !paren && matches!(t, Ast::Index(..) | Ast::Slice(..) | Ast::Iterate(..));
        loop {
            match self.peek().clone() {
                Tok::Field(f) => {
                    self.bump();
                    t = Ast::Index(Box::new(t), Box::new(Ast::Lit(Value::str(&f))));
                }
                Tok::Op(".") if matches!(self.peek_at(1), Tok::Str(_)) => {
                    self.bump();
                    let s = self.term_primary()?;
                    t = Ast::Index(Box::new(t), Box::new(s));
                }
                Tok::Op(".") if matches!(self.peek_at(1), Tok::Op("[")) => {
                    self.bump();
                    t = self.bracket(t)?;
                }
                Tok::Op("[") => {
                    t = self.bracket(t)?;
                }
                Tok::Op("?") => {
                    self.bump();
                    t = if indexed { Ast::IndexOpt(Box::new(t)) } else { Ast::Try(Box::new(t), None) };
                    indexed = false;
                    continue;
                }
                _ => break,
            }
            indexed = true;
        }
        Ok(t)
    }

    fn bracket(&mut self, t: Ast) -> P<Ast> {
        self.expect_op("[")?;
        if self.is_op("]") {
            self.bump();
            return Ok(Ast::Iterate(Box::new(t)));
        }
        if self.is_op(":") {
            self.bump();
            let to = self.pipe()?;
            self.close_query("]")?;
            return Ok(Ast::Slice(Box::new(t), None, Some(Box::new(to))));
        }
        let q = self.pipe()?;
        if self.is_op(":") {
            self.bump();
            if self.is_op("]") {
                self.bump();
                return Ok(Ast::Slice(Box::new(t), Some(Box::new(q)), None));
            }
            let to = self.pipe()?;
            self.close_query("]")?;
            return Ok(Ast::Slice(Box::new(t), Some(Box::new(q)), Some(Box::new(to))));
        }
        if !self.is_op("]") {
            return self.unexpected_expecting(&["']'", "':'", "'|'", "','"]);
        }
        self.bump();
        Ok(Ast::Index(Box::new(t), Box::new(q)))
    }

    fn string(&mut self, fmt: Option<String>, parts: Vec<StrPart>) -> P<Ast> {
        let mut out = vec![];
        for p in parts {
            match p {
                StrPart::Text(t) => out.push(StrAst::Text(t)),
                StrPart::Interp(toks) => {
                    let end = toks.last().map(|t| t.end).unwrap_or(0);
                    let mut sub = self.sub(toks, end);
                    let q = sub.pipe()?;
                    if !matches!(sub.peek(), Tok::Eof) {
                        return sub.unexpected_expecting(&["QQSTRING_INTERP_END", "'|'", "','"]);
                    }
                    out.push(StrAst::Interp(q));
                }
            }
        }
        Ok(Ast::Str(fmt, out))
    }

    fn term_primary(&mut self) -> P<Ast> {
        let at = self.span();
        let t = self.bump();
        match t.tok {
            Tok::Op(".") => {
                if let Tok::Str(_) = self.peek() {
                    let s = self.term_primary()?;
                    return Ok(Ast::Index(Box::new(Ast::Identity), Box::new(s)));
                }
                if self.is_op("[") {
                    return self.bracket(Ast::Identity);
                }
                Ok(Ast::Identity)
            }
            Tok::Op("..") => Ok(Ast::RecurseDefault),
            Tok::Field(f) => Ok(Ast::Index(Box::new(Ast::Identity), Box::new(Ast::Lit(Value::str(&f))))),
            Tok::Literal(v) => Ok(Ast::Lit(v)),
            Tok::Str(parts) => self.string(None, parts),
            Tok::Format(f) => {
                if let Tok::Str(parts) = self.peek().clone() {
                    self.bump();
                    return self.string(Some(f), parts);
                }
                Ok(Ast::Format(f))
            }
            Tok::Op("(") => {
                let q = self.pipe()?;
                self.close_query(")")?;
                Ok(q)
            }
            Tok::Op("[") => {
                if self.is_op("]") {
                    self.bump();
                    return Ok(Ast::Array(None));
                }
                let q = self.pipe()?;
                self.close_query("]")?;
                Ok(Ast::Array(Some(Box::new(q))))
            }
            Tok::Op("{") => self.object(),
            Tok::Op("$") => {
                // `$$$$name`: builtin.jq's private LOADVN.
                if self.is_op("$") && matches!(self.peek_at(1), Tok::Op("$")) {
                    self.bump();
                    self.bump();
                    if let Tok::Binding(n) = self.bump().tok {
                        return Ok(Ast::Var(n, (at.0, self.prev_end())));
                    }
                }
                self.i -= 1;
                self.unexpected()
            }
            Tok::Kw("reduce") => {
                let src = self.postfix_or_expr()?;
                self.expect_kw("as")?;
                let pats = self.patterns()?;
                self.open_paren_after_patterns()?;
                let init = self.pipe()?;
                self.close_query(";")?;
                let upd = self.pipe()?;
                self.close_query(")")?;
                Ok(Ast::Reduce(Box::new(src), Rc::new(pats), Box::new(init), Box::new(upd)))
            }
            Tok::Kw("foreach") => {
                let src = self.postfix_or_expr()?;
                self.expect_kw("as")?;
                let pats = self.patterns()?;
                self.open_paren_after_patterns()?;
                let init = self.pipe()?;
                self.close_query(";")?;
                let upd = self.pipe()?;
                let ext = if self.is_op(";") {
                    self.bump();
                    let x = self.pipe()?;
                    self.close_query(")")?;
                    Some(Box::new(x))
                } else {
                    if !self.is_op(")") {
                        return self.unexpected_expecting(&["';'", "')'", "'|'", "','"]);
                    }
                    self.bump();
                    None
                };
                Ok(Ast::Foreach(Box::new(src), Rc::new(pats), Box::new(init), Box::new(upd), ext))
            }
            Tok::Kw("if") => {
                let c = self.pipe()?;
                self.close_query("then")?;
                // bison's `"if" Query "then" error` rule: a syntax error after `then` adds this.
                let r = self.pipe().and_then(|th| self.if_rest(c, th));
                r.map_err(|mut e| {
                    if e.is_syntax() {
                        e.notes.push(("Possibly unterminated 'if' statement".into(), (at.0, e.span.1)));
                    }
                    e
                })
            }
            Tok::Kw("try") => {
                let body = match self.postfix_unary() {
                    Ok(b) => b,
                    Err(mut e) => {
                        // Recovery as bison's: a `catch` the body stopped at still starts the
                        // catch part, whose own error ends the try ("Possibly unterminated").
                        if let Some(k) = e.tok.filter(|_| e.is_syntax()) {
                            if matches!(self.toks.get(k).map(|t| &t.tok), Some(Tok::Kw("catch"))) {
                                self.i = k + 1;
                                if let Err(e2) = self.postfix_unary() {
                                    if e2.is_syntax() {
                                        e.notes.push(("Possibly unterminated 'try' statement".into(), (at.0, e2.span.1)));
                                    }
                                }
                            }
                        }
                        return Err(e);
                    }
                };
                let catch = if self.is_kw("catch") {
                    self.bump();
                    let c = self.postfix_unary().map_err(|mut e| {
                        if e.is_syntax() {
                            e.notes.push(("Possibly unterminated 'try' statement".into(), (at.0, e.span.1)));
                        }
                        e
                    })?;
                    Some(Box::new(c))
                } else {
                    None
                };
                Ok(Ast::Try(Box::new(body), catch))
            }
            Tok::Kw("break") => match self.bump().tok {
                Tok::Binding(n) => Ok(Ast::Break(n, (at.0, self.prev_end()))),
                _ => Err(PErr { notes: vec![], tok: None, msg: "break requires a label to break to".into(), span: (at.0, at.1) }),
            },
            Tok::Loc => Ok(self.loc(at.0)),
            Tok::Binding(n) => Ok(Ast::Var(n, (at.0, at.1))),
            Tok::Ident(n) => {
                match n.as_str() {
                    "true" => return Ok(Ast::Lit(Value::Bool(true))),
                    "false" => return Ok(Ast::Lit(Value::Bool(false))),
                    "null" => return Ok(Ast::Lit(Value::Null)),
                    _ => {}
                }
                let mut args = vec![];
                if self.is_op("(") {
                    self.bump();
                    loop {
                        args.push(self.pipe()?);
                        if self.is_op(";") {
                            self.bump();
                            continue;
                        }
                        if !self.is_op(")") {
                            return self.unexpected_expecting(&["';'", "')'"]);
                        }
                        self.bump();
                        break;
                    }
                }
                Ok(Ast::Call(n, args, (at.0, at.1)))
            }
            _ => {
                self.i -= 1;
                self.unexpected()
            }
        }
    }

    /// try/catch bodies: a postfix term (with unary minus).
    fn postfix_unary(&mut self) -> P<Ast> {
        self.unary()
    }

    /// reduce/foreach sources: an expression up to `as`.
    fn postfix_or_expr(&mut self) -> P<Ast> {
        self.expr(0)
    }

    fn if_rest(&mut self, c: Ast, th: Ast) -> P<Ast> {
        if self.is_kw("elif") {
            self.bump();
            let c2 = self.pipe()?;
            self.close_query("then")?;
            let th2 = self.pipe()?;
            let rest = self.if_rest(c2, th2)?;
            return Ok(Ast::If(Box::new(c), Box::new(th), Some(Box::new(rest))));
        }
        if self.is_kw("else") {
            self.bump();
            let el = self.pipe()?;
            self.close_query("end")?;
            return Ok(Ast::If(Box::new(c), Box::new(th), Some(Box::new(el))));
        }
        self.expect_kw("end")?;
        Ok(Ast::If(Box::new(c), Box::new(th), None))
    }

    /// Object construction: `{a, $x, "s", "s": v, (q): v, @fmt "..": v, $__loc__, kw: v}`.
    fn object(&mut self) -> P<Ast> {
        let mut pairs = vec![];
        if self.is_op("}") {
            self.bump();
            return Ok(Ast::Object(pairs));
        }
        loop {
            let first = self.i;
            if let Err(mut e) = self.dict_pair(&mut pairs) {
                self.recover_pair(&mut e, first);
                return Err(e);
            }
            if self.is_op(",") {
                self.bump();
                continue;
            }
            if !self.is_op("}") {
                let mut e = self.unexpected_expecting::<()>(&["'}'"]).unwrap_err();
                self.recover_pair(&mut e, first);
                return Err(e);
            }
            self.bump();
            break;
        }
        Ok(Ast::Object(pairs))
    }

    /// bison's recovery in an object: `error ':' DictExpr` when a ':' comes (before a '}')
    /// after the error, which adds "May need parentheses"; an error inside brackets opened in
    /// the pair is theirs to recover from.
    fn recover_pair(&mut self, e: &mut PErr, first: usize) {
        let Some(k) = e.tok.filter(|_| e.is_syntax() && e.notes.is_empty()) else { return };
        let mut depth = 0i32;
        for t in &self.toks[first..k.min(self.toks.len())] {
            match t.tok {
                Tok::Op("(" | "[" | "{") => depth += 1,
                Tok::Op(")" | "]" | "}") => depth -= 1,
                _ => {}
            }
        }
        if depth > 0 {
            return;
        }
        let Some(j) = (k..self.toks.len()).find(|&j| matches!(self.toks[j].tok, Tok::Op(":" | "}"))) else { return };
        if !matches!(self.toks[j].tok, Tok::Op(":")) {
            return;
        }
        // The error token spans what was popped and discarded: the pair up to the ':'.
        let span = (self.toks[first].start, self.toks[j - 1].end);
        self.i = j + 1;
        if self.dict_expr().is_ok() {
            e.notes.push(("May need parentheses around object key expression".into(), span));
        }
    }

    fn dict_pair(&mut self, pairs: &mut Vec<(Ast, Ast)>) -> P<()> {
        let at = self.span();
        match self.peek().clone() {
            Tok::Ident(n) => {
                self.bump();
                if self.is_op(":") {
                    self.bump();
                    let v = self.dict_expr()?;
                    pairs.push((Ast::Lit(Value::str(&n)), v));
                } else {
                    pairs.push((Ast::Lit(Value::str(&n)), Ast::Index(Box::new(Ast::Identity), Box::new(Ast::Lit(Value::str(&n))))));
                }
            }
            Tok::Kw(k) => {
                self.bump();
                if self.is_op(":") {
                    self.bump();
                    let v = self.dict_expr()?;
                    pairs.push((Ast::Lit(Value::str(k)), v));
                } else {
                    pairs.push((Ast::Lit(Value::str(k)), Ast::Index(Box::new(Ast::Identity), Box::new(Ast::Lit(Value::str(k))))));
                }
            }
            Tok::Binding(n) => {
                self.bump();
                if self.is_op(":") {
                    self.bump();
                    let v = self.dict_expr()?;
                    pairs.push((Ast::Var(n, at), v));
                } else {
                    pairs.push((Ast::Lit(Value::str(&n)), Ast::Var(n, at)));
                }
            }
            Tok::Loc => {
                self.bump();
                pairs.push((Ast::Lit(Value::str("__loc__")), self.loc(at.0)));
            }
            Tok::Str(_) | Tok::Format(_) => {
                let s = self.term_primary()?;
                if self.is_op(":") {
                    self.bump();
                    let v = self.dict_expr()?;
                    pairs.push((s, v));
                } else {
                    pairs.push((s.clone(), Ast::Index(Box::new(Ast::Identity), Box::new(s))));
                }
            }
            Tok::Op("(") => {
                self.bump();
                let kat = self.span();
                let q = self.pipe()?;
                let kend = self.prev_end();
                self.expect_op(")")?;
                if let Some(v) = const_value(&q) {
                    if !matches!(v, Value::Str(_)) {
                        return Err(PErr { notes: vec![], tok: None, msg: format!("Cannot use {} ({}) as object key", v.kind(), crate::value::dump_trunc(&v, 30)), span: (kat.0, kend) });
                    }
                }
                if !self.is_op(":") {
                    return self.unexpected_expecting(&["':'"]);
                }
                self.bump();
                let v = self.dict_expr()?;
                pairs.push((q, v));
            }
            _ => return self.unexpected(),
        }
        Ok(())
    }

    /// An object value: expressions joined by `|` (no `,`).
    fn dict_expr(&mut self) -> P<Ast> {
        let mut left = self.expr(0)?;
        while self.is_op("|") {
            self.bump();
            let right = self.expr(0)?;
            left = Ast::Pipe(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
}

/// A query that is a constant (for module metadata and object keys).
pub fn const_value(a: &Ast) -> Option<Value> {
    match a {
        Ast::Lit(v) => Some(v.clone()),
        Ast::Str(None, parts) if parts.iter().all(|p| matches!(p, StrAst::Text(_))) => {
            let s: String = parts.iter().map(|p| if let StrAst::Text(t) = p { t.as_str() } else { "" }).collect();
            Some(Value::string(s))
        }
        Ast::Array(None) => Some(Value::arr(vec![])),
        Ast::Array(Some(q)) => {
            let mut items = vec![];
            let mut cur: &Ast = q;
            loop {
                match cur {
                    Ast::Comma(a, b) => {
                        items.push(const_value(b)?);
                        cur = a;
                    }
                    other => {
                        items.push(const_value(other)?);
                        break;
                    }
                }
            }
            items.reverse();
            Some(Value::arr(items))
        }
        Ast::Object(pairs) => {
            let mut m = crate::value::Map::new();
            for (k, v) in pairs {
                let Value::Str(k) = const_value(k)? else { return None };
                m.insert(k, const_value(v)?);
            }
            Some(Value::obj(m))
        }
        Ast::Neg(x) => match const_value(x)? {
            Value::Num(n) => Some(Value::num(-n.f)),
            _ => None,
        },
        _ => None,
    }
}
