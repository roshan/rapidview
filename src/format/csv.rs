//! CSV/TSV indexer + xsv path formatter + table-layout metadata.
//!
//! Unlike JSON/XML, CSV renders as an aligned table computed at *draw*
//! time, and — since RAPIDVIEW-2 — keeps **no per-cell index**. The
//! indexer records only record starts (`line_starts`, 4 B/row) and
//! per-column display widths; everything cell-shaped (field ranges for
//! drawing, click→cell resolution, copy ranges) is re-derived on
//! demand by rescanning the one record involved (`scan_cells`,
//! `locate`). Records are short, and a frame only ever needs the ~60
//! visible rows, so the rescan is noise — while the index for a
//! multi-GB file shrinks from gigabytes to megabytes.
//!
//! Column widths come from the first `WIDTH_SAMPLE_BYTES` of the file;
//! after that the indexer freezes the layout and degrades to a fast
//! record-boundary scan. The 64-char cap means a sample that large is
//! effectively always representative.
//!
//! One record per display line: `line_starts` holds *record* starts,
//! so a quoted field with an embedded newline does not split its row.
//! Embedded control characters render as picture glyphs
//! (`display_char`), one char per byte, which keeps width accounting
//! and layout in agreement.
//!
//! The first record is always treated as a header row. Duplicate or
//! empty header names fall back to 1-based positional selectors, which
//! is also what `xsv select` needs to address them unambiguously.

use super::{
    Offset, PROGRESS_GRANULARITY, ParseOutput, PathEntry, PathIndex, PathSegment, ProgressSink,
    ROOT_PARENT, StyleKind,
};
use std::sync::atomic::Ordering;

/// Widest a column may render, in characters. Longer fields draw
/// `MAX_COL_CHARS - 1` chars plus `…`; the bytes underneath are intact.
pub const MAX_COL_CHARS: u32 = 64;
/// Blank chars between columns.
pub const GUTTER_CHARS: u32 = 2;
/// Column widths are computed from this prefix of the file, then
/// frozen so indexing the remainder is a pure record-boundary scan and
/// the layout never shifts under the user during a progressive load.
pub const WIDTH_SAMPLE_BYTES: usize = 16 << 20;

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Table layout computed by the indexer: per-column display widths,
/// the char offset each column starts at, and ready-to-emit `xsv
/// select` names. Everything is in character columns (× the view's
/// monospace advance = pixels).
#[derive(Debug)]
pub struct CsvMeta {
    pub delimiter: u8,
    /// Per-column display width in chars: max sampled content width, capped.
    pub col_widths: Vec<u32>,
    /// Char column each column starts at (prefix sums incl. gutters).
    pub col_origins: Vec<u32>,
    /// Total table width in chars.
    pub table_width: u32,
    /// `xsv select` selector per column — quoted header name, or a
    /// 1-based position for duplicate/empty/missing headers.
    pub col_selects: Vec<String>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Term {
    Delim,
    Newline,
    Eof,
}

/// Advance over one field starting at `pos`. Returns the byte offset
/// of the terminator (delimiter / unquoted newline / EOF) and which it
/// was. Quoted fields ("" = escaped quote) may contain delimiters and
/// newlines; an unterminated quote runs to EOF.
#[inline]
fn scan_one_field(bytes: &[u8], mut pos: usize, delimiter: u8) -> (usize, Term) {
    let n = bytes.len();
    if pos < n && bytes[pos] == b'"' {
        pos += 1;
        while pos < n {
            if bytes[pos] == b'"' {
                if pos + 1 < n && bytes[pos + 1] == b'"' {
                    pos += 2;
                } else {
                    pos += 1;
                    break;
                }
            } else {
                pos += 1;
            }
        }
    }
    // Unquoted remainder (the whole field when it didn't start with a
    // quote; trailing junk after a closing quote otherwise).
    while pos < n {
        let b = bytes[pos];
        if b == delimiter {
            return (pos, Term::Delim);
        }
        if b == b'\n' {
            return (pos, Term::Newline);
        }
        pos += 1;
    }
    (pos, Term::Eof)
}

/// Strip the `\r` of a `\r\n` line ending off a field's content range.
#[inline]
fn strip_cr(bytes: &[u8], start: usize, end: usize, term: Term) -> usize {
    if term != Term::Delim && end > start && bytes[end - 1] == b'\r' {
        end - 1
    } else {
        end
    }
}

/// Incremental CSV indexer. `scan` consumes input in budgeted slices
/// (always stopping on a record boundary) so the worker can publish
/// browsable snapshots of a huge file while the tail is still being
/// indexed; `parse` below drives it to completion in one call.
pub struct Indexer<'a> {
    input: &'a [u8],
    delimiter: u8,
    pos: usize,
    line_starts: Vec<Offset>,
    /// Max content chars seen per column (uncapped) — sample only.
    col_chars: Vec<u32>,
    col_selects: Vec<String>,
    header_done: bool,
    widths_frozen: bool,
    sample_limit: usize,
    scratch: Vec<u8>,
    progress: Option<&'a ProgressSink>,
    next_progress_at: usize,
}

