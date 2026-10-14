//! The reader and scanner (go-yaml's reader.go and scanner.go): characters to tokens, with the
//! comments collected beside them.

use super::parser::{Comment, Parser, SimpleKey};
use super::*;

type R<T> = Result<T, LoadError>;

/// The whole input decoded to UTF-8, as go-yaml's reader would hand it over: the characters up to
/// the first decoding error, and that error (raised when the scanner wants a character past it).
pub(crate) fn decode_input(raw: &[u8]) -> (Vec<u8>, i64, Option<LoadError>) {
    let reader_error = |msg: String| LoadError::new(Stage::Reader, msg, Mark::default());
    let mut out = Vec::with_capacity(raw.len());
    let mut chars = 0i64;
    let mut pos;
    let utf16: Option<bool>; // Some(little endian)
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        utf16 = Some(true);
        pos = 2;
    } else if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        utf16 = Some(false);
        pos = 2;
    } else if raw.len() >= 3 && raw[0] == 0xEF && raw[1] == 0xBB && raw[2] == 0xBF {
        utf16 = None;
        pos = 3;
    } else {
        utf16 = None;
        pos = 0;
    }
    loop {
        if pos >= raw.len() {
            return (out, chars, None);
        }
        let value: u32;
        let w: usize;
        match utf16 {
            None => {
                let octet = raw[pos];
                w = width(octet);
                if w == 0 {
                    return (out, chars, Some(reader_error(format!("invalid leading UTF-8 octet (value: {octet})"))));
                }
                if w > raw.len() - pos {
                    return (out, chars, Some(reader_error("incomplete UTF-8 octet sequence".into())));
                }
                let mut v = match w {
                    1 => (octet & 0x7F) as u32,
                    2 => (octet & 0x1F) as u32,
                    3 => (octet & 0x0F) as u32,
                    _ => (octet & 0x07) as u32,
                };
                for k in 1..w {
                    let o = raw[pos + k];
                    if o & 0xC0 != 0x80 {
                        return (out, chars, Some(reader_error(format!("invalid trailing UTF-8 octet (value: {o})"))));
                    }
                    v = (v << 6) + (o & 0x3F) as u32;
                }
                let ok = w == 1 || (w == 2 && v >= 0x80) || (w == 3 && v >= 0x800) || (w == 4 && v >= 0x10000);
                if !ok {
                    return (out, chars, Some(reader_error("invalid length of a UTF-8 sequence".into())));
                }
                if (0xD800..=0xDFFF).contains(&v) || v > 0x10FFFF {
                    return (out, chars, Some(reader_error(format!("invalid Unicode character (value: {v})"))));
                }
                value = v;
            }
            Some(le) => {
                let (lo, hi) = if le { (0, 1) } else { (1, 0) };
                if raw.len() - pos < 2 {
                    return (out, chars, Some(reader_error("incomplete UTF-16 character".into())));
                }
                let mut v = raw[pos + lo] as u32 + ((raw[pos + hi] as u32) << 8);
                if v & 0xFC00 == 0xDC00 {
                    return (out, chars, Some(reader_error(format!("unexpected low surrogate area (value: {v})"))));
                }
                if v & 0xFC00 == 0xD800 {
                    if raw.len() - pos < 4 {
                        return (out, chars, Some(reader_error("incomplete UTF-16 surrogate pair".into())));
                    }
                    let v2 = raw[pos + lo + 2] as u32 + ((raw[pos + hi + 2] as u32) << 8);
                    if v2 & 0xFC00 != 0xDC00 {
                        return (out, chars, Some(reader_error(format!("expected low surrogate area (value: {v2})"))));
                    }
                    v = 0x10000 + ((v & 0x3FF) << 10) + (v2 & 0x3FF);
                    w = 4;
                } else {
                    w = 2;
                }
                value = v;
            }
        }
        let allowed = value == 0x09
            || value == 0x0A
            || value == 0x0D
            || (0x20..=0x7E).contains(&value)
            || (0x7F..=0x9F).contains(&value)
            || (0xA0..=0xD7FF).contains(&value)
            || (0xE000..=0xFFFD).contains(&value)
            || (0x10000..=0x10FFFF).contains(&value);
        if !allowed {
            return (out, chars, Some(reader_error(format!("control characters are not allowed (value: {value})"))));
        }
        pos += w;
        let c = char::from_u32(value).unwrap_or('\u{FFFD}');
        let mut tmp = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
        chars += 1;
    }
}

fn scanner_error(problem: &str, mark: Mark) -> LoadError {
    LoadError::new(Stage::Scanner, problem, mark)
}
fn scanner_error_context(context: &str, context_mark: Mark, problem: &str, mark: Mark) -> LoadError {
    LoadError::with_context(Stage::Scanner, context, context_mark, problem, mark)
}

const MAX_NUMBER_LENGTH: i32 = 2;

impl Parser {
    // ---- the reader's interface ----

    /// Makes sure `length` characters are available (libyaml's CACHE).
    #[inline]
    pub(crate) fn cache(&mut self, length: i64) -> R<()> {
        if self.unread >= length {
            return Ok(());
        }
        if let Some(e) = &self.reader_error {
            return Err(e.clone());
        }
        Ok(())
    }
    #[inline]
    pub(crate) fn b(&self, off: usize) -> u8 {
        at(&self.buffer, self.buffer_pos + off)
    }
    #[inline]
    fn buf(&self) -> &[u8] {
        &self.buffer
    }

    pub(crate) fn skip(&mut self) {
        if !is_blank(&self.buffer, self.buffer_pos) {
            self.newlines = 0;
        }
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread -= 1;
        self.buffer_pos += width(self.b(0)).max(1);
    }

    pub(crate) fn skip_line(&mut self) {
        if is_crlf(&self.buffer, self.buffer_pos) {
            self.mark.index += 2;
            self.mark.column = 1;
            self.mark.line += 1;
            self.unread -= 2;
            self.buffer_pos += 2;
            self.newlines += 1;
        } else if is_line_break(&self.buffer, self.buffer_pos) {
            self.mark.index += 1;
            self.mark.column = 1;
            self.mark.line += 1;
            self.unread -= 1;
            self.buffer_pos += width(self.b(0));
            self.newlines += 1;
        }
    }

