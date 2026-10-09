//! Compiling: builtin.jq, the program, and the modules it imports (linker.c's search rules
//! and binding order), then jq's compile-time checks (undefined functions, variables and
//! labels) with its located error messages.

use crate::builtins::{BUILTIN_JQ, NATIVE_NAMES};
use crate::interp::{err, Bind, Env, Flow, Interp};
use crate::parser::{Ast, FuncDef, ObjKey, Param, Parser, Pattern, Program, StrAst};
use crate::value::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// locfile_locate: the message, where, the line, and carets under the span.
pub fn locate(src: &str, fname: &str, span: (usize, usize), msg: &str) -> String {
    let b = src.as_bytes();
    let start = span.0.min(b.len());
    let line_start = b[..start].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
    let line_end = b[start..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| start + p);
    let lineno = b[..line_start].iter().filter(|&&c| c == b'\n').count() + 1;
    // As jq: the carets stop at the line's end, but cover at least one character.
    let end = span.1.min(line_end.max(start + 1)).max(start + 1);
    let carets = "^".repeat(end - start);
    format!(
        "{msg} at {fname}, line {lineno}, column {}:\n    {}\n    {:>width$}",
        start - line_start + 1,
        String::from_utf8_lossy(&b[line_start..line_end]),
        carets,
        width = end - line_start
    )
}

/// A parse error's messages: the error, then what bison's recovery added.
fn perr_messages(src: &str, fname: &str, e: &crate::parser::PErr) -> Vec<String> {
    let mut v = vec![locate(src, fname, e.span, &format!("jq: error: {}", e.msg))];
    for (m, span) in &e.notes {
        v.push(locate(src, fname, *span, &format!("jq: error: {m}")));
    }
    v
}

pub struct Options {
    pub lib_dirs: Vec<String>,
    /// The directory of the jq binary ($ORIGIN).
    pub jq_origin: String,
    /// The directory of the program file (or "."): relative search paths start there.
    pub prog_origin: String,
    /// `$name` globals: --arg and friends, and $ARGS.
    pub globals: Vec<(String, Value)>,
}

struct Lib {
    /// Every definition, in order, with the environment it closes over.
    defs: Vec<(&'static FuncDef, Env)>,
}

enum Loaded {
    Code(Rc<Lib>),
    Data(Value),
}

pub struct Linker<'a> {
    opts: &'a Options,
    loaded: HashMap<PathBuf, Rc<Lib>>,
    loading: Vec<PathBuf>,
    pub errors: Vec<String>,
}

fn leak_program(p: Program) -> &'static Program {
    Box::leak(Box::new(p))
}

fn home() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}

fn expand_path(p: &str) -> Result<String, String> {
    if p == "~" || p.starts_with("~/") {
        match home() {
            Some(h) => Ok(format!("{h}{}", &p[1..])),
            None => Err("Could not expand ~ (HOME not set)".into()),
        }
    } else {
        Ok(p.to_string())
    }
}

fn validate_relpath(name: &str) -> Result<(), String> {
    if name.contains('\0') {
        return Err("Module path contains a NUL byte".into());
    }
    if name.contains('\\') {
        return Err(format!("Modules must be named by relative paths using '/', not '\\' ({name})"));
    }
    let comps: Vec<&str> = name.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        if *c == ".." {
            return Err(format!("Relative paths to modules may not traverse to parent directories ({name})"));
        }
        if i > 0 && comps[i - 1] == *c {
            return Err(format!("module names must not have equal consecutive components: {name}"));
        }
    }
    Ok(())
}

