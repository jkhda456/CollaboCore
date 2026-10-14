//! The smaller formats: Java properties (magiconair/properties), CSV/TSV (encoding/csv),
//! base64, URI, @sh and shell variables.

use super::{Decoder, Encoder};
use crate::eval::*;
use crate::node::*;
use crate::parser::*;
use crate::prefs::{CsvPrefs, PropsPrefs, ShellVarsPrefs};

/// mapKeysToStrings
pub fn map_keys_to_strings(e: &mut Engine, node: NodeId) {
    if e.n(node).kind == MAP {
        let c = e.n(node).content.clone();
        for (i, k) in c.iter().enumerate() {
            if i % 2 == 0 {
                e.nm(*k).tag = "!!str".into();
            }
        }
    }
    for c in e.n(node).content.clone() {
        map_keys_to_strings(e, c);
    }
}

fn leading_content_passthrough(out: &mut Vec<u8>, content: &str) {
    // the properties encoder prints leading content as is (separators dropped)
    let mut rest = content;
    loop {
        let (line, eof) = match rest.find('\n') {
            Some(i) => {
                let l = &rest[..=i];
                rest = &rest[i + 1..];
                (l, false)
            }
            None => {
                let l = rest;
                rest = "";
                (l, true)
            }
        };
        if !line.contains("$yqDocSeparator$") {
            out.extend_from_slice(line.as_bytes());
        }
        if eof {
            if !line.is_empty() {
                out.push(b'\n');
            }
            break;
        }
    }
}

// ---- properties ----

