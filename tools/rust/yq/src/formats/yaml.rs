//! yq's YAML decoder (with its leading-content pre-processing) and encoder.

use super::{Decoder, Encoder};
use crate::eval::Res;
use crate::node::*;
use crate::prefs::YamlPrefs;
use crate::yaml;
use std::collections::HashMap;

pub struct YamlDecoder {
    prefs: YamlPrefs,
    composer: Option<yaml::composer::Composer>,
    leading_content: String,
    buffer_read: String,
    anchor_map: HashMap<String, NodeId>,
    read_anything: bool,
    first_file: bool,
    document_index: u64,
}

impl YamlDecoder {
    pub fn new(prefs: YamlPrefs) -> YamlDecoder {
        YamlDecoder {
            prefs,
            composer: None,
            leading_content: String::new(),
            buffer_read: String::new(),
            anchor_map: HashMap::new(),
            read_anything: false,
            first_file: true,
            document_index: 0,
        }
    }
}

/// RE2's \s: [\t\n\f\r ]
fn re_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ')
}

fn starts_after_re_space(line: &str, prefix: &str) -> bool {
    line.trim_start_matches(re_space).starts_with(prefix)
}

/// `^\s*---\s*$` on the trimmed line
fn is_separator_line(trimmed: &str) -> bool {
    let t = trimmed.trim_start_matches(re_space);
    if let Some(rest) = t.strip_prefix("---") {
        return rest.chars().all(re_space);
    }
    false
}

/// `^\s*---\s+`: the length of the match
fn separator_prefix(line: &str) -> Option<usize> {
    let lead = line.len() - line.trim_start_matches(re_space).len();
    let rest = &line[lead..];
    let after = rest.strip_prefix("---")?;
    let ws = after.len() - after.trim_start_matches(re_space).len();
    if ws == 0 {
        return None;
    }
    Some(lead + 3 + ws)
}

/// yamlDecoder.processReadStream: the leading comments, directives, blank lines and separators,
/// and the rest of the input. A `--- text` line's remainder is pushed back in front of the rest
/// without its line break, as yq does.
fn process_read_stream(input: &[u8]) -> (Vec<u8>, String) {
    let mut sb = String::new();
    let mut stream: Vec<u8> = input.to_vec();
    let mut pos = 0;
    loop {
        let (line_bytes, eof) = match stream[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => {
                let l = stream[pos..pos + i + 1].to_vec();
                pos += i + 1;
                (l, false)
            }
            None => {
                let l = stream[pos..].to_vec();
                pos = stream.len();
                (l, true)
            }
        };
        if eof && line_bytes.is_empty() {
            return (Vec::new(), sb);
        }
        let line_full = String::from_utf8_lossy(&line_bytes).into_owned();
        let (line, newline) = if let Some(l) = line_full.strip_suffix("\r\n") {
            (l.to_string(), "\r\n")
        } else if let Some(l) = line_full.strip_suffix('\n') {
            (l.to_string(), "\n")
        } else {
            (line_full.clone(), "")
        };
        let trimmed = crate::util::go_trim_space(&line);
        if is_separator_line(trimmed) {
            sb.push_str("$yqDocSeparator$");
            sb.push_str(newline);
            if eof {
                return (Vec::new(), sb);
            }
            continue;
        }
        if let Some(m) = separator_prefix(&line) {
            let remainder = line[m..].to_string();
            sb.push_str("$yqDocSeparator$");
            sb.push_str(if newline.is_empty() { "\n" } else { newline });
            if eof && remainder.is_empty() {
                return (Vec::new(), sb);
            }
            let mut rest = remainder.into_bytes();
            rest.extend_from_slice(&stream[pos..]);
            stream = rest;
            pos = 0;
            continue;
        }
        if starts_after_re_space(&line, "#") || starts_after_re_space(&line, "%YAML") || trimmed.is_empty() {
            sb.push_str(&line);
            sb.push_str(newline);
            if eof {
                return (Vec::new(), sb);
            }
            continue;
        }
        let mut rest = format!("{line}{newline}").into_bytes();
        rest.extend_from_slice(&stream[pos..]);
        return (rest, sb);
    }
}

