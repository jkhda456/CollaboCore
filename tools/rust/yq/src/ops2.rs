//! yq's arithmetic, comparison, boolean, sorting, string and date-time operators.

use crate::eval::*;
use crate::gotime;
use crate::node::*;
use crate::ops::map_each;
use crate::parser::*;
use crate::util::{checked_arith, parse_float, parse_int64};

// ---- booleans ----

fn owner_of(e: &mut Engine, lhs: Option<NodeId>, rhs: Option<NodeId>) -> NodeId {
    match (lhs, rhs) {
        (Some(l), _) => l,
        (None, Some(r)) => r,
        (None, None) => e.add(CNode::default()),
    }
}

pub fn or_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    bool_op(e, ctx, x, true)
}

pub fn and_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    bool_op(e, ctx, x, false)
}

fn bool_op(e: &mut Engine, ctx: Ctx, x: &Expr, target: bool) -> Res<Ctx> {
    let lhs_result = move |e: &mut Engine, lhs: Option<NodeId>| -> Res<Option<NodeId>> {
        if is_truthy(e, lhs) != target {
            return Ok(None);
        }
        let owner = match lhs {
            Some(l) => l,
            None => e.add(CNode::default()),
        };
        Ok(Some(create_boolean(e, Some(owner), target)))
    };
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
        let owner = owner_of(e, l, r);
        let rb = is_truthy(e, r);
        Ok(Some(create_boolean(e, Some(owner), rb)))
    };
    cross_with(e, ctx.readonly_clone(), x, true, Some(&lhs_result), &mut calc)
}

pub fn any_all(e: &mut Engine, ctx: Ctx, x: &Expr, any: bool) -> Res<Ctx> {
    map_each(e, ctx.clone(), |e, c| {
        if e.n(c).kind != SEQ {
            return Err(format!("{} only supports arrays, was {}", if any { "any" } else { "all" }, e.n(c).tag));
        }
        let want = any;
        let mut found = false;
        for n in e.n(c).content.clone() {
            let mut node = n;
            if x.rhs.is_some() {
                let r = e.eval(&ctx.single_readonly_child(n), &x.rhs)?;
                match r.nodes.first() {
                    Some(f) => node = *f,
                    None => continue,
                }
            }
            if is_truthy(e, Some(node)) == want {
                found = true;
                break;
            }
        }
        let v = if any { found } else { !found };
        Ok(create_boolean(e, Some(c), v))
    })
}

pub fn alternative_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let lhs_result = |e: &mut Engine, lhs: Option<NodeId>| -> Res<Option<NodeId>> {
        match lhs {
            Some(l) if is_truthy(e, Some(l)) => Ok(Some(l)),
            _ => Ok(None),
        }
    };
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
        match (l, r) {
            (None, r) => Ok(r),
            (l, None) => Ok(l),
            (Some(l), Some(r)) => Ok(Some(if is_truthy(e, Some(l)) { l } else { r })),
        }
    };
    cross_with(e, ctx, x, true, Some(&lhs_result), &mut calc)
}

// ---- equality and comparison ----

pub fn equals_operator(e: &mut Engine, ctx: Ctx, x: &Expr, flip: bool) -> Res<Ctx> {
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
        match (l, r) {
            (None, None) => {
                let owner = e.add(CNode::default());
                Ok(Some(create_boolean(e, Some(owner), !flip)))
            }
            (None, Some(r)) => {
                let v = (e.n(r).tag == "!!null") != flip;
                Ok(Some(create_boolean(e, Some(r), v)))
            }
            (Some(l), None) => {
                let v = (e.n(l).tag == "!!null") != flip;
                Ok(Some(create_boolean(e, Some(l), v)))
            }
            (Some(l), Some(r)) => {
                let (ln, rn) = (e.n(l), e.n(r));
                let mut v = false;
                if ln.tag == "!!null" {
                    v = rn.tag == "!!null";
                } else if ln.kind == SCALAR && rn.kind == SCALAR {
                    v = crate::util::match_key(&ln.value, &rn.value);
                }
                Ok(Some(create_boolean(e, Some(l), v != flip)))
            }
        }
    };
    cross_function(e, ctx, x, &mut calc, true)
}

pub fn parse_date_time(layout: &str, s: &str) -> Result<gotime::Time, String> {
    match gotime::parse(layout, s) {
        Ok(t) => Ok(t),
        Err(err) => {
            if layout == gotime::RFC3339 {
                return gotime::parse("2006-01-02", s).map_err(|e| e.to_string());
            }
            Err(err.to_string())
        }
    }
}

fn compare_scalars(e: &mut Engine, ctx: &Ctx, or_equal: bool, greater: bool, l: NodeId, r: NodeId) -> Res<bool> {
    let lt = e.guess_tag_from_custom_type(l);
    let rt = e.guess_tag_from_custom_type(r);
    let mut is_dt = e.n(l).tag == "!!timestamp";
    if lt == "!!str" {
        is_dt = parse_date_time(ctx.layout(), &e.n(l).value).is_ok();
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if is_dt {
        let a = parse_date_time(ctx.layout(), &lv)?;
        let b = parse_date_time(ctx.layout(), &rv)?;
        let (a, b) = ((a.sec, a.nsec), (b.sec, b.nsec));
        if or_equal && a == b {
            return Ok(true);
        }
        return Ok(if greater { a > b } else { a < b });
    } else if lt == "!!int" && rt == "!!int" {
        let (_, a) = parse_int64(&lv)?;
        let (_, b) = parse_int64(&rv)?;
        if or_equal && a == b {
            return Ok(true);
        }
        return Ok(if greater { a > b } else { a < b });
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        if or_equal && a == b {
            return Ok(true);
        }
        return Ok(if greater { a > b } else { a < b });
    } else if lt == "!!str" && rt == "!!str" {
        if or_equal && lv == rv {
            return Ok(true);
        }
        return Ok(if greater { lv > rv } else { lv < rv });
    } else if lt == "!!null" && rt == "!!null" && or_equal {
        return Ok(true);
    } else if lt == "!!null" || rt == "!!null" {
        return Ok(false);
    }
    Err(format!("{} not yet supported for comparison", e.n(l).tag))
}

fn compare_fn(e: &mut Engine, ctx: &Ctx, or_equal: bool, greater: bool, l: Option<NodeId>, r: Option<NodeId>) -> Res<Option<NodeId>> {
    match (l, r) {
        (None, None) => {
            let owner = e.add(CNode::default());
            Ok(Some(create_boolean(e, Some(owner), or_equal)))
        }
        (None, Some(r)) => Ok(Some(create_boolean(e, Some(r), false))),
        (Some(l), None) => Ok(Some(create_boolean(e, Some(l), false))),
        (Some(l), Some(r)) => match e.n(l).kind {
            MAP => Err("maps not yet supported for comparison".into()),
            SEQ => Err("arrays not yet supported for comparison".into()),
            _ => {
                if e.n(r).kind != SCALAR {
                    return Err(format!("{} ({}) cannot be subtracted from {}", e.n(r).tag, e.get_nice_path(r), e.n(l).tag));
                }
                let target = e.copy_without_content(l);
                let v = compare_scalars(e, ctx, or_equal, greater, l, r);
                match v {
                    Ok(v) => Ok(Some(create_boolean(e, Some(target), v))),
                    Err(err) => Err(err),
                }
            }
        },
    }
}

pub fn compare_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::Compare { or_equal, greater } = x.op.prefs else { unreachable!() };
    let mut calc = |e: &mut Engine, c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| compare_fn(e, c, or_equal, greater, l, r);
    cross_function(e, ctx, x, &mut calc, true)
}

