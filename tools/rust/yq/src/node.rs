//! yq's CandidateNode, kept in an arena: nodes refer to their parent, key and alias target by
//! index, as the Go structs do by pointer. Nodes are never freed (yq is a short-lived process).

use crate::prefs::Prefs;

pub type NodeId = usize;

// Kind (as yq's, bit flags; 0 is "no kind")
pub const SEQ: u32 = 1;
pub const MAP: u32 = 2;
pub const SCALAR: u32 = 4;
pub const ALIAS: u32 = 8;

// Style
pub const TAGGED: u32 = 1;
pub const DOUBLE: u32 = 2;
pub const SINGLE: u32 = 4;
pub const LITERAL: u32 = 8;
pub const FOLDED: u32 = 16;
pub const FLOW: u32 = 32;

#[derive(Clone, Debug, Default)]
pub struct CNode {
    pub kind: u32,
    pub style: u32,
    pub tag: String,
    pub value: String,
    pub anchor: String,
    pub alias: Option<NodeId>,
    pub content: Vec<NodeId>,
    pub head_comment: String,
    pub line_comment: String,
    pub foot_comment: String,
    pub parent: Option<NodeId>,
    pub key: Option<NodeId>,
    pub leading_content: String,
    pub document: u64,
    pub filename: std::rc::Rc<str>,
    pub line: i64,
    pub column: i64,
    pub file_index: i64,
    pub evaluate_together: bool,
    pub is_map_key: bool,
    pub encode_hint: u8,
}

impl CNode {
    pub fn scalar(tag: &str, value: &str) -> CNode {
        CNode { kind: SCALAR, tag: tag.into(), value: value.into(), filename: "".into(), ..Default::default() }
    }
    pub fn new(kind: u32, tag: &str) -> CNode {
        CNode { kind, tag: tag.into(), filename: "".into(), ..Default::default() }
    }
}

/// A path element: a map key or a sequence index.
#[derive(Clone, Debug, PartialEq)]
pub enum PathElem {
    Str(String),
    Int(i64),
}

impl PathElem {
    pub fn to_go_string(&self) -> String {
        match self {
            PathElem::Str(s) => s.clone(),
            PathElem::Int(i) => i.to_string(),
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct AssignPrefs {
    pub dont_overwrite_anchor: bool,
    pub only_write_null: bool,
    pub clobber_custom_tags: bool,
}

/// The evaluation state: the arena of nodes and the configured preferences.
pub struct Engine {
    pub nodes: Vec<CNode>,
    pub prefs: Prefs,
    pub show_merge_warning: bool,
    pub parse_cache: std::collections::HashMap<String, crate::parser::Expr>,
}

impl Engine {
    pub fn new(prefs: Prefs) -> Engine {
        Engine { nodes: Vec::with_capacity(1024), prefs, show_merge_warning: true, parse_cache: Default::default() }
    }

    #[inline]
    pub fn n(&self, id: NodeId) -> &CNode {
        &self.nodes[id]
    }
    #[inline]
    pub fn nm(&mut self, id: NodeId) -> &mut CNode {
        &mut self.nodes[id]
    }

    pub fn add(&mut self, n: CNode) -> NodeId {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    pub fn scalar(&mut self, tag: &str, value: &str) -> NodeId {
        self.add(CNode::scalar(tag, value))
    }

    /// createScalarNode with an untyped (string) value: !!str, or !!merge for "<<".
    pub fn string_scalar(&mut self, value: &str) -> NodeId {
        let tag = if value == "<<" { "!!merge" } else { "!!str" };
        self.scalar(tag, value)
    }

    pub fn create_child(&mut self, parent: NodeId) -> NodeId {
        self.add(CNode { parent: Some(parent), filename: "".into(), ..Default::default() })
    }

    pub fn get_document(&self, id: NodeId) -> u64 {
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            n = p;
        }
        self.nodes[n].document
    }

    pub fn get_filename(&self, id: NodeId) -> std::rc::Rc<str> {
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            n = p;
        }
        self.nodes[n].filename.clone()
    }

    pub fn get_file_index(&self, id: NodeId) -> i64 {
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            n = p;
        }
        self.nodes[n].file_index
    }

    pub fn set_document(&mut self, id: NodeId, idx: u64) {
        self.nodes[id].document = idx;
    }

    pub fn get_key(&self, id: NodeId) -> String {
        let n = &self.nodes[id];
        let mut s = String::new();
        if n.is_map_key {
            s.push_str("key-");
            s.push_str(&n.value);
            s.push('-');
        }
        s.push_str(&self.get_document(id).to_string());
        s.push_str(" - ");
        if let Some(k) = n.key {
            s.push_str(&self.nodes[k].value);
        }
        s
    }

    pub fn get_parsed_key(&self, id: NodeId) -> Option<PathElem> {
        let n = &self.nodes[id];
        if n.is_map_key {
            return Some(PathElem::Str(n.value.clone()));
        }
        let k = &self.nodes[n.key?];
        if k.tag == "!!str" {
            return Some(PathElem::Str(k.value.clone()));
        }
        match crate::util::parse_int(&k.value) {
            Ok(i) => Some(PathElem::Int(i)),
            Err(_) => Some(PathElem::Str(k.value.clone())),
        }
    }

    pub fn get_path(&self, id: NodeId) -> Vec<PathElem> {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            match self.get_parsed_key(c) {
                Some(k) => {
                    chain.push(k);
                    cur = self.nodes[c].parent;
                }
                None => break,
            }
        }
        chain.reverse();
        chain
    }

