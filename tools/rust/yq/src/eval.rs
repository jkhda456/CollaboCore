//! yq's evaluation: Context, the data tree navigator and the core operators (traversal,
//! collection, assignment, variables, pipes and unions, cross functions).

use crate::node::*;
use crate::parser::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type Res<T> = Result<T, String>;
pub type List = Rc<Vec<NodeId>>;
type VarMap = HashMap<String, List>;

/// yq's Context. `nodes` keeps Go's list identity (union compares it); `vars` is shared between
/// copies of the same context as a Go map is, and copied by child_context.
#[derive(Clone)]
pub struct Ctx {
    pub nodes: List,
    pub vars: Option<Rc<RefCell<VarMap>>>,
    pub dont_auto_create: bool,
    pub dt_layout: Option<Rc<str>>,
}

impl Ctx {
    pub fn new(nodes: Vec<NodeId>) -> Ctx {
        Ctx { nodes: Rc::new(nodes), vars: None, dont_auto_create: false, dt_layout: None }
    }

    pub fn child_list(&self, results: List) -> Ctx {
        let mut map = VarMap::new();
        if let Some(v) = &self.vars {
            for (k, l) in v.borrow().iter() {
                map.insert(k.clone(), Rc::new((**l).clone()));
            }
        }
        Ctx { nodes: results, vars: Some(Rc::new(RefCell::new(map))), dont_auto_create: self.dont_auto_create, dt_layout: self.dt_layout.clone() }
    }

    pub fn child(&self, results: Vec<NodeId>) -> Ctx {
        self.child_list(Rc::new(results))
    }

    pub fn single_child(&self, n: NodeId) -> Ctx {
        self.child(vec![n])
    }

    pub fn single_readonly_child(&self, n: NodeId) -> Ctx {
        let mut c = self.child(vec![n]);
        c.dont_auto_create = true;
        c
    }

    pub fn clone_ctx(&self) -> Ctx {
        self.child_list(self.nodes.clone())
    }

    pub fn readonly_clone(&self) -> Ctx {
        let mut c = self.clone_ctx();
        c.dont_auto_create = true;
        c
    }

    pub fn writable_clone(&self) -> Ctx {
        let mut c = self.clone_ctx();
        c.dont_auto_create = false;
        c
    }

    pub fn get_variable(&self, name: &str) -> Option<List> {
        self.vars.as_ref().and_then(|v| v.borrow().get(name).cloned())
    }

    pub fn set_variable(&mut self, name: &str, value: List) {
        match &self.vars {
            Some(v) => {
                v.borrow_mut().insert(name.to_string(), value);
            }
            None => {
                let mut m = VarMap::new();
                m.insert(name.to_string(), value);
                self.vars = Some(Rc::new(RefCell::new(m)));
            }
        }
    }