fn realpath(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// find_lib: the first of search/rel.jq, search/rel/jq/main.jq, search/rel/base.jq.
fn find_lib(rel: &str, search: &[Value], suffix: &str, jq_origin: &str, lib_origin: Option<&str>) -> Result<PathBuf, String> {
    validate_relpath(rel)?;
    let mut chain = vec![];
    let mut experr = None;
    for p in search {
        let Value::Str(p) = p else { continue };
        let p = match expand_path(p) {
            Ok(p) => p,
            Err(e) => {
                experr = Some(e);
                continue;
            }
        };
        if p == "." {
            chain.push(p);
        } else if let Some(rest) = p.strip_prefix("$ORIGIN/") {
            chain.push(format!("{jq_origin}/{rest}"));
        } else if let (Some(o), false) = (lib_origin, p.starts_with('/')) {
            chain.push(format!("{o}/{p}"));
        } else {
            chain.push(p);
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    for s in chain {
        if s.is_empty() {
            continue;
        }
        for cand in [format!("{s}/{rel}{suffix}"), format!("{s}/{rel}/jq/main{suffix}"), format!("{s}/{rel}/{base}{suffix}")] {
            if std::fs::metadata(&cand).is_ok() {
                return Ok(realpath(&cand));
            }
        }
    }
    Err(match experr {
        Some(e) => format!("module not found: {rel} ({e})"),
        None => format!("module not found: {rel}"),
    })
}

/// The JSON texts of a data file, as an array.
fn load_data(path: &Path, raw: bool) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|e| crate::io_error_text(&e))?;
    if raw {
        return Ok(Value::string(crate::value::utf8_lossy(&bytes)));
    }
    let mut p = crate::json::Parser::new(false);
    p.feed(&bytes, true);
    let mut out = vec![];
    loop {
        match p.next() {
            crate::json::Next::Value(v) => out.push(v),
            crate::json::Next::Error(e) => return Err(e),
            _ => break,
        }
    }
    Ok(Value::arr(out))
}

fn dep_meta(imp: &crate::parser::Import) -> Value {
    let mut m = match &imp.meta {
        Some(Value::Obj(m)) => (**m).clone(),
        _ => Map::new(),
    };
    if let Some(a) = &imp.alias {
        m.insert(Rc::from("as"), Value::str(a));
    }
    m.insert(Rc::from("is_data"), Value::Bool(imp.data));
    m.insert(Rc::from("relpath"), Value::str(&imp.path));
    Value::obj(m)
}

fn meta_get<'v>(imp: &'v crate::parser::Import, k: &str) -> Option<&'v Value> {
    match &imp.meta {
        Some(Value::Obj(m)) => m.get(k),
        _ => None,
    }
}

impl<'a> Linker<'a> {
    pub fn new(opts: &'a Options) -> Self {
        Linker { opts, loaded: HashMap::new(), loading: vec![], errors: vec![] }
    }

    fn search_for(&self, imp: &crate::parser::Import) -> Vec<Value> {
        match meta_get(imp, "search") {
            None => {
                let mut v = vec![Value::str(".")];
                v.extend(self.opts.lib_dirs.iter().map(|s| Value::str(s)));
                v
            }
            Some(Value::Arr(a)) => (**a).clone(),
            Some(other) => vec![other.clone()],
        }
    }

    /// Binds a program's (or library's) imports into `env`: later imports shadow earlier ones.
    fn bind_imports(&mut self, imports: &[crate::parser::Import], lib_origin: &str, mut env: Env) -> Option<Env> {
        let mut ok = true;
        for imp in imports {
            let optional = matches!(meta_get(imp, "optional"), Some(Value::Bool(true)));
            let raw = matches!(meta_get(imp, "raw"), Some(Value::Bool(true)));
            let search = self.search_for(imp);
            let suffix = if imp.data { ".json" } else { ".jq" };
            let path = match find_lib(&imp.path, &search, suffix, &self.opts.jq_origin, Some(lib_origin)) {
                Ok(p) => p,
                Err(e) => {
                    if optional {
                        continue;
                    }
                    self.errors.push(format!("jq: error: {e}\n"));
                    return None;
                }
            };
            match self.load(&path, imp.data, raw, optional) {
                Some(Loaded::Data(v)) => {
                    let a = imp.alias.clone().unwrap_or_default();
                    env = env.push(Bind::Var(Rc::from(format!("{a}::{a}").as_str()), v.clone(), None));
                    env = env.push(Bind::Var(Rc::from(a.as_str()), v, None));
                }
                Some(Loaded::Code(lib)) => {
                    let prefix = match &imp.alias {
                        Some(a) if !a.is_empty() => format!("{a}::"),
                        _ => String::new(),
                    };
                    for (fd, denv) in &lib.defs {
                        env = env.push(Bind::Lib(Rc::from(format!("{prefix}{}", fd.name).as_str()), fd, denv.clone()));
                    }
                }
                None => {
                    if !optional {
                        ok = false;
                    }
                }
            }
        }
        ok.then_some(env)
    }

