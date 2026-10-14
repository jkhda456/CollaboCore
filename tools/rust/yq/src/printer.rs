//! yq's results printer (document separators, leading content, NUL-separated output, split
//! files) and the operators that encode, decode and load data or read the environment.

use crate::eval::*;
use crate::formats::{decoder_for, encoder_for, Encoder};
use crate::node::*;
use crate::parser::*;
use std::io::Write;

pub enum Writer {
    Stdout(Vec<u8>),
    Buffer(Vec<u8>),
    /// --split-exp: each result into the file the expression names
    Split { exp: Option<Expr>, extension: &'static str, index: usize },
}

pub struct Printer {
    pub encoder: Box<dyn Encoder>,
    pub writer: Writer,
    first_time: bool,
    previous_doc_index: u64,
    previous_file_index: i64,
    printed_matches: bool,
    pub nul_sep: bool,
    pub appendix: Option<Vec<u8>>,
    pub node_info: bool,
    pub out_file: Option<std::fs::File>,
}

impl Printer {
    pub fn new(encoder: Box<dyn Encoder>, writer: Writer) -> Printer {
        Printer {
            encoder,
            writer,
            first_time: true,
            previous_doc_index: 0,
            previous_file_index: 0,
            printed_matches: false,
            nul_sep: false,
            appendix: None,
            node_info: false,
            out_file: None,
        }
    }

    pub fn printed_anything(&self) -> bool {
        self.printed_matches
    }

    fn flush_to(&mut self, bytes: &[u8]) -> Res<()> {
        match &mut self.writer {
            Writer::Stdout(_) => {
                if let Some(f) = &mut self.out_file {
                    f.write_all(bytes).map_err(|e| e.to_string())?;
                } else {
                    let mut so = std::io::stdout().lock();
                    if let Err(e) = so.write_all(bytes) {
                        if e.kind() == std::io::ErrorKind::BrokenPipe {
                            std::process::exit(1);
                        }
                        return Err(e.to_string());
                    }
                    let _ = so.flush();
                }
            }
            Writer::Buffer(b) => b.extend_from_slice(bytes),
            Writer::Split { .. } => {}
        }
        Ok(())
    }

    pub fn print_results(&mut self, e: &mut Engine, nodes: &[NodeId]) -> Res<()> {
        if self.node_info {
            return self.print_node_info(e, nodes);
        }
        if nodes.is_empty() {
            return Ok(());
        }
        let mut nodes = nodes.to_vec();
        if !self.encoder.can_handle_aliases() {
            let explode = expr(Operation::new(Op::Explode), None, None);
            let ctx = e.get_matching_nodes(&Ctx::new(nodes.clone()), Some(&explode))?;
            nodes = ctx.nodes.to_vec();
            // explode with no RHS explodes the nodes themselves
        }
        if self.first_time {
            let n = nodes[0];
            self.previous_doc_index = e.get_document(n);
            self.previous_file_index = e.get_file_index(n);
            self.first_time = false;
        }
        for node in nodes {
            let mut out: Vec<u8> = Vec::new();
            let split_target = self.split_file_for(e, node)?;
            let leading = e.n(node).leading_content.clone();
            let starts_with_sep = leading.starts_with("$yqDocSeparator$");
            if (self.previous_doc_index != e.get_document(node) || self.previous_file_index != e.get_file_index(node)) && !starts_with_sep {
                self.encoder.print_document_separator(&mut out)?;
            }
            let mut dest: Vec<u8> = Vec::new();
            self.encoder.print_leading_content(&mut dest, &leading)?;
            {
                let nn = e.n(node);
                self.printed_matches = self.printed_matches || (nn.tag != "!!null" && (nn.tag != "!!bool" || nn.value != "false"));
            }
            self.encoder.encode(e, &mut dest, node)?;
            if self.nul_sep {
                if dest.ends_with(b"\r\n") {
                    dest.truncate(dest.len() - 2);
                } else if dest.ends_with(b"\n") || dest.ends_with(b"\r") {
                    dest.truncate(dest.len() - 1);
                }
                if dest.contains(&0) {
                    return Err("can't serialise value because it contains NUL char and you are using NUL separated output".into());
                }
                out.extend_from_slice(&dest);
                out.push(0);
            } else {
                out.extend_from_slice(&dest);
            }
            self.previous_doc_index = e.get_document(node);
            match split_target {
                Some(mut f) => {
                    f.write_all(&out).map_err(|e| e.to_string())?;
                }
                None => self.flush_to(&out)?,
            }
        }
        if let Some(app) = self.appendix.take() {
            self.flush_to(&app)?;
        }
        Ok(())
    }

