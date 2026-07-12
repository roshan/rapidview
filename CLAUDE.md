# Rapid View

Native macOS viewer for JSON, XML, and CSV/TSV files. Rust + AppKit via `objc2`/`objc2-app-kit`. No Electron, no web view. Multi-window with AppKit auto-tabbing.

This repo is a Cargo workspace. **Rapid View** (this crate) is one of three members; the others are **Markview** (a separate macOS app for markdown — see `markview/`) and **markdown-core** (the parser Markview depends on). Rapid View handles JSON, XML, and CSV/TSV; markdown lives in Markview because rendering it requires very different presentation.

## File layout

```
src/format/mod.rs    shared types + dispatch (Format, ParseOutput, PathSegment,
                     StyleSpan, NameInterner, ProgressSink, plus detect/parse/
                     prettify/path_expression/value_bytes_for_entry dispatch fns)
src/format/json.rs   JSON tokenizer + jq path formatter + prettifier
src/format/xml.rs    XML tokenizer + XPath formatter + two-pass prettifier
                     (classify each element as block-or-mixed, then emit)
src/format/csv.rs    CSV/TSV tokenizer + xsv path formatter + table-layout
                     metadata (CsvMeta: per-column widths/origins) + the
                     draw-time helpers DocView uses (record_cells,
                     visual_col_of_byte, byte_of_visual_col, render_row)
src/doc.rs           Document = bytes + format + ParseOutput + max_line_bytes.
                     ByteSource is Arc<Mmap> for files, Arc<[u8]> for clipboard
                     and prettify output.
src/doc_view.rs      DocView, NSView subclass. Fixed monospace font →
                     constant line height + advance → drawRect: picks the
                     visible byte range and paints with CoreText.
src/worker.rs        Background thread per request, mpsc back to main, drained
                     by a 16 ms NSTimer that tears itself down on idle.
src/main.rs          AppDelegate, window/tab management, menu bar, toolbar,
                     worker-message dispatch.
markdown-core/       Markdown parser + types (path tree + per-line BlockKind
                     classification). Used by Markview, not by Rapid View.
markview/            Separate binary crate — Markview app. NSTextView-based
                     viewer with rendered/source mode toggle.
```

The renderer is format-agnostic — `ParseOutput` is identical between JSON and XML. Anything format-specific (path expression, sub-tree extraction, pretty-printer) dispatches on `Format` at the call site, e.g. `format::path_expression(doc.format, ...)`. The one exception is CSV: `ParseOutput.csv` carries table-layout metadata, and `DocView` renders CSV as an aligned table computed **at draw time** — fields drawn at column origins, capped at 64 chars with a visual-only `…` (the mmapped bytes are never copied or padded, so Copy always yields the full field). `line_starts` for CSV are *record* starts; embedded newlines in quoted fields render as `␤` via `csv::display_char` in both table and raw mode.

## Critical conventions

- `#![deny(unsafe_op_in_unsafe_fn)]` at the crate root. Cocoa calls go inside explicit `unsafe { ... }` blocks.
- objc2 `define_class!` for AppKit subclasses (`RVAppDelegate`, `RVDocView`).
- Per-tab state lives in `app_state::WINDOWS: HashMap<WindowId, WindowState>`. `WindowId` is the raw `NSWindow` pointer reinterpreted as `usize` — stable for the window's lifetime, never dereferenced.
- One worker thread per request. Worker emits `ParseStarted` first (carrying `Arc<ProgressSink>`), then exactly one terminal message: `DocumentReady`, `PrettyReady`, or `Error`. `WORK_PENDING` only decrements on terminal messages; the poll timer tears down when it hits zero.
- Format auto-detection: `format::detect` looks at the first non-whitespace byte after a possible UTF-8 BOM. `<` → XML, else JSON. CSV/TSV are picked by **file extension only** (`detect_for_path`: `.csv` / `.tsv` / `.tab`) — never by content sniffing, so clipboard pastes can't become CSV (deliberate, per Roshan).
- For CSV docs the Prettify button is a Table ↔ Original toggle (`DocView::set_csv_table_mode`) — a draw-flag flip, no worker round-trip, no second `Document`. `format::prettify` is never invoked for CSV.

