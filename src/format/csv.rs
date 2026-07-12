//! CSV/TSV tokenizer + xsv-style path formatter + table-layout metadata.
//!
//! Unlike JSON/XML, CSV renders as an aligned table computed at *draw*
//! time: the parser records per-column content widths (capped at
//! `MAX_COL_CHARS`) and per-cell byte ranges (ordinary `PathEntry`s),
//! and `DocView` lays each field out at its column origin when
//! painting. The raw bytes are never copied or padded — the document
//! stays mmapped, and truncation past the column cap is visual only,
//! so Copy always yields the full field.
//!
//! One record per display line: `line_starts` holds *record* starts,
//! so a quoted field with an embedded newline does not split its row.
//! Embedded control characters are substituted with picture glyphs at
//! draw time (`display_char`), one char per byte, which keeps the
//! parser's width accounting and the renderer's layout in agreement.
//!
//! The first record is always treated as a header row. Duplicate or
//! empty header names fall back to 1-based positional selectors, which
//! is also what `xsv select` needs to address them unambiguously.

use super::{
    NameInterner, Offset, PROGRESS_GRANULARITY, ParseOutput, PathEntry, PathIndex, PathSegment,
    ProgressSink, ROOT_PARENT, StyleKind,
};
use std::sync::atomic::Ordering;

/// Widest a column may render, in characters. Longer fields draw
/// `MAX_COL_CHARS - 1` chars plus `…`; the bytes underneath are intact.
pub const MAX_COL_CHARS: u32 = 64;
/// Blank chars between columns.
pub const GUTTER_CHARS: u32 = 2;

