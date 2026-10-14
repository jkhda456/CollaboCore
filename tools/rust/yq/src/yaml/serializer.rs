//! The desolver and serializer (go-yaml's desolver.go, serializer.go): a node to events for the
//! emitter, implicit tags dropped where the value would resolve to them anyway.

use super::emitter::{Emitter, QuoteStyle};
use super::resolve::resolve;
use super::*;

/// The options yq's YAML encoder dumps with (WithV3Defaults, then its own).
#[derive(Clone, Copy)]
pub struct DumpOptions {
    pub indent: i64,
    pub compact_seq_indent: bool,
    pub line_width: i64,
}

/// go-yaml's Dumper.Dump of a single (non-document) node, then Close: one document.
pub fn dump(node: &Node, opts: DumpOptions) -> Result<Vec<u8>, DumpError> {
    let mut doc = Node { kind: DOCUMENT_NODE, content: vec![node.clone()], ..Default::default() };
    desolve(&mut doc);
    let mut s = Serializer::new(opts);
    s.emit(Event { typ: EventType::StreamStart, ..Default::default() })?;
    s.node(&doc, "")?;
    s.emitter.set_open_ended(false);
    s.emit(Event { typ: EventType::StreamEnd, ..Default::default() })?;
    Ok(s.emitter.out)
}

fn desolve(n: &mut Node) {
    match n.kind {
        SCALAR_NODE => desolve_scalar(n),
        DOCUMENT_NODE | SEQUENCE_NODE | MAPPING_NODE => {
            if n.style & TAGGED_STYLE == 0 {
                let stag = short_tag(&n.tag);
                match n.kind {
                    MAPPING_NODE if stag == MAP_TAG => n.tag.clear(),
                    SEQUENCE_NODE if stag == SEQ_TAG => n.tag.clear(),
                    DOCUMENT_NODE => n.tag.clear(),
                    _ => {}
                }
            }
            n.content.iter_mut().for_each(desolve);
        }
        _ => {}
    }
}

fn desolve_scalar(n: &mut Node) {
    if n.style & TAGGED_STYLE != 0 || n.tag.is_empty() {
        return;
    }
    let stag = short_tag(&n.tag);
    match stag.as_str() {
        NULL_TAG | BOOL_TAG | STR_TAG | INT_TAG | FLOAT_TAG | TIMESTAMP_TAG => {}
        MERGE_TAG => {
            if n.value == "<<" {
                n.tag.clear();
            }
            return;
        }
        _ => return,
    }
    let rtag = resolve(&n.value);
    if rtag == stag {
        n.tag.clear();
    } else if stag == STR_TAG {
        n.tag.clear();
        if n.style & (SINGLE_QUOTED_STYLE | DOUBLE_QUOTED_STYLE | LITERAL_STYLE | FOLDED_STYLE) == 0 {
            // QuoteLegacy prefers double quotes here
            n.style |= DOUBLE_QUOTED_STYLE;
        }
    } else if stag == FLOAT_TAG || stag == INT_TAG {
        n.tag.clear();
    }
}

struct Serializer {
    emitter: Emitter,
}

fn dump_err(stage: &'static str, message: String) -> DumpError {
    DumpError { stage, message }
}

impl Serializer {
    fn new(opts: DumpOptions) -> Serializer {
        let mut emitter = Emitter::new();
        emitter.compact_sequence_indent = opts.compact_seq_indent;
        emitter.quote_preference = QuoteStyle::Legacy;
        emitter.set_width(opts.line_width);
        emitter.best_indent = if opts.indent == 0 { 2 } else { opts.indent };
        Serializer { emitter }
    }

    fn emit(&mut self, event: Event) -> Result<(), DumpError> {
        self.emitter.emit(event).map_err(|m| dump_err("emitter", m))
    }

