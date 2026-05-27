//! Hand-rolled streaming XML tokenizer + prettifier.
//!
//! Produces the same `ParseOutput` shape as the JSON parser so the
//! renderer doesn't have to care which format it's drawing. The path
//! formatter emits XPath; sub-tree extraction returns the whole element
//! `<tag>...</tag>` for elements and the attribute value for attributes.
//!
//! Lenient: malformed tags are skipped rather than aborting, so the
//! viewer can show a partly-broken document up to the point it goes
//! wrong.

use super::{
    NameInterner, Offset, PROGRESS_GRANULARITY, ParseError, ParseErrorKind, ParseOutput,
    PathEntry, PathIndex, PathSegment, ProgressSink, ROOT_PARENT, StyleKind, StyleSpan,
};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

// --- parser ---------------------------------------------------------

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line_starts: Vec<Offset>,
    paths: Vec<PathEntry>,
    styles: Vec<StyleSpan>,
    names: NameInterner,
    /// Stack of open elements: (entry_idx, child_count_per_name).
    /// child_count_per_name lets us assign sibling_index at open time.
    stack: Vec<(u32, HashMap<u32, u32>)>,
    progress: Option<&'a ProgressSink>,
    next_progress_at: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8], progress: Option<&'a ProgressSink>) -> Self {
        let next_progress_at = if progress.is_some() {
            PROGRESS_GRANULARITY
        } else {
            usize::MAX
        };
        Self {
            input,
            pos: 0,
            line_starts: vec![0],
            paths: Vec::new(),
            styles: Vec::new(),
            names: NameInterner::default(),
            stack: Vec::new(),
            progress,
            next_progress_at,
        }
    }

    #[inline]
    fn peek_at(&self, off: usize) -> Option<u8> {
        self.input.get(self.pos + off).copied()
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.peek_at(0)
    }

    #[inline]
    fn advance(&mut self) -> Option<u8> {
        let b = self.peek()?;
        if b == b'\n' {
            self.line_starts.push((self.pos + 1) as u32);
        }
        self.pos += 1;
        if self.pos >= self.next_progress_at {
            self.flush_progress();
        }
        Some(b)
    }

    #[cold]
    #[inline(never)]
    fn flush_progress(&mut self) {
        if let Some(p) = self.progress {
            p.bytes_done.store(self.pos as u64, Ordering::Relaxed);
        }
        self.next_progress_at = self.pos + PROGRESS_GRANULARITY;
    }

    fn err(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            offset: self.pos as u32,
            kind,
        }
    }

    fn parse_document(&mut self) -> Result<(), ParseError> {
        // Root spans the whole input so clicks on whitespace or text
        // outside any element resolve to "/".
        let root_idx = self.paths.len() as u32;
        self.paths.push(PathEntry {
            start: 0,
            end: 0,
            parent: ROOT_PARENT,
            segment: PathSegment::Root,
        });
        self.stack.push((root_idx, HashMap::new()));

        while self.pos < self.input.len() {
            if self.peek() == Some(b'<') {
                self.parse_markup()?;
            } else {
                // Text content between tags — consumed but produces no entry.
                while let Some(b) = self.peek() {
                    if b == b'<' {
                        break;
                    }
                    self.advance();
                }
            }
        }

        self.stack.pop();
        self.paths[root_idx as usize].end = self.input.len() as u32;
        self.fixup_unique_siblings();
        Ok(())
    }

    fn parse_markup(&mut self) -> Result<(), ParseError> {
        let mark_start = self.pos as u32;
        // Lookahead at the byte after `<`.
        match self.peek_at(1) {
            Some(b'?') => self.parse_pi(mark_start),
            Some(b'!') => {
                if self.starts_with_at(self.pos + 2, b"--") {
                    self.parse_comment(mark_start)
                } else if self.starts_with_at(self.pos + 2, b"[CDATA[") {
                    self.parse_cdata(mark_start)
                } else {
                    // <!DOCTYPE … > or any other `<!…>` declaration.
                    self.parse_doctype(mark_start)
                }
            }
            Some(b'/') => self.parse_close_tag(mark_start),
            Some(b) if is_name_start(b) => self.parse_open_or_self_close(mark_start),
            _ => {
                // Stray `<` — consume one byte and move on lenient.
                self.advance();
                Ok(())
            }
        }
    }

    fn starts_with_at(&self, pos: usize, needle: &[u8]) -> bool {
        self.input.get(pos..pos + needle.len()) == Some(needle)
    }

    fn parse_pi(&mut self, mark_start: u32) -> Result<(), ParseError> {
        // Skip `<?` then read until `?>`.
        self.advance();
        self.advance();
        while self.pos < self.input.len() {
            if self.peek() == Some(b'?') && self.peek_at(1) == Some(b'>') {
                self.advance();
                self.advance();
                let end = self.pos as u32;
                self.styles.push(StyleSpan {
                    start: mark_start,
                    end,
                    kind: StyleKind::Pi,
                });
                return Ok(());
            }
            self.advance();
        }
        // EOF mid-PI — style what we have.
        self.styles.push(StyleSpan {
            start: mark_start,
            end: self.pos as u32,
            kind: StyleKind::Pi,
        });
        Ok(())
    }

    fn parse_comment(&mut self, mark_start: u32) -> Result<(), ParseError> {
        // Skip `<!--` then read until `-->`.
        for _ in 0..4 {
            self.advance();
        }
        while self.pos < self.input.len() {
            if self.peek() == Some(b'-')
                && self.peek_at(1) == Some(b'-')
                && self.peek_at(2) == Some(b'>')
            {
                for _ in 0..3 {
                    self.advance();
                }
                self.styles.push(StyleSpan {
                    start: mark_start,
                    end: self.pos as u32,
                    kind: StyleKind::Comment,
                });
                return Ok(());
            }
            self.advance();
        }
        self.styles.push(StyleSpan {
            start: mark_start,
            end: self.pos as u32,
            kind: StyleKind::Comment,
        });
        Ok(())
    }

    fn parse_cdata(&mut self, mark_start: u32) -> Result<(), ParseError> {
        // Skip `<![CDATA[` (9 chars) then read until `]]>`.
        for _ in 0..9 {
            self.advance();
        }
        while self.pos < self.input.len() {
            if self.peek() == Some(b']')
                && self.peek_at(1) == Some(b']')
                && self.peek_at(2) == Some(b'>')
            {
                for _ in 0..3 {
                    self.advance();
                }
                self.styles.push(StyleSpan {
                    start: mark_start,
                    end: self.pos as u32,
                    kind: StyleKind::CData,
                });
                return Ok(());
            }
            self.advance();
        }
        self.styles.push(StyleSpan {
            start: mark_start,
            end: self.pos as u32,
            kind: StyleKind::CData,
        });
        Ok(())
    }

    fn parse_doctype(&mut self, mark_start: u32) -> Result<(), ParseError> {
        // `<!…>` — just walk to next `>` outside of quotes/brackets.
        self.advance(); // `<`
        self.advance(); // `!`
        let mut depth = 0i32;
        while self.pos < self.input.len() {
            let b = self.peek().unwrap();
            if b == b'"' || b == b'\'' {
                self.skip_quoted(b);
                continue;
            }
            if b == b'[' {
                depth += 1;
            } else if b == b']' {
                depth -= 1;
            } else if b == b'>' && depth <= 0 {
                self.advance();
                self.styles.push(StyleSpan {
                    start: mark_start,
                    end: self.pos as u32,
                    kind: StyleKind::Pi,
                });
                return Ok(());
            }
            self.advance();
        }
        self.styles.push(StyleSpan {
            start: mark_start,
            end: self.pos as u32,
            kind: StyleKind::Pi,
        });
        Ok(())
    }

    fn skip_quoted(&mut self, quote: u8) {
        self.advance(); // opening quote
        while self.pos < self.input.len() {
            if self.peek() == Some(quote) {
                self.advance();
                return;
            }
            self.advance();
        }
    }

    fn parse_open_or_self_close(&mut self, mark_start: u32) -> Result<(), ParseError> {
        // Consume `<`.
        self.advance();
        let name_start = self.pos as u32;
        self.consume_name();
        let name_end = self.pos as u32;
        if name_end == name_start {
            return Err(self.err(ParseErrorKind::UnexpectedByte(self.peek().unwrap_or(0))));
        }
        let name_bytes = &self.input[name_start as usize..name_end as usize];
        let name_id = self.names.intern(name_bytes);
        self.styles.push(StyleSpan {
            start: name_start,
            end: name_end,
            kind: StyleKind::Tag,
        });

        // Push the element entry; sibling_index will be set after we
        // know its parent (which is `stack.last()`).
        let parent_entry = self.stack.last().expect("root frame is always present").0;
        let parent_counts = &mut self.stack.last_mut().unwrap().1;
        let count = parent_counts.entry(name_id).or_insert(0);
        *count += 1;
        let sibling_index = *count; // 1-based; may be reset to 0 by fixup if unique

        let elem_idx = self.paths.len() as u32;
        self.paths.push(PathEntry {
            start: mark_start,
            end: 0,
            parent: parent_entry,
            segment: PathSegment::Element {
                name: name_id,
                sibling_index,
            },
        });

        // Push onto open-element stack so attribute and child entries
        // get the right parent.
        self.stack.push((elem_idx, HashMap::new()));

        // Parse attributes / self-close / open-end.
        loop {
            self.skip_xml_ws();
            match self.peek() {
                Some(b'>') => {
                    self.advance();
                    return Ok(());
                }
                Some(b'/') => {
                    if self.peek_at(1) == Some(b'>') {
                        self.advance();
                        self.advance();
                        // Self-closing: close this element immediately.
                        let close_pos = self.pos as u32;
                        self.paths[elem_idx as usize].end = close_pos;
                        self.stack.pop();
                        return Ok(());
                    } else {
                        // Stray `/` — skip and continue.
                        self.advance();
                    }
                }
                Some(b) if is_name_start(b) => {
                    self.parse_attribute(elem_idx)?;
                }
                None => return Err(self.err(ParseErrorKind::UnexpectedEof)),
                Some(_) => {
                    // Skip unknown byte rather than aborting.
                    self.advance();
                }
            }
        }
    }

    fn parse_attribute(&mut self, parent_entry: u32) -> Result<(), ParseError> {
        let attr_start = self.pos as u32;
        let name_start = self.pos as u32;
        self.consume_name();
        let name_end = self.pos as u32;
        if name_end == name_start {
            self.advance();
            return Ok(());
        }
        let name_bytes = &self.input[name_start as usize..name_end as usize];
        let name_id = self.names.intern(name_bytes);
        self.styles.push(StyleSpan {
            start: name_start,
            end: name_end,
            kind: StyleKind::AttrName,
        });

        self.skip_xml_ws();
        if self.peek() != Some(b'=') {
            // Boolean-style attribute (HTML-ish). Record an entry that
            // spans just the name; no value style.
            let entry_idx = self.paths.len() as u32;
            self.paths.push(PathEntry {
                start: attr_start,
                end: name_end,
                parent: parent_entry,
                segment: PathSegment::Attribute(name_id),
            });
            let _ = entry_idx;
            return Ok(());
        }
        self.advance(); // `=`
        self.skip_xml_ws();
        let q = self.peek().unwrap_or(0);
        let value_end = if q == b'"' || q == b'\'' {
            let val_start = self.pos as u32;
            self.advance();
            while self.pos < self.input.len() && self.peek() != Some(q) {
                self.advance();
            }
            if self.peek() == Some(q) {
                self.advance();
            }
            let val_end = self.pos as u32;
            self.styles.push(StyleSpan {
                start: val_start,
                end: val_end,
                kind: StyleKind::AttrValue,
            });
            val_end
        } else {
            // Unquoted value — read until whitespace or `>`.
            let val_start = self.pos as u32;
            while let Some(b) = self.peek() {
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/') {
                    break;
                }
                self.advance();
            }
            let val_end = self.pos as u32;
            self.styles.push(StyleSpan {
                start: val_start,
                end: val_end,
                kind: StyleKind::AttrValue,
            });
            val_end
        };

        self.paths.push(PathEntry {
            start: attr_start,
            end: value_end,
            parent: parent_entry,
            segment: PathSegment::Attribute(name_id),
        });
        Ok(())
    }

    fn parse_close_tag(&mut self, _mark_start: u32) -> Result<(), ParseError> {
        // Skip `</`.
        self.advance();
        self.advance();
        let name_start = self.pos as u32;
        self.consume_name();
        let name_end = self.pos as u32;
        let name_bytes = &self.input[name_start as usize..name_end as usize];
        let name_id = self.names.intern(name_bytes);
        self.styles.push(StyleSpan {
            start: name_start,
            end: name_end,
            kind: StyleKind::Tag,
        });
        // Skip whitespace and `>`.
        while let Some(b) = self.peek() {
            if b == b'>' {
                self.advance();
                break;
            }
            self.advance();
        }

        // Pop matching open element. Lenient: if names mismatch, walk up
        // the stack looking for a match (HTML-ish recovery).
        let close_pos = self.pos as u32;
        let mut found = None;
        for (depth, (entry, _)) in self.stack.iter().enumerate().rev() {
            if *entry == 0 {
                // Root frame; never matches a close tag.
                break;
            }
            if let PathSegment::Element { name, .. } = self.paths[*entry as usize].segment {
                if name == name_id {
                    found = Some(depth);
                    break;
                }
            }
        }
        if let Some(depth) = found {
            // Pop everything down to and including `depth`.
            while self.stack.len() > depth {
                let (entry, _) = self.stack.pop().unwrap();
                self.paths[entry as usize].end = close_pos;
            }
        }
        Ok(())
    }

    fn consume_name(&mut self) {
        while let Some(b) = self.peek() {
            if is_name_char(b) {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn skip_xml_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.advance();
            } else {
                break;
            }
        }
    }

    /// Walk all Element entries; if `(parent, name)` group has only one
    /// member, reset its `sibling_index` to 0 (sentinel for "omit `[N]`").
    fn fixup_unique_siblings(&mut self) {
        // (parent_entry_id, name_id) → count
        let mut counts: HashMap<(u32, u32), u32> = HashMap::new();
        for e in &self.paths {
            if let PathSegment::Element { name, .. } = e.segment {
                *counts.entry((e.parent, name)).or_insert(0) += 1;
            }
        }
        for e in &mut self.paths {
            if let PathSegment::Element {
                name,
                sibling_index,
            } = e.segment
            {
                let total = counts.get(&(e.parent, name)).copied().unwrap_or(1);
                if total == 1 {
                    e.segment = PathSegment::Element {
                        name,
                        sibling_index: 0,
                    };
                } else {
                    // Keep 1-based sibling_index.
                    let _ = sibling_index;
                }
            }
        }
    }

    fn finish(self, error: Option<ParseError>) -> ParseOutput {
        ParseOutput {
            line_starts: self.line_starts,
            paths: PathIndex {
                entries: self.paths,
            },
            styles: self.styles,
            names: self.names,
            error,
            bytes: self.input.len(),
        }
    }
}

