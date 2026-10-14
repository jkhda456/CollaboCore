//! The operator dispatcher and most of yq's operators (the arithmetic, comparison, sorting,
//! string and date ones are in ops2.rs).

use crate::eval::*;
use crate::node::*;
use crate::ops2;
use crate::parser::*;
use std::rc::Rc;

pub fn dispatch(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    use Op::*;
    match x.op.op {
        Or => ops2::or_operator(e, ctx, x),
        And => ops2::and_operator(e, ctx, x),
        Reduce => reduce_operator(e, ctx, x),
        IfThen => if_then_else(e, ctx, x.lhs.clone(), x.rhs.clone(), None),
        IfElse => {
            let then = x.lhs.clone().unwrap();
            if_then_else(e, ctx, then.lhs.clone(), then.rhs.clone(), x.rhs.clone())
        }
        Block | Empty => {
            let mut c = ctx;
            c.nodes = Rc::new(Vec::new());
            Ok(c)
        }
        Union => union_operator(e, ctx, x),
        Pipe | ShortPipe => pipe_operator(e, ctx, x),
        Assign => assign_update_operator(e, ctx, x),
        AddAssign => compound_assign(e, ctx, x, |l, r, _| expr(Operation::new(Op::Add), Some(l), r)),
        SubtractAssign => compound_assign(e, ctx, x, |l, r, _| expr(Operation::new(Op::Subtract), Some(l), r)),
        MultiplyAssign => compound_assign(e, ctx, x, |l, r, p| expr(Operation::with_prefs(Op::Multiply, p.clone()), Some(l), r)),
        AssignAttributes => assign_attributes_operator(e, ctx, x),
        AssignStyle => assign_style_operator(e, ctx, x),
        AssignVariable => Err("must use variable with a pipe, e.g. `exp as $x | ...`".into()),
        AssignTag => assign_string_attr(e, ctx, x, |n, v| n.tag = v),
        AssignComment => assign_comments_operator(e, ctx, x),
        AssignAnchor => assign_string_attr(e, ctx, x, |n, v| n.anchor = v),
        AssignAlias => assign_alias_operator(e, ctx, x),
        Multiply => ops2::multiply_operator(e, ctx, x),
        Divide => ops2::divide_operator(e, ctx, x),
        Modulo => ops2::modulo_operator(e, ctx, x),
        Add => ops2::add_operator(e, ctx, x),
        Subtract => ops2::subtract_operator(e, ctx, x),
        Alternative => ops2::alternative_operator(e, ctx, x),
        Equals => ops2::equals_operator(e, ctx, x, false),
        NotEquals => ops2::equals_operator(e, ctx.readonly_clone(), x, true),
        Compare => ops2::compare_operator(e, ctx, x),
        Min => ops2::superlative(e, ctx, false),
        Max => ops2::superlative(e, ctx, true),
        CreateMap => create_map_operator(e, ctx, x),
        Length => map_each(e, ctx, |e, c| {
            let n = e.n(c);
            let len = match n.kind {
                SCALAR => {
                    if n.tag == "!!null" {
                        0
                    } else {
                        n.value.len()
                    }
                }
                MAP => n.content.len() / 2,
                SEQ => n.content.len(),
                _ => 0,
            };
            Ok(e.create_replacement(c, SCALAR, "!!int", &len.to_string()))
        }),
        Line => map_each(e, ctx, |e, c| {
            let v = e.n(c).line.to_string();
            Ok(e.create_replacement(c, SCALAR, "!!int", &v))
        }),
        Column => map_each(e, ctx, |e, c| {
            let v = e.n(c).column.to_string();
            Ok(e.create_replacement(c, SCALAR, "!!int", &v))
        }),
        Expression => {
            let Prefs::Expression(src) = &x.op.prefs else { unreachable!() };
            let exp = crate::parser::parse_expression(e, src)?;
            e.eval(&ctx, &exp)
        }
        Collect => collect_operator(e, ctx, x),
        Map => map_operator(e, ctx, x),
        Filter => filter_operator(e, ctx, x),
        Error => {
            let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
            let msg = rhs.nodes.first().map(|n| e.n(*n).value.clone()).unwrap_or_else(|| "aborted".into());
            Err(msg)
        }
        Pick => ops2::pick_operator(e, ctx, x),
        Omit => ops2::omit_operator(e, ctx, x),
        Eval => eval_operator(e, ctx, x),
        MapValues => map_values_operator(e, ctx, x),
        FormatDateTime => ops2::format_date_time(e, ctx, x),
        WithDtFormat => ops2::with_date_time_format(e, ctx, x),
        Now => ops2::now_op(e, ctx),
        Tz => ops2::tz_op(e, ctx, x),
        FromUnix => ops2::from_unix_op(e, ctx),
        ToUnix => ops2::to_unix_op(e, ctx),
        Encode => crate::printer::encode_operator(e, ctx, x),
        Decode => crate::printer::decode_operator(e, ctx, x),
        Any | AnyCondition => ops2::any_all(e, ctx, x, true),
        All | AllCondition => ops2::any_all(e, ctx, x, false),
        Contains => ops2::contains_operator(e, ctx, x),
        ToEntries => to_entries_operator(e, ctx),
        FromEntries => from_entries_operator(e, ctx),
        WithEntries => with_entries_operator(e, ctx, x),
        With => with_operator(e, ctx, x),
        SplitDocument => {
            for (i, c) in ctx.nodes.iter().enumerate() {
                e.nm(*c).document = i as u64;
                e.nm(*c).parent = None;
            }
            Ok(ctx)
        }
        GetVariable => get_variable_operator(e, ctx, x),
        GetStyle => map_each(e, ctx, |e, c| {
            let s = match e.n(c).style {
                TAGGED => "tagged",
                DOUBLE => "double",
                SINGLE => "single",
                LITERAL => "literal",
                FOLDED => "folded",
                FLOW => "flow",
                0 => "",
                _ => "<unknown>",
            };
            Ok(e.create_replacement(c, SCALAR, "!!str", s))
        }),
        GetTag => map_each(e, ctx, |e, c| {
            let t = e.n(c).tag.clone();
            Ok(e.create_replacement(c, SCALAR, "!!str", &t))
        }),
        GetKind => map_each(e, ctx, |e, c| {
            let k = match e.n(c).kind {
                MAP => "map",
                SEQ => "seq",
                SCALAR => "scalar",
                ALIAS => "alias",
                _ => "unknown",
            };
            Ok(e.create_replacement(c, SCALAR, "!!str", k))
        }),
        GetKey => {
            let out: Vec<NodeId> = ctx.nodes.iter().filter_map(|c| e.n(*c).key).collect();
            Ok(ctx.child(out))
        }
        IsKey => map_each(e, ctx, |e, c| {
            let v = e.n(c).is_map_key;
            Ok(create_boolean(e, Some(c), v))
        }),
        GetParent => get_parent_operator(e, ctx, x),
        GetParents => map_each(e, ctx, |e, c| {
            let list = e.add(CNode::new(SEQ, "!!seq"));
            let mut p = e.n(c).parent;
            while let Some(pp) = p {
                e.add_child(list, pp);
                p = e.n(pp).parent;
            }
            Ok(list)
        }),
        GetComment => get_comments_operator(e, ctx, x),
        GetAnchor => map_each(e, ctx, |e, c| {
            let a = e.n(c).anchor.clone();
            Ok(e.create_replacement(c, SCALAR, "!!str", &a))
        }),
        GetAlias => map_each(e, ctx, |e, c| {
            let v = e.n(c).value.clone();
            Ok(e.create_replacement(c, SCALAR, "!!str", &v))
        }),
        GetDocumentIndex => map_each(e, ctx, |e, c| {
            let d = e.get_document(c).to_string();
            Ok(e.create_replacement(c, SCALAR, "!!int", &d))
        }),
        GetFilename => map_each(e, ctx, |e, c| {
            let f = e.get_filename(c).to_string();
            Ok(e.create_replacement(c, SCALAR, "!!str", &f))
        }),
        GetFileIndex => map_each(e, ctx, |e, c| {
            let f = e.get_file_index(c).to_string();
            Ok(e.create_replacement(c, SCALAR, "!!int", &f))
        }),
        GetPath => map_each(e, ctx, |e, c| {
            let node = e.create_replacement(c, SEQ, "!!seq", "");
            let path = e.get_path(c);
            let content: Vec<NodeId> = path.iter().map(|p| create_path_node(e, p)).collect();
            e.add_children(node, &content);
            Ok(node)
        }),
        SetPath => set_path_operator(e, ctx, x),
        DelPaths => del_paths_operator(e, ctx, x),
        Explode => explode_operator(e, ctx, x),
        SortBy => ops2::sort_by_operator(e, ctx, &x.rhs),
        Sort => ops2::sort_by_operator(e, ctx, &Some(self_expr())),
        First => first_operator(e, ctx, x),
        Reverse => map_each(e, ctx, |e, c| {
            if e.n(c).kind != SEQ {
                return Err(format!("node at path [{}] is not an array (it's a {})", e.get_nice_path(c), e.n(c).tag));
            }
            let style = e.n(c).style;
            let r = e.create_replacement_with_comments(c, SEQ, "!!seq", style);
            let mut content = e.n(c).content.clone();
            content.reverse();
            e.add_children(r, &content);
            Ok(r)
        }),
        Shuffle => ops2::shuffle_operator(e, ctx),
        SortKeys => sort_keys_operator(e, ctx, x),
        Join => ops2::join_operator(e, ctx, x),
        SubString => ops2::substitute_operator(e, ctx, x),
        Match => ops2::match_operator(e, ctx, x, ops2::MatchKind::Match),
        Capture => ops2::match_operator(e, ctx, x, ops2::MatchKind::Capture),
        Test => ops2::match_operator(e, ctx, x, ops2::MatchKind::Test),
        SplitString => ops2::split_operator(e, ctx, x),
        ChangeCase => ops2::change_case_operator(e, ctx, x),
        Trim => ops2::trim_operator(e, ctx),
        ToString => ops2::to_string_operator(e, ctx),
        StringInterpolation => ops2::string_interpolation_operator(e, ctx, x),
        Load => crate::printer::load_operator(e, ctx, x),
        LoadString => crate::printer::load_string_operator(e, ctx, x),
        System => crate::printer::system_operator(e, ctx, x),
        Keys => map_each(e, ctx, |e, c| match e.n(c).kind {
            MAP => {
                let keys: Vec<NodeId> = e.n(c).content.iter().step_by(2).copied().collect();
                let seq = e.add(CNode::new(SEQ, "!!seq"));
                e.add_children(seq, &keys);
                Ok(seq)
            }
            SEQ => {
                let len = e.n(c).content.len();
                let idx: Vec<NodeId> = (0..len).map(|i| e.scalar("!!int", &i.to_string())).collect();
                let seq = e.add(CNode::new(SEQ, "!!seq"));
                e.add_children(seq, &idx);
                Ok(seq)
            }
            _ => Err(format!("cannot get keys of {}, keys only works for maps and arrays", e.n(c).tag)),
        }),
        CollectObject => collect_object_operator(e, ctx),
        TraversePath => traverse_path_operator(e, ctx, x),
        TraverseArray => traverse_array_operator(e, ctx, x),
        SelfReference => Ok(ctx),
        Value => value_operator(e, ctx, x),
        Reference => Ok(ctx.single_child(x.op.node.unwrap())),
        Env => crate::printer::env_operator(e, ctx, x),
        Not => map_each(e, ctx, |e, c| {
            let t = is_truthy(e, Some(c));
            Ok(create_boolean(e, Some(c), !t))
        }),
        ToNumber => map_each(e, ctx, |e, c| {
            let n = e.n(c);
            if n.kind != SCALAR {
                return Err(format!("cannot convert node at path {} of tag {} to number", e.get_nice_path(c), n.tag));
            }
            if n.tag == "!!int" || n.tag == "!!float" {
                return Ok(c);
            }
            let v = n.value.clone();
            let tag = if crate::util::parse_int64(&v).is_ok() {
                "!!int"
            } else if crate::gostr::parse_float(&v).is_ok() {
                "!!float"
            } else {
                return Err(format!("cannot convert node value [{}] at path {} of tag {} to number", v, e.get_nice_path(c), e.n(c).tag));
            };
            Ok(e.create_replacement(c, SCALAR, tag, &v))
        }),
        Envsubst => crate::printer::envsubst_operator(e, ctx, x),
        RecursiveDescent => {
            let Prefs::RecursiveDescent { traverse, recurse_array } = x.op.prefs else { unreachable!() };
            let mut results = Vec::new();
            recursive_descent(e, &mut results, &ctx, traverse, recurse_array)?;
            Ok(ctx.child(results))
        }
        Select => select_operator(e, ctx, &x.rhs),
        Has => has_operator(e, ctx, x),
        Unique => ops2::unique_by(e, ctx, &Some(self_expr())),
        UniqueBy => ops2::unique_by(e, ctx, &x.rhs),
        GroupBy => ops2::group_by(e, ctx, &x.rhs),
        Flatten => {
            let Prefs::Flatten(depth) = x.op.prefs else { unreachable!() };
            for c in ctx.nodes.iter() {
                if e.n(*c).kind != SEQ {
                    return Err("only arrays are supported for flatten".into());
                }
                flatten(e, *c, depth);
            }
            Ok(ctx)
        }
        DeleteChild => delete_child_operator(e, ctx, x),
        Pivot => ops2::pivot_operator(e, ctx),
    }
}

