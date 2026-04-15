//! Document model — owns the byte buffer and the parse indexes.
//!
//! Backing storage is a `ByteSource` so an open document can be either an
//! `mmap`'d file (zero-copy, great for huge inputs) or an owned byte
//! buffer (used for pretty-printed output which has no on-disk source).
//! `ByteSource` is cheaply cloneable — both variants are `Arc`-wrapped —
//! so the worker thread and the main thread can share the same bytes.

#![allow(dead_code)]

use crate::parser::{self, ParseOutput};
use memmap2::Mmap;
use std::sync::Arc;

#[derive(Clone)]
pub enum ByteSource {
    Mmap(Arc<Mmap>),
    Owned(Arc<[u8]>),
}

impl ByteSource {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            ByteSource::Mmap(m) => m.as_ref(),
            ByteSource::Owned(v) => v,
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }
}

pub struct Document {
    pub bytes: ByteSource,
    pub output: ParseOutput,
    /// Longest line in bytes — used to size the document view width so
    /// long lines don't trigger per-frame layout scans.
    pub max_line_bytes: u32,
}

impl Document {
    pub fn from_source(bytes: ByteSource) -> Arc<Self> {
        let output = parser::parse(bytes.as_slice());
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
        let all = self.bytes.as_slice();
        if starts.is_empty() {
            return all;
        }
        let start = starts[line] as usize;
        let end = if line + 1 < starts.len() {
            (starts[line + 1] as usize).saturating_sub(1)
        } else {
            all.len()
        };
        let slice = &all[start..end.min(all.len())];
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