struct Props {
    m: std::collections::HashMap<String, String>,
    c: std::collections::HashMap<String, Vec<String>>,
    k: Vec<String>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum PItem {
    Error,
    Eof,
    Key,
    Value,
    Comment,
}

fn props_lex(input: &str) -> Vec<(PItem, String, usize)> {
    let chars: Vec<char> = input.chars().collect();
    let mut byte_pos: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut b = 0;
    for c in &chars {
        byte_pos.push(b);
        b += c.len_utf8();
    }
    byte_pos.push(b);
    let mut items = Vec::new();
    let mut pos = 0usize;
    let mut start = 0usize;
    let mut runes: Vec<char> = Vec::new();
    let next = |pos: &mut usize| -> Option<char> {
        if *pos >= chars.len() {
            *pos += 1;
            return None;
        }
        let c = chars[*pos];
        *pos += 1;
        Some(c)
    };
    let is_eol = |c: Option<char>| matches!(c, Some('\n') | Some('\r'));
    let ws = |c: Option<char>| matches!(c, Some(' ') | Some('\x0c') | Some('\t'));
    #[derive(Clone, Copy)]
    enum S {
        BeforeKey,
        Comment,
        Key,
        BeforeValue,
        Value,
    }
    let mut state = Some(S::BeforeKey);
    macro_rules! emit {
        ($t:expr) => {{
            items.push(($t, runes.iter().collect::<String>(), byte_pos[start.min(chars.len())]));
            start = pos.min(chars.len());
            runes.clear();
        }};
    }
    // escape sequence: Ok(()) or Err(message)
    let scan_escape = |pos: &mut usize, runes: &mut Vec<char>| -> Result<(), String> {
        let r = if *pos >= chars.len() {
            *pos += 1;
            None
        } else {
            *pos += 1;
            Some(chars[*pos - 1])
        };
        match r {
            Some(c) if " :=fnrt".contains(c) => {
                runes.push(match c {
                    'f' => '\x0c',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    o => o,
                });
                Ok(())
            }
            Some('u') => {
                let mut d = String::new();
                for _ in 0..4 {
                    if *pos >= chars.len() || !chars[*pos].is_ascii_hexdigit() {
                        *pos += 1;
                        return Err("invalid unicode literal".into());
                    }
                    d.push(chars[*pos]);
                    *pos += 1;
                }
                let v = u32::from_str_radix(&d, 16).unwrap();
                runes.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                Ok(())
            }
            None => Err("premature EOF".into()),
            Some(c) => {
                runes.push(c);
                Ok(())
            }
        }
    };
    while let Some(s) = state {
        state = match s {
            S::BeforeKey => {
                let r = next(&mut pos);
                match r {
                    None => {
                        emit!(PItem::Eof);
                        None
                    }
                    _ if is_eol(r) => {
                        start = pos;
                        Some(S::BeforeKey)
                    }
                    Some('#') | Some('!') => Some(S::Comment),
                    _ if ws(r) => {
                        start = pos;
                        Some(S::BeforeKey)
                    }
                    _ => {
                        pos -= 1;
                        Some(S::Key)
                    }
                }
            }
            S::Comment => {
                while ws(chars.get(pos).copied()) {
                    pos += 1;
                }
                start = pos;
                loop {
                    let r = next(&mut pos);
                    match r {
                        None => {
                            pos -= 1;
                            start = pos;
                            runes.clear();
                            emit!(PItem::Eof);
                            break None;
                        }
                        _ if is_eol(r) => {
                            emit!(PItem::Comment);
                            break Some(S::BeforeKey);
                        }
                        Some(c) => runes.push(c),
                    }
                }
            }
            S::Key => {
                let mut eof = false;
                let mut err: Option<String> = None;
                loop {
                    let r = next(&mut pos);
                    match r {
                        Some('\\') => {
                            if let Err(e) = scan_escape(&mut pos, &mut runes) {
                                err = Some(e);
                                break;
                            }
                        }
                        Some(c) if " \x0c\t\r\n:=".contains(c) => {
                            pos -= 1;
                            break;
                        }
                        None => {
                            pos -= 1;
                            eof = true;
                            break;
                        }
                        Some(c) => runes.push(c),
                    }
                }
                if let Some(e) = err {
                    items.push((PItem::Error, e, byte_pos[start.min(chars.len())]));
                    None
                } else {
                    if !runes.is_empty() {
                        emit!(PItem::Key);
                    }
                    if eof {
                        emit!(PItem::Eof);
                        None
                    } else {
                        Some(S::BeforeValue)
                    }
                }
            }
            S::BeforeValue => {
                while ws(chars.get(pos).copied()) {
                    pos += 1;
                }
                if matches!(chars.get(pos), Some(':') | Some('=')) {
                    pos += 1;
                }
                while ws(chars.get(pos).copied()) {
                    pos += 1;
                }
                start = pos;
                Some(S::Value)
            }
            S::Value => loop {
                let r = next(&mut pos);
                match r {
                    Some('\\') => {
                        if is_eol(chars.get(pos).copied()) {
                            pos += 1;
                            while ws(chars.get(pos).copied()) {
                                pos += 1;
                            }
                        } else if let Err(e) = scan_escape(&mut pos, &mut runes) {
                            items.push((PItem::Error, e, byte_pos[start.min(chars.len())]));
                            break None;
                        }
                    }
                    _ if is_eol(r) => {
                        emit!(PItem::Value);
                        start = pos;
                        break Some(S::BeforeKey);
                    }
                    None => {
                        pos -= 1;
                        emit!(PItem::Value);
                        emit!(PItem::Eof);
                        break None;
                    }
                    Some(c) => runes.push(c),
                }
            },
        };
    }
    items
}

fn props_parse(input: &str) -> Result<Props, String> {
    let items = props_lex(input);
    let mut p = Props { m: Default::default(), c: Default::default(), k: Vec::new() };
    let mut idx = 0;
    let mut last_pos = 0usize;
    let mut comments: Vec<String> = Vec::new();
    let line_of = |pos: usize| 1 + input[..pos.min(input.len())].matches('\n').count();
    let fmt_item = |t: PItem, v: &str| -> String {
        match t {
            PItem::Eof => "EOF".into(),
            PItem::Error => v.into(),
            _ => {
                if v.chars().count() > 10 {
                    let s: String = v.chars().take(10).collect();
                    format!("{}...", crate::util::go_quote(&s))
                } else {
                    crate::util::go_quote(v)
                }
            }
        }
    };
    let mut next = |idx: &mut usize, last_pos: &mut usize| -> (PItem, String) {
        let (t, v, pos) = items.get(*idx).cloned().unwrap_or((PItem::Eof, String::new(), 0));
        *idx += 1;
        *last_pos = pos;
        (t, v)
    };
    loop {
        let (t, v) = next(&mut idx, &mut last_pos);
        let key;
        match t {
            PItem::Eof => break,
            PItem::Comment => {
                comments.push(v);
                continue;
            }
            PItem::Key => {
                key = v;
                if !p.m.contains_key(&key) {
                    p.k.push(key.clone());
                }
            }
            _ => return Err(format!("properties: Line {}: {}", line_of(last_pos), fmt_item(t, &v))),
        }
        let (t, v) = next(&mut idx, &mut last_pos);
        if !comments.is_empty() {
            p.c.insert(key.clone(), std::mem::take(&mut comments));
        }
        match t {
            PItem::Eof => {
                p.m.insert(key, String::new());
                break;
            }
            PItem::Value => {
                p.m.insert(key, v);
            }
            _ => return Err(format!("properties: Line {}: {}", line_of(last_pos), fmt_item(t, &v))),
        }
    }
    Ok(p)
}

fn props_expand(s: &str, keys: &mut Vec<String>, values: &std::collections::HashMap<String, String>) -> Result<String, String> {
    if keys.len() > 64 {
        return Err("expansion too deep".into());
    }
    let mut s = s.to_string();
    loop {
        let Some(start) = s.find("${") else { return Ok(s) };
        let key_start = start + 2;
        let Some(key_len) = s[key_start..].find('}') else { return Err("malformed expression".into()) };
        let end = key_start + key_len;
        let key = s[key_start..key_start + key_len].to_string();
        if keys.contains(&key) {
            let mut b = String::from("circular reference in:\n");
            for k in keys.iter() {
                b.push_str(&format!("{}={}\n", k, values.get(k).cloned().unwrap_or_default()));
            }
            return Err(b);
        }
        let val = values.get(&key).cloned().unwrap_or_else(|| std::env::var(&key).unwrap_or_default());
        keys.push(key);
        let nv = props_expand(&val, keys, values);
        keys.pop();
        let nv = nv?;
        s = format!("{}{}{}", &s[..start], nv, &s[end + 1..]);
    }
}

pub struct PropsDecoder {
    input: Vec<u8>,
    finished: bool,
    prefs: PropsPrefs,
}

impl PropsDecoder {
    pub fn new(prefs: PropsPrefs) -> PropsDecoder {
        PropsDecoder { input: Vec::new(), finished: false, prefs }
    }
}

fn parse_prop_key(key: &str, brackets: bool) -> Vec<PathElem> {
    let mut path = Vec::new();
    for seg in key.split('.') {
        if brackets && seg.contains('[') {
            if let Some(bp) = bracket_segment(seg) {
                path.extend(bp);
                continue;
            }
        }
        match crate::gostr::parse_int_bits(seg, 10, 32) {
            Ok(n) => path.push(PathElem::Int(n)),
            Err(_) => path.push(PathElem::Str(seg.to_string())),
        }
    }
    path
}

fn bracket_segment(seg: &str) -> Option<Vec<PathElem>> {
    let mut path = Vec::new();
    let bi = seg.find('[')?;
    if bi > 0 {
        path.push(PathElem::Str(seg[..bi].to_string()));
    }
    let mut rem = &seg[bi..];
    while !rem.is_empty() {
        if !rem.starts_with('[') {
            return None;
        }
        let close = rem.find(']')?;
        let n = crate::gostr::parse_int_bits(&rem[1..close], 10, 32).ok()?;
        path.push(PathElem::Int(n));
        rem = &rem[close + 1..];
    }
    Some(path)
}

impl Decoder for PropsDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        self.input = input;
        self.finished = false;
        Ok(())
    }

    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        if self.finished || self.input.is_empty() {
            self.finished = true;
            return Ok(None);
        }
        let mut text = String::from_utf8_lossy(&self.input).into_owned();
        if let Some(t) = text.strip_prefix('\u{feff}') {
            text = t.to_string();
        }
        let props = props_parse(&text)?;
        for (k, v) in &props.m {
            let mut keys = vec![k.clone()];
            props_expand(v, &mut keys, &props.m)?;
        }
        let root = e.add(CNode::new(MAP, "!!map"));
        let ctx = Ctx::new(Vec::new()).single_child(root);
        for key in &props.k {
            let value = props.m.get(key).cloned().unwrap_or_default();
            let path = parse_prop_key(key, self.prefs.use_array_brackets);
            if let Some(comments) = props.c.get(key) {
                if !comments.is_empty() {
                    let joined = comments.join("\n");
                    let hc = if joined.is_empty() { String::new() } else { format!("# {joined}") };
                    let last = path.last().unwrap().to_go_string();
                    let rhs = e.scalar("!!str", &last);
                    e.nm(rhs).head_comment = hc;
                    let tag = e.guess_tag_from_custom_type(rhs);
                    e.nm(rhs).tag = tag;
                    let lhs = create_traversal_tree(e, &path, TraversePrefs::default(), true);
                    let node = expr(Operation::with_prefs(Op::Assign, Prefs::Assign(Default::default())), Some(lhs), Some(reference_expr(rhs)));
                    let _ = e.get_matching_nodes(&ctx, Some(&node));
                }
            }
            let rhs = e.string_scalar(&value);
            let tag = e.guess_tag_from_custom_type(rhs);
            e.nm(rhs).tag = tag;
            deeply_assign(e, &ctx, &path, rhs)?;
        }
        self.finished = true;
        Ok(Some(root))
    }
}

