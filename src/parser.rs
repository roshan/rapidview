//! Hand-rolled streaming JSON tokenizer.
//!
//! Walks `&[u8]` once and produces the three indexes the UI needs:
//!
//! * `line_starts` — byte offsets of each line start, for `drawRect:` to
//!   map pixel Y back to a byte range.
//! * `PathIndex`   — sorted list of nested value ranges tagged with a path
//!   segment, for click → JSON path lookup.
//! * `styles`      — non-overlapping style ranges for syntax colouring.
//!
//! Lenient: on parse error the partial indexes are still returned, so the
//! viewer can display broken files up to the point they go wrong.


use std::collections::HashMap;

pub type Offset = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Key,
    String,
    Number,
    Bool,
    Null,
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
    Key(u32),
    Index(u32),
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
pub struct KeyInterner {
    map: HashMap<Box<[u8]>, u32>,
    buf: Vec<u8>,
    ranges: Vec<(u32, u32)>,
}

impl KeyInterner {
    pub fn intern(&mut self, key: &[u8]) -> u32 {
        if let Some(&id) = self.map.get(key) {
            return id;
        }
        let start = self.buf.len() as u32;
        self.buf.extend_from_slice(key);
        let len = key.len() as u32;
        let id = self.ranges.len() as u32;
        self.ranges.push((start, len));
        self.map.insert(key.to_vec().into_boxed_slice(), id);
        id
    }