pub fn superlative(e: &mut Engine, ctx: Ctx, greater: bool) -> Res<Ctx> {
    let mut results = Vec::new();
    for seq in ctx.nodes.iter() {
        let splatted = splat(e, &ctx.single_child(*seq), TraversePrefs::default())?;
        if let Some(first) = splatted.nodes.first() {
            let mut result = *first;
            for el in splatted.nodes.iter().skip(1) {
                let cmp = compare_fn(e, &ctx, false, greater, Some(*el), Some(result))?;
                if is_truthy(e, cmp) {
                    result = *el;
                }
            }
            results.push(result);
        }
    }
    Ok(ctx.child(results))
}

// ---- arithmetic ----

fn fmt_float(f: f64) -> String {
    crate::gostr::fmt_v(f)
}

pub fn add_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let calc_when_empty = !ctx.nodes.is_empty();
    let mut calc = |e: &mut Engine, c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| add(e, c, l, r);
    cross_function(e, ctx.readonly_clone(), x, &mut calc, calc_when_empty)
}

fn to_nodes(e: &mut Engine, candidate: NodeId, lhs: NodeId) -> Vec<NodeId> {
    if e.n(candidate).tag == "!!null" {
        return Vec::new();
    }
    let clone = e.copy(candidate);
    if e.n(candidate).kind == SEQ {
        return e.n(clone).content.clone();
    }
    if let Some(f) = e.n(lhs).content.first() {
        e.nm(clone).style = e.n(*f).style;
    }
    vec![clone]
}

pub fn add(e: &mut Engine, ctx: &Ctx, lhs: Option<NodeId>, rhs: Option<NodeId>) -> Res<Option<NodeId>> {
    let (l, r) = match (lhs, rhs) {
        (None, None) => return Ok(None),
        (None, Some(r)) => return Ok(Some(e.copy(r))),
        (Some(l), None) => return Ok(Some(e.copy(l))),
        (Some(l), Some(r)) => (l, r),
    };
    if e.n(l).tag == "!!null" {
        return Ok(Some(e.copy_as_replacement(l, r)));
    }
    let target = e.copy_without_content(l);
    match e.n(l).kind {
        MAP => {
            if e.n(r).kind != MAP {
                return Err(format!("{} ({}) cannot be added to a {} ({})", e.n(r).tag, e.get_nice_path(r), e.n(l).tag, e.get_nice_path(l)));
            }
            add_maps(e, target, l, r);
        }
        SEQ => {
            e.nm(target).kind = SEQ;
            if e.n(l).content.is_empty() {
                e.nm(target).style = 0;
            }
            e.nm(target).tag = e.n(l).tag.clone();
            let extra = to_nodes(e, r, l);
            let lc = e.n(l).content.clone();
            e.add_children(target, &lc);
            e.add_children(target, &extra);
        }
        SCALAR => {
            if e.n(r).kind != SCALAR {
                return Err(format!("{} ({}) cannot be added to a {} ({})", e.n(r).tag, e.get_nice_path(r), e.n(l).tag, e.get_nice_path(l)));
            }
            e.nm(target).kind = SCALAR;
            e.nm(target).style = e.n(l).style;
            add_scalars(e, ctx, target, l, r)?;
        }
        _ => {}
    }
    Ok(Some(target))
}

fn add_scalars(e: &mut Engine, ctx: &Ctx, target: NodeId, l: NodeId, r: NodeId) -> Res<()> {
    let mut lt = e.n(l).tag.clone();
    let rt = e.guess_tag_from_custom_type(r);
    let mut l_custom = false;
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
        l_custom = true;
    }
    let mut is_dt = e.n(l).tag == "!!timestamp";
    if lt == "!!str" && ctx.layout() != gotime::RFC3339 {
        is_dt = parse_date_time(ctx.layout(), &e.n(l).value).is_ok();
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if is_dt {
        let d = crate::util::parse_duration(&rv).map_err(|err| format!("unable to parse duration [{rv}]: {err}"))?;
        let t = parse_date_time(ctx.layout(), &lv)?;
        let nt = gotime::Time::unix(t.sec, t.nsec + d, t.loc.clone());
        e.nm(target).value = nt.format(ctx.layout());
    } else if lt == "!!str" {
        e.nm(target).tag = e.n(l).tag.clone();
        e.nm(target).value = if rt == "!!null" { lv } else { lv + &rv };
    } else if rt == "!!str" {
        e.nm(target).tag = e.n(r).tag.clone();
        e.nm(target).value = lv + &rv;
    } else if lt == "!!int" && rt == "!!int" {
        let (f, a) = parse_int64(&lv)?;
        let (_, b) = parse_int64(&rv)?;
        let sum = checked_arith('+', a, b)?;
        e.nm(target).tag = e.n(l).tag.clone();
        e.nm(target).value = f.format(sum);
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        e.nm(target).tag = if l_custom { e.n(l).tag.clone() } else { "!!float".into() };
        e.nm(target).value = fmt_float(a + b);
    } else {
        return Err(format!("{lt} cannot be added to {rt}"));
    }
    Ok(())
}

fn find_key_in_map(e: &mut Engine, map: NodeId, item: NodeId) -> Option<usize> {
    let content = e.n(map).content.clone();
    let mut i = 0;
    while i < content.len() {
        if recursive_node_equal(e, content[i], item) {
            return Some(i);
        }
        i += 2;
    }
    None
}

fn find_in_array(e: &mut Engine, array: NodeId, item: NodeId) -> Option<usize> {
    let content = e.n(array).content.clone();
    (0..content.len()).find(|&i| recursive_node_equal(e, content[i], item))
}

pub fn recursive_node_equal(e: &mut Engine, l: NodeId, r: NodeId) -> bool {
    if e.n(l).kind != e.n(r).kind {
        return false;
    }
    if e.n(l).kind == SCALAR {
        let lt = e.guess_tag_from_custom_type(l);
        let rt = e.guess_tag_from_custom_type(r);
        if lt != rt {
            return false;
        }
    }
    if e.n(l).tag == "!!null" {
        return true;
    }
    match e.n(l).kind {
        SCALAR => e.n(l).value == e.n(r).value,
        SEQ => {
            let (lc, rc) = (e.n(l).content.clone(), e.n(r).content.clone());
            lc.len() == rc.len() && lc.iter().zip(rc.iter()).all(|(a, b)| recursive_node_equal(e, *a, *b))
        }
        MAP => {
            let (lc, rc) = (e.n(l).content.clone(), e.n(r).content.clone());
            if lc.len() != rc.len() {
                return false;
            }
            let mut i = 0;
            while i + 1 < lc.len() {
                match find_key_in_map(e, r, lc[i]) {
                    Some(j) => {
                        if !recursive_node_equal(e, lc[i + 1], rc[j + 1]) {
                            return false;
                        }
                    }
                    None => return false,
                }
                i += 2;
            }
            true
        }
        _ => false,
    }
}

