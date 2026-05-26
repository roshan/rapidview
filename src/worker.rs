//! Background worker — runs file I/O, parsing and pretty-printing off
//! the main thread. Results come back through an `mpsc::Receiver` that
//! main polls on an `NSTimer`.
//!
//! We deliberately don't use a runtime: one `std::thread::spawn` per
//! request, one channel, and a timer tick. For a doc viewer the total
//! async surface is two events per file (parse done, pretty done), so
//! anything fancier would be overhead.

use crate::doc::{ByteSource, Document};
use crate::format::{self, Format};
use memmap2::Mmap;
use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

/// Every worker message is tagged with a window identifier so the
/// dispatcher can route it to the right tab. The identifier is the
/// raw NSWindow pointer reinterpreted as a usize — stable for the
/// window's lifetime, zero cost to compare.
pub type WindowId = usize;

pub enum WorkerMsg {
    /// The original document is ready.
    DocumentReady {
        window_id: WindowId,
        doc: Arc<Document>,
        path: String,
    },
    /// The pretty-printed document is ready.
    PrettyReady {
        window_id: WindowId,
        doc: Arc<Document>,
    },
    /// Load or pretty job failed.
    Error { window_id: WindowId, message: String },
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

/// Spawn a worker that opens `path`, mmaps it, sniffs the format,
/// parses it, and sends a `DocumentReady` on the channel. Errors come
/// back as `Error { .. }`.
pub fn spawn_load(window_id: WindowId, path: String, tx: Sender<WorkerMsg>) {
    thread::spawn(move || {
        let msg = match load(&path) {
            Ok(doc) => WorkerMsg::DocumentReady {
                window_id,
                doc,
                path,
            },
            Err(e) => WorkerMsg::Error {
                window_id,
                message: format!("open {}: {}", path, e),
            },
        };
        let _ = tx.send(msg);
    });
}

/// Spawn a worker that parses an in-memory byte buffer (clipboard
/// contents, typically) and delivers it as `DocumentReady` with the
/// given display label as the "path".
pub fn spawn_parse_bytes(
    window_id: WindowId,
    bytes: Vec<u8>,
    label: String,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        let format = format::detect(&bytes);
        let doc = Document::from_source(format, ByteSource::from_vec(bytes));
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path: label,
        });
    });
}

/// Spawn a worker that pretty-prints the given bytes for `format` and
/// re-parses the result, returning a second `Document` that main can
/// swap into the view when the user toggles Prettify.
pub fn spawn_prettify(
    window_id: WindowId,
    format: Format,
    source: ByteSource,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        let pretty_bytes = format::prettify(format, source.as_slice());
        let doc = Document::from_source(format, ByteSource::from_vec(pretty_bytes));
        let _ = tx.send(WorkerMsg::PrettyReady { window_id, doc });
    });
}

fn load(path: &str) -> Result<Arc<Document>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let source = if metadata.len() == 0 {
        ByteSource::from_vec(Vec::new())
    } else {
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
        ByteSource::Mmap(Arc::new(mmap))
    };
    let format = format::detect(source.as_slice());
    Ok(Document::from_source(format, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TEST_WINDOW_ID: WindowId = 0xDEADBEEF;

    #[test]
    fn spawn_load_fixture_via_channel() {
        let chan = WorkerChannel::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.json").to_string();
        spawn_load(TEST_WINDOW_ID, path, chan.tx.clone());
        let msg = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should produce a result within 5s");
        match msg {
            WorkerMsg::DocumentReady {
                window_id,
                doc,
                path,
            } => {
                assert_eq!(window_id, TEST_WINDOW_ID);
                assert!(doc.bytes.len() > 0);
                assert!(doc.line_count() > 1, "fixture has multiple lines");
                assert!(doc.output.error.is_none(), "fixture parses cleanly");
                assert_eq!(doc.format, Format::Json);
                assert!(path.ends_with("sample.json"));
            }
            WorkerMsg::Error { message, .. } => panic!("unexpected worker error: {}", message),
            WorkerMsg::PrettyReady { .. } => panic!("got PrettyReady from spawn_load"),
        }
    }

    #[test]
    fn spawn_prettify_round_trip() {
        let compact: Arc<[u8]> =
            Arc::<[u8]>::from(br#"{"a":1,"b":[1,2,3]}"#.to_vec().into_boxed_slice());
        let source = ByteSource::Owned(compact.clone());
        let chan = WorkerChannel::new();
        spawn_prettify(TEST_WINDOW_ID, Format::Json, source, chan.tx.clone());
        let msg = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("pretty worker should finish within 5s");
        match msg {
            WorkerMsg::PrettyReady { window_id, doc } => {
                assert_eq!(window_id, TEST_WINDOW_ID);
                assert!(doc.bytes.len() >= compact.len());
                assert!(doc.output.error.is_none());
                assert!(doc.line_count() > 1);
            }
            _ => panic!("wrong message type"),
        }
    }
}