impl<'a> Indexer<'a> {
    pub fn new(input: &'a [u8], delimiter: u8, progress: Option<&'a ProgressSink>) -> Self {
        let next_progress_at = if progress.is_some() {
            PROGRESS_GRANULARITY
        } else {
            usize::MAX
        };
        let mut pos = 0;
        if input.starts_with(BOM) {
            pos = 3;
        }
        Self {
            input,
            delimiter,
            pos,
            line_starts: vec![0],
            col_chars: Vec::new(),
            col_selects: Vec::new(),
            header_done: false,
            widths_frozen: false,
            sample_limit: WIDTH_SAMPLE_BYTES,
            scratch: Vec::with_capacity(64),
            progress,
            next_progress_at,
        }
    }

    /// Shrink the width sample (tests only — the default is 16 MB).
    #[cfg(test)]
    pub fn with_sample_limit(mut self, limit: usize) -> Self {
        self.sample_limit = limit;
        self
    }

    /// Index up to `budget` more bytes, stopping on a record boundary.
    /// Returns true once the whole input has been consumed.
    pub fn scan(&mut self, budget: usize) -> bool {
        let target = self.pos.saturating_add(budget);
        while self.pos < self.input.len() && self.pos < target {
            if self.pos >= self.next_progress_at {
                self.flush_progress();
            }
            if self.widths_frozen {
                self.skip_record();
            } else {
                self.index_record();
                if !self.widths_frozen && self.pos >= self.sample_limit {
                    self.widths_frozen = true;
                }
            }
        }
        self.pos >= self.input.len()
    }

    /// Cold path: publish current `pos` to the progress sink and bump
    /// the next threshold. `#[cold]` keeps the per-record path tight.
    #[cold]
    #[inline(never)]
    fn flush_progress(&mut self) {
        if let Some(p) = self.progress {
            p.bytes_done.store(self.pos as u64, Ordering::Relaxed);
        }
        self.next_progress_at = self.pos + PROGRESS_GRANULARITY;
    }

    /// Sample-phase record walk: exact field scan, width + header
    /// accounting per field.
    fn index_record(&mut self) {
        let is_header = !self.header_done;
        let mut col = 0usize;
        loop {
            let start = self.pos;
            let (stop, term) = scan_one_field(self.input, self.pos, self.delimiter);
            let end = strip_cr(self.input, start, stop, term);

            let chars = char_count(&self.input[start..end]);
            if col >= self.col_chars.len() {
                self.col_chars.push(0);
            }
            if chars > self.col_chars[col] {
                self.col_chars[col] = chars;
            }
            if is_header {
                self.note_header_name(start, end);
            } else {
                while self.col_selects.len() <= col {
                    self.col_selects.push((self.col_selects.len() + 1).to_string());
                }
            }

            col += 1;
            match term {
                Term::Delim => self.pos = stop + 1,
                Term::Newline => {
                    self.pos = stop + 1;
                    self.line_starts.push(self.pos as u32);
                    break;
                }
                Term::Eof => {
                    self.pos = stop;
                    break;
                }
            }
        }
        if is_header {
            self.header_done = true;
        }
    }