impl Decoder for YamlDecoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()> {
        let mut to_use = input;
        let mut leading = String::new();
        self.buffer_read.clear();
        if self.prefs.leading_content_pre_processing && (!self.prefs.evaluate_together || self.first_file) {
            let (rest, lc) = process_read_stream(&to_use);
            to_use = rest;
            leading = lc;
        } else if !self.prefs.leading_content_pre_processing {
            self.buffer_read = String::from_utf8_lossy(&to_use).into_owned();
        }
        self.leading_content = leading;
        self.read_anything = false;
        self.composer = Some(yaml::composer::Composer::new(&to_use));
        self.first_file = false;
        self.document_index = 0;
        self.anchor_map = HashMap::new();
        Ok(())
    }

    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>> {
        let doc = self.composer.as_mut().unwrap().next_document().map_err(|err| err.to_string())?;
        let Some(doc) = doc else {
            if !self.leading_content.is_empty() && !self.read_anything {
                self.read_anything = true;
                return Ok(Some(self.blank_node_with_comment(e)));
            } else if !self.prefs.leading_content_pre_processing && !self.read_anything {
                self.read_anything = true;
                self.leading_content = self.buffer_read.clone();
                if !self.leading_content.is_empty() {
                    return Ok(Some(self.blank_node_with_comment(e)));
                }
                return Ok(None);
            }
            return Ok(None);
        };
        if doc.content.is_empty() {
            return Err("yaml node has no content".into());
        }
        let c = e.add(CNode { document: self.document_index, filename: "".into(), ..Default::default() });
        unmarshal_yaml(e, c, &doc.content[0], &mut self.anchor_map)?;
        let hc = format!("{}{}", doc.head_comment, e.n(c).head_comment);
        let fc = format!("{}{}", doc.foot_comment, e.n(c).foot_comment);
        e.nm(c).head_comment = hc;
        e.nm(c).foot_comment = fc;
        if !self.leading_content.is_empty() {
            e.nm(c).leading_content = std::mem::take(&mut self.leading_content);
        }
        self.read_anything = true;
        self.document_index += 1;
        Ok(Some(c))
    }
}

impl YamlDecoder {
    fn blank_node_with_comment(&self, e: &mut Engine) -> NodeId {
        let n = e.scalar("!!null", "");
        e.nm(n).leading_content = self.leading_content.clone();
        n
    }
}

fn map_style(s: u32) -> u32 {
    // go-yaml's and yq's style bits are the same
    s
}

fn copy_from_yaml(e: &mut Engine, o: NodeId, node: &yaml::Node, anchors: &mut HashMap<String, NodeId>) {
    let n = e.nm(o);
    n.style = map_style(node.style);
    n.tag = node.tag.clone();
    n.value = node.value.clone();
    n.anchor = node.anchor.clone();
    n.head_comment = node.head_comment.clone();
    n.line_comment = node.line_comment.clone();
    n.foot_comment = node.foot_comment.clone();
    n.line = node.line;
    n.column = node.column;
    if !node.anchor.is_empty() {
        anchors.insert(node.anchor.clone(), o);
    }
    if let Some(a) = &node.alias {
        if !a.is_empty() {
            e.nm(o).alias = anchors.get(a).copied();
        }
    }
}

fn decode_into_child(e: &mut Engine, parent: NodeId, child: &yaml::Node, anchors: &mut HashMap<String, NodeId>) -> Res<NodeId> {
    let c = e.create_child(parent);
    if child.tag == "!!null" {
        e.nm(c).kind = SCALAR;
        copy_from_yaml(e, c, child, anchors);
        return Ok(c);
    }
    unmarshal_yaml(e, c, child, anchors)?;
    Ok(c)
}

pub fn unmarshal_yaml(e: &mut Engine, o: NodeId, node: &yaml::Node, anchors: &mut HashMap<String, NodeId>) -> Res<()> {
    match node.kind {
        yaml::ALIAS_NODE => {
            e.nm(o).kind = ALIAS;
            copy_from_yaml(e, o, node, anchors);
        }
        yaml::SCALAR_NODE => {
            e.nm(o).kind = SCALAR;
            copy_from_yaml(e, o, node, anchors);
        }
        yaml::MAPPING_NODE => {
            e.nm(o).kind = MAP;
            copy_from_yaml(e, o, node, anchors);
            let mut content = Vec::with_capacity(node.content.len());
            let mut i = 0;
            while i + 1 < node.content.len() {
                let k = decode_into_child(e, o, &node.content[i], anchors)?;
                e.nm(k).is_map_key = true;
                let v = decode_into_child(e, o, &node.content[i + 1], anchors)?;
                e.nm(v).key = Some(k);
                content.push(k);
                content.push(v);
                i += 2;
            }
            e.nm(o).content = content;
        }
        yaml::SEQUENCE_NODE => {
            e.nm(o).kind = SEQ;
            copy_from_yaml(e, o, node, anchors);
            let mut content = Vec::with_capacity(node.content.len());
            for (i, c) in node.content.iter().enumerate() {
                let k = e.create_child(o);
                {
                    let kn = e.nm(k);
                    kn.is_map_key = true;
                    kn.tag = "!!int".into();
                    kn.kind = SCALAR;
                    kn.value = i.to_string();
                }
                let v = decode_into_child(e, o, c, anchors)?;
                e.nm(v).key = Some(k);
                content.push(v);
            }
            e.nm(o).content = content;
        }
        0 => copy_from_yaml(e, o, node, anchors),
        _ => return Err("orderedMap: invalid yaml node".into()),
    }
    Ok(())
}

