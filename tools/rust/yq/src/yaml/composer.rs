//! The composer and loader (go-yaml's composer.go, loader.go): events to document nodes, tags
//! resolved, one document per call.

use super::parser::Parser;
use super::resolve::resolve;
use super::*;
use std::collections::HashSet;

type R<T> = Result<T, LoadError>;

pub struct Composer {
    parser: Parser,
    event: Option<Event>,
    anchors: HashSet<String>,
    done_init: bool,
}

fn composer_error(message: String, mark: Mark) -> LoadError {
    LoadError::new(Stage::Composer, message, mark)
}

impl Composer {
    pub fn new(input: &[u8]) -> Composer {
        Composer { parser: Parser::new(input), event: None, anchors: HashSet::new(), done_init: false }
    }

    /// The next document, its tags resolved (go-yaml's Loader.Load into a Node), or None at the
    /// end of the stream.
    pub fn next_document(&mut self) -> R<Option<Node>> {
        let node = self.compose()?;
        Ok(node.map(|mut n| {
            resolve_node(&mut n);
            n
        }))
    }

    fn peek(&mut self) -> R<EventType> {
        if let Some(e) = &self.event {
            return Ok(e.typ);
        }
        match self.parser.parse()? {
            Some(e) => {
                let t = e.typ;
                self.event = Some(e);
                Ok(t)
            }
            None => {
                // go-yaml's Parse returns io.EOF here, which the composer treats as a failure
                // with an empty event; it never gets here for a well-formed stream.
                Err(composer_error("attempted to go past the end of stream; corrupted value?".into(), Mark::default()))
            }
        }
    }

    fn ev(&self) -> &Event {
        self.event.as_ref().unwrap()
    }

    fn expect(&mut self, e: EventType) -> R<()> {
        if self.event.is_none() {
            match self.parser.parse()? {
                Some(ev) => self.event = Some(ev),
                None => return Err(composer_error("attempted to go past the end of stream; corrupted value?".into(), Mark::default())),
            }
        }
        let ev = self.ev();
        let mark = Mark::new(0, ev.start_mark.line, ev.start_mark.column);
        if ev.typ == EventType::StreamEnd {
            return Err(composer_error("attempted to go past the end of stream; corrupted value?".into(), mark));
        }
        if ev.typ != e {
            return Err(composer_error(format!("expected {} event but got {}", e.name(), ev.typ.name()), mark));
        }
        self.event = None;
        Ok(())
    }

    fn init(&mut self) -> R<()> {
        if self.done_init {
            return Ok(());
        }
        self.peek()?;
        self.expect(EventType::StreamStart)?;
        self.done_init = true;
        Ok(())
    }

    fn compose(&mut self) -> R<Option<Node>> {
        self.init()?;
        match self.peek()? {
            EventType::Scalar => Ok(Some(self.scalar()?)),
            EventType::Alias => Ok(Some(self.alias()?)),
            EventType::MappingStart => Ok(Some(self.mapping()?)),
            EventType::SequenceStart => Ok(Some(self.sequence()?)),
            EventType::DocumentStart => Ok(Some(self.document()?)),
            EventType::StreamEnd => Ok(None),
            EventType::TailComment => panic!("internal error: unexpected tail comment event (please report)"),
            other => panic!("internal error: attempted to parse unknown event (please report): {}", other.name()),
        }
    }

    fn compose_child(&mut self) -> R<Node> {
        match self.compose()? {
            Some(n) => Ok(n),
            None => Err(composer_error("attempted to go past the end of stream; corrupted value?".into(), Mark::default())),
        }
    }

    fn node(&self, kind: u32, tag: &[u8], value: String) -> Node {
        let ev = self.ev();
        let mut tag = String::from_utf8_lossy(tag).into_owned();
        let mut style = 0;
        if !tag.is_empty() && tag != "!" {
            tag = short_tag(&tag);
            style = TAGGED_STYLE;
        }
        Node {
            kind,
            tag,
            value,
            style,
            line: ev.start_mark.line,
            column: ev.start_mark.column,
            head_comment: String::from_utf8_lossy(&ev.head_comment).into_owned(),
            line_comment: String::from_utf8_lossy(&ev.line_comment).into_owned(),
            foot_comment: String::from_utf8_lossy(&ev.foot_comment).into_owned(),
            ..Default::default()
        }
    }

    fn document(&mut self) -> R<Node> {
        let mut n = self.node(DOCUMENT_NODE, b"", String::new());
        self.expect(EventType::DocumentStart)?;
        let child = self.compose_child()?;
        n.content.push(child);
        if self.peek()? == EventType::DocumentEnd {
            n.foot_comment = String::from_utf8_lossy(&self.ev().foot_comment).into_owned();
        }
        self.expect(EventType::DocumentEnd)?;
        Ok(n)
    }

    fn alias(&mut self) -> R<Node> {
        let name = String::from_utf8_lossy(&self.ev().anchor).into_owned();
        let mut n = self.node(ALIAS_NODE, b"", name.clone());
        if !self.anchors.contains(&name) {
            return Err(composer_error(format!("unknown anchor '{name}' referenced"), Mark::new(0, n.line, n.column)));
        }
        n.alias = Some(name);
        self.expect(EventType::Alias)?;
        Ok(n)
    }