    /// Post-freeze fast path: find the next record boundary and nothing
    /// else. The bare quote toggle matches the field-aware scan on any
    /// RFC-quoted input ("" toggles twice = no net change); it can only
    /// disagree on pathological bare quotes mid-field, where a slightly
    /// misplaced row boundary is an acceptable trade for scan speed.
    fn skip_record(&mut self) {
        let bytes = self.input;
        let n = bytes.len();
        let mut in_quotes = false;
        while self.pos < n {
            match bytes[self.pos] {
                b'"' => in_quotes = !in_quotes,
                b'\n' if !in_quotes => {
                    self.pos += 1;
                    self.line_starts.push(self.pos as u32);
                    return;
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    fn note_header_name(&mut self, start: usize, end: usize) {
        decode_field(&self.input[start..end], &mut self.scratch);
        let col = self.col_selects.len();
        let positional = (col + 1).to_string();
        let select = if self.scratch.is_empty() {
            positional
        } else {
            let name = select_name(&self.scratch);
            if self.col_selects.contains(&name) {
                // Duplicate header — xsv resolves the name to the
                // first occurrence, so later ones go positional.
                positional
            } else {
                name
            }
        };
        self.col_selects.push(select);
    }

    fn build_meta(&self) -> CsvMeta {
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
        CsvMeta {
            delimiter: self.delimiter,
            table_width: acc.saturating_sub(GUTTER_CHARS),
            col_widths,
            col_origins,
            col_selects: self.col_selects.clone(),
        }
    }

    fn build_output(&self, line_starts: Vec<Offset>) -> ParseOutput {
        ParseOutput {
            line_starts,
            paths: PathIndex {
                // Root only — cells are re-derived on demand, clicks
                // resolve through `locate`, never through PathIndex.
                entries: vec![PathEntry {
                    start: 0,
                    end: self.input.len() as u32,
                    parent: ROOT_PARENT,
                    segment: PathSegment::Root,
                }],
            },
            styles: Vec::new(),
            names: Default::default(),
            error: None,
            bytes: self.input.len(),
            csv: Some(self.build_meta()),
        }
    }

    /// Browsable snapshot of everything indexed so far. Clones the
    /// record-start vector — cheap relative to the scan itself.
    pub fn snapshot(&self) -> ParseOutput {
        self.build_output(self.line_starts.clone())
    }

    /// Final output; consumes the indexer, no clone.
    pub fn into_output(mut self) -> ParseOutput {
        if let Some(sink) = self.progress {
            sink.bytes_done
                .store(self.input.len() as u64, Ordering::Relaxed);
        }
        let line_starts = std::mem::take(&mut self.line_starts);
        self.build_output(line_starts)
    }
}

pub fn parse(input: &[u8], delimiter: u8, progress: Option<&ProgressSink>) -> ParseOutput {
    let mut ix = Indexer::new(input, delimiter, progress);
    while !ix.scan(usize::MAX) {}
    ix.into_output()
}

/// Unquote a field: strip surrounding quotes, collapse `""` escapes.
/// Unquoted fields copy through as-is.
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

// --- on-demand record access (draw / click / copy) ---------------------

/// Field byte ranges of the record starting at `start` (a `line_starts`
/// entry), in column order. Rescans the record's bytes — the only
/// per-cell state the document keeps is this function's input.
pub fn scan_cells(bytes: &[u8], start: u32, delimiter: u8) -> Vec<(u32, u32)> {
    let mut pos = start as usize;
    if pos == 0 && bytes.starts_with(BOM) {
        pos = 3;
    }
    if pos >= bytes.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    loop {
        let s = pos;
        let (stop, term) = scan_one_field(bytes, pos, delimiter);
        let e = strip_cr(bytes, s, stop, term);
        out.push((s as u32, e as u32));
        match term {
            Term::Delim => pos = stop + 1,
            Term::Newline | Term::Eof => break,
        }
    }
    out
}

/// What a byte offset points at: the record, and the cell when the
/// offset lands inside one (delimiters/gutters resolve to row-only).
pub struct Hit {
    /// 0-based data-row index; None on the header record.
    pub data_row: Option<u32>,
    pub col: Option<usize>,
    pub cell: Option<(u32, u32)>,
    /// Record content range (trailing newline / `\r` excluded).
    pub record: (u32, u32),
}

/// Resolve `offset` to its record and (maybe) cell by rescanning the
/// one record that contains it.
pub fn locate(bytes: &[u8], line_starts: &[u32], delimiter: u8, offset: u32) -> Hit {
    let line = line_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1);
    let start = line_starts.get(line).copied().unwrap_or(0);
    let cells = scan_cells(bytes, start, delimiter);
    let record_end = cells.last().map(|c| c.1).unwrap_or(start);
    let mut col = None;
    let mut cell = None;
    for (i, &(s, e)) in cells.iter().enumerate() {
        if offset >= s && offset < e {
            col = Some(i);
            cell = Some((s, e));
            break;
        }
    }
    Hit {
        data_row: if line == 0 { None } else { Some(line as u32 - 1) },
        col,
        cell,
        record: (start, record_end),
    }
}

/// xsv pipeline for a hit: cell → `xsv slice -i R | xsv select C`,
/// row → `xsv slice -i R`, header cell → `xsv select C`, header
/// gutter / nothing → `xsv table`.
pub fn expression_for(meta: &CsvMeta, hit: &Hit) -> String {
    let mut parts = Vec::new();
    if let Some(r) = hit.data_row {
        parts.push(format!("xsv slice -i {}", r));
    }
    if let Some(c) = hit.col {
        let select = meta
            .col_selects
            .get(c)
            .cloned()
            .unwrap_or_else(|| (c + 1).to_string());
        parts.push(format!("xsv select {}", select));
    }
    if parts.is_empty() {
        return "xsv table".to_string();
    }
    parts.join(" | ")
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
/// Clicks in a gutter clamp to the end of the preceding cell (so the
/// hit resolves to the row); clicks past a row's last cell clamp to
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

// --- format-dispatch fallbacks ------------------------------------------

/// CSV paths never round-trip through `PathIndex` (only the root entry
/// exists) — clicks resolve via `locate`/`expression_for`. This exists
/// to keep the `format::path_expression` dispatch total.
pub fn path_expression(
    _segments: &[PathSegment],
    _names: &super::NameInterner,
) -> String {
    "xsv table".to_string()
}

/// Raw bytes covered by `entry` — only the root entry exists for CSV,
/// so this is the whole input (used by whole-document copy).
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

    fn expr_at(src: &[u8], out: &ParseOutput, offset: u32) -> String {
        let meta = out.csv.as_ref().unwrap();
        let hit = locate(src, &out.line_starts, meta.delimiter, offset);
        expression_for(meta, &hit)
    }

    const SIMPLE: &[u8] = b"name,age,city\nalice,30,sf\nbob,7,\"new york, ny\"\n";

    #[test]
    fn line_starts_are_record_starts() {
        let out = parse_csv(SIMPLE);
        assert_eq!(out.line_starts, vec![0, 14, 26, 47]);
        assert!(out.error.is_none());
    }

    #[test]
    fn index_is_root_only() {
        let out = parse_csv(SIMPLE);
        assert_eq!(out.paths.entries.len(), 1);
        assert!(matches!(out.paths.entries[0].segment, PathSegment::Root));
    }

    #[test]
    fn quoted_newline_does_not_split_record() {
        let src = b"a,b\n1,\"x\ny\"\n2,z\n";
        let out = parse_csv(src);
        assert_eq!(out.line_starts, vec![0, 4, 12, 16]);
    }

    #[test]
    fn widths_are_content_max_capped() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
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
        assert_eq!(expr_at(SIMPLE, &out, pos), "xsv slice -i 0 | xsv select city");
    }

    #[test]
    fn header_cell_path_is_select() {
        let out = parse_csv(SIMPLE);
        assert_eq!(expr_at(SIMPLE, &out, 0), "xsv select name");
    }

    #[test]
    fn gutter_click_resolves_to_row() {
        let out = parse_csv(SIMPLE);
        // Offset of the comma after "alice" — the delimiter belongs to
        // no cell, so the hit is row-only ("alice" is data row 0).
        let comma = 14 + 5;
        assert_eq!(expr_at(SIMPLE, &out, comma), "xsv slice -i 0");
    }

    #[test]
    fn header_gutter_resolves_to_table() {
        let out = parse_csv(SIMPLE);
        // The comma after "name" on the header row.
        assert_eq!(expr_at(SIMPLE, &out, 4), "xsv table");
    }

    #[test]
    fn weird_header_names_are_quoted() {
        let src = b"first name,\"a,b\"\nx,y\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'x').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select \"first name\"");
        let pos = src.iter().position(|&b| b == b'y').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select \"a,b\"");
    }