/// DataTreeNavigator.DeeplyAssign
pub fn deeply_assign(e: &mut Engine, ctx: &Ctx, path: &[PathElem], rhs: NodeId) -> Res<()> {
    let op = if e.n(rhs).kind == MAP {
        Operation::with_prefs(
            Op::MultiplyAssign,
            Prefs::Multiply(MultiplyPrefs {
                append_arrays: true,
                traverse: TraversePrefs { dont_follow_alias: true, ..Default::default() },
                ..Default::default()
            }),
        )
    } else {
        Operation::with_prefs(Op::Assign, Prefs::Assign(Default::default()))
    };
    let lhs = create_traversal_tree(e, path, TraversePrefs::default(), false);
    let value = expr(Operation { node: Some(rhs), ..Operation::new(Op::Value) }, None, None);
    let node = expr(op, Some(lhs), Some(value));
    e.get_matching_nodes(ctx, Some(&node))?;
    Ok(())
}

pub struct PropsEncoder {
    pub prefs: PropsPrefs,
}

fn props_escape(s: &str, special: &str) -> String {
    let mut v = String::new();
    for (i, r) in s.chars().enumerate() {
        let sp = if i == 0 && crate::util::go_is_space(r) { " " } else { special };
        match r {
            '\x0c' => v.push_str("\\f"),
            '\n' => v.push_str("\\n"),
            '\r' => v.push_str("\\r"),
            '\t' => v.push_str("\\t"),
            '\\' => v.push_str("\\\\"),
            c if sp.contains(c) => {
                v.push('\\');
                v.push(c);
            }
            c => v.push(c),
        }
    }
    v
}