/// Table layout computed by the parser: per-column display widths and
/// the char offset each column starts at. Everything is in character
/// columns (× the view's monospace advance = pixels).
#[derive(Debug)]
pub struct CsvMeta {
    /// Per-column display width in chars: max content width, capped.
    pub col_widths: Vec<u32>,
    /// Char column each column starts at (prefix sums incl. gutters).
    pub col_origins: Vec<u32>,
    /// Total table width in chars.
    pub table_width: u32,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Term {
    Delim,
    Newline,
    Eof,
}

struct Parser<'a> {
    input: &'a [u8],
    delimiter: u8,
    pos: usize,
    line_starts: Vec<Offset>,
    paths: Vec<PathEntry>,
    names: NameInterner,
    /// Interned select-name id per column (header name, or 1-based
    /// position for duplicate/empty/missing headers).
    col_keys: Vec<u32>,
    /// Max content chars seen per column (uncapped).
    col_chars: Vec<u32>,
    scratch: Vec<u8>,
    progress: Option<&'a ProgressSink>,
    next_progress_at: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8], delimiter: u8, progress: Option<&'a ProgressSink>) -> Self {
        let next_progress_at = if progress.is_some() {
            PROGRESS_GRANULARITY
        } else {
            usize::MAX
        };
        Self {
            input,
            delimiter,
            pos: 0,
            line_starts: vec![0],
            paths: Vec::new(),
            names: NameInterner::default(),
            col_keys: Vec::new(),
            col_chars: Vec::new(),
            scratch: Vec::with_capacity(64),
            progress,
            next_progress_at,
        }
    }

    /// Cold path: publish current `pos` to the progress sink and bump
    /// the next threshold. `#[cold]` keeps the per-field path tight.
    #[cold]
    #[inline(never)]
    fn flush_progress(&mut self) {
        if let Some(p) = self.progress {
            p.bytes_done.store(self.pos as u64, Ordering::Relaxed);
        }
        self.next_progress_at = self.pos + PROGRESS_GRANULARITY;
    }

    fn parse_document(&mut self) {
        // Root spans the whole input so a click anywhere resolves.
        self.paths.push(PathEntry {
            start: 0,
            end: self.input.len() as u32,
            parent: ROOT_PARENT,
            segment: PathSegment::Root,
        });
        if self.input.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.pos = 3;
        }
        let mut record: u32 = 0;
        while self.pos < self.input.len() {
            self.parse_record(record);
            record += 1;
        }
    }

    fn parse_record(&mut self, record: u32) {
        let record_start = self.pos as u32;
        // Data rows get a PathEntry so a click in the gutter still
        // resolves to `xsv slice -i N`. The header row's parent is the
        // root directly.
        let row_idx = if record > 0 {
            let idx = self.paths.len() as u32;
            self.paths.push(PathEntry {
                start: record_start,
                end: record_start,
                parent: 0,
                segment: PathSegment::Index(record - 1),
            });
            Some(idx)
        } else {
            None
        };

        let mut col = 0usize;
        let mut record_end;
        loop {
            if self.pos >= self.next_progress_at {
                self.flush_progress();
            }
            let field_start = self.pos;
            let (mut field_end, term) = self.scan_field();
            // Strip the \r of a \r\n line ending off the last field.
            if term != Term::Delim
                && field_end > field_start
                && self.input[field_end - 1] == b'\r'
            {
                field_end -= 1;
            }
            self.note_field(record, row_idx, col, field_start as u32, field_end as u32);
            record_end = field_end as u32;
            col += 1;
            match term {
                Term::Delim => self.pos += 1,
                Term::Newline => {
                    self.pos += 1;
                    self.line_starts.push(self.pos as u32);
                    break;
                }
                Term::Eof => break,
            }
        }
        if let Some(idx) = row_idx {
            self.paths[idx as usize].end = record_end;
        }
    }

    /// Consume one field starting at `self.pos`, leaving `pos` on the
    /// terminator (delimiter / newline) or at EOF. Returns the byte
    /// offset one past the field's raw content and what ended it.
    /// Quoted fields ("" = escaped quote) may contain delimiters and
    /// newlines; an unterminated quote runs to EOF.
    fn scan_field(&mut self) -> (usize, Term) {
        let input = self.input;
        let n = input.len();
        if self.pos < n && input[self.pos] == b'"' {
            self.pos += 1;
            while self.pos < n {
                if input[self.pos] == b'"' {
                    if self.pos + 1 < n && input[self.pos + 1] == b'"' {
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                } else {
                    self.pos += 1;
                }
            }
        }
        // Unquoted remainder (the whole field when it didn't start with
        // a quote; trailing junk after a closing quote otherwise).
        while self.pos < n {
            let b = input[self.pos];
            if b == self.delimiter {
                return (self.pos, Term::Delim);
            }
            if b == b'\n' {
                return (self.pos, Term::Newline);
            }
            self.pos += 1;
        }
        (self.pos, Term::Eof)
    }

    fn note_field(&mut self, record: u32, row_idx: Option<u32>, col: usize, start: u32, end: u32) {
        let chars = char_count(&self.input[start as usize..end as usize]);
        if col >= self.col_chars.len() {
            self.col_chars.push(0);
        }
        if chars > self.col_chars[col] {
            self.col_chars[col] = chars;
        }

        if record == 0 {
            decode_field(&self.input[start as usize..end as usize], &mut self.scratch);
            let key = if self.scratch.is_empty() {
                self.names.intern(format!("{}", col + 1).as_bytes())
            } else {
                let id = self.names.intern(&self.scratch);
                if self.col_keys.contains(&id) {
                    // Duplicate header — xsv resolves the name to the
                    // first occurrence, so later ones go positional.
                    self.names.intern(format!("{}", col + 1).as_bytes())
                } else {
                    id
                }
            };
            self.col_keys.push(key);
            self.paths.push(PathEntry {
                start,
                end,
                parent: 0,
                segment: PathSegment::Key(key),
            });
        } else {
            while self.col_keys.len() <= col {
                let key = self
                    .names
                    .intern(format!("{}", self.col_keys.len() + 1).as_bytes());
                self.col_keys.push(key);
            }
            self.paths.push(PathEntry {
                start,
                end,
                parent: row_idx.expect("data rows always have a row entry"),
                segment: PathSegment::Key(self.col_keys[col]),
            });
        }
    }

    fn finish(self) -> ParseOutput {
        let col_widths: Vec<u32> = self
            .col_chars
            .iter()
            .map(|&c| c.clamp(1, MAX_COL_CHARS))
            .collect();
        let mut col_origins = Vec::with_capacity(col_widths.len());
        let mut acc = 0u32;
        for &w in &col_widths {
            col_origins.push(acc);
            acc += w + GUTTER_CHARS;
        }
        let table_width = acc.saturating_sub(GUTTER_CHARS);
        ParseOutput {
            line_starts: self.line_starts,
            paths: PathIndex {
                entries: self.paths,
            },
            // Colors are computed at draw time from the cell entries
            // (header row / numeric fields), so no spans — this keeps
            // index memory at one PathEntry per cell.
            styles: Vec::new(),
            names: self.names,
            error: None,
            bytes: self.input.len(),
            csv: Some(CsvMeta {
                col_widths,
                col_origins,
                table_width,
            }),
        }
    }
}

