//! Markdown tokenizer — builds the path tree from ATX headings and
//! emits style spans for headings, fenced code blocks, and inline
//! `code`. Emphasis, links, lists, blockquotes, and indented code
//! blocks are deliberately not styled: they render as plain text. The
//! goal is structural navigation (the heading hierarchy), not full
//! presentation.
//!
//! Prettify is a verbatim copy — markdown has no canonical form.

use super::{
    NameInterner, PROGRESS_GRANULARITY, ParseOutput, PathEntry, PathIndex, PathSegment,
    ProgressSink, ROOT_PARENT, StyleKind, StyleSpan,
};
use std::sync::atomic::Ordering;

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line_starts: Vec<u32>,
    paths: Vec<PathEntry>,
    styles: Vec<StyleSpan>,
    names: NameInterner,
    /// Stack of (heading entry idx, level). Encountering a heading at
    /// level L pops every entry with level >= L (setting its `end` to
    /// the new heading's line start) and pushes this one. Drained at
    /// EOF.
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

        // Up to 3 leading spaces are allowed before a heading or fence
        // marker (CommonMark §4). 4+ would be an indented code block,
        // which we render as plain text.
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
                _ => {}
            }
        }

        self.parse_text_line();
    }

    /// Returns true if the line was a valid ATX heading (1-6 `#` then
    /// space, tab, end-of-line, or end-of-file).
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

        // Consume indent + hashes + one optional separating space/tab.
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

    /// Returns true if a fenced block (opening + content + closing or
    /// EOF) was consumed. A fenced block is 3+ of the same fence char
    /// (` or ~), optionally preceded by up to 3 spaces.
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

        // Walk to end of the opening-fence line and consume the newline.
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
            if q_indent < 4 {
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
                        if ok {
                            while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
                                self.advance();
                            }
                            if self.input.get(self.pos) == Some(&b'\n') {
                                self.advance();
                            }
                            close_end = self.pos;
                            break;
                        }
                    }
                }
            }
            // Not a closing fence — drain to end-of-line and continue.
            while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
                self.advance();
            }
            if self.input.get(self.pos) == Some(&b'\n') {
                self.advance();
            }
        }

        self.styles.push(StyleSpan {
            start: line_start as u32,
            end: close_end as u32,
            kind: StyleKind::CodeBlock,
        });
        true
    }

    fn parse_text_line(&mut self) {
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

    /// Walk a `` ` ``-bounded inline-code span, matching the opening run
    /// length exactly. Dangling backticks (no matching close on the same
    /// line) are left as plain text.
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
            names: self.names,
            error: None,
            bytes: self.input.len(),
        }
    }
}