    pub(crate) fn read(&mut self, s: &mut Vec<u8>) {
        if !is_blank(&self.buffer, self.buffer_pos) {
            self.newlines = 0;
        }
        let w = width(self.b(0)).max(1);
        let end = (self.buffer_pos + w).min(self.buffer.len());
        s.extend_from_slice(&self.buffer[self.buffer_pos..end]);
        self.buffer_pos += w;
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread -= 1;
    }

    pub(crate) fn read_line(&mut self, s: &mut Vec<u8>) {
        let (c0, c1, c2) = (self.b(0), self.b(1), self.b(2));
        if c0 == b'\r' && c1 == b'\n' {
            s.push(b'\n');
            self.buffer_pos += 2;
            self.mark.index += 1;
            self.unread -= 1;
        } else if c0 == b'\r' || c0 == b'\n' {
            s.push(b'\n');
            self.buffer_pos += 1;
        } else if c0 == 0xC2 && c1 == 0x85 {
            s.push(b'\n');
            self.buffer_pos += 2;
        } else if c0 == 0xE2 && c1 == 0x80 && (c2 == 0xA8 || c2 == 0xA9) {
            s.extend_from_slice(&[c0, c1, c2]);
            self.buffer_pos += 3;
        } else {
            return;
        }
        self.mark.index += 1;
        self.mark.column = 1;
        self.mark.line += 1;
        self.unread -= 1;
        self.newlines += 1;
    }

    // ---- the scanner ----

    /// The next token, removed from the queue.
    pub fn scan(&mut self) -> R<Token> {
        if let Some(e) = &self.last_error {
            return Err(e.clone());
        }
        if self.stream_end_produced {
            return Ok(Token::default());
        }
        if !self.token_available {
            if let Err(e) = self.fetch_more_tokens() {
                self.last_error = Some(e.clone());
                return Err(e);
            }
        }
        let token = self.tokens[self.tokens_head].clone();
        self.tokens_head += 1;
        self.tokens_parsed += 1;
        self.token_available = false;
        if token.typ == TokenType::StreamEnd {
            self.stream_end_produced = true;
        }
        Ok(token)
    }

    pub(crate) fn fetch_more_tokens(&mut self) -> R<()> {
        loop {
            if (self.tokens_head as i64) < self.tokens.len() as i64 - 2 {
                let first_key;
                let mut found = false;
                if !self.simple_key_stack.is_empty() {
                    first_key = self.simple_key_stack[0].token_number;
                    found = true;
                } else if self.simple_key_possible {
                    first_key = self.simple_key.token_number;
                    found = true;
                } else {
                    first_key = 0;
                }
                if !found || self.tokens_parsed != first_key {
                    break;
                }
            }
            self.fetch_next_token()?;
        }
        self.token_available = true;
        Ok(())
    }

    fn fetch_next_token(&mut self) -> R<()> {
        self.cache(1)?;
        if !self.stream_start_produced {
            self.fetch_stream_start();
            return Ok(());
        }
        let scan_mark = self.mark;
        self.scan_to_next_token()?;
        self.unroll_indent(self.mark.column, scan_mark);
        self.cache(4)?;
        if is_z(self.buf(), self.buffer_pos) {
            return self.fetch_stream_end();
        }
        let (c0, c1, c2) = (self.b(0), self.b(1), self.b(2));
        if self.mark.column == 1 && c0 == b'%' {
            return self.fetch_directive();
        }
        if self.mark.column == 1 && c0 == b'-' && c1 == b'-' && c2 == b'-' && is_blankz(self.buf(), self.buffer_pos + 3) {
            return self.fetch_document_indicator(TokenType::DocumentStart);
        }
        if self.mark.column == 1 && c0 == b'.' && c1 == b'.' && c2 == b'.' && is_blankz(self.buf(), self.buffer_pos + 3) {
            return self.fetch_document_indicator(TokenType::DocumentEnd);
        }
        let mut comment_mark = self.mark;
        if !self.tokens.is_empty() && (self.flow_level == 0 && c0 == b':' || self.flow_level > 0 && c0 == b',') {
            comment_mark = self.tokens[self.tokens.len() - 1].start_mark;
        }
        self.fetch_token_at(c0)?;
        // go-yaml scans the rest of the line for a comment after every token (a deferred call).
        if self.skip_comments {
            return Ok(());
        }
        if !self.tokens.is_empty() && self.tokens[self.tokens.len() - 1].typ == TokenType::BlockEntry {
            return Ok(());
        }
        self.scan_line_comment(comment_mark)
    }

    fn fetch_token_at(&mut self, c: u8) -> R<()> {
        let b = &self.buffer;
        let p = self.buffer_pos;
        match c {
            b'[' => return self.fetch_flow_collection_start(TokenType::FlowSequenceStart),
            b'{' => return self.fetch_flow_collection_start(TokenType::FlowMappingStart),
            b']' => return self.fetch_flow_collection_end(TokenType::FlowSequenceEnd),
            b'}' => return self.fetch_flow_collection_end(TokenType::FlowMappingEnd),
            b',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if c == b'-' && is_blankz(b, p + 1) {
            return self.fetch_block_entry();
        }
        if c == b'?' && is_blankz(b, p + 1) {
            return self.fetch_key();
        }
        if c == b':' && (self.flow_level > 0 && !self.is_flow_sequence() || is_blankz(b, p + 1)) {
            return self.fetch_value();
        }
        match c {
            b'*' => return self.fetch_anchor(TokenType::Alias),
            b'&' => return self.fetch_anchor(TokenType::Anchor),
            b'!' => return self.fetch_tag(),
            b'|' if self.flow_level == 0 => return self.fetch_block_scalar(true),
            b'>' if self.flow_level == 0 => return self.fetch_block_scalar(false),
            b'\'' => return self.fetch_flow_scalar(true),
            b'"' => return self.fetch_flow_scalar(false),
            _ => {}
        }
        let b = &self.buffer;
        let special = is_blankz(b, p)
            || matches!(
                c,
                b'-' | b'?' | b':' | b',' | b'[' | b']' | b'{' | b'}' | b'#' | b'&' | b'*' | b'!' | b'|' | b'>' | b'\''
                    | b'"' | b'%' | b'@' | b'`'
            );
        if !special || (c == b'-' && !is_blank(b, p + 1)) || ((c == b'?' || c == b':') && !is_blankz(b, p + 1)) {
            return self.fetch_plain_scalar();
        }
        Err(scanner_error_context(
            "while scanning for the next token",
            self.mark,
            "found character that cannot start any token",
            self.mark,
        ))
    }