fn strip_first_hash(s: &str) -> String {
    s.replacen('#', "", 1)
}

impl PropsEncoder {
    fn do_encode(&self, e: &mut Engine, p: &mut Props, node: NodeId, path: &str, key: Option<NodeId>) -> Res<()> {
        let mut comments = String::new();
        if let Some(k) = key {
            comments = strip_first_hash(&e.n(k).head_comment) + &strip_first_hash(&e.n(k).line_comment);
        }
        comments += &(strip_first_hash(&e.n(node).head_comment) + &strip_first_hash(&e.n(node).line_comment));
        let with_spaces = comments.replace('\n', "\n ");
        p.c.insert(path.to_string(), with_spaces.split('\n').map(String::from).collect());
        match e.n(node).kind {
            SCALAR => {
                let v = e.n(node).value.clone();
                let value = if self.prefs.unwrap_scalar || !v.contains(' ') { v } else { crate::util::go_quote(&v) };
                if path.is_empty() {
                    return Ok(());
                }
                let existed = p.m.contains_key(path);
                p.m.insert(path.to_string(), value.clone());
                let mut keys = vec![path.to_string()];
                if let Err(err) = props_expand(&value, &mut keys, &p.m) {
                    p.m.remove(path);
                    return Err(err);
                }
                if !existed {
                    p.k.push(path.to_string());
                }
                Ok(())
            }
            SEQ => {
                for (i, c) in e.n(node).content.clone().iter().enumerate() {
                    let np = self.append_path(path, &PathElem::Int(i as i64));
                    self.do_encode(e, p, *c, &np, None)?;
                }
                Ok(())
            }
            MAP => {
                let c = e.n(node).content.clone();
                let mut i = 0;
                while i + 1 < c.len() {
                    let np = self.append_path(path, &PathElem::Str(e.n(c[i]).value.clone()));
                    self.do_encode(e, p, c[i + 1], &np, Some(c[i]))?;
                    i += 2;
                }
                Ok(())
            }
            ALIAS => {
                let a = e.n(node).alias.unwrap();
                self.do_encode(e, p, a, path, None)
            }
            _ => Err(format!("unsupported node {}", e.n(node).tag)),
        }
    }

    fn append_path(&self, path: &str, key: &PathElem) -> String {
        if path.is_empty() {
            return key.to_go_string();
        }
        if let PathElem::Int(i) = key {
            if self.prefs.use_array_brackets {
                return format!("{path}[{i}]");
            }
        }
        format!("{path}.{}", key.to_go_string())
    }
}

impl Encoder for PropsEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, out: &mut Vec<u8>, content: &str) -> Res<()> {
        leading_content_passthrough(out, content);
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        if e.n(node).kind == SCALAR {
            out.extend_from_slice(e.n(node).value.as_bytes());
            out.push(b'\n');
            return Ok(());
        }
        map_keys_to_strings(e, node);
        let mut p = Props { m: Default::default(), c: Default::default(), k: Vec::new() };
        self.do_encode(e, &mut p, node, "", None)?;
        let sep = if self.prefs.key_value_separator.is_empty() { " = " } else { &self.prefs.key_value_separator };
        let mut n = 0usize;
        for key in &p.k {
            let value = &p.m[key];
            if let Some(comments) = p.c.get(key) {
                let all_empty = comments.iter().all(|c| c.is_empty());
                if !all_empty {
                    if !comments.is_empty() && n > 0 {
                        out.push(b'\n');
                        n += 1;
                    }
                    for c in comments {
                        let line = format!("#{c}\n");
                        n += line.len();
                        out.extend_from_slice(line.as_bytes());
                    }
                }
            }
            let line = format!("{}{}{}\n", props_escape(key, " :="), sep, props_escape(value, ""));
            n += line.len();
            out.extend_from_slice(line.as_bytes());
        }
        Ok(())
    }
}

// ---- CSV ----

pub struct CsvDecoder {
    prefs: CsvPrefs,
    input: Vec<u8>,
    finished: bool,
}

impl CsvDecoder {
    pub fn new(prefs: CsvPrefs) -> CsvDecoder {
        CsvDecoder { prefs, input: Vec::new(), finished: false }
    }
}

/// encoding/csv's Reader.
pub struct CsvReader<'a> {
    data: &'a [u8],
    pos: usize,
    num_line: usize,
    comma: char,
    fields_per_record: i64,
}

