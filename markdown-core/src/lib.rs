//! markdown-core — markdown structure parser shared by markview's two
//! views.
//!
//! Walks the source once and produces:
//!
//! * `line_starts` — byte offset of each line, for the source view's
//!   byte-to-line mapping.
//! * `paths` — heading tree, so clicking text shows the section path.
//! * `styles` — header / inline-code / fenced-code style spans for the
//!   source view's syntax colouring.
//! * `blocks` — per-line block classification (paragraph, heading,
//!   list item, blockquote, fenced code, table, hr, blank). Render.rs
//!   in markview uses this to build the rendered NSAttributedString.
//!
//! The renderer for "code" mode keys off `styles`; the renderer for
//! "rendered" mode keys off `blocks` (plus its own inline parse for
//! bold/italic/links). Both views share the same heading-tree path
//! index for the breadcrumb.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// --- progress -------------------------------------------------------

pub struct ProgressSink {
    pub total: u64,
    pub bytes_done: AtomicU64,
}

impl ProgressSink {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            bytes_done: AtomicU64::new(0),
        }
    }

    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        let done = self.bytes_done.load(Ordering::Relaxed) as f64;
        (done / self.total as f64).clamp(0.0, 1.0)
    }
}

pub const PROGRESS_GRANULARITY: usize = 1 << 20;

// --- shared types ---------------------------------------------------

pub type Offset = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Heading,
    Code,
    CodeBlock,
}