/// Unquote a field for interning: strip surrounding quotes, collapse
/// `""` escapes. Unquoted fields copy through as-is.
fn decode_field(raw: &[u8], out: &mut Vec<u8>) {
    out.clear();
    if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        let inner = &raw[1..raw.len() - 1];
        let mut i = 0;
        while i < inner.len() {
            if inner[i] == b'"' && i + 1 < inner.len() && inner[i + 1] == b'"' {
                out.push(b'"');
                i += 2;
            } else {
                out.push(inner[i]);
                i += 1;
            }
        }
    } else {
        out.extend_from_slice(raw);
    }
}

pub fn parse(input: &[u8], delimiter: u8, progress: Option<&ProgressSink>) -> ParseOutput {
    let mut p = Parser::new(input, delimiter, progress);
    p.parse_document();
    if let Some(sink) = progress {
        sink.bytes_done.store(input.len() as u64, Ordering::Relaxed);
    }
    p.finish()
}

// --- layout helpers (used by DocView at draw time) --------------------

/// Count characters the renderer will draw for `bytes`: one per
/// non-continuation byte. Matches `display_char` substitution (one
/// glyph per control byte) and `char` iteration for valid UTF-8.
pub fn char_count(bytes: &[u8]) -> u32 {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u32
}

/// Byte offset of the `n`th character boundary in `bytes` (clamped).
fn byte_of_char(bytes: &[u8], n: u32) -> u32 {
    let mut seen = 0u32;
    for (i, &b) in bytes.iter().enumerate() {
        if b & 0xC0 != 0x80 {
            if seen == n {
                return i as u32;
            }
            seen += 1;
        }
    }
    bytes.len() as u32
}

/// Absolute byte ranges of the cells of the record starting at
/// `record_start`, in column order. `next_start` is the following
/// record's start offset, or `u32::MAX` for the last record.
pub fn record_cells(entries: &[PathEntry], record_start: u32, next_start: u32) -> Vec<(u32, u32)> {
    let lo = entries.partition_point(|e| e.start < record_start);
    let mut out = Vec::new();
    for e in &entries[lo..] {
        if e.start >= next_start {
            break;
        }
        if matches!(e.segment, PathSegment::Key(_)) {
            out.push((e.start, e.end));
        }
    }
    out
}

/// Visual char column at which absolute `byte` renders on its row.
/// Bytes inside a cell map to origin + chars-into-field (clamped to
/// the column width); delimiter/gutter bytes collapse to the end of
/// the preceding cell's content.
pub fn visual_col_of_byte(meta: &CsvMeta, cells: &[(u32, u32)], bytes: &[u8], byte: u32) -> u32 {
    if cells.is_empty() {
        return 0;
    }
    let i = cells
        .partition_point(|c| c.0 <= byte)
        .saturating_sub(1)
        .min(cells.len() - 1);
    let (s, e) = cells[i];
    let origin = meta.col_origins.get(i).copied().unwrap_or(0);
    let w = meta.col_widths.get(i).copied().unwrap_or(0);
    let upto = byte.clamp(s, e);
    origin + char_count(&bytes[s as usize..upto as usize]).min(w)
}

/// Absolute byte offset for a click at visual char column `col`.
/// Clicks in a gutter clamp to the end of the preceding cell (so path
/// lookup resolves to the row); clicks past a row's last cell clamp to
/// its end. Returns `None` when the record has no cells.
pub fn byte_of_visual_col(
    meta: &CsvMeta,
    cells: &[(u32, u32)],
    bytes: &[u8],
    col: u32,
) -> Option<u32> {
    if cells.is_empty() {
        return None;
    }
    let j = meta
        .col_origins
        .partition_point(|&o| o <= col)
        .saturating_sub(1)
        .min(cells.len() - 1);
    let (s, e) = cells[j];
    let origin = meta.col_origins.get(j).copied().unwrap_or(0);
    let w = meta.col_widths.get(j).copied().unwrap_or(0);
    let rel = col.saturating_sub(origin).min(w);
    let field = &bytes[s as usize..e as usize];
    Some(s + byte_of_char(field, rel))
}

