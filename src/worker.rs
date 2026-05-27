//! Background worker — runs file I/O, parsing and pretty-printing off
//! the main thread. Results come back through an `mpsc::Receiver` that
//! main polls on an `NSTimer`.
//!
//! We deliberately don't use a runtime: one `std::thread::spawn` per
//! request, one channel, and a timer tick. For a doc viewer the total
//! async surface is two events per file (parse done, pretty done), so
//! anything fancier would be overhead.

use crate::doc::{ByteSource, Document};
use crate::format::{self, Format, ProgressSink};
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
    /// Parsing has begun. Carries a shared progress handle the UI can
    /// poll to drive a determinate progress bar. Sent before any
    /// expensive work so the indicator can appear immediately.
    ParseStarted {
        window_id: WindowId,
        progress: Arc<ProgressSink>,
    },
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
/// parses it (publishing progress to a shared sink so the UI can show
/// a progress bar), and sends a `DocumentReady` on the channel.
/// Errors come back as `Error { .. }`.
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
        let format = format::detect_with_path(&path, source.as_slice());
        let progress = Arc::new(ProgressSink::new(source.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(format, source, Some(&progress));
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path,
        });
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
        let progress = Arc::new(ProgressSink::new(bytes.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(
            format,
            ByteSource::from_vec(bytes),
            Some(&progress),
        );
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path: label,
        });
    });
}

/// Re-parse an already-loaded `ByteSource` under a different `format`.
/// Used when the user overrides the auto-detected format from the
/// header picker — the bytes don't change but the parse indexes do.
/// The result arrives as `DocumentReady` and is installed by
/// `on_document_ready` exactly like a fresh load, which resets the
/// pretty cache and viewport state.
pub fn spawn_reparse(
    window_id: WindowId,
    format: Format,
    source: ByteSource,
    label: String,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        let progress = Arc::new(ProgressSink::new(source.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(format, source, Some(&progress));
        let _ = tx.send(WorkerMsg::DocumentReady {
            window_id,
            doc,
            path: label,
        });
    });
}

/// Spawn a worker that pretty-prints the given bytes for `format` and
/// re-parses the result, returning a second `Document` that main can
/// swap into the view when the user toggles Prettify. Progress is
/// reported during the re-parse phase only — the prettifier itself
/// is fast enough that adding a counter would just be noise.
pub fn spawn_prettify(
    window_id: WindowId,
    format: Format,
    source: ByteSource,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        let pretty_bytes = format::prettify(format, source.as_slice());
        let progress = Arc::new(ProgressSink::new(pretty_bytes.len() as u64));
        let _ = tx.send(WorkerMsg::ParseStarted {
            window_id,
            progress: progress.clone(),
        });
        let doc = Document::from_source(
            format,
            ByteSource::from_vec(pretty_bytes),
            Some(&progress),
        );
        let _ = tx.send(WorkerMsg::PrettyReady { window_id, doc });
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

    const TEST_WINDOW_ID: WindowId = 0xDEADBEEF;

    /// Drain `ParseStarted` (always sent first now) and return the
    /// next worker message — the one tests actually want to assert on.
    fn recv_after_started(chan: &WorkerChannel) -> WorkerMsg {
        let first = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ParseStarted should arrive within 5s");
        assert!(matches!(first, WorkerMsg::ParseStarted { .. }));
        chan.rx
            .recv_timeout(Duration::from_secs(5))
            .expect("terminal message should follow ParseStarted within 5s")
    }

    #[test]
    fn spawn_load_fixture_via_channel() {
        let chan = WorkerChannel::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.json").to_string();
        spawn_load(TEST_WINDOW_ID, path, chan.tx.clone());
        let msg = recv_after_started(&chan);
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
            WorkerMsg::ParseStarted { .. } => panic!("expected DocumentReady, got ParseStarted"),
        }
    }

    #[test]
    fn spawn_prettify_round_trip() {
        let compact: Arc<[u8]> =
            Arc::<[u8]>::from(br#"{"a":1,"b":[1,2,3]}"#.to_vec().into_boxed_slice());
        let source = ByteSource::Owned(compact.clone());
        let chan = WorkerChannel::new();
        spawn_prettify(TEST_WINDOW_ID, Format::Json, source, chan.tx.clone());
        let msg = recv_after_started(&chan);
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

    #[test]
    fn progress_reaches_total_on_load() {
        // After a load completes, the sink's bytes_done should equal
        // the file length — both the per-MB updates and the final
        // sentinel store at the end of parse_with_progress contribute.
        let chan = WorkerChannel::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.json").to_string();
        spawn_load(TEST_WINDOW_ID, path, chan.tx.clone());
        let first = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ParseStarted within 5s");
        let progress = match first {
            WorkerMsg::ParseStarted { progress, .. } => progress,
            _ => panic!("expected ParseStarted first"),
        };
        // Drain DocumentReady so the parser has finished.
        let _ = chan
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("DocumentReady within 5s");
        assert_eq!(
            progress.bytes_done.load(std::sync::atomic::Ordering::Relaxed),
            progress.total,
        );
        assert!((progress.fraction() - 1.0).abs() < 1e-9);
    }
}