/// Trim the title bytes between an ATX heading's hashes and end-of-line:
/// strip leading separator whitespace, then trailing whitespace, then an
/// optional closing `#` run preceded by whitespace.
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
/// so the toggle is a visual no-op for markdown.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entries_paths(out: &ParseOutput) -> Vec<String> {
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
        let out = parse(b"", None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
        assert!(matches!(out.paths.entries[0].segment, PathSegment::Root));
    }

    #[test]
    fn single_h1() {
        let out = parse(b"# Hello\n", None);
        assert_eq!(entries_paths(&out), vec!["/Hello"]);
    }

    #[test]
    fn nested_headers_form_tree() {
        let src = b"# Intro\n\n## Setup\n\n## Usage\n\n### Example\n\n# Reference\n";
        let out = parse(src, None);
        assert_eq!(
            entries_paths(&out),
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
    fn closing_hashes_are_stripped_from_title() {
        let out = parse(b"## Title ##\n", None);
        assert_eq!(entries_paths(&out), vec!["/Title"]);
    }

    #[test]
    fn hash_without_space_is_not_a_header() {
        let out = parse(b"#nope\n", None);
        assert!(entries_paths(&out).is_empty());
    }

    #[test]
    fn seven_hashes_is_not_a_header() {
        let out = parse(b"####### nope\n", None);
        assert!(entries_paths(&out).is_empty());
    }

    #[test]
    fn lookup_in_section_body_returns_section_path() {
        let src = b"# Intro\nbody text here\n## Setup\nmore text\n";
        let out = parse(src, None);
        let pos = src.iter().position(|&b| b == b'b').unwrap() as u32;
        let idx = out.paths.lookup(pos).unwrap();
        let p = out.paths.path_of(idx);
        assert_eq!(path_expression(&p, &out.names), "/Intro");
    }

    #[test]
    fn lookup_in_nested_section_returns_innermost() {
        let src = b"# A\n## B\ninside B\n";
        let out = parse(src, None);
        let pos = src.iter().position(|&b| b == b'i').unwrap() as u32;
        let idx = out.paths.lookup(pos).unwrap();
        let p = out.paths.path_of(idx);
        assert_eq!(path_expression(&p, &out.names), "/A/B");
    }

    #[test]
    fn h1_end_set_at_next_h1_start() {
        let src = b"# A\nbody\n# B\n";
        let out = parse(src, None);
        let a = &out.paths.entries[1];
        let b_off = src.windows(3).position(|w| w == b"# B").unwrap();
        assert_eq!(a.end, b_off as u32);
    }

    #[test]
    fn h2_closes_when_followed_by_h1() {
        let src = b"# A\n## B\nbody\n# C\n";
        let out = parse(src, None);
        // entries: 0=root, 1=#A, 2=##B, 3=#C
        let b_h2 = &out.paths.entries[2];
        let c_off = src.windows(3).position(|w| w == b"# C").unwrap();
        assert_eq!(b_h2.end, c_off as u32);
    }

    #[test]
    fn fenced_code_styled_and_hides_inner_headers() {
        let src = b"prefix\n```rust\n# not a heading\n```\n# Real\n";
        let out = parse(src, None);
        assert!(
            out.styles.iter().any(|s| s.kind == StyleKind::CodeBlock),
            "expected a code-block style span"
        );
        assert_eq!(entries_paths(&out), vec!["/Real"]);
    }

    #[test]
    fn tilde_fence_works() {
        let src = b"~~~\n# inner\n~~~\n";
        let out = parse(src, None);
        assert!(out.styles.iter().any(|s| s.kind == StyleKind::CodeBlock));
        assert!(entries_paths(&out).is_empty());
    }

    #[test]
    fn unclosed_fence_extends_to_eof() {
        let src = b"```\nfoo\nbar\n";
        let out = parse(src, None);
        let cb = out
            .styles
            .iter()
            .find(|s| s.kind == StyleKind::CodeBlock)
            .expect("code block span");
        assert_eq!(cb.start, 0);
        assert_eq!(cb.end as usize, src.len());
    }

    #[test]
    fn inline_code_styled() {
        let src = b"call `foo()` here\n";
        let out = parse(src, None);
        let code = out
            .styles
            .iter()
            .find(|s| s.kind == StyleKind::Code)
            .expect("inline-code span");
        let s = &src[code.start as usize..code.end as usize];
        assert_eq!(s, b"`foo()`");
    }

    #[test]
    fn dangling_backtick_is_plain_text() {
        let src = b"unmatched ` backtick\n";
        let out = parse(src, None);
        assert!(out.styles.iter().all(|s| s.kind != StyleKind::Code));
    }

    #[test]
    fn prettify_is_verbatim() {
        let src = b"# Title\n\nBody text.\n";
        assert_eq!(prettify(src), src.to_vec());
    }

    #[test]
    fn value_bytes_for_h1_section_spans_to_eof_when_last() {
        let src = b"# A\nhello\n";
        let out = parse(src, None);
        let a = out.paths.entries[1];
        assert_eq!(value_bytes_for_entry(src, &a), src);
    }

    #[test]
    fn value_bytes_for_root_trimmed() {
        let src = b"  \n# A\n";
        let out = parse(src, None);
        let root = out.paths.entries[0];
        assert_eq!(value_bytes_for_entry(src, &root), b"# A");
    }

    #[test]
    fn line_starts_track_newlines() {
        let src = b"# A\n\nbody\n";
        let out = parse(src, None);
        assert_eq!(out.line_starts, vec![0, 4, 5, 10]);
    }

    #[test]
    fn path_expression_empty_is_slash() {
        let names = NameInterner::default();
        assert_eq!(path_expression(&[], &names), "/");
    }
}