#[derive(Debug, Clone, Copy)]
pub struct StyleSpan {
    pub start: Offset,
    pub end: Offset,
    pub kind: StyleKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSegment {
    Root,
    /// ATX heading. `level` is 1..=6; `text` is the interned title.
    Heading { level: u32, text: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct PathEntry {
    pub start: Offset,
    pub end: Offset,
    pub parent: u32,
    pub segment: PathSegment,
}

pub const ROOT_PARENT: u32 = u32::MAX;

#[derive(Debug, Default)]
pub struct NameInterner {
    map: HashMap<Box<[u8]>, u32>,
    buf: Vec<u8>,
    ranges: Vec<(u32, u32)>,
}

impl NameInterner {
    pub fn intern(&mut self, name: &[u8]) -> u32 {
        if let Some(&id) = self.map.get(name) {
            return id;
        }
        let start = self.buf.len() as u32;
        self.buf.extend_from_slice(name);
        let len = name.len() as u32;
        let id = self.ranges.len() as u32;
        self.ranges.push((start, len));
        self.map.insert(name.to_vec().into_boxed_slice(), id);
        id
    }

    pub fn get(&self, id: u32) -> &[u8] {
        let (start, len) = self.ranges[id as usize];
        &self.buf[start as usize..(start + len) as usize]
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

#[derive(Debug, Default)]
pub struct PathIndex {
    pub entries: Vec<PathEntry>,
}

impl PathIndex {
    /// Innermost entry whose range contains `offset`. O(depth).
    pub fn lookup(&self, offset: Offset) -> Option<u32> {
        if self.entries.is_empty() {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.entries[mid].start <= offset {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let mut i = (lo - 1) as u32;
        loop {
            let e = self.entries[i as usize];
            if e.end > offset {
                return Some(i);
            }
            if e.parent == ROOT_PARENT {
                return Some(i);
            }
            i = e.parent;
        }
    }

    pub fn path_of(&self, entry: u32) -> Vec<PathSegment> {
        let mut out = Vec::new();
        let mut i = entry;
        while i != ROOT_PARENT {
            let e = self.entries[i as usize];
            if !matches!(e.segment, PathSegment::Root) {
                out.push(e.segment);
            }
            i = e.parent;
        }
        out.reverse();
        out
    }
}

// --- block classification ------------------------------------------

/// Per-source-line block kind. Drives render.rs in markview when
/// building the rendered attributed string. Inline markers like
/// emphasis or links are not classified here — render.rs scans for
/// those itself inside `Paragraph` / `BlockquoteLine` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Blank,
    Heading { level: u32 },
    Paragraph,
    BlockquoteLine,
    ListItem { ordered: bool, marker_len: u32 },
    /// A line inside a fenced code block (including the open/close fence lines).
    FencedCode,
    /// A line that's part of a contiguous table block. Rendered as a
    /// monospace pre block, not as a real table.
    TableLine,
    /// A `---` / `***` / `___` horizontal-rule line.
    HorizontalRule,
}

#[derive(Debug, Clone, Copy)]
pub struct BlockLine {
    pub line_index: u32,
    pub kind: BlockKind,
}

// --- parse output ---------------------------------------------------

#[derive(Debug, Default)]
pub struct ParseOutput {
    pub line_starts: Vec<Offset>,
    pub paths: PathIndex,
    pub styles: Vec<StyleSpan>,
    pub blocks: Vec<BlockLine>,
    pub names: NameInterner,
    pub error: Option<ParseError>,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub offset: Offset,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone)]
pub enum ParseErrorKind {
    UnexpectedByte(u8),
    UnexpectedEof,
}

// --- parser ---------------------------------------------------------

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line_starts: Vec<u32>,
    paths: Vec<PathEntry>,
    styles: Vec<StyleSpan>,
    blocks: Vec<BlockLine>,
    names: NameInterner,
    heading_stack: Vec<(u32, u32)>,
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
            blocks: Vec::new(),
            names: NameInterner::default(),
            heading_stack: Vec::new(),
            progress,
            next_progress_at,
        }
    }

    #[inline]
    fn advance(&mut self) {
        if self.pos >= self.input.len() {
            return;
        }
        let b = self.input[self.pos];
        if b == b'\n' {
            self.line_starts.push((self.pos + 1) as u32);
        }
        self.pos += 1;
        if self.pos >= self.next_progress_at {
            self.flush_progress();
        }
    }

    #[cold]
    #[inline(never)]
    fn flush_progress(&mut self) {
        if let Some(p) = self.progress {
            p.bytes_done.store(self.pos as u64, Ordering::Relaxed);
        }
        self.next_progress_at = self.pos + PROGRESS_GRANULARITY;
    }

    fn current_line_index(&self) -> u32 {
        (self.line_starts.len() as u32).saturating_sub(1)
    }

    fn record_block(&mut self, kind: BlockKind) {
        let line_index = self.current_line_index();
        self.blocks.push(BlockLine { line_index, kind });
    }

    fn parse_document(&mut self) {
        let root_idx = self.paths.len() as u32;
        self.paths.push(PathEntry {
            start: 0,
            end: 0,
            parent: ROOT_PARENT,
            segment: PathSegment::Root,
        });

        while self.pos < self.input.len() {
            self.parse_line(root_idx);
        }

        let end = self.input.len() as u32;
        while let Some((idx, _)) = self.heading_stack.pop() {
            self.paths[idx as usize].end = end;
        }
        self.paths[root_idx as usize].end = end;
    }

    fn parse_line(&mut self, root_idx: u32) {
        let line_start = self.pos;

        let mut indent = 0;
        while indent < 4
            && self.input.get(line_start + indent).copied() == Some(b' ')
        {
            indent += 1;
        }

        if indent < 4 {
            let after_indent = line_start + indent;
            match self.input.get(after_indent).copied() {
                Some(b'#') => {
                    if self.try_parse_heading(line_start, after_indent, root_idx) {
                        return;
                    }
                }
                Some(c @ (b'`' | b'~')) => {
                    if self.try_parse_fence(line_start, after_indent, c) {
                        return;
                    }
                }
                Some(b'>') => {
                    self.consume_line_classified(BlockKind::BlockquoteLine);
                    return;
                }
                Some(b'-' | b'*' | b'_') => {
                    if let Some(kind) = self.classify_hr_or_list(after_indent) {
                        self.consume_line_classified(kind);
                        return;
                    }
                }
                Some(b'0'..=b'9') => {
                    if let Some(marker_len) = self.try_ordered_list_marker(after_indent) {
                        self.consume_line_classified(BlockKind::ListItem {
                            ordered: true,
                            marker_len,
                        });
                        return;
                    }
                }
                Some(b'|') => {
                    self.consume_line_classified(BlockKind::TableLine);
                    return;
                }
                _ => {}
            }
        }

        if is_blank_line(self.input, line_start) {
            self.consume_line_classified(BlockKind::Blank);
        } else {
            self.parse_paragraph_line();
        }
    }

    fn consume_line_classified(&mut self, kind: BlockKind) {
        self.record_block(kind);
        while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
            self.advance();
        }
        if self.input.get(self.pos) == Some(&b'\n') {
            self.advance();
        }
    }

    fn classify_hr_or_list(&self, after_indent: usize) -> Option<BlockKind> {
        // `-`, `*`, or `_` repeated 3+ times with only whitespace
        // between → horizontal rule. A single `-` or `*` followed by
        // space → list item.
        let c = self.input[after_indent];
        let mut count = 0;
        let mut p = after_indent;
        let mut only_marker = true;
        while p < self.input.len() && self.input[p] != b'\n' {
            let b = self.input[p];
            if b == c {
                count += 1;
            } else if !matches!(b, b' ' | b'\t' | b'\r') {
                only_marker = false;
                break;
            }
            p += 1;
        }
        if only_marker && count >= 3 {
            return Some(BlockKind::HorizontalRule);
        }
        // List item only if `-` or `*` (not `_`) followed by space/tab.
        if matches!(c, b'-' | b'*')
            && self.input.get(after_indent + 1).copied().map(|b| b == b' ' || b == b'\t')
                == Some(true)
        {
            return Some(BlockKind::ListItem {
                ordered: false,
                marker_len: 2,
            });
        }
        None
    }

    fn try_ordered_list_marker(&self, after_indent: usize) -> Option<u32> {
        let mut p = after_indent;
        let mut digits = 0;
        while p < self.input.len() && self.input[p].is_ascii_digit() {
            p += 1;
            digits += 1;
            if digits > 9 {
                return None;
            }
        }
        if digits == 0 {
            return None;
        }
        if !matches!(self.input.get(p).copied(), Some(b'.' | b')')) {
            return None;
        }
        p += 1;
        if !matches!(self.input.get(p).copied(), Some(b' ' | b'\t')) {
            return None;
        }
        Some((p - after_indent + 1) as u32)
    }

    fn try_parse_heading(
        &mut self,
        line_start: usize,
        after_indent: usize,
        root_idx: u32,
    ) -> bool {
        let mut p = after_indent;
        let mut level = 0u32;
        while p < self.input.len() && self.input[p] == b'#' && level < 7 {
            p += 1;
            level += 1;
        }
        if !(1..=6).contains(&level) {
            return false;
        }
        match self.input.get(p).copied() {
            None | Some(b'\n') | Some(b' ') | Some(b'\t') | Some(b'\r') => {}
            _ => return false,
        }

        self.record_block(BlockKind::Heading { level });

        while self.pos < p {
            self.advance();
        }
        if matches!(self.input.get(self.pos).copied(), Some(b' ' | b'\t')) {
            self.advance();
        }

        let text_start = self.pos as u32;
        while let Some(&b) = self.input.get(self.pos) {
            if b == b'\n' {
                break;
            }
            self.advance();
        }
        let line_end_excl_nl = self.pos as u32;

        let raw_title = &self.input[text_start as usize..line_end_excl_nl as usize];
        let title = strip_atx_trailing(raw_title);
        let title_id = self.names.intern(title);

        while let Some(&(idx, lvl)) = self.heading_stack.last() {
            if lvl >= level {
                self.paths[idx as usize].end = line_start as u32;
                self.heading_stack.pop();
            } else {
                break;
            }
        }
        let parent = self
            .heading_stack
            .last()
            .map(|&(idx, _)| idx)
            .unwrap_or(root_idx);

        self.styles.push(StyleSpan {
            start: line_start as u32,
            end: line_end_excl_nl,
            kind: StyleKind::Heading,
        });

        let entry_idx = self.paths.len() as u32;
        self.paths.push(PathEntry {
            start: line_start as u32,
            end: 0,
            parent,
            segment: PathSegment::Heading {
                level,
                text: title_id,
            },
        });
        self.heading_stack.push((entry_idx, level));

        if self.input.get(self.pos) == Some(&b'\n') {
            self.advance();
        }
        true
    }

    fn try_parse_fence(
        &mut self,
        line_start: usize,
        after_indent: usize,
        fence_char: u8,
    ) -> bool {
        let mut count = 0;
        let mut p = after_indent;
        while p < self.input.len() && self.input[p] == fence_char {
            p += 1;
            count += 1;
        }
        if count < 3 {
            return false;
        }

        self.record_block(BlockKind::FencedCode);

        while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
            self.advance();
        }
        if self.input.get(self.pos) == Some(&b'\n') {
            self.advance();
        }

        let mut close_end = self.input.len();
        while self.pos < self.input.len() {
            let l_start = self.pos;
            let mut q_indent = 0;
            while q_indent < 4
                && self.input.get(l_start + q_indent).copied() == Some(b' ')
            {
                q_indent += 1;
            }
            let is_close = if q_indent < 4 {
                let q_after = l_start + q_indent;
                if self.input.get(q_after).copied() == Some(fence_char) {
                    let mut q_count = 0;
                    let mut qp = q_after;
                    while qp < self.input.len() && self.input[qp] == fence_char {
                        qp += 1;
                        q_count += 1;
                    }
                    if q_count >= count {
                        let mut tail = qp;
                        let mut ok = true;
                        while tail < self.input.len() && self.input[tail] != b'\n' {
                            if !matches!(self.input[tail], b' ' | b'\t' | b'\r') {
                                ok = false;
                                break;
                            }
                            tail += 1;
                        }
                        ok
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            self.record_block(BlockKind::FencedCode);
            while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
                self.advance();
            }
            if self.input.get(self.pos) == Some(&b'\n') {
                self.advance();
            }

            if is_close {
                close_end = self.pos;
                break;
            }
        }

        self.styles.push(StyleSpan {
            start: line_start as u32,
            end: close_end as u32,
            kind: StyleKind::CodeBlock,
        });
        true
    }

    fn parse_paragraph_line(&mut self) {
        self.record_block(BlockKind::Paragraph);
        while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
            if self.input[self.pos] == b'`' {
                self.try_scan_inline_code();
            } else {
                self.advance();
            }
        }
        if self.input.get(self.pos) == Some(&b'\n') {
            self.advance();
        }
    }

    fn try_scan_inline_code(&mut self) {
        let start = self.pos;
        let mut count = 0;
        while self.input.get(self.pos) == Some(&b'`') {
            self.advance();
            count += 1;
        }
        let mut scan = self.pos;
        let mut found_end = None;
        while scan < self.input.len() && self.input[scan] != b'\n' {
            if self.input[scan] == b'`' {
                let mut q = scan;
                while q < self.input.len() && self.input[q] == b'`' {
                    q += 1;
                }
                if q - scan == count {
                    found_end = Some(q);
                    break;
                }
                scan = q;
            } else {
                scan += 1;
            }
        }
        if let Some(end) = found_end {
            while self.pos < end {
                self.advance();
            }
            self.styles.push(StyleSpan {
                start: start as u32,
                end: end as u32,
                kind: StyleKind::Code,
            });
        }
    }

    fn finish(self) -> ParseOutput {
        ParseOutput {
            line_starts: self.line_starts,
            paths: PathIndex {
                entries: self.paths,
            },
            styles: self.styles,
            blocks: self.blocks,
            names: self.names,
            error: None,
            bytes: self.input.len(),
        }
    }
}

fn is_blank_line(input: &[u8], line_start: usize) -> bool {
    let mut p = line_start;
    while p < input.len() {
        match input[p] {
            b'\n' => return true,
            b' ' | b'\t' | b'\r' => p += 1,
            _ => return false,
        }
    }
    true
}

fn strip_atx_trailing(raw: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < raw.len() && matches!(raw[start], b' ' | b'\t') {
        start += 1;
    }
    let mut end = raw.len();
    while end > start && matches!(raw[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    if end > start {
        let mut k = end;
        while k > start && raw[k - 1] == b'#' {
            k -= 1;
        }
        if k < end && (k == start || matches!(raw[k - 1], b' ' | b'\t')) {
            end = k;
            while end > start && matches!(raw[end - 1], b' ' | b'\t') {
                end -= 1;
            }
        }
    }
    &raw[start..end]
}

pub fn parse(input: &[u8], progress: Option<&ProgressSink>) -> ParseOutput {
    let mut p = Parser::new(input, progress);
    p.parse_document();
    if let Some(sink) = progress {
        sink.bytes_done.store(input.len() as u64, Ordering::Relaxed);
    }
    p.finish()
}

/// Markdown has no canonical form. Prettify returns the input verbatim
/// so the source view's "prettify" toggle (if any) is a no-op.
pub fn prettify(input: &[u8]) -> Vec<u8> {
    input.to_vec()
}

pub fn path_expression(segments: &[PathSegment], names: &NameInterner) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for seg in segments {
        if let PathSegment::Heading { text, .. } = seg {
            out.push('/');
            let bytes = names.get(*text);
            out.push_str(std::str::from_utf8(bytes).unwrap_or("\u{FFFD}"));
        }
    }
    if out.is_empty() { "/".to_string() } else { out }
}

pub fn value_bytes_for_entry<'a>(bytes: &'a [u8], entry: &PathEntry) -> &'a [u8] {
    let start = (entry.start as usize).min(bytes.len());
    let end = (entry.end as usize).min(bytes.len());
    if end <= start {
        return &[];
    }
    let slice = &bytes[start..end];
    match entry.segment {
        PathSegment::Root => slice.trim_ascii(),
        _ => slice,
    }
}