fn add_maps(e: &mut Engine, target: NodeId, l: NodeId, r: NodeId) {
    if e.n(l).content.is_empty() {
        e.nm(target).style = 0;
    }
    e.nm(target).content = Vec::new();
    let lc = e.n(l).content.clone();
    e.add_children(target, &lc);
    let rc = e.n(r).content.clone();
    let mut i = 0;
    while i + 1 < rc.len() {
        let (k, v) = (rc[i], rc[i + 1]);
        match find_key_in_map(e, target, k) {
            None => {
                e.add_key_value_child(target, k, v);
            }
            Some(idx) => {
                let old = e.n(target).content[idx + 1];
                let nv = e.copy_as_replacement(old, v);
                e.nm(target).content[idx + 1] = nv;
            }
        }
        i += 2;
    }
    e.nm(target).kind = MAP;
    if !e.n(l).content.is_empty() {
        e.nm(target).style = e.n(l).style;
    }
    e.nm(target).tag = e.n(l).tag.clone();
}

pub fn subtract_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut calc = |e: &mut Engine, c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| subtract(e, c, l.unwrap(), r.unwrap()).map(Some);
    cross_function(e, ctx.readonly_clone(), x, &mut calc, false)
}

fn subtract(e: &mut Engine, ctx: &Ctx, l: NodeId, r: NodeId) -> Res<NodeId> {
    if e.n(l).tag == "!!null" {
        return Ok(e.copy_as_replacement(l, r));
    }
    let target = e.copy_without_content(l);
    match e.n(l).kind {
        MAP => return Err("maps not yet supported for subtraction".into()),
        SEQ => {
            if e.n(r).kind != SEQ {
                return Err(format!("{} ({}) cannot be subtracted from {}", e.n(r).tag, e.get_nice_path(r), e.n(l).tag));
            }
            let (lc, rc) = (e.n(l).content.clone(), e.n(r).content.clone());
            let mut keep = Vec::new();
            for a in lc {
                let mut include = true;
                for b in &rc {
                    if recursive_node_equal(e, a, *b) {
                        include = false;
                        break;
                    }
                }
                if include {
                    keep.push(a);
                }
            }
            e.nm(target).content = keep;
        }
        SCALAR => {
            if e.n(r).kind != SCALAR {
                return Err(format!("{} ({}) cannot be subtracted from {}", e.n(r).tag, e.get_nice_path(r), e.n(l).tag));
            }
            e.nm(target).kind = SCALAR;
            e.nm(target).style = e.n(l).style;
            subtract_scalars(e, ctx, target, l, r)?;
        }
        _ => {}
    }
    Ok(target)
}

fn subtract_scalars(e: &mut Engine, ctx: &Ctx, target: NodeId, l: NodeId, r: NodeId) -> Res<()> {
    let mut lt = e.n(l).tag.clone();
    let mut rt = e.n(r).tag.clone();
    let mut l_custom = false;
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
        l_custom = true;
    }
    if !rt.starts_with("!!") {
        rt = e.guess_tag_from_custom_type(r);
    }
    let mut is_dt = lt == "!!timestamp";
    if lt == "!!str" && ctx.layout() != gotime::RFC3339 {
        is_dt = parse_date_time(ctx.layout(), &e.n(l).value).is_ok();
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if is_dt {
        let ds = if let Some(s) = rv.strip_prefix('-') { s.to_string() } else { format!("-{rv}") };
        let d = crate::util::parse_duration(&ds).map_err(|err| format!("unable to parse duration [{rv}]: {err}"))?;
        let t = parse_date_time(ctx.layout(), &lv)?;
        let nt = gotime::Time::unix(t.sec, t.nsec + d, t.loc.clone());
        e.nm(target).value = nt.format(ctx.layout());
    } else if lt == "!!str" {
        return Err("strings cannot be subtracted".into());
    } else if lt == "!!int" && rt == "!!int" {
        let (f, a) = parse_int64(&lv)?;
        let (_, b) = parse_int64(&rv)?;
        let res = checked_arith('-', a, b)?;
        e.nm(target).tag = e.n(l).tag.clone();
        e.nm(target).value = f.format(res);
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        e.nm(target).tag = if l_custom { e.n(l).tag.clone() } else { "!!float".into() };
        e.nm(target).value = fmt_float(a - b);
    } else {
        return Err(format!("{} cannot be added to {}", e.n(l).tag, e.n(r).tag));
    }
    Ok(())
}

pub fn multiply_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let prefs = match x.op.prefs {
        Prefs::Multiply(m) => m,
        _ => MultiplyPrefs::default(),
    };
    let mut calc = |e: &mut Engine, c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| multiply(e, c, l.unwrap(), r.unwrap(), prefs).map(Some);
    cross_function(e, ctx.readonly_clone(), x, &mut calc, false)
}

pub fn multiply(e: &mut Engine, ctx: &Ctx, l: NodeId, r: NodeId, prefs: MultiplyPrefs) -> Res<NodeId> {
    let (mut leading, mut head, mut foot) = (e.n(r).leading_content.clone(), e.n(r).head_comment.clone(), e.n(r).foot_comment.clone());
    if !e.n(l).head_comment.is_empty() || !e.n(l).leading_content.is_empty() {
        head = e.n(l).head_comment.clone();
        leading = e.n(l).leading_content.clone();
    }
    if !e.n(l).foot_comment.is_empty() {
        foot = e.n(l).foot_comment.clone();
    }
    if e.n(r).tag == "!!null" {
        return Ok(e.copy(l));
    }
    let (lk, rk, ltag) = (e.n(l).kind, e.n(r).kind, e.n(l).tag.clone());
    if (lk == MAP && rk == MAP) || (ltag == "!!null" && rk == MAP) || (lk == SEQ && rk == SEQ) || (ltag == "!!null" && rk == SEQ) {
        let blank = e.copy(l);
        let n = e.nm(blank);
        n.leading_content = leading;
        n.head_comment = head;
        n.foot_comment = foot;
        return merge_objects(e, &ctx.writable_clone(), blank, r, prefs);
    }
    multiply_scalars(e, l, r)
}

