//! Hand-rolled streaming JSON tokenizer + prettifier.
//!
//! Walks `&[u8]` once and produces the three indexes the renderer needs:
//!
//! * `line_starts` — byte offsets of each line start, for `drawRect:` to
//!   map pixel Y back to a byte range.
//! * `PathIndex`   — sorted list of nested value ranges tagged with a
//!   path segment, for click → JSON path lookup.
//! * `styles`      — non-overlapping style ranges for syntax colouring.
//!
//! Lenient: on parse error the partial indexes are still returned, so
//! the viewer can display broken files up to the point they go wrong.

use super::{
    NameInterner, Offset, PROGRESS_GRANULARITY, ParseError, ParseErrorKind, ParseOutput,
    PathEntry, PathIndex, PathSegment, ProgressSink, ROOT_PARENT, StyleKind, StyleSpan,
};
use std::sync::atomic::Ordering;

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line_starts: Vec<Offset>,
    paths: Vec<PathEntry>,
    styles: Vec<StyleSpan>,
    names: NameInterner,
    scratch: Vec<u8>,
    progress: Option<&'a ProgressSink>,
    /// Next `pos` value at which we'll publish progress. Set to
    /// `usize::MAX` when there's no sink so the hot-path branch is
    /// effectively dead.
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
            scratch: Vec::with_capacity(64),
            progress,
            next_progress_at,
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
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

    /// Cold path: publish current `pos` to the progress sink and bump
    /// the next threshold. `#[cold]` keeps the hot path tight.
    #[cold]
    #[inline(never)]
    fn flush_progress(&mut self) {
        if let Some(p) = self.progress {
            p.bytes_done.store(self.pos as u64, Ordering::Relaxed);
        }
        self.next_progress_at = self.pos + PROGRESS_GRANULARITY;
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => {
                    self.advance();
                }
                _ => break,
            }
        }
    }

    fn err(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            offset: self.pos as u32,
            kind,
        }
    }

    fn parse_document(&mut self) -> Result<(), ParseError> {
        // Root spans the whole input so a click on leading/trailing
        // whitespace still resolves to path `.`.
        let root_idx = self.paths.len() as u32;
        self.paths.push(PathEntry {
            start: 0,
            end: 0,
            parent: ROOT_PARENT,
            segment: PathSegment::Root,
        });
        self.parse_value_body(root_idx)?;
        self.paths[root_idx as usize].end = self.input.len() as u32;
        // Trailing garbage is ignored — we're a viewer, not a validator.
        Ok(())
    }

    fn parse_value_body(&mut self, this: u32) -> Result<(), ParseError> {
        self.skip_ws();
        let b = self
            .peek()
            .ok_or_else(|| self.err(ParseErrorKind::UnexpectedEof))?;
        match b {
            b'{' => self.parse_object(this),
            b'[' => self.parse_array(this),
            b'"' => self.parse_string_value(),
            b't' => self.parse_literal(b"true", StyleKind::Bool),
            b'f' => self.parse_literal(b"false", StyleKind::Bool),
            b'n' => self.parse_literal(b"null", StyleKind::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(self.err(ParseErrorKind::UnexpectedByte(b))),
        }
    }

    fn parse_object(&mut self, this: u32) -> Result<(), ParseError> {
        self.advance(); // '{'
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.advance();
            return Ok(());
        }
        loop {
            self.skip_ws();
            let field_start = self.pos as u32;
            if self.peek() != Some(b'"') {
                return Err(self.err(ParseErrorKind::UnexpectedByte(self.peek().unwrap_or(0))));
            }
            let key_start = self.pos as u32;
            let content_range = self.parse_string_raw()?;
            let key_end = self.pos as u32;
            self.styles.push(StyleSpan {
                start: key_start,
                end: key_end,
                kind: StyleKind::Key,
            });

            // Decode escapes into scratch, then intern.
            decode_escapes_into(self.input, content_range, &mut self.scratch)?;
            let key_id = self.names.intern(&self.scratch);

            let field_idx = self.paths.len() as u32;
            self.paths.push(PathEntry {
                start: field_start,
                end: 0,
                parent: this,
                segment: PathSegment::Key(key_id),
            });

            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err(ParseErrorKind::UnexpectedByte(self.peek().unwrap_or(0))));
            }
            self.advance();

            self.parse_value_body(field_idx)?;
            self.paths[field_idx as usize].end = self.pos as u32;

            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.advance();
                }
                Some(b'}') => {
                    self.advance();
                    return Ok(());
                }
                Some(b) => return Err(self.err(ParseErrorKind::UnexpectedByte(b))),
                None => return Err(self.err(ParseErrorKind::UnexpectedEof)),
            }
        }
    }

    fn parse_array(&mut self, this: u32) -> Result<(), ParseError> {
        self.advance(); // '['
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.advance();
            return Ok(());
        }
        let mut index = 0u32;
        loop {
            self.skip_ws();
            let elem_start = self.pos as u32;
            let elem_idx = self.paths.len() as u32;
            self.paths.push(PathEntry {
                start: elem_start,
                end: 0,
                parent: this,
                segment: PathSegment::Index(index),
            });
            self.parse_value_body(elem_idx)?;
            self.paths[elem_idx as usize].end = self.pos as u32;
            index += 1;

            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.advance();
                }
                Some(b']') => {
                    self.advance();
                    return Ok(());
                }
                Some(b) => return Err(self.err(ParseErrorKind::UnexpectedByte(b))),
                None => return Err(self.err(ParseErrorKind::UnexpectedEof)),
            }
        }
    }

    fn parse_string_value(&mut self) -> Result<(), ParseError> {
        let start = self.pos as u32;
        self.parse_string_raw()?;
        let end = self.pos as u32;
        self.styles.push(StyleSpan {
            start,
            end,
            kind: StyleKind::String,
        });
        Ok(())
    }

    /// Consume opening `"` through matching `"`. Returns the byte range
    /// of the raw (still-escaped) contents, exclusive of quotes.
    fn parse_string_raw(&mut self) -> Result<(u32, u32), ParseError> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.advance();
        let content_start = self.pos as u32;
        loop {
            let b = self
                .peek()
                .ok_or_else(|| self.err(ParseErrorKind::UnexpectedEof))?;
            match b {
                b'"' => {
                    let content_end = self.pos as u32;
                    self.advance();
                    return Ok((content_start, content_end));
                }
                b'\\' => {
                    self.advance();
                    let esc = self
                        .peek()
                        .ok_or_else(|| self.err(ParseErrorKind::UnexpectedEof))?;
                    if esc == b'u' {
                        self.advance();
                        for _ in 0..4 {
                            if self.peek().is_none() {
                                return Err(self.err(ParseErrorKind::UnexpectedEof));
                            }
                            self.advance();
                        }
                    } else {
                        self.advance();
                    }
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn parse_literal(
        &mut self,
        word: &'static [u8],
        kind: StyleKind,
    ) -> Result<(), ParseError> {
        let start = self.pos as u32;
        for &expected in word {
            let actual = self
                .peek()
                .ok_or_else(|| self.err(ParseErrorKind::UnexpectedEof))?;
            if actual != expected {
                return Err(self.err(ParseErrorKind::UnexpectedByte(actual)));
            }
            self.advance();
        }
        let end = self.pos as u32;
        self.styles.push(StyleSpan { start, end, kind });
        Ok(())
    }

    fn parse_number(&mut self) -> Result<(), ParseError> {
        let start = self.pos as u32;
        if self.peek() == Some(b'-') {
            self.advance();
        }
        let mut has_digit = false;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.advance();
                has_digit = true;
            } else {
                break;
            }
        }
        if !has_digit {
            return Err(self.err(ParseErrorKind::InvalidNumber));
        }
        if self.peek() == Some(b'.') {
            self.advance();
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.advance();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.advance();
            }
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let end = self.pos as u32;
        self.styles.push(StyleSpan {
            start,
            end,
            kind: StyleKind::Number,
        });
        Ok(())
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

fn decode_escapes_into(
    input: &[u8],
    range: (u32, u32),
    out: &mut Vec<u8>,
) -> Result<(), ParseError> {
    out.clear();
    let (a, b) = range;
    let src = &input[a as usize..b as usize];
    let mut i = 0usize;
    while i < src.len() {
        let c = src[i];
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= src.len() {
            return Err(ParseError {
                offset: a + i as u32,
                kind: ParseErrorKind::InvalidEscape,
            });
        }
        let e = src[i];
        i += 1;
        match e {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                if i + 4 > src.len() {
                    return Err(ParseError {
                        offset: a + i as u32,
                        kind: ParseErrorKind::InvalidEscape,
                    });
                }
                let n = parse_hex4(&src[i..i + 4]).ok_or_else(|| ParseError {
                    offset: a + i as u32,
                    kind: ParseErrorKind::InvalidEscape,
                })?;
                i += 4;
                let cp = if (0xD800..=0xDBFF).contains(&n) {
                    if i + 6 <= src.len() && src[i] == b'\\' && src[i + 1] == b'u' {
                        if let Some(n2) = parse_hex4(&src[i + 2..i + 6]) {
                            if (0xDC00..=0xDFFF).contains(&n2) {
                                i += 6;
                                0x10000 + ((n - 0xD800) << 10) + (n2 - 0xDC00)
                            } else {
                                0xFFFD
                            }
                        } else {
                            0xFFFD
                        }
                    } else {
                        0xFFFD
                    }
                } else if (0xDC00..=0xDFFF).contains(&n) {
                    0xFFFD
                } else {
                    n
                };
                let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                let mut tmp = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
            _ => {
                return Err(ParseError {
                    offset: a + i as u32,
                    kind: ParseErrorKind::InvalidEscape,
                });
            }
        }
    }
    Ok(())
}

fn parse_hex4(bytes: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    for &h in bytes {
        n = n * 16
            + match h {
                b'0'..=b'9' => (h - b'0') as u32,
                b'a'..=b'f' => (h - b'a' + 10) as u32,
                b'A'..=b'F' => (h - b'A' + 10) as u32,
                _ => return None,
            };
    }
    Some(n)
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

/// Render a JSON path as a jq expression. Non-identifier keys are
/// emitted as `["..."]`; array indices as `[N]`. Empty path → ".".
pub fn path_expression(segments: &[PathSegment], names: &NameInterner) -> String {
    if segments.is_empty() {
        return ".".to_string();
    }
    let mut out = String::new();
    for seg in segments {
        match seg {
            PathSegment::Root => {}
            PathSegment::Key(id) => {
                let bytes = names.get(*id);
                let s = std::str::from_utf8(bytes).unwrap_or("\u{FFFD}");
                if is_identifier(s) {
                    out.push('.');
                    out.push_str(s);
                } else {
                    out.push_str(".[\"");
                    for ch in s.chars() {
                        match ch {
                            '"' => out.push_str("\\\""),
                            '\\' => out.push_str("\\\\"),
                            c if (c as u32) < 0x20 => {
                                use std::fmt::Write;
                                let _ = write!(out, "\\u{:04x}", c as u32);
                            }
                            c => out.push(c),
                        }
                    }
                    out.push_str("\"]");
                }
            }
            PathSegment::Index(i) => {
                use std::fmt::Write;
                let _ = write!(out, "[{}]", i);
            }
            // Non-JSON segments shouldn't appear in a JSON doc — be safe.
            PathSegment::Element { .. }
            | PathSegment::Attribute(_)
            | PathSegment::Heading { .. } => {}
        }
    }
    // When the first segment is an index, `out` starts with `[` — jq
    // wants `.[0]`, so prepend a dot in that case.
    if out.starts_with('[') {
        let mut tmp = String::with_capacity(out.len() + 1);
        tmp.push('.');
        tmp.push_str(&out);
        out = tmp;
    }
    out
}

/// Bytes of the JSON *value* covered by `entry`. For object fields the
/// entry's raw range covers `"key": value`; this skips the key + colon
/// so the result is a standalone JSON value. For array elements and
/// the root the raw range is already a value, modulo leading/trailing
/// whitespace.
pub fn value_bytes_for_entry<'a>(bytes: &'a [u8], entry: &PathEntry) -> &'a [u8] {
    let start = (entry.start as usize).min(bytes.len());
    let end = (entry.end as usize).min(bytes.len());
    if end <= start {
        return &[];
    }
    let slice = &bytes[start..end];
    match entry.segment {
        PathSegment::Root => slice.trim_ascii(),
        PathSegment::Index(_) => slice,
        PathSegment::Key(_) => skip_key_and_colon(slice),
        PathSegment::Element { .. }
        | PathSegment::Attribute(_)
        | PathSegment::Heading { .. } => slice,
    }
}

fn skip_key_and_colon(slice: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < slice.len() && matches!(slice[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    if i >= slice.len() || slice[i] != b'"' {
        return slice;
    }
    i += 1;
    while i < slice.len() {
        let c = slice[i];
        i += 1;
        if c == b'\\' && i < slice.len() {
            i += 1;
        } else if c == b'"' {
            break;
        }
    }
    while i < slice.len() && matches!(slice[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    if i < slice.len() && slice[i] == b':' {
        i += 1;
    }
    while i < slice.len() && matches!(slice[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    &slice[i..]
}

fn is_identifier(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// --- prettifier -----------------------------------------------------

const INDENT: &[u8] = b"  ";

/// Byte-level state machine: tokenises the input just enough to
/// recognise structural punctuation and string boundaries, and re-emits
/// with 2-space indentation and a newline after each comma.
pub fn prettify(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 4);
    let mut depth: usize = 0;
    let mut i: usize = 0;
    let n = input.len();

    while i < n {
        let b = input[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'{' | b'[' => {
                out.push(b);
                i += 1;
                let mut j = i;
                while j < n && matches!(input[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                let close = if b == b'{' { b'}' } else { b']' };
                if j < n && input[j] == close {
                    out.push(close);
                    i = j + 1;
                } else {
                    depth += 1;
                    write_indent(&mut out, depth);
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                write_indent(&mut out, depth);
                out.push(b);
                i += 1;
            }
            b',' => {
                out.push(b',');
                write_indent(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.push(b':');
                out.push(b' ');
                i += 1;
            }
            b'"' => {
                copy_string(input, &mut i, &mut out);
            }
            _ => {
                while i < n {
                    let c = input[i];
                    if matches!(
                        c,
                        b' ' | b'\t'
                            | b'\n'
                            | b'\r'
                            | b','
                            | b'}'
                            | b']'
                            | b':'
                            | b'"'
                            | b'{'
                            | b'['
                    ) {
                        break;
                    }
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
    out
}

fn write_indent(out: &mut Vec<u8>, depth: usize) {
    out.push(b'\n');
    for _ in 0..depth {
        out.extend_from_slice(INDENT);
    }
}

fn copy_string(input: &[u8], pos: &mut usize, out: &mut Vec<u8>) {
    out.push(input[*pos]);
    *pos += 1;
    while *pos < input.len() {
        let c = input[*pos];
        out.push(c);
        *pos += 1;
        if c == b'\\' && *pos < input.len() {
            out.push(input[*pos]);
            *pos += 1;
        } else if c == b'"' {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset_of(src: &[u8], byte: u8) -> Offset {
        src.iter().position(|&b| b == byte).unwrap() as Offset
    }

    #[test]
    fn empty_object() {
        let out = parse(b"{}", None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn empty_array() {
        let out = parse(b"[]", None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn scalar_document() {
        let out = parse(b"42", None);
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn nested_path_lookup() {
        let src = br#"{"a":{"b":[1,2,3]}}"#;
        let out = parse(src, None);
        assert!(out.error.is_none(), "error: {:?}", out.error);

        let pos = offset_of(src, b'2');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path.len(), 3);
        assert!(matches!(path[2], PathSegment::Index(1)));

        let jq = path_expression(&path, &out.names);
        assert_eq!(jq, ".a.b[1]");
    }

    #[test]
    fn weird_key_needs_brackets() {
        let src = br#"{"has space":1}"#;
        let out = parse(src, None);
        assert!(out.error.is_none());
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), r#".["has space"]"#);
    }

    #[test]
    fn leading_index_prepends_dot() {
        let src = b"[10,20,30]";
        let out = parse(src, None);
        let pos = offset_of(src, b'2');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), ".[1]");
    }

    #[test]
    fn root_lookup_empty_path() {
        let src = b"   {   }   ";
        let out = parse(src, None);
        let entry = out.paths.lookup(0).unwrap();
        let path = out.paths.path_of(entry);
        assert!(path.is_empty());
        assert_eq!(path_expression(&path, &out.names), ".");
    }

    #[test]
    fn line_index_simple() {
        let src = b"{\n  \"a\": 1\n}";
        let out = parse(src, None);
        assert_eq!(out.line_starts, vec![0, 2, 11]);
    }

    #[test]
    fn unicode_escape_in_key() {
        // {"é":1}  → key "é"
        let src = br#"{"\u00e9":1}"#;
        let out = parse(src, None);
        assert!(out.error.is_none(), "error: {:?}", out.error);
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), r#".["é"]"#);
    }

    #[test]
    fn surrogate_pair_in_key() {
        // U+1F600 GRINNING FACE = 😀
        let src = br#"{"\uD83D\uDE00":1}"#;
        let out = parse(src, None);
        assert!(out.error.is_none(), "error: {:?}", out.error);
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), r#".["😀"]"#);
    }

    #[test]
    fn styles_cover_tokens() {
        let src = br#"{"a":1,"b":"x","c":true,"d":null}"#;
        let out = parse(src, None);
        assert!(out.error.is_none());
        let counts = |k: StyleKind| out.styles.iter().filter(|s| s.kind == k).count();
        assert_eq!(counts(StyleKind::Key), 4);
        assert_eq!(counts(StyleKind::Number), 1);
        assert_eq!(counts(StyleKind::String), 1);
        assert_eq!(counts(StyleKind::Bool), 1);
        assert_eq!(counts(StyleKind::Null), 1);
    }

    #[test]
    fn lookup_inside_key_returns_field_path() {
        let src = br#"{"a":1}"#;
        let out = parse(src, None);
        let pos = offset_of(src, b'a');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), ".a");
    }

    fn value_bytes_at(src: &[u8], offset: Offset) -> &[u8] {
        let out = parse(src, None);
        let entry_idx = out.paths.lookup(offset).expect("offset has a path entry");
        let entry = out.paths.entries[entry_idx as usize];
        value_bytes_for_entry(src, &entry)
    }

    #[test]
    fn value_bytes_for_root_is_whole_doc_trimmed() {
        let src = b"  { \"a\": 1 }  ";
        assert_eq!(value_bytes_at(src, 0), b"{ \"a\": 1 }");
    }

    #[test]
    fn value_bytes_for_object_field_skips_key() {
        let src = br#"{"a":{"b":[1,2,3]}}"#;
        let pos = offset_of(src, b'b');
        assert_eq!(value_bytes_at(src, pos), b"[1,2,3]");
    }

    #[test]
    fn value_bytes_for_array_element() {
        let src = b"[10,20,30]";
        let pos = offset_of(src, b'2');
        assert_eq!(value_bytes_at(src, pos), b"20");
    }

    #[test]
    fn value_bytes_for_nested_object_value() {
        let src = br#"{"a": {"b": 1}, "c": 2}"#;
        let pos = offset_of(src, b'a');
        assert_eq!(value_bytes_at(src, pos), b"{\"b\": 1}");
    }

    #[test]
    fn value_bytes_for_pretty_field_keeps_value() {
        let src = b"{\n  \"a\": 42\n}";
        let pos = offset_of(src, b'a');
        assert_eq!(value_bytes_at(src, pos), b"42");
    }

    #[test]
    fn lenient_on_trailing_garbage() {
        let out = parse(b"{}  garbage", None);
        assert!(out.error.is_none());
    }

    // --- prettifier ---

    fn run_pretty(src: &[u8]) -> String {
        String::from_utf8(prettify(src)).unwrap()
    }

    #[test]
    fn pretty_empty_containers_stay_inline() {
        assert_eq!(run_pretty(b"{}"), "{}");
        assert_eq!(run_pretty(b"[]"), "[]");
        assert_eq!(run_pretty(b"{  }"), "{}");
        assert_eq!(run_pretty(b"[ \n ]"), "[]");
    }

    #[test]
    fn pretty_object_with_one_field() {
        assert_eq!(run_pretty(br#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn pretty_nested_object() {
        let got = run_pretty(br#"{"a":{"b":1}}"#);
        assert_eq!(got, "{\n  \"a\": {\n    \"b\": 1\n  }\n}");
    }

    #[test]
    fn pretty_array_with_items() {
        let got = run_pretty(b"[1,2,3]");
        assert_eq!(got, "[\n  1,\n  2,\n  3\n]");
    }

    #[test]
    fn pretty_mixed_nesting() {
        let got = run_pretty(br#"{"a":1,"b":[1,2,{"c":3}]}"#);
        let expected =
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    {\n      \"c\": 3\n    }\n  ]\n}";
        assert_eq!(got, expected);
    }

    #[test]
    fn pretty_strings_are_copied_verbatim_including_escapes() {
        let got = run_pretty(br#"{"k":"hello\"world\\x"}"#);
        assert_eq!(got, "{\n  \"k\": \"hello\\\"world\\\\x\"\n}");
    }

    #[test]
    fn pretty_input_whitespace_is_collapsed() {
        let got = run_pretty(b"{\n  \"a\"  :   1  }");
        assert_eq!(got, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn pretty_already_pretty_stays_pretty() {
        let input = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        let got = run_pretty(input.as_bytes());
        assert_eq!(got, input);
    }

    #[test]
    fn pretty_literals_and_numbers() {
        let got = run_pretty(b"[true,false,null,-1.5e10]");
        assert_eq!(got, "[\n  true,\n  false,\n  null,\n  -1.5e10\n]");
    }

    #[test]
    fn pretty_empty_nested() {
        let got = run_pretty(br#"{"a":{},"b":[]}"#);
        assert_eq!(got, "{\n  \"a\": {},\n  \"b\": []\n}");
    }

    #[test]
    #[ignore]
    fn bench_parse_synthetic() {
        let mut src = String::with_capacity(128 * 1024 * 1024);
        src.push('[');
        for i in 0..500_000 {
            if i > 0 {
                src.push(',');
            }
            src.push_str(&format!(
                "{{\"id\":{},\"name\":\"row-{}\",\"value\":{}.{},\"active\":{},\"tags\":[\"a\",\"b\",\"c\"]}}",
                i,
                i,
                i,
                i % 1000,
                i % 2 == 0
            ));
        }
        src.push(']');
        let bytes = src.as_bytes();
        let size_mb = bytes.len() as f64 / (1024.0 * 1024.0);

        let t0 = std::time::Instant::now();
        let out = parse(bytes, None);
        let dt = t0.elapsed();

        assert!(out.error.is_none(), "parse error: {:?}", out.error);
        let mbps = size_mb / dt.as_secs_f64();
        eprintln!(
            "parsed {:.1} MB in {:?} → {:.0} MB/s, entries={}, lines={}, styles={}",
            size_mb,
            dt,
            mbps,
            out.paths.entries.len(),
            out.line_starts.len(),
            out.styles.len(),
        );
    }
}