/// Walk a block-level table run. Returns the (line_index_start, end_offset)
/// of the contiguous table block beginning at `start_line`, or None if
/// the line isn't a table line.
pub fn find_table_run(blocks: &[BlockLine], start_index: usize) -> Option<(usize, usize)> {
    if blocks.get(start_index)?.kind != BlockKind::TableLine {
        return None;
    }
    let mut end = start_index + 1;
    while let Some(b) = blocks.get(end) {
        if b.kind != BlockKind::TableLine {
            break;
        }
        end += 1;
    }
    Some((start_index, end))
}

/// Column alignment derived from the `| :--: |` separator row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellAlign {
    Left,
    Center,
    Right,
}

/// Split a table row on `|`. Leading and trailing pipes (and surrounding
/// whitespace) are stripped, and a backslash-escaped pipe (`\|`) is
/// treated as a literal cell character rather than a separator.
pub fn split_table_row(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    // Skip leading whitespace.
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    // Skip a leading `|`.
    let starts_with_pipe = i < bytes.len() && bytes[i] == b'|';
    if starts_with_pipe {
        i += 1;
    }
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && bytes.get(i + 1) == Some(&b'|') {
            cur.push('|');
            i += 2;
            continue;
        }
        if b == b'|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
            i += 1;
            continue;
        }
        if b == b'\r' {
            i += 1;
            continue;
        }
        // UTF-8 safe push: advance by char length.
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        cur.push_str(&line[i..end]);
        i = end;
    }
    let trimmed = cur.trim();
    // If the row ended on a trailing pipe, `cur` is just whitespace and
    // we already pushed the real last cell; skip the empty trailing one.
    if !(starts_with_pipe && trimmed.is_empty() && !cells.is_empty()) {
        cells.push(trimmed.to_string());
    }
    cells
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xC0 {
        1 // continuation byte — shouldn't start a char, but be defensive
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

/// Recognise a table-separator row like `| :--- | ---: | :---: |`.
/// Returns the per-column alignments on success.
pub fn parse_table_separator(line: &str) -> Option<Vec<CellAlign>> {
    let cells = split_table_row(line);
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for cell in &cells {
        let bytes = cell.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let left = bytes.first() == Some(&b':');
        let right = bytes.last() == Some(&b':');
        let start = if left { 1 } else { 0 };
        let end = if right { bytes.len() - 1 } else { bytes.len() };
        if end <= start {
            return None;
        }
        let middle = &bytes[start..end];
        if middle.is_empty() || !middle.iter().all(|&b| b == b'-') {
            return None;
        }
        if middle.len() < 3 {
            // Spec requires at least 3 dashes, but real-world tables
            // often use just two. Accept >=1 to match GFM in practice.
            // (Still reject 0, handled above.)
        }
        let align = match (left, right) {
            (true, true) => CellAlign::Center,
            (false, true) => CellAlign::Right,
            _ => CellAlign::Left,
        };
        out.push(align);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_md(s: &[u8]) -> ParseOutput {
        parse(s, None)
    }

    fn heading_paths(out: &ParseOutput) -> Vec<String> {
        out.paths
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e.segment {
                PathSegment::Heading { .. } => {
                    let p = out.paths.path_of(i as u32);
                    Some(path_expression(&p, &out.names))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_doc_has_only_root() {
        let out = parse_md(b"");
        assert_eq!(out.paths.entries.len(), 1);
        assert!(matches!(out.paths.entries[0].segment, PathSegment::Root));
    }

    #[test]
    fn nested_headers_form_tree() {
        let src = b"# Intro\n## Setup\n## Usage\n### Example\n# Reference\n";
        let out = parse_md(src);
        assert_eq!(
            heading_paths(&out),
            vec![
                "/Intro",
                "/Intro/Setup",
                "/Intro/Usage",
                "/Intro/Usage/Example",
                "/Reference",
            ]
        );
    }

    #[test]
    fn closing_hashes_stripped() {
        let out = parse_md(b"## Title ##\n");
        assert_eq!(heading_paths(&out), vec!["/Title"]);
    }

    #[test]
    fn hash_without_space_is_not_a_header() {
        let out = parse_md(b"#nope\n");
        assert!(heading_paths(&out).is_empty());
    }

    #[test]
    fn fenced_code_hides_inner_headers() {
        let src = b"```\n# not a heading\n```\n# Real\n";
        let out = parse_md(src);
        assert_eq!(heading_paths(&out), vec!["/Real"]);
        assert!(out.styles.iter().any(|s| s.kind == StyleKind::CodeBlock));
    }

    #[test]
    fn inline_code_styled() {
        let out = parse_md(b"call `foo()` here\n");
        let span = out
            .styles
            .iter()
            .find(|s| s.kind == StyleKind::Code)
            .expect("inline code span");
        let bytes = &b"call `foo()` here\n"[span.start as usize..span.end as usize];
        assert_eq!(bytes, b"`foo()`");
    }

    #[test]
    fn lookup_inside_section_returns_section() {
        let src = b"# A\n## B\ninside B\n";
        let out = parse_md(src);
        let pos = src.iter().position(|&b| b == b'i').unwrap() as u32;
        let idx = out.paths.lookup(pos).unwrap();
        let p = out.paths.path_of(idx);
        assert_eq!(path_expression(&p, &out.names), "/A/B");
    }

    #[test]
    fn blocks_classify_paragraph_and_blank() {
        let out = parse_md(b"hello\n\nworld\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![BlockKind::Paragraph, BlockKind::Blank, BlockKind::Paragraph]
        );
    }

    #[test]
    fn blocks_classify_blockquote() {
        let out = parse_md(b"> quoted\nplain\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![BlockKind::BlockquoteLine, BlockKind::Paragraph]);
    }

    #[test]
    fn blocks_classify_unordered_list() {
        let out = parse_md(b"- one\n- two\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert!(matches!(kinds[0], BlockKind::ListItem { ordered: false, .. }));
        assert!(matches!(kinds[1], BlockKind::ListItem { ordered: false, .. }));
    }

    #[test]
    fn blocks_classify_ordered_list() {
        let out = parse_md(b"1. one\n2. two\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert!(matches!(kinds[0], BlockKind::ListItem { ordered: true, .. }));
        assert!(matches!(kinds[1], BlockKind::ListItem { ordered: true, .. }));
    }

    #[test]
    fn blocks_classify_hr() {
        let out = parse_md(b"---\n");
        assert_eq!(out.blocks[0].kind, BlockKind::HorizontalRule);
    }

    #[test]
    fn blocks_classify_table() {
        let out = parse_md(b"| a | b |\n| - | - |\n| 1 | 2 |\n");
        for b in &out.blocks {
            assert_eq!(b.kind, BlockKind::TableLine);
        }
    }

    #[test]
    fn blocks_classify_fenced_code() {
        let out = parse_md(b"```\nfoo\n```\n");
        for b in &out.blocks {
            assert_eq!(b.kind, BlockKind::FencedCode);
        }
    }

    #[test]
    fn blocks_classify_headings() {
        let out = parse_md(b"# A\n## B\nbody\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading { level: 1 },
                BlockKind::Heading { level: 2 },
                BlockKind::Paragraph
            ]
        );
    }

    #[test]
    fn prettify_is_verbatim() {
        let src = b"# Title\n\nBody.\n";
        assert_eq!(prettify(src), src.to_vec());
    }

    #[test]
    fn star_list_works() {
        let out = parse_md(b"* one\n* two\n");
        let kinds: Vec<_> = out.blocks.iter().map(|b| b.kind).collect();
        assert!(matches!(kinds[0], BlockKind::ListItem { ordered: false, .. }));
    }

    #[test]
    fn underscore_hr() {
        let out = parse_md(b"___\n");
        assert_eq!(out.blocks[0].kind, BlockKind::HorizontalRule);
    }

    #[test]
    fn find_table_run_picks_contiguous_range() {
        let out = parse_md(b"para\n| a | b |\n| - | - |\nafter\n");
        // blocks: [Paragraph, TableLine, TableLine, Paragraph]
        assert_eq!(find_table_run(&out.blocks, 0), None);
        assert_eq!(find_table_run(&out.blocks, 1), Some((1, 3)));
        assert_eq!(find_table_run(&out.blocks, 3), None);
    }

    #[test]
    fn split_table_row_basic() {
        assert_eq!(split_table_row("| a | b | c |"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_table_row_no_outer_pipes() {
        assert_eq!(split_table_row("a | b | c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_table_row_escaped_pipe() {
        assert_eq!(split_table_row("| a\\|b | c |"), vec!["a|b", "c"]);
    }

    #[test]
    fn split_table_row_trims_whitespace() {
        assert_eq!(split_table_row("|   x   |  y  |"), vec!["x", "y"]);
    }

    #[test]
    fn parse_separator_all_alignments() {
        assert_eq!(
            parse_table_separator("| :--- | ---: | :---: | --- |"),
            Some(vec![
                CellAlign::Left,
                CellAlign::Right,
                CellAlign::Center,
                CellAlign::Left,
            ])
        );
    }

    #[test]
    fn parse_separator_rejects_non_separator() {
        assert_eq!(parse_table_separator("| header | row |"), None);
        assert_eq!(parse_table_separator("| :no | --- |"), None);
    }
}
