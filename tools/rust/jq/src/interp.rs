//! The evaluator: every expression is a generator, run in continuation-passing style (each
//! output is handed to a callback, which runs the rest of the pipeline). Backtracking is the
//! callback returning; `empty` is calling it zero times; errors, `break` and `halt` unwind as
//! Flow values. A `try` catches only what its own body raises: errors coming back from the
//! callback (later stages of the pipeline) are wrapped as Outer while they pass through it.
//!
//! Paths: inside `path(f)` (and the assignment operators) values travel with the path they
//! were reached by; indexing a value that has none is jq's "Invalid path expression" error.

use crate::builtins;
use crate::parser::{Ast, BinOp, FuncDef, ObjKey, Param, Pattern, StrAst};
use crate::value::{dump_trunc, Map, Value};
use std::collections::HashMap;
use std::rc::Rc;

pub type Path = Rc<Vec<Value>>;

#[derive(Clone, Debug)]
pub struct Pv {
    pub v: Value,
    pub p: Option<Path>,
}

impl Pv {
    pub fn val(v: Value) -> Pv {
        Pv { v, p: None }
    }
}

#[derive(Debug)]
pub enum Flow {
    Err(Value),
    Break(usize),
    /// halt / halt_error: exit status, and the value to print (halt_error).
    Halt(i32, Option<Value>),
    /// An error from downstream of a try body, on its way out through it.
    Outer(Box<Flow>),
}

pub type R = Result<(), Flow>;

pub fn err(msg: impl Into<String>) -> Flow {
    Flow::Err(Value::string(msg.into()))
}

#[derive(Clone, Default)]
pub struct Env(pub Option<Rc<Node>>);

pub struct Node {
    pub b: Bind,
    pub next: Env,
}