/// Applies f to every matching node; the results form the child context.
pub fn map_each<F>(e: &mut Engine, ctx: Ctx, mut f: F) -> Res<Ctx>
where
    F: FnMut(&mut Engine, NodeId) -> Res<NodeId>,
{
    let mut out = Vec::with_capacity(ctx.nodes.len());
    for c in ctx.nodes.iter() {
        out.push(f(e, *c)?);
    }
    Ok(ctx.child(out))
}

pub fn create_path_node(e: &mut Engine, p: &PathElem) -> NodeId {
    match p {
        PathElem::Str(s) => e.scalar("!!str", s),
        PathElem::Int(i) => e.scalar("!!int", &i.to_string()),
    }
}

fn if_then_else(e: &mut Engine, ctx: Ctx, cond: Option<Expr>, then: Option<Expr>, els: Option<Expr>) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let conditions = e.eval(&ctx.single_readonly_child(*c), &cond)?;
        let conds: Vec<Option<NodeId>> =
            if conditions.nodes.is_empty() { vec![None] } else { conditions.nodes.iter().map(|n| Some(*n)).collect() };
        for cn in conds {
            let branch = if is_truthy(e, cn) { &then } else { &els };
            let r = e.eval(&ctx.single_child(*c), branch)?;
            results.extend(r.nodes.iter());
        }
    }
    Ok(ctx.child(results))
}