    pub fn get_nice_path(&self, id: NodeId) -> String {
        let mut sb = String::new();
        for (i, e) in self.get_path(id).iter().enumerate() {
            match e {
                PathElem::Int(n) => {
                    sb.push('[');
                    sb.push_str(&n.to_string());
                    sb.push(']');
                }
                PathElem::Str(s) => {
                    if i == 0 {
                        sb.push_str(s);
                    } else if s.contains('.') {
                        sb.push('[');
                        sb.push_str(s);
                        sb.push(']');
                    } else {
                        sb.push('.');
                        sb.push_str(s);
                    }
                }
            }
        }
        sb
    }

    pub fn add_key_value_child(&mut self, n: NodeId, raw_key: NodeId, raw_value: NodeId) -> (NodeId, NodeId) {
        let key = self.copy(raw_key);
        self.nodes[key].parent = Some(n);
        self.nodes[key].is_map_key = true;
        let value = self.copy(raw_value);
        self.nodes[value].parent = Some(n);
        self.nodes[value].is_map_key = false;
        self.nodes[value].key = Some(key);
        self.nodes[n].content.push(key);
        self.nodes[n].content.push(value);
        (key, value)
    }

    pub fn add_child(&mut self, n: NodeId, raw: NodeId) {
        let value = self.copy(raw);
        self.nodes[value].parent = Some(n);
        self.nodes[value].is_map_key = false;
        let index = self.nodes[n].content.len();
        let key = self.scalar("!!int", &index.to_string());
        self.nodes[key].parent = Some(n);
        self.nodes[value].key = Some(key);
        self.nodes[n].content.push(value);
    }

    pub fn add_children(&mut self, n: NodeId, children: &[NodeId]) {
        if self.nodes[n].kind == MAP {
            let mut i = 0;
            while i + 1 < children.len() {
                self.add_key_value_child(n, children[i], children[i + 1]);
                i += 2;
            }
        } else {
            for c in children {
                self.add_child(n, *c);
            }
        }
    }

    fn do_copy(&mut self, id: NodeId, clone_content: bool) -> NodeId {
        let copy_key = self.nodes[id].key.map(|k| self.copy(k));
        let src = &self.nodes[id];
        let clone = CNode {
            kind: src.kind,
            style: src.style,
            tag: src.tag.clone(),
            value: src.value.clone(),
            anchor: src.anchor.clone(),
            alias: src.alias,
            content: Vec::new(),
            head_comment: src.head_comment.clone(),
            line_comment: src.line_comment.clone(),
            foot_comment: src.foot_comment.clone(),
            parent: src.parent,
            key: copy_key,
            leading_content: src.leading_content.clone(),
            document: src.document,
            filename: src.filename.clone(),
            line: src.line,
            column: src.column,
            file_index: src.file_index,
            evaluate_together: src.evaluate_together,
            is_map_key: src.is_map_key,
            encode_hint: src.encode_hint,
        };
        let content = if clone_content { src.content.clone() } else { Vec::new() };
        let c = self.add(clone);
        if clone_content {
            self.add_children(c, &content);
        }
        c
    }

    pub fn copy(&mut self, id: NodeId) -> NodeId {
        self.do_copy(id, true)
    }

    pub fn copy_without_content(&mut self, id: NodeId) -> NodeId {
        self.do_copy(id, false)
    }

    pub fn copy_as_replacement(&mut self, n: NodeId, replacement: NodeId) -> NodeId {
        let c = self.copy(replacement);
        self.nodes[c].parent = self.nodes[n].parent;
        if self.nodes[n].is_map_key {
            self.nodes[c].key = Some(n);
        } else {
            self.nodes[c].key = self.nodes[n].key;
        }
        c
    }