    pub fn layout(&self) -> &str {
        match &self.dt_layout {
            Some(l) if !l.is_empty() => l,
            _ => crate::gotime::RFC3339,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

fn evaluate_all_together(e: &Engine, ctx: &Ctx) -> bool {
    ctx.nodes.iter().all(|n| e.n(*n).evaluate_together)
}

pub fn is_truthy(e: &Engine, n: Option<NodeId>) -> bool {
    let Some(n) = n else { return false };
    let node = e.n(n);
    if node.tag == "!!null" {
        return false;
    }
    if node.kind == SCALAR && node.tag == "!!bool" {
        let v = node.value.to_ascii_lowercase();
        return v == "y" || v == "yes" || v == "on" || v == "true";
    }
    true
}

pub fn create_boolean(e: &mut Engine, owner: Option<NodeId>, value: bool) -> NodeId {
    let owner = match owner {
        Some(o) => o,
        None => e.add(CNode::default()),
    };
    let noob = e.create_replacement(owner, SCALAR, "!!bool", if value { "true" } else { "false" });
    if e.n(owner).is_map_key {
        e.nm(noob).is_map_key = false;
        e.nm(noob).key = Some(owner);
    }
    noob
}

/// createTraversalTree
pub fn create_traversal_tree(e: &mut Engine, path: &[PathElem], prefs: TraversePrefs, target_key: bool) -> Expr {
    let _ = e;
    if path.is_empty() {
        return expr(Operation::new(Op::SelfReference), None, None);
    } else if path.len() == 1 {
        let mut last = prefs;
        if target_key {
            last.include_map_keys = true;
            last.dont_include_map_values = true;
        }
        let value = match &path[0] {
            PathElem::Str(s) => OpValue::Str(s.clone()),
            PathElem::Int(i) => OpValue::Int(*i),
        };
        return expr(
            Operation { value, string_value: path[0].to_go_string(), prefs: Prefs::Traverse(last), ..Operation::new(Op::TraversePath) },
            None,
            None,
        );
    }
    let lhs = create_traversal_tree(e, &path[0..1], prefs, false);
    let rhs = create_traversal_tree(e, &path[1..], prefs, target_key);
    expr(Operation::new(Op::ShortPipe), Some(lhs), Some(rhs))
}

pub fn self_expr() -> Expr {
    expr(Operation::new(Op::SelfReference), None, None)
}

pub fn reference_expr(n: NodeId) -> Expr {
    expr(Operation { node: Some(n), ..Operation::new(Op::Reference) }, None, None)
}

impl Engine {
    pub fn get_matching_nodes(&mut self, ctx: &Ctx, x: Option<&Expr>) -> Res<Ctx> {
        let Some(x) = x else { return Ok(ctx.clone()) };
        crate::ops::dispatch(self, ctx.clone(), x)
    }

    pub fn eval(&mut self, ctx: &Ctx, x: &Option<Expr>) -> Res<Ctx> {
        self.get_matching_nodes(ctx, x.as_ref())
    }

    /// DeeplyAssign (unused by the CLI, kept for completeness).
    pub fn first_value(&self, ctx: &Ctx) -> Option<NodeId> {
        ctx.nodes.first().copied()
    }
}

// ---- cross functions ----

pub type LhsResult<'a> = Option<&'a dyn Fn(&mut Engine, Option<NodeId>) -> Res<Option<NodeId>>>;

fn results_for_rhs<F>(e: &mut Engine, ctx: &Ctx, lhs: Option<NodeId>, calc_when_empty: bool, lhs_result: LhsResult, calc: &mut F, rhs_exp: &Option<Expr>, results: &mut Vec<NodeId>) -> Res<()>
where
    F: FnMut(&mut Engine, &Ctx, Option<NodeId>, Option<NodeId>) -> Res<Option<NodeId>>,
{
    if let Some(f) = lhs_result {
        if let Some(r) = f(e, lhs)? {
            results.push(r);
            return Ok(());
        }
    }
    let rhs = e.eval(ctx, rhs_exp)?;
    if calc_when_empty && rhs.nodes.is_empty() {
        if let Some(r) = calc(e, ctx, lhs, None)? {
            results.push(r);
        }
        return Ok(());
    }
    for r in rhs.nodes.iter() {
        if let Some(res) = calc(e, ctx, lhs, Some(*r))? {
            results.push(res);
        }
    }
    Ok(())
}

fn do_cross_func<F>(e: &mut Engine, ctx: &Ctx, x: &Expr, calc_when_empty: bool, lhs_result: LhsResult, calc: &mut F) -> Res<Ctx>
where
    F: FnMut(&mut Engine, &Ctx, Option<NodeId>, Option<NodeId>) -> Res<Option<NodeId>>,
{
    let mut results = Vec::new();
    let lhs = e.eval(ctx, &x.lhs)?;
    if calc_when_empty && !ctx.nodes.is_empty() && lhs.nodes.is_empty() {
        results_for_rhs(e, ctx, None, calc_when_empty, lhs_result, calc, &x.rhs, &mut results)?;
    }
    for l in lhs.nodes.iter() {
        results_for_rhs(e, ctx, Some(*l), calc_when_empty, lhs_result, calc, &x.rhs, &mut results)?;
    }
    Ok(ctx.child(results))
}

/// crossFunctionWithPrefs
pub fn cross_with<F>(e: &mut Engine, ctx: Ctx, x: &Expr, calc_when_empty: bool, lhs_result: LhsResult, calc: &mut F) -> Res<Ctx>
where
    F: FnMut(&mut Engine, &Ctx, Option<NodeId>, Option<NodeId>) -> Res<Option<NodeId>>,
{
    if evaluate_all_together(e, &ctx) {
        return do_cross_func(e, &ctx, x, calc_when_empty, lhs_result, calc);
    }
    let mut results = Vec::new();
    for n in ctx.nodes.iter() {
        let inner = do_cross_func(e, &ctx.single_child(*n), x, calc_when_empty, lhs_result, calc)?;
        results.extend(inner.nodes.iter());
    }
    Ok(ctx.child(results))
}

/// crossFunction
pub fn cross_function<F>(e: &mut Engine, ctx: Ctx, x: &Expr, calc: &mut F, calc_when_empty: bool) -> Res<Ctx>
where
    F: FnMut(&mut Engine, &Ctx, Option<NodeId>, Option<NodeId>) -> Res<Option<NodeId>>,
{
    cross_with(e, ctx, x, calc_when_empty, None, calc)
}

// ---- traversal ----

/// An ordered map keyed by GetKey (as elliotchance/orderedmap: Set keeps the first position).
pub struct OrderedMatches {
    keys: HashMap<String, usize>,
    values: Vec<NodeId>,
}

impl OrderedMatches {
    pub fn new() -> Self {
        OrderedMatches { keys: HashMap::new(), values: Vec::new() }
    }
    pub fn set(&mut self, k: String, v: NodeId) {
        match self.keys.get(&k) {
            Some(i) => self.values[*i] = v,
            None => {
                self.keys.insert(k, self.values.len());
                self.values.push(v);
            }
        }
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn into_vec(self) -> Vec<NodeId> {
        self.values
    }
}

pub fn splat(e: &mut Engine, ctx: &Ctx, prefs: TraversePrefs) -> Res<Ctx> {
    traverse_nodes_with_array_indices(e, ctx, &[], prefs)
}

pub fn traverse_path_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut matches = Vec::new();
    for n in ctx.nodes.iter() {
        let found = traverse(e, &ctx, *n, &x.op)?;
        matches.extend(found);
    }
    Ok(ctx.child(matches))
}

fn traverse(e: &mut Engine, ctx: &Ctx, node: NodeId, op: &Operation) -> Res<Vec<NodeId>> {
    let node = e.resolve_alias_chain(node)?;
    let is_brackets = matches!(&op.value, OpValue::Str(s) if s == "[]");
    if e.n(node).tag == "!!null" && !is_brackets && !ctx.dont_auto_create {
        match op.value {
            OpValue::Int(_) => e.nm(node).kind = SEQ,
            _ => e.nm(node).kind = MAP,
        }
        e.nm(node).tag = String::new();
    }
    match e.n(node).kind {
        MAP => {
            let key = e.string_scalar(&op.string_value);
            traverse_map(e, ctx, node, key, op.traverse_prefs(), false)
        }
        SEQ => {
            let idx = e.add(CNode { value: op.string_value.clone(), filename: "".into(), ..Default::default() });
            traverse_array_with_indices(e, node, &[idx], op.traverse_prefs())
        }
        _ => Ok(Vec::new()),
    }
}

pub fn traverse_array_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if let Some(rhs) = &x.rhs {
        if let Some(rr) = &rhs.rhs {
            if rr.op.op == Op::CreateMap {
                let lhs_ctx = e.eval(&ctx, &x.lhs)?;
                return crate::ops::slice_array_operator(e, lhs_ctx, rr);
            }
        }
    }
    let lhs = e.eval(&ctx, &x.lhs)?;
    let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
    let prefs = x.op.traverse_prefs();
    let first = *rhs.nodes.first().ok_or("invalid memory address or nil pointer dereference")?;
    let indices = e.n(first).content.clone();
    let result = traverse_nodes_with_array_indices(e, &lhs, &indices, prefs)?;
    Ok(ctx.child_list(result.nodes))
}

pub fn traverse_nodes_with_array_indices(e: &mut Engine, ctx: &Ctx, indices: &[NodeId], prefs: TraversePrefs) -> Res<Ctx> {
    let mut out = Vec::new();
    for n in ctx.nodes.iter() {
        out.extend(traverse_array_indices(e, ctx, *n, indices, prefs)?);
    }
    Ok(ctx.child(out))
}

fn traverse_array_indices(e: &mut Engine, ctx: &Ctx, node: NodeId, indices: &[NodeId], prefs: TraversePrefs) -> Res<Vec<NodeId>> {
    let node = e.resolve_alias_chain(node)?;
    if e.n(node).tag == "!!null" {
        e.nm(node).tag = String::new();
        e.nm(node).kind = SEQ;
        if !indices.is_empty() && e.n(indices[0]).tag != "!!int" {
            e.nm(node).kind = MAP;
        }
    }
    match e.n(node).kind {
        SEQ => traverse_array_with_indices(e, node, indices, prefs),
        MAP => traverse_map_with_indices(e, ctx, node, indices, prefs),
        _ => Ok(Vec::new()),
    }
}

fn traverse_map_with_indices(e: &mut Engine, ctx: &Ctx, node: NodeId, indices: &[NodeId], prefs: TraversePrefs) -> Res<Vec<NodeId>> {
    if indices.is_empty() {
        let k = e.string_scalar("");
        return traverse_map(e, ctx, node, k, prefs, true);
    }
    let mut out = Vec::new();
    for idx in indices {
        out.extend(traverse_map(e, ctx, node, *idx, prefs, false)?);
    }
    Ok(out)
}

pub fn traverse_array_with_indices(e: &mut Engine, node: NodeId, indices: &[NodeId], prefs: TraversePrefs) -> Res<Vec<NodeId>> {
    let mut out = Vec::new();
    if indices.is_empty() {
        return Ok(e.n(node).content.clone());
    }
    for idx in indices {
        let v = e.n(*idx).value.clone();
        let index = match crate::util::parse_int(&v) {
            Ok(i) => i,
            Err(err) => {
                if prefs.optional_traverse {
                    continue;
                }
                return Err(format!("cannot index array with '{v}' ({err})"));
            }
        };
        let mut index_to_use = index;
        let mut len = e.n(node).content.len() as i64;
        while len <= index {
            if len == 0 {
                e.nm(node).style = 0;
            }
            let v = e.scalar("!!null", "null");
            e.add_child(node, v);
            len = e.n(node).content.len() as i64;
        }
        if index_to_use < 0 {
            index_to_use += len;
        }
        if index_to_use < 0 {
            return Err(format!("index [{index}] out of range, array size is {len}"));
        }
        out.push(e.n(node).content[index_to_use as usize]);
    }
    Ok(out)
}

fn key_matches(e: &Engine, key: NodeId, wanted: &str, exact: bool) -> bool {
    if exact {
        return e.n(key).value == wanted;
    }
    crate::util::match_key(&e.n(key).value, wanted)
}

pub fn traverse_map(e: &mut Engine, ctx: &Ctx, node: NodeId, key: NodeId, prefs: TraversePrefs, splat: bool) -> Res<Vec<NodeId>> {
    let mut matches = OrderedMatches::new();
    let wanted = e.n(key).value.clone();
    do_traverse_map(e, &mut matches, node, &wanted, prefs, splat)?;
    if !splat && !prefs.dont_auto_create && !ctx.dont_auto_create && matches.len() == 0 {
        let value = e.create_child(node);
        e.nm(value).kind = SCALAR;
        e.nm(value).tag = "!!null".into();
        e.nm(value).value = "null".into();
        if e.n(node).content.is_empty() {
            e.nm(node).style = 0;
        }
        let (k, v) = e.add_key_value_child(node, key, value);
        if prefs.include_map_keys {
            let gk = e.get_key(k);
            matches.set(gk, k);
        }
        if !prefs.dont_include_map_values {
            let gk = e.get_key(v);
            matches.set(gk, v);
        }
    }
    Ok(matches.into_vec())
}

fn do_traverse_map(e: &mut Engine, matches: &mut OrderedMatches, node: NodeId, wanted: &str, prefs: TraversePrefs, splat: bool) -> Res<()> {
    let contents = e.n(node).content.clone();
    if !prefs.dont_follow_alias && e.prefs.yaml.fix_merge_anchor_to_spec {
        let mut index = contents.len() as i64 - 2;
        while index >= 0 {
            let k = contents[index as usize];
            let v = contents[index as usize + 1];
            if e.n(k).tag == "!!merge" {
                traverse_merge_anchor(e, matches, v, wanted, prefs, splat)?;
            }
            index -= 2;
        }
    }
    let mut index = 0;
    while index + 1 < contents.len() {
        let key = contents[index];
        let value = contents[index + 1];
        if e.n(key).tag == "!!merge" && !prefs.dont_follow_alias && wanted != e.n(key).value {
            if !e.prefs.yaml.fix_merge_anchor_to_spec {
                if e.show_merge_warning {
                    crate::util::warn("--yaml-fix-merge-anchor-to-spec is false; causing merge anchors to override the existing values which isn't to the yaml spec. This flag will default to true in late 2025. See https://mikefarah.gitbook.io/yq/operators/traverse-read for more details.");
                    e.show_merge_warning = false;
                }
                traverse_merge_anchor(e, matches, value, wanted, prefs, splat)?;
            }
        } else if splat || key_matches(e, key, wanted, prefs.exact_key_match) {
            if prefs.include_map_keys {
                let k = e.get_key(key);
                matches.set(k, key);
            }
            if !prefs.dont_include_map_values {
                let k = e.get_key(value);
                matches.set(k, value);
            }
        }
        index += 2;
    }
    Ok(())
}

fn traverse_merge_anchor(e: &mut Engine, matches: &mut OrderedMatches, merge: NodeId, wanted: &str, prefs: TraversePrefs, splat: bool) -> Res<()> {
    let mut merge = merge;
    if e.n(merge).kind == ALIAS {
        if let Some(a) = e.n(merge).alias {
            merge = a;
        }
    }
    match e.n(merge).kind {
        MAP => do_traverse_map(e, matches, merge, wanted, prefs, splat),
        SEQ => {
            let mut content = e.n(merge).content.clone();
            if e.prefs.yaml.fix_merge_anchor_to_spec {
                content.reverse();
            }
            for child in content {
                let mut c = child;
                if e.n(c).kind == ALIAS {
                    if let Some(a) = e.n(c).alias {
                        c = a;
                    }
                }
                if e.n(c).kind != MAP {
                    crate::util::warn(&format!(
                        "can only use merge anchors with maps (!!map) or sequences (!!seq) of maps, but got sequence containing {}",
                        e.n(c).tag
                    ));
                    return Ok(());
                }
                do_traverse_map(e, matches, c, wanted, prefs, splat)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---- recursive descent ----

pub fn recursive_descent(e: &mut Engine, results: &mut Vec<NodeId>, ctx: &Ctx, traverse: TraversePrefs, recurse_array: bool) -> Res<()> {
    let nodes = ctx.nodes.clone();
    for c in nodes.iter() {
        results.push(*c);
        let n = e.n(*c);
        if n.kind != ALIAS && !n.content.is_empty() && (recurse_array || n.kind != SEQ) {
            let children = splat(e, &ctx.single_child(*c), traverse)?;
            recursive_descent(e, results, &children, traverse, recurse_array)?;
        }
    }
    Ok(())
}

// ---- collect ----

pub fn collect_together(e: &mut Engine, ctx: &Ctx, x: &Option<Expr>) -> Res<NodeId> {
    let collected = e.add(CNode::new(SEQ, "!!seq"));
    for c in ctx.nodes.clone().iter() {
        let r = e.eval(&ctx.single_readonly_child(*c), x)?;
        for n in r.nodes.iter() {
            e.add_child(collected, *n);
        }
    }
    Ok(collected)
}

pub fn collect_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if ctx.nodes.is_empty() {
        let node = e.add(CNode { kind: SEQ, tag: "!!seq".into(), value: "[]".into(), filename: "".into(), ..Default::default() });
        return Ok(ctx.single_child(node));
    }
    if evaluate_all_together(e, &ctx) {
        let collected = collect_together(e, &ctx, &x.rhs)?;
        return Ok(ctx.single_child(collected));
    }
    let mut results = Vec::new();
    for c in ctx.nodes.clone().iter() {
        let cc = e.create_replacement(*c, SEQ, "!!seq", "");
        let r = e.eval(&ctx.single_child(*c), &x.rhs)?;
        for n in r.nodes.iter() {
            e.add_child(cc, *n);
        }
        results.push(cc);
    }
    Ok(ctx.child(results))
}

// ---- assignment ----

pub fn get_assign_prefs(p: &Prefs) -> AssignPrefs {
    match p {
        Prefs::Assign(a) => *a,
        Prefs::Multiply(m) => m.assign,
        _ => AssignPrefs::default(),
    }
}

pub fn assign_update_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    let prefs = get_assign_prefs(&x.op.prefs);
    if !x.op.update_assign {
        let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
            let l = l.unwrap();
            let r = r.unwrap();
            if !prefs.only_write_null || e.n(l).tag == "!!null" {
                e.update_from(l, r, prefs);
            }
            Ok(Some(l))
        };
        cross_function(e, ctx.readonly_clone(), x, &mut calc, false)?;
        return Ok(ctx);
    }
    for c in lhs.nodes.iter().rev() {
        let rhs = e.eval(&ctx.single_child(*c), &x.rhs)?;
        if let Some(first) = rhs.nodes.first() {
            e.update_from(*c, *first, prefs);
        }
    }
    Ok(ctx)
}

pub fn assign_attributes_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    for c in lhs.nodes.iter() {
        let rhs = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
        if let Some(first) = rhs.nodes.first() {
            let prefs = match &x.op.prefs {
                Prefs::Assign(a) => *a,
                _ => AssignPrefs::default(),
            };
            if !prefs.only_write_null || e.n(*c).tag == "!!null" {
                e.update_attributes_from(*c, *first, prefs);
            }
        }
    }
    Ok(ctx)
}

pub type Calc = fn(Expr, Option<Expr>, &Prefs) -> Expr;

pub fn compound_assign(e: &mut Engine, ctx: Ctx, x: &Expr, calc: Calc) -> Res<Ctx> {
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    let mut prefs = AssignPrefs::default();
    match &x.op.prefs {
        Prefs::Assign(a) => prefs = *a,
        Prefs::Multiply(m) => prefs.clobber_custom_tags = m.assign.clobber_custom_tags,
        _ => {}
    }
    let assign_op = Operation { prefs: Prefs::Assign(prefs), ..Operation::new(Op::Assign) };
    for c in lhs.nodes.iter() {
        let clone = e.copy(*c);
        let value_copy = reference_expr(clone);
        let value_exp = reference_expr(*c);
        let node = expr(assign_op.clone(), Some(value_exp), Some(calc(value_copy, x.rhs.clone(), &x.op.prefs)));
        e.get_matching_nodes(&ctx, Some(&node))?;
    }
    Ok(ctx)
}

// ---- pipes, unions, variables ----

pub fn pipe_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if x.lhs.as_ref().map(|l| l.op.op) == Some(Op::AssignVariable) {
        return variable_loop(e, ctx, x);
    }
    let lhs = e.eval(&ctx, &x.lhs)?;
    let rhs_ctx = ctx.child_list(lhs.nodes.clone());
    let rhs = e.eval(&rhs_ctx, &x.rhs)?;
    Ok(ctx.child_list(rhs.nodes))
}

pub fn union_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs = e.eval(&ctx, &x.lhs)?;
    let rhs = e.eval(&ctx, &x.rhs)?;
    let mut out: Vec<NodeId> = lhs.nodes.iter().copied().collect();
    if !Rc::ptr_eq(&rhs.nodes, &lhs.nodes) {
        out.extend(rhs.nodes.iter());
    }
    Ok(lhs.child(out))
}