fn multiply_scalars(e: &mut Engine, l: NodeId, r: NodeId) -> Res<NodeId> {
    let mut lt = e.n(l).tag.clone();
    let rt = e.guess_tag_from_custom_type(r);
    let mut l_custom = false;
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
        l_custom = true;
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if lt == "!!int" && rt == "!!int" {
        let target = e.copy_without_content(l);
        e.nm(target).kind = SCALAR;
        e.nm(target).style = e.n(l).style;
        e.nm(target).tag = e.n(l).tag.clone();
        let (f, a) = parse_int64(&lv)?;
        let (_, b) = parse_int64(&rv)?;
        let p = checked_arith('*', a, b)?;
        e.nm(target).value = f.format(p);
        Ok(target)
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        let target = e.copy_without_content(l);
        e.nm(target).kind = SCALAR;
        e.nm(target).style = e.n(l).style;
        e.nm(target).tag = if l_custom { e.n(l).tag.clone() } else { "!!float".into() };
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        e.nm(target).value = fmt_float(a * b);
        Ok(target)
    } else if (lt == "!!str" && rt == "!!int") || (lt == "!!int" && rt == "!!str") {
        let (s, i) = if e.n(l).tag == "!!str" { (l, r) } else { (r, l) };
        let target = e.copy_without_content(l);
        e.update_attributes_from(target, s, AssignPrefs::default());
        let count = crate::util::parse_int(&e.n(i).value)?;
        if count < 0 {
            return Err(format!("cannot repeat string by a negative number ({count})"));
        }
        let max = 10 * 1024 * 1024i64;
        let slen = e.n(s).value.len() as i64;
        if count > 0 && slen > max / count {
            return Err(format!("result of repeating string ({slen} bytes) by {count} would exceed {max} bytes"));
        }
        e.nm(target).value = e.n(s).value.repeat(count as usize);
        Ok(target)
    } else {
        Err(format!("cannot multiply {} with {}", e.n(l).tag, e.n(r).tag))
    }
}

fn merge_objects(e: &mut Engine, ctx: &Ctx, l: NodeId, r: NodeId, prefs: MultiplyPrefs) -> Res<NodeId> {
    let mut results = Vec::new();
    let tp = TraversePrefs { dont_follow_alias: true, include_map_keys: true, exact_key_match: true, ..Default::default() };
    recursive_descent(e, &mut results, &ctx.single_child(r), tp, prefs.deep_merge_arrays)?;
    let start = results.first().map(|f| e.get_path(*f).len()).unwrap_or(0);
    for c in results {
        apply_assignment(e, ctx, start, l, c, prefs)?;
    }
    Ok(l)
}

fn apply_assignment(e: &mut Engine, ctx: &Ctx, start: usize, l: NodeId, r: NodeId, prefs: MultiplyPrefs) -> Res<()> {
    let path = e.get_path(r);
    let lhs_path = &path[start.min(path.len())..];
    let rk = e.n(r).kind;
    let mut op = Operation { prefs: Prefs::Assign(prefs.assign), ..Operation::new(Op::AssignAttributes) };
    if prefs.append_arrays && rk == SEQ {
        op.op = Op::AddAssign;
    } else if !prefs.deep_merge_arrays && rk == SEQ || (rk == SCALAR || rk == ALIAS) {
        op.op = Op::Assign;
        op.update_assign = false;
    }
    let is_key = e.n(r).is_map_key;
    let lhs = create_traversal_tree(e, lhs_path, prefs.traverse, is_key);
    let node = expr(op, Some(lhs), Some(reference_expr(r)));
    e.get_matching_nodes(&ctx.single_child(l), Some(&node))?;
    Ok(())
}

pub fn divide_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| divide(e, l.unwrap(), r.unwrap()).map(Some);
    cross_function(e, ctx.readonly_clone(), x, &mut calc, false)
}

fn divide(e: &mut Engine, l: NodeId, r: NodeId) -> Res<NodeId> {
    if e.n(l).tag == "!!null" || !(e.n(l).kind == SCALAR && e.n(r).kind == SCALAR) {
        return Err(format!("{} ({}) cannot be divided by {} ({})", e.n(l).tag, e.get_nice_path(l), e.n(r).tag, e.get_nice_path(r)));
    }
    let target = e.copy_without_content(l);
    let mut lt = e.n(l).tag.clone();
    let rt = e.guess_tag_from_custom_type(r);
    let mut l_custom = false;
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
        l_custom = true;
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if lt == "!!str" && rt == "!!str" {
        let parts = split_string(e, &lv, &rv);
        e.nm(target).kind = SEQ;
        e.nm(target).tag = "!!seq".into();
        e.add_children(target, &parts);
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        e.nm(target).kind = SCALAR;
        e.nm(target).style = e.n(l).style;
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        e.nm(target).tag = if l_custom { e.n(l).tag.clone() } else { "!!float".into() };
        e.nm(target).value = fmt_float(a / b);
    } else {
        return Err(format!("{lt} cannot be divided by {rt}"));
    }
    Ok(target)
}

pub fn modulo_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| modulo(e, l.unwrap(), r.unwrap()).map(Some);
    cross_function(e, ctx.readonly_clone(), x, &mut calc, false)
}

fn modulo(e: &mut Engine, l: NodeId, r: NodeId) -> Res<NodeId> {
    if e.n(l).tag == "!!null" || !(e.n(l).kind == SCALAR && e.n(r).kind == SCALAR) {
        return Err(format!("{} ({}) cannot modulo by {} ({})", e.n(l).tag, e.get_nice_path(l), e.n(r).tag, e.get_nice_path(r)));
    }
    let target = e.copy_without_content(l);
    let mut lt = e.n(l).tag.clone();
    let rt = e.guess_tag_from_custom_type(r);
    let mut l_custom = false;
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
        l_custom = true;
    }
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if lt == "!!int" && rt == "!!int" {
        e.nm(target).kind = SCALAR;
        e.nm(target).style = e.n(l).style;
        let (f, a) = parse_int64(&lv)?;
        let (_, b) = parse_int64(&rv)?;
        if b == 0 {
            return Err("cannot modulo by 0".into());
        }
        e.nm(target).tag = e.n(l).tag.clone();
        e.nm(target).value = f.format(a.wrapping_rem(b));
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        e.nm(target).kind = SCALAR;
        e.nm(target).style = e.n(l).style;
        let a = parse_float(&lv)?;
        let b = parse_float(&rv)?;
        e.nm(target).tag = if l_custom { e.n(l).tag.clone() } else { "!!float".into() };
        e.nm(target).value = fmt_float(a % b);
    } else {
        return Err(format!("{lt} cannot modulo by {rt}"));
    }
    Ok(target)
}

// ---- contains, pick, omit, unique, group, pivot ----