    fn split_file_for(&mut self, e: &mut Engine, node: NodeId) -> Res<Option<std::fs::File>> {
        let Writer::Split { exp, extension, index } = &mut self.writer else { return Ok(None) };
        let mut ctx = Ctx::new(vec![node]);
        let idx = e.scalar("!!int", &index.to_string());
        ctx.set_variable("index", std::rc::Rc::new(vec![idx]));
        let r = e.get_matching_nodes(&ctx, exp.as_ref())?;
        let mut name = r.nodes.first().map(|n| e.n(*n).value.clone()).unwrap_or_default();
        let has_ext = {
            let b = name.as_bytes();
            match name.rfind('.') {
                Some(d) => d + 1 < b.len() && b[d + 1..].iter().all(|c| c.is_ascii_alphanumeric()),
                None => false,
            }
        };
        if !has_ext {
            name = format!("{name}.{extension}");
        }
        let dir = std::path::Path::new(&name).parent().map(|p| p.to_path_buf()).unwrap_or_default();
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {}", dir.display(), crate::main_support::io_err(&e)))?;
        }
        let f = std::fs::File::create(&name).map_err(|err| format!("open {name}: {}", crate::main_support::io_err(&err)))?;
        *index += 1;
        Ok(Some(f))
    }

    fn print_node_info(&mut self, e: &mut Engine, nodes: &[NodeId]) -> Res<()> {
        for n in nodes {
            let info = node_info(e, *n);
            let opts = crate::yaml::serializer::DumpOptions { indent: 4, compact_seq_indent: false, line_width: -1 };
            let mut b = crate::yaml::serializer::dump(&info, opts).map_err(|e| e.to_string())?;
            b.push(b'\n');
            self.printed_matches = true;
            self.flush_to(&b)?;
        }
        if let Some(app) = self.appendix.take() {
            self.flush_to(&app)?;
        }
        Ok(())
    }
}

/// ConvertToNodeInfo, as a YAML node (yaml.Marshal of the NodeInfo struct).
fn node_info(e: &Engine, n: NodeId) -> crate::yaml::Node {
    use crate::yaml::{Node, MAPPING_NODE, SCALAR_NODE, SEQUENCE_NODE};
    let c = e.n(n);
    let mut m = Node { kind: MAPPING_NODE, tag: "!!map".into(), ..Default::default() };
    let mut field = |k: &str, v: Node| {
        m.content.push(Node { kind: SCALAR_NODE, tag: "!!str".into(), value: k.into(), ..Default::default() });
        m.content.push(v);
    };
    let s = |v: &str| Node { kind: SCALAR_NODE, tag: "!!str".into(), value: v.into(), ..Default::default() };
    let kind = match c.kind {
        SEQ => "SequenceNode",
        MAP => "MappingNode",
        SCALAR => "ScalarNode",
        ALIAS => "AliasNode",
        _ => "Unknown",
    };
    field("kind", s(kind));
    let mut styles = Vec::new();
    for (bit, name) in [(TAGGED, "TaggedStyle"), (DOUBLE, "DoubleQuotedStyle"), (SINGLE, "SingleQuotedStyle"), (LITERAL, "LiteralStyle"), (FOLDED, "FoldedStyle"), (FLOW, "FlowStyle")] {
        if c.style & bit != 0 {
            styles.push(name);
        }
    }
    let style = styles.join(",");
    if !style.is_empty() {
        field("style", s(&style));
    }
    if !c.anchor.is_empty() {
        field("anchor", s(&c.anchor));
    }
    if !c.tag.is_empty() {
        field("tag", s(&c.tag));
    }
    if !c.head_comment.is_empty() {
        field("headComment", s(&c.head_comment));
    }
    if !c.line_comment.is_empty() {
        field("lineComment", s(&c.line_comment));
    }
    if !c.foot_comment.is_empty() {
        field("footComment", s(&c.foot_comment));
    }
    if !c.value.is_empty() {
        field("value", s(&c.value));
    }
    if c.line != 0 {
        field("line", Node { kind: SCALAR_NODE, tag: "!!int".into(), value: c.line.to_string(), ..Default::default() });
    }
    if c.column != 0 {
        field("column", Node { kind: SCALAR_NODE, tag: "!!int".into(), value: c.column.to_string(), ..Default::default() });
    }
    if !c.content.is_empty() {
        let seq = Node { kind: SEQUENCE_NODE, tag: "!!seq".into(), content: c.content.iter().map(|x| node_info(e, *x)).collect(), ..Default::default() };
        field("content", seq);
    }
    m
}