pub fn get_variable_operator(_e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let name = &x.op.string_value;
    let result = ctx.get_variable(name).unwrap_or_else(|| Rc::new(Vec::new()));
    Ok(ctx.child_list(result))
}

fn variable_loop(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if evaluate_all_together(e, &ctx) {
        return variable_loop_single(e, ctx, x);
    }
    let mut results = Vec::new();
    for n in ctx.nodes.clone().iter() {
        let r = variable_loop_single(e, ctx.single_child(*n), x)?;
        results.extend(r.nodes.iter());
    }
    Ok(ctx.child(results))
}

fn variable_loop_single(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let var_exp = x.lhs.as_ref().unwrap();
    let lhs = e.eval(&ctx.readonly_clone(), &var_exp.lhs)?;
    let rhs_op = var_exp.rhs.as_ref().map(|r| r.op.op);
    if rhs_op != Some(Op::GetVariable) {
        return Err("RHS of 'as' operator must be a variable name e.g. $foo".into());
    }
    let name = var_exp.rhs.as_ref().unwrap().op.string_value.clone();
    let is_ref = matches!(var_exp.op.prefs, Prefs::AssignVar { is_reference: true });
    let mut results = Vec::new();
    for n in lhs.nodes.iter() {
        let value = if is_ref { *n } else { e.copy(*n) };
        let mut new_ctx = ctx.child_list(ctx.nodes.clone());
        new_ctx.set_variable(&name, Rc::new(vec![value]));
        let rhs = e.eval(&new_ctx, &x.rhs)?;
        results.extend(rhs.nodes.iter());
    }
    if lhs.nodes.is_empty() {
        return e.eval(&ctx, &x.rhs);
    }
    Ok(ctx.child(results))
}