pub fn select_operator(e: &mut Engine, ctx: Ctx, rhs: &Option<Expr>) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let r = e.eval(&ctx.single_readonly_child(*c), rhs)?;
        let include = r.nodes.iter().any(|n| is_truthy(e, Some(*n)));
        if include {
            results.push(*c);
        }
    }
    Ok(ctx.child(results))
}

fn map_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let splatted = splat(e, &ctx.single_child(*c), TraversePrefs::default())?;
        if splatted.nodes.is_empty() {
            results.push(e.copy(*c));
            continue;
        }
        let r = e.eval(&splatted, &x.rhs)?;
        let collected = collect_together(e, &r, &Some(self_expr()))?;
        e.nm(collected).style = e.n(*c).style;
        results.push(collected);
    }
    Ok(ctx.child(results))
}

fn filter_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let splatted = splat(e, &ctx.single_child(*c), TraversePrefs::default())?;
        let filtered = select_operator(e, splatted, &x.rhs)?;
        let collected = collect_together(e, &filtered, &Some(self_expr()))?;
        e.nm(collected).style = e.n(*c).style;
        results.push(collected);
    }
    Ok(ctx.child(results))
}

fn map_values_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    for c in ctx.nodes.iter() {
        let splatted = splat(e, &ctx.single_child(*c), TraversePrefs::default())?;
        let assign = expr(Operation { update_assign: true, ..Operation::new(Op::Assign) }, None, x.rhs.clone());
        assign_update_operator(e, splatted, &assign)?;
    }
    Ok(ctx)
}

fn eval_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let strs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
    let mut exprs = Vec::new();
    for s in strs.nodes.iter() {
        let src = e.n(*s).value.clone();
        exprs.push(crate::parser::parse_expression(e, &src)?);
    }
    let mut results = Vec::new();
    for _ in ctx.nodes.iter() {
        for ex in &exprs {
            let r = e.eval(&ctx, ex)?;
            results.extend(r.nodes.iter());
        }
    }
    Ok(ctx.child(results))
}

fn with_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let rhs = x.rhs.as_ref().unwrap();
    if rhs.op.op != Op::Block {
        return Err(format!("with must be given a block (;), got {} instead", rhs.op.op.name()));
    }
    let update_ctx = e.eval(&ctx, &rhs.lhs)?;
    for c in update_ctx.nodes.iter() {
        e.eval(&update_ctx.single_child(*c), &rhs.rhs)?;
    }
    Ok(ctx)
}

fn first_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        if x.rhs.is_none() {
            if let Some(f) = e.n(*c).content.first() {
                results.push(*f);
            }
            continue;
        }
        let splatted = splat(e, &ctx.single_child(*c), TraversePrefs::default())?;
        for s in splatted.nodes.iter() {
            let r = e.eval(&ctx.single_child(*s), &x.rhs)?;
            if r.nodes.iter().any(|n| is_truthy(e, Some(*n))) {
                results.push(*s);
                break;
            }
        }
    }
    Ok(ctx.child(results))
}