/// Substitution for control bytes so embedded newlines/tabs render as
/// one picture glyph instead of breaking the row layout. Also used by
/// DocView's raw mode: a CSV "line" is a record, so an embedded
/// newline would otherwise wrap and overdraw the next row.
pub fn display_char(c: char) -> char {
    match c {
        '\n' => '␤',
        '\r' => '␍',
        '\t' => '␉',
        c if (c as u32) < 0x20 => '␦',
        c => c,
    }
}

/// One row's drawable text: fields padded to their column origins,
/// truncated at the column width with `…`. `origin_chars` is the char
/// column `text` starts at (the origin of the first visible cell);
/// `spans` are UTF-16 ranges into `text` to colorize.
pub struct RenderedRow {
    pub origin_chars: u32,
    pub text: String,
    pub spans: Vec<(usize, usize, StyleKind)>,
}

/// Compose the drawable text for one record, limited to cells that
/// intersect the visible char-column range.
pub fn render_row(
    meta: &CsvMeta,
    cells: &[(u32, u32)],
    bytes: &[u8],
    is_header: bool,
    vis_col_start: u32,
    vis_col_end: u32,
) -> RenderedRow {
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut base: Option<u32> = None;
    let mut chars = 0u32;
    let mut u16s = 0usize;

    for (i, &(s, e)) in cells.iter().enumerate() {
        let Some(&origin) = meta.col_origins.get(i) else {
            break;
        };
        let w = meta.col_widths.get(i).copied().unwrap_or(0);
        if origin + w < vis_col_start {
            continue;
        }
        if origin > vis_col_end {
            break;
        }
        let base = *base.get_or_insert(origin);

        let target = origin - base;
        while chars < target {
            text.push(' ');
            chars += 1;
            u16s += 1;
        }

        let raw = &bytes[s as usize..e as usize];
        let field = String::from_utf8_lossy(raw);
        let span_start = u16s;
        let mut it = field.chars();
        let mut emitted = 0u32;
        while emitted < w {
            let Some(c) = it.next() else { break };
            let d = display_char(c);
            text.push(d);
            u16s += d.len_utf16();
            chars += 1;
            emitted += 1;
        }
        if w > 0 && it.next().is_some() {
            // Over the cap — swap the last glyph for an ellipsis.
            let popped = text.pop().expect("emitted at least one char");
            u16s -= popped.len_utf16();
            text.push('…');
            u16s += 1;
        }

        let kind = if is_header {
            Some(StyleKind::Key)
        } else if looks_numeric(raw) {
            Some(StyleKind::Number)
        } else {
            None
        };
        if let Some(k) = kind {
            if u16s > span_start {
                spans.push((span_start, u16s, k));
            }
        }
    }

    RenderedRow {
        origin_chars: base.unwrap_or(0),
        text,
        spans,
    }
}

/// Loose number shape check for draw-time coloring: optional sign,
/// digits, one optional dot, optional exponent. Surrounding ASCII
/// whitespace is ignored.
pub fn looks_numeric(field: &[u8]) -> bool {
    let t = field.trim_ascii();
    if t.is_empty() {
        return false;
    }
    let mut i = 0;
    if t[0] == b'+' || t[0] == b'-' {
        i = 1;
    }
    let mut digits = false;
    let mut dot = false;
    while i < t.len() {
        match t[i] {
            b'0'..=b'9' => digits = true,
            b'.' if !dot => dot = true,
            b'e' | b'E' if digits => {
                i += 1;
                if i < t.len() && (t[i] == b'+' || t[i] == b'-') {
                    i += 1;
                }
                if i >= t.len() {
                    return false;
                }
                while i < t.len() {
                    if !t[i].is_ascii_digit() {
                        return false;
                    }
                    i += 1;
                }
                return true;
            }
            _ => return false,
        }
        i += 1;
    }
    digits
}

// --- path expression ---------------------------------------------------