pub fn contains_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    fn contains(e: &mut Engine, l: NodeId, r: NodeId) -> Res<bool> {
        match e.n(l).kind {
            MAP => {
                if e.n(r).kind != MAP {
                    return Ok(false);
                }
                let rc = e.n(r).content.clone();
                let mut i = 0;
                while i + 1 < rc.len() {
                    let Some(idx) = find_key_in_map(e, l, rc[i]) else { return Ok(false) };
                    let lv = e.n(l).content[idx + 1];
                    if !contains(e, lv, rc[i + 1])? {
                        return Ok(false);
                    }
                    i += 2;
                }
                Ok(true)
            }
            SEQ => {
                let items: Vec<NodeId> = if e.n(r).kind != SEQ { vec![r] } else { e.n(r).content.clone() };
                let lc = e.n(l).content.clone();
                for item in items {
                    let mut found = false;
                    for a in &lc {
                        if contains(e, *a, item)? {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            SCALAR => {
                let (ln, rn) = (e.n(l), e.n(r));
                if rn.kind != SCALAR || ln.tag != rn.tag {
                    return Ok(false);
                }
                if ln.tag == "!!null" {
                    return Ok(rn.tag == "!!null");
                }
                if ln.tag == "!!str" {
                    return Ok(ln.value.contains(&rn.value));
                }
                Ok(ln.value == rn.value)
            }
            _ => Err(format!("{} not yet supported for contains", e.n(l).tag)),
        }
    }
    let mut calc = |e: &mut Engine, _c: &Ctx, l: Option<NodeId>, r: Option<NodeId>| -> Res<Option<NodeId>> {
        let (l, r) = (l.unwrap(), r.unwrap());
        if e.n(l).kind != e.n(r).kind {
            return Err(format!("{} cannot check contained in {}", e.n(r).tag, e.n(l).tag));
        }
        let v = contains(e, l, r)?;
        Ok(Some(create_boolean(e, Some(l), v)))
    };
    cross_function(e, ctx.readonly_clone(), x, &mut calc, false)
}

pub fn pick_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let picks = e.eval(&ctx, &x.rhs)?;
    let indices: Vec<NodeId> = match picks.nodes.first() {
        Some(p) => e.n(*p).content.clone(),
        None => Vec::new(),
    };
    map_each(e, ctx, |e, node| {
        let replacement = match e.n(node).kind {
            MAP => {
                let mut filtered = Vec::new();
                for k in &indices {
                    if let Some(idx) = find_key_in_map(e, node, *k) {
                        let (kk, vv) = (e.n(node).content[idx], e.n(node).content[idx + 1]);
                        let ck = e.copy(kk);
                        let cv = e.copy(vv);
                        filtered.push(ck);
                        filtered.push(cv);
                    }
                }
                let n = e.copy_without_content(node);
                e.add_children(n, &filtered);
                n
            }
            SEQ => {
                let mut filtered = Vec::new();
                for k in &indices {
                    let v = e.n(*k).value.clone();
                    let idx = crate::util::parse_int(&v).map_err(|_| format!("cannot index array with {v}"))?;
                    if idx > -1 && (idx as usize) < e.n(node).content.len() {
                        let c = e.n(node).content[idx as usize];
                        filtered.push(e.copy(c));
                    }
                }
                let n = e.copy_without_content(node);
                e.add_children(n, &filtered);
                n
            }
            _ => return Err(format!("cannot pick indices from type {} ({})", e.n(node).tag, e.get_nice_path(node))),
        };
        e.nm(replacement).leading_content = e.n(node).leading_content.clone();
        Ok(replacement)
    })
}

pub fn omit_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let omits = e.eval(&ctx, &x.rhs)?;
    let indices = match omits.nodes.first() {
        Some(p) => *p,
        None => e.add(CNode::default()),
    };
    if e.n(indices).content.is_empty() {
        return Ok(ctx);
    }
    let mut results = Vec::new();
    for node in ctx.nodes.iter() {
        let replacement = match e.n(*node).kind {
            MAP => {
                let content = e.n(*node).content.clone();
                let mut filtered = Vec::new();
                let mut i = 0;
                while i + 1 < content.len() {
                    if find_in_array(e, indices, content[i]).is_none() {
                        filtered.push(e.copy(content[i]));
                        filtered.push(e.copy(content[i + 1]));
                    }
                    i += 2;
                }
                let n = e.copy_without_content(*node);
                e.add_children(n, &filtered);
                n
            }
            SEQ => {
                let content = e.n(*node).content.clone();
                let mut filtered = Vec::new();
                for (i, c) in content.iter().enumerate() {
                    let idx = e.scalar("!!int", &i.to_string());
                    if find_in_array(e, indices, idx).is_none() {
                        filtered.push(e.copy(*c));
                    }
                }
                let n = e.copy_without_content(*node);
                e.add_children(n, &filtered);
                n
            }
            _ => return Ok(ctx),
        };
        e.nm(replacement).leading_content = e.n(*node).leading_content.clone();
        results.push(replacement);
    }
    Ok(ctx.child(results))
}

fn unique_key_value(e: &mut Engine, rhs: &Ctx) -> Res<String> {
    match rhs.nodes.first() {
        None => Ok("null".into()),
        Some(k) => {
            if e.n(*k).kind != SCALAR {
                crate::printer::encode_to_string(e, *k, Fmt::Yaml, 0)
            } else {
                Ok(e.n(*k).value.clone())
            }
        }
    }
}

pub fn unique_by(e: &mut Engine, ctx: Ctx, rhs: &Option<Expr>) -> Res<Ctx> {
    map_each(e, ctx.clone(), |e, c| {
        if e.n(c).kind != SEQ {
            return Err("only arrays are supported for unique".into());
        }
        let mut seen = std::collections::HashSet::new();
        let mut keep = Vec::new();
        for child in e.n(c).content.clone() {
            let r = e.eval(&ctx.single_readonly_child(child), rhs)?;
            let k = unique_key_value(e, &r)?;
            if seen.insert(k) {
                keep.push(child);
            }
        }
        let style = e.n(c).style;
        let result = e.create_replacement_with_comments(c, SEQ, "!!seq", style);
        for k in keep {
            e.add_child(result, k);
        }
        Ok(result)
    })
}

pub fn group_by(e: &mut Engine, ctx: Ctx, rhs: &Option<Expr>) -> Res<Ctx> {
    map_each(e, ctx.clone(), |e, c| {
        if e.n(c).kind != SEQ {
            return Err("only arrays are supported for group by".into());
        }
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<NodeId>> = Default::default();
        for child in e.n(c).content.clone() {
            let r = e.eval(&ctx.single_readonly_child(child), rhs)?;
            let k = r.nodes.first().map(|n| e.n(*n).value.clone()).unwrap_or_else(|| "null".into());
            if !groups.contains_key(&k) {
                order.push(k.clone());
            }
            groups.entry(k).or_default().push(child);
        }
        let result = e.create_replacement(c, SEQ, "!!seq", "");
        for k in order {
            let g = e.add(CNode::new(SEQ, "!!seq"));
            for item in &groups[&k] {
                e.add_child(g, *item);
            }
            e.add_child(result, g);
        }
        Ok(result)
    })
}

pub fn pivot_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    map_each(e, ctx, |e, c| {
        if e.n(c).tag != "!!seq" {
            return Err(format!("cannot pivot node of type {}", e.n(c).tag));
        }
        let content = e.n(c).content.clone();
        let tag = match content.len() {
            0 => String::new(),
            _ => {
                let first = e.n(content[0]).tag.clone();
                for x in &content[1..] {
                    if e.n(*x).tag != first {
                        return Err(format!("sequence contains elements of {} and {} types", first, e.n(*x).tag));
                    }
                }
                first
            }
        };
        let sz = content.len();
        match tag.as_str() {
            "!!seq" => {
                if sz == 0 {
                    return Ok(c);
                }
                let mut m: std::collections::BTreeMap<usize, Vec<NodeId>> = Default::default();
                for (i, row) in content.iter().enumerate() {
                    for (j, v) in e.n(*row).content.clone().iter().enumerate() {
                        let col = m.entry(j).or_default();
                        while col.len() < i {
                            col.push(usize::MAX);
                        }
                        col.push(*v);
                    }
                }
                let result = e.add(CNode::new(SEQ, ""));
                for i in 0..m.len() {
                    let mut col = m[&i].clone();
                    while col.len() < sz {
                        col.push(usize::MAX);
                    }
                    let col: Vec<NodeId> = col.into_iter().map(|n| if n == usize::MAX { e.scalar("!!null", "") } else { n }).collect();
                    let row = e.add(CNode::new(SEQ, ""));
                    e.add_children(row, &col);
                    e.add_child(result, row);
                }
                Ok(result)
            }
            "!!map" => {
                if sz == 0 {
                    return Ok(e.add(CNode::new(MAP, "")));
                }
                let mut keys: Vec<String> = Vec::new();
                let mut m: std::collections::HashMap<String, Vec<NodeId>> = Default::default();
                for (i, row) in content.iter().enumerate() {
                    let rc = e.n(*row).content.clone();
                    let mut j = 0;
                    while j + 1 < rc.len() {
                        let k = e.n(rc[j]).value.clone();
                        if !m.contains_key(&k) {
                            keys.push(k.clone());
                        }
                        let col = m.entry(k).or_default();
                        while col.len() < i {
                            col.push(usize::MAX);
                        }
                        col.push(rc[j + 1]);
                        j += 2;
                    }
                }
                let result = e.add(CNode::new(MAP, ""));
                for k in keys {
                    let mut col = m[&k].clone();
                    while col.len() < sz {
                        col.push(usize::MAX);
                    }
                    let col: Vec<NodeId> = col.into_iter().map(|n| if n == usize::MAX { e.scalar("!!null", "") } else { n }).collect();
                    let row = e.add(CNode::new(SEQ, ""));
                    e.add_children(row, &col);
                    let kn = e.scalar("!!str", &k);
                    e.add_key_value_child(result, kn, row);
                }
                Ok(result)
            }
            other => Err(format!("can only pivot elements of !!seq or !!map types, received {other}")),
        }
    })
}