pub fn reduce_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs_op = x.lhs.as_ref().map(|l| l.op.op).unwrap();
    if lhs_op != Op::AssignVariable {
        return Err(format!("reduce must be given a variables assignment, got {} instead", lhs_op.name()));
    }
    let rhs_op = x.rhs.as_ref().map(|l| l.op.op).unwrap();
    if rhs_op != Op::Block {
        return Err(format!("reduce must be given a block, got {} instead", rhs_op.name()));
    }
    if evaluate_all_together(e, &ctx) {
        return reduce_single(e, ctx, x);
    }
    let mut results = Vec::new();
    for n in ctx.nodes.clone().iter() {
        let r = reduce_single(e, ctx.single_child(*n), x)?;
        results.extend(r.nodes.iter());
    }
    Ok(ctx.child(results))
}

fn reduce_single(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs = x.lhs.as_ref().unwrap();
    let array = e.eval(&ctx, &lhs.lhs)?;
    let name = lhs.rhs.as_ref().unwrap().op.string_value.clone();
    let rhs = x.rhs.as_ref().unwrap();
    let mut accum = e.eval(&ctx, &rhs.lhs)?;
    for c in array.nodes.iter() {
        accum.set_variable(&name, Rc::new(vec![*c]));
        accum = e.eval(&accum, &rhs.rhs)?;
    }
    Ok(accum)
}

pub fn value_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let node = x.op.node.unwrap();
    if ctx.nodes.is_empty() {
        let c = e.copy(node);
        return Ok(ctx.single_child(c));
    }
    let mut results = Vec::with_capacity(ctx.nodes.len());
    for _ in 0..ctx.nodes.len() {
        results.push(e.copy(node));
    }
    Ok(ctx.child(results))
}