/// configureEncoder (operator_encoder_decoder.go): the encoder an encode operation uses.
fn configure_encoder(e: &Engine, format: Fmt, indent: i64) -> Option<Box<dyn Encoder>> {
    match format {
        Fmt::Json => Some(Box::new(crate::formats::json::JsonEncoder { indent, unwrap_scalar: false, colors: false })),
        Fmt::Yaml => {
            let mut p = e.prefs.yaml.clone();
            p.indent = indent;
            p.colors_enabled = false;
            Some(Box::new(crate::formats::yaml::YamlEncoder { prefs: p }))
        }
        Fmt::Xml => crate::formats_extra::xml_encoder_with_indent(e, indent),
        _ => encoder_for(e, format),
    }
}

/// encodeToString
pub fn encode_to_string(e: &mut Engine, node: NodeId, format: Fmt, indent: i64) -> Res<String> {
    let Some(enc) = configure_encoder(e, format, indent) else { return Err("no support for output format".into()) };
    let mut p = Printer::new(enc, Writer::Buffer(Vec::new()));
    p.print_results(e, &[node])?;
    let Writer::Buffer(b) = p.writer else { unreachable!() };
    Ok(String::from_utf8_lossy(&b).into_owned())
}

fn chomp_newlines(s: &str) -> String {
    s.trim_end_matches('\n').to_string()
}

pub fn encode_to_yaml_string(e: &mut Engine, node: NodeId) -> Res<String> {
    let indent = e.prefs.yaml.indent;
    let s = encode_to_string(e, node, Fmt::Yaml, indent)?;
    Ok(chomp_newlines(&s))
}

pub fn encode_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::Encoder { format, indent } = x.op.prefs else { unreachable!() };
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let mut s = encode_to_string(e, *c, format, indent)?;
        let key = format!("decoded: {}", e.get_key(*c));
        if let Some(orig) = ctx.get_variable(&key) {
            // `[^\n].*\n$`: one line, ending with a newline
            let has_only_one_newline = one_line_regex(&s);
            if !orig.is_empty() && has_only_one_newline {
                let ov = e.n(orig[0]).value.clone();
                // `.*\n$`
                if !ends_with_newline_regex(&ov) {
                    s = chomp_newlines(&s);
                }
            }
        }
        if (format == Fmt::Json && indent == 0) || format == Fmt::Csv || format == Fmt::Tsv {
            s = chomp_newlines(&s);
        }
        results.push(e.create_replacement(*c, SCALAR, "!!str", &s));
    }
    Ok(ctx.child(results))
}

/// Go's `[^\n].*\n$` (unanchored at the start: any non-newline followed by the rest of a line
/// that ends the string).
fn one_line_regex(s: &str) -> bool {
    // the match can start anywhere: some line (the last one) has a non-newline char and the
    // string ends with "\n" right after it
    let Some(body) = s.strip_suffix('\n') else { return false };
    match body.rfind('\n') {
        Some(i) => i + 1 < body.len(),
        None => !body.is_empty(),
    }
}

/// `.*\n$`
fn ends_with_newline_regex(s: &str) -> bool {
    s.ends_with('\n')
}