fn get_parent_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::Parent(level) = x.op.prefs else { unreachable!() };
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let mut levels = level;
        if level < 0 {
            let mut total = 0i64;
            let mut t = e.n(*c).parent;
            while let Some(p) = t {
                total += 1;
                t = e.n(p).parent;
            }
            levels = (total + level + 1).max(0);
        }
        let mut cand = Some(*c);
        let mut cur = 0;
        while cur < levels {
            match cand {
                Some(n) => cand = e.n(n).parent,
                None => break,
            }
            cur += 1;
        }
        if let Some(n) = cand {
            results.push(n);
        }
    }
    Ok(ctx.child(results))
}

fn has_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
    let (wanted_key, wanted_tag) = match rhs.nodes.first() {
        Some(w) => (e.n(*w).value.clone(), e.n(*w).tag.clone()),
        None => ("null".to_string(), "!!null".to_string()),
    };
    map_each(e, ctx, |e, c| {
        let n = e.n(c);
        match n.kind {
            MAP => {
                let has = n.content.iter().step_by(2).any(|k| e.n(*k).value == wanted_key);
                Ok(create_boolean(e, Some(c), has))
            }
            SEQ => {
                let mut has = false;
                if wanted_tag == "!!int" {
                    let num = crate::gostr::parse_int(&wanted_key, 10).map_err(|_| {
                        format!("strconv.ParseInt: parsing {}: invalid syntax", crate::util::go_quote(&wanted_key))
                    })?;
                    has = num >= 0 && (n.content.len() as i64) > num;
                }
                Ok(create_boolean(e, Some(c), has))
            }
            _ => Ok(create_boolean(e, Some(c), false)),
        }
    })
}

fn sort_keys_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    for c in ctx.nodes.iter() {
        let r = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
        for n in r.nodes.iter() {
            if e.n(*n).kind == MAP {
                sort_keys(e, *n);
            }
        }
    }
    Ok(ctx)
}

fn sort_keys(e: &mut Engine, node: NodeId) {
    let content = e.n(node).content.clone();
    let mut keys = Vec::new();
    let mut kb = std::collections::HashMap::new();
    let mut vb = std::collections::HashMap::new();
    let mut i = 0;
    while i + 1 < content.len() {
        let k = e.n(content[i]).value.clone();
        keys.push(k.clone());
        kb.insert(k.clone(), content[i]);
        vb.insert(k, content[i + 1]);
        i += 2;
    }
    keys.sort();
    let mut sorted = Vec::with_capacity(content.len());
    for k in &keys {
        sorted.push(kb[k]);
        sorted.push(vb[k]);
    }
    e.nm(node).content = sorted;
}

fn flatten(e: &mut Engine, node: NodeId, depth: i64) {
    if depth == 0 || e.n(node).kind != SEQ {
        return;
    }
    let content = e.n(node).content.clone();
    let mut new_seq = Vec::new();
    for c in content {
        if e.n(c).kind == SEQ {
            flatten(e, c, depth - 1);
            new_seq.extend(e.n(c).content.iter());
        } else {
            new_seq.push(c);
        }
    }
    e.nm(node).content = Vec::new();
    e.add_children(node, &new_seq);
}

fn assign_style_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    fn parse_style(s: &str) -> Res<u32> {
        Ok(match s {
            "tagged" => TAGGED,
            "double" => DOUBLE,
            "single" => SINGLE,
            "literal" => LITERAL,
            "folded" => FOLDED,
            "flow" => FLOW,
            "" => 0,
            other => return Err(format!("unknown style {other}")),
        })
    }
    let mut style = 0;
    if !x.op.update_assign {
        let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
        if let Some(f) = rhs.nodes.first() {
            style = parse_style(&e.n(*f).value)?;
        }
    }
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    for c in lhs.nodes.iter() {
        if x.op.update_assign {
            let rhs = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
            if let Some(f) = rhs.nodes.first() {
                style = parse_style(&e.n(*f).value)?;
            }
        }
        e.nm(*c).style = style;
    }
    Ok(ctx)
}

/// tag = / anchor =: a string attribute set from the RHS.
fn assign_string_attr(e: &mut Engine, ctx: Ctx, x: &Expr, set: fn(&mut CNode, String)) -> Res<Ctx> {
    let mut value = String::new();
    if !x.op.update_assign {
        let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
        if let Some(f) = rhs.nodes.first() {
            value = e.n(*f).value.clone();
        }
    }
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    for c in lhs.nodes.iter() {
        if x.op.update_assign {
            let rhs = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
            if let Some(f) = rhs.nodes.first() {
                value = e.n(*f).value.clone();
            }
        }
        set(e.nm(*c), value.clone());
    }
    Ok(ctx)
}

fn assign_alias_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut name = String::new();
    if !x.op.update_assign {
        let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
        if let Some(f) = rhs.nodes.first() {
            name = e.n(*f).value.clone();
        }
    }
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    for c in lhs.nodes.iter() {
        if x.op.update_assign {
            let rhs = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
            if let Some(f) = rhs.nodes.first() {
                name = e.n(*f).value.clone();
            }
        }
        if !name.is_empty() {
            e.nm(*c).kind = ALIAS;
            e.nm(*c).value = name.clone();
        }
    }
    Ok(ctx)
}

