//! YAML, as yq reads and writes it: a port of go.yaml.in/yaml/v4 (v4.0.0-rc.6, itself a Go port
//! of libyaml) — reader, scanner (with go-yaml's comment handling), parser, composer, resolver,
//! desolver, serializer and emitter — so that the documents yq prints are byte for byte the same.

pub mod composer;
pub mod emitter;
pub mod parser;
pub mod resolve;
pub mod scanner;
pub mod serializer;

use std::fmt;

/// A position: 1-based line and column (0 = unknown), and the index in characters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    pub index: i64,
    pub line: i64,
    pub column: i64,
}

impl Mark {
    pub fn new(index: i64, line: i64, column: i64) -> Mark {
        Mark { index, line, column }
    }
    fn short_string(&self) -> String {
        if self.line == 0 {
            return "<unknown position>".into();
        }
        if self.column > 0 {
            return format!("L{}.C{}", self.line, self.column);
        }
        format!("L{}", self.line)
    }
    fn range_string(&self, end: &Mark) -> String {
        let start = self.short_string();
        if self.line == end.line {
            if self.column == 0 && end.column == 0 {
                return start;
            }
            if self.column > 0 && end.column > 0 {
                if self.column == end.column {
                    return start;
                }
                return format!("{start}-C{}", end.column);
            }
        }
        format!("{start}-{}", end.short_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Reader,
    Scanner,
    Parser,
    Composer,
    Resolver,
    Constructor,
}

impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Stage::Reader => "reader",
            Stage::Scanner => "scanner",
            Stage::Parser => "parser",
            Stage::Composer => "composer",
            Stage::Resolver => "resolver",
            Stage::Constructor => "constructor",
        }
    }
}

/// go-yaml's LoadError.
#[derive(Clone, Debug)]
pub struct LoadError {
    pub stage: Stage,
    pub message: String,
    pub mark: Mark,
    pub context_mark: Mark,
    pub context: String,
}

impl LoadError {
    pub fn new(stage: Stage, message: impl Into<String>, mark: Mark) -> LoadError {
        LoadError { stage, message: message.into(), mark, context_mark: Mark::default(), context: String::new() }
    }
    pub fn with_context(stage: Stage, context: &str, context_mark: Mark, message: impl Into<String>, mark: Mark) -> LoadError {
        LoadError { stage, message: message.into(), mark, context_mark, context: context.into() }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if !self.context.is_empty() {
            write!(f, "go-yaml load error in {} ({}) at {}: {}", self.stage.name(), self.context, self.context_mark.range_string(&self.mark), self.message)
        } else {
            write!(f, "go-yaml load error in {} at {}: {}", self.stage.name(), self.mark.short_string(), self.message)
        }
    }
}

/// An error while writing YAML (go-yaml's DumpError).
#[derive(Clone, Debug)]
pub struct DumpError {
    pub stage: &'static str,
    pub message: String,
}

impl fmt::Display for DumpError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "go-yaml dump error in {}: {}", self.stage, self.message)
    }
}

// ---- scalar styles (libyaml's, bit flags) ----
pub const ANY_SCALAR_STYLE: u8 = 0;
pub const PLAIN_SCALAR_STYLE: u8 = 2;
pub const SINGLE_QUOTED_SCALAR_STYLE: u8 = 4;
pub const DOUBLE_QUOTED_SCALAR_STYLE: u8 = 8;
pub const LITERAL_SCALAR_STYLE: u8 = 16;
pub const FOLDED_SCALAR_STYLE: u8 = 32;

pub const ANY_SEQUENCE_STYLE: u8 = 0;
pub const BLOCK_SEQUENCE_STYLE: u8 = 1;
pub const FLOW_SEQUENCE_STYLE: u8 = 2;
pub const BLOCK_MAPPING_STYLE: u8 = 1;
pub const FLOW_MAPPING_STYLE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TokenType {
    #[default]
    NoToken,
    StreamStart,
    StreamEnd,
    VersionDirective,
    TagDirective,
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowSequenceEnd,
    FlowMappingStart,
    FlowMappingEnd,
    BlockEntry,
    FlowEntry,
    Key,
    Value,
    Alias,
    Anchor,
    Tag,
    Scalar,
    Comment,
}

