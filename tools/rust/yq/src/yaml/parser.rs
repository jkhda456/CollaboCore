//! The parser (go-yaml's parser.go): tokens to events, comments attached.

use super::scanner::decode_input;
use super::*;

type R<T> = Result<T, LoadError>;

#[derive(Clone, Copy, Debug, Default)]
pub struct SimpleKey {
    pub flow_level: i64,
    pub required: bool,
    pub token_number: i64,
    pub mark: Mark,
}

#[derive(Clone, Debug, Default)]
pub struct Comment {
    pub scan_mark: Mark,
    pub token_mark: Mark,
    pub start_mark: Mark,
    pub end_mark: Mark,
    pub head: Vec<u8>,
    pub line: Vec<u8>,
    pub foot: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    StreamStart,
    ImplicitDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
    End,
}

pub struct Parser {
    pub(crate) last_error: Option<LoadError>,
    // reader
    pub(crate) buffer: Vec<u8>,
    pub(crate) buffer_pos: usize,
    pub(crate) unread: i64,
    pub(crate) reader_error: Option<LoadError>,
    pub(crate) newlines: i64,
    pub(crate) mark: Mark,
    // comments
    pub(crate) head_comment: Vec<u8>,
    pub(crate) line_comment: Vec<u8>,
    pub(crate) foot_comment: Vec<u8>,
    pub(crate) tail_comment: Vec<u8>,
    pub(crate) stem_comment: Option<Vec<u8>>,
    pub(crate) comments: Vec<Comment>,
    pub(crate) comments_head: usize,
    pub(crate) skip_comments: bool,
    // scanner
    pub(crate) stream_start_produced: bool,
    pub(crate) stream_end_produced: bool,
    pub(crate) flow_level: i64,
    pub(crate) tokens: Vec<Token>,
    pub(crate) tokens_head: usize,
    pub(crate) tokens_parsed: i64,
    pub(crate) token_available: bool,
    pub(crate) indent: i64,
    pub(crate) indents: Vec<i64>,
    pub(crate) simple_key_allowed: bool,
    pub(crate) simple_key_possible: bool,
    pub(crate) simple_key: SimpleKey,
    pub(crate) simple_key_stack: Vec<SimpleKey>,
    pub(crate) max_depth: i64,
    // parser
    state: State,
    states: Vec<State>,
    marks: Vec<Mark>,
    tag_directives: Vec<TagDirective>,
}

fn default_tag_directives() -> [TagDirective; 2] {
    [
        TagDirective { handle: b"!".to_vec(), prefix: b"!".to_vec() },
        TagDirective { handle: b"!!".to_vec(), prefix: b"tag:yaml.org,2002:".to_vec() },
    ]
}

fn parser_error(problem: &str, mark: Mark) -> LoadError {
    LoadError::new(Stage::Parser, problem, mark)
}
fn parser_error_context(context: &str, context_mark: Mark, problem: &str, mark: Mark) -> LoadError {
    LoadError::with_context(Stage::Parser, context, context_mark, problem, mark)
}

impl Parser {
    pub fn new(input: &[u8]) -> Parser {
        let (buffer, chars, reader_error) = decode_input(input);
        Parser {
            last_error: None,
            buffer,
            buffer_pos: 0,
            // the characters, and the NUL go-yaml appends at the end of a well-formed input
            unread: if reader_error.is_some() { chars } else { chars + 1 },
            reader_error,
            newlines: 0,
            mark: Mark::new(0, 1, 1),
            head_comment: Vec::new(),
            line_comment: Vec::new(),
            foot_comment: Vec::new(),
            tail_comment: Vec::new(),
            stem_comment: None,
            comments: Vec::new(),
            comments_head: 0,
            skip_comments: false,
            stream_start_produced: false,
            stream_end_produced: false,
            flow_level: 0,
            tokens: Vec::new(),
            tokens_head: 0,
            tokens_parsed: 0,
            token_available: false,
            indent: 0,
            indents: Vec::new(),
            simple_key_allowed: false,
            simple_key_possible: false,
            simple_key: SimpleKey::default(),
            simple_key_stack: Vec::new(),
            max_depth: 10000,
            state: State::StreamStart,
            states: Vec::new(),
            marks: Vec::new(),
            tag_directives: Vec::new(),
        }
    }