// ---- sorting ----

struct Sortable {
    node: NodeId,
    compare: Vec<NodeId>,
}

fn sort_compare(e: &mut Engine, l: NodeId, r: NodeId, layout: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let mut lt = e.n(l).tag.clone();
    let mut rt = e.n(r).tag.clone();
    if !lt.starts_with("!!") {
        lt = e.guess_tag_from_custom_type(l);
    }
    if !rt.starts_with("!!") {
        rt = e.guess_tag_from_custom_type(r);
    }
    let mut is_dt = lt == "!!timestamp" && rt == "!!timestamp";
    let (lv, rv) = (e.n(l).value.clone(), e.n(r).value.clone());
    if lt == "!!str" && layout != gotime::RFC3339 {
        is_dt = parse_date_time(layout, &lv).is_ok() && parse_date_time(layout, &rv).is_ok();
    }
    if lt == "!!null" && rt != "!!null" {
        Less
    } else if lt != "!!null" && rt == "!!null" {
        Greater
    } else if lt == "!!bool" && rt != "!!bool" {
        Less
    } else if lt != "!!bool" && rt == "!!bool" {
        Greater
    } else if lt == "!!bool" && rt == "!!bool" {
        let (a, b) = (is_truthy(e, Some(l)), is_truthy(e, Some(r)));
        if a == b {
            Equal
        } else if a {
            Greater
        } else {
            Less
        }
    } else if is_dt {
        let a = match parse_date_time(layout, &lv) {
            Ok(t) => t,
            Err(err) => {
                crate::util::warn(&format!("Could not parse time {lv} with layout {layout} for sort, sorting by string instead: {err}"));
                return lv.cmp(&rv);
            }
        };
        let b = match parse_date_time(layout, &rv) {
            Ok(t) => t,
            Err(err) => {
                crate::util::warn(&format!("Could not parse time {rv} with layout {layout} for sort, sorting by string instead: {err}"));
                return lv.cmp(&rv);
            }
        };
        (a.sec, a.nsec).cmp(&(b.sec, b.nsec))
    } else if lt == "!!int" && rt == "!!int" {
        let a = parse_int64(&lv).map(|x| x.1).unwrap_or(0);
        let b = parse_int64(&rv).map(|x| x.1).unwrap_or(0);
        a.cmp(&b)
    } else if (lt == "!!int" || lt == "!!float") && (rt == "!!int" || rt == "!!float") {
        let a = parse_float(&lv).unwrap_or(0.0);
        let b = parse_float(&rv).unwrap_or(0.0);
        if a == b {
            Equal
        } else if a < b {
            Less
        } else {
            Greater
        }
    } else {
        lv.cmp(&rv)
    }
}

pub fn sort_by_operator(e: &mut Engine, ctx: Ctx, rhs: &Option<Expr>) -> Res<Ctx> {
    let layout = ctx.layout().to_string();
    map_each(e, ctx.clone(), |e, c| {
        let kind = e.n(c).kind;
        if kind != MAP && kind != SEQ {
            return Err(format!("node at path [{}] is not an array or map (it's a {})", e.get_nice_path(c), e.n(c).tag));
        }
        let values: Vec<NodeId> = if kind == MAP { e.n(c).content.iter().skip(1).step_by(2).copied().collect() } else { e.n(c).content.clone() };
        let mut arr = Vec::new();
        for v in values {
            let r = e.eval(&ctx.single_readonly_child(v), rhs)?;
            arr.push(Sortable { node: v, compare: r.nodes.to_vec() });
        }
        crate::util::go_stable_sort(&mut arr, |a, b| {
            for (l, r) in a.compare.iter().zip(b.compare.iter()) {
                match sort_compare(e, *l, *r, &layout) {
                    std::cmp::Ordering::Less => return true,
                    std::cmp::Ordering::Greater => return false,
                    _ => {}
                }
            }
            a.compare.len() < b.compare.len()
        });
        let sorted = e.copy_without_content(c);
        for s in arr {
            if kind == MAP {
                let k = e.n(s.node).key.unwrap();
                e.add_key_value_child(sorted, k, s.node);
            } else {
                e.add_child(sorted, s.node);
            }
        }
        Ok(sorted)
    })
}

pub fn shuffle_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    let now = gotime::Time::now();
    let seed = now.sec.wrapping_mul(1_000_000_000).wrapping_add(now.nsec);
    let mut rng = crate::gorand::GoRand::new(seed);
    map_each(e, ctx, |e, c| {
        if e.n(c).kind != SEQ {
            return Err(format!("node at path [{}] is not an array (it's a {})", e.get_nice_path(c), e.n(c).tag));
        }
        let result = e.copy(c);
        let mut a = e.n(result).content.clone();
        rng.shuffle(a.len(), |i, j| {
            a.swap(i, j);
            let (ki, kj) = (e.n(a[i]).key.unwrap(), e.n(a[j]).key.unwrap());
            let (vi, vj) = (e.n(ki).value.clone(), e.n(kj).value.clone());
            e.nm(ki).value = vj;
            e.nm(kj).value = vi;
        });
        e.nm(result).content = a;
        Ok(result)
    })
}

// ---- strings ----

