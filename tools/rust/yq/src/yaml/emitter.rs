//! The emitter (go-yaml's emitter.go): events to YAML text.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    FlowSequenceFirstItem,
    FlowSequenceTrailItem,
    FlowSequenceItem,
    FlowMappingFirstKey,
    FlowMappingTrailKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    BlockSequenceFirstItem,
    BlockSequenceItem,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    Single,
    Double,
    Legacy,
}

type R = Result<(), String>;

pub struct Emitter {
    pub out: Vec<u8>,
    canonical: bool,
    pub best_indent: i64,
    best_width: i64,
    unicode: bool,
    pub quote_preference: QuoteStyle,
    pub compact_sequence_indent: bool,
    state: State,
    states: Vec<State>,
    events: Vec<Event>,
    events_head: usize,
    indents: Vec<i64>,
    tag_directives: Vec<TagDirective>,
    indent: i64,
    flow_level: i64,
    root_context: bool,
    sequence_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    line: i64,
    column: i64,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
    space_above: bool,
    foot_indent: i64,
    anchor: Option<Vec<u8>>,
    anchor_alias: bool,
    tag_handle: Vec<u8>,
    tag_suffix: Vec<u8>,
    sc_value: Vec<u8>,
    sc_multiline: bool,
    sc_flow_plain_allowed: bool,
    sc_block_plain_allowed: bool,
    sc_single_quoted_allowed: bool,
    sc_block_allowed: bool,
    sc_style: u8,
    head_comment: Vec<u8>,
    line_comment: Vec<u8>,
    foot_comment: Vec<u8>,
    tail_comment: Vec<u8>,
    key_line_comment: Vec<u8>,
    line_break: &'static [u8],
}

fn default_tag_directives() -> [TagDirective; 2] {
    [
        TagDirective { handle: b"!".to_vec(), prefix: b"!".to_vec() },
        TagDirective { handle: b"!!".to_vec(), prefix: b"tag:yaml.org,2002:".to_vec() },
    ]
}

impl Emitter {
    pub fn new() -> Emitter {
        Emitter {
            out: Vec::new(),
            canonical: false,
            best_indent: 2,
            best_width: -1,
            unicode: true,
            quote_preference: QuoteStyle::Legacy,
            compact_sequence_indent: false,
            state: State::StreamStart,
            states: Vec::new(),
            events: Vec::new(),
            events_head: 0,
            indents: Vec::new(),
            tag_directives: Vec::new(),
            indent: 0,
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            line: 0,
            column: 0,
            whitespace: false,
            indention: false,
            open_ended: false,
            space_above: false,
            foot_indent: 0,
            anchor: None,
            anchor_alias: false,
            tag_handle: Vec::new(),
            tag_suffix: Vec::new(),
            sc_value: Vec::new(),
            sc_multiline: false,
            sc_flow_plain_allowed: false,
            sc_block_plain_allowed: false,
            sc_single_quoted_allowed: false,
            sc_block_allowed: false,
            sc_style: 0,
            head_comment: Vec::new(),
            line_comment: Vec::new(),
            foot_comment: Vec::new(),
            tail_comment: Vec::new(),
            key_line_comment: Vec::new(),
            line_break: b"\n",
        }
    }

    pub fn set_width(&mut self, width: i64) {
        self.best_width = if width < 0 { -1 } else { width };
    }

    pub fn set_open_ended(&mut self, v: bool) {
        self.open_ended = v;
    }

    pub fn emit(&mut self, event: Event) -> R {
        self.events.push(event);
        while !self.need_more_events() {
            let event = std::mem::take(&mut self.events[self.events_head]);
            // keep the type for lookahead checks done while handling it
            self.events[self.events_head].typ = event.typ;
            self.analyze_event(&event)?;
            self.state_machine(&event)?;
            self.events_head += 1;
        }
        if self.events_head == self.events.len() {
            self.events.clear();
            self.events_head = 0;
        }
        Ok(())
    }

    fn need_more_events(&self) -> bool {
        if self.events_head == self.events.len() {
            return true;
        }
        let accumulate = match self.events[self.events_head].typ {
            EventType::DocumentStart => 1,
            EventType::SequenceStart => 2,
            EventType::MappingStart => 3,
            _ => return false,
        };
        if self.events.len() - self.events_head > accumulate {
            return false;
        }
        let mut level = 0i64;
        for e in &self.events[self.events_head..] {
            match e.typ {
                EventType::StreamStart | EventType::DocumentStart | EventType::SequenceStart | EventType::MappingStart => level += 1,
                EventType::StreamEnd | EventType::DocumentEnd | EventType::SequenceEnd | EventType::MappingEnd => level -= 1,
                _ => {}
            }
            if level == 0 {
                return false;
            }
        }
        true
    }