impl<'a> CsvReader<'a> {
    pub fn new(data: &'a [u8], comma: char) -> Self {
        CsvReader { data, pos: 0, num_line: 0, comma, fields_per_record: 0 }
    }

    fn read_line(&mut self) -> (Vec<u8>, bool) {
        if self.pos >= self.data.len() {
            return (Vec::new(), true);
        }
        let end = match self.data[self.pos..].iter().position(|b| *b == b'\n') {
            Some(i) => self.pos + i + 1,
            None => self.data.len(),
        };
        let mut line = self.data[self.pos..end].to_vec();
        let at_eof_no_nl = end == self.data.len() && !line.ends_with(b"\n");
        self.pos = end;
        if at_eof_no_nl && line.ends_with(b"\r") {
            line.pop();
        }
        self.num_line += 1;
        let n = line.len();
        if n >= 2 && line[n - 2] == b'\r' && line[n - 1] == b'\n' {
            line[n - 2] = b'\n';
            line.pop();
        }
        (line, false)
    }

    /// Ok(None) at EOF
    pub fn read(&mut self) -> Result<Option<Vec<String>>, String> {
        let comma = self.comma;
        if comma == '\0' || comma == '"' || comma == '\r' || comma == '\n' || comma == '\u{fffd}' {
            return Err("csv: invalid field or comment delimiter".into());
        }
        let mut line;
        let mut eof;
        loop {
            let (l, e) = self.read_line();
            line = l;
            eof = e;
            if !eof && line.len() == length_nl(&line) {
                continue;
            }
            break;
        }
        if eof {
            return Ok(None);
        }
        let mut cb = [0u8; 4];
        let comma_bytes = comma.encode_utf8(&mut cb).as_bytes().to_vec();
        let comma_len = comma_bytes.len();
        let rec_line = self.num_line;
        let mut record: Vec<u8> = Vec::new();
        let mut field_indexes: Vec<usize> = Vec::new();
        let (mut pos_line, mut pos_col) = (self.num_line, 1usize);
        let mut err: Option<String> = None;
        let mut l: &[u8] = &line;
        let mut owned: Vec<u8>;
        'parse: loop {
            if l.is_empty() || l[0] != b'"' {
                let i = find_sub(l, &comma_bytes);
                let field = match i {
                    Some(i) => &l[..i],
                    None => &l[..l.len() - length_nl(l)],
                };
                if let Some(j) = field.iter().position(|b| *b == b'"') {
                    err = Some(csv_err(rec_line, self.num_line, pos_col + j, "bare \" in non-quoted-field"));
                    break 'parse;
                }
                record.extend_from_slice(field);
                field_indexes.push(record.len());
                if let Some(i) = i {
                    l = &l[i + comma_len..];
                    pos_col += i + comma_len;
                    continue 'parse;
                }
                break 'parse;
            } else {
                l = &l[1..];
                pos_col += 1;
                loop {
                    if let Some(i) = l.iter().position(|b| *b == b'"') {
                        record.extend_from_slice(&l[..i]);
                        l = &l[i + 1..];
                        pos_col += i + 1;
                        let rn = std::str::from_utf8(&l[..l.len().min(4)]).ok().and_then(|s| s.chars().next());
                        if rn == Some('"') {
                            record.push(b'"');
                            l = &l[1..];
                            pos_col += 1;
                        } else if rn == Some(comma) {
                            l = &l[comma_len..];
                            pos_col += comma_len;
                            field_indexes.push(record.len());
                            continue 'parse;
                        } else if length_nl(l) == l.len() {
                            field_indexes.push(record.len());
                            break 'parse;
                        } else {
                            err = Some(csv_err(rec_line, self.num_line, pos_col - 1, "extraneous or missing \" in quoted-field"));
                            break 'parse;
                        }
                    } else if !l.is_empty() {
                        record.extend_from_slice(l);
                        pos_col += l.len();
                        let (nl, _eof) = self.read_line();
                        owned = nl;
                        l = &owned;
                        if !l.is_empty() {
                            pos_line += 1;
                            pos_col = 1;
                        }
                    } else {
                        err = Some(csv_err(rec_line, pos_line, pos_col, "extraneous or missing \" in quoted-field"));
                        break 'parse;
                    }
                }
            }
        }
        let s = String::from_utf8_lossy(&record).into_owned();
        let mut fields = Vec::new();
        let mut pre = 0;
        for idx in field_indexes {
            fields.push(s.get(pre..idx).map(String::from).unwrap_or_default());
            pre = idx;
        }
        if let Some(e) = err {
            return Err(e);
        }
        if self.fields_per_record > 0 {
            if fields.len() as i64 != self.fields_per_record {
                return Err(format!("record on line {rec_line}: wrong number of fields"));
            }
        } else if self.fields_per_record == 0 {
            self.fields_per_record = fields.len() as i64;
        }
        Ok(Some(fields))
    }
}