/// CandidateNode.MarshalYAML
pub fn marshal_yaml(e: &Engine, o: NodeId) -> yaml::Node {
    let n = e.n(o);
    let kind = match n.kind {
        ALIAS => yaml::ALIAS_NODE,
        SCALAR => yaml::SCALAR_NODE,
        MAP => yaml::MAPPING_NODE,
        SEQ => yaml::SEQUENCE_NODE,
        _ => 0,
    };
    let mut t = yaml::Node {
        kind,
        style: n.style,
        tag: n.tag.clone(),
        value: n.value.clone(),
        anchor: n.anchor.clone(),
        head_comment: n.head_comment.clone(),
        line_comment: n.line_comment.clone(),
        foot_comment: n.foot_comment.clone(),
        line: n.line,
        column: n.column,
        ..Default::default()
    };
    if n.kind == MAP || n.kind == SEQ {
        t.content = n.content.iter().map(|c| marshal_yaml(e, *c)).collect();
    }
    t
}

/// parseSnippet: a scalar value read as YAML (to guess a custom-tagged value's type).
pub fn parse_snippet(e: &mut Engine, value: &str) -> Res<NodeId> {
    if value.is_empty() {
        return Ok(e.scalar("!!null", ""));
    }
    let mut dec = YamlDecoder::new(e.prefs.yaml.clone());
    dec.init(value.as_bytes().to_vec())?;
    let result = dec.decode(e)?.ok_or_else(|| "EOF".to_string())?;
    let lc = std::mem::take(&mut e.nm(result).leading_content);
    if e.n(result).kind == SCALAR {
        e.nm(result).line_comment = lc;
    } else {
        e.nm(result).head_comment = lc;
    }
    if e.n(result).tag == "!!str" {
        let n = e.scalar("!!str", value);
        e.nm(n).line_comment = e.n(result).line_comment.clone();
        return Ok(n);
    }
    e.nm(result).line = 0;
    e.nm(result).column = 0;
    Ok(result)
}

pub struct YamlEncoder {
    pub prefs: YamlPrefs,
}

/// PrintYAMLLeadingContent
pub fn print_leading_content(out: &mut Vec<u8>, content: &str, print_doc_separators: bool, colors: bool) {
    let mut rest = content;
    loop {
        let (line, eof) = match rest.find('\n') {
            Some(i) => {
                let l = &rest[..=i];
                rest = &rest[i + 1..];
                (l.to_string(), false)
            }
            None => {
                let l = rest.to_string();
                rest = "";
                (l, true)
            }
        };
        let mut readline = line;
        if readline.contains("$yqDocSeparator$") {
            let ending = if readline.ends_with("\r\n") { "\r\n" } else { "\n" };
            if print_doc_separators {
                out.extend_from_slice(b"---");
                out.extend_from_slice(ending.as_bytes());
            }
        } else {
            if !readline.is_empty() && readline != "\n" && !readline.starts_with('%') && !starts_after_re_space(&readline, "#") {
                readline = format!("# {readline}");
            }
            if colors && !crate::util::go_trim_space(&readline).is_empty() {
                readline = format!("\x1b[90m{readline}\x1b[0m");
            }
            out.extend_from_slice(readline.as_bytes());
        }
        if eof {
            if !readline.is_empty() {
                out.push(b'\n');
            }
            break;
        }
    }
}

impl Encoder for YamlEncoder {
    fn can_handle_aliases(&self) -> bool {
        true
    }

    fn print_document_separator(&mut self, out: &mut Vec<u8>) -> Res<()> {
        if self.prefs.print_doc_separators {
            out.extend_from_slice(b"---\n");
        }
        Ok(())
    }

    fn print_leading_content(&mut self, out: &mut Vec<u8>, content: &str) -> Res<()> {
        print_leading_content(out, content, self.prefs.print_doc_separators, self.prefs.colors_enabled);
        Ok(())
    }

    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()> {
        let n = e.n(node);
        let line_ending = if n.leading_content.contains("\r\n") { "\r\n" } else { "\n" };
        if n.kind == SCALAR && self.prefs.unwrap_scalar {
            let mut v = n.value.clone();
            if n.leading_content.is_empty() || !v.is_empty() {
                v.push_str(line_ending);
            }
            out.extend_from_slice(v.as_bytes());
            return Ok(());
        }
        let indent = self.prefs.indent.clamp(2, 9);
        let mut target = marshal_yaml(e, node);
        let trailing = std::mem::take(&mut target.foot_comment);
        let opts = yaml::serializer::DumpOptions { indent, compact_seq_indent: self.prefs.compact_sequence_indent, line_width: -1 };
        let dumped = yaml::serializer::dump(&target, opts).map_err(|err| err.to_string())?;
        if self.prefs.colors_enabled {
            let mut buf = dumped;
            print_leading_content(&mut buf, &trailing, self.prefs.print_doc_separators, true);
            out.extend_from_slice(&crate::color::colorize_yaml(&buf));
            return Ok(());
        }
        out.extend_from_slice(&dumped);
        print_leading_content(out, &trailing, self.prefs.print_doc_separators, false);
        Ok(())
    }
}