#[inline]
fn is_name_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b':') || b >= 0x80
}

#[inline]
fn is_name_char(b: u8) -> bool {
    is_name_start(b) || matches!(b, b'0'..=b'9' | b'-' | b'.')
}

pub fn parse(input: &[u8], progress: Option<&ProgressSink>) -> ParseOutput {
    let mut p = Parser::new(input, progress);
    let err = p.parse_document().err();
    if let Some(sink) = progress {
        sink.bytes_done
            .store(input.len() as u64, Ordering::Relaxed);
    }
    p.finish(err)
}

// --- path expression -----------------------------------------------

pub fn path_expression(segments: &[PathSegment], names: &NameInterner) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for seg in segments {
        match seg {
            PathSegment::Root => {}
            PathSegment::Element {
                name,
                sibling_index,
            } => {
                out.push('/');
                let bytes = names.get(*name);
                out.push_str(std::str::from_utf8(bytes).unwrap_or("\u{FFFD}"));
                if *sibling_index > 0 {
                    use std::fmt::Write;
                    let _ = write!(out, "[{}]", sibling_index);
                }
            }
            PathSegment::Attribute(id) => {
                out.push_str("/@");
                let bytes = names.get(*id);
                out.push_str(std::str::from_utf8(bytes).unwrap_or("\u{FFFD}"));
            }
            // Non-XML segments shouldn't appear in an XML doc — be safe.
            PathSegment::Key(_)
            | PathSegment::Index(_)
            | PathSegment::Heading { .. } => {}
        }
    }
    out
}