    #[test]
    fn duplicate_and_empty_headers_go_positional() {
        let src = b"a,a,\n1,2,3\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'1').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select a");
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select 2");
        let pos = src.iter().position(|&b| b == b'3').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select 3");
    }

    #[test]
    fn ragged_row_gets_positional_columns() {
        let src = b"a\n1,2\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select 2");
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths.len(), 2);
        assert_eq!(meta.col_selects, vec!["a", "2"]);
    }

    #[test]
    fn root_path_is_table() {
        let out = parse_csv(b"");
        assert_eq!(expr_at(b"", &out, 0), "xsv table");
    }

    #[test]
    fn locate_finds_cell_row_and_record() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        // Cell: quoted field comes back verbatim, quotes included.
        let pos = SIMPLE.windows(4).position(|w| w == b"new ").unwrap() as u32;
        let hit = locate(SIMPLE, &out.line_starts, meta.delimiter, pos);
        let (s, e) = hit.cell.unwrap();
        assert_eq!(&SIMPLE[s as usize..e as usize], b"\"new york, ny\"");
        // Record: whole row without the trailing newline.
        let (rs, re) = hit.record;
        assert_eq!(&SIMPLE[rs as usize..re as usize], b"bob,7,\"new york, ny\"");
        assert_eq!(hit.data_row, Some(1));
        assert_eq!(hit.col, Some(2));
    }

    #[test]
    fn crlf_is_stripped_from_fields() {
        let src = b"a,b\r\n1,2\r\n";
        let out = parse_csv(src);
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![1, 1]);
        let pos = src.iter().position(|&b| b == b'2').unwrap() as u32;
        let hit = locate(src, &out.line_starts, b',', pos);
        let (s, e) = hit.cell.unwrap();
        assert_eq!(&src[s as usize..e as usize], b"2");
    }

    #[test]
    fn tsv_delimiter() {
        let out = parse(b"a\tb\n1\t2\n", b'\t', None);
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![1, 1]);
        assert_eq!(meta.delimiter, b'\t');
        let src = b"a\tb\n1\t2\n";
        let hit = locate(src, &out.line_starts, b'\t', 4);
        assert_eq!(expression_for(meta, &hit), "xsv slice -i 0 | xsv select a");
    }

    #[test]
    fn bom_is_skipped_for_header_name() {
        let src = b"\xEF\xBB\xBFa,b\n1,2\n";
        let out = parse_csv(src);
        let pos = src.iter().position(|&b| b == b'1').unwrap() as u32;
        assert_eq!(expr_at(src, &out, pos), "xsv slice -i 0 | xsv select a");
    }

    // --- on-demand cells ---

    fn cells_of_line(src: &[u8], out: &ParseOutput, line: usize) -> Vec<(u32, u32)> {
        scan_cells(src, out.line_starts[line], b',')
    }

    #[test]
    fn scan_cells_finds_row_fields() {
        let out = parse_csv(SIMPLE);
        let cells = cells_of_line(SIMPLE, &out, 1);
        assert_eq!(cells.len(), 3);
        assert_eq!(&SIMPLE[cells[0].0 as usize..cells[0].1 as usize], b"alice");
        assert_eq!(&SIMPLE[cells[2].0 as usize..cells[2].1 as usize], b"sf");
    }

    #[test]
    fn scan_cells_empty_past_eof() {
        let out = parse_csv(SIMPLE);
        // Phantom line after the trailing newline.
        assert!(scan_cells(SIMPLE, *out.line_starts.last().unwrap(), b',').is_empty());
    }

    // --- layout mapping ---

    #[test]
    fn visual_col_round_trip() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(SIMPLE, &out, 1);
        let b30 = SIMPLE.windows(2).position(|w| w == b"30").unwrap() as u32;
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, b30), 7);
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, b30 + 1), 8);
        assert_eq!(byte_of_visual_col(meta, &cells, SIMPLE, 7), Some(b30));
        assert_eq!(byte_of_visual_col(meta, &cells, SIMPLE, 8), Some(b30 + 1));
        assert_eq!(visual_col_of_byte(meta, &cells, SIMPLE, 14 + 5), 5);
        let back = byte_of_visual_col(meta, &cells, SIMPLE, 6).unwrap();
        assert_eq!(back, 14 + 5);
    }

    #[test]
    fn visual_col_clamps_past_truncation() {
        let long = format!("h\n{}\n", "x".repeat(200));
        let out = parse_csv(long.as_bytes());
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(long.as_bytes(), &out, 1);
        let end_byte = 2 + 200;
        assert_eq!(
            visual_col_of_byte(meta, &cells, long.as_bytes(), end_byte),
            MAX_COL_CHARS
        );
        let clicked = byte_of_visual_col(meta, &cells, long.as_bytes(), 500).unwrap();
        assert_eq!(clicked, 2 + MAX_COL_CHARS);
    }

    // --- rendering ---

    #[test]
    fn render_row_pads_and_colors() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(SIMPLE, &out, 1);
        let rr = render_row(meta, &cells, SIMPLE, false, 0, 1000);
        assert_eq!(rr.origin_chars, 0);
        assert_eq!(rr.text, "alice  30   sf");
        assert_eq!(rr.spans, vec![(7, 9, StyleKind::Number)]);

        let hdr = render_row(meta, &cells_of_line(SIMPLE, &out, 0), SIMPLE, true, 0, 1000);
        assert_eq!(hdr.text, "name   age  city");
        assert!(hdr.spans.iter().all(|s| s.2 == StyleKind::Key));
    }

    #[test]
    fn render_row_truncates_with_ellipsis() {
        let long = format!("h,k\n{},z\n", "x".repeat(200));
        let out = parse_csv(long.as_bytes());
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(long.as_bytes(), &out, 1);
        let rr = render_row(meta, &cells, long.as_bytes(), false, 0, 1000);
        let first: String = rr.text.chars().take(MAX_COL_CHARS as usize).collect();
        assert!(first.ends_with('…'));
        assert_eq!(first.chars().count(), MAX_COL_CHARS as usize);
        assert!(rr.text.ends_with('z'));
        assert_eq!(rr.text.chars().count() as u32, meta.col_origins[1] + 1);
    }

    #[test]
    fn render_row_skips_cells_left_of_viewport() {
        let out = parse_csv(SIMPLE);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(SIMPLE, &out, 1);
        let rr = render_row(meta, &cells, SIMPLE, false, 13, 1000);
        assert_eq!(rr.origin_chars, 12);
        assert_eq!(rr.text, "sf");
    }

    #[test]
    fn render_row_substitutes_embedded_newline() {
        let src = b"a,b\n1,\"x\ny\"\n";
        let out = parse_csv(src);
        let meta = out.csv.as_ref().unwrap();
        let cells = cells_of_line(src, &out, 1);
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
    fn unterminated_quote_runs_to_eof() {
        let src = b"a,b\n1,\"oops\n2,3\n";
        let out = parse_csv(src);
        assert_eq!(out.line_starts.len(), 2);
        assert!(out.error.is_none());
    }

    // --- incremental indexing ---

    #[test]
    fn budgeted_scan_matches_full_parse() {
        let mut src = String::from("id,name,note\n");
        for i in 0..500 {
            use std::fmt::Write;
            let _ = writeln!(src, "{i},row{i},\"note {i}, quoted\"");
        }
        let full = parse(src.as_bytes(), b',', None);

        let mut ix = Indexer::new(src.as_bytes(), b',', None);
        let mut snapshots = 0;
        let mut last_lines = 0;
        while !ix.scan(64) {
            let snap = ix.snapshot();
            assert!(snap.line_starts.len() >= last_lines, "snapshots only grow");
            last_lines = snap.line_starts.len();
            snapshots += 1;
        }
        assert!(snapshots > 3, "budget produced multiple snapshots");
        let out = ix.into_output();
        assert_eq!(out.line_starts, full.line_starts);
        let (a, b) = (out.csv.unwrap(), full.csv.unwrap());
        assert_eq!(a.col_widths, b.col_widths);
        assert_eq!(a.col_selects, b.col_selects);
    }

    #[test]
    fn widths_freeze_after_sample() {
        // Sample covers the header + first row; the later, wider field
        // and the quoted-newline record must not widen columns but must
        // still index record boundaries correctly.
        let src = b"a,b\n1,22\nlonger-than-sample,\"x\ny\"\n3,4\n";
        let mut ix = Indexer::new(src, b',', None).with_sample_limit(9);
        while !ix.scan(usize::MAX) {}
        let out = ix.into_output();
        let meta = out.csv.as_ref().unwrap();
        assert_eq!(meta.col_widths, vec![1, 2], "post-freeze rows don't widen");
        // Header + 3 data records + phantom trailing line.
        assert_eq!(out.line_starts, vec![0, 4, 9, 34, 38]);
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

        eprintln!(
            "parsed {:.1} MB in {:?} → {:.0} MB/s, entries={}, index={:.1} MB ({} record starts)",
            size_mb,
            dt,
            size_mb / dt.as_secs_f64(),
            out.paths.entries.len(),
            (out.line_starts.len() * std::mem::size_of::<Offset>()) as f64 / (1024.0 * 1024.0),
            out.line_starts.len(),
        );
    }
}