    fn state_machine(&mut self, event: &Event) -> R {
        match self.state {
            State::StreamStart => self.emit_stream_start(event),
            State::FirstDocumentStart => self.emit_document_start(event, true),
            State::DocumentStart => self.emit_document_start(event, false),
            State::DocumentContent => self.emit_document_content(event),
            State::DocumentEnd => self.emit_document_end(event),
            State::FlowSequenceFirstItem => self.emit_flow_sequence_item(event, true, false),
            State::FlowSequenceTrailItem => self.emit_flow_sequence_item(event, false, true),
            State::FlowSequenceItem => self.emit_flow_sequence_item(event, false, false),
            State::FlowMappingFirstKey => self.emit_flow_mapping_key(event, true, false),
            State::FlowMappingTrailKey => self.emit_flow_mapping_key(event, false, true),
            State::FlowMappingKey => self.emit_flow_mapping_key(event, false, false),
            State::FlowMappingSimpleValue => self.emit_flow_mapping_value(event, true),
            State::FlowMappingValue => self.emit_flow_mapping_value(event, false),
            State::BlockSequenceFirstItem => self.emit_block_sequence_item(event, true),
            State::BlockSequenceItem => self.emit_block_sequence_item(event, false),
            State::BlockMappingFirstKey => self.emit_block_mapping_key(event, true),
            State::BlockMappingKey => self.emit_block_mapping_key(event, false),
            State::BlockMappingSimpleValue => self.emit_block_mapping_value(event, true),
            State::BlockMappingValue => self.emit_block_mapping_value(event, false),
            State::End => Err("expected nothing after STREAM-END".into()),
        }
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }
    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().unwrap_or(-1);
    }

    fn emit_stream_start(&mut self, event: &Event) -> R {
        if event.typ != EventType::StreamStart {
            return Err("expected STREAM-START".into());
        }
        if self.best_indent < 2 || self.best_indent > 9 {
            self.best_indent = 2;
        }
        if self.best_width >= 0 && self.best_width <= self.best_indent * 2 {
            self.best_width = 80;
        }
        if self.best_width < 0 {
            self.best_width = (1i64 << 31) - 1;
        }
        self.indent = -1;
        self.line = 0;
        self.column = 0;
        self.whitespace = true;
        self.indention = true;
        self.space_above = true;
        self.foot_indent = -1;
        self.state = State::FirstDocumentStart;
        Ok(())
    }

    fn emit_document_start(&mut self, event: &Event, first: bool) -> R {
        if event.typ == EventType::DocumentStart {
            for d in &event.tag_directives {
                self.append_tag_directive(d.clone(), false)?;
            }
            for d in default_tag_directives() {
                self.append_tag_directive(d, true)?;
            }
            let mut implicit = event.implicit;
            if !first || self.canonical {
                implicit = false;
            }
            if self.open_ended && (event.version_directive.is_some() || !event.tag_directives.is_empty()) {
                self.write_indicator(b"...", true, false, false)?;
                self.write_indent()?;
            }
            if event.version_directive.is_some() {
                implicit = false;
                self.write_indicator(b"%YAML", true, false, false)?;
                self.write_indicator(b"1.1", true, false, false)?;
                self.write_indent()?;
            }
            if !event.tag_directives.is_empty() {
                implicit = false;
                for d in &event.tag_directives {
                    self.write_indicator(b"%TAG", true, false, false)?;
                    self.write_tag_handle(&d.handle)?;
                    self.write_tag_content(&d.prefix, true)?;
                    self.write_indent()?;
                }
            }
            if !implicit {
                self.write_indent()?;
                self.write_indicator(b"---", true, false, false)?;
                self.write_indent()?;
            }
            if !self.head_comment.is_empty() {
                self.process_head_comment()?;
                self.put_line_break()?;
            }
            self.state = State::DocumentContent;
            return Ok(());
        }
        if event.typ == EventType::StreamEnd {
            if self.open_ended {
                self.write_indicator(b"...", true, false, false)?;
                self.write_indent()?;
            }
            self.state = State::End;
            return Ok(());
        }
        Err("expected DOCUMENT-START or STREAM-END".into())
    }

    fn emit_document_content(&mut self, event: &Event) -> R {
        self.states.push(State::DocumentEnd);
        self.process_head_comment()?;
        self.emit_node(event, true, false, false, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    fn emit_document_end(&mut self, event: &Event) -> R {
        if event.typ != EventType::DocumentEnd {
            return Err("expected DOCUMENT-END".into());
        }
        self.foot_indent = 0;
        self.process_foot_comment()?;
        self.foot_indent = -1;
        self.write_indent()?;
        if !event.implicit {
            self.write_indicator(b"...", true, false, false)?;
            self.write_indent()?;
        }
        self.state = State::DocumentStart;
        self.tag_directives.clear();
        Ok(())
    }

    fn comments_pending(&self) -> bool {
        self.line_comment.len() + self.foot_comment.len() + self.tail_comment.len() > 0
    }

    fn emit_flow_sequence_item(&mut self, event: &Event, first: bool, trail: bool) -> R {
        if first {
            self.write_indicator(b"[", true, true, false)?;
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if event.typ == EventType::SequenceEnd {
            if self.canonical && !first && !trail {
                self.write_indicator(b",", false, false, false)?;
            }
            self.flow_level -= 1;
            self.pop_indent();
            if self.column == 0 || self.canonical && !first {
                self.write_indent()?;
            }
            self.write_indicator(b"]", false, false, false)?;
            self.process_line_comment()?;
            self.process_foot_comment()?;
            self.pop_state();
            return Ok(());
        }
        if !first && !trail {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_head_comment()?;
        if self.column == 0 {
            self.write_indent()?;
        }
        if self.canonical || self.column > self.best_width {
            self.write_indent()?;
        }
        if self.comments_pending() {
            self.states.push(State::FlowSequenceTrailItem);
        } else {
            self.states.push(State::FlowSequenceItem);
        }
        self.emit_node(event, false, true, false, false)?;
        if self.comments_pending() {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    fn emit_flow_mapping_key(&mut self, event: &Event, first: bool, trail: bool) -> R {
        if first {
            self.write_indicator(b"{", true, true, false)?;
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if event.typ == EventType::MappingEnd {
            if (self.canonical || self.head_comment.len() + self.foot_comment.len() + self.tail_comment.len() > 0) && !first && !trail {
                self.write_indicator(b",", false, false, false)?;
            }
            self.process_head_comment()?;
            self.flow_level -= 1;
            self.pop_indent();
            if self.canonical && !first {
                self.write_indent()?;
            }
            self.write_indicator(b"}", false, false, false)?;
            self.process_line_comment()?;
            self.process_foot_comment()?;
            self.pop_state();
            return Ok(());
        }
        if !first && !trail {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_head_comment()?;
        if self.column == 0 {
            self.write_indent()?;
        }
        if self.canonical || self.column > self.best_width {
            self.write_indent()?;
        }
        if !self.canonical && self.check_simple_key() {
            self.states.push(State::FlowMappingSimpleValue);
            return self.emit_node(event, false, false, true, true);
        }
        self.write_indicator(b"?", true, false, false)?;
        self.states.push(State::FlowMappingValue);
        self.emit_node(event, false, false, true, false)
    }

    fn emit_flow_mapping_value(&mut self, event: &Event, simple: bool) -> R {
        if simple {
            self.write_indicator(b":", false, false, false)?;
        } else {
            if self.canonical || self.column > self.best_width {
                self.write_indent()?;
            }
            self.write_indicator(b":", true, false, false)?;
        }
        if self.comments_pending() {
            self.states.push(State::FlowMappingTrailKey);
        } else {
            self.states.push(State::FlowMappingKey);
        }
        self.emit_node(event, false, false, true, false)?;
        if self.comments_pending() {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    fn emit_block_sequence_item(&mut self, event: &Event, first: bool) -> R {
        if first {
            let seq = self.mapping_context && (self.column == 0 || !self.indention) && self.compact_sequence_indent;
            self.increase_indent_compact(false, false, seq);
        }
        if event.typ == EventType::SequenceEnd {
            self.pop_indent();
            self.pop_state();
            return Ok(());
        }
        self.process_head_comment()?;
        self.write_indent()?;
        self.write_indicator(b"-", true, false, true)?;
        self.states.push(State::BlockSequenceItem);
        self.emit_node(event, false, true, false, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    fn emit_block_mapping_key(&mut self, event: &Event, first: bool) -> R {
        if first {
            self.increase_indent(false, false);
        }
        self.process_head_comment()?;
        if event.typ == EventType::MappingEnd {
            self.pop_indent();
            self.pop_state();
            return Ok(());
        }
        self.write_indent()?;
        if !self.line_comment.is_empty() {
            self.key_line_comment = std::mem::take(&mut self.line_comment);
        }
        if self.check_simple_key() {
            self.states.push(State::BlockMappingSimpleValue);
            self.emit_node(event, false, false, true, true)?;
            if event.typ == EventType::Alias {
                return self.put(b' ');
            }
            return Ok(());
        }
        self.write_indicator(b"?", true, false, true)?;
        self.states.push(State::BlockMappingValue);
        self.emit_node(event, false, false, true, false)
    }

    fn emit_block_mapping_value(&mut self, event: &Event, simple: bool) -> R {
        if simple {
            self.write_indicator(b":", false, false, false)?;
        } else {
            self.write_indent()?;
            self.write_indicator(b":", true, false, true)?;
        }
        if !self.key_line_comment.is_empty() {
            if event.typ == EventType::Scalar {
                if self.line_comment.is_empty() {
                    self.line_comment = std::mem::take(&mut self.key_line_comment);
                }
            } else if event.style != FLOW_SEQUENCE_STYLE && (event.typ == EventType::MappingStart || event.typ == EventType::SequenceStart) {
                std::mem::swap(&mut self.line_comment, &mut self.key_line_comment);
                self.process_line_comment()?;
                std::mem::swap(&mut self.line_comment, &mut self.key_line_comment);
            }
        }
        self.states.push(State::BlockMappingKey);
        self.emit_node(event, false, false, true, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    fn emit_node(&mut self, event: &Event, root: bool, sequence: bool, mapping: bool, simple_key: bool) -> R {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match event.typ {
            EventType::Alias => self.emit_alias(),
            EventType::Scalar => self.emit_scalar(event),
            EventType::SequenceStart => self.emit_sequence_start(event),
            EventType::MappingStart => self.emit_mapping_start(event),
            other => Err(format!("expected SCALAR, SEQUENCE-START, MAPPING-START, or ALIAS, but got {}", other.name())),
        }
    }

    fn emit_alias(&mut self) -> R {
        self.process_anchor()?;
        self.pop_state();
        Ok(())
    }

    fn emit_scalar(&mut self, event: &Event) -> R {
        self.select_scalar_style(event)?;
        self.process_anchor()?;
        self.process_tag()?;
        self.increase_indent(true, false);
        self.process_scalar()?;
        self.pop_indent();
        self.pop_state();
        Ok(())
    }

    fn emit_sequence_start(&mut self, event: &Event) -> R {
        self.process_anchor()?;
        self.process_tag()?;
        if self.flow_level > 0 || self.canonical || event.style == FLOW_SEQUENCE_STYLE || self.check_empty_sequence() {
            self.state = State::FlowSequenceFirstItem;
        } else {
            self.state = State::BlockSequenceFirstItem;
        }
        Ok(())
    }

    fn emit_mapping_start(&mut self, event: &Event) -> R {
        self.process_anchor()?;
        self.process_tag()?;
        if self.flow_level > 0 || self.canonical || event.style == FLOW_MAPPING_STYLE || self.check_empty_mapping() {
            self.state = State::FlowMappingFirstKey;
        } else {
            self.state = State::BlockMappingFirstKey;
        }
        Ok(())
    }

    fn check_empty_sequence(&self) -> bool {
        if self.events.len() - self.events_head < 2 {
            return false;
        }
        self.events[self.events_head].typ == EventType::SequenceStart && self.events[self.events_head + 1].typ == EventType::SequenceEnd
    }

    fn check_empty_mapping(&self) -> bool {
        if self.events.len() - self.events_head < 2 {
            return false;
        }
        self.events[self.events_head].typ == EventType::MappingStart && self.events[self.events_head + 1].typ == EventType::MappingEnd
    }

    fn check_simple_key(&self) -> bool {
        let anchor_len = self.anchor.as_ref().map_or(0, |a| a.len());
        let length = match self.events[self.events_head].typ {
            EventType::Alias => anchor_len,
            EventType::Scalar => {
                if self.sc_multiline {
                    return false;
                }
                anchor_len + self.tag_handle.len() + self.tag_suffix.len() + self.sc_value.len()
            }
            EventType::SequenceStart => {
                if !self.check_empty_sequence() {
                    return false;
                }
                anchor_len + self.tag_handle.len() + self.tag_suffix.len()
            }
            EventType::MappingStart => {
                if !self.check_empty_mapping() {
                    return false;
                }
                anchor_len + self.tag_handle.len() + self.tag_suffix.len()
            }
            _ => return false,
        };
        length <= 128
    }

    fn process_anchor(&mut self) -> R {
        let Some(anchor) = self.anchor.clone() else { return Ok(()) };
        let c: &[u8] = if self.anchor_alias { b"*" } else { b"&" };
        self.write_indicator(c, true, false, false)?;
        self.write_anchor(&anchor)
    }

    fn process_tag(&mut self) -> R {
        if self.tag_handle.is_empty() && self.tag_suffix.is_empty() {
            return Ok(());
        }
        if !self.tag_handle.is_empty() {
            let h = self.tag_handle.clone();
            self.write_tag_handle(&h)?;
            if !self.tag_suffix.is_empty() {
                let s = self.tag_suffix.clone();
                self.write_tag_content(&s, false)?;
            }
        } else {
            self.write_indicator(b"!<", true, false, false)?;
            let s = self.tag_suffix.clone();
            self.write_tag_content(&s, false)?;
            self.write_indicator(b">", false, false, false)?;
        }
        Ok(())
    }

    fn process_scalar(&mut self) -> R {
        let value = std::mem::take(&mut self.sc_value);
        let r = match self.sc_style {
            PLAIN_SCALAR_STYLE => self.write_plain_scalar(&value, !self.simple_key_context),
            SINGLE_QUOTED_SCALAR_STYLE => self.write_single_quoted_scalar(&value, !self.simple_key_context),
            DOUBLE_QUOTED_SCALAR_STYLE => self.write_double_quoted_scalar(&value, !self.simple_key_context),
            LITERAL_SCALAR_STYLE => self.write_literal_scalar(&value),
            FOLDED_SCALAR_STYLE => self.write_folded_scalar(&value),
            _ => panic!("unknown scalar style"),
        };
        self.sc_value = value;
        r
    }

    fn process_head_comment(&mut self) -> R {
        if !self.tail_comment.is_empty() {
            self.write_indent()?;
            let c = std::mem::take(&mut self.tail_comment);
            self.write_comment(&c)?;
            self.foot_indent = self.indent.max(0);
        }
        if self.head_comment.is_empty() {
            return Ok(());
        }
        self.write_indent()?;
        let c = std::mem::take(&mut self.head_comment);
        self.write_comment(&c)
    }

    fn process_line_comment(&mut self) -> R {
        self.process_line_comment_linebreak(false)
    }

    fn process_line_comment_linebreak(&mut self, linebreak: bool) -> R {
        if self.line_comment.is_empty() {
            if linebreak {
                self.put_line_break()?;
            }
            return Ok(());
        }
        if !self.whitespace {
            self.put(b' ')?;
        }
        let c = std::mem::take(&mut self.line_comment);
        self.write_comment(&c)
    }

    fn process_foot_comment(&mut self) -> R {
        if self.foot_comment.is_empty() {
            return Ok(());
        }
        self.write_indent()?;
        let c = std::mem::take(&mut self.foot_comment);
        self.write_comment(&c)?;
        self.foot_indent = self.indent.max(0);
        Ok(())
    }

    fn analyze_anchor(&mut self, anchor: &[u8], alias: bool) -> R {
        if anchor.is_empty() {
            return Err(if alias { "alias value must not be empty" } else { "anchor value must not be empty" }.into());
        }
        let bom = is_bom_at_start(anchor);
        let mut i = 0;
        while i < anchor.len() {
            if !is_anchor_char_with(anchor, i, bom) {
                return Err(if alias {
                    "alias value must contain valid characters only"
                } else {
                    "anchor value must contain valid characters only"
                }
                .into());
            }
            i += width(anchor[i]).max(1);
        }
        self.anchor = Some(anchor.to_vec());
        self.anchor_alias = alias;
        Ok(())
    }

    fn analyze_tag(&mut self, tag: &[u8]) -> R {
        if tag.is_empty() {
            return Err("tag value must not be empty".into());
        }
        for d in &self.tag_directives {
            if tag.starts_with(&d.prefix) {
                self.tag_handle = d.handle.clone();
                self.tag_suffix = tag[d.prefix.len()..].to_vec();
                return Ok(());
            }
        }
        self.tag_suffix = tag.to_vec();
        Ok(())
    }

    fn analyze_scalar(&mut self, value: &[u8]) {
        self.sc_value = value.to_vec();
        if value.is_empty() {
            self.sc_multiline = false;
            self.sc_flow_plain_allowed = false;
            self.sc_block_plain_allowed = true;
            self.sc_single_quoted_allowed = true;
            self.sc_block_allowed = false;
            return;
        }
        let (mut block_indicators, mut flow_indicators, mut line_breaks, mut special_characters, mut tab_characters) =
            (false, false, false, false, false);
        let (mut leading_space, mut leading_break, mut trailing_space, mut trailing_break) = (false, false, false, false);
        let (mut break_space, mut space_break, mut previous_space, mut previous_break) = (false, false, false, false);
        if value.len() >= 3 && ((value[0] == b'-' && value[1] == b'-' && value[2] == b'-') || (value[0] == b'.' && value[1] == b'.' && value[2] == b'.')) {
            block_indicators = true;
            flow_indicators = true;
        }
        let mut preceded_by_whitespace = true;
        let mut i = 0;
        while i < value.len() {
            let w = width(value[i]).max(1);
            let followed_by_whitespace = i + w >= value.len() || is_blank(value, i + w);
            if i == 0 {
                match value[i] {
                    b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>' | b'\'' | b'"' | b'%' | b'@' | b'`' => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    b'?' | b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'-' => {
                        if followed_by_whitespace {
                            flow_indicators = true;
                            block_indicators = true;
                        }
                    }
                    _ => {}
                }
            } else {
                match value[i] {
                    b',' | b'?' | b'[' | b']' | b'{' | b'}' => flow_indicators = true,
                    b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'#' => {
                        if preceded_by_whitespace {
                            flow_indicators = true;
                            block_indicators = true;
                        }
                    }
                    _ => {}
                }
            }
            if value[i] == b'\t' {
                tab_characters = true;
            } else if !is_printable(value, i) || !is_ascii(value, i) && !self.unicode {
                special_characters = true;
            }
            if is_space(value, i) {
                if i == 0 {
                    leading_space = true;
                }
                if i + width(value[i]) == value.len() {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_line_break(value, i) {
                line_breaks = true;
                if i == 0 {
                    leading_break = true;
                }
                if i + width(value[i]) == value.len() {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }
            preceded_by_whitespace = is_blankz(value, i);
            i += w;
        }
        self.sc_multiline = line_breaks;
        self.sc_flow_plain_allowed = true;
        self.sc_block_plain_allowed = true;
        self.sc_single_quoted_allowed = true;
        self.sc_block_allowed = true;
        if leading_space || leading_break || trailing_space || trailing_break {
            self.sc_flow_plain_allowed = false;
            self.sc_block_plain_allowed = false;
        }
        if trailing_space {
            self.sc_block_allowed = false;
        }
        if break_space {
            self.sc_flow_plain_allowed = false;
            self.sc_block_plain_allowed = false;
            self.sc_single_quoted_allowed = false;
        }
        if space_break || tab_characters || special_characters {
            self.sc_flow_plain_allowed = false;
            self.sc_block_plain_allowed = false;
            self.sc_single_quoted_allowed = false;
        }
        if space_break || special_characters {
            self.sc_block_allowed = false;
        }
        if line_breaks {
            self.sc_flow_plain_allowed = false;
            self.sc_block_plain_allowed = false;
        }
        if flow_indicators {
            self.sc_flow_plain_allowed = false;
        }
        if block_indicators {
            self.sc_block_plain_allowed = false;
        }
    }

    fn analyze_event(&mut self, event: &Event) -> R {
        self.anchor = None;
        self.tag_handle.clear();
        self.tag_suffix.clear();
        self.sc_value.clear();
        if !event.head_comment.is_empty() {
            self.head_comment = event.head_comment.clone();
        }
        if !event.line_comment.is_empty() {
            self.line_comment = event.line_comment.clone();
        }
        if !event.foot_comment.is_empty() {
            self.foot_comment = event.foot_comment.clone();
        }
        if !event.tail_comment.is_empty() {
            self.tail_comment = event.tail_comment.clone();
        }
        match event.typ {
            EventType::Alias => self.analyze_anchor(&event.anchor, true)?,
            EventType::Scalar => {
                if !event.anchor.is_empty() {
                    self.analyze_anchor(&event.anchor, false)?;
                }
                if !event.tag.is_empty() && (self.canonical || (!event.implicit && !event.quoted_implicit)) {
                    self.analyze_tag(&event.tag)?;
                }
                self.analyze_scalar(&event.value);
            }
            EventType::SequenceStart | EventType::MappingStart => {
                if !event.anchor.is_empty() {
                    self.analyze_anchor(&event.anchor, false)?;
                }
                if !event.tag.is_empty() && (self.canonical || !event.implicit) {
                    self.analyze_tag(&event.tag)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn required_quote_style(&self) -> u8 {
        if self.quote_preference == QuoteStyle::Double {
            DOUBLE_QUOTED_SCALAR_STYLE
        } else {
            SINGLE_QUOTED_SCALAR_STYLE
        }
    }

    fn select_scalar_style(&mut self, event: &Event) -> R {
        let no_tag = self.tag_handle.is_empty() && self.tag_suffix.is_empty();
        if no_tag && !event.implicit && !event.quoted_implicit {
            return Err("neither tag nor implicit flags are specified".into());
        }
        let mut style = event.style;
        if style == ANY_SCALAR_STYLE {
            style = PLAIN_SCALAR_STYLE;
        }
        if self.canonical {
            style = DOUBLE_QUOTED_SCALAR_STYLE;
        }
        if self.simple_key_context && self.sc_multiline {
            style = DOUBLE_QUOTED_SCALAR_STYLE;
        }
        if style == PLAIN_SCALAR_STYLE {
            if self.flow_level > 0 && !self.sc_flow_plain_allowed || self.flow_level == 0 && !self.sc_block_plain_allowed {
                style = self.required_quote_style();
            }
            if self.sc_value.is_empty() && (self.flow_level > 0 || self.simple_key_context) {
                style = self.required_quote_style();
            }
            if no_tag && !event.implicit {
                style = self.required_quote_style();
            }
        }
        if style == SINGLE_QUOTED_SCALAR_STYLE && !self.sc_single_quoted_allowed {
            style = DOUBLE_QUOTED_SCALAR_STYLE;
        }
        if (style == LITERAL_SCALAR_STYLE || style == FOLDED_SCALAR_STYLE)
            && (!self.sc_block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = DOUBLE_QUOTED_SCALAR_STYLE;
        }
        if no_tag && !event.quoted_implicit && style != PLAIN_SCALAR_STYLE {
            self.tag_handle = vec![b'!'];
        }
        self.sc_style = style;
        Ok(())
    }

    // ---- writers ----

    fn write_indent(&mut self) -> R {
        let indent = self.indent.max(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_line_break()?;
        }
        if self.foot_indent == indent {
            self.put_line_break()?;
        }
        while self.column < indent {
            self.put(b' ')?;
        }
        self.whitespace = true;
        self.space_above = false;
        self.foot_indent = -1;
        Ok(())
    }

    fn write_indicator(&mut self, indicator: &[u8], need_whitespace: bool, is_whitespace: bool, is_indention: bool) -> R {
        if need_whitespace && !self.whitespace {
            self.put(b' ')?;
        }
        self.write_all(indicator)?;
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
        self.open_ended = false;
        Ok(())
    }

    fn write_anchor(&mut self, value: &[u8]) -> R {
        self.write_all(value)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    fn write_tag_handle(&mut self, value: &[u8]) -> R {
        if !self.whitespace {
            self.put(b' ')?;
        }
        self.write_all(value)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    fn write_tag_content(&mut self, value: &[u8], need_whitespace: bool) -> R {
        if need_whitespace && !self.whitespace {
            self.put(b' ')?;
        }
        let mut i = 0;
        while i < value.len() {
            let must_write = match value[i] {
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'_' | b'.' | b'~' | b'*' | b'\'' | b'('
                | b')' | b'[' | b']' => true,
                _ => is_alpha(value, i),
            };
            if must_write {
                self.write(value, &mut i)?;
            } else {
                let w = width(value[i]).max(1);
                for _ in 0..w {
                    let octet = value[i];
                    i += 1;
                    self.put(b'%')?;
                    let hex = |c: u8| if c < 10 { c + b'0' } else { c + b'A' - 10 };
                    self.put(hex(octet >> 4))?;
                    self.put(hex(octet & 0x0f))?;
                }
            }
        }
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    fn write_plain_scalar(&mut self, value: &[u8], allow_breaks: bool) -> R {
        if !value.is_empty() && !self.whitespace {
            self.put(b' ')?;
        }
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks && !spaces && self.column > self.best_width && !is_space(value, i + 1) {
                    self.write_indent()?;
                    i += width(value[i]);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else if is_line_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_line_break()?;
                }
                self.write_line_break(value, &mut i)?;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        if !value.is_empty() {
            self.whitespace = false;
        }
        self.indention = false;
        if self.root_context {
            self.open_ended = true;
        }
        Ok(())
    }

    fn write_single_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) -> R {
        self.write_indicator(b"'", true, false, false)?;
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks && !spaces && self.column > self.best_width && i > 0 && i < value.len() - 1 && !is_space(value, i + 1) {
                    self.write_indent()?;
                    i += width(value[i]);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else if is_line_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_line_break()?;
                }
                self.write_line_break(value, &mut i)?;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                if value[i] == b'\'' {
                    self.put(b'\'')?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.write_indicator(b"'", false, false, false)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    fn write_double_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) -> R {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false)?;
        let bom = is_bom_at_start(value);
        let mut i = 0;
        while i < value.len() {
            if !is_printable(value, i)
                || (!self.unicode && !is_ascii(value, i))
                || bom
                || is_line_break(value, i)
                || value[i] == b'"'
                || value[i] == b'\\'
            {
                let octet = value[i];
                let (mut w, mut v): (usize, u32) = if octet & 0x80 == 0 {
                    (1, (octet & 0x7F) as u32)
                } else if octet & 0xE0 == 0xC0 {
                    (2, (octet & 0x1F) as u32)
                } else if octet & 0xF0 == 0xE0 {
                    (3, (octet & 0x0F) as u32)
                } else {
                    (4, (octet & 0x07) as u32)
                };
                for k in 1..w {
                    v = (v << 6) + (at(value, i + k) & 0x3F) as u32;
                }
                i += w;
                self.put(b'\\')?;
                let simple = match v {
                    0x00 => Some(b'0'),
                    0x07 => Some(b'a'),
                    0x08 => Some(b'b'),
                    0x09 => Some(b't'),
                    0x0A => Some(b'n'),
                    0x0B => Some(b'v'),
                    0x0C => Some(b'f'),
                    0x0D => Some(b'r'),
                    0x1B => Some(b'e'),
                    0x22 => Some(b'"'),
                    0x5C => Some(b'\\'),
                    0x85 => Some(b'N'),
                    0xA0 => Some(b'_'),
                    0x2028 => Some(b'L'),
                    0x2029 => Some(b'P'),
                    _ => None,
                };
                match simple {
                    Some(c) => self.put(c)?,
                    None => {
                        if v <= 0xFF {
                            self.put(b'x')?;
                            w = 2;
                        } else if v <= 0xFFFF {
                            self.put(b'u')?;
                            w = 4;
                        } else {
                            self.put(b'U')?;
                            w = 8;
                        }
                        let mut k = (w as i32 - 1) * 4;
                        while k >= 0 {
                            let digit = ((v >> k) & 0x0F) as u8;
                            self.put(if digit < 10 { digit + b'0' } else { digit + b'A' - 10 })?;
                            k -= 4;
                        }
                    }
                }
                spaces = false;
            } else if is_space(value, i) {
                if allow_breaks && !spaces && self.column > self.best_width && i > 0 && i < value.len() - 1 {
                    self.write_indent()?;
                    if is_space(value, i + 1) {
                        self.put(b'\\')?;
                    }
                    i += width(value[i]);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else {
                self.write(value, &mut i)?;
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    fn write_block_scalar_hints(&mut self, value: &[u8]) -> R {
        if is_space(value, 0) {
            let hint = [b'0' + self.best_indent as u8];
            self.write_indicator(&hint, false, false, false)?;
        }
        self.open_ended = false;
        let mut chomp: u8 = 0;
        if value.is_empty() {
            chomp = b'-';
        } else {
            let mut i = value.len() - 1;
            while value[i] & 0xC0 == 0x80 {
                i -= 1;
            }
            if !is_line_break(value, i) {
                chomp = b'-';
            } else if i == 0 {
                chomp = b'+';
                self.open_ended = true;
            } else {
                i -= 1;
                while value[i] & 0xC0 == 0x80 {
                    i -= 1;
                }
                if is_line_break(value, i) {
                    chomp = b'+';
                    self.open_ended = true;
                }
            }
        }
        if chomp != 0 {
            self.write_indicator(&[chomp], false, false, false)?;
        }
        Ok(())
    }

    fn write_literal_scalar(&mut self, value: &[u8]) -> R {
        self.write_indicator(b"|", true, false, false)?;
        self.write_block_scalar_hints(value)?;
        self.process_line_comment_linebreak(true)?;
        self.whitespace = true;
        let mut breaks = true;
        let mut i = 0;
        while i < value.len() {
            if is_line_break(value, i) {
                self.write_line_break(value, &mut i)?;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                breaks = false;
            }
        }
        Ok(())
    }

    fn write_folded_scalar(&mut self, value: &[u8]) -> R {
        self.write_indicator(b">", true, false, false)?;
        self.write_block_scalar_hints(value)?;
        self.process_line_comment_linebreak(true)?;
        self.whitespace = true;
        let mut breaks = true;
        let mut leading_spaces = true;
        let mut i = 0;
        while i < value.len() {
            if is_line_break(value, i) {
                if !breaks && !leading_spaces && value[i] == b'\n' {
                    // (go-yaml looks from the start of the value here)
                    let mut k = 0;
                    while is_line_break(value, k) {
                        k += width(value[k]);
                    }
                    if !is_blankz(value, k) {
                        self.put_line_break()?;
                    }
                }
                self.write_line_break(value, &mut i)?;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                    leading_spaces = is_blank(value, i);
                }
                if !breaks && is_space(value, i) && !is_space(value, i + 1) && self.column > self.best_width {
                    self.write_indent()?;
                    i += width(value[i]);
                } else {
                    self.write(value, &mut i)?;
                }
                self.indention = false;
                breaks = false;
            }
        }
        Ok(())
    }

    fn write_comment(&mut self, comment: &[u8]) -> R {
        let mut breaks = false;
        let mut pound = false;
        let mut i = 0;
        while i < comment.len() {
            if is_line_break(comment, i) {
                self.write_line_break(comment, &mut i)?;
                breaks = true;
                pound = false;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                if !pound {
                    if comment[i] != b'#' {
                        self.put(b'#')?;
                        self.put(b' ')?;
                    }
                    pound = true;
                }
                self.write(comment, &mut i)?;
                self.indention = false;
                breaks = false;
            }
        }
        if !breaks {
            self.put_line_break()?;
        }
        self.whitespace = true;
        Ok(())
    }

    fn put(&mut self, c: u8) -> R {
        self.out.push(c);
        self.column += 1;
        Ok(())
    }

    fn put_line_break(&mut self) -> R {
        self.out.extend_from_slice(self.line_break);
        if self.column == 0 {
            self.space_above = true;
        }
        self.column = 0;
        self.line += 1;
        self.indention = true;
        Ok(())
    }

    fn write(&mut self, s: &[u8], i: &mut usize) -> R {
        let w = width(s[*i]).max(1);
        let end = (*i + w).min(s.len());
        self.out.extend_from_slice(&s[*i..end]);
        self.column += 1;
        *i += w;
        Ok(())
    }

    fn write_all(&mut self, s: &[u8]) -> R {
        let mut i = 0;
        while i < s.len() {
            self.write(s, &mut i)?;
        }
        Ok(())
    }

    fn write_line_break(&mut self, s: &[u8], i: &mut usize) -> R {
        if s[*i] == b'\n' {
            self.put_line_break()?;
            *i += 1;
        } else {
            self.write(s, i)?;
            if self.column == 0 {
                self.space_above = true;
            }
            self.column = 0;
            self.line += 1;
            self.indention = true;
        }
        Ok(())
    }

    fn append_tag_directive(&mut self, value: TagDirective, allow_duplicates: bool) -> R {
        if self.tag_directives.iter().any(|d| d.handle == value.handle) {
            if allow_duplicates {
                return Ok(());
            }
            return Err("duplicate %TAG directive".into());
        }
        self.tag_directives.push(value);
        Ok(())
    }

    fn increase_indent_compact(&mut self, flow: bool, indentless: bool, compact_seq: bool) {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if !indentless {
            if self.states.last() == Some(&State::BlockSequenceItem) {
                self.indent += 2;
            } else {
                self.indent = self.best_indent * ((self.indent + self.best_indent) / self.best_indent);
                if compact_seq {
                    self.indent -= 2;
                }
            }
        }
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.increase_indent_compact(flow, indentless, false);
    }
}