    /// The next event; a StreamEnd-typed NoEvent after the end (go-yaml returns io.EOF).
    pub fn parse(&mut self) -> R<Option<Event>> {
        if let Some(e) = &self.last_error {
            return Err(e.clone());
        }
        if self.stream_end_produced || self.state == State::End {
            return Ok(None);
        }
        match self.state_machine() {
            Ok(ev) => Ok(Some(ev)),
            Err(e) => {
                self.last_error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn state_machine(&mut self) -> R<Event> {
        match self.state {
            State::StreamStart => self.parse_stream_start(),
            State::ImplicitDocumentStart => self.parse_document_start(true),
            State::DocumentStart => self.parse_document_start(false),
            State::DocumentContent => self.parse_document_content(),
            State::DocumentEnd => self.parse_document_end(),
            State::BlockNode => self.parse_node(true, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_entry(true),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(false),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(),
            State::BlockMappingFirstKey => self.parse_block_mapping_key(true),
            State::BlockMappingKey => self.parse_block_mapping_key(false),
            State::BlockMappingValue => self.parse_block_mapping_value(),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_entry(true),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key(),
            State::FlowSequenceEntryMappingValue => self.parse_flow_sequence_entry_mapping_value(),
            State::FlowSequenceEntryMappingEnd => self.parse_flow_sequence_entry_mapping_end(),
            State::FlowMappingFirstKey => self.parse_flow_mapping_key(true),
            State::FlowMappingKey => self.parse_flow_mapping_key(false),
            State::FlowMappingValue => self.parse_flow_mapping_value(false),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_value(true),
            State::End => panic!("invalid parser state"),
        }
    }

    /// Peeks at the next token (a copy; the queue keeps it).
    fn peek_token(&mut self) -> R<Token> {
        if !self.token_available {
            self.fetch_more_tokens()?;
        }
        self.unfold_comments();
        Ok(self.tokens[self.tokens_head].clone())
    }

    fn unfold_comments(&mut self) {
        let token_index = self.tokens[self.tokens_head].start_mark.index;
        let token_type = self.tokens[self.tokens_head].typ;
        while self.comments_head < self.comments.len() && token_index >= self.comments[self.comments_head].token_mark.index {
            let comment = &mut self.comments[self.comments_head];
            if !comment.head.is_empty() {
                if token_type == TokenType::BlockEnd {
                    break;
                }
                if !self.head_comment.is_empty() {
                    self.head_comment.push(b'\n');
                }
                self.head_comment.extend_from_slice(&comment.head);
            }
            if !comment.foot.is_empty() {
                if !self.foot_comment.is_empty() {
                    self.foot_comment.push(b'\n');
                }
                self.foot_comment.extend_from_slice(&comment.foot);
            }
            if !comment.line.is_empty() {
                if !self.line_comment.is_empty() {
                    self.line_comment.push(b'\n');
                }
                self.line_comment.extend_from_slice(&comment.line);
            }
            *comment = Comment::default();
            self.comments_head += 1;
        }
        // Drop the processed comments now and then (go-yaml keeps them all).
        if self.comments_head > 256 && self.comments_head * 2 > self.comments.len() {
            self.comments.drain(..self.comments_head);
            self.comments_head = 0;
        }
    }

    fn skip_token(&mut self) {
        self.token_available = false;
        self.tokens_parsed += 1;
        self.stream_end_produced = self.tokens[self.tokens_head].typ == TokenType::StreamEnd;
        self.tokens_head += 1;
    }

    fn set_event_comments(&mut self, event: &mut Event) {
        event.head_comment = std::mem::take(&mut self.head_comment);
        event.line_comment = std::mem::take(&mut self.line_comment);
        event.foot_comment = std::mem::take(&mut self.foot_comment);
        self.tail_comment.clear();
        self.stem_comment = None;
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }

    fn empty_scalar(mark: Mark) -> Event {
        Event { typ: EventType::Scalar, start_mark: mark, end_mark: mark, implicit: true, style: PLAIN_SCALAR_STYLE, ..Default::default() }
    }

    fn parse_stream_start(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        if token.typ != TokenType::StreamStart {
            return Err(parser_error("did not find expected <stream-start>", token.start_mark));
        }
        self.state = State::ImplicitDocumentStart;
        let event = Event { typ: EventType::StreamStart, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
        self.skip_token();
        Ok(event)
    }

    fn parse_document_start(&mut self, implicit: bool) -> R<Event> {
        let mut token = self.peek_token()?;
        while token.typ == TokenType::DocumentEnd {
            self.skip_token();
            token = self.peek_token()?;
        }
        if implicit
            && token.typ != TokenType::VersionDirective
            && token.typ != TokenType::TagDirective
            && token.typ != TokenType::DocumentStart
            && token.typ != TokenType::StreamEnd
        {
            self.process_directives()?;
            self.states.push(State::DocumentEnd);
            self.state = State::BlockNode;
            let mut head_comment = Vec::new();
            let hc = &self.head_comment;
            if !hc.is_empty() {
                let mut i = hc.len() - 1;
                while i > 0 {
                    if hc[i] == b'\n' {
                        if i == hc.len() - 1 {
                            head_comment = hc[..i].to_vec();
                            self.head_comment = self.head_comment[i + 1..].to_vec();
                            break;
                        } else if hc[i - 1] == b'\n' {
                            head_comment = hc[..i - 1].to_vec();
                            self.head_comment = self.head_comment[i + 1..].to_vec();
                            break;
                        }
                    }
                    i -= 1;
                }
            }
            return Ok(Event {
                typ: EventType::DocumentStart,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                implicit: true,
                head_comment,
                ..Default::default()
            });
        }
        if token.typ != TokenType::StreamEnd {
            let start_mark = token.start_mark;
            let (version, tags) = self.process_directives()?;
            let token = self.peek_token()?;
            if token.typ != TokenType::DocumentStart {
                return Err(parser_error("did not find expected <document start>", token.start_mark));
            }
            self.states.push(State::DocumentEnd);
            self.state = State::DocumentContent;
            let event = Event {
                typ: EventType::DocumentStart,
                start_mark,
                end_mark: token.end_mark,
                version_directive: version,
                tag_directives: tags,
                implicit: false,
                ..Default::default()
            };
            self.skip_token();
            return Ok(event);
        }
        self.state = State::End;
        let mut event = Event { typ: EventType::StreamEnd, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
        self.set_event_comments(&mut event);
        self.skip_token();
        Ok(event)
    }

    fn parse_document_content(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        if matches!(
            token.typ,
            TokenType::VersionDirective | TokenType::TagDirective | TokenType::DocumentStart | TokenType::DocumentEnd | TokenType::StreamEnd
        ) {
            self.pop_state();
            return Ok(Self::empty_scalar(token.start_mark));
        }
        self.parse_node(true, false)
    }

    fn parse_document_end(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        let start_mark = token.start_mark;
        let mut end_mark = token.start_mark;
        let mut implicit = true;
        if token.typ == TokenType::DocumentEnd {
            end_mark = token.end_mark;
            self.skip_token();
            implicit = false;
        }
        self.tag_directives.clear();
        self.state = State::DocumentStart;
        let mut event = Event { typ: EventType::DocumentEnd, start_mark, end_mark, implicit, ..Default::default() };
        self.set_event_comments(&mut event);
        if !event.head_comment.is_empty() && event.foot_comment.is_empty() {
            event.foot_comment = std::mem::take(&mut event.head_comment);
        }
        Ok(event)
    }

    fn process_directives(&mut self) -> R<(Option<(i8, i8)>, Vec<TagDirective>)> {
        let mut version: Option<(i8, i8)> = None;
        let mut tags = Vec::new();
        let mut token = self.peek_token()?;
        while token.typ == TokenType::VersionDirective || token.typ == TokenType::TagDirective {
            if token.typ == TokenType::VersionDirective {
                if version.is_some() {
                    return Err(parser_error("found duplicate %YAML directive", token.start_mark));
                }
                if token.major != 1 || token.minor != 1 {
                    return Err(parser_error("found incompatible YAML document", token.start_mark));
                }
                version = Some((token.major, token.minor));
            } else {
                let value = TagDirective { handle: token.value.clone(), prefix: token.prefix.clone() };
                self.append_tag_directive(value.clone(), false, token.start_mark)?;
                tags.push(value);
            }
            self.skip_token();
            token = self.peek_token()?;
        }
        for d in default_tag_directives() {
            self.append_tag_directive(d, true, token.start_mark)?;
        }
        Ok((version, tags))
    }

    fn append_tag_directive(&mut self, value: TagDirective, allow_duplicates: bool, mark: Mark) -> R<()> {
        if self.tag_directives.iter().any(|d| d.handle == value.handle) {
            if allow_duplicates {
                return Ok(());
            }
            return Err(parser_error("found duplicate %TAG directive", mark));
        }
        self.tag_directives.push(value);
        Ok(())
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> R<Event> {
        let mut token = self.peek_token()?;
        if token.typ == TokenType::Alias {
            self.pop_state();
            let mut event = Event { typ: EventType::Alias, start_mark: token.start_mark, end_mark: token.end_mark, anchor: token.value, ..Default::default() };
            self.set_event_comments(&mut event);
            self.skip_token();
            return Ok(event);
        }
        let mut start_mark = token.start_mark;
        let mut end_mark = token.start_mark;
        let mut tag_token = false;
        let mut tag_handle = Vec::new();
        let mut tag_suffix = Vec::new();
        let mut anchor = Vec::new();
        let mut tag_mark = Mark::default();
        if token.typ == TokenType::Anchor {
            anchor = token.value.clone();
            start_mark = token.start_mark;
            end_mark = token.end_mark;
            self.skip_token();
            token = self.peek_token()?;
            if token.typ == TokenType::Tag {
                tag_token = true;
                tag_handle = token.value.clone();
                tag_suffix = token.suffix.clone();
                tag_mark = token.start_mark;
                end_mark = token.end_mark;
                self.skip_token();
                token = self.peek_token()?;
            }
        } else if token.typ == TokenType::Tag {
            tag_token = true;
            tag_handle = token.value.clone();
            tag_suffix = token.suffix.clone();
            start_mark = token.start_mark;
            tag_mark = token.start_mark;
            end_mark = token.end_mark;
            self.skip_token();
            token = self.peek_token()?;
            if token.typ == TokenType::Anchor {
                anchor = token.value.clone();
                end_mark = token.end_mark;
                self.skip_token();
                token = self.peek_token()?;
            }
        }
        let mut tag = Vec::new();
        if tag_token {
            if tag_handle.is_empty() {
                tag = tag_suffix;
            } else {
                for d in &self.tag_directives {
                    if d.handle == tag_handle {
                        tag = d.prefix.clone();
                        tag.extend_from_slice(&tag_suffix);
                        break;
                    }
                }
                if tag.is_empty() {
                    return Err(parser_error_context("while parsing a node", start_mark, "found undefined tag handle", tag_mark));
                }
            }
        }
        let implicit = tag.is_empty();
        if indentless_sequence && token.typ == TokenType::BlockEntry {
            end_mark = token.end_mark;
            self.state = State::IndentlessSequenceEntry;
            return Ok(Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                style: BLOCK_SEQUENCE_STYLE,
                ..Default::default()
            });
        }
        if token.typ == TokenType::Scalar {
            let mut plain_implicit = false;
            let mut quoted_implicit = false;
            end_mark = token.end_mark;
            if (tag.is_empty() && token.style == PLAIN_SCALAR_STYLE) || (tag.len() == 1 && tag[0] == b'!') {
                plain_implicit = true;
            } else if tag.is_empty() {
                quoted_implicit = true;
            }
            self.pop_state();
            let mut event = Event {
                typ: EventType::Scalar,
                start_mark,
                end_mark,
                anchor,
                tag,
                value: token.value,
                implicit: plain_implicit,
                quoted_implicit,
                style: token.style,
                ..Default::default()
            };
            self.set_event_comments(&mut event);
            self.skip_token();
            return Ok(event);
        }
        if token.typ == TokenType::FlowSequenceStart {
            end_mark = token.end_mark;
            self.state = State::FlowSequenceFirstEntry;
            let mut event = Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                style: FLOW_SEQUENCE_STYLE,
                ..Default::default()
            };
            self.set_event_comments(&mut event);
            return Ok(event);
        }
        if token.typ == TokenType::FlowMappingStart {
            end_mark = token.end_mark;
            self.state = State::FlowMappingFirstKey;
            let mut event = Event {
                typ: EventType::MappingStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                style: FLOW_MAPPING_STYLE,
                ..Default::default()
            };
            self.set_event_comments(&mut event);
            return Ok(event);
        }
        if block && token.typ == TokenType::BlockSequenceStart {
            end_mark = token.end_mark;
            self.state = State::BlockSequenceFirstEntry;
            let mut event = Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                style: BLOCK_SEQUENCE_STYLE,
                ..Default::default()
            };
            if let Some(stem) = self.stem_comment.take() {
                event.head_comment = stem;
            }
            return Ok(event);
        }
        if block && token.typ == TokenType::BlockMappingStart {
            end_mark = token.end_mark;
            self.state = State::BlockMappingFirstKey;
            let mut event = Event {
                typ: EventType::MappingStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                style: BLOCK_MAPPING_STYLE,
                ..Default::default()
            };
            if let Some(stem) = self.stem_comment.take() {
                event.head_comment = stem;
            }
            return Ok(event);
        }
        if !anchor.is_empty() || !tag.is_empty() {
            self.pop_state();
            return Ok(Event {
                typ: EventType::Scalar,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                quoted_implicit: false,
                style: PLAIN_SCALAR_STYLE,
                ..Default::default()
            });
        }
        let context = if block { "while parsing a block node" } else { "while parsing a flow node" };
        Err(parser_error_context(context, start_mark, "did not find expected node content", token.start_mark))
    }

    fn parse_block_sequence_entry(&mut self, first: bool) -> R<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start_mark);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            let prior_head_len = self.head_comment.len();
            self.skip_token();
            self.split_stem_comment(prior_head_len)?;
            token = self.peek_token()?;
            if token.typ != TokenType::BlockEntry && token.typ != TokenType::BlockEnd {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::BlockSequenceEntry;
            return Ok(Self::empty_scalar(mark));
        }
        if token.typ == TokenType::BlockEnd {
            self.pop_state();
            self.marks.pop();
            let event = Event { typ: EventType::SequenceEnd, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
            self.skip_token();
            return Ok(event);
        }
        let context_mark = self.marks.pop().unwrap_or_default();
        Err(parser_error_context("while parsing a block collection", context_mark, "did not find expected '-' indicator", token.start_mark))
    }

    fn parse_indentless_sequence_entry(&mut self) -> R<Event> {
        let mut token = self.peek_token()?;
        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            let prior_head_len = self.head_comment.len();
            self.skip_token();
            self.split_stem_comment(prior_head_len)?;
            token = self.peek_token()?;
            if !matches!(token.typ, TokenType::BlockEntry | TokenType::Key | TokenType::Value | TokenType::BlockEnd) {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::IndentlessSequenceEntry;
            return Ok(Self::empty_scalar(mark));
        }
        self.pop_state();
        Ok(Event { typ: EventType::SequenceEnd, start_mark: token.start_mark, end_mark: token.start_mark, ..Default::default() })
    }

    fn split_stem_comment(&mut self, stem_len: usize) -> R<()> {
        if stem_len == 0 {
            return Ok(());
        }
        let token = self.peek_token()?;
        if token.typ != TokenType::BlockSequenceStart && token.typ != TokenType::BlockMappingStart {
            return Ok(());
        }
        self.stem_comment = Some(self.head_comment[..stem_len].to_vec());
        if self.head_comment.len() == stem_len {
            self.head_comment = Vec::new();
        } else {
            self.head_comment = self.head_comment[stem_len + 1..].to_vec();
        }
        Ok(())
    }

    fn parse_block_mapping_key(&mut self, first: bool) -> R<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start_mark);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if !self.tail_comment.is_empty() {
            return Ok(Event {
                typ: EventType::TailComment,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                foot_comment: std::mem::take(&mut self.tail_comment),
                ..Default::default()
            });
        }
        if token.typ == TokenType::Key {
            let mark = token.end_mark;
            self.skip_token();
            token = self.peek_token()?;
            if !matches!(token.typ, TokenType::Key | TokenType::Value | TokenType::BlockEnd) {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = State::BlockMappingValue;
            return Ok(Self::empty_scalar(mark));
        }
        if token.typ == TokenType::BlockEnd {
            self.pop_state();
            self.marks.pop();
            let mut event = Event { typ: EventType::MappingEnd, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
            self.set_event_comments(&mut event);
            self.skip_token();
            return Ok(event);
        }
        let context_mark = self.marks.pop().unwrap_or_default();
        Err(parser_error_context("while parsing a block mapping", context_mark, "did not find expected key", token.start_mark))
    }

    fn parse_block_mapping_value(&mut self) -> R<Event> {
        let mut token = self.peek_token()?;
        if token.typ == TokenType::Value {
            let mark = token.end_mark;
            self.skip_token();
            token = self.peek_token()?;
            if !matches!(token.typ, TokenType::Key | TokenType::Value | TokenType::BlockEnd) {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(true, true);
            }
            self.state = State::BlockMappingKey;
            return Ok(Self::empty_scalar(mark));
        }
        self.state = State::BlockMappingKey;
        Ok(Self::empty_scalar(token.start_mark))
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> R<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start_mark);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if token.typ != TokenType::FlowSequenceEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip_token();
                    token = self.peek_token()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error_context(
                        "while parsing a flow sequence",
                        context_mark,
                        "did not find expected ',' or ']'",
                        token.start_mark,
                    ));
                }
            }
            if token.typ == TokenType::Key {
                self.state = State::FlowSequenceEntryMappingKey;
                let event = Event {
                    typ: EventType::MappingStart,
                    start_mark: token.start_mark,
                    end_mark: token.end_mark,
                    implicit: true,
                    style: FLOW_MAPPING_STYLE,
                    ..Default::default()
                };
                self.skip_token();
                return Ok(event);
            } else if token.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        let mut event = Event { typ: EventType::SequenceEnd, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
        self.set_event_comments(&mut event);
        self.skip_token();
        Ok(event)
    }