/// Render a path as an xsv pipeline: cell → `xsv slice -i R | xsv
/// select C`, row → `xsv slice -i R`, header cell → `xsv select C`,
/// root → `xsv table` (the whole-document view).
pub fn path_expression(segments: &[PathSegment], names: &NameInterner) -> String {
    let mut row: Option<u32> = None;
    let mut col: Option<u32> = None;
    for seg in segments {
        match seg {
            PathSegment::Index(i) => row = Some(*i),
            PathSegment::Key(k) => col = Some(*k),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if let Some(r) = row {
        parts.push(format!("xsv slice -i {}", r));
    }
    if let Some(c) = col {
        parts.push(format!("xsv select {}", select_name(names.get(c))));
    }
    if parts.is_empty() {
        return "xsv table".to_string();
    }
    parts.join(" | ")
}

/// Quote a column name for `xsv select` unless it's a bare identifier
/// or a positional index.
fn select_name(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let all_digits = !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let identifier = s
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if all_digits || identifier {
        s.into_owned()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Raw bytes covered by `entry` — the exact CSV fragment: field bytes
/// (including any quoting) for a cell, the record for a row, the whole
/// input for the root.
pub fn value_bytes_for_entry<'a>(bytes: &'a [u8], entry: &PathEntry) -> &'a [u8] {
    let start = (entry.start as usize).min(bytes.len());
    let end = (entry.end as usize).min(bytes.len());
    if end <= start {
        return &[];
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_csv(src: &[u8]) -> ParseOutput {
        parse(src, b',', None)
    }

    fn expr_at(out: &ParseOutput, offset: u32) -> String {
        let entry = out.paths.lookup(offset).unwrap();
        let path = out.paths.path_of(entry);
        path_expression(&path, &out.names)
    }

    const SIMPLE: &[u8] = b"name,age,city\nalice,30,sf\nbob,7,\"new york, ny\"\n";

    #[test]
    fn line_starts_are_record_starts() {
        let out = parse_csv(SIMPLE);
        assert_eq!(out.line_starts, vec![0, 14, 26, 47]);
        assert!(out.error.is_none());
    }

    #[test]
    fn quoted_newline_does_not_split_record() {
        let src = b"a,b\n1,\"x\ny\"\n2,z\n";
        let out = parse_csv(src);
        // Header, row with embedded newline, row "2,z" — 3 records
        // (+ the phantom line after the trailing newline).
        assert_eq!(out.line_starts, vec![0, 4, 12, 16]);
    }

    #[test]
    fn widths_are_content_max_capped() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        // name/alice/bob → 5; age/30/7 → 3; city/sf/"new york, ny" → 14.
        assert_eq!(meta.col_widths, vec![5, 3, 14]);
        assert_eq!(meta.col_origins, vec![0, 7, 12]);
        assert_eq!(meta.table_width, 26);

        let long = format!("h\n{}\n", "x".repeat(200));
        let out = parse_csv(long.as_bytes());
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![MAX_COL_CHARS]);
    }

    #[test]
    fn cell_path_is_slice_and_select() {
        let out = parse_csv(SIMPLE);
        let pos = SIMPLE.windows(2).position(|w| w == b"sf").unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select city");
    }

    #[test]
    fn header_cell_path_is_select() {
        let out = parse_csv(SIMPLE);
        assert_eq!(expr_at(&out, 0), "xsv select name");
    }

    #[test]
    fn gutter_click_resolves_to_row() {
        let out = parse_csv(SIMPLE);
        // Offset of the comma after "alice" — the delimiter belongs to
        // no cell, so lookup walks up to the row ("alice" is the first
        // data row → index 0).
        let comma = 14 + 5;
        assert_eq!(expr_at(&out, comma), "xsv slice -i 0");
    }

    #[test]
    fn weird_header_names_are_quoted() {
        let src = b"first name,\"a,b\"\nx,y\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'x').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select \"first name\"");
        let pos = src.iter().position(|&b| b == b'y').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select \"a,b\"");
    }

    #[test]
    fn duplicate_and_empty_headers_go_positional() {
        let src = b"a,a,\n1,2,3\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'1').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select a");
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select 2");
        let pos = src.iter().position(|&b| b == b'3').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select 3");
    }

    #[test]
    fn ragged_row_gets_positional_columns() {
        let src = b"a\n1,2\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select 2");
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths.len(), 2);
    }

    #[test]
    fn root_path_is_table() {
        let out = parse_csv(b"");
        let entry = out.paths.lookup(0).unwrap();
        let path = out.paths.path_of(entry);
        assert_eq!(path_expression(&path, &out.names), "xsv table");
    }

    #[test]
    fn value_bytes_cell_row_root() {
        let out = parse_csv(SIMPLE);
        let bytes = SIMPLE;
        // Cell: quoted field comes back verbatim, quotes included.
        let pos = bytes.windows(4).position(|w| w == b"new ").unwrap() as u32;
        let entry_idx = out.paths.lookup(pos).unwrap();
        let entry = out.paths.entries[entry_idx as usize];
        assert_eq!(value_bytes_for_entry(bytes, &entry), b"\"new york, ny\"");
        // Row: whole record without the trailing newline.
        let row_idx = out.paths.entries[entry_idx as usize].parent;
        let row = out.paths.entries[row_idx as usize];
        assert_eq!(value_bytes_for_entry(bytes, &row), b"bob,7,\"new york, ny\"");
    }

    #[test]
    fn crlf_is_stripped_from_fields() {
        let src = b"a,b\r\n1,2\r\n";
        let out = parse_csv(src);
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![1, 1]);
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        let entry_idx = out.paths.lookup(pos).unwrap();
        let entry = out.paths.entries[entry_idx as usize];
        assert_eq!(value_bytes_for_entry(src, &entry), b"2");
    }

    #[test]
    fn tsv_delimiter() {
        let out = parse(b"a\tb\n1\t2\n", b'\t', None);
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![1, 1]);
        assert_eq!(expr_at(&out, 4), "xsv slice -i 0 | xsv select a");
    }

    #[test]
    fn bom_is_skipped_for_header_name() {
        let src = b"\xEF\xBB\xBFa,b\n1,2\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'1').unwrap() as u32;
        assert_eq!(expr_at(&out, pos), "xsv slice -i 0 | xsv select a");
    }

    // --- layout mapping ---

    fn cells_of_line(out: &ParseOutput, line: usize) -> Vec<(u32, u32)> {
        let start = out.line_starts[line];
        let next = out
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(u32::MAX);
        record_cells(&out.paths.entries, start, next)
    }

    #[test]
    fn record_cells_finds_row_fields() {
        let out = parse_csv(SIMPLE);
        let cells = cells_of_line(&out, 1);
        assert_eq!(cells.len(), 3);
        assert_eq!(&SIMPLE[cells[0].0 as usize..cells[0].1 as usize], b"alice");
        assert_eq!(&SIMPLE[cells[2].0 as usize..cells[2].1 as usize], b"sf");
    }

    #[test]
    fn visual_col_round_trip() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        // Byte of "30" (col 1, origin 7).
        let b30 = SIMPLE.windows(2).position(|w| w == b"30").unwrap() as u32;
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, b30), 7);
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, b30 + 1), 8);
        // Inverse: col 7 → byte of '3'; col 8 → byte of '0'.
        assert_eq!(byte_of_visual_col(meta, &cells, SIMPLE, 7), Some(b30));
        assert_eq!(byte_of_visual_col(meta, &cells, SIMPLE, 8), Some(b30 + 1));
        // Gutter after "alice" (cols 5..6) collapses to end of field.
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, 14 + 5), 5);
        let back = byte_of_visual_col(meta, &cells, SIMPLE, 6).unwrap();
        assert_eq!(back, 14 + 5);
    }

    #[test]
    fn visual_col_clamps_past_truncation() {
        let long = format!("h\n{}\n", "x".repeat(200));
        let out = parse_csv(long.as_bytes());
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        let end_byte = 2 + 200;
        assert_eq!(
            visual_col_of_byte(meta, &cells, long.as_bytes(), end_byte),
            MAX_COL_CHARS
        );
        let clicked = byte_of_visual_col(meta, &cells, long.as_bytes(), 500).unwrap();
        assert_eq!(clicked, 2 + MAX_COL_CHARS); // clamped to the cap boundary
    }

    // --- rendering ---

    #[test]
    fn render_row_pads_and_colors() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        let rr = render_row(meta, &cells, SIMPLE, false, 0, 1000);
        assert_eq!(rr.origin_chars, 0);
        assert_eq!(rr.text, "alice  30   sf");
        // "30" is numeric → one Number span at utf16 7..9.
        assert_eq!(rr.spans, vec![(7, 9, StyleKind::Number)]);

        let hdr = render_row(meta, &cells_of_line(&out, 0), SIMPLE, true, 0, 1000);
        assert_eq!(hdr.text, "name   age  city");
        assert!(hdr.spans.iter().all(|s| s.2 == StyleKind::Key));
    }

    #[test]
    fn render_row_truncates_with_ellipsis() {
        let long = format!("h,k\n{},z\n", "x".repeat(200));
        let out = parse_csv(long.as_bytes());
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        let rr = render_row(meta, &cells, long.as_bytes(), false, 0, 1000);
        let first: String = rr.text.chars().take(MAX_COL_CHARS as usize).collect();
        assert!(first.ends_with('…'));
        assert_eq!(first.chars().count(), MAX_COL_CHARS as usize);
        assert!(rr.text.ends_with('z'));
        // The z column starts exactly at its origin.
        assert_eq!(
            rr.text.chars().count() as u32,
            meta.col_origins[1] + 1
        );
    }

    #[test]
    fn render_row_skips_cells_left_of_viewport() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        // Viewport starts inside the "city" column (origin 12).
        let rr = render_row(meta, &cells, SIMPLE, false, 13, 1000);
        assert_eq!(rr.origin_chars, 12);
        assert_eq!(rr.text, "sf");
    }

    #[test]
    fn render_row_substitutes_embedded_newline() {
        let src = b"a,b\n1,\"x\ny\"\n";
        let out = parse_csv(src);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(&out, 1);
        let rr = render_row(meta, &cells, src, false, 0, 1000);
        assert!(rr.text.contains('␤'));
        assert!(!rr.text.contains('\n'));
    }

    #[test]
    fn looks_numeric_shapes() {
        assert!(looks_numeric(b"42"));
        assert!(looks_numeric(b"-1.5"));
        assert!(looks_numeric(b"+0.5e10"));
        assert!(looks_numeric(b" 7 "));
        assert!(!looks_numeric(b""));
        assert!(!looks_numeric(b"1.2.3"));
        assert!(!looks_numeric(b"12a"));
        assert!(!looks_numeric(b"-"));
        assert!(!looks_numeric(b"1e"));
    }

    #[test]
    fn empty_input_is_sane() {
        let out = parse_csv(b"");
        assert_eq!(out.line_starts, vec![0]);
        assert_eq!(out.paths.entries.len(), 1);
        let meta = out.csv.as_ref().unwrap();
        assert!(meta.col_widths.is_empty());
        assert_eq!(meta.table_width, 0);
    }

    #[test]
    #[ignore]
    fn bench_parse_synthetic() {
        // ~10-col rows mimicking a log export; run with
        // cargo test -p rapid-view --release -- --ignored --nocapture
        let mut src = String::with_capacity(256 * 1024 * 1024);
        src.push_str("id,ts,user,event,dur_ms,host,region,status,bytes,note\n");
        for i in 0..2_000_000u64 {
            use std::fmt::Write;
            let _ = writeln!(
                src,
                "{i},2026-07-12T10:{:02}:{:02}Z,user{},click_{},{}.{},host-{}.internal,us-west-{},{},{},\"note {i}, quoted\"",
                i / 60 % 60, i % 60, i % 10_000, i % 37, i % 900, i % 10,
                i % 64, i % 4, 200 + (i % 5) as u32, i % 100_000,
            );
        }
        let bytes = src.as_bytes();
        let size_mb = bytes.len() as f64 / (1024.0 * 1024.0);

        let t0 = std::time::Instant::now();
        let out = parse(bytes, b',', None);
        let dt = t0.elapsed();

        let entries = out.paths.entries.len();
        eprintln!(
            "parsed {:.1} MB in {:?} → {:.0} MB/s, entries={} ({:.1} MB index), lines={}",
            size_mb,
            dt,
            size_mb / dt.as_secs_f64(),
            entries,
            (entries * std::mem::size_of::<PathEntry>()) as f64 / (1024.0 * 1024.0),
            out.line_starts.len(),
        );
    }

    #[test]
    fn unterminated_quote_runs_to_eof() {
        let src = b"a,b\n1,\"oops\n2,3\n";
        let out = parse_csv(src);
        // The unterminated quote swallows the rest — 2 records total.
        assert_eq!(out.line_starts.len(), 2);
        assert!(out.error.is_none());
    }
}