fn split_string(e: &mut Engine, value: &str, sep: &str) -> Vec<NodeId> {
    if value.is_empty() {
        return Vec::new();
    }
    let parts: Vec<String> = if sep.is_empty() {
        value.chars().map(|c| c.to_string()).collect()
    } else {
        value.split(sep).map(|s| s.to_string()).collect()
    };
    parts.iter().map(|p| e.scalar("!!str", p)).collect()
}

fn first_value(e: &mut Engine, ctx: &Ctx, x: &Option<Expr>) -> Res<String> {
    let r = e.eval(ctx, x)?;
    Ok(r.nodes.first().map(|n| e.n(*n).value.clone()).unwrap_or_default())
}

pub fn join_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let sep = first_value(e, &ctx.readonly_clone(), &x.rhs)?;
    map_each(e, ctx, |e, c| {
        if e.n(c).kind != SEQ {
            return Err(format!("cannot join with {}, can only join arrays of scalars", e.n(c).tag));
        }
        let parts: Vec<String> =
            e.n(c).content.iter().map(|n| if e.n(*n).tag == "!!null" { String::new() } else { e.n(*n).value.clone() }).collect();
        let joined = parts.join(&sep);
        Ok(e.create_replacement(c, SCALAR, "!!str", &joined))
    })
}

pub fn split_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let sep = first_value(e, &ctx.readonly_clone(), &x.rhs)?;
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        if e.n(*c).tag == "!!null" {
            continue;
        }
        if e.guess_tag_from_custom_type(*c) != "!!str" {
            return Err(format!("cannot split {}, can only split strings", e.n(*c).tag));
        }
        let v = e.n(*c).value.clone();
        let parts = split_string(e, &v, &sep);
        let r = e.create_replacement(*c, SEQ, "!!seq", "");
        e.add_children(r, &parts);
        results.push(r);
    }
    Ok(ctx.child(results))
}

pub fn change_case_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::ChangeCase { upper } = x.op.prefs else { unreachable!() };
    map_each(e, ctx, |e, c| {
        if e.guess_tag_from_custom_type(c) != "!!str" {
            return Err(format!("cannot change case with {}, can only operate on strings. ", e.n(c).tag));
        }
        let v = if upper { crate::util::go_to_upper(&e.n(c).value) } else { crate::util::go_to_lower(&e.n(c).value) };
        let tag = e.n(c).tag.clone();
        let r = e.create_replacement(c, SCALAR, &tag, &v);
        e.nm(r).style = e.n(c).style;
        Ok(r)
    })
}

pub fn trim_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    map_each(e, ctx, |e, c| {
        if e.guess_tag_from_custom_type(c) != "!!str" {
            return Err(format!("cannot trim {}, can only operate on strings. ", e.n(c).tag));
        }
        let v = crate::util::go_trim_space(&e.n(c).value).to_string();
        let tag = e.n(c).tag.clone();
        let r = e.create_replacement(c, SCALAR, &tag, &v);
        e.nm(r).style = e.n(c).style;
        Ok(r)
    })
}

pub fn to_string_operator(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    map_each(e, ctx, |e, c| {
        let r;
        if e.n(c).tag == "!!str" {
            let v = e.n(c).value.clone();
            r = e.create_replacement(c, SCALAR, "!!str", &v);
        } else if e.n(c).kind == SCALAR {
            let v = e.n(c).value.clone();
            r = e.create_replacement(c, SCALAR, "!!str", &v);
            e.nm(r).style = DOUBLE;
        } else {
            let s = crate::printer::encode_to_yaml_string(e, c)?;
            r = e.create_replacement(c, SCALAR, "!!str", &s);
            e.nm(r).style = DOUBLE;
        }
        e.nm(r).tag = "!!str".into();
        Ok(r)
    })
}

fn interpolate(e: &mut Engine, ctx: &Ctx, s: &str) -> Res<String> {
    let runes: Vec<char> = s.chars().collect();
    let mut sb = String::new();
    let mut exp = String::new();
    let mut in_exp = false;
    let mut nested = 0;
    let mut i = 0;
    while i < runes.len() {
        let c = runes[i];
        if !in_exp {
            if c == '\\' && i < runes.len() - 1 {
                match runes[i + 1] {
                    '(' => {
                        in_exp = true;
                        i += 2;
                        continue;
                    }
                    '\\' => i += 1,
                    _ => {}
                }
            }
            sb.push(c);
        } else {
            if c == ')' {
                if nested == 0 {
                    let v = evaluate_interpolation(e, ctx, &exp)?;
                    in_exp = false;
                    exp.clear();
                    sb.push_str(&v);
                    i += 1;
                    continue;
                }
                nested -= 1;
            } else if c == '(' {
                nested += 1;
            } else if c == '\\' && i < runes.len() - 1 {
                let esc = runes[i + 1];
                if esc == ')' || esc == '\\' {
                    exp.push(esc);
                    i += 2;
                    continue;
                }
            }
            exp.push(c);
        }
        i += 1;
    }
    if in_exp {
        crate::util::warn("unclosed interpolation string, skipping interpolation");
        return Ok(s.to_string());
    }
    Ok(sb)
}

fn evaluate_interpolation(e: &mut Engine, ctx: &Ctx, src: &str) -> Res<String> {
    let exp = crate::parser::parse_expression(e, src)?;
    let r = e.eval(ctx, &exp)?;
    match r.nodes.first() {
        None => Ok(String::new()),
        Some(n) => {
            if e.n(*n).kind != SCALAR {
                crate::printer::encode_to_yaml_string(e, *n)
            } else {
                Ok(e.n(*n).value.clone())
            }
        }
    }
}

pub fn string_interpolation_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let sv = x.op.string_value.clone();
    if !e.prefs.string_interpolation {
        let n = e.scalar("!!str", &sv);
        return Ok(ctx.single_child(n));
    }
    if ctx.nodes.is_empty() {
        let v = interpolate(e, &ctx, &sv)?;
        let n = e.scalar("!!str", &v);
        return Ok(ctx.single_child(n));
    }
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let v = interpolate(e, &ctx.single_child(*c), &sv)?;
        results.push(e.scalar("!!str", &v));
    }
    Ok(ctx.child(results))
}