fn assign_comments_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs = e.eval(&ctx.writable_clone(), &x.lhs)?;
    let Prefs::Comment { line, head, foot } = x.op.prefs else { unreachable!() };
    let mut comment = String::new();
    if !x.op.update_assign {
        let rhs = e.eval(&ctx.readonly_clone(), &x.rhs)?;
        if let Some(f) = rhs.nodes.first() {
            comment = e.n(*f).value.clone();
        }
    }
    for c in lhs.nodes.iter() {
        if x.op.update_assign {
            let rhs = e.eval(&ctx.single_readonly_child(*c), &x.rhs)?;
            if let Some(f) = rhs.nodes.first() {
                comment = e.n(*f).value.clone();
            }
        }
        let n = e.nm(*c);
        if line {
            n.line_comment = comment.clone();
        }
        if head {
            n.head_comment = comment.clone();
            n.leading_content = String::new();
        }
        if foot {
            n.foot_comment = comment.clone();
        }
    }
    Ok(ctx)
}

fn get_comments_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::Comment { line, head, foot } = x.op.prefs else { unreachable!() };
    map_each(e, ctx, |e, c| {
        let n = e.n(c);
        let mut comment = if line {
            n.line_comment.clone()
        } else if head && !n.leading_content.is_empty() {
            let mut out = Vec::new();
            crate::formats::yaml::print_leading_content(&mut out, &n.leading_content, false, false);
            let mut s = String::from_utf8_lossy(&out).into_owned();
            if s.ends_with('\n') {
                s.pop();
            }
            s
        } else if head {
            n.head_comment.clone()
        } else if foot {
            n.foot_comment.clone()
        } else {
            String::new()
        };
        if let Some(rest) = comment.strip_prefix("# ") {
            comment = rest.to_string();
        }
        comment = comment.replace("\n# ", "\n");
        let is_key = e.n(c).is_map_key;
        let r = e.create_replacement(c, SCALAR, "!!str", &comment);
        if is_key {
            e.nm(r).is_map_key = false;
            e.nm(r).key = Some(c);
        }
        Ok(r)
    })
}

fn create_map_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut sequences = Vec::new();
    if !ctx.nodes.is_empty() {
        for m in ctx.nodes.iter() {
            sequences.push(sequence_for(e, &ctx, Some(*m), x)?);
        }
    } else {
        sequences.push(sequence_for(e, &ctx, None, x)?);
    }
    let node = list_to_node_seq(e, &sequences);
    Ok(ctx.single_child(node))
}

fn sequence_for(e: &mut Engine, ctx: &Ctx, m: Option<NodeId>, x: &Expr) -> Res<NodeId> {
    let (document, filename, file_index) = match m {
        Some(m) => (e.get_document(m), e.get_filename(m), e.get_file_index(m)),
        None => (0, Rc::from(""), 0),
    };
    let matches: Vec<NodeId> = m.into_iter().collect();
    let fname = filename.clone();
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
        let node = e.add(CNode::new(MAP, "!!map"));
        e.add_key_value_child(node, l.unwrap(), r.unwrap());
        let n = e.nm(node);
        n.document = document;
        n.file_index = file_index;
        n.filename = fname.clone();
        Ok(Some(node))
    };
    let pairs = cross_function(e, ctx.child(matches), x, &mut calc, false)?;
    let inner = list_to_node_seq(e, &pairs.nodes);
    let n = e.nm(inner);
    n.style = FLOW;
    n.document = document;
    n.file_index = file_index;
    n.filename = filename;
    Ok(inner)
}

fn list_to_node_seq(e: &mut Engine, list: &[NodeId]) -> NodeId {
    let node = e.add(CNode::new(SEQ, "!!seq"));
    for c in list {
        e.add_child(node, *c);
    }
    node
}

fn collect_object_operator(e: &mut Engine, original: Ctx) -> Res<Ctx> {
    let ctx = original.writable_clone();
    if ctx.nodes.is_empty() {
        let c = e.add(CNode { kind: MAP, tag: "!!map".into(), value: "{}".into(), filename: "".into(), ..Default::default() });
        return Ok(ctx.single_child(c));
    }
    let first = ctx.nodes[0];
    let first_len = e.n(first).content.len();
    let mut rotated: Vec<Vec<NodeId>> = vec![Vec::new(); first_len];
    for c in ctx.nodes.iter() {
        let content = &e.n(*c).content;
        if content.len() < first_len {
            return Err("CollectObject: mismatching node sizes; are you creating a map with mismatching key value pairs?".into());
        }
        for i in 0..first_len {
            rotated[i].push(content[i]);
        }
    }
    let mut new_object = Vec::new();
    for r in rotated {
        let additions = collect(e, ctx.child(Vec::new()), r)?;
        for a in additions.nodes.iter() {
            let c = e.copy(*a);
            e.nm(c).parent = None;
            e.nm(c).key = None;
            new_object.push(c);
        }
    }
    Ok(ctx.child(new_object))
}

fn collect(e: &mut Engine, ctx: Ctx, mut remaining: Vec<NodeId>) -> Res<Ctx> {
    if remaining.is_empty() {
        return Ok(ctx);
    }
    let candidate = remaining.remove(0);
    let splatted = splat(e, &ctx.single_child(candidate), TraversePrefs { dont_follow_alias: true, ..Default::default() })?;
    if ctx.nodes.is_empty() {
        return collect(e, splatted, remaining);
    }
    let mut new_agg = Vec::new();
    for a in ctx.nodes.iter() {
        for s in splatted.nodes.iter() {
            let nc = e.copy(*a);
            let merged = ops2::multiply(e, &ctx, nc, *s, MultiplyPrefs::default())?;
            new_agg.push(merged);
        }
    }
    collect(e, ctx.child(new_agg), remaining)
}

// ---- entries ----

fn entry_seq_for(e: &mut Engine, key: NodeId, value: NodeId) -> NodeId {
    let kk = e.scalar("!!str", "key");
    let vk = e.scalar("!!str", "value");
    let c = e.add(CNode::new(MAP, "!!map"));
    e.add_key_value_child(c, kk, key);
    e.add_key_value_child(c, vk, value);
    c
}