    fn load(&mut self, path: &Path, data: bool, raw: bool, optional: bool) -> Option<Loaded> {
        if self.loading.iter().any(|p| p == path) {
            self.errors.push(format!("jq: error: circular import of {}\n", path.display()));
            return None;
        }
        if data {
            return match load_data(path, raw) {
                Ok(v) => Some(Loaded::Data(v)),
                Err(e) => {
                    if !optional {
                        self.errors.push(format!("jq: error loading data file {}: {e}\n", path.display()));
                    }
                    None
                }
            };
        }
        if let Some(l) = self.loaded.get(path) {
            return Some(Loaded::Code(l.clone()));
        }
        let src = match std::fs::read(path) {
            Ok(b) => crate::value::utf8_lossy(&b),
            Err(e) => {
                if !optional {
                    self.errors.push(format!("jq: error loading data file {}: {}\n", path.display(), crate::io_error_text(&e)));
                }
                return None;
            }
        };
        let fname = path.display().to_string();
        let prog = match Parser::new(&src, &fname).and_then(|mut p| p.program()) {
            Ok(p) => p,
            Err(e) => {
                self.errors.extend(perr_messages(&src, &fname, &e));
                return None;
            }
        };
        if prog.main.is_some() {
            self.errors.push(format!("jq: error: library should only have function definitions, not a main expression"));
            return None;
        }
        let prog = leak_program(prog);
        self.loading.push(path.to_path_buf());
        let origin = path.parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into());
        let env = self.bind_imports(&prog.imports, &origin, Env::default());
        self.loading.pop();
        let env = env?;
        let mut defs = vec![];
        let mut e = env;
        let mut scope = Scope::from_env(&e);
        for d in &prog.defs {
            let fd: &'static FuncDef = d;
            e = e.push(Bind::Func(fd));
            scope.check_def(fd, &src, &fname, &mut self.errors);
            defs.push((fd, e.clone()));
        }
        let lib = Rc::new(Lib { defs });
        self.loaded.insert(path.to_path_buf(), lib.clone());
        Some(Loaded::Code(lib))
    }
}

/// Parses builtin.jq into the interpreter's library.
pub fn load_builtins(it: &mut Interp) {
    let prog = Parser::new(BUILTIN_JQ, "<builtin>").and_then(|mut p| p.program()).expect("builtin.jq parses");
    let prog = leak_program(prog);
    let mut e = Env::default();
    for d in &prog.defs {
        let fd: &'static FuncDef = d;
        // These run natively (as loops: their jq definitions recurse once per iteration).
        if crate::builtins::NATIVE_OVERRIDES.contains(&(fd.name.as_str(), fd.params.len())) {
            continue;
        }
        e = e.push(Bind::Func(fd));
        it.defs.insert((fd.name.clone(), fd.params.len()), (fd, e.clone()));
    }
    it.builtin_order = prog.defs.iter().map(|d| (d.name.clone(), d.params.len())).collect();
}

pub struct Compiled {
    pub main: &'static Ast,
    pub env: Env,
}