    fn scalar(&mut self) -> R<Node> {
        let ev = self.ev();
        let parsed = ev.style;
        let node_style = if parsed & DOUBLE_QUOTED_SCALAR_STYLE != 0 {
            DOUBLE_QUOTED_STYLE
        } else if parsed & SINGLE_QUOTED_SCALAR_STYLE != 0 {
            SINGLE_QUOTED_STYLE
        } else if parsed & LITERAL_SCALAR_STYLE != 0 {
            LITERAL_STYLE
        } else if parsed & FOLDED_SCALAR_STYLE != 0 {
            FOLDED_STYLE
        } else {
            0
        };
        let value = String::from_utf8_lossy(&ev.value).into_owned();
        let tag = ev.tag.clone();
        let anchor = ev.anchor.clone();
        let mut n = self.node(SCALAR_NODE, &tag, value);
        n.style |= node_style;
        self.anchor(&mut n, &anchor);
        self.expect(EventType::Scalar)?;
        Ok(n)
    }

    fn anchor(&mut self, n: &mut Node, anchor: &[u8]) {
        if !anchor.is_empty() {
            n.anchor = String::from_utf8_lossy(anchor).into_owned();
            self.anchors.insert(n.anchor.clone());
        }
    }

    fn sequence(&mut self) -> R<Node> {
        let ev = self.ev();
        let tag = ev.tag.clone();
        let anchor = ev.anchor.clone();
        let flow = ev.style & FLOW_SEQUENCE_STYLE != 0;
        let mut n = self.node(SEQUENCE_NODE, &tag, String::new());
        if flow {
            n.style |= FLOW_STYLE;
        }
        self.anchor(&mut n, &anchor);
        self.expect(EventType::SequenceStart)?;
        while self.peek()? != EventType::SequenceEnd {
            let child = self.compose_child()?;
            n.content.push(child);
        }
        n.line_comment = String::from_utf8_lossy(&self.ev().line_comment).into_owned();
        n.foot_comment = String::from_utf8_lossy(&self.ev().foot_comment).into_owned();
        self.expect(EventType::SequenceEnd)?;
        Ok(n)
    }

    fn mapping(&mut self) -> R<Node> {
        let ev = self.ev();
        let tag = ev.tag.clone();
        let anchor = ev.anchor.clone();
        let block = ev.style & FLOW_MAPPING_STYLE == 0;
        let mut n = self.node(MAPPING_NODE, &tag, String::new());
        if !block {
            n.style |= FLOW_STYLE;
        }
        self.anchor(&mut n, &anchor);
        self.expect(EventType::MappingStart)?;
        while self.peek()? != EventType::MappingEnd {
            let mut k = self.compose_child()?;
            if block && !k.foot_comment.is_empty() && n.content.len() >= 2 {
                // (go-yaml checks len > 2 after appending the key)
                let len = n.content.len();
                n.content[len - 2].foot_comment = std::mem::take(&mut k.foot_comment);
            }
            let mut v = self.compose_child()?;
            if k.foot_comment.is_empty() && !v.foot_comment.is_empty() {
                k.foot_comment = std::mem::take(&mut v.foot_comment);
            }
            if self.peek()? == EventType::TailComment {
                if k.foot_comment.is_empty() {
                    k.foot_comment = String::from_utf8_lossy(&self.ev().foot_comment).into_owned();
                }
                self.expect(EventType::TailComment)?;
            }
            n.content.push(k);
            n.content.push(v);
        }
        n.line_comment = String::from_utf8_lossy(&self.ev().line_comment).into_owned();
        n.foot_comment = String::from_utf8_lossy(&self.ev().foot_comment).into_owned();
        if n.style & FLOW_STYLE == 0 && !n.foot_comment.is_empty() && n.content.len() > 1 {
            let len = n.content.len();
            n.content[len - 2].foot_comment = std::mem::take(&mut n.foot_comment);
        }
        self.expect(EventType::MappingEnd)?;
        Ok(n)
    }
}

/// go-yaml's Resolver: the implicit tags of a composed document.
fn resolve_node(n: &mut Node) {
    match n.kind {
        SCALAR_NODE => {
            if n.tag.is_empty() {
                if n.style & (SINGLE_QUOTED_STYLE | DOUBLE_QUOTED_STYLE | LITERAL_STYLE | FOLDED_STYLE) != 0 {
                    n.tag = STR_TAG.into();
                } else {
                    n.tag = resolve(&n.value).to_string();
                }
            }
        }
        SEQUENCE_NODE => {
            if n.tag.is_empty() {
                n.tag = SEQ_TAG.into();
            }
            n.content.iter_mut().for_each(resolve_node);
        }
        MAPPING_NODE => {
            if n.tag.is_empty() {
                n.tag = MAP_TAG.into();
            }
            n.content.iter_mut().for_each(resolve_node);
        }
        DOCUMENT_NODE => n.content.iter_mut().for_each(resolve_node),
        _ => {}
    }
}