fn to_entries_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        match e.n(*c).kind {
            MAP => {
                let seq = e.create_replacement_with_comments(*c, SEQ, "!!seq", 0);
                let content = e.n(*c).content.clone();
                let mut i = 0;
                while i + 1 < content.len() {
                    let entry = entry_seq_for(e, content[i], content[i + 1]);
                    e.add_child(seq, entry);
                    i += 2;
                }
                results.push(seq);
            }
            SEQ => {
                let seq = e.create_replacement_with_comments(*c, SEQ, "!!seq", 0);
                let content = e.n(*c).content.clone();
                for (i, v) in content.iter().enumerate() {
                    let k = e.scalar("!!int", &i.to_string());
                    let entry = entry_seq_for(e, k, *v);
                    e.add_child(seq, entry);
                }
                results.push(seq);
            }
            _ => {
                if e.n(*c).tag != "!!null" {
                    return Err(format!("{} has no keys", e.n(*c).tag));
                }
            }
        }
    }
    Ok(ctx.child(results))
}

fn parse_entry(e: &mut Engine, entry: NodeId, position: usize) -> Res<(NodeId, NodeId)> {
    let prefs = TraversePrefs { dont_auto_create: true, ..Default::default() };
    let empty = Ctx::new(Vec::new());
    let k = e.string_scalar("key");
    let keys = traverse_map(e, &empty, entry, k, prefs, false)?;
    if keys.len() != 1 {
        return Err(format!("expected to find one 'key' entry but found {} in position {}", keys.len(), position));
    }
    let v = e.string_scalar("value");
    let values = traverse_map(e, &empty, entry, v, prefs, false)?;
    if values.len() != 1 {
        return Err(format!("expected to find one 'value' entry but found {} in position {}", values.len(), position));
    }
    Ok((keys[0], values[0]))
}

fn from_entries(e: &mut Engine, c: NodeId) -> Res<NodeId> {
    let node = e.copy_without_content(c);
    let content = e.n(c).content.clone();
    for (i, entry) in content.iter().enumerate() {
        let (k, v) = parse_entry(e, *entry, i)?;
        e.add_key_value_child(node, k, v);
    }
    e.nm(node).kind = MAP;
    e.nm(node).tag = "!!map".into();
    Ok(node)
}

fn from_entries_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        if e.n(*c).kind == SEQ {
            results.push(from_entries(e, *c)?);
        } else {
            return Err("from entries only runs against arrays".into());
        }
    }
    Ok(ctx.child(results))
}

fn with_entries_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let to = to_entries_operator(e, ctx.clone())?;
    let mut results = Vec::new();
    for c in to.nodes.iter() {
        let splatted = splat(e, &ctx.single_child(*c), TraversePrefs::default())?;
        let mut new_results = Vec::new();
        for item in splatted.nodes.iter() {
            let r = e.eval(&splatted.single_child(*item), &x.rhs)?;
            new_results.extend(r.nodes.iter());
        }
        let collected = collect_together(e, &splatted.child(new_results), &Some(self_expr()))?;
        let (lc, hc, fc) = (e.n(*c).leading_content.clone(), e.n(*c).head_comment.clone(), e.n(*c).foot_comment.clone());
        let n = e.nm(collected);
        n.leading_content = lc;
        n.head_comment = hc;
        n.foot_comment = fc;
        let fe = from_entries_operator(e, ctx.single_child(collected))?;
        results.extend(fe.nodes.iter());
    }
    Ok(ctx.child(results))
}

// ---- paths and deletion ----

fn get_path_array(e: &Engine, func: &str, node: NodeId) -> Res<Vec<PathElem>> {
    let n = e.n(node);
    if n.kind != SEQ {
        return Err(format!("{func}: expected path array, but got {} instead", n.tag));
    }
    let mut path = Vec::new();
    for c in &n.content {
        let cn = e.n(*c);
        match cn.tag.as_str() {
            "!!str" => path.push(PathElem::Str(cn.value.clone())),
            "!!int" => {
                let num = crate::util::parse_int(&cn.value).map_err(|err| format!("{func}: could not parse {} as an int: {err}", cn.value))?;
                path.push(PathElem::Int(num));
            }
            other => return Err(format!("{func}: expected either a !!str or !!int in the path, found {other} instead")),
        }
    }
    Ok(path)
}

fn set_path_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let rhs = x.rhs.as_ref().unwrap();
    if rhs.op.op != Op::Block {
        return Err(format!("SETPATH must be given a block (;), got {} instead", rhs.op.op.name()));
    }
    let lhs_path = e.eval(&ctx.readonly_clone(), &rhs.lhs)?;
    if lhs_path.nodes.len() != 1 {
        return Err(format!("SETPATH: expected single path but found {} results instead", lhs_path.nodes.len()));
    }
    let path = get_path_array(e, "SETPATH", lhs_path.nodes[0])?;
    let tree = create_traversal_tree(e, &path, TraversePrefs::default(), false);
    for c in ctx.nodes.iter() {
        let target = e.eval(&ctx.single_readonly_child(*c), &rhs.rhs)?;
        if target.nodes.len() != 1 {
            return Err(format!("SETPATH: expected single value on RHS but found {}", target.nodes.len()));
        }
        let node = expr(Operation::new(Op::Assign), Some(tree.clone()), Some(reference_expr(target.nodes[0])));
        e.get_matching_nodes(&ctx.single_child(*c), Some(&node))?;
    }
    Ok(ctx)
}

fn del_paths_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let arrays = e.eval(&ctx.readonly_clone(), &x.rhs)?;
    if arrays.nodes.len() != 1 {
        return Err(format!("DELPATHS: expected single value but found {}", arrays.nodes.len()));
    }
    let node = arrays.nodes[0];
    if e.n(node).tag != "!!seq" {
        return Err(format!("DELPATHS: expected a sequence of sequences, but found {}", e.n(node).tag));
    }
    let mut updated = ctx;
    let content = e.n(node).content.clone();
    for (i, child) in content.iter().enumerate() {
        if e.n(*child).tag != "!!seq" {
            return Err(format!(
                "DELPATHS: expected entry [{i}] to be a sequence, but its a {}. Note that delpaths takes an array of path arrays, e.g. [[\"a\", \"b\"]]",
                e.n(*child).tag
            ));
        }
        let path = get_path_array(e, "DELPATHS", *child)?;
        let tree = create_traversal_tree(e, &path, TraversePrefs::default(), false);
        let del = expr(Operation::new(Op::DeleteChild), None, Some(tree));
        updated = e.get_matching_nodes(&updated, Some(&del))?;
    }
    Ok(updated)
}