    fn fetch_stream_start(&mut self) {
        self.indent = -1;
        self.simple_key = SimpleKey::default();
        self.simple_key_stack.clear();
        self.simple_key_allowed = true;
        self.stream_start_produced = true;
        let token = Token { typ: TokenType::StreamStart, start_mark: self.mark, end_mark: self.mark, ..Default::default() };
        self.insert_token(-1, token);
    }

    fn fetch_stream_end(&mut self) -> R<()> {
        if self.mark.column != 1 {
            self.mark.column = 1;
            self.mark.line += 1;
        }
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key = SimpleKey::default();
        self.simple_key_stack.clear();
        self.simple_key_allowed = false;
        let token = Token { typ: TokenType::StreamEnd, start_mark: self.mark, end_mark: self.mark, ..Default::default() };
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_directive(&mut self) -> R<()> {
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_directive()?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_document_indicator(&mut self, typ: TokenType) -> R<()> {
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let start_mark = self.mark;
        self.skip();
        self.skip();
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, typ: TokenType) -> R<()> {
        self.save_simple_key()?;
        self.increase_flow_level()?;
        self.simple_key_allowed = true;
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, typ: TokenType) -> R<()> {
        self.remove_simple_key()?;
        self.decrease_flow_level();
        self.simple_key_allowed = false;
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> R<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ: TokenType::FlowEntry, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(scanner_error("block sequence entries are not allowed in this context", self.mark));
            }
            self.roll_indent(self.mark.column, -1, TokenType::BlockSequenceStart, self.mark)?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ: TokenType::BlockEntry, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_key(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(scanner_error("mapping keys are not allowed in this context", self.mark));
            }
            self.roll_indent(self.mark.column, -1, TokenType::BlockMappingStart, self.mark)?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = self.flow_level == 0;
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ: TokenType::Key, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_value(&mut self) -> R<()> {
        if self.simple_key_possible && self.simple_key.mark.line == self.mark.line {
            let sk = self.simple_key;
            let token = Token { typ: TokenType::Key, start_mark: sk.mark, end_mark: sk.mark, ..Default::default() };
            self.insert_token(sk.token_number - self.tokens_parsed, token);
            self.roll_indent(sk.mark.column, sk.token_number, TokenType::BlockMappingStart, sk.mark)?;
            self.simple_key_possible = false;
            self.simple_key.required = false;
            self.simple_key_allowed = false;
        } else {
            if self.flow_level == 0 {
                if !self.simple_key_allowed {
                    return Err(scanner_error("mapping values are not allowed in this context", self.mark));
                }
                self.roll_indent(self.mark.column, -1, TokenType::BlockMappingStart, self.mark)?;
            }
            self.simple_key_allowed = self.flow_level == 0;
        }
        let start_mark = self.mark;
        self.skip();
        let end_mark = self.mark;
        self.insert_token(-1, Token { typ: TokenType::Value, start_mark, end_mark, ..Default::default() });
        Ok(())
    }

    fn fetch_anchor(&mut self, typ: TokenType) -> R<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_anchor(typ)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_tag(&mut self) -> R<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_tag()?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, literal: bool) -> R<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let token = self.scan_block_scalar(literal)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, single: bool) -> R<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_flow_scalar(single)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_plain_scalar(&mut self) -> R<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_plain_scalar()?;
        self.insert_token(-1, token);
        Ok(())
    }

    // ---- simple keys, indentation ----

    fn save_simple_key(&mut self) -> R<()> {
        let required = self.flow_level == 0 && self.indent == self.mark.column;
        if self.simple_key_allowed {
            self.remove_simple_key()?;
            self.simple_key_possible = true;
            self.simple_key = SimpleKey {
                required,
                flow_level: self.flow_level,
                token_number: self.tokens_parsed + (self.tokens.len() as i64 - self.tokens_head as i64),
                mark: self.mark,
            };
        }
        Ok(())
    }

    fn remove_simple_key(&mut self) -> R<()> {
        if self.simple_key.required {
            return Err(scanner_error_context(
                "while scanning a simple key",
                self.simple_key.mark,
                "could not find expected ':'",
                self.mark,
            ));
        }
        self.simple_key_possible = false;
        Ok(())
    }

    fn is_flow_sequence(&self) -> bool {
        match self.tokens.last() {
            None => false,
            Some(t) => t.typ == TokenType::FlowEntry || t.typ == TokenType::FlowSequenceStart,
        }
    }

    fn increase_flow_level(&mut self) -> R<()> {
        self.flow_level += 1;
        if self.flow_level > self.max_depth {
            return Err(scanner_error_context(
                "while increasing flow level",
                self.simple_key.mark,
                &format!("exceeded max depth of {}", self.max_depth),
                self.mark,
            ));
        }
        if self.simple_key_possible {
            self.simple_key_stack.push(self.simple_key);
        }
        self.simple_key = SimpleKey::default();
        Ok(())
    }

    fn decrease_flow_level(&mut self) {
        if self.flow_level > 0 {
            self.flow_level -= 1;
            if let Some(last) = self.simple_key_stack.last() {
                if last.flow_level == self.flow_level {
                    self.simple_key = *last;
                    self.simple_key_stack.pop();
                    self.simple_key_possible = true;
                }
            }
        }
    }

    fn roll_indent(&mut self, column: i64, number: i64, typ: TokenType, mark: Mark) -> R<()> {
        if self.flow_level > 0 {
            return Ok(());
        }
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            if self.indents.len() as i64 > self.max_depth {
                return Err(scanner_error_context(
                    "while increasing indent level",
                    self.simple_key.mark,
                    &format!("exceeded max depth of {}", self.max_depth),
                    self.mark,
                ));
            }
            let token = Token { typ, start_mark: mark, end_mark: mark, ..Default::default() };
            let mut number = number;
            if number > -1 {
                number -= self.tokens_parsed;
            }
            self.insert_token(number, token);
        }
        Ok(())
    }

    fn unroll_indent(&mut self, column: i64, scan_mark: Mark) {
        if self.flow_level > 0 {
            return;
        }
        let mut block_mark = scan_mark;
        block_mark.index -= 1;
        while self.indent > column {
            let mut stop_index = block_mark.index;
            for i in (0..self.comments.len()).rev() {
                let comment = &self.comments[i];
                if comment.end_mark.index < stop_index {
                    break;
                }
                if comment.start_mark.column == self.indent + 1 {
                    block_mark = comment.start_mark;
                }
                stop_index = comment.scan_mark.index;
            }
            let token = Token { typ: TokenType::BlockEnd, start_mark: block_mark, end_mark: block_mark, ..Default::default() };
            self.insert_token(-1, token);
            self.indent = self.indents.pop().unwrap_or(-1);
        }
    }

    fn insert_token(&mut self, pos: i64, token: Token) {
        if self.tokens_head > 64 && self.tokens_head * 2 > self.tokens.len() {
            self.tokens.drain(..self.tokens_head);
            self.tokens_head = 0;
        }
        if pos < 0 {
            self.tokens.push(token);
            return;
        }
        let at = self.tokens_head + pos as usize;
        self.tokens.insert(at.min(self.tokens.len()), token);
    }

    // ---- whitespace and comments ----

    fn scan_to_next_token(&mut self) -> R<()> {
        let scan_mark = self.mark;
        loop {
            self.cache(1)?;
            if self.mark.column == 1 && is_bom(self.buf(), self.buffer_pos) {
                self.skip();
            }
            self.cache(1)?;
            while self.b(0) == b' ' || ((self.flow_level > 0 || !self.simple_key_allowed) && self.b(0) == b'\t') {
                self.skip();
                self.cache(1)?;
            }
            // A line comment after a "- " entry that is followed by more content on the next
            // line is the head of what follows.
            if !self.comments.is_empty() && self.tokens.len() > 1 {
                let ta = self.tokens[self.tokens.len() - 2].typ;
                let tb = self.tokens[self.tokens.len() - 1].typ;
                let lb = is_line_break(self.buf(), self.buffer_pos);
                let line = self.mark.line;
                let mark = self.mark;
                let comment = self.comments.last_mut().unwrap();
                if ta == TokenType::BlockSequenceStart && tb == TokenType::BlockEntry && !comment.line.is_empty() && !lb {
                    comment.head = std::mem::take(&mut comment.line);
                    if comment.start_mark.line == line - 1 {
                        comment.token_mark = mark;
                    }
                }
            }
            if self.b(0) == b'#' {
                if self.skip_comments {
                    while !is_line_break(self.buf(), self.buffer_pos) && !is_z(self.buf(), self.buffer_pos) {
                        self.skip();
                        self.cache(1)?;
                    }
                } else {
                    self.scan_comments(scan_mark)?;
                }
            }
            if is_line_break(self.buf(), self.buffer_pos) {
                self.cache(2)?;
                self.skip_line();
                if self.flow_level == 0 {
                    self.simple_key_allowed = true;
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    fn scan_line_comment(&mut self, token_mark: Mark) -> R<()> {
        if self.newlines > 0 {
            return Ok(());
        }
        let mut start_mark = Mark::default();
        let mut text = Vec::new();
        let mut peek = 0usize;
        while peek < 512 {
            if self.unread < peek as i64 + 1 && self.cache(peek as i64 + 1).is_err() {
                break;
            }
            if is_blank(self.buf(), self.buffer_pos + peek) {
                peek += 1;
                continue;
            }
            if self.b(peek) == b'#' {
                let seen = self.mark.index + peek as i64;
                loop {
                    self.cache(1)?;
                    if is_breakz(self.buf(), self.buffer_pos) {
                        if self.mark.index >= seen {
                            break;
                        }
                        self.cache(2)?;
                        self.skip_line();
                    } else if self.mark.index >= seen {
                        if text.is_empty() {
                            start_mark = self.mark;
                        }
                        self.read(&mut text);
                    } else {
                        self.skip();
                    }
                }
            }
            break;
        }
        if !text.is_empty() {
            self.comments.push(Comment {
                scan_mark: token_mark,
                token_mark,
                start_mark,
                end_mark: self.mark,
                line: text,
                ..Default::default()
            });
        }
        Ok(())
    }

    fn scan_comments(&mut self, scan_mark_in: Mark) -> R<()> {
        let mut scan_mark = scan_mark_in;
        let mut token = self.tokens[self.tokens.len() - 1].clone();
        if token.typ == TokenType::FlowEntry && self.tokens.len() > 1 {
            token = self.tokens[self.tokens.len() - 2].clone();
        }
        let mut token_mark = token.start_mark;
        let mut start_mark = Mark::default();
        let mut next_indent = self.indent.max(0);
        let mut recent_empty = false;
        let mut first_empty = self.newlines <= 1;
        let mut line = self.mark.line;
        let mut column = self.mark.column;
        let mut text: Vec<u8> = Vec::new();
        let mut foot_line: i64 = -1;
        if scan_mark.line > 0 {
            foot_line = self.mark.line - self.newlines + 1;
            if self.newlines == 0 && self.mark.column > 1 {
                foot_line += 1;
            }
        }
        let mut peek: i64 = 0;
        while peek < 512 {
            if self.unread < peek + 1 && self.cache(peek + 1).is_err() {
                break;
            }
            column += 1;
            let p = self.buffer_pos + peek as usize;
            if is_blank(self.buf(), p) {
                peek += 1;
                continue;
            }
            let c = at(self.buf(), p);
            let close_flow = self.flow_level > 0 && (c == b']' || c == b'}');
            if close_flow || is_breakz(self.buf(), p) {
                if close_flow || !recent_empty {
                    if close_flow
                        || first_empty
                            && (start_mark.line == foot_line && token.typ != TokenType::Value
                                || start_mark.column - 1 < next_indent)
                    {
                        if !text.is_empty() {
                            if start_mark.column - 1 < next_indent {
                                token_mark = start_mark;
                            }
                            let end = Mark::new(self.mark.index + peek, line, column);
                            self.comments.push(Comment {
                                scan_mark,
                                token_mark,
                                start_mark,
                                end_mark: end,
                                foot: std::mem::take(&mut text),
                                ..Default::default()
                            });
                            scan_mark = end;
                            token_mark = scan_mark;
                        }
                    } else if !text.is_empty() && at(self.buf(), p) != 0 {
                        text.push(b'\n');
                    }
                }
                if !is_line_break(self.buf(), p) {
                    break;
                }
                first_empty = false;
                recent_empty = true;
                column = 1;
                line += 1;
                peek += 1;
                continue;
            }
            if !text.is_empty() && (close_flow || column - 1 < next_indent && column != start_mark.column) {
                let end = Mark::new(self.mark.index + peek, line, column);
                self.comments.push(Comment {
                    scan_mark,
                    token_mark,
                    start_mark,
                    end_mark: end,
                    foot: std::mem::take(&mut text),
                    ..Default::default()
                });
                scan_mark = end;
                token_mark = scan_mark;
            }
            if at(self.buf(), p) != b'#' {
                break;
            }
            if text.is_empty() {
                start_mark = Mark::new(self.mark.index + peek, line, column);
            } else {
                text.push(b'\n');
            }
            recent_empty = false;
            let seen = self.mark.index + peek;
            loop {
                self.cache(1)?;
                if is_breakz(self.buf(), self.buffer_pos) {
                    if self.mark.index >= seen {
                        break;
                    }
                    self.cache(2)?;
                    self.skip_line();
                } else if self.mark.index >= seen {
                    self.read(&mut text);
                } else {
                    self.skip();
                }
            }
            column = 1;
            line = self.mark.line;
            next_indent = self.indent.max(0);
            // go-yaml sets peek to 0 here, and its `for ...; peek++` makes that 1.
            peek = 1;
        }
        if !text.is_empty() {
            self.comments.push(Comment {
                scan_mark,
                token_mark: start_mark,
                start_mark,
                end_mark: Mark::new(self.mark.index + peek - 1, line, column),
                head: text,
                ..Default::default()
            });
        }
        Ok(())
    }

    // ---- token scanners ----

    fn scan_directive(&mut self) -> R<Token> {
        let start_mark = self.mark;
        self.skip();
        let name = self.scan_directive_name(start_mark)?;
        let token;
        if name == b"YAML" {
            let (major, minor) = self.scan_version_directive_value(start_mark)?;
            token = Token { typ: TokenType::VersionDirective, start_mark, end_mark: self.mark, major, minor, ..Default::default() };
        } else if name == b"TAG" {
            let (handle, prefix) = self.scan_tag_directive_value(start_mark)?;
            token = Token { typ: TokenType::TagDirective, start_mark, end_mark: self.mark, value: handle, prefix, ..Default::default() };
        } else {
            return Err(scanner_error_context("while scanning a directive", start_mark, "found unknown directive name", self.mark));
        }
        self.cache(1)?;
        while is_blank(self.buf(), self.buffer_pos) {
            self.skip();
            self.cache(1)?;
        }
        if self.b(0) == b'#' {
            while !is_breakz(self.buf(), self.buffer_pos) {
                self.skip();
                self.cache(1)?;
            }
        }
        if !is_breakz(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a directive",
                start_mark,
                "did not find expected comment or line break",
                self.mark,
            ));
        }
        if is_line_break(self.buf(), self.buffer_pos) {
            self.cache(2)?;
            self.skip_line();
        }
        Ok(token)
    }

    fn scan_directive_name(&mut self, start_mark: Mark) -> R<Vec<u8>> {
        self.cache(1)?;
        let mut s = Vec::new();
        while is_alpha(self.buf(), self.buffer_pos) {
            self.read(&mut s);
            self.cache(1)?;
        }
        if s.is_empty() {
            return Err(scanner_error_context("while scanning a directive", start_mark, "could not find expected directive name", self.mark));
        }
        if !is_blankz(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a directive",
                start_mark,
                "found unexpected non-alphabetical character",
                self.mark,
            ));
        }
        Ok(s)
    }

    fn scan_version_directive_value(&mut self, start_mark: Mark) -> R<(i8, i8)> {
        self.cache(1)?;
        while is_blank(self.buf(), self.buffer_pos) {
            self.skip();
            self.cache(1)?;
        }
        let major = self.scan_version_directive_number(start_mark)?;
        if self.b(0) != b'.' {
            return Err(scanner_error_context(
                "while scanning a %YAML directive",
                start_mark,
                "did not find expected digit or '.' character",
                self.mark,
            ));
        }
        self.skip();
        let minor = self.scan_version_directive_number(start_mark)?;
        Ok((major, minor))
    }

    fn scan_version_directive_number(&mut self, start_mark: Mark) -> R<i8> {
        self.cache(1)?;
        let mut value: i8 = 0;
        let mut length = 0;
        while is_digit(self.buf(), self.buffer_pos) {
            length += 1;
            if length > MAX_NUMBER_LENGTH {
                return Err(scanner_error_context(
                    "while scanning a %YAML directive",
                    start_mark,
                    "found extremely long version number",
                    self.mark,
                ));
            }
            value = value.wrapping_mul(10).wrapping_add(as_digit(self.buf(), self.buffer_pos) as i8);
            self.skip();
            self.cache(1)?;
        }
        if length == 0 {
            return Err(scanner_error_context(
                "while scanning a %YAML directive",
                start_mark,
                "did not find expected version number",
                self.mark,
            ));
        }
        Ok(value)
    }

    fn scan_tag_directive_value(&mut self, start_mark: Mark) -> R<(Vec<u8>, Vec<u8>)> {
        self.cache(1)?;
        while is_blank(self.buf(), self.buffer_pos) {
            self.skip();
            self.cache(1)?;
        }
        let handle = self.scan_tag_handle(true, start_mark)?;
        self.cache(1)?;
        if !is_blank(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a %TAG directive",
                start_mark,
                "did not find expected whitespace",
                self.mark,
            ));
        }
        while is_blank(self.buf(), self.buffer_pos) {
            self.skip();
            self.cache(1)?;
        }
        let prefix = self.scan_tag_uri(true, true, &[], start_mark)?;
        self.cache(1)?;
        if !is_blankz(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a %TAG directive",
                start_mark,
                "did not find expected whitespace or line break",
                self.mark,
            ));
        }
        Ok((handle, prefix))
    }

    fn scan_anchor(&mut self, typ: TokenType) -> R<Token> {
        let mut s = Vec::new();
        let start_mark = self.mark;
        self.skip();
        self.cache(1)?;
        while is_anchor_char_with(self.buf(), self.buffer_pos, is_bom(self.buf(), self.buffer_pos)) {
            self.read(&mut s);
            self.cache(1)?;
        }
        let end_mark = self.mark;
        let c = self.b(0);
        if s.is_empty() || !(is_blankz(self.buf(), self.buffer_pos) || matches!(c, b'?' | b':' | b',' | b']' | b'}' | b'%' | b'@' | b'`')) {
            let context = if typ == TokenType::Anchor { "while scanning an anchor" } else { "while scanning an alias" };
            return Err(scanner_error_context(context, start_mark, "did not find expected alphabetic or numeric character", self.mark));
        }
        Ok(Token { typ, start_mark, end_mark, value: s, ..Default::default() })
    }

    fn scan_tag(&mut self) -> R<Token> {
        let start_mark = self.mark;
        let mut handle;
        let suffix;
        self.cache(2)?;
        if self.b(1) == b'<' {
            handle = Vec::new();
            self.skip();
            self.skip();
            suffix = self.scan_tag_uri(false, true, &[], start_mark)?;
            if self.b(0) != b'>' {
                return Err(scanner_error_context("while scanning a tag", start_mark, "did not find the expected '>'", self.mark));
            }
            self.skip();
        } else {
            handle = self.scan_tag_handle(false, start_mark)?;
            if handle[0] == b'!' && handle.len() > 1 && handle[handle.len() - 1] == b'!' {
                suffix = self.scan_tag_uri(false, false, &[], start_mark)?;
            } else {
                let s = self.scan_tag_uri(false, false, &handle, start_mark)?;
                handle = vec![b'!'];
                if s.is_empty() {
                    suffix = handle;
                    handle = Vec::new();
                } else {
                    suffix = s;
                }
            }
        }
        self.cache(1)?;
        if !is_blankz(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a tag",
                start_mark,
                "did not find expected whitespace or line break",
                self.mark,
            ));
        }
        Ok(Token { typ: TokenType::Tag, start_mark, end_mark: self.mark, value: handle, suffix, ..Default::default() })
    }

    fn tag_error(&self, directive: bool, context_mark: Mark, problem: &str) -> LoadError {
        let context = if directive { "while parsing a %TAG directive" } else { "while parsing a tag" };
        scanner_error_context(context, context_mark, problem, self.mark)
    }

    fn scan_tag_handle(&mut self, directive: bool, start_mark: Mark) -> R<Vec<u8>> {
        self.cache(1)?;
        if self.b(0) != b'!' {
            return Err(self.tag_error(directive, start_mark, "did not find expected '!'"));
        }
        let mut s = Vec::new();
        self.read(&mut s);
        self.cache(1)?;
        while is_alpha(self.buf(), self.buffer_pos) {
            self.read(&mut s);
            self.cache(1)?;
        }
        if self.b(0) == b'!' {
            self.read(&mut s);
        } else if directive && s != b"!" {
            return Err(self.tag_error(directive, start_mark, "did not find expected '!'"));
        }
        Ok(s)
    }

    fn scan_tag_uri(&mut self, directive: bool, verbatim: bool, head: &[u8], start_mark: Mark) -> R<Vec<u8>> {
        let mut s = Vec::new();
        let mut has_tag = !head.is_empty();
        if head.len() > 1 {
            s.extend_from_slice(&head[1..]);
        }
        self.cache(1)?;
        while is_tag_uri_char(self.buf(), self.buffer_pos, verbatim) {
            if self.b(0) == b'%' {
                self.scan_uri_escapes(directive, start_mark, &mut s)?;
            } else {
                self.read(&mut s);
            }
            self.cache(1)?;
            has_tag = true;
        }
        if !verbatim {
            let c = self.b(0);
            if !is_blankz(self.buf(), self.buffer_pos) && (0x20..=0x7E).contains(&c) {
                return Err(self.tag_error(directive, start_mark, &format!("found character '{}' that is not allowed in a YAML tag", c as char)));
            }
        }
        if !has_tag {
            return Err(self.tag_error(directive, start_mark, "did not find expected tag URI"));
        }
        Ok(s)
    }

    fn scan_uri_escapes(&mut self, directive: bool, start_mark: Mark, s: &mut Vec<u8>) -> R<()> {
        let mut w = 1024usize;
        while w > 0 {
            self.cache(3)?;
            if !(self.b(0) == b'%' && is_hex(self.buf(), self.buffer_pos + 1) && is_hex(self.buf(), self.buffer_pos + 2)) {
                return Err(self.tag_error(directive, start_mark, "did not find URI escaped octet"));
            }
            let octet = ((as_hex(self.buf(), self.buffer_pos + 1) << 4) + as_hex(self.buf(), self.buffer_pos + 2)) as u8;
            if w == 1024 {
                w = width(octet);
                if w == 0 {
                    return Err(self.tag_error(directive, start_mark, "found an incorrect leading UTF-8 octet"));
                }
            } else if octet & 0xC0 != 0x80 {
                return Err(self.tag_error(directive, start_mark, "found an incorrect trailing UTF-8 octet"));
            }
            s.push(octet);
            self.skip();
            self.skip();
            self.skip();
            w -= 1;
        }
        Ok(())
    }

    fn scan_block_scalar(&mut self, literal: bool) -> R<Token> {
        let start_mark = self.mark;
        self.skip();
        self.cache(1)?;
        let mut chomping = 0i32;
        let mut increment = 0i64;
        if self.b(0) == b'+' || self.b(0) == b'-' {
            chomping = if self.b(0) == b'+' { 1 } else { -1 };
            self.skip();
            self.cache(1)?;
            if is_digit(self.buf(), self.buffer_pos) {
                if self.b(0) == b'0' {
                    return Err(scanner_error_context(
                        "while scanning a block scalar",
                        start_mark,
                        "found an indentation indicator equal to 0",
                        self.mark,
                    ));
                }
                increment = as_digit(self.buf(), self.buffer_pos) as i64;
                self.skip();
            }
        } else if is_digit(self.buf(), self.buffer_pos) {
            if self.b(0) == b'0' {
                return Err(scanner_error_context(
                    "while scanning a block scalar",
                    start_mark,
                    "found an indentation indicator equal to 0",
                    self.mark,
                ));
            }
            increment = as_digit(self.buf(), self.buffer_pos) as i64;
            self.skip();
            self.cache(1)?;
            if self.b(0) == b'+' || self.b(0) == b'-' {
                chomping = if self.b(0) == b'+' { 1 } else { -1 };
                self.skip();
            }
        }
        self.cache(1)?;
        while is_blank(self.buf(), self.buffer_pos) {
            self.skip();
            self.cache(1)?;
        }
        if self.b(0) == b'#' {
            self.scan_line_comment(start_mark)?;
            while !is_breakz(self.buf(), self.buffer_pos) {
                self.skip();
                self.cache(1)?;
            }
        }
        if !is_breakz(self.buf(), self.buffer_pos) {
            return Err(scanner_error_context(
                "while scanning a block scalar",
                start_mark,
                "did not find expected comment or line break",
                self.mark,
            ));
        }
        if is_line_break(self.buf(), self.buffer_pos) {
            self.cache(2)?;
            self.skip_line();
        }
        let mut end_mark = self.mark;
        let mut indent: i64 = 0;
        if increment > 0 {
            indent = if self.indent >= 0 { self.indent + increment } else { increment + 1 };
        }
        let mut s = Vec::new();
        let mut leading_break: Vec<u8> = Vec::new();
        let mut trailing_breaks: Vec<u8> = Vec::new();
        self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start_mark, &mut end_mark)?;
        self.cache(1)?;
        let mut leading_blank = false;
        while self.mark.column == indent && !is_z(self.buf(), self.buffer_pos) {
            let trailing_blank = is_blank(self.buf(), self.buffer_pos);
            if !literal && !leading_blank && !trailing_blank && !leading_break.is_empty() && leading_break[0] == b'\n' {
                if trailing_breaks.is_empty() {
                    s.push(b' ');
                }
            } else {
                s.extend_from_slice(&leading_break);
            }
            leading_break.clear();
            s.extend_from_slice(&trailing_breaks);
            trailing_breaks.clear();
            leading_blank = is_blank(self.buf(), self.buffer_pos);
            while !is_breakz(self.buf(), self.buffer_pos) {
                self.read(&mut s);
                self.cache(1)?;
            }
            self.cache(2)?;
            self.read_line(&mut leading_break);
            self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start_mark, &mut end_mark)?;
        }
        if chomping != -1 {
            s.extend_from_slice(&leading_break);
        }
        if chomping == 1 {
            s.extend_from_slice(&trailing_breaks);
        }
        let style = if literal { LITERAL_SCALAR_STYLE } else { FOLDED_SCALAR_STYLE };
        Ok(Token { typ: TokenType::Scalar, start_mark, end_mark, value: s, style, ..Default::default() })
    }

    fn scan_block_scalar_breaks(&mut self, indent: &mut i64, breaks: &mut Vec<u8>, start_mark: Mark, end_mark: &mut Mark) -> R<()> {
        *end_mark = self.mark;
        let mut max_indent = 0i64;
        loop {
            self.cache(1)?;
            while (*indent == 0 || self.mark.column < *indent) && is_space(self.buf(), self.buffer_pos) {
                self.skip();
                self.cache(1)?;
            }
            if self.mark.column > max_indent {
                max_indent = self.mark.column;
            }
            if (*indent == 0 || self.mark.column < *indent) && is_tab(self.buf(), self.buffer_pos) {
                return Err(scanner_error_context(
                    "while scanning a block scalar",
                    start_mark,
                    "found a tab character where an indentation space is expected",
                    self.mark,
                ));
            }
            if !is_line_break(self.buf(), self.buffer_pos) {
                break;
            }
            self.cache(2)?;
            self.read_line(breaks);
            *end_mark = self.mark;
        }
        if *indent == 0 {
            *indent = max_indent;
            if *indent < self.indent + 1 {
                *indent = self.indent + 1;
            }
            if *indent < 1 {
                *indent = 1;
            }
        }
        Ok(())
    }

    fn scan_flow_scalar(&mut self, single: bool) -> R<Token> {
        let start_mark = self.mark;
        self.skip();
        let mut s = Vec::new();
        let mut leading_break = Vec::new();
        let mut trailing_breaks = Vec::new();
        let mut whitespaces = Vec::new();
        loop {
            self.cache(4)?;
            let (c0, c1, c2) = (self.b(0), self.b(1), self.b(2));
            if self.mark.column == 1
                && ((c0 == b'-' && c1 == b'-' && c2 == b'-') || (c0 == b'.' && c1 == b'.' && c2 == b'.'))
                && is_blankz(self.buf(), self.buffer_pos + 3)
            {
                return Err(scanner_error_context(
                    "while scanning a quoted scalar",
                    start_mark,
                    "found unexpected document indicator",
                    self.mark,
                ));
            }
            if is_z(self.buf(), self.buffer_pos) {
                return Err(scanner_error_context("while scanning a quoted scalar", start_mark, "found unexpected end of stream", self.mark));
            }
            let mut leading_blanks = false;
            while !is_blankz(self.buf(), self.buffer_pos) {
                let c = self.b(0);
                if single && c == b'\'' && self.b(1) == b'\'' {
                    s.push(b'\'');
                    self.skip();
                    self.skip();
                } else if single && c == b'\'' {
                    break;
                } else if !single && c == b'"' {
                    break;
                } else if !single && c == b'\\' && is_line_break(self.buf(), self.buffer_pos + 1) {
                    self.cache(3)?;
                    self.skip();
                    self.skip_line();
                    leading_blanks = true;
                    break;
                } else if !single && c == b'\\' {
                    let mut code_length = 0usize;
                    match self.b(1) {
                        b'0' => s.push(0),
                        b'a' => s.push(0x07),
                        b'b' => s.push(0x08),
                        b't' | b'\t' => s.push(0x09),
                        b'n' => s.push(0x0A),
                        b'v' => s.push(0x0B),
                        b'f' => s.push(0x0C),
                        b'r' => s.push(0x0D),
                        b'e' => s.push(0x1B),
                        b' ' => s.push(0x20),
                        b'"' => s.push(b'"'),
                        b'\'' => s.push(b'\''),
                        b'\\' => s.push(b'\\'),
                        b'N' => s.extend_from_slice(&[0xC2, 0x85]),
                        b'_' => s.extend_from_slice(&[0xC2, 0xA0]),
                        b'L' => s.extend_from_slice(&[0xE2, 0x80, 0xA8]),
                        b'P' => s.extend_from_slice(&[0xE2, 0x80, 0xA9]),
                        b'x' => code_length = 2,
                        b'u' => code_length = 4,
                        b'U' => code_length = 8,
                        _ => {
                            return Err(scanner_error_context(
                                "while scanning a quoted scalar",
                                start_mark,
                                "found unknown escape character",
                                self.mark,
                            ))
                        }
                    }
                    self.skip();
                    self.skip();
                    if code_length > 0 {
                        let mut value: u32 = 0;
                        self.cache(code_length as i64)?;
                        for k in 0..code_length {
                            if !is_hex(self.buf(), self.buffer_pos + k) {
                                return Err(scanner_error_context(
                                    "while scanning a quoted scalar",
                                    start_mark,
                                    "did not find expected hexadecimal number",
                                    self.mark,
                                ));
                            }
                            value = (value << 4) + as_hex(self.buf(), self.buffer_pos + k);
                        }
                        if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
                            return Err(scanner_error_context(
                                "while scanning a quoted scalar",
                                start_mark,
                                "found invalid Unicode character escape code",
                                self.mark,
                            ));
                        }
                        push_utf8(&mut s, value);
                        for _ in 0..code_length {
                            self.skip();
                        }
                    }
                } else {
                    self.read(&mut s);
                }
                self.cache(2)?;
            }
            self.cache(1)?;
            if single {
                if self.b(0) == b'\'' {
                    break;
                }
            } else if self.b(0) == b'"' {
                break;
            }
            while is_blank(self.buf(), self.buffer_pos) || is_line_break(self.buf(), self.buffer_pos) {
                if is_blank(self.buf(), self.buffer_pos) {
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.cache(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.cache(1)?;
            }
            if leading_blanks {
                if !leading_break.is_empty() && leading_break[0] == b'\n' {
                    if trailing_breaks.is_empty() {
                        s.push(b' ');
                    } else {
                        s.extend_from_slice(&trailing_breaks);
                    }
                } else {
                    s.extend_from_slice(&leading_break);
                    s.extend_from_slice(&trailing_breaks);
                }
                trailing_breaks.clear();
                leading_break.clear();
            } else {
                s.extend_from_slice(&whitespaces);
                whitespaces.clear();
            }
        }
        self.skip();
        let style = if single { SINGLE_QUOTED_SCALAR_STYLE } else { DOUBLE_QUOTED_SCALAR_STYLE };
        Ok(Token { typ: TokenType::Scalar, start_mark, end_mark: self.mark, value: s, style, ..Default::default() })
    }

    fn scan_plain_scalar(&mut self) -> R<Token> {
        let mut s = Vec::new();
        let mut leading_break: Vec<u8> = Vec::new();
        let mut trailing_breaks: Vec<u8> = Vec::new();
        let mut whitespaces: Vec<u8> = Vec::new();
        let mut leading_blanks = false;
        let indent = self.indent + 1;
        let start_mark = self.mark;
        let mut end_mark = self.mark;
        loop {
            self.cache(4)?;
            let (c0, c1, c2) = (self.b(0), self.b(1), self.b(2));
            if self.mark.column == 1
                && ((c0 == b'-' && c1 == b'-' && c2 == b'-') || (c0 == b'.' && c1 == b'.' && c2 == b'.'))
                && is_blankz(self.buf(), self.buffer_pos + 3)
            {
                break;
            }
            if c0 == b'#' {
                break;
            }
            while !is_blankz(self.buf(), self.buffer_pos) {
                if (self.b(0) == b':' && is_blankz(self.buf(), self.buffer_pos + 1))
                    || (self.flow_level > 0 && is_end_of_scalar_in_flow(self.buf(), self.buffer_pos))
                {
                    break;
                }
                if leading_blanks || !whitespaces.is_empty() {
                    if leading_blanks {
                        if !leading_break.is_empty() && leading_break[0] == b'\n' {
                            if trailing_breaks.is_empty() {
                                s.push(b' ');
                            } else {
                                s.extend_from_slice(&trailing_breaks);
                            }
                        } else {
                            s.extend_from_slice(&leading_break);
                            s.extend_from_slice(&trailing_breaks);
                        }
                        trailing_breaks.clear();
                        leading_break.clear();
                        leading_blanks = false;
                    } else {
                        s.extend_from_slice(&whitespaces);
                        whitespaces.clear();
                    }
                }
                self.read(&mut s);
                end_mark = self.mark;
                self.cache(2)?;
            }
            if !(is_blank(self.buf(), self.buffer_pos) || is_line_break(self.buf(), self.buffer_pos)) {
                break;
            }
            self.cache(1)?;
            while is_blank(self.buf(), self.buffer_pos) || is_line_break(self.buf(), self.buffer_pos) {
                if is_blank(self.buf(), self.buffer_pos) {
                    if leading_blanks && self.mark.column < indent && is_tab(self.buf(), self.buffer_pos) {
                        return Err(scanner_error_context(
                            "while scanning a plain scalar",
                            start_mark,
                            "found a tab character that violates indentation",
                            self.mark,
                        ));
                    }
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.cache(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.cache(1)?;
            }
            if self.flow_level == 0 && self.mark.column < indent {
                break;
            }
        }
        if leading_blanks {
            self.simple_key_allowed = true;
        }
        Ok(Token { typ: TokenType::Scalar, start_mark, end_mark, value: s, style: PLAIN_SCALAR_STYLE, ..Default::default() })
    }
}

fn is_end_of_scalar_in_flow(b: &[u8], i: usize) -> bool {
    match at(b, i) {
        b',' | b'[' | b']' | b'{' | b'}' => true,
        b'?' => is_blankz(b, i + 1),
        b':' => matches!(at(b, i + 1), b' ' | b',' | b']' | b'}'),
        _ => false,
    }
}

pub(crate) fn push_utf8(s: &mut Vec<u8>, value: u32) {
    if value <= 0x7F {
        s.push(value as u8);
    } else if value <= 0x7FF {
        s.push((0xC0 + (value >> 6)) as u8);
        s.push((0x80 + (value & 0x3F)) as u8);
    } else if value <= 0xFFFF {
        s.push((0xE0 + (value >> 12)) as u8);
        s.push((0x80 + ((value >> 6) & 0x3F)) as u8);
        s.push((0x80 + (value & 0x3F)) as u8);
    } else {
        s.push((0xF0 + (value >> 18)) as u8);
        s.push((0x80 + ((value >> 12) & 0x3F)) as u8);
        s.push((0x80 + ((value >> 6) & 0x3F)) as u8);
        s.push((0x80 + (value & 0x3F)) as u8);
    }
}
