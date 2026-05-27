//! Background worker — opens + parses markdown off the main thread.
//! Results come back through an `mpsc::Receiver` polled on an
//! `NSTimer`, same pattern Rapid View uses.

use crate::doc::{ByteSource, Document};
use markdown_core::ProgressSink;
use memmap2::Mmap;
use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

/// Raw NSWindow pointer reinterpreted as a usize. Stable for the
/// window's lifetime, used as a map key only.
pub type WindowId = usize;

pub enum WorkerMsg {
    ParseStarted {
        window_id: WindowId,
        progress: Arc<ProgressSink>,
    },
    DocumentReady {
        window_id: WindowId,
        doc: Arc<Document>,
        path: String,
    },
    Error {
        window_id: WindowId,
        message: String,
    },
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

pub fn spawn_load(window_id: WindowId, path: String, tx: Sender<WorkerMsg>) {
    thread::spawn(move || {
        let source = match open_source(&path) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(WorkerMsg::Error {
                    window_id,
                    message: format!("open {}: {}", path, e),
                });
                return;
            }
        };
        let progress = Arc::new(ProgressSink::new(source.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(source, Some(&progress));
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path,
        });
    });
}

pub fn spawn_parse_bytes(
    window_id: WindowId,
    bytes: Vec<u8>,
    label: String,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        let progress = Arc::new(ProgressSink::new(bytes.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(ByteSource::from_vec(bytes), Some(&progress));
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path: label,
        });
    });
}

fn open_source(path: &str) -> Result<ByteSource, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if metadata.len() == 0 {
        Ok(ByteSource::from_vec(Vec::new()))
    } else {
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
        Ok(ByteSource::Mmap(Arc::new(mmap)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TEST_WINDOW_ID: WindowId = 0xCAFE;

    fn recv_after_started(chan: &WorkerChannel) -> WorkerMsg {
        let first = chan.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first, WorkerMsg::ParseStarted { .. }));
        chan.rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn loads_fixture() {
        let chan = WorkerChannel::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.md").to_string();
        spawn_load(TEST_WINDOW_ID, path, chan.tx.clone());
        match recv_after_started(&chan) {
            WorkerMsg::DocumentReady { doc, .. } => {
                assert!(doc.bytes.len() > 0);
                assert!(!doc.output.blocks.is_empty());
            }
            other => panic!("unexpected message: {}", match other {
                WorkerMsg::Error { message, .. } => format!("Error: {}", message),
                _ => "other".to_string(),
            }),
        }
    }
}