fn delete_child_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let to_delete = e.eval(&ctx.readonly_clone(), &x.rhs)?;
    for c in to_delete.nodes.iter().rev() {
        let Some(parent) = e.n(*c).parent else {
            let out: Vec<NodeId> = ctx.nodes.iter().copied().filter(|n| n != c).collect();
            return Ok(ctx.child(out));
        };
        let path = e.get_path(*c);
        let child_path = path.last().cloned();
        match e.n(parent).kind {
            MAP => delete_from_map(e, parent, child_path),
            SEQ => delete_from_array(e, parent, child_path),
            _ => return Err(format!("cannot delete nodes from parent of tag {}", e.n(parent).tag)),
        }
    }
    Ok(ctx)
}

fn delete_from_map(e: &mut Engine, node: NodeId, child_path: Option<PathElem>) {
    let contents = e.n(node).content.clone();
    let mut new_contents = Vec::new();
    let mut i = 0;
    while i + 1 < contents.len() {
        let should_delete = matches!(&child_path, Some(PathElem::Str(s)) if *s == e.n(contents[i]).value);
        if !should_delete {
            new_contents.push(contents[i]);
            new_contents.push(contents[i + 1]);
        }
        i += 2;
    }
    e.nm(node).content = new_contents;
    normalise_empty_collection_key_comment(e, node);
}

fn delete_from_array(e: &mut Engine, node: NodeId, child_path: Option<PathElem>) {
    let contents = e.n(node).content.clone();
    let target = child_path.map(|p| p.to_go_string()).unwrap_or_else(|| "<nil>".into());
    let mut new_contents = Vec::new();
    for (i, v) in contents.iter().enumerate() {
        if i.to_string() != target {
            if let Some(k) = e.n(*v).key {
                e.nm(k).value = new_contents.len().to_string();
            }
            new_contents.push(*v);
        }
    }
    e.nm(node).content = new_contents;
    normalise_empty_collection_key_comment(e, node);
}

fn normalise_empty_collection_key_comment(e: &mut Engine, node: NodeId) {
    let n = e.n(node);
    if (n.kind != SEQ && n.kind != MAP) || !n.content.is_empty() || !n.line_comment.is_empty() {
        return;
    }
    let mut key = n.key;
    if let Some(p) = n.parent {
        if e.n(p).kind == MAP {
            let pc = &e.n(p).content;
            let mut i = 0;
            while i + 1 < pc.len() {
                if pc[i + 1] == node {
                    key = Some(pc[i]);
                    break;
                }
                i += 2;
            }
        }
    }
    let Some(k) = key else { return };
    if e.n(k).line_comment.is_empty() {
        return;
    }
    let lc = std::mem::take(&mut e.nm(k).line_comment);
    e.nm(node).line_comment = lc;
    e.nm(node).style = FLOW;
}

// ---- anchors and aliases ----

fn explode_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    for c in ctx.nodes.iter() {
        let r = e.eval(&ctx.single_child(*c), &x.rhs)?;
        for n in r.nodes.iter() {
            explode_node(e, *n)?;
        }
    }
    Ok(ctx)
}

