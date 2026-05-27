//! Document model — bytes + the structure parse from markdown-core.
//!
//! Backing storage is a `ByteSource` so we can either mmap a file
//! (cheap for large inputs) or own an in-memory buffer (clipboard
//! paste). The `Arc` wrappers let the worker thread and main thread
//! share a document without copying.

use markdown_core::{ParseOutput, ProgressSink, parse};
use memmap2::Mmap;
use std::sync::Arc;

#[derive(Clone)]
pub enum ByteSource {
    Mmap(Arc<Mmap>),
    Owned(Arc<[u8]>),
}

impl ByteSource {
    pub fn from_vec(v: Vec<u8>) -> Self {
        ByteSource::Owned(Arc::<[u8]>::from(v.into_boxed_slice()))
    }

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
}

impl Document {
    pub fn from_source(bytes: ByteSource, progress: Option<&ProgressSink>) -> Arc<Self> {
        let output = parse(bytes.as_slice(), progress);
        Arc::new(Self { bytes, output })
    }
}