    fn parse_flow_sequence_entry_mapping_key(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        if !matches!(token.typ, TokenType::Value | TokenType::FlowEntry | TokenType::FlowSequenceEnd) {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(false, false);
        }
        let mark = token.end_mark;
        self.skip_token();
        self.state = State::FlowSequenceEntryMappingValue;
        Ok(Self::empty_scalar(mark))
    }

    fn parse_flow_sequence_entry_mapping_value(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        if token.typ == TokenType::Value {
            self.skip_token();
            let token2 = self.peek_token()?;
            if token2.typ != TokenType::FlowEntry && token2.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(false, false);
            }
        }
        self.state = State::FlowSequenceEntryMappingEnd;
        // go-yaml uses the start of the token peeked first here
        Ok(Self::empty_scalar(token.start_mark))
    }

    fn parse_flow_sequence_entry_mapping_end(&mut self) -> R<Event> {
        let token = self.peek_token()?;
        self.state = State::FlowSequenceEntry;
        Ok(Event { typ: EventType::MappingEnd, start_mark: token.start_mark, end_mark: token.start_mark, ..Default::default() })
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> R<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start_mark);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if token.typ != TokenType::FlowMappingEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip_token();
                    token = self.peek_token()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error_context(
                        "while parsing a flow mapping",
                        context_mark,
                        "did not find expected ',' or '}'",
                        token.start_mark,
                    ));
                }
            }
            if token.typ == TokenType::Key {
                self.skip_token();
                token = self.peek_token()?;
                if !matches!(token.typ, TokenType::Value | TokenType::FlowEntry | TokenType::FlowMappingEnd) {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = State::FlowMappingValue;
                return Ok(Self::empty_scalar(token.start_mark));
            } else if token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        let mut event = Event { typ: EventType::MappingEnd, start_mark: token.start_mark, end_mark: token.end_mark, ..Default::default() };
        self.set_event_comments(&mut event);
        self.skip_token();
        Ok(event)
    }

    fn parse_flow_mapping_value(&mut self, empty: bool) -> R<Event> {
        let mut token = self.peek_token()?;
        if empty {
            self.state = State::FlowMappingKey;
            return Ok(Self::empty_scalar(token.start_mark));
        }
        if token.typ == TokenType::Value {
            self.skip_token();
            token = self.peek_token()?;
            if token.typ != TokenType::FlowEntry && token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(false, false);
            }
        }
        self.state = State::FlowMappingKey;
        Ok(Self::empty_scalar(token.start_mark))
    }
}