pub enum Bind {
    Var(Rc<str>, Value, Option<Path>),
    Func(&'static FuncDef),
    Closure(Rc<str>, &'static Ast, Env),
    Label(Rc<str>, usize),
    /// An imported module's definition, under its (possibly `alias::`-prefixed) name.
    Lib(Rc<str>, &'static FuncDef, Env),
}

impl Env {
    pub fn push(&self, b: Bind) -> Env {
        Env(Some(Rc::new(Node { b, next: self.clone() })))
    }
    fn iter(&self) -> impl Iterator<Item = &Rc<Node>> {
        let mut cur = self.0.as_ref();
        std::iter::from_fn(move || {
            let n = cur?;
            cur = n.next.0.as_ref();
            Some(n)
        })
    }
}

pub enum Callee {
    User(&'static FuncDef, Env),
    Closure(&'static Ast, Env),
}

type Cb<'c> = &'c mut dyn FnMut(&mut Interp, Pv) -> R;

/// Where `input`/`inputs` read from.
pub trait Inputs {
    fn next(&mut self) -> Option<Result<Value, String>>;
    fn filename(&self) -> Value;
    fn line(&self) -> usize;
}

pub struct Interp {
    /// builtin.jq's definitions and those of included/imported modules, by name/arity.
    pub defs: HashMap<(String, usize), (&'static FuncDef, Env)>,
    pub labels: usize,
    pub inputs: Option<Box<dyn Inputs>>,
    pub env_value: Value,
    pub debug_out: Box<dyn FnMut(&Value)>,
    pub stderr_out: Box<dyn FnMut(&Value)>,
    pub search_list: Vec<String>,
    pub jq_origin: String,
    pub prog_origin: Value,
    /// builtin.jq's definitions in order (for `builtins`).
    pub builtin_order: Vec<(String, usize)>,
    pub depth: usize,
    /// `_modify(lhs; ...)` calls standing for update-assignments in path expressions.
    pub synth: HashMap<usize, &'static Ast>,
}

thread_local! {
    /// The interpreter thread's stack: where it starts, and how much of it may be used.
    static STACK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Called first thing on the interpreter's thread, with its stack size.
pub fn set_stack_base(size: usize) {
    let x = 0u8;
    let here = std::hint::black_box(&x) as *const u8 as usize;
    STACK.with(|s| s.set((here, size - size / 8)));
}

fn max_depth() -> usize {
    thread_local! {
        static MAX: usize = std::env::var("JQ_MAX_EVAL_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_DEPTH);
    }
    MAX.with(|m| *m)
}

/// Measured: a simple recursion traps near 50000 nested evaluations; half leaves room for
/// heavier frames.
#[cfg(target_arch = "wasm32")]
const DEFAULT_MAX_DEPTH: usize = 20000;
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_MAX_DEPTH: usize = usize::MAX;

fn stack_exhausted() -> bool {
    let x = 0u8;
    let here = std::hint::black_box(&x) as *const u8 as usize;
    let (base, limit) = STACK.with(|s| s.get());
    base != 0 && base.saturating_sub(here) > limit
}

impl Interp {
    pub fn new() -> Interp {
        let mut it = Interp {
            defs: HashMap::new(),
            labels: 0,
            inputs: None,
            env_value: Value::Null,
            debug_out: Box::new(|_| {}),
            stderr_out: Box::new(|_| {}),
            search_list: vec![],
            jq_origin: String::new(),
            prog_origin: Value::Null,
            builtin_order: vec![],
            depth: 0,
            synth: HashMap::new(),
        };
        crate::modules::load_builtins(&mut it);
        crate::modules::register_builtin_names(&it);
        it
    }

    pub fn lookup_fn(&self, env: &Env, name: &str, arity: usize) -> Option<Callee> {
        for n in env.iter() {
            match &n.b {
                Bind::Func(fd) if fd.name == name && fd.params.len() == arity => return Some(Callee::User(fd, Env(Some(n.clone())))),
                Bind::Closure(c, ast, cenv) if arity == 0 && &**c == name => return Some(Callee::Closure(ast, cenv.clone())),
                Bind::Lib(n, fd, denv) if fd.params.len() == arity && &**n == name => return Some(Callee::User(fd, denv.clone())),
                _ => {}
            }
        }
        self.defs.get(&(name.to_string(), arity)).map(|(fd, e)| Callee::User(fd, e.clone()))
    }

    fn lookup_var(&self, env: &Env, name: &str) -> Option<(Value, Option<Path>)> {
        for n in env.iter() {
            if let Bind::Var(v, val, p) = &n.b {
                if &**v == name {
                    return Some((val.clone(), p.clone()));
                }
            }
        }
        None
    }

    fn lookup_label(&self, env: &Env, name: &str) -> Option<usize> {
        for n in env.iter() {
            if let Bind::Label(l, id) = &n.b {
                if &**l == name {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Runs `ast` on `input`, giving each output to `cb`. `paths`: track paths (path mode).
    pub fn eval(&mut self, ast: &'static Ast, env: &Env, input: Pv, paths: bool, cb: Cb) -> R {
        // Two guards: the thread's stack (linear memory on wasm), and, on wasm, the native stack
        // the runtime gives the whole machine (32 MiB, the kernel's frames included), which a
        // trap would take down with us: there the depth of evaluation is counted.
        if stack_exhausted() || self.depth >= max_depth() {
            return Err(err("Recursion too deep (out of stack)"));
        }
        self.depth += 1;
        let r = self.eval_inner(ast, env, input, paths, cb);
        self.depth -= 1;
        r
    }

    fn eval_inner(&mut self, ast: &'static Ast, env: &Env, input: Pv, paths: bool, cb: Cb) -> R {
        match ast {
            Ast::Identity => cb(self, input),
            Ast::RecurseDefault => self.call(env, "recurse", &[], input, paths, (0, 0), cb),
            Ast::Lit(v) => cb(self, Pv::val(v.clone())),
            Ast::Str(fmt, parts) => {
                let fmt = fmt.clone().unwrap_or_else(|| "text".into());
                self.eval_str(env, &input, &fmt, parts, parts.len(), &mut |s, text: String| cb(s, Pv::val(Value::string(text))))
            }
            Ast::Format(f) => {
                let v = builtins::format(&input.v, f)?;
                cb(self, Pv::val(v))
            }
            Ast::Index(..) | Ast::Slice(..) | Ast::Iterate(..) => self.eval_index(ast, false, env, input, paths, cb),
            // `.a?`, `.[k]?`, `.[]?`, `.[a:b]?`: only the indexing itself may fail quietly.
            Ast::IndexOpt(inner) => self.eval_index(inner, true, env, input, paths, cb),
            Ast::Try(body, catch) => {
                let r = self.eval(body, env, input.clone(), paths, &mut |s, v| cb(s, v).map_err(|e| Flow::Outer(Box::new(e))));
                match r {
                    Ok(()) => Ok(()),
                    Err(Flow::Outer(e)) => Err(*e),
                    Err(Flow::Err(e)) => {
                        // jq: errors raised by `break`-like control (an object {__jq: n}) are
                        // not caught; everything else is.
                        if let Some(c) = catch {
                            self.eval(c, env, Pv::val(e), false, cb)
                        } else {
                            Ok(())
                        }
                    }
                    Err(other) => Err(other),
                }
            }
            Ast::Neg(t) => self.eval(t, env, input, false, &mut |s, v| {
                let r = builtins::negate(v.v)?;
                cb(s, Pv::val(r))
            }),
            Ast::Array(q) => {
                let mut items = vec![];
                if let Some(q) = q {
                    self.eval(q, env, Pv::val(input.v.clone()), false, &mut |_, v| {
                        items.push(v.v);
                        Ok(())
                    })?;
                }
                cb(self, Pv::val(Value::arr(items)))
            }
            Ast::Object(pairs) => self.eval_object(env, &input.v, pairs, 0, Map::new(), cb),
            Ast::Pipe(a, b) => self.eval(a, env, input, paths, &mut |s, v| s.eval(b, env, v, paths, cb)),
            Ast::Comma(a, b) => {
                self.eval(a, env, input.clone(), paths, cb)?;
                self.eval(b, env, input, paths, cb)
            }
            Ast::Bin(op, l, r) if !paths && matches!(**l, Ast::Identity) && input_free(r) => {
                // `. op rhs`, the rhs not reading the input: its outputs first, then the input
                // itself goes into the last operation (an array or object held only here is
                // added to in place, as jq does in `reduce ... (.; . + [$x])`).
                let (rvs, e) = collect_values(self, r, env, input.v.clone());
                let n = rvs.len();
                let mut inp = Some(input.v);
                for (i, rv) in rvs.into_iter().enumerate() {
                    let lv = if i + 1 == n && e.is_none() { inp.take().unwrap() } else { inp.clone().unwrap() };
                    let v = builtins::binop(*op, lv, rv)?;
                    cb(self, Pv::val(v))?;
                }
                match e {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            Ast::Bin(op, l, r) => {
                let inp = input.v.clone();
                self.eval(r, env, Pv::val(input.v.clone()), false, &mut |s, rv| {
                    s.eval(l, env, Pv::val(inp.clone()), false, &mut |s, lv| {
                        let v = builtins::binop(*op, lv.v, rv.v.clone())?;
                        cb(s, Pv::val(v))
                    })
                })
            }
            Ast::And(a, b) => {
                let inp = input.v.clone();
                self.eval(a, env, Pv::val(input.v.clone()), false, &mut |s, av| {
                    if !av.v.truthy() {
                        return cb(s, Pv::val(Value::Bool(false)));
                    }
                    s.eval(b, env, Pv::val(inp.clone()), false, &mut |s, bv| cb(s, Pv::val(Value::Bool(bv.v.truthy()))))
                })
            }
            Ast::Or(a, b) => {
                let inp = input.v.clone();
                self.eval(a, env, Pv::val(input.v.clone()), false, &mut |s, av| {
                    if av.v.truthy() {
                        return cb(s, Pv::val(Value::Bool(true)));
                    }
                    s.eval(b, env, Pv::val(inp.clone()), false, &mut |s, bv| cb(s, Pv::val(Value::Bool(bv.v.truthy()))))
                })
            }
            Ast::Alt(a, b) => {
                let mut found = false;
                self.eval(a, env, input.clone(), paths, &mut |s, v| {
                    if v.v.truthy() {
                        found = true;
                        cb(s, v)
                    } else {
                        Ok(())
                    }
                })?;
                if !found {
                    self.eval(b, env, input, paths, cb)?;
                }
                Ok(())
            }
            // In a path expression, jq's own _modify runs (and fails as jq's does).
            Ast::Assign(op, lhs, rhs) if paths && *op != "=" => self.eval_assign_paths(ast, op, lhs, rhs, env, input, cb),
            Ast::Assign(op, lhs, rhs) => self.eval_assign(op, lhs, rhs, env, input, cb),
            Ast::If(c, t, e) => {
                // The condition's outputs first (only their truth is kept), so that the input
                // goes on to the (last) branch held once.
                let mut conds = vec![];
                let r = self.eval(c, env, Pv::val(input.v.clone()), false, &mut |_, cv| {
                    conds.push(cv.v.truthy());
                    Ok(())
                });
                let n = conds.len();
                let mut inp = Some(input);
                for (i, c) in conds.into_iter().enumerate() {
                    let v = if i + 1 == n && r.is_ok() { inp.take().unwrap() } else { inp.clone().unwrap() };
                    if c {
                        self.eval(t, env, v, paths, cb)?;
                    } else if let Some(e) = e {
                        self.eval(e, env, v, paths, cb)?;
                    } else {
                        cb(self, v)?;
                    }
                }
                r
            }
            Ast::Reduce(src, pats, init, upd) => {
                let inp = input.clone();
                self.eval(init, env, input, paths, &mut |s, iv| {
                    let mut acc: Option<Pv> = Some(iv);
                    s.eval(src, env, Pv::val(inp.v.clone()), false, &mut |s, item| {
                        s.with_bindings(env, pats, &item, &mut |s, env2, alt| {
                            // A retried alternative finds the accumulator gone (null), as jq's.
                            let cur = if alt > 0 { Pv::val(Value::Null) } else { acc.take().unwrap_or(Pv::val(Value::Null)) };
                            let mut last = None;
                            s.eval(upd, &env2, cur, paths, &mut |_, o| {
                                last = Some(o);
                                Ok(())
                            })?;
                            acc = Some(last.unwrap_or(Pv::val(Value::Null)));
                            Ok(())
                        })
                    })?;
                    // The accumulator is a variable in jq: the result has no path.
                    cb(s, Pv::val(acc.take().map_or(Value::Null, |a| a.v)))
                })
            }
            Ast::Foreach(src, pats, init, upd, ext) => {
                let inp = input.clone();
                self.eval(init, env, input, paths, &mut |s, iv| {
                    let mut state = iv;
                    s.eval(src, env, inp.clone(), paths, &mut |s, item| {
                        s.with_bindings(env, pats, &item, &mut |s, env2, alt| {
                            let cur = if alt > 0 { Pv::val(Value::Null) } else { state.clone() };
                            s.eval(upd, &env2, cur, paths, &mut |s, o| {
                                state = o.clone();
                                match ext {
                                    Some(x) => s.eval(x, &env2, o, paths, cb),
                                    None => cb(s, o),
                                }
                            })
                        })
                    })
                })
            }
            Ast::Defs(defs, rest) => {
                let mut e = env.clone();
                for d in defs {
                    let d: &'static FuncDef = d;
                    e = e.push(Bind::Func(d));
                }
                self.eval(rest, &e, input, paths, cb)
            }
            Ast::Call(name, args, span) => self.call(env, name, args, input, paths, *span, cb),
            Ast::Var(name, _) => {
                let (v, p) = match self.lookup_var(env, name) {
                    Some(x) => x,
                    None => match name.as_str() {
                        "ENV" => (self.env_value.clone(), None),
                        _ => return Err(err(format!("${name} is not defined"))),
                    },
                };
                // In a path expression a variable has a path when it came with one (a foreach
                // source), or when it is the very value the path leads to (jq compares by
                // identity, jv_identical).
                let p = if !paths {
                    None
                } else if p.is_some() {
                    p
                } else if identical(&v, &input.v) {
                    input.p
                } else {
                    None
                };
                cb(self, Pv { v, p })
            }
            Ast::As(src, pats, body) => {
                let inp = input.clone();
                self.eval(src, env, Pv::val(input.v.clone()), false, &mut |s, sv| s.destructure_alts(env, pats, &sv.v, body, inp.clone(), paths, cb))
            }
            Ast::Label(name, body) => {
                self.labels += 1;
                let id = self.labels;
                let env2 = env.push(Bind::Label(Rc::from(name.as_str()), id));
                match self.eval(body, &env2, input, paths, cb) {
                    Err(Flow::Break(b)) if b == id => Ok(()),
                    r => r,
                }
            }
            Ast::Break(name, _) => match self.lookup_label(env, name) {
                Some(id) => Err(Flow::Break(id)),
                None => Err(err(format!("$*label-{name} is not defined"))),
            },
        }
    }

    fn eval_index(&mut self, ast: &'static Ast, opt: bool, env: &Env, input: Pv, paths: bool, cb: Cb) -> R {
        // A failed index: an error, or (optional) no output.
        let fail = |e: Flow| if opt { Ok(()) } else { Err(e) };
        match ast {
            Ast::Index(t, k) => {
                // The key outer, evaluated on the input; then the term.
                let inp = input.clone();
                self.eval(k, env, Pv::val(input.v.clone()), false, &mut |s, key| {
                    s.eval(t, env, inp.clone(), paths, &mut |s, tv| {
                        let p = if paths { Some(path_step(&tv, &key.v)?) } else { None };
                        match index(&tv.v, &key.v) {
                            Ok(r) => cb(s, Pv { v: r, p }),
                            Err(e) => fail(e),
                        }
                    })
                })
            }
            Ast::Slice(t, from, to) => {
                let inp = input.clone();
                let lit_null: &'static Ast = &Ast::Lit(Value::Null);
                let from: &'static Ast = from.as_deref().unwrap_or(lit_null);
                let to: &'static Ast = to.as_deref().unwrap_or(lit_null);
                self.eval(to, env, Pv::val(input.v.clone()), false, &mut |s, tv2| {
                    let inp2 = inp.clone();
                    s.eval(from, env, Pv::val(inp.v.clone()), false, &mut |s, fv| {
                        let mut m = Map::new();
                        m.insert(Rc::from("start"), fv.v.clone());
                        m.insert(Rc::from("end"), tv2.v.clone());
                        let key = Value::obj(m);
                        s.eval(t, env, inp2.clone(), paths, &mut |s, tv| {
                            let p = if paths { Some(path_step(&tv, &key)?) } else { None };
                            match index(&tv.v, &key) {
                                Ok(r) => cb(s, Pv { v: r, p }),
                                Err(e) => fail(e),
                            }
                        })
                    })
                })
            }
            Ast::Iterate(t) => self.eval(t, env, input, paths, &mut |s, tv| {
                if paths && tv.p.is_none() {
                    return Err(err(format!("Invalid path expression near attempt to iterate through {}", dump_trunc(&tv.v, 30))));
                }
                match &tv.v {
                    Value::Arr(a) => {
                        for (i, x) in a.iter().enumerate() {
                            let p = tv.p.as_ref().map(|p| push_path(p, Value::num(i as f64)));
                            cb(s, Pv { v: x.clone(), p })?;
                        }
                        Ok(())
                    }
                    Value::Obj(m) => {
                        for (k, x) in m.iter() {
                            let p = tv.p.as_ref().map(|p| push_path(p, Value::Str(k.clone())));
                            cb(s, Pv { v: x.clone(), p })?;
                        }
                        Ok(())
                    }
                    v => fail(err(format!("Cannot iterate over {} ({})", v.kind(), dump_trunc(v, 30)))),
                }
            }),
            _ => unreachable!(),
        }
    }

    /// String interpolation: the parts right to left, the last one outermost (jq builds the
    /// string with `+`, whose right side is the outer loop).
    fn eval_str(&mut self, env: &Env, input: &Pv, fmt: &str, parts: &'static [StrAst], k: usize, cb: &mut dyn FnMut(&mut Interp, String) -> R) -> R {
        if k == 0 {
            return cb(self, String::new());
        }
        match &parts[k - 1] {
            StrAst::Text(t) => self.eval_str(env, input, fmt, parts, k - 1, &mut |s, l| cb(s, l + t)),
            StrAst::Interp(q) => self.eval(q, env, Pv::val(input.v.clone()), false, &mut |s, v| {
                let r = builtins::format(&v.v, fmt)?;
                let r = match r {
                    Value::Str(x) => x,
                    other => Rc::from(crate::value::dump(&other, &crate::value::Fmt::compact()).as_str()),
                };
                s.eval_str(env, input, fmt, parts, k - 1, &mut |s, l| cb(s, l + &r))
            }),
        }
    }

    fn eval_object(&mut self, env: &Env, input: &Value, pairs: &'static [(Ast, Ast)], i: usize, acc: Map, cb: Cb) -> R {
        if i == pairs.len() {
            return cb(self, Pv::val(Value::obj(acc)));
        }
        let (k, v) = &pairs[i];
        self.eval(k, env, Pv::val(input.clone()), false, &mut |s, kv| {
            let key = match &kv.v {
                Value::Str(x) => x.clone(),
                other => return Err(err(format!("Cannot use {} ({}) as object key", other.kind(), dump_trunc(other, 30)))),
            };
            s.eval(v, env, Pv::val(input.clone()), false, &mut |s, vv| {
                let mut m = acc.clone();
                m.insert(key.clone(), vv.v);
                s.eval_object(env, input, pairs, i + 1, m, cb)
            })
        })
    }

    pub fn call(&mut self, env: &Env, name: &str, args: &'static [Ast], input: Pv, paths: bool, span: (usize, usize), cb: Cb) -> R {
        match self.lookup_fn(env, name, args.len()) {
            Some(Callee::Closure(ast, cenv)) => self.eval(ast, &cenv, input, paths, cb),
            Some(Callee::User(fd, defenv)) => {
                let mut e = defenv;
                for (p, a) in fd.params.iter().zip(args.iter()) {
                    let n = match p {
                        Param::Filter(n) | Param::Value(n) => n,
                    };
                    e = e.push(Bind::Closure(Rc::from(n.as_str()), a, env.clone()));
                }
                self.bind_params(fd, 0, e, args, env, input, paths, cb)
            }
            None => builtins::call_native(self, env, name, args, input, paths, span, cb),
        }
    }

    /// `$name` parameters: each argument's outputs, the first parameter outermost.
    #[allow(clippy::too_many_arguments)]
    fn bind_params(&mut self, fd: &'static FuncDef, k: usize, e: Env, args: &'static [Ast], callenv: &Env, input: Pv, paths: bool, cb: Cb) -> R {
        if k == fd.params.len() {
            return self.eval(&fd.body, &e, input, paths, cb);
        }
        match &fd.params[k] {
            Param::Filter(_) => self.bind_params(fd, k + 1, e, args, callenv, input, paths, cb),
            Param::Value(n) => {
                let inp = input.clone();
                self.eval(&args[k], callenv, Pv::val(input.v.clone()), false, &mut |s, v| {
                    let e2 = e.push(Bind::Var(Rc::from(n.as_str()), v.v, None));
                    s.bind_params(fd, k + 1, e2, args, callenv, inp.clone(), paths, cb)
                })
            }
        }
    }

    /// Native functions' value arguments: the last outermost.
    pub fn eval_args(&mut self, env: &Env, args: &'static [Ast], input: &Value, cb: &mut dyn FnMut(&mut Interp, &[Value]) -> R) -> R {
        let mut vals = vec![Value::Null; args.len()];
        self.eval_args_k(env, args, input, args.len(), &mut vals, cb)
    }

    fn eval_args_k(&mut self, env: &Env, args: &'static [Ast], input: &Value, k: usize, vals: &mut Vec<Value>, cb: &mut dyn FnMut(&mut Interp, &[Value]) -> R) -> R {
        if k == 0 {
            let v = vals.clone();
            return cb(self, &v);
        }
        self.eval(&args[k - 1], env, Pv::val(input.clone()), false, &mut |s, v| {
            vals[k - 1] = v.v;
            s.eval_args_k(env, args, input, k - 1, vals, cb)
        })
    }

    // Destructuring

    fn pattern_vars(p: &Pattern, out: &mut Vec<String>) {
        match p {
            Pattern::Var(n) => {
                if !out.contains(n) {
                    out.push(n.clone())
                }
            }
            Pattern::Array(ps) => ps.iter().for_each(|p| Self::pattern_vars(p, out)),
            Pattern::Object(entries) => {
                for (k, p) in entries {
                    if let ObjKey::Var(n) = k {
                        if !out.contains(n) {
                            out.push(n.clone());
                        }
                    }
                    if let Some(p) = p {
                        Self::pattern_vars(p, out);
                    }
                }
            }
        }
    }

    /// Binds a pattern against a value; key expressions may have several outputs, so this
    /// is a generator of environments.
    /// `base` is the environment the destructuring started from: a variable a pattern binds
    /// twice keeps its first binding (as jq's code for it stores the first one last).
    fn bind_pattern(&mut self, env: &Env, base: &Env, p: &'static Pattern, v: &Value, cb: &mut dyn FnMut(&mut Interp, Env) -> R) -> R {
        match p {
            Pattern::Var(n) => {
                if bound_since(env, base, n) {
                    return cb(self, env.clone());
                }
                cb(self, env.push(Bind::Var(Rc::from(n.as_str()), v.clone(), None)))
            }
            // jq matches an array pattern's elements last first.
            Pattern::Array(ps) => self.bind_array(env, base, ps, v, ps.len(), cb),
            Pattern::Object(entries) => self.bind_object(env, base, entries, v, 0, cb),
        }
    }

    fn bind_array(&mut self, env: &Env, base: &Env, ps: &'static [Pattern], v: &Value, i: usize, cb: &mut dyn FnMut(&mut Interp, Env) -> R) -> R {
        if i == 0 {
            return cb(self, env.clone());
        }
        let k = i - 1;
        let item = index(v, &Value::num(k as f64))?;
        self.bind_pattern(env, base, &ps[k], &item, &mut |s, e| s.bind_array(&e, base, ps, v, k, cb))
    }

    fn bind_object(&mut self, env: &Env, base: &Env, entries: &'static [(ObjKey, Option<Pattern>)], v: &Value, i: usize, cb: &mut dyn FnMut(&mut Interp, Env) -> R) -> R {
        if i == entries.len() {
            return cb(self, env.clone());
        }
        let (k, sub) = &entries[i];
        match k {
            ObjKey::Var(n) => {
                let item = index(v, &Value::str(n))?;
                let e = if bound_since(env, base, n) { env.clone() } else { env.push(Bind::Var(Rc::from(n.as_str()), item.clone(), None)) };
                match sub {
                    Some(p) => self.bind_pattern(&e, base, p, &item, &mut |s, e3| s.bind_object(&e3, base, entries, v, i + 1, cb)),
                    None => self.bind_object(&e, base, entries, v, i + 1, cb),
                }
            }
            ObjKey::Expr(kx) => {
                // Key expressions see the variables bound so far (`{$a, ($a): $b}`).
                let kenv = env.clone();
                self.eval(kx, &kenv, Pv::val(v.clone()), false, &mut |s, kv| {
                    let item = index(v, &kv.v)?;
                    let p = sub.as_ref().unwrap();
                    s.bind_pattern(&kenv, base, p, &item, &mut |s, e3| s.bind_object(&e3, base, entries, v, i + 1, cb))
                })
            }
        }
    }

    /// `E as P1 ?// P2 ... | body`: all patterns' variables exist (null when not bound).
    #[allow(clippy::too_many_arguments)]
    fn destructure_alts(&mut self, env: &Env, pats: &'static [Pattern], v: &Value, body: &'static Ast, input: Pv, paths: bool, cb: Cb) -> R {
        self.with_bindings(env, pats, &Pv::val(v.clone()), &mut |s, e, _| s.eval(body, &e, input.clone(), paths, cb))
    }

    /// Runs `body` for each binding of the patterns to `item` (key expressions may give
    /// several). With alternatives (`?//`), any error raised while an alternative runs, its
    /// body and what follows included (even a `break`, as jq's DESTRUCTURE_ALT catches every
    /// raised error), moves on to the next alternative; the last one's errors go on.
    fn with_bindings(&mut self, env: &Env, pats: &'static [Pattern], item: &Pv, body: &mut dyn FnMut(&mut Interp, Env, usize) -> R) -> R {
        let base = if pats.len() > 1 {
            let mut all = vec![];
            for p in pats {
                Self::pattern_vars(p, &mut all);
            }
            let mut e = env.clone();
            for n in &all {
                e = e.push(Bind::Var(Rc::from(n.as_str()), Value::Null, None));
            }
            e
        } else {
            env.clone()
        };
        for (i, p) in pats.iter().enumerate() {
            let last = i + 1 == pats.len();
            let r = match p {
                // A plain variable keeps the item's path (a foreach source in a path expression).
                Pattern::Var(n) => body(self, base.push(Bind::Var(Rc::from(n.as_str()), item.v.clone(), item.p.clone())), i),
                _ => self.bind_pattern(&base, &base, p, &item.v, &mut |s, e| body(s, e, i)),
            };
            match r {
                Err(Flow::Err(_) | Flow::Outer(_) | Flow::Break(_)) if !last => continue,
                r => return r,
            }
        }
        Ok(())
    }

    // Assignment

    #[allow(clippy::too_many_arguments)]
    fn eval_assign_paths(&mut self, ast: &'static Ast, op: &'static str, lhs: &'static Ast, rhs: &'static Ast, env: &Env, input: Pv, cb: Cb) -> R {
        let call = *self.synth.entry(ast as *const Ast as usize).or_insert_with(|| {
            let tmp = || Box::new(Ast::Var("*tmp".into(), (0, 0)));
            let upd = match op {
                "|=" => rhs.clone(),
                "//" => Ast::Alt(Box::new(Ast::Identity), tmp()),
                o => {
                    let b = match o {
                        "+" => BinOp::Add,
                        "-" => BinOp::Sub,
                        "*" => BinOp::Mul,
                        "/" => BinOp::Div,
                        _ => BinOp::Mod,
                    };
                    Ast::Bin(b, Box::new(Ast::Identity), tmp())
                }
            };
            Box::leak(Box::new(Ast::Call("_modify".into(), vec![lhs.clone(), upd], (0, 0))))
        });
        if op == "|=" {
            return self.eval(call, env, input, true, cb);
        }
        let inp = input.clone();
        self.eval(rhs, env, Pv::val(input.v), false, &mut |s, rv| {
            let e2 = env.push(Bind::Var(Rc::from("*tmp"), rv.v, None));
            s.eval(call, &e2, inp.clone(), true, cb)
        })
    }

    fn eval_assign(&mut self, op: &'static str, lhs: &'static Ast, rhs: &'static Ast, env: &Env, input: Pv, cb: Cb) -> R {
        // The right side runs on the input, as the outer loop. When it does not read the
        // input, its outputs are taken first, so that the last update gets the input itself
        // (held only here, it is then changed in place).
        let rhs_free = input_free(rhs);
        let apply = |s: &mut Interp, inp: Value, rv: Value| -> Result<Value, Flow> {
            match op {
                "=" => {
                    let paths = s.collect_paths(lhs, env, &inp)?;
                    let mut acc = inp;
                    for p in paths {
                        acc = setpath(acc, &p, rv.clone())?;
                    }
                    Ok(acc)
                }
                _ => s.modify(lhs, env, inp, &mut |_, v| {
                    if op == "//" {
                        return Ok(Some(if v.truthy() { v } else { rv.clone() }));
                    }
                    let b = match op {
                        "+" => BinOp::Add,
                        "-" => BinOp::Sub,
                        "*" => BinOp::Mul,
                        "/" => BinOp::Div,
                        _ => BinOp::Mod,
                    };
                    Ok(Some(builtins::binop(b, v, rv.clone())?))
                }),
            }
        };
        match op {
            "|=" => {
                let r = self.modify(lhs, env, input.v, &mut |s, v| s.first_output(rhs, env, v))?;
                cb(self, Pv::val(r))
            }
            _ if rhs_free => {
                let (rvs, e) = collect_values(self, rhs, env, input.v.clone());
                let n = rvs.len();
                let mut inp = Some(input.v);
                for (i, rv) in rvs.into_iter().enumerate() {
                    let v = if i + 1 == n && e.is_none() { inp.take().unwrap() } else { inp.clone().unwrap() };
                    let r = apply(self, v, rv)?;
                    cb(self, Pv::val(r))?;
                }
                match e {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            _ => {
                let inp = input.v.clone();
                self.eval(rhs, env, Pv::val(input.v), false, &mut |s, rv| {
                    let r = apply(s, inp.clone(), rv.v)?;
                    cb(s, Pv::val(r))
                })
            }
        }
    }

    fn first_output(&mut self, f: &'static Ast, env: &Env, v: Value) -> Result<Option<Value>, Flow> {
        self.labels += 1;
        let id = self.labels;
        let mut out = None;
        let r = self.eval(f, env, Pv::val(v), false, &mut |_, o| {
            out = Some(o.v);
            Err(Flow::Break(id))
        });
        match r {
            Ok(()) => Ok(out),
            Err(Flow::Break(b)) if b == id => Ok(out),
            Err(e) => Err(e),
        }
    }

    pub fn collect_paths(&mut self, f: &'static Ast, env: &Env, input: &Value) -> Result<Vec<Vec<Value>>, Flow> {
        let mut out = vec![];
        self.eval(f, env, Pv { v: input.clone(), p: Some(Rc::new(vec![])) }, true, &mut |_, pv| match pv.p {
            Some(p) => {
                out.push((*p).clone());
                Ok(())
            }
            None => Err(err(format!("Invalid path expression with result {}", dump_trunc(&pv.v, 30)))),
        })?;
        Ok(out)
    }

    /// _modify: each path of `lhs` updated with the first output of `upd`; paths whose update
    /// is empty are deleted at the end.
    fn modify(&mut self, lhs: &'static Ast, env: &Env, input: Value, upd: &mut dyn FnMut(&mut Interp, Value) -> Result<Option<Value>, Flow>) -> Result<Value, Flow> {
        let paths = self.collect_paths(lhs, env, &input)?;
        let mut acc = input;
        let mut dels = vec![];
        for p in paths {
            let cur = getpath(&acc, &p)?;
            match upd(self, cur)? {
                Some(nv) => acc = setpath(acc, &p, nv)?,
                None => dels.push(Value::arr(p)),
            }
        }
        if !dels.is_empty() {
            acc = delpaths(acc, &dels)?;
        }
        Ok(acc)
    }
}

/// Whether evaluating `a` never reads its input (so it can run before, and apart from, it).
pub fn input_free(a: &Ast) -> bool {
    match a {
        Ast::Lit(_) | Ast::Var(..) | Ast::Break(..) => true,
        Ast::Str(_, parts) => parts.iter().all(|p| match p {
            StrAst::Text(_) => true,
            StrAst::Interp(q) => input_free(q),
        }),
        Ast::Array(None) => true,
        Ast::Array(Some(q)) | Ast::Neg(q) | Ast::Label(_, q) | Ast::Iterate(q) | Ast::IndexOpt(q) => input_free(q),
        Ast::Object(pairs) => pairs.iter().all(|(k, v)| input_free(k) && input_free(v)),
        Ast::Pipe(a, _) => input_free(a),
        Ast::Comma(a, b) | Ast::Bin(_, a, b) | Ast::And(a, b) | Ast::Or(a, b) | Ast::Alt(a, b) | Ast::Index(a, b) => input_free(a) && input_free(b),
        Ast::Slice(t, f, e) => input_free(t) && f.as_deref().map_or(true, input_free) && e.as_deref().map_or(true, input_free),
        Ast::If(c, t, e) => input_free(c) && input_free(t) && e.as_deref().map_or(false, input_free),
        Ast::Try(b, _) => input_free(b),
        Ast::Reduce(src, _, init, _) | Ast::Foreach(src, _, init, _, _) => input_free(src) && input_free(init),
        Ast::As(src, _, body) => input_free(src) && input_free(body),
        Ast::Defs(_, rest) => input_free(rest),
        _ => false,
    }
}

/// A generator's values up to its first error (which comes after them).
pub fn collect_values(it: &mut Interp, f: &'static Ast, env: &Env, input: Value) -> (Vec<Value>, Option<Flow>) {
    let mut out = vec![];
    let r = it.eval(f, env, Pv::val(input), false, &mut |_, v| {
        out.push(v.v);
        Ok(())
    });
    (out, r.err())
}

fn push_path(p: &Path, k: Value) -> Path {
    let mut v = (**p).clone();
    v.push(k);
    Rc::new(v)
}

fn path_step(tv: &Pv, key: &Value) -> Result<Path, Flow> {
    match &tv.p {
        Some(p) => Ok(push_path(p, key.clone())),
        None => Err(err(format!(
            "Invalid path expression near attempt to access element {} of {}",
            dump_trunc(key, 30),
            dump_trunc(&tv.v, 30)
        ))),
    }
}

/// jv_identical: the same value object (scalars: the same value; numbers' literal objects).
fn identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => match (&x.lit, &y.lit) {
            (Some(p), Some(q)) => Rc::ptr_eq(p, q),
            (None, None) => x.f.to_bits() == y.f.to_bits(),
            _ => false,
        },
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y),
        (Value::Arr(x), Value::Arr(y)) => Rc::ptr_eq(x, y),
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// Whether `name` was bound between `base` and `env` (by the pattern being matched).
fn bound_since(env: &Env, base: &Env, name: &str) -> bool {
    let stop = base.0.as_ref().map(|n| Rc::as_ptr(n));
    let mut cur = env.0.as_ref();
    while let Some(n) = cur {
        if Some(Rc::as_ptr(n)) == stop {
            return false;
        }
        if let Bind::Var(v, _, _) = &n.b {
            if &**v == name {
                return true;
            }
        }
        cur = n.next.0.as_ref();
    }
    false
}

/// jv_get: `.[k]` with jq's rules and messages.
pub fn index(t: &Value, k: &Value) -> Result<Value, Flow> {
    match (t, k) {
        (Value::Obj(m), Value::Str(s)) => Ok(m.get(s).cloned().unwrap_or(Value::Null)),
        (Value::Arr(a), Value::Num(n)) => {
            if n.f.is_nan() {
                return Ok(Value::Null);
            }
            let i = n.f.clamp(i32::MIN as f64, i32::MAX as f64) as i64;
            let idx = if i < 0 { i + a.len() as i64 } else { i };
            Ok(if idx >= 0 && (idx as usize) < a.len() { a[idx as usize].clone() } else { Value::Null })
        }
        (Value::Arr(a), Value::Obj(_)) => {
            let (s, e) = parse_slice(a.len(), k)?;
            Ok(Value::arr(a[s..e].to_vec()))
        }
        (Value::Str(st), Value::Obj(_)) => {
            let chars: Vec<char> = st.chars().collect();
            let (s, e) = parse_slice(chars.len(), k)?;
            Ok(Value::string(chars[s..e].iter().collect()))
        }
        (Value::Arr(a), Value::Arr(b)) => {
            crate::value::took_too_deep();
            let r = builtins::array_indexes(a, b);
            if crate::value::took_too_deep() {
                return Err(err("Equality check too deep"));
            }
            Ok(r)
        }
        (Value::Null, Value::Str(_) | Value::Num(_) | Value::Obj(_)) => Ok(Value::Null),
        _ => Err(err(format!("Cannot index {} with {} ({})", t.kind(), k.kind(), dump_trunc(k, 30)))),
    }
}

/// parse_slice: {start, end} into array bounds.
pub fn parse_slice(len: usize, k: &Value) -> Result<(usize, usize), Flow> {
    let Value::Obj(m) = k else { return Err(err("Only arrays and strings can be sliced")) };
    let len_f = len as f64;
    let start = m.get("start").cloned().unwrap_or(Value::Null);
    let end = m.get("end").cloned().unwrap_or(Value::Null);
    let ds = match start {
        Value::Null => 0.0,
        Value::Num(n) => n.f,
        _ => return Err(err("Array/string slice indices must be integers")),
    };
    let de = match end {
        Value::Null => len_f,
        Value::Num(n) => n.f,
        _ => return Err(err("Array/string slice indices must be integers")),
    };
    let mut ds = if ds.is_nan() { 0.0 } else { ds };
    if ds < 0.0 {
        ds += len_f;
    }
    if ds < 0.0 {
        ds = 0.0;
    }
    if ds > len_f {
        ds = len_f;
    }
    let start = if ds > i32::MAX as f64 { i32::MAX as i64 } else { ds as i64 };
    let mut de = if de.is_nan() { len_f } else { de };
    if de < 0.0 {
        de += len_f;
    }
    if de < 0.0 {
        de = start as f64;
    }
    let mut end = if de > i32::MAX as f64 { i32::MAX as i64 } else { de as i64 };
    if end > len as i64 {
        end = len as i64;
    }
    if end < len as i64 && (end as f64) < de {
        end += 1;
    }
    if end < start {
        end = start;
    }
    Ok((start as usize, end as usize))
}

/// jv_set, on an owned value: a container held only here is changed in place.
pub fn set(t: Value, k: &Value, v: Value) -> Result<Value, Flow> {
    match (t, k) {
        (t @ (Value::Obj(_) | Value::Null), Value::Str(s)) => {
            let mut m = match t {
                Value::Obj(m) => m,
                _ => Rc::new(Map::new()),
            };
            Rc::make_mut(&mut m).insert(s.clone(), v);
            Ok(Value::Obj(m))
        }
        (t @ (Value::Arr(_) | Value::Null), Value::Num(n)) => {
            if n.f.is_nan() {
                return Err(err("Cannot set array element at NaN index"));
            }
            let mut a = match t {
                Value::Arr(a) => a,
                _ => Rc::new(vec![]),
            };
            let d = n.f.clamp(i32::MIN as f64, i32::MAX as f64) as i64;
            let idx = if d < 0 { d + a.len() as i64 } else { d };
            if idx < 0 {
                return Err(err("Out of bounds negative array index"));
            }
            if idx > (i32::MAX >> 2) as i64 {
                return Err(err("Array index too large"));
            }
            let idx = idx as usize;
            let av = Rc::make_mut(&mut a);
            if idx >= av.len() {
                av.resize(idx + 1, Value::Null);
            }
            av[idx] = v;
            Ok(Value::Arr(a))
        }
        (t @ (Value::Arr(_) | Value::Null), Value::Obj(_)) => {
            let a = match t {
                Value::Arr(a) => a,
                _ => Rc::new(vec![]),
            };
            let (s, e) = parse_slice(a.len(), k)?;
            let Value::Arr(ins) = v else { return Err(err("A slice of an array can only be assigned another array")) };
            let mut out = a[..s].to_vec();
            out.extend(ins.iter().cloned());
            out.extend_from_slice(&a[e..]);
            Ok(Value::arr(out))
        }
        (Value::Str(_), Value::Obj(_)) => Err(err("Cannot update string slices")),
        (t, _) => Err(err(format!("Cannot update field at {} index of {}", k.kind(), t.kind()))),
    }
}

/// The child at `k`, taken out of an owned container (left null there) so that it too is
/// held only once; or a copy, for slices; or jv_get's error.
fn take_child(t: &mut Value, k: &Value) -> Result<Value, Flow> {
    match (&mut *t, k) {
        (Value::Obj(m), Value::Str(s)) => Ok(match Rc::make_mut(m).get_mut(s) {
            Some(x) => std::mem::replace(x, Value::Null),
            None => Value::Null,
        }),
        (Value::Arr(a), Value::Num(n)) if !n.f.is_nan() => {
            let i = n.f.clamp(i32::MIN as f64, i32::MAX as f64) as i64;
            let idx = if i < 0 { i + a.len() as i64 } else { i };
            if idx >= 0 && (idx as usize) < a.len() {
                Ok(std::mem::replace(&mut Rc::make_mut(a)[idx as usize], Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        _ => index(t, k),
    }
}

pub fn getpath(v: &Value, p: &[Value]) -> Result<Value, Flow> {
    let mut cur = v.clone();
    for (i, k) in p.iter().enumerate() {
        if matches!(cur, Value::Null) {
            // null all the way down (jv_getpath on null returns null for any key).
            let _ = i;
            return Ok(Value::Null);
        }
        cur = index(&cur, k)?;
    }
    Ok(cur)
}

pub fn setpath(mut root: Value, p: &[Value], v: Value) -> Result<Value, Flow> {
    if p.is_empty() {
        return Ok(v);
    }
    let k = &p[0];
    let sub = take_child(&mut root, k)?;
    let nv = setpath(sub, &p[1..], v)?;
    set(root, k, nv)
}

/// jv_delpaths: sorted paths, deepest handled through their parents.
pub fn delpaths(v: Value, paths: &[Value]) -> Result<Value, Flow> {
    let mut ps: Vec<Value> = paths.to_vec();
    ps.sort_by(|a, b| a.compare(b));
    for p in &ps {
        match p {
            Value::Arr(a) if a.len() > 10000 => return Err(err("Path too deep")),
            Value::Arr(_) => {}
            _ => return Err(err(format!("Path must be specified as array, not {}", p.kind()))),
        }
    }
    if ps.is_empty() {
        return Ok(v);
    }
    let Value::Arr(first) = &ps[0] else { unreachable!() };
    if first.is_empty() {
        return Ok(Value::Null);
    }
    let lists: Vec<Vec<Value>> = ps.iter().map(|p| if let Value::Arr(a) = p { (**a).clone() } else { vec![] }).collect();
    delpaths_sorted(v, &lists, 0)
}

fn delpaths_sorted(mut object: Value, paths: &[Vec<Value>], start: usize) -> Result<Value, Flow> {
    let mut delkeys = vec![];
    let mut i = 0;
    while i < paths.len() {
        let delkey = paths[i].len() == start + 1;
        let key = paths[i][start].clone();
        let mut j = i + 1;
        while j < paths.len() && paths[j][start] == key {
            j += 1;
        }
        if delkey {
            delkeys.push(key);
        } else {
            let sub = index(&object, &key)?;
            if !matches!(sub, Value::Null) {
                let sub = take_child(&mut object, &key)?;
                let nsub = delpaths_sorted(sub, &paths[i..j], start + 1)?;
                object = set(object, &key, nsub)?;
            }
        }
        i = j;
    }
    dels(object, &delkeys)
}

fn dels(t: Value, keys: &[Value]) -> Result<Value, Flow> {
    if keys.is_empty() || matches!(t, Value::Null) {
        return Ok(t);
    }
    match t {
        Value::Arr(mut a) => {
            let len = a.len() as i64;
            let mut del = vec![false; a.len()];
            for k in keys {
                match k {
                    Value::Num(n) => {
                        if n.f.is_nan() {
                            continue;
                        }
                        let mut i = n.f as i64;
                        if n.f < 0.0 {
                            i += len;
                        }
                        if i >= 0 && i < len {
                            del[i as usize] = true;
                        }
                    }
                    Value::Obj(_) => {
                        let (s, e) = parse_slice(a.len(), k)?;
                        for d in del.iter_mut().take(e).skip(s) {
                            *d = true;
                        }
                    }
                    other => return Err(err(format!("Cannot delete {} element of array", other.kind()))),
                }
            }
            let mut it = del.into_iter();
            Rc::make_mut(&mut a).retain(|_| !it.next().unwrap());
            Ok(Value::Arr(a))
        }
        Value::Obj(mut m) => {
            for k in keys {
                if !matches!(k, Value::Str(_)) {
                    return Err(err(format!("Cannot delete {} field of object", k.kind())));
                }
            }
            let mm = Rc::make_mut(&mut m);
            for k in keys {
                if let Value::Str(s) = k {
                    mm.shift_remove(s);
                }
            }
            Ok(Value::Obj(m))
        }
        other => Err(err(format!("Cannot delete fields from {}", other.kind()))),
    }
}