pub fn decode_operator(e: &mut Engine, mut ctx: Ctx, x: &Expr) -> Res<Ctx> {
    let Prefs::Decoder(format) = x.op.prefs else { unreachable!() };
    let Some(mut dec) = decoder_for(e, format) else { return Err("no support for input format".into()) };
    let mut results = Vec::new();
    for c in ctx.nodes.clone().iter() {
        let key = format!("decoded: {}", e.get_key(*c));
        ctx.set_variable(&key, std::rc::Rc::new(vec![*c]));
        let v = e.n(*c).value.clone();
        dec.init(v.into_bytes())?;
        let node = dec.decode(e)?.ok_or_else(|| "EOF".to_string())?;
        e.nm(node).key = e.n(*c).key;
        e.nm(node).parent = e.n(*c).parent;
        results.push(node);
    }
    Ok(ctx.child(results))
}

pub fn read_documents(e: &mut Engine, input: Vec<u8>, filename: &str, file_index: i64, dec: &mut dyn crate::formats::Decoder) -> Res<Vec<NodeId>> {
    dec.init(input)?;
    let mut docs = Vec::new();
    let mut idx = 0u64;
    let fname: std::rc::Rc<str> = filename.into();
    loop {
        match dec.decode(e) {
            Ok(None) => return Ok(docs),
            Ok(Some(n)) => {
                let nn = e.nm(n);
                nn.document = idx;
                nn.filename = fname.clone();
                nn.file_index = file_index;
                nn.evaluate_together = true;
                docs.push(n);
                idx += 1;
            }
            Err(err) => return Err(format!("bad file '{filename}': {err}")),
        }
    }
}

fn first_name(e: &mut Engine, ctx: &Ctx, c: NodeId, x: &Expr) -> Res<String> {
    let r = e.eval(&ctx.single_readonly_child(c), &x.rhs)?;
    match r.nodes.first() {
        Some(n) => Ok(e.n(*n).value.clone()),
        None => Err("filename expression returned nil".into()),
    }
}

pub fn load_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if e.prefs.security.disable_file_ops {
        return Err("file operations have been disabled".into());
    }
    let Prefs::Load(format) = x.op.prefs else { unreachable!() };
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let filename = first_name(e, &ctx, *c, x)?;
        let r = (|| -> Res<NodeId> {
            let mut dec: Box<dyn crate::formats::Decoder> = match format {
                Fmt::Yaml => Box::new(crate::formats::yaml::YamlDecoder::new(crate::prefs::YamlPrefs::load())),
                f => decoder_for(e, f).ok_or_else(|| format!("could not load {filename}"))?,
            };
            let data = std::fs::read(&filename).map_err(|err| format!("open {filename}: {}", crate::main_support::io_err(&err)))?;
            let docs = read_documents(e, data, &filename, 0, dec.as_mut())?;
            Ok(match docs.len() {
                0 => e.add(CNode::new(SCALAR, "!!null")),
                1 => docs[0],
                _ => {
                    let seq = e.add(CNode::new(SEQ, ""));
                    for d in docs {
                        e.add_child(seq, d);
                    }
                    seq
                }
            })
        })()
        .map_err(|err| format!("failed to load {filename}: {err}"))?;
        results.push(r);
    }
    Ok(ctx.child(results))
}

pub fn load_string_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if e.prefs.security.disable_file_ops {
        return Err("file operations have been disabled".into());
    }
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let filename = first_name(e, &ctx, *c, x)?;
        let data = std::fs::read(&filename)
            .map_err(|err| format!("failed to load {filename}: open {filename}: {}", crate::main_support::io_err(&err)))?;
        results.push(e.scalar("!!str", &String::from_utf8_lossy(&data)));
    }
    Ok(ctx.child(results))
}

pub fn env_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if e.prefs.security.disable_env_ops {
        return Err("env operations have been disabled".into());
    }
    let name = e.n(x.op.node.unwrap()).value.clone();
    let raw = std::env::var(&name).unwrap_or_default();
    let Prefs::Env { string_value, .. } = x.op.prefs else { unreachable!() };
    let node = if string_value {
        e.scalar("!!str", &raw)
    } else if raw.is_empty() {
        return Err(format!("value for env variable '{name}' not provided in env()"));
    } else {
        let mut dec = crate::formats::yaml::YamlDecoder::new(e.prefs.yaml.clone());
        crate::formats::Decoder::init(&mut dec, raw.into_bytes())?;
        crate::formats::Decoder::decode(&mut dec, e)?.ok_or_else(|| "EOF".to_string())?
    };
    Ok(ctx.single_child(node))
}

