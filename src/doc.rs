//! Document model — owns the byte buffer and the parse indexes.
//!
//! Loading is synchronous today (invoked from the main thread after file
//! open). T5 moves the parse onto a worker thread with progress reporting.

#![allow(dead_code)]

use crate::parser::{self, ParseOutput};
use std::sync::Arc;

pub struct Document {
    pub bytes: Vec<u8>,
    pub output: ParseOutput,
    /// Longest line in bytes — used to size the document view width so
    /// long lines don't trigger per-frame layout scans.
    pub max_line_bytes: u32,
}

impl Document {
    pub fn from_bytes(bytes: Vec<u8>) -> Arc<Self> {
        let output = parser::parse(&bytes);
        let max_line_bytes = max_line_length(&output.line_starts, bytes.len() as u32);
        Arc::new(Self {
            bytes,
            output,
            max_line_bytes,
        })
    }

    pub fn line_count(&self) -> usize {
        self.output.line_starts.len().max(1)
    }

    /// Bytes of `line` with any trailing `\n` / `\r\n` stripped.
    pub fn line_bytes(&self, line: usize) -> &[u8] {
        let starts = &self.output.line_starts;
        if starts.is_empty() {
            return &self.bytes;
        }
        let start = starts[line] as usize;
        let end = if line + 1 < starts.len() {
            // next line starts *after* the newline, so strip one byte
            (starts[line + 1] as usize).saturating_sub(1)
        } else {
            self.bytes.len()
        };
        let slice = &self.bytes[start..end.min(self.bytes.len())];
        if slice.ends_with(b"\r") {
            &slice[..slice.len() - 1]
        } else {
            slice
        }
    }
}

fn max_line_length(starts: &[u32], total: u32) -> u32 {
    if starts.is_empty() {
        return total;
    }
    let mut max = 0u32;
    for i in 0..starts.len() {
        let a = starts[i];
        let b = if i + 1 < starts.len() {
            starts[i + 1].saturating_sub(1)
        } else {
            total
        };
        let len = b.saturating_sub(a);
        if len > max {
            max = len;
        }
    }
    max
}