    pub fn get(&self, id: u32) -> &[u8] {
        let (start, len) = self.ranges[id as usize];
        &self.buf[start as usize..(start + len) as usize]
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

#[derive(Debug, Default)]
pub struct PathIndex {
    pub entries: Vec<PathEntry>,
}

impl PathIndex {
    /// Innermost entry whose range contains `offset`. Walks up via `parent`
    /// if the nearest-by-start sibling has already ended — proper nesting
    /// guarantees the parent then contains it, so this is O(depth).
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

    /// Segments from root (exclusive) to `entry` (inclusive).
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

#[derive(Debug, Default)]
pub struct ParseOutput {
    pub line_starts: Vec<Offset>,
    pub paths: PathIndex,
    pub styles: Vec<StyleSpan>,
    pub keys: KeyInterner,
    #[allow(dead_code)] // inspected in tests and by callers
    pub error: Option<ParseError>,
    #[allow(dead_code)]
    pub bytes: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields read via Debug formatting in error paths
pub struct ParseError {
    pub offset: Offset,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // variants constructed by parser, read via Debug
pub enum ParseErrorKind {
    UnexpectedByte(u8),
    UnexpectedEof,
    InvalidEscape,
    InvalidNumber,
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line_starts: Vec<Offset>,
    paths: Vec<PathEntry>,
    styles: Vec<StyleSpan>,
    keys: KeyInterner,
    scratch: Vec<u8>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            line_starts: vec![0],
            paths: Vec::new(),
            styles: Vec::new(),
            keys: KeyInterner::default(),
            scratch: Vec::with_capacity(64),
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
        Some(b)
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
            let key_id = self.keys.intern(&self.scratch);

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

    /// Consume opening `"` through matching `"`. Returns the byte range of
    /// the raw (still-escaped) contents, exclusive of quotes.
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
            keys: self.keys,
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
                    // High surrogate — look for paired low surrogate.
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

pub fn parse(input: &[u8]) -> ParseOutput {
    let mut p = Parser::new(input);
    let err = p.parse_document().err();
    p.finish(err)
}

/// Render a path as a jq expression. Non-identifier keys are emitted as
/// `["..."]`; array indices as `[N]`. Empty path → ".".
pub fn jq_path(segments: &[PathSegment], keys: &KeyInterner) -> String {
    if segments.is_empty() {
        return ".".to_string();
    }
    let mut out = String::new();
    for seg in segments {
        match seg {
            PathSegment::Root => {}
            PathSegment::Key(id) => {
                let bytes = keys.get(*id);
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
        }
    }
    // When the first segment is an index, `out` starts with `[` — jq wants
    // `.[0]`, so prepend a dot in that case.
    if out.starts_with('[') {
        let mut tmp = String::with_capacity(out.len() + 1);
        tmp.push('.');
        tmp.push_str(&out);
        out = tmp;
    }
    out
}

/// Bytes of the JSON *value* covered by `entry`. For object fields the
/// entry's raw range covers `"key": value`; this skips the key + colon so
/// the result is a standalone JSON value. For array elements and the root
/// the raw range is already a value, modulo leading/trailing whitespace.
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
    }
}

/// Walk past `"key" : ` at the start of `slice` and return the rest.
/// Defensive: if the shape doesn't match (malformed input), returns the
/// slice unchanged so the user sees something rather than nothing.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn offset_of(src: &[u8], byte: u8) -> Offset {
        src.iter().position(|&b| b == byte).unwrap() as Offset
    }

    #[test]
    fn empty_object() {
        let out = parse(b"{}");
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn empty_array() {
        let out = parse(b"[]");
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn scalar_document() {
        let out = parse(b"42");
        assert!(out.error.is_none());
        assert_eq!(out.paths.entries.len(), 1);
    }

    #[test]
    fn nested_path_lookup() {
        let src = br#"{"a":{"b":[1,2,3]}}"#;
        let out = parse(src);
        assert!(out.error.is_none(), "error: {:?}", out.error);

        let pos = offset_of(src, b'2');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path.len(), 3);
        assert!(matches!(path[2], PathSegment::Index(1)));

        let jq = jq_path(&path, &out.keys);
        assert_eq!(jq, ".a.b[1]");
    }

    #[test]
    fn weird_key_needs_brackets() {
        let src = br#"{"has space":1}"#;
        let out = parse(src);
        assert!(out.error.is_none());
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(jq_path(&path, &out.keys), r#".["has space"]"#);
    }

    #[test]
    fn leading_index_prepends_dot() {
        let src = b"[10,20,30]";
        let out = parse(src);
        let pos = offset_of(src, b'2'); // hits the '2' in 20
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(jq_path(&path, &out.keys), ".[1]");
    }

    #[test]
    fn root_lookup_empty_path() {
        let src = b"   {   }   ";
        let out = parse(src);
        // Click on the leading whitespace (offset 0)
        let entry = out.paths.lookup(0).unwrap();
        let path = out.paths.path_of(entry);
        assert!(path.is_empty());
        assert_eq!(jq_path(&path, &out.keys), ".");
    }

    #[test]
    fn line_index_simple() {
        let src = b"{\n  \"a\": 1\n}";
        let out = parse(src);
        assert_eq!(out.line_starts, vec![0, 2, 11]);
    }

    #[test]
    fn unicode_escape_in_key() {
        // {"\u00e9":1}  → key "é"
        let src = br#"{"\u00e9":1}"#;
        let out = parse(src);
        assert!(out.error.is_none(), "error: {:?}", out.error);
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(jq_path(&path, &out.keys), r#".["é"]"#);
    }

    #[test]
    fn surrogate_pair_in_key() {
        // U+1F600 GRINNING FACE = \uD83D\uDE00
        let src = br#"{"\uD83D\uDE00":1}"#;
        let out = parse(src);
        assert!(out.error.is_none(), "error: {:?}", out.error);
        let pos = offset_of(src, b'1');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(jq_path(&path, &out.keys), r#".["😀"]"#);
    }

    #[test]
    fn styles_cover_tokens() {
        let src = br#"{"a":1,"b":"x","c":true,"d":null}"#;
        let out = parse(src);
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
        // Clicking on the `a` character of the key should give [.a].
        let src = br#"{"a":1}"#;
        let out = parse(src);
        let pos = offset_of(src, b'a');
        let entry = out.paths.lookup(pos).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(jq_path(&path, &out.keys), ".a");
    }

    fn value_bytes_at(src: &[u8], offset: Offset) -> &[u8] {
        let out = parse(src);
        let entry_idx = out.paths.lookup(offset).expect("offset has a path entry");
        let entry = out.paths.entries[entry_idx as usize];
        value_bytes_for_entry(src, &entry)
    }

    #[test]
    fn value_bytes_for_root_is_whole_doc_trimmed() {
        let src = b"  { \"a\": 1 }  ";
        // Clicking on leading whitespace gives root.
        assert_eq!(value_bytes_at(src, 0), b"{ \"a\": 1 }");
    }

    #[test]
    fn value_bytes_for_object_field_skips_key() {
        let src = br#"{"a":{"b":[1,2,3]}}"#;
        let pos = offset_of(src, b'b'); // click on key "b"
        // Should give the value of "b", which is the array.
        assert_eq!(value_bytes_at(src, pos), b"[1,2,3]");
    }

    #[test]
    fn value_bytes_for_array_element() {
        let src = b"[10,20,30]";
        let pos = offset_of(src, b'2'); // hits the "2" in 20
        assert_eq!(value_bytes_at(src, pos), b"20");
    }

    #[test]
    fn value_bytes_for_nested_object_value() {
        let src = br#"{"a": {"b": 1}, "c": 2}"#;
        // Click on the "a" key — the value is the nested object.
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
        let out = parse(b"{}  garbage");
        // Document parses successfully; we stop at the end of the value.
        assert!(out.error.is_none());
    }

    #[test]
    #[ignore] // run with `cargo test --release -- --ignored bench_parse_synthetic --nocapture`
    fn bench_parse_synthetic() {
        // Generate ~100 MB of JSON: array of 500k objects.
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
        let out = parse(bytes);
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