    fn node(&mut self, node: &Node, tail: &str) -> Result<(), DumpError> {
        if node.kind == 0 && node.is_zero() {
            return self.emit_scalar("null", "", "", PLAIN_SCALAR_STYLE, "", "", "", "");
        }
        let tag = node.tag.as_str();
        let force_quoting = tag.is_empty()
            && node.kind == SCALAR_NODE
            && node.style & (SINGLE_QUOTED_STYLE | DOUBLE_QUOTED_STYLE | LITERAL_STYLE | FOLDED_STYLE) != 0;
        match node.kind {
            DOCUMENT_NODE => {
                let mut ev = Event { typ: EventType::DocumentStart, implicit: true, ..Default::default() };
                ev.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(ev)?;
                for c in &node.content {
                    self.node(c, "")?;
                }
                let mut ev = Event { typ: EventType::DocumentEnd, implicit: true, ..Default::default() };
                ev.foot_comment = node.foot_comment.as_bytes().to_vec();
                self.emit(ev)
            }
            SEQUENCE_NODE => {
                let style = if node.style & FLOW_STYLE != 0 { FLOW_SEQUENCE_STYLE } else { BLOCK_SEQUENCE_STYLE };
                let mut ev = Event {
                    typ: EventType::SequenceStart,
                    anchor: node.anchor.as_bytes().to_vec(),
                    tag: long_tag(tag).into_bytes(),
                    implicit: tag.is_empty(),
                    style,
                    ..Default::default()
                };
                ev.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(ev)?;
                for c in &node.content {
                    self.node(c, "")?;
                }
                let mut ev = Event { typ: EventType::SequenceEnd, ..Default::default() };
                ev.line_comment = node.line_comment.as_bytes().to_vec();
                ev.foot_comment = node.foot_comment.as_bytes().to_vec();
                self.emit(ev)
            }
            MAPPING_NODE => {
                let style = if node.style & FLOW_STYLE != 0 { FLOW_MAPPING_STYLE } else { BLOCK_MAPPING_STYLE };
                let mut ev = Event {
                    typ: EventType::MappingStart,
                    anchor: node.anchor.as_bytes().to_vec(),
                    tag: long_tag(tag).into_bytes(),
                    implicit: tag.is_empty(),
                    style,
                    ..Default::default()
                };
                ev.tail_comment = tail.as_bytes().to_vec();
                ev.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(ev)?;
                let mut tail = String::new();
                let mut i = 0;
                while i + 1 < node.content.len() {
                    let k = &node.content[i];
                    let foot = k.foot_comment.clone();
                    if !foot.is_empty() {
                        let mut kopy = k.clone();
                        kopy.foot_comment.clear();
                        self.node(&kopy, &tail)?;
                    } else {
                        self.node(k, &tail)?;
                    }
                    tail = foot;
                    self.node(&node.content[i + 1], "")?;
                    i += 2;
                }
                let mut ev = Event { typ: EventType::MappingEnd, ..Default::default() };
                ev.tail_comment = tail.into_bytes();
                ev.line_comment = node.line_comment.as_bytes().to_vec();
                ev.foot_comment = node.foot_comment.as_bytes().to_vec();
                self.emit(ev)
            }
            ALIAS_NODE => {
                let mut ev = Event { typ: EventType::Alias, anchor: node.value.as_bytes().to_vec(), ..Default::default() };
                ev.head_comment = node.head_comment.as_bytes().to_vec();
                ev.line_comment = node.line_comment.as_bytes().to_vec();
                ev.foot_comment = node.foot_comment.as_bytes().to_vec();
                self.emit(ev)
            }
            SCALAR_NODE => {
                let value = node.value.as_str();
                let style = if node.style & DOUBLE_QUOTED_STYLE != 0 {
                    DOUBLE_QUOTED_SCALAR_STYLE
                } else if node.style & SINGLE_QUOTED_STYLE != 0 {
                    SINGLE_QUOTED_SCALAR_STYLE
                } else if node.style & LITERAL_STYLE != 0 {
                    LITERAL_SCALAR_STYLE
                } else if node.style & FOLDED_STYLE != 0 {
                    FOLDED_SCALAR_STYLE
                } else if value.contains('\n') {
                    LITERAL_SCALAR_STYLE
                } else if force_quoting {
                    DOUBLE_QUOTED_SCALAR_STYLE
                } else {
                    PLAIN_SCALAR_STYLE
                };
                self.emit_scalar(value, &node.anchor, tag, style, &node.head_comment, &node.line_comment, &node.foot_comment, tail)
            }
            k => Err(dump_err("serializer", format!("cannot represent node with unknown kind {k}"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_scalar(&mut self, value: &str, anchor: &str, tag: &str, style: u8, head: &str, line: &str, foot: &str, tail: &str) -> Result<(), DumpError> {
        let implicit = tag.is_empty();
        let tag = if implicit { String::new() } else { long_tag(tag) };
        let ev = Event {
            typ: EventType::Scalar,
            anchor: anchor.as_bytes().to_vec(),
            tag: tag.into_bytes(),
            value: value.as_bytes().to_vec(),
            implicit,
            quoted_implicit: implicit,
            style,
            head_comment: head.as_bytes().to_vec(),
            line_comment: line.as_bytes().to_vec(),
            foot_comment: foot.as_bytes().to_vec(),
            tail_comment: tail.as_bytes().to_vec(),
            ..Default::default()
        };
        self.emit(ev)
    }
}
