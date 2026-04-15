//! Background worker — runs file I/O, parsing and pretty-printing off
//! the main thread. Results come back through an `mpsc::Receiver` that
//! main polls on an `NSTimer`.
//!
//! We deliberately don't use a runtime: one `std::thread::spawn` per
//! request, one channel, and a timer tick. For a JSON viewer the total
//! async surface is two events per file (parse done, pretty done), so
//! anything fancier would be overhead.

#![allow(dead_code)]

use crate::doc::{ByteSource, Document};
use crate::pretty;
use memmap2::Mmap;
use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

pub enum WorkerMsg {
    /// The original document is ready.
    DocumentReady { doc: Arc<Document>, path: String },
    /// The pretty-printed document is ready.
    PrettyReady(Arc<Document>),
    /// Load or pretty job failed.
    Error(String),
}

pub struct WorkerChannel {
    pub rx: Receiver<WorkerMsg>,
    pub tx: Sender<WorkerMsg>,
}

impl WorkerChannel {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { rx, tx }
    }
}

/// Spawn a worker that opens `path`, mmaps it, parses it, and sends a
/// `DocumentReady` on the channel. Errors come back as `Error(msg)`.
pub fn spawn_load(path: String, tx: Sender<WorkerMsg>) {
    thread::spawn(move || {
        let msg = match load(&path) {
            Ok(doc) => WorkerMsg::DocumentReady { doc, path },
            Err(e) => WorkerMsg::Error(format!("open {}: {}", path, e)),
        };
        let _ = tx.send(msg);
    });
}

/// Spawn a worker that pretty-prints the given bytes and parses the
/// result, returning a second `Document` that main can swap into the
/// view when the user toggles Prettify.
pub fn spawn_prettify(source: ByteSource, tx: Sender<WorkerMsg>) {
    thread::spawn(move || {
        let pretty_bytes = pretty::prettify(source.as_slice());
        let owned: Arc<[u8]> = Arc::<[u8]>::from(pretty_bytes.into_boxed_slice());
        let doc = Document::from_source(ByteSource::Owned(owned));
        let _ = tx.send(WorkerMsg::PrettyReady(doc));
    });
}

fn load(path: &str) -> Result<Arc<Document>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    // Empty files can't be mmaped on some platforms; fall back to Vec.
    let source = if metadata.len() == 0 {
        ByteSource::Owned(Arc::<[u8]>::from(Vec::new().into_boxed_slice()))
    } else {
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
        ByteSource::Mmap(Arc::new(mmap))
    };
    Ok(Document::from_source(source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn spawn_load_fixture_via_channel() {
        // End-to-end: spawn a worker, pull DocumentReady out of the
        // channel, and sanity-check the parse succeeded.
        let chan = WorkerChannel::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.json").to_string();
        spawn_load(path, chan.tx.clone());
        let msg = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should produce a result within 5s");
        match msg {
            WorkerMsg::DocumentReady { doc, path } => {
                assert!(doc.bytes.len() > 0);
                assert!(doc.line_count() > 1, "fixture has multiple lines");
                assert!(doc.output.error.is_none(), "fixture parses cleanly");
                assert!(path.ends_with("sample.json"));
            }
            WorkerMsg::Error(e) => panic!("unexpected worker error: {}", e),
            WorkerMsg::PrettyReady(_) => panic!("got PrettyReady from spawn_load"),
        }
    }

    #[test]
    fn spawn_prettify_round_trip() {
        // Pretty-printed buffer should parse cleanly and produce a
        // larger (or equal) byte length than the compact input.
        let compact: Arc<[u8]> = Arc::<[u8]>::from(br#"{"a":1,"b":[1,2,3]}"#.to_vec().into_boxed_slice());
        let source = ByteSource::Owned(compact.clone());
        let chan = WorkerChannel::new();
        spawn_prettify(source, chan.tx.clone());
        let msg = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("pretty worker should finish within 5s");
        match msg {
            WorkerMsg::PrettyReady(doc) => {
                assert!(doc.bytes.len() >= compact.len());
                assert!(doc.output.error.is_none());
                assert!(doc.line_count() > 1);
            }
            _ => panic!("wrong message type"),
        }
    }
}