fn length_nl(b: &[u8]) -> usize {
    if b.last() == Some(&b'\n') {
        1
    } else {
        0
    }
}

fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}

fn csv_err(start: usize, line: usize, col: usize, msg: &str) -> String {
    if start != line {
        format!("record on line {start}; parse error on line {line}, column {col}: {msg}")
    } else {
        format!("parse error on line {line}, column {col}: {msg}")
    }
}

impl CsvDecoder {
    fn convert(&self, e: &mut Engine, content: &str) -> NodeId {
        match crate::formats::yaml::parse_snippet(e, content) {
            Ok(n) => {
                if !self.prefs.auto_parse && (e.n(n).kind != SCALAR || e.n(n).value != content) {
                    return e.scalar("!!str", content);
                }
                n
            }
            Err(_) => e.scalar("!!str", content),
        }
    }
}

impl Decoder for CsvDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        let mut input = input;
        for bom in [&[0xEF, 0xBB, 0xBF][..], &[0x00, 0x00, 0xFE, 0xFF], &[0xFF, 0xFE, 0x00, 0x00], &[0xFE, 0xFF], &[0xFF, 0xFE]] {
            if input.starts_with(bom) {
                input = input[bom.len()..].to_vec();
                break;
            }
        }
        self.input = input;
        self.finished = false;
        Ok(())
    }

    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        if self.finished {
            return Ok(None);
        }
        let data = std::mem::take(&mut self.input);
        let mut r = CsvReader::new(&data, self.prefs.separator);
        let header = match r.read()? {
            Some(h) => h,
            None => return Ok(None),
        };
        let root = e.add(CNode::new(SEQ, "!!seq"));
        loop {
            match r.read()? {
                Some(row) if !row.is_empty() => {
                    let obj = e.add(CNode::new(MAP, "!!map"));
                    for (i, h) in header.iter().enumerate() {
                        let k = e.scalar("!!str", h);
                        let v = self.convert(e, row.get(i).map(String::as_str).unwrap_or(""));
                        e.add_key_value_child(obj, k, v);
                    }
                    e.add_child(root, obj);
                }
                _ => break,
            }
        }
        self.finished = true;
        Ok(Some(root))
    }
}

pub struct CsvEncoder {
    pub separator: char,
}

impl CsvEncoder {
    fn needs_quotes(&self, f: &str) -> bool {
        if f.is_empty() {
            return false;
        }
        if f == "\\." {
            return true;
        }
        if f.contains(self.separator) || f.contains(['"', '\r', '\n']) {
            return true;
        }
        crate::util::go_is_space(f.chars().next().unwrap())
    }

    fn write_row(&self, out: &mut Vec<u8>, fields: &[String]) {
        for (n, f) in fields.iter().enumerate() {
            if n > 0 {
                let mut b = [0u8; 4];
                out.extend_from_slice(self.separator.encode_utf8(&mut b).as_bytes());
            }
            if !self.needs_quotes(f) {
                out.extend_from_slice(f.as_bytes());
                continue;
            }
            out.push(b'"');
            for c in f.chars() {
                if c == '"' {
                    out.extend_from_slice(b"\"\"");
                } else {
                    let mut b = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                }
            }
            out.push(b'"');
        }
        out.push(b'\n');
    }

    fn encode_row(&self, e: &Engine, out: &mut Vec<u8>, contents: &[NodeId]) -> Res<()> {
        let mut vals = Vec::new();
        for (i, c) in contents.iter().enumerate() {
            if e.n(*c).kind != SCALAR {
                return Err(format!("csv encoding only works for arrays of scalars (string/numbers/booleans), child[{i}] is a {}", e.n(*c).tag));
            }
            vals.push(e.n(*c).value.clone());
        }
        self.write_row(out, &vals);
        Ok(())
    }
}

impl Encoder for CsvEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        let n = e.n(node);
        if n.kind == SCALAR {
            out.extend_from_slice(n.value.as_bytes());
            out.push(b'\n');
            return Ok(());
        }
        if n.kind != SEQ {
            return Err(format!("csv encoding only works for arrays, got: {}", n.tag));
        }
        if n.content.is_empty() {
            return Ok(());
        }
        let content = n.content.clone();
        match e.n(content[0]).kind {
            SCALAR => self.encode_row(e, out, &content),
            MAP => {
                let headers: Vec<NodeId> = e.n(content[0]).content.iter().step_by(2).copied().collect();
                if self.encode_row(e, out, &headers).is_err() {
                    return Ok(());
                }
                for (i, c) in content.iter().enumerate() {
                    if e.n(*c).kind != MAP {
                        return Err(format!(
                            "csv object encoding only works for arrays of flat objects (string key => string/numbers/boolean value), child[{i}] is a {}",
                            e.n(*c).tag
                        ));
                    }
                    let mut row = Vec::new();
                    for h in &headers {
                        let cc = e.n(*c).content.clone();
                        let mut found = None;
                        let mut j = 0;
                        while j + 1 < cc.len() {
                            if crate::ops2::recursive_node_equal(e, cc[j], *h) {
                                found = Some(cc[j + 1]);
                                break;
                            }
                            j += 2;
                        }
                        row.push(found.unwrap_or_else(|| e.scalar("!!null", "")));
                    }
                    self.encode_row(e, out, &row)?;
                }
                Ok(())
            }
            _ => {
                for (i, c) in content.iter().enumerate() {
                    if e.n(*c).kind != SEQ {
                        return Err(format!("csv encoding only works for arrays of scalars (string/numbers/booleans), child[{i}] is a {}", e.n(*c).tag));
                    }
                    let cc = e.n(*c).content.clone();
                    self.encode_row(e, out, &cc)?;
                }
                Ok(())
            }
        }
    }
}