#[derive(Clone, Debug, Default)]
pub struct Token {
    pub typ: TokenType,
    pub start_mark: Mark,
    pub end_mark: Mark,
    pub value: Vec<u8>,
    pub suffix: Vec<u8>,
    pub prefix: Vec<u8>,
    pub style: u8,
    pub major: i8,
    pub minor: i8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EventType {
    #[default]
    NoEvent,
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias,
    Scalar,
    SequenceStart,
    SequenceEnd,
    MappingStart,
    MappingEnd,
    TailComment,
}

impl EventType {
    pub fn name(&self) -> &'static str {
        match self {
            EventType::NoEvent => "none",
            EventType::StreamStart => "stream start",
            EventType::StreamEnd => "stream end",
            EventType::DocumentStart => "document start",
            EventType::DocumentEnd => "document end",
            EventType::Alias => "alias",
            EventType::Scalar => "scalar",
            EventType::SequenceStart => "sequence start",
            EventType::SequenceEnd => "sequence end",
            EventType::MappingStart => "mapping start",
            EventType::MappingEnd => "mapping end",
            EventType::TailComment => "tail comment",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TagDirective {
    pub handle: Vec<u8>,
    pub prefix: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Event {
    pub typ: EventType,
    pub start_mark: Mark,
    pub end_mark: Mark,
    pub version_directive: Option<(i8, i8)>,
    pub tag_directives: Vec<TagDirective>,
    pub head_comment: Vec<u8>,
    pub line_comment: Vec<u8>,
    pub foot_comment: Vec<u8>,
    pub tail_comment: Vec<u8>,
    pub anchor: Vec<u8>,
    pub tag: Vec<u8>,
    pub value: Vec<u8>,
    pub implicit: bool,
    pub quoted_implicit: bool,
    pub style: u8,
}

// ---- nodes ----
pub const DOCUMENT_NODE: u32 = 1;
pub const SEQUENCE_NODE: u32 = 2;
pub const MAPPING_NODE: u32 = 4;
pub const SCALAR_NODE: u32 = 8;
pub const ALIAS_NODE: u32 = 16;

pub const TAGGED_STYLE: u32 = 1;
pub const DOUBLE_QUOTED_STYLE: u32 = 2;
pub const SINGLE_QUOTED_STYLE: u32 = 4;
pub const LITERAL_STYLE: u32 = 8;
pub const FOLDED_STYLE: u32 = 16;
pub const FLOW_STYLE: u32 = 32;

/// go-yaml's Node. An alias keeps the anchor it refers to by name.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub kind: u32,
    pub style: u32,
    pub tag: String,
    pub value: String,
    pub anchor: String,
    pub alias: Option<String>,
    pub content: Vec<Node>,
    pub head_comment: String,
    pub line_comment: String,
    pub foot_comment: String,
    pub line: i64,
    pub column: i64,
}

impl Node {
    pub fn is_zero(&self) -> bool {
        self.kind == 0 && self.style == 0 && self.tag.is_empty() && self.value.is_empty() && self.anchor.is_empty()
            && self.alias.is_none() && self.content.is_empty() && self.head_comment.is_empty()
            && self.line_comment.is_empty() && self.foot_comment.is_empty() && self.line == 0 && self.column == 0
    }
}

pub const NULL_TAG: &str = "!!null";
pub const BOOL_TAG: &str = "!!bool";
pub const STR_TAG: &str = "!!str";
pub const INT_TAG: &str = "!!int";
pub const FLOAT_TAG: &str = "!!float";
pub const TIMESTAMP_TAG: &str = "!!timestamp";
pub const SEQ_TAG: &str = "!!seq";
pub const MAP_TAG: &str = "!!map";
pub const BINARY_TAG: &str = "!!binary";
pub const MERGE_TAG: &str = "!!merge";
const LONG_TAG_PREFIX: &str = "tag:yaml.org,2002:";

pub fn short_tag(tag: &str) -> String {
    if let Some(rest) = tag.strip_prefix(LONG_TAG_PREFIX) {
        return format!("!!{rest}");
    }
    tag.to_string()
}

pub fn long_tag(tag: &str) -> String {
    if let Some(rest) = tag.strip_prefix("!!") {
        return format!("{LONG_TAG_PREFIX}{rest}");
    }
    tag.to_string()
}

// ---- character classes (libyaml's macros, on a byte buffer read past its end as zeros) ----

#[inline]
pub(crate) fn at(b: &[u8], i: usize) -> u8 {
    *b.get(i).unwrap_or(&0)
}
#[inline]
pub(crate) fn is_alpha(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c.is_ascii_digit() || c.is_ascii_uppercase() || c.is_ascii_lowercase() || c == b'_' || c == b'-'
}
#[inline]
pub(crate) fn is_flow_indicator(b: &[u8], i: usize) -> bool {
    matches!(at(b, i), b'[' | b']' | b'{' | b'}' | b',')
}
/// go-yaml checks the BOM at the start of its working buffer, not at `i`; on a whole buffer that
/// is the start of the value, which is what this does.
#[inline]
pub(crate) fn is_bom_at_start(b: &[u8]) -> bool {
    at(b, 0) == 0xEF && at(b, 1) == 0xBB && at(b, 2) == 0xBF
}
#[inline]
pub(crate) fn is_bom(b: &[u8], i: usize) -> bool {
    at(b, i) == 0xEF && at(b, i + 1) == 0xBB && at(b, i + 2) == 0xBF
}
pub(crate) fn is_anchor_char_with(b: &[u8], i: usize, bom: bool) -> bool {
    if at(b, i) == b':' {
        return false;
    }
    is_printable(b, i) && !is_line_break(b, i) && !is_blank(b, i) && !bom && !is_flow_indicator(b, i) && is_ascii(b, i)
}
#[inline]
pub(crate) fn is_tag_uri_char(b: &[u8], i: usize, verbatim: bool) -> bool {
    if is_alpha(b, i) {
        return true;
    }
    match at(b, i) {
        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'('
        | b')' | b'%' => true,
        b',' | b'[' | b']' | b'{' | b'}' => verbatim,
        _ => false,
    }
}
#[inline]
pub(crate) fn is_digit(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_digit()
}
#[inline]
pub(crate) fn as_digit(b: &[u8], i: usize) -> i32 {
    at(b, i) as i32 - b'0' as i32
}
#[inline]
pub(crate) fn is_hex(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_hexdigit()
}
#[inline]
pub(crate) fn as_hex(b: &[u8], i: usize) -> u32 {
    let c = at(b, i);
    match c {
        b'A'..=b'F' => (c - b'A' + 10) as u32,
        b'a'..=b'f' => (c - b'a' + 10) as u32,
        _ => (c as u32).wrapping_sub(b'0' as u32),
    }
}
#[inline]
pub(crate) fn is_ascii(b: &[u8], i: usize) -> bool {
    at(b, i) <= 0x7F
}
#[inline]
pub(crate) fn is_printable(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == 0x0A
        || (0x20..=0x7E).contains(&c)
        || (c == 0xC2 && at(b, i + 1) >= 0xA0)
        || (c > 0xC2 && c < 0xED)
        || (c == 0xED && at(b, i + 1) < 0xA0)
        || c == 0xEE
        || (c == 0xEF
            && !(at(b, i + 1) == 0xBB && at(b, i + 2) == 0xBF)
            && !(at(b, i + 1) == 0xBF && (at(b, i + 2) == 0xBE || at(b, i + 2) == 0xBF)))
}
#[inline]
pub(crate) fn is_z(b: &[u8], i: usize) -> bool {
    at(b, i) == 0
}
#[inline]
pub(crate) fn is_space(b: &[u8], i: usize) -> bool {
    at(b, i) == b' '
}
#[inline]
pub(crate) fn is_tab(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\t'
}
#[inline]
pub(crate) fn is_blank(b: &[u8], i: usize) -> bool {
    matches!(at(b, i), b' ' | b'\t')
}
#[inline]
pub(crate) fn is_line_break(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b'\r'
        || c == b'\n'
        || (c == 0xC2 && at(b, i + 1) == 0x85)
        || (c == 0xE2 && at(b, i + 1) == 0x80 && (at(b, i + 2) == 0xA8 || at(b, i + 2) == 0xA9))
}
#[inline]
pub(crate) fn is_crlf(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\r' && at(b, i + 1) == b'\n'
}
#[inline]
pub(crate) fn is_breakz(b: &[u8], i: usize) -> bool {
    is_line_break(b, i) || at(b, i) == 0
}
#[inline]
pub(crate) fn is_spacez(b: &[u8], i: usize) -> bool {
    at(b, i) == b' ' || is_breakz(b, i)
}
#[inline]
pub(crate) fn is_blankz(b: &[u8], i: usize) -> bool {
    is_blank(b, i) || is_breakz(b, i)
}
#[inline]
pub(crate) fn width(c: u8) -> usize {
    if c & 0x80 == 0 {
        1
    } else if c & 0xE0 == 0xC0 {
        2
    } else if c & 0xF0 == 0xE0 {
        3
    } else if c & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}