## Build / test / deploy

```sh
cargo build -p rapid-view             # debug
cargo test --workspace                # all unit + integration tests across crates
cargo run -p rapid-view -- some.json  # run from source against a file
./bundle.sh release                   # build the .app (writes to $CARGO_TARGET_DIR/release)
mise run deploy                       # build + codesign --sign - + install to /Applications
mise run deploy-markview              # same, for Markview.app
mise run deploy-all                   # both apps in one go
```

`Info.plist` declares `public.json` and `public.xml` as `LSHandlerRank=Alternate` so Rapid View is offered as a viewer but doesn't fight other apps for ownership.

## Gotchas

- **mmap means reading and parsing are interleaved** via demand-paging. There's no separate "I/O phase" to time or progress-bar — the parser's `pos / total` is the truth of what's happening.
- **`setReleasedWhenClosed(false)` on every window** is required. The Rust `Retained<NSWindow>` is the canonical owner; without this, AppKit sends a second release on user-close and double-frees.
- **`tabbingIdentifier = "RapidView"`** on every window so AppKit auto-tabs them regardless of the user's global "Prefer tabs" preference.
- **XPath `[N]` is only emitted when an element has same-named siblings.** Computed by `fixup_unique_siblings` at the end of XML parse. Don't remove it without rewriting the formatter.
- **Index memory grows with structure, not bytes.** For Apple Health-sized XML (millions of `<Record>`s), `Vec<StyleSpan>` (12 B each) and `Vec<PathEntry>` (16 B each) dominate — total RSS during load can hit several GB. Cheapest fixes if this matters: pack `StyleSpan` to 8 B, drop per-attribute `PathEntry`s.
- **Panics on user-launch lose stderr** (Finder gives `/dev/null`). A panic hook in `main` writes them to `/tmp/rapid-view-panic.log` instead. Check there first when diagnosing crash reports from real launches.
- **`#[cold] #[inline(never)]` on `flush_progress`** in both parsers is load-bearing — keeps the hot `advance()` loop tight. Don't drop those attributes.

## Selectors and chrome

ObjC selectors on `RVAppDelegate`: `rvNewWindow:`, `rvOpenDocument:`, `rvTogglePrettify:`, `rvPaste:`, `rvClearDocument:`, `rvCopyPath:`, `rvCopySubtree:`, `rvShowSearch:`, `rvSearchNext:`, `rvSearchPrev:`, `rvDismissSearch:`, `rvSearchFieldAction:`, `rvSearchChanged:`, `rvWorkerTick:`.

Toolbar buttons "Copy jq" / "Copy XPath" / "Copy xsv" and "Copy JSON" / "Copy XML" / "Copy CSV" have their titles updated by `refresh_format_chrome` when a document loads (which also sets the Prettify button to "Original" for CSV). CSV path expressions are xsv pipelines: cell → `xsv slice -i R | xsv select C`, header → `xsv select C`, root → `xsv table`.

## Fixtures

- `fixtures/sample.json`, `fixtures/sample.xml` — small clean inputs.
- `fixtures/sample.csv` — exercises quoting, `""` escapes, an over-64-char field (truncation), an embedded newline, and a trailing empty field.
- `fixtures/reddit.json`, `fixtures/large.json` — bigger inputs.
- `~/Downloads/apple_health_export/export.xml` (1.4 GB) — a known stress test on Roshan's machine, not in the repo.

## Related KB

- `KB:RAPIDVIEW:docs/overview` — longer-form project overview in Taskmaster KB, fetched via the `taskmaster-kb` skill.