// ---- base64 ----

const STD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64_encode(data: &[u8], url: bool) -> String {
    let a = if url { URL } else { STD };
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let v = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(a[(v >> 18) as usize & 63] as char);
        out.push(a[(v >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { a[(v >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { a[v as usize & 63] as char } else { '=' });
    }
    out
}

/// encoding/base64 decoding (with padding), newlines ignored.
pub fn b64_decode(s: &str, url: bool) -> Result<Vec<u8>, String> {
    let a = if url { URL } else { STD };
    let filtered: Vec<u8> = s.bytes().filter(|b| *b != b'\r' && *b != b'\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < filtered.len() {
        let mut vals = [0u32; 4];
        let mut n = 0;
        let mut j = 0;
        while j < 4 {
            if i + j >= filtered.len() {
                if j == 0 {
                    break;
                }
                return Err(format!("illegal base64 data at input byte {}", i + j));
            }
            let c = filtered[i + j];
            if c == b'=' {
                // padding: only in the last two places of the final quantum
                match j {
                    0 | 1 => return Err(format!("illegal base64 data at input byte {}", i + j)),
                    2 => {
                        if i + 3 >= filtered.len() || filtered[i + 3] != b'=' {
                            return Err(format!("illegal base64 data at input byte {}", (i + 3).min(filtered.len())));
                        }
                        if i + 4 < filtered.len() {
                            return Err(format!("illegal base64 data at input byte {}", i + 4));
                        }
                    }
                    _ => {
                        if i + 4 < filtered.len() {
                            return Err(format!("illegal base64 data at input byte {}", i + 4));
                        }
                    }
                }
                break;
            }
            match a.iter().position(|x| *x == c) {
                Some(v) => vals[j] = v as u32,
                None => return Err(format!("illegal base64 data at input byte {}", i + j)),
            }
            n += 1;
            j += 1;
        }
        let v = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        if n >= 2 {
            out.push((v >> 16) as u8);
        }
        if n >= 3 {
            out.push((v >> 8) as u8);
        }
        if n >= 4 {
            out.push(v as u8);
        }
        if n < 4 {
            break;
        }
        i += 4;
    }
    Ok(out)
}

pub struct Base64Decoder {
    url: bool,
    data: String,
    finished: bool,
    read_anything: bool,
}

impl Base64Decoder {
    pub fn new(url: bool) -> Base64Decoder {
        Base64Decoder { url, data: String::new(), finished: false, read_anything: false }
    }
}

impl Decoder for Base64Decoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        let s = String::from_utf8_lossy(&input);
        let mut stripped = crate::util::go_trim_space(&s).to_string();
        let pad = stripped.len() % 4;
        if pad > 0 {
            stripped.push_str(&"=".repeat(4 - pad));
        }
        self.data = stripped;
        self.finished = false;
        self.read_anything = false;
        Ok(())
    }
    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        if self.finished {
            return Ok(None);
        }
        let d = b64_decode(&std::mem::take(&mut self.data), self.url)?;
        if d.is_empty() {
            self.finished = true;
            if self.read_anything {
                return Ok(None);
            }
        }
        self.read_anything = true;
        Ok(Some(e.string_scalar(&String::from_utf8_lossy(&d))))
    }
}

pub struct Base64Encoder {
    pub url: bool,
}

impl Encoder for Base64Encoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        if e.guess_tag_from_custom_type(node) != "!!str" {
            return Err(format!("cannot encode {} as base64, can only operate on strings", e.n(node).tag));
        }
        out.extend_from_slice(b64_encode(e.n(node).value.as_bytes(), self.url).as_bytes());
        Ok(())
    }
}

// ---- URI ----

pub fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub fn query_unescape(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                    let mut rest = &s[i..];
                    if rest.len() > 3 {
                        let mut end = 3;
                        while !rest.is_char_boundary(end) {
                            end += 1;
                        }
                        rest = &rest[..end];
                    }
                    return Err(format!("invalid URL escape {}", crate::util::go_quote(rest)));
                }
                let v = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap();
                out.push(v);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub struct UriDecoder {
    data: Option<Vec<u8>>,
    finished: bool,
    read_anything: bool,
}