pub fn substitute_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let block = x.rhs.as_ref().unwrap();
    let re_src = first_value(e, &ctx.readonly_clone(), &block.lhs)?;
    let replacement = first_value(e, &ctx, &block.rhs)?;
    let re = crate::goregex::compile(&re_src)?;
    let rep = crate::goregex::convert_replacement(&replacement);
    map_each(e, ctx, |e, c| {
        if e.guess_tag_from_custom_type(c) != "!!str" {
            return Err(format!(
                "cannot substitute with {}, can only substitute strings. Hint: Most often you'll want to use '|=' over '=' for this operation",
                e.n(c).tag
            ));
        }
        let v = re.replace_all(&e.n(c).value, rep.as_str()).into_owned();
        Ok(e.create_replacement(c, SCALAR, "!!str", &v))
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum MatchKind {
    Match,
    Capture,
    Test,
}

pub fn match_operator(e: &mut Engine, ctx: Ctx, x: &Expr, kind: MatchKind) -> Res<Ctx> {
    let rhs = x.rhs.as_ref().unwrap();
    let mut re_exp = &x.rhs;
    let mut global = false;
    if rhs.op.op == Op::Block {
        re_exp = &rhs.lhs;
        let mut params = first_value(e, &ctx, &rhs.rhs)?;
        if params.contains('g') {
            params = params.replace('g', "");
            global = true;
        }
        if params.contains('i') {
            return Err("'i' is not a valid option for match. To ignore case, use an expression like match(\"(?i)cat\")".into());
        }
        if !params.is_empty() {
            return Err(format!("unrecognised match params '{params}', please see docs at https://mikefarah.gitbook.io/yq/operators/string-operators"));
        }
    }
    let re_src = first_value(e, &ctx.readonly_clone(), re_exp)?;
    let re = crate::goregex::compile(&re_src)?;
    let names: Vec<String> = re.capture_names().map(|n| n.unwrap_or("").to_string()).collect();
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        if e.guess_tag_from_custom_type(*c) != "!!str" {
            return Err(format!(
                "cannot match with {}, can only match strings. Hint: Most often you'll want to use '|=' over '=' for this operation",
                e.n(*c).tag
            ));
        }
        let value = e.n(*c).value.clone();
        if kind == MatchKind::Test {
            let m = re.is_match(&value);
            results.push(create_boolean(e, Some(*c), m));
            continue;
        }
        let caps: Vec<regex::Captures> = if global { re.captures_iter(&value).collect() } else { re.captures(&value).into_iter().collect() };
        for cap in caps {
            if kind == MatchKind::Match {
                let captures_list = e.add(CNode::new(SEQ, ""));
                for j in 1..cap.len() {
                    let capnode = e.add(CNode::new(MAP, ""));
                    let content = add_match(e, cap.get(j).map(|m| (m.as_str(), m.start() as i64)), &names[j]);
                    e.add_children(capnode, &content);
                    e.add_child(captures_list, capnode);
                }
                let node = e.create_replacement(*c, MAP, "!!map", "");
                let m0 = cap.get(0).unwrap();
                let content = add_match(e, Some((m0.as_str(), m0.start() as i64)), "");
                e.add_children(node, &content);
                let ck = e.scalar("!!str", "captures");
                e.add_key_value_child(node, ck, captures_list);
                results.push(node);
            } else {
                let node = e.create_replacement(*c, MAP, "!!map", "");
                for j in 1..cap.len() {
                    let k = e.scalar("!!str", &names[j]);
                    let v = match cap.get(j) {
                        Some(m) => e.scalar("!!str", m.as_str()),
                        None => e.scalar("!!null", "null"),
                    };
                    e.add_key_value_child(node, k, v);
                }
                results.push(node);
            }
        }
    }
    Ok(ctx.child(results))
}

fn add_match(e: &mut Engine, m: Option<(&str, i64)>, name: &str) -> Vec<NodeId> {
    let mut out = vec![e.scalar("!!str", "string")];
    let (text, offset) = match m {
        Some((t, o)) => (t, o),
        None => ("", -1),
    };
    if offset < 0 {
        out.push(e.scalar("!!null", "null"));
    } else {
        out.push(e.scalar("!!str", text));
    }
    out.push(e.scalar("!!str", "offset"));
    out.push(e.scalar("!!int", &offset.to_string()));
    out.push(e.scalar("!!str", "length"));
    out.push(e.scalar("!!int", &text.len().to_string()));
    if !name.is_empty() {
        out.push(e.scalar("!!str", "name"));
        out.push(e.scalar("!!str", name));
    }
    out
}

// ---- date and time ----

fn string_parameter(e: &mut Engine, name: &str, ctx: &Ctx, x: &Option<Expr>) -> Res<String> {
    let r = e.eval(&ctx.readonly_clone(), x)?;
    match r.nodes.first() {
        Some(n) => Ok(e.n(*n).value.clone()),
        None => Err(format!("could not find {name} for format_time")),
    }
}

pub fn with_date_time_format(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let rhs = x.rhs.as_ref().unwrap();
    if rhs.op.op == Op::Block || rhs.op.op == Op::Union {
        let layout = string_parameter(e, "layout", &ctx, &rhs.lhs).map_err(|err| format!("could not get date time format: {err}"))?;
        let mut c = ctx;
        c.dt_layout = Some(layout.into());
        return e.eval(&c, &rhs.rhs);
    }
    Err("must provide a date time format string and an expression, e.g. with_dtf(\"Monday, 02-Jan-06 at 3:04PM MST\"; <exp>)".into())
}

pub fn now_op(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    let v = gotime::Time::now().format(gotime::RFC3339);
    let n = e.scalar("!!timestamp", &v);
    Ok(ctx.single_child(n))
}

pub fn format_date_time(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let format = string_parameter(e, "format", &ctx, &x.rhs);
    let layout = ctx.layout().to_string();
    let format = format?;
    map_each(e, ctx, |e, c| {
        let t = parse_date_time(&layout, &e.n(c).value).map_err(|err| format!("could not parse datetime of [{}]: {err}", e.get_nice_path(c)))?;
        let s = t.format(&format);
        let node = match crate::formats::yaml::parse_snippet(e, &s) {
            Ok(n) => n,
            Err(_) => e.scalar("!!str", &s),
        };
        e.nm(node).parent = e.n(c).parent;
        e.nm(node).key = e.n(c).key;
        Ok(node)
    })
}

pub fn tz_op(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let tz = string_parameter(e, "timezone", &ctx, &x.rhs);
    let layout = ctx.layout().to_string();
    let tz = tz?;
    let loc = gotime::load_location(&tz).map_err(|err| format!("could not load tz [{tz}]: {err}"))?;
    map_each(e, ctx, |e, c| {
        let t = parse_date_time(&layout, &e.n(c).value)
            .map_err(|err| format!("could not parse datetime of [{}] using layout [{layout}]: {err}", e.get_nice_path(c)))?;
        let s = t.in_loc(&loc).format(&layout);
        let tag = e.n(c).tag.clone();
        Ok(e.create_replacement(c, SCALAR, &tag, &s))
    })
}

pub fn from_unix_op(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    map_each(e, ctx, |e, c| {
        let t = e.guess_tag_from_custom_type(c);
        if t != "!!int" && t != "!!float" {
            return Err(format!("from_unix only works on numbers, found {} instead", e.n(c).tag));
        }
        let secs = parse_float(&e.n(c).value)?;
        let millis = (secs * 1000.0) as i64;
        let tm = gotime::Time::unix(millis.div_euclid(1000), millis.rem_euclid(1000) * 1_000_000, gotime::local().clone());
        let s = tm.format(gotime::RFC3339);
        Ok(e.create_replacement(c, SCALAR, "!!timestamp", &s))
    })
}

pub fn to_unix_op(e: &mut Engine, ctx: Ctx) -> Res<Ctx> {
    let layout = ctx.layout().to_string();
    map_each(e, ctx, |e, c| {
        let t = parse_date_time(&layout, &e.n(c).value)
            .map_err(|err| format!("could not parse datetime of [{}] using layout [{layout}]: {err}", e.get_nice_path(c)))?;
        Ok(e.create_replacement(c, SCALAR, "!!int", &t.sec.to_string()))
    })
}