// --- value extraction ----------------------------------------------

pub fn value_bytes_for_entry<'a>(bytes: &'a [u8], entry: &PathEntry) -> &'a [u8] {
    let start = (entry.start as usize).min(bytes.len());
    let end = (entry.end as usize).min(bytes.len());
    if end <= start {
        return &[];
    }
    let slice = &bytes[start..end];
    match entry.segment {
        PathSegment::Root => slice.trim_ascii(),
        PathSegment::Element { .. } => slice,
        PathSegment::Attribute(_) => extract_attr_value(slice),
        PathSegment::Key(_) | PathSegment::Index(_) | PathSegment::Heading { .. } => slice,
    }
}

/// `name="value"` → `value`. `name='v'` → `v`. `name=v` → `v`. `name` → `name` (boolean attr).
fn extract_attr_value(slice: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < slice.len() && slice[i] != b'=' {
        i += 1;
    }
    if i >= slice.len() {
        return slice;
    }
    i += 1;
    while i < slice.len() && matches!(slice[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    if i >= slice.len() {
        return &[];
    }
    let q = slice[i];
    if q == b'"' || q == b'\'' {
        let start = i + 1;
        let mut j = start;
        while j < slice.len() && slice[j] != q {
            j += 1;
        }
        &slice[start..j]
    } else {
        &slice[i..]
    }
}

// --- prettifier ----------------------------------------------------

/// Two-pass pretty-printer:
/// 1. Tokenize, classify each open element as "block" (no direct
///    non-whitespace text children) or "mixed" (has such text).
/// 2. Emit: block elements get each child on its own line at the right
///    depth; mixed elements emit their contents verbatim so we don't
///    add or remove whitespace inside text.
pub fn prettify(input: &[u8]) -> Vec<u8> {
    let tokens = tokenize_pretty(input);
    let block_flags = classify_blocks(&tokens, input);
    emit_pretty(&tokens, &block_flags, input)
}

#[derive(Debug, Clone, Copy)]
enum Tok {
    Open(u32, u32),
    Close(u32, u32),
    SelfClose(u32, u32),
    Text(u32, u32),
    Comment(u32, u32),
    CData(u32, u32),
    Pi(u32, u32),
    Doctype(u32, u32),
}

fn tokenize_pretty(input: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let n = input.len();
    while i < n {
        if input[i] == b'<' {
            let start = i;
            if input.get(i + 1) == Some(&b'?') {
                let end = find_until(input, i + 2, b"?>");
                out.push(Tok::Pi(start as u32, end as u32));
                i = end;
            } else if input.get(i + 1) == Some(&b'!') {
                if input.get(i + 2..i + 4) == Some(b"--") {
                    let end = find_until(input, i + 4, b"-->");
                    out.push(Tok::Comment(start as u32, end as u32));
                    i = end;
                } else if input.get(i + 2..i + 9) == Some(b"[CDATA[") {
                    let end = find_until(input, i + 9, b"]]>");
                    out.push(Tok::CData(start as u32, end as u32));
                    i = end;
                } else {
                    let end = find_doctype_end(input, i + 2);
                    out.push(Tok::Doctype(start as u32, end as u32));
                    i = end;
                }
            } else if input.get(i + 1) == Some(&b'/') {
                let end = find_until(input, i + 2, b">");
                out.push(Tok::Close(start as u32, end as u32));
                i = end;
            } else if input.get(i + 1).map_or(false, |b| is_name_start(*b)) {
                let (end, self_close) = find_open_end(input, i + 1);
                if self_close {
                    out.push(Tok::SelfClose(start as u32, end as u32));
                } else {
                    out.push(Tok::Open(start as u32, end as u32));
                }
                i = end;
            } else {
                // Stray `<` — emit as text and move on.
                out.push(Tok::Text(start as u32, (start + 1) as u32));
                i += 1;
            }
        } else {
            let start = i;
            while i < n && input[i] != b'<' {
                i += 1;
            }
            out.push(Tok::Text(start as u32, i as u32));
        }
    }
    out
}

fn find_until(input: &[u8], from: usize, needle: &[u8]) -> usize {
    let n = input.len();
    let mut i = from;
    while i + needle.len() <= n {
        if &input[i..i + needle.len()] == needle {
            return i + needle.len();
        }
        i += 1;
    }
    n
}

fn find_doctype_end(input: &[u8], from: usize) -> usize {
    // Walk to next `>` outside quotes / bracketed internal subsets.
    let mut i = from;
    let n = input.len();
    let mut bracket_depth = 0i32;
    while i < n {
        let b = input[i];
        if b == b'"' || b == b'\'' {
            i += 1;
            while i < n && input[i] != b {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if b == b'[' {
            bracket_depth += 1;
        } else if b == b']' {
            bracket_depth -= 1;
        } else if b == b'>' && bracket_depth <= 0 {
            return i + 1;
        }
        i += 1;
    }
    n
}

/// Returns (end_offset, is_self_closing).
fn find_open_end(input: &[u8], from: usize) -> (usize, bool) {
    let mut i = from;
    let n = input.len();
    while i < n {
        let b = input[i];
        if b == b'"' || b == b'\'' {
            i += 1;
            while i < n && input[i] != b {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if b == b'>' {
            let self_close = i > 0 && input[i - 1] == b'/';
            return (i + 1, self_close);
        }
        i += 1;
    }
    (n, false)
}

fn classify_blocks(tokens: &[Tok], input: &[u8]) -> Vec<bool> {
    // For each Open token (in document order), is its content block?
    // Block = no direct child Text token with non-whitespace content.
    let mut flags = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for t in tokens {
        match t {
            Tok::Open(_, _) => {
                flags.push(true);
                stack.push(flags.len() - 1);
            }
            Tok::Close(_, _) => {
                stack.pop();
            }
            Tok::Text(a, b) => {
                let slice = &input[*a as usize..*b as usize];
                let has_non_ws = slice
                    .iter()
                    .any(|&c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'));
                if has_non_ws {
                    if let Some(&top) = stack.last() {
                        flags[top] = false;
                    }
                }
            }
            _ => {}
        }
    }
    flags
}

fn emit_pretty(tokens: &[Tok], block_flags: &[bool], input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 8);
    let mut depth: usize = 0;
    // Stack of block-flags for each open element.
    let mut stack_modes: Vec<bool> = Vec::new();
    let mut open_idx: usize = 0;

    let parent_is_block =
        |stack_modes: &[bool]| stack_modes.last().copied().unwrap_or(true);

    let indent = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        for _ in 0..depth {
            out.extend_from_slice(b"  ");
        }
    };

    for t in tokens {
        match *t {
            Tok::Open(a, b) => {
                let is_block = block_flags[open_idx];
                open_idx += 1;
                if parent_is_block(&stack_modes) && !out.is_empty() {
                    indent(&mut out, depth);
                }
                out.extend_from_slice(&input[a as usize..b as usize]);
                stack_modes.push(is_block);
                depth += 1;
            }
            Tok::Close(a, b) => {
                let was_block = stack_modes.pop().unwrap_or(true);
                depth = depth.saturating_sub(1);
                if was_block {
                    indent(&mut out, depth);
                }
                out.extend_from_slice(&input[a as usize..b as usize]);
            }
            Tok::SelfClose(a, b)
            | Tok::Comment(a, b)
            | Tok::CData(a, b)
            | Tok::Pi(a, b)
            | Tok::Doctype(a, b) => {
                if parent_is_block(&stack_modes) && !out.is_empty() {
                    indent(&mut out, depth);
                }
                out.extend_from_slice(&input[a as usize..b as usize]);
            }
            Tok::Text(a, b) => {
                let slice = &input[a as usize..b as usize];
                let has_non_ws = slice
                    .iter()
                    .any(|&c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'));
                if parent_is_block(&stack_modes) {
                    // Block context: pure-whitespace text is replaced by
                    // formatter newlines; any stray non-ws text gets put
                    // on its own line trimmed.
                    if has_non_ws {
                        if !out.is_empty() {
                            indent(&mut out, depth);
                        }
                        let trimmed = trim_ascii(slice);
                        out.extend_from_slice(trimmed);
                    }
                } else {
                    out.extend_from_slice(slice);
                }
            }
        }
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

fn trim_ascii(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && matches!(s[start], b' ' | b'\t' | b'\n' | b'\r') {
        start += 1;
    }
    while end > start && matches!(s[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    &s[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset_of(src: &[u8], byte: u8) -> Offset {
        src.iter().position(|&b| b == byte).unwrap() as Offset
    }

    #[test]
    fn empty_doc_is_root_only() {
        let out = parse(b"", None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
        assert!(matches!(out.paths.entries[0].segment, PathSegment::Root));
    }

    #[test]
    fn single_self_closing() {
        let src = b"<root/>";
        let out = parse(src, None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 2);
        let path = out.paths.path_of(1);
        assert_eq!(path_expression(&path, &out.names), "/root");
    }

    #[test]
    fn nested_path_lookup() {
        let src = b"<root><a><b>x</b></a></root>";
        let out = parse(src, None);
        assert!(out.error.is_none(), "error: {:?}", out.error);
        // Click on `x`.
        let pos = offset_of(src, b'x');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), "/root/a/b");
    }

    #[test]
    fn sibling_predicate_only_when_ambiguous() {
        let src = b"<root><a/><a/><b/></root>";
        let out = parse(src, None);
        // a is repeated → expect [1], [2]. b is unique → no predicate.
        let mut got = Vec::new();
        for (i, e) in out.paths.entries.iter().enumerate() {
            if matches!(e.segment, PathSegment::Element { .. }) {
                let p = out.paths.path_of(i as u32);
                got.push(path_expression(&p, &out.names));
            }
        }
        assert_eq!(got, vec!["/root", "/root/a[1]", "/root/a[2]", "/root/b"]);
    }

    #[test]
    fn attribute_path() {
        let src = br#"<root><a name="foo"/></root>"#;
        let out = parse(src, None);
        assert!(out.error.is_none());
        // Click inside `name`.
        let pos = src.iter().position(|&b| b == b'n').unwrap() as u32;
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), "/root/a/@name");
    }

    #[test]
    fn root_click_on_whitespace_gives_slash() {
        let src = b"   <a/>   ";
        let out = parse(src, None);
        let entry = out.paths.lookup(0).unwrap();
        let path = out.paths.path_of(entry);
        assert!(path.is_empty());
        assert_eq!(path_expression(&path, &out.names), "/");
    }

    #[test]
    fn comments_and_pi_are_styled() {
        let src = b"<?xml version=\"1.0\"?><!-- hi --><a/>";
        let out = parse(src, None);
        assert!(out.error.is_none());
        let has = |k: StyleKind| out.styles.iter().any(|s| s.kind == k);
        assert!(has(StyleKind::Pi));
        assert!(has(StyleKind::Comment));
        assert!(has(StyleKind::Tag));
    }

    #[test]
    fn cdata_is_styled() {
        let src = b"<a><![CDATA[hello]]></a>";
        let out = parse(src, None);
        assert!(out.error.is_none());
        assert!(out.styles.iter().any(|s| s.kind == StyleKind::CData));
    }

    #[test]
    fn value_bytes_for_element() {
        let src = b"<root><a>hi</a></root>";
        let out = parse(src, None);
        let pos = offset_of(src, b'h');
        let entry_idx = out.paths.lookup(pos).unwrap();
        let entry = out.paths.entries[entry_idx as usize];
        assert_eq!(value_bytes_for_entry(src, &entry), b"<a>hi</a>");
    }

    #[test]
    fn value_bytes_for_attribute() {
        let src = br#"<a name="foo"/>"#;
        let out = parse(src, None);
        let pos = src.iter().position(|&b| b == b'n').unwrap() as u32;
        let entry_idx = out.paths.lookup(pos).unwrap();
        let entry = out.paths.entries[entry_idx as usize];
        assert_eq!(value_bytes_for_entry(src, &entry), b"foo");
    }

    #[test]
    fn pretty_block_element() {
        let src = b"<root><a/><b/></root>";
        let out = String::from_utf8(prettify(src)).unwrap();
        assert_eq!(out, "<root>\n  <a/>\n  <b/>\n</root>\n");
    }

    #[test]
    fn pretty_mixed_content_preserved() {
        let src = b"<p>Hello <b>world</b>!</p>";
        let out = String::from_utf8(prettify(src)).unwrap();
        // p has non-ws text, so it stays as mixed — no whitespace added inside.
        assert_eq!(out, "<p>Hello <b>world</b>!</p>\n");
    }

    #[test]
    fn pretty_nested_block_with_text() {
        let src = b"<root>\n  <a>hi</a>\n</root>";
        let out = String::from_utf8(prettify(src)).unwrap();
        assert_eq!(out, "<root>\n  <a>hi</a>\n</root>\n");
    }

    #[test]
    fn pretty_keeps_xml_declaration() {
        let src = b"<?xml version=\"1.0\"?><root/>";
        let out = String::from_utf8(prettify(src)).unwrap();
        assert_eq!(out, "<?xml version=\"1.0\"?>\n<root/>\n");
    }

    #[test]
    fn pretty_with_comment() {
        let src = b"<root><!-- note --><a/></root>";
        let out = String::from_utf8(prettify(src)).unwrap();
        assert_eq!(out, "<root>\n  <!-- note -->\n  <a/>\n</root>\n");
    }

    #[test]
    fn unmatched_close_recovers() {
        let src = b"<root><a></b></root>";
        let out = parse(src, None);
        // Lenient: parse completes without panic.
        let _ = out;
    }

    #[test]
    fn line_starts_tracked() {
        let src = b"<a>\n<b/>\n</a>";
        let out = parse(src, None);
        assert_eq!(out.line_starts, vec![0, 4, 9]);
    }
}