impl UriDecoder {
    pub fn new() -> UriDecoder {
        UriDecoder { data: None, finished: false, read_anything: false }
    }
}

impl Decoder for UriDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        self.data = Some(input);
        self.finished = false;
        self.read_anything = false;
        Ok(())
    }
    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        if self.finished {
            return Ok(None);
        }
        let d = self.data.take().unwrap_or_default();
        if d.is_empty() {
            self.finished = true;
            if self.read_anything {
                return Ok(None);
            }
        }
        let v = query_unescape(&String::from_utf8_lossy(&d))?;
        self.read_anything = true;
        Ok(Some(e.string_scalar(&v)))
    }
}

pub struct UriEncoder;

impl Encoder for UriEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        if e.guess_tag_from_custom_type(node) != "!!str" {
            return Err(format!(
                "cannot encode {} as URI, can only operate on strings. Please first pipe through another encoding operator to convert the value to a string",
                e.n(node).tag
            ));
        }
        out.extend_from_slice(query_escape(&e.n(node).value).as_bytes());
        Ok(())
    }
}

// ---- @sh ----

pub struct ShEncoder;

fn sh_unsafe(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || c == '_' || "@%+=:,./-".contains(c))
}

pub fn sh_encode(input: &str) -> String {
    let mut in_quote = false;
    let mut out = String::new();
    for c in input.chars() {
        if c == '\'' {
            if in_quote {
                out.push('\'');
                in_quote = !in_quote;
            }
            out.push('\\');
        } else if sh_unsafe(c) && !in_quote {
            out.push('\'');
            in_quote = !in_quote;
        }
        out.push(c);
    }
    if in_quote {
        out.push('\'');
    }
    out
}

impl Encoder for ShEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        if e.guess_tag_from_custom_type(node) != "!!str" {
            return Err(format!(
                "cannot encode {} as URI, can only operate on strings. Please first pipe through another encoding operator to convert the value to a string",
                e.n(node).tag
            ));
        }
        out.extend_from_slice(sh_encode(&e.n(node).value).as_bytes());
        Ok(())
    }
}

// ---- shell variables ----

pub struct ShellVarsEncoder {
    pub prefs: ShellVarsPrefs,
}

fn alpha_or_underscore(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn alnum_or_underscore(c: char) -> bool {
    alpha_or_underscore(c) || c.is_ascii_digit()
}

fn quote_value(v: &str) -> String {
    if v.chars().any(|c| !alnum_or_underscore(c)) {
        return format!("'{}'", v.replace('\'', "'\"'\"'"));
    }
    v.to_string()
}

impl ShellVarsEncoder {
    fn append_path(&self, cooked: &str, raw: &str) -> String {
        use unicode_normalization::UnicodeNormalization;
        let key: String = raw
            .nfkd()
            .filter_map(|r| {
                if alnum_or_underscore(r) {
                    Some(r)
                } else if (r as u32) < 32 || 126 < (r as u32) {
                    None
                } else {
                    Some('_')
                }
            })
            .collect();
        if cooked.is_empty() {
            let first = key.chars().next().unwrap_or('\u{fffd}');
            if !alpha_or_underscore(first) {
                return format!("_{key}");
            }
            return key;
        }
        format!("{cooked}{}{key}", self.prefs.key_separator)
    }

    fn do_encode(&self, e: &Engine, out: &mut Vec<u8>, node: NodeId, path: &str) -> Res<()> {
        let n = e.n(node);
        match n.kind {
            SCALAR => {
                let p = if path.is_empty() { "value" } else { path };
                let v = if self.prefs.unwrap_scalar { n.value.clone() } else { quote_value(&n.value) };
                out.extend_from_slice(format!("{p}={v}\n").as_bytes());
                Ok(())
            }
            SEQ => {
                for (i, c) in n.content.iter().enumerate() {
                    self.do_encode(e, out, *c, &self.append_path(path, &i.to_string()))?;
                }
                Ok(())
            }
            MAP => {
                let mut i = 0;
                while i + 1 < n.content.len() {
                    let k = e.n(n.content[i]).value.clone();
                    self.do_encode(e, out, n.content[i + 1], &self.append_path(path, &k))?;
                    i += 2;
                }
                Ok(())
            }
            ALIAS => self.do_encode(e, out, n.alias.unwrap(), path),
            _ => Err(format!("unsupported node {}", n.tag)),
        }
    }
}

impl Encoder for ShellVarsEncoder {
    fn can_handle_aliases(&self) -> bool {
        false
    }
    fn print_document_separator(&mut self, _out: &mut Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn print_leading_content(&mut self, _out: &mut Vec<u8>, _content: &str) -> Res<()> {
        Ok(())
    }
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        map_keys_to_strings(e, node);
        self.do_encode(e, out, node, "")
    }
}
