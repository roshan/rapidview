//! Format-agnostic parsing API.
//!
//! `detect` sniffs the first non-whitespace byte to pick JSON vs XML.
//! Parsing produces a `ParseOutput` the renderer treats identically
//! regardless of source format — the format-specific bits (path
//! expression, pretty-printer, sub-tree extraction) live behind
//! dispatch functions that switch on `Format`.

pub mod json;
pub mod xml;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free progress channel between a parser running on the worker
/// thread and the UI thread that polls it for a determinate progress
/// bar. Created by the worker with the file's total byte count; the
/// parser stores `pos` into `bytes_done` every ~1 MB on the hot path.
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

    /// 0.0–1.0 progress fraction. Empty inputs report 1.0 immediately.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        let done = self.bytes_done.load(Ordering::Relaxed) as f64;
        (done / self.total as f64).clamp(0.0, 1.0)
    }
}

/// How many input bytes between progress-counter updates from a parser.
/// 1 MB → ~1400 updates on a 1.4 GB file; well under any sane UI refresh
/// rate and cheap enough to amortise to ~free per byte.
pub const PROGRESS_GRANULARITY: usize = 1 << 20;

pub type Offset = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Xml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    // JSON
    Key,
    String,
    Number,
    Bool,
    Null,
    // XML
    Tag,
    AttrName,
    AttrValue,
    Comment,
    CData,
    Pi,
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
    /// JSON object key — interned key name id.
    Key(u32),
    /// JSON array index.
    Index(u32),
    /// XML element. `sibling_index` is 1-based when the element has
    /// same-named siblings under its parent, else 0 (omit predicate).
    Element { name: u32, sibling_index: u32 },
    /// XML attribute name (interned).
    Attribute(u32),
}

#[derive(Debug, Clone, Copy)]
pub struct PathEntry {
    pub start: Offset,
    pub end: Offset,
    pub parent: u32,
    pub segment: PathSegment,
}

pub const ROOT_PARENT: u32 = u32::MAX;

/// Bytes-keyed string interner. Used for JSON object keys and XML
/// element / attribute names — same shape, same lifetime as the doc.
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
    /// Innermost entry whose range contains `offset`. Walks up via
    /// `parent` if the nearest-by-start sibling has already ended —
    /// proper nesting guarantees the parent then contains it, so this
    /// is O(depth).
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
    pub names: NameInterner,
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
#[allow(dead_code)] // variants constructed by parsers, read via Debug
pub enum ParseErrorKind {
    UnexpectedByte(u8),
    UnexpectedEof,
    InvalidEscape,
    InvalidNumber,
    /// XML: closing tag doesn't match the open tag at the top of the stack.
    MismatchedTag,
}

// --- dispatch -------------------------------------------------------

/// Sniff `bytes` to decide format. Skips UTF-8 BOM and leading
/// whitespace. `<` → XML; anything else → JSON (which is also the
/// default for empty input).
pub fn detect(bytes: &[u8]) -> Format {
    let mut i = 0;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        i = 3;
    }
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'<' => return Format::Xml,
            _ => return Format::Json,
        }
    }
    Format::Json
}

pub fn parse(
    format: Format,
    input: &[u8],
    progress: Option<&ProgressSink>,
) -> ParseOutput {
    match format {
        Format::Json => json::parse(input, progress),
        Format::Xml => xml::parse(input, progress),
    }
}

pub fn prettify(format: Format, input: &[u8]) -> Vec<u8> {
    match format {
        Format::Json => json::prettify(input),
        Format::Xml => xml::prettify(input),
    }
}

pub fn path_expression(
    format: Format,
    segments: &[PathSegment],
    names: &NameInterner,
) -> String {
    match format {
        Format::Json => json::path_expression(segments, names),
        Format::Xml => xml::path_expression(segments, names),
    }
}

pub fn value_bytes_for_entry<'a>(
    format: Format,
    bytes: &'a [u8],
    entry: &PathEntry,
) -> &'a [u8] {
    match format {
        Format::Json => json::value_bytes_for_entry(bytes, entry),
        Format::Xml => xml::value_bytes_for_entry(bytes, entry),
    }
}

/// Short label for the path-expression dialect — used in the toolbar
/// button title so JSON users see "Copy jq" and XML users see "Copy XPath".
pub fn path_label(format: Format) -> &'static str {
    match format {
        Format::Json => "jq",
        Format::Xml => "XPath",
    }
}

/// Short label for the document content kind — used in the sub-tree
/// copy button title.
pub fn content_label(format: Format) -> &'static str {
    match format {
        Format::Json => "JSON",
        Format::Xml => "XML",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_json_vs_xml() {
        assert_eq!(detect(b"{\"a\":1}"), Format::Json);
        assert_eq!(detect(b"[]"), Format::Json);
        assert_eq!(detect(b"  42"), Format::Json);
        assert_eq!(detect(b"<root/>"), Format::Xml);
        assert_eq!(detect(b"\n  <?xml version=\"1.0\"?><a/>"), Format::Xml);
        assert_eq!(detect(b"\xEF\xBB\xBF<a/>"), Format::Xml);
        assert_eq!(detect(b""), Format::Json);
    }
}