/// Compiles a program: Err holds the error messages (the caller adds "jq: N compile errors").
pub fn compile(it: &mut Interp, src: &str, fname: &str, opts: &Options) -> Result<Compiled, Vec<String>> {
    let prog = match Parser::new(src, fname).and_then(|mut p| p.program()) {
        Ok(p) => p,
        Err(e) => return Err(perr_messages(src, fname, &e)),
    };
    if prog.main.is_none() {
        return Err(vec!["jq: error: Top-level program not given (try \".\")".into()]);
    }
    let prog = leak_program(prog);
    let mut linker = Linker::new(opts);
    let mut env = Env::default();
    for (k, v) in &opts.globals {
        env = env.push(Bind::Var(Rc::from(k.as_str()), v.clone(), None));
    }
    // ~/.jq, as an optional library included before the program's imports.
    let mut imports = vec![];
    if let Some(h) = home() {
        let mut m = Map::new();
        m.insert(Rc::from("optional"), Value::Bool(true));
        m.insert(Rc::from("search"), Value::string(h));
        imports.push(crate::parser::Import { path: String::new(), alias: Some(String::new()), data: false, meta: Some(Value::obj(m)) });
    }
    imports.extend(prog.imports.iter().cloned());
    let env = match linker.bind_imports(&imports, &opts.prog_origin, env) {
        Some(e) => e,
        None => return Err(linker.errors),
    };
    let mut errors = linker.errors;
    let mut scope = Scope::from_env(&env);
    let mut e = env;
    for d in &prog.defs {
        let fd: &'static FuncDef = d;
        e = e.push(Bind::Func(fd));
        scope.check_def(fd, src, fname, &mut errors);
    }
    let main: &'static Ast = prog.main.as_ref().unwrap();
    scope.check(main, src, fname, &mut errors);
    check_limits(Some(main), &prog.defs, &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    let _ = it;
    Ok(Compiled { main, env: e })
}

/// modulemeta: a module's metadata, its imports ("deps") and definitions ("defs").
pub fn modulemeta(it: &Interp, name: &str) -> Result<Value, Flow> {
    let search: Vec<Value> = it.search_list.iter().map(|s| Value::str(s)).collect();
    let path = find_lib(name, &search, ".jq", &it.jq_origin, None).map_err(err)?;
    let Ok(bytes) = std::fs::read(&path) else { return Ok(Value::Null) };
    let src = crate::value::utf8_lossy(&bytes);
    let fname = path.display().to_string();
    let prog = match Parser::new(&src, &fname).and_then(|mut p| p.program()) {
        Ok(p) => p,
        Err(e) => {
            for m in perr_messages(&src, &fname, &e) {
                eprintln!("{m}");
            }
            return Ok(Value::Null);
        }
    };
    let mut m = match prog.module {
        Some(Value::Obj(m)) => (*m).clone(),
        _ => Map::new(),
    };
    m.insert(Rc::from("deps"), Value::arr(prog.imports.iter().map(dep_meta).collect()));
    let mut defs: Vec<String> = vec![];
    for d in &prog.defs {
        let n = format!("{}/{}", d.name, d.params.len());
        if !defs.contains(&n) {
            defs.push(n);
        }
    }
    m.insert(Rc::from("defs"), Value::arr(defs.into_iter().map(Value::string).collect()));
    Ok(Value::obj(m))
}

// Compile-time checks

/// jq's bytecode limit (ARG_NEWCLOSURE - 1): a function's subfunctions (its local definitions
/// and the closures its calls pass) and its parameters.
const MAX_CLOSURES: usize = 4095;

fn walk_children(a: &Ast, f: &mut dyn FnMut(&Ast)) {
    match a {
        Ast::Identity | Ast::RecurseDefault | Ast::Lit(_) | Ast::Format(_) | Ast::Var(..) | Ast::Break(..) => {}
        Ast::Str(_, parts) => {
            for p in parts {
                if let StrAst::Interp(q) = p {
                    f(q);
                }
            }
        }
        Ast::Index(x, y) | Ast::Pipe(x, y) | Ast::Comma(x, y) | Ast::Bin(_, x, y) | Ast::And(x, y) | Ast::Or(x, y) | Ast::Alt(x, y) | Ast::Assign(_, x, y) => {
            f(x);
            f(y);
        }
        Ast::Slice(t, a, b) => {
            f(t);
            a.as_deref().map(&mut *f);
            b.as_deref().map(&mut *f);
        }
        Ast::Iterate(t) | Ast::Neg(t) | Ast::Label(_, t) | Ast::IndexOpt(t) => f(t),
        Ast::Try(b, c) => {
            f(b);
            c.as_deref().map(&mut *f);
        }
        Ast::Array(q) => {
            q.as_deref().map(&mut *f);
        }
        Ast::Object(pairs) => {
            for (k, v) in pairs {
                f(k);
                f(v);
            }
        }
        Ast::If(c, t, e) => {
            f(c);
            f(t);
            e.as_deref().map(&mut *f);
        }
        Ast::Reduce(s, _, i, u) => {
            f(s);
            f(i);
            f(u);
        }
        Ast::Foreach(s, _, i, u, x) => {
            f(s);
            f(i);
            f(u);
            x.as_deref().map(&mut *f);
        }
        Ast::Defs(_, rest) => f(rest),
        Ast::Call(_, args, _) => args.iter().for_each(&mut *f),
        Ast::As(s, _, b) => {
            f(s);
            f(b);
        }
    }
}

/// The subfunctions one function body makes (not counting those of the functions it defines).
fn count_subfunctions(a: &Ast) -> usize {
    let mut n = match a {
        Ast::Defs(defs, _) => defs.len(),
        Ast::Call(_, args, _) => args.len(),
        _ => 0,
    };
    walk_children(a, &mut |c| n += count_subfunctions(c));
    n
}

/// The function bodies under `a` (its local definitions, at any depth).
fn each_def(a: &Ast, f: &mut dyn FnMut(&FuncDef)) {
    if let Ast::Defs(defs, _) = a {
        for d in defs {
            f(d);
            each_def(&d.body, f);
        }
    }
    walk_children(a, &mut |c| each_def(c, f));
}

fn check_limits(main: Option<&Ast>, defs: &[Rc<FuncDef>], errors: &mut Vec<String>) {
    let mut bad = false;
    // jq stops compiling at the first of these.
    let mut check = |n: usize, params: usize| {
        if bad {
        } else if params > MAX_CLOSURES {
            errors.push(format!("jq: error: function has too many parameters (max {MAX_CLOSURES})"));
            bad = true;
        } else if n > MAX_CLOSURES {
            errors.push(format!("jq: error: too many function parameters or local function definitions (max {MAX_CLOSURES})"));
            bad = true;
        }
    };
    let mut bodies: Vec<(usize, usize)> = vec![];
    if let Some(m) = main {
        bodies.push((count_subfunctions(m) + defs.len(), 0));
        each_def(m, &mut |d| bodies.push((count_subfunctions(&d.body), d.params.len())));
    }
    for d in defs {
        bodies.push((count_subfunctions(&d.body), d.params.len()));
        each_def(&d.body, &mut |d| bodies.push((count_subfunctions(&d.body), d.params.len())));
    }
    for (n, p) in bodies {
        check(n, p);
    }
}

#[derive(Clone)]
enum Name {
    Var(Rc<str>),
    Func(Rc<str>, usize),
    Label(Rc<str>),
}

/// What is in scope: a persistent list (the builtins and natives are always there).
#[derive(Clone)]
struct Scope(Option<Rc<(Name, Scope)>>);

impl Scope {
    fn from_env(env: &Env) -> Scope {
        let mut names = vec![];
        let mut cur = env.0.clone();
        while let Some(n) = cur {
            match &n.b {
                Bind::Var(v, _, _) => names.push(Name::Var(v.clone())),
                Bind::Func(fd) => names.push(Name::Func(Rc::from(fd.name.as_str()), fd.params.len())),
                Bind::Lib(name, fd, _) => names.push(Name::Func(name.clone(), fd.params.len())),
                _ => {}
            }
            cur = n.next.0.clone();
        }
        let mut s = Scope(None);
        for n in names.into_iter().rev() {
            s = s.push(n);
        }
        s
    }

    fn push(&self, n: Name) -> Scope {
        Scope(Some(Rc::new((n, self.clone()))))
    }

    fn has(&self, f: impl Fn(&Name) -> bool) -> bool {
        let mut cur = &self.0;
        while let Some(n) = cur {
            if f(&n.0) {
                return true;
            }
            cur = &n.1 .0;
        }
        false
    }

    fn has_func(&self, name: &str, arity: usize) -> bool {
        self.has(|n| matches!(n, Name::Func(f, a) if &**f == name && *a == arity)) || builtin_exists(name, arity)
    }

    fn check_def(&mut self, fd: &'static FuncDef, src: &str, fname: &str, errors: &mut Vec<String>) {
        *self = self.push(Name::Func(Rc::from(fd.name.as_str()), fd.params.len()));
        let mut inner = self.clone();
        for p in &fd.params {
            match p {
                Param::Filter(n) => inner = inner.push(Name::Func(Rc::from(n.as_str()), 0)),
                Param::Value(n) => {
                    inner = inner.push(Name::Func(Rc::from(n.as_str()), 0));
                    inner = inner.push(Name::Var(Rc::from(n.as_str())));
                }
            }
        }
        inner.check(&fd.body, src, fname, errors);
    }

    fn with_patterns(&self, pats: &'static [Pattern], src: &str, fname: &str, errors: &mut Vec<String>) -> Scope {
        let mut s = self.clone();
        for p in pats {
            s = s.pattern(p, src, fname, errors);
        }
        s
    }

    fn pattern(&self, p: &'static Pattern, src: &str, fname: &str, errors: &mut Vec<String>) -> Scope {
        match p {
            Pattern::Var(n) => self.push(Name::Var(Rc::from(n.as_str()))),
            Pattern::Array(ps) => {
                let mut s = self.clone();
                for p in ps {
                    s = s.pattern(p, src, fname, errors);
                }
                s
            }
            Pattern::Object(entries) => {
                let mut s = self.clone();
                for (k, sub) in entries {
                    match k {
                        ObjKey::Var(n) => s = s.push(Name::Var(Rc::from(n.as_str()))),
                        ObjKey::Expr(e) => s.check(e, src, fname, errors),
                    }
                    if let Some(sub) = sub {
                        s = s.pattern(sub, src, fname, errors);
                    }
                }
                s
            }
        }
    }

    fn check(&self, a: &'static Ast, src: &str, fname: &str, errors: &mut Vec<String>) {
        let mut go = |x: &'static Ast| self.check(x, src, fname, errors);
        match a {
            Ast::Identity | Ast::RecurseDefault | Ast::Lit(_) | Ast::Format(_) => {}
            Ast::Str(_, parts) => {
                for p in parts {
                    if let StrAst::Interp(q) = p {
                        go(q);
                    }
                }
            }
            Ast::Index(t, k) => {
                go(t);
                go(k);
            }
            Ast::Slice(t, f, e) => {
                go(t);
                if let Some(f) = f {
                    go(f);
                }
                if let Some(e) = e {
                    go(e);
                }
            }
            Ast::Iterate(t) | Ast::Neg(t) | Ast::IndexOpt(t) => go(t),
            Ast::Try(b, c) => {
                go(b);
                if let Some(c) = c {
                    go(c);
                }
            }
            Ast::Array(q) => {
                if let Some(q) = q {
                    go(q);
                }
            }
            Ast::Object(pairs) => {
                for (k, v) in pairs {
                    go(k);
                    go(v);
                }
            }
            Ast::Pipe(x, y) | Ast::Comma(x, y) | Ast::Bin(_, x, y) | Ast::And(x, y) | Ast::Or(x, y) | Ast::Alt(x, y) | Ast::Assign(_, x, y) => {
                go(x);
                go(y);
            }
            Ast::If(c, t, e) => {
                go(c);
                go(t);
                if let Some(e) = e {
                    go(e);
                }
            }
            Ast::Reduce(s, pats, init, upd) => {
                go(s);
                go(init);
                let inner = self.with_patterns(pats, src, fname, errors);
                inner.check(upd, src, fname, errors);
            }
            Ast::Foreach(s, pats, init, upd, ext) => {
                go(s);
                go(init);
                let inner = self.with_patterns(pats, src, fname, errors);
                inner.check(upd, src, fname, errors);
                if let Some(x) = ext {
                    inner.check(x, src, fname, errors);
                }
            }
            Ast::Defs(defs, rest) => {
                let mut s = self.clone();
                for d in defs {
                    s.check_def(d, src, fname, errors);
                }
                s.check(rest, src, fname, errors);
            }
            Ast::Call(name, args, span) => {
                for x in args {
                    go(x);
                }
                if !self.has_func(name, args.len()) {
                    errors.push(locate(src, fname, *span, &format!("jq: error: {}/{} is not defined", name, args.len())));
                }
            }
            Ast::Var(name, span) => {
                if name != "ENV" && !self.has(|n| matches!(n, Name::Var(v) if **v == **name)) {
                    errors.push(locate(src, fname, *span, &format!("jq: error: ${name} is not defined")));
                }
            }
            Ast::As(s, pats, body) => {
                go(s);
                let inner = self.with_patterns(pats, src, fname, errors);
                inner.check(body, src, fname, errors);
            }
            Ast::Label(name, body) => {
                let inner = self.push(Name::Label(Rc::from(name.as_str())));
                inner.check(body, src, fname, errors);
            }
            Ast::Break(name, span) => {
                if !self.has(|n| matches!(n, Name::Label(l) if **l == **name)) {
                    errors.push(locate(src, fname, *span, &format!("jq: error: $*label-{name} is not defined")));
                }
            }
        }
    }
}

thread_local! {
    static BUILTIN_NAMES: std::cell::RefCell<HashSet<(String, usize)>> = std::cell::RefCell::new(HashSet::new());
}

pub fn register_builtin_names(it: &Interp) {
    BUILTIN_NAMES.with(|b| {
        let mut b = b.borrow_mut();
        for k in it.defs.keys() {
            b.insert(k.clone());
        }
        for (n, a) in NATIVE_NAMES {
            b.insert((n.to_string(), *a));
        }
    });
}

fn builtin_exists(name: &str, arity: usize) -> bool {
    BUILTIN_NAMES.with(|b| b.borrow().contains(&(name.to_string(), arity)))
}