pub fn envsubst_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if e.prefs.security.disable_env_ops {
        return Err("env operations have been disabled".into());
    }
    let (no_unset, no_empty, fail_fast) = match x.op.prefs {
        Prefs::Env { no_unset, no_empty, fail_fast, .. } => (no_unset, no_empty, fail_fast),
        _ => (false, false, false),
    };
    let r = crate::envsubst::Restrictions { no_unset, no_empty };
    let env = |n: &str| std::env::var(n).ok();
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        if e.n(*c).tag != "!!str" {
            crate::util::warn(&format!("EnvSubstOperator, env name: {} {}", e.n(*c).tag, e.n(*c).value));
            return Err(format!(
                "cannot substitute with {}, can only substitute strings. Hint: Most often you'll want to use '|=' over '=' for this operation",
                e.n(*c).tag
            ));
        }
        let v = crate::envsubst::substitute(&e.n(*c).value, &env, &r, fail_fast)?;
        results.push(e.create_replacement(*c, SCALAR, "!!str", &v));
    }
    Ok(ctx.child(results))
}

pub fn system_operator(e: &mut Engine, ctx: Ctx, x: &Expr) -> Res<Ctx> {
    if !e.prefs.security.enable_system_ops {
        return Err("system operations are disabled, use --security-enable-system-operator to enable".into());
    }
    let rhs = x.rhs.as_ref().unwrap();
    let has_args = rhs.op.op == Op::Block;
    let mut results = Vec::new();
    for c in ctx.nodes.iter() {
        let nctx = ctx.single_readonly_child(*c);
        let cmd_exp = if has_args { &rhs.lhs } else { &x.rhs };
        let cmds = e.eval(&nctx, cmd_exp)?;
        let Some(cmd_node) = cmds.nodes.first().copied() else {
            return Err("system operator: command expression returned no results".into());
        };
        if e.n(cmd_node).kind != SCALAR || e.guess_tag_from_custom_type(cmd_node) != "!!str" {
            return Err("system operator: command must be a string scalar".into());
        }
        let command = e.n(cmd_node).value.clone();
        if command.is_empty() {
            return Err("system operator: command must be a non-empty string".into());
        }
        let mut args: Vec<String> = Vec::new();
        if has_args {
            let a = e.eval(&nctx, &rhs.rhs)?;
            if let Some(an) = a.nodes.first() {
                let an = *an;
                let n = e.n(an);
                if n.kind == SEQ {
                    for ch in &n.content {
                        let cn = e.n(*ch);
                        if cn.kind != SCALAR || cn.tag == "!!null" {
                            return Err(format!(
                                "system operator: argument must be a non-null scalar; got kind={} tag={}",
                                cn.kind, cn.tag
                            ));
                        }
                        args.push(cn.value.clone());
                    }
                } else if n.tag != "!!null" {
                    if n.kind != SCALAR {
                        return Err(format!(
                            "system operator: args must be a non-null scalar or sequence of non-null scalars; got kind={} tag={}",
                            n.kind, n.tag
                        ));
                    }
                    args.push(n.value.clone());
                }
            }
        }
        let stdin = encode_to_yaml_string(e, *c)?;
        let out = crate::main_support::run_command(&command, &args, stdin.as_bytes())
            .map_err(|(err, stderr)| {
                let s = String::from_utf8_lossy(&stderr).trim().to_string();
                if s.is_empty() {
                    format!("system command '{command}' failed: {err}")
                } else {
                    format!("system command '{command}' failed: {err}\nstderr: {s}")
                }
            })?;
        let mut result = String::from_utf8_lossy(&out).into_owned();
        if result.ends_with("\r\n") {
            result.truncate(result.len() - 2);
        } else if result.ends_with('\n') {
            result.pop();
        }
        results.push(e.create_replacement(*c, SCALAR, "!!str", &result));
    }
    Ok(ctx.child(results))
}