pub fn explode_node(e: &mut Engine, node: NodeId) -> Res<()> {
    e.nm(node).anchor = String::new();
    match e.n(node).kind {
        SEQ => {
            for c in e.n(node).content.clone() {
                explode_node(e, c)?;
            }
            Ok(())
        }
        ALIAS => {
            if let Some(a) = e.n(node).alias {
                let (kind, style, tag, value, content) =
                    (e.n(a).kind, e.n(a).style, e.n(a).tag.clone(), e.n(a).value.clone(), e.n(a).content.clone());
                e.nm(node).kind = kind;
                e.nm(node).style = style;
                e.nm(node).tag = tag;
                e.add_children(node, &content);
                e.nm(node).value = value;
                e.nm(node).alias = None;
            }
            Ok(())
        }
        MAP => {
            let content = e.n(node).content.clone();
            let has_alias = content.iter().step_by(2).any(|k| e.n(*k).value == "<<");
            if has_alias {
                if e.prefs.yaml.fix_merge_anchor_to_spec {
                    return fixed_reconstruct_aliased_map(e, node);
                }
                if e.show_merge_warning {
                    crate::util::warn("--yaml-fix-merge-anchor-to-spec is false; causing merge anchors to override the existing values which isn't to the yaml spec. This flag will default to true in late 2025.");
                    e.show_merge_warning = false;
                }
                return reconstruct_aliased_map(e, node);
            }
            let mut i = 0;
            while i + 1 < content.len() {
                explode_node(e, content[i])?;
                explode_node(e, content[i + 1])?;
                i += 2;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn content_value_by_key(e: &Engine, content: &[NodeId], key: &str) -> Option<NodeId> {
    let mut i = 0;
    while i + 1 < content.len() {
        if e.n(content[i]).value == key {
            return Some(content[i + 1]);
        }
        i += 2;
    }
    None
}

fn fixed_reconstruct_aliased_map(e: &mut Engine, node: NodeId) -> Res<()> {
    let mut new_content: Vec<NodeId> = Vec::new();
    let content = e.n(node).content.clone();
    let mut i = 0;
    while i + 1 < content.len() {
        let k = content[i];
        let v = content[i + 1];
        if e.n(k).tag != "!!merge" {
            explode_node(e, k)?;
            explode_node(e, v)?;
            new_content.push(k);
            new_content.push(v);
        } else {
            let mut seq = v;
            if e.n(seq).kind == ALIAS {
                if let Some(a) = e.n(seq).alias {
                    seq = a;
                }
            }
            let items: Vec<NodeId> = if e.n(seq).kind != SEQ { vec![seq] } else { e.n(seq).content.clone() };
            for m in items {
                let mut m = m;
                if e.n(m).kind == ALIAS {
                    if let Some(a) = e.n(m).alias {
                        m = a;
                    }
                }
                let m = e.copy(m);
                explode_node(e, m)?;
                if e.n(m).kind != MAP {
                    return Err(format!(
                        "can only use merge anchors with maps (!!map) or sequences (!!seq) of maps, but got sequence containing {}",
                        e.n(m).tag
                    ));
                }
                let mc = e.n(m).content.clone();
                let node_content = e.n(node).content.clone();
                let mut j = 0;
                while j + 1 < mc.len() {
                    let kv = e.n(mc[j]).value.clone();
                    if content_value_by_key(e, &node_content, &kv).is_none() && content_value_by_key(e, &new_content, &kv).is_none() {
                        for item in [mc[j], mc[j + 1]] {
                            let copied = e.copy(item);
                            e.nm(copied).parent = Some(node);
                            new_content.push(copied);
                        }
                    }
                    j += 2;
                }
            }
        }
        i += 2;
    }
    e.nm(node).content = new_content;
    Ok(())
}

fn reconstruct_aliased_map(e: &mut Engine, node: NodeId) -> Res<()> {
    let mut new_content: Vec<NodeId> = Vec::new();
    let content = e.n(node).content.clone();
    let mut i = 0;
    while i + 1 < content.len() {
        let k = content[i];
        let v = content[i + 1];
        if e.n(k).value != "<<" {
            override_entry(e, node, k, v, i, &mut new_content)?;
        } else if e.n(v).kind == SEQ {
            let vc = e.n(v).content.clone();
            for j in (0..vc.len()).rev() {
                let alias = e.n(vc[j]).alias;
                apply_alias(e, node, alias, j, &mut new_content)?;
            }
        } else {
            let alias = e.n(v).alias;
            apply_alias(e, node, alias, i, &mut new_content)?;
        }
        i += 2;
    }
    e.nm(node).content = Vec::new();
    let mut j = 0;
    while j + 1 < new_content.len() {
        e.add_key_value_child(node, new_content[j], new_content[j + 1]);
        j += 2;
    }
    Ok(())
}

fn apply_alias(e: &mut Engine, node: NodeId, alias: Option<NodeId>, alias_index: usize, new_content: &mut Vec<NodeId>) -> Res<()> {
    let Some(alias) = alias else { return Ok(()) };
    if e.n(alias).kind != MAP {
        return Err(format!(
            "can only use merge anchors with maps (!!map) or sequences (!!seq) of maps, but got sequence containing {}",
            e.n(alias).tag
        ));
    }
    let ac = e.n(alias).content.clone();
    let mut i = 0;
    while i + 1 < ac.len() {
        override_entry(e, node, ac[i], ac[i + 1], alias_index, new_content)?;
        i += 2;
    }
    Ok(())
}

fn override_entry(e: &mut Engine, node: NodeId, key: NodeId, value: NodeId, start: usize, new_content: &mut Vec<NodeId>) -> Res<()> {
    explode_node(e, value)?;
    let mut i = 0;
    while i < new_content.len() {
        let k = new_content[i];
        if e.n(k).value == e.n(key).value && e.n(k).alias.is_none() && e.n(key).alias.is_none() {
            if i + 1 < new_content.len() {
                new_content[i + 1] = value;
            }
            return Ok(());
        }
        i += 2;
    }
    let content = e.n(node).content.clone();
    let mut idx = start + 2;
    while idx < content.len() {
        let k = content[idx];
        if e.n(k).value == e.n(key).value && e.n(k).alias.is_none() {
            return Ok(());
        }
        idx += 2;
    }
    explode_node(e, key)?;
    new_content.push(key);
    new_content.push(value);
    Ok(())
}

pub fn slice_array_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    fn number(e: &mut Engine, ctx: &Ctx, node: NodeId, ex: &Option<Expr>) -> Res<i64> {
        let r = e.eval(&ctx.single_child(node), ex)?;
        if r.nodes.len() != 1 {
            return Err(format!("expected to find 1 number, got {} instead", r.nodes.len()));
        }
        crate::util::parse_int(&e.n(r.nodes[0]).value)
    }
    fn clamp(i: i64, len: i64) -> i64 {
        let mut i = i;
        if i < 0 {
            i += len;
        }
        i.clamp(0, len)
    }
    let mut results = Vec::new();
    for l in ctx.nodes.iter() {
        let first = number(e, &ctx, *l, &x.lhs)?;
        let second = number(e, &ctx, *l, &x.rhs)?;
        if e.n(*l).kind == SCALAR && e.guess_tag_from_custom_type(*l) == "!!str" {
            let runes: Vec<char> = e.n(*l).value.chars().collect();
            let len = runes.len() as i64;
            let a = clamp(first, len);
            let mut b = clamp(second, len);
            if b < a {
                b = a;
            }
            let s: String = runes[a as usize..b as usize].iter().collect();
            let tag = e.n(*l).tag.clone();
            let r = e.create_replacement(*l, SCALAR, &tag, &s);
            e.nm(r).style = e.n(*l).style;
            results.push(r);
            continue;
        }
        let len = e.n(*l).content.len() as i64;
        let a = clamp(first, len);
        let b = clamp(second, len);
        let items: Vec<NodeId> = if a < b { e.n(*l).content[a as usize..b as usize].to_vec() } else { Vec::new() };
        let tag = e.n(*l).tag.clone();
        let r = e.create_replacement(*l, SEQ, &tag, "");
        e.add_children(r, &items);
        results.push(r);
    }
    Ok(ctx.child(results))
}