    pub fn create_replacement(&mut self, n: NodeId, kind: u32, tag: &str, value: &str) -> NodeId {
        let node = self.add(CNode { kind, tag: tag.into(), value: value.into(), filename: "".into(), ..Default::default() });
        self.copy_as_replacement(n, node)
    }

    pub fn create_replacement_with_comments(&mut self, n: NodeId, kind: u32, tag: &str, style: u32) -> NodeId {
        let r = self.create_replacement(n, kind, tag, "");
        let src = self.nodes[n].clone_comments();
        let d = &mut self.nodes[r];
        d.leading_content = src.0;
        d.head_comment = src.1;
        d.line_comment = src.2;
        d.foot_comment = src.3;
        d.style = style;
        r
    }

    /// The tag of a custom-tagged scalar's value, as if it were untagged YAML.
    pub fn guess_tag_from_custom_type(&mut self, id: NodeId) -> String {
        let n = &self.nodes[id];
        if n.tag.starts_with("!!") {
            return n.tag.clone();
        } else if n.value.is_empty() {
            return n.tag.clone();
        }
        let value = n.value.clone();
        match crate::formats::yaml::parse_snippet(self, &value) {
            Ok(s) => self.nodes[s].tag.clone(),
            Err(_) => self.nodes[id].tag.clone(),
        }
    }

    pub fn update_from(&mut self, n: NodeId, other: NodeId, prefs: AssignPrefs) {
        if n == other {
            return;
        }
        let empty_collection = self.nodes[n].kind != SCALAR && self.nodes[n].content.is_empty();
        if empty_collection || self.guess_tag_from_custom_type(n) != self.guess_tag_from_custom_type(other) {
            self.nodes[n].style = self.nodes[other].style;
        }
        self.nodes[n].content = Vec::new();
        self.nodes[n].kind = self.nodes[other].kind;
        let other_content = self.nodes[other].content.clone();
        self.add_children(n, &other_content);
        self.nodes[n].value = self.nodes[other].value.clone();
        self.update_attributes_from(n, other, prefs);
    }

    pub fn update_attributes_from(&mut self, n: NodeId, other: NodeId, prefs: AssignPrefs) {
        let o = self.nodes[other].clone_attrs();
        let d = &mut self.nodes[n];
        if d.kind != o.kind {
            d.content = Vec::new();
            d.value = String::new();
        }
        d.kind = o.kind;
        if prefs.clobber_custom_tags || d.tag.starts_with("!!") || d.tag.is_empty() {
            d.tag = o.tag;
        }
        d.alias = o.alias;
        if !prefs.dont_overwrite_anchor {
            d.anchor = o.anchor;
        }
        d.encode_hint = o.encode_hint;
        if d.style == 0 {
            d.style = o.style;
        }
        if !o.foot_comment.is_empty() {
            d.foot_comment = o.foot_comment;
        }
        if !o.head_comment.is_empty() {
            d.head_comment = o.head_comment;
        }
        if !o.line_comment.is_empty() {
            d.line_comment = o.line_comment;
        }
    }

    /// Follows aliases to the node they refer to (resolveAliasChain).
    pub fn resolve_alias_chain(&self, id: NodeId) -> Result<NodeId, String> {
        let mut node = id;
        if self.nodes[node].kind != ALIAS {
            return Ok(node);
        }
        let mut visited = std::collections::HashSet::new();
        while self.nodes[node].kind == ALIAS {
            if !visited.insert(node) {
                return Err("alias cycle detected".into());
            }
            match self.nodes[node].alias {
                Some(a) => node = a,
                None => return Err("alias cycle detected".into()),
            }
        }
        Ok(node)
    }
}

struct Attrs {
    kind: u32,
    tag: String,
    alias: Option<NodeId>,
    anchor: String,
    encode_hint: u8,
    style: u32,
    foot_comment: String,
    head_comment: String,
    line_comment: String,
}

impl CNode {
    fn clone_comments(&self) -> (String, String, String, String) {
        (self.leading_content.clone(), self.head_comment.clone(), self.line_comment.clone(), self.foot_comment.clone())
    }
    fn clone_attrs(&self) -> Attrs {
        Attrs {
            kind: self.kind,
            tag: self.tag.clone(),
            alias: self.alias,
            anchor: self.anchor.clone(),
            encode_hint: self.encode_hint,
            style: self.style,
            foot_comment: self.foot_comment.clone(),
            head_comment: self.head_comment.clone(),
            line_comment: self.line_comment.clone(),
        }
    }
}
