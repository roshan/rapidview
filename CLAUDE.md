# Rapid View

Native macOS viewer for JSON, XML, and Markdown files. Rust + AppKit via `objc2`/`objc2-app-kit`. No Electron, no web view. Multi-window with AppKit auto-tabbing.

## File layout

```
src/format/mod.rs       shared types + dispatch (Format, ParseOutput,
                        PathSegment, StyleSpan, NameInterner, ProgressSink,
                        plus detect/detect_with_path/parse/prettify/
                        path_expression/value_bytes_for_entry dispatch fns)
src/format/json.rs      JSON tokenizer + jq path formatter + prettifier
src/format/xml.rs       XML tokenizer + XPath formatter + two-pass prettifier
                        (classify each element as block-or-mixed, then emit)
src/format/markdown.rs  Line-oriented markdown scanner. ATX headings build
                        the path tree (PathSegment::Heading); fenced code
                        and inline `code` produce style spans. Prettify is
                        a verbatim copy — markdown has no canonical form.
src/doc.rs          Document = bytes + format + ParseOutput + max_line_bytes.
                    ByteSource is Arc<Mmap> for files, Arc<[u8]> for clipboard
                    and prettify output.
src/doc_view.rs     DocView, NSView subclass. Fixed monospace font →
                    constant line height + advance → drawRect: picks the
                    visible byte range and paints with CoreText.
src/worker.rs       Background thread per request, mpsc back to main, drained
                    by a 16 ms NSTimer that tears itself down on idle.
src/main.rs         AppDelegate, window/tab management, menu bar, toolbar,
                    worker-message dispatch.
```

The renderer is format-agnostic — `ParseOutput` is identical between JSON, XML, and Markdown. Anything format-specific (path expression, sub-tree extraction, pretty-printer) dispatches on `Format` at the call site, e.g. `format::path_expression(doc.format, ...)`.

## Critical conventions

- `#![deny(unsafe_op_in_unsafe_fn)]` at the crate root. Cocoa calls go inside explicit `unsafe { ... }` blocks.
- objc2 `define_class!` for AppKit subclasses (`RVAppDelegate`, `RVDocView`).
- Per-tab state lives in `app_state::WINDOWS: HashMap<WindowId, WindowState>`. `WindowId` is the raw `NSWindow` pointer reinterpreted as `usize` — stable for the window's lifetime, never dereferenced.
- One worker thread per request. Worker emits `ParseStarted` first (carrying `Arc<ProgressSink>`), then exactly one terminal message: `DocumentReady`, `PrettyReady`, or `Error`. `WORK_PENDING` only decrements on terminal messages; the poll timer tears down when it hits zero.
- Format auto-detection: `format::detect` looks at the first non-whitespace byte after a possible UTF-8 BOM. `<` → XML, else JSON. Markdown has no reliable byte signature, so the worker calls `detect_with_path` instead — extensions `.md`/`.markdown`/`.mdown`/`.mkd`/`.markdn` select Markdown; anything else falls through to byte sniffing. Clipboard pastes therefore can't be detected as markdown today.

## Build / test / deploy

```sh
cargo build               # debug
cargo test                # all unit + integration tests
cargo run -- some.json    # run from source against a file
./bundle.sh release       # build the .app (writes to $CARGO_TARGET_DIR/release)
mise run deploy           # build + codesign --sign - + install to /Applications
```

`Info.plist` declares `public.json`, `public.xml`, and `net.daringfireball.markdown` as `LSHandlerRank=Alternate` so Rapid View is offered as a viewer but doesn't fight other apps for ownership.

## Gotchas

- **mmap means reading and parsing are interleaved** via demand-paging. There's no separate "I/O phase" to time or progress-bar — the parser's `pos / total` is the truth of what's happening.
- **`setReleasedWhenClosed(false)` on every window** is required. The Rust `Retained<NSWindow>` is the canonical owner; without this, AppKit sends a second release on user-close and double-frees.
- **`tabbingIdentifier = "RapidView"`** on every window so AppKit auto-tabs them regardless of the user's global "Prefer tabs" preference.
- **XPath `[N]` is only emitted when an element has same-named siblings.** Computed by `fixup_unique_siblings` at the end of XML parse. Don't remove it without rewriting the formatter.
- **Markdown sections close on the next same-or-shallower heading.** A heading's `PathEntry.end` is set when popped from `heading_stack`, either by a later heading or at EOF. Folding (when added) should key off these ranges.
- **Index memory grows with structure, not bytes.** For Apple Health-sized XML (millions of `<Record>`s), `Vec<StyleSpan>` (12 B each) and `Vec<PathEntry>` (16 B each) dominate — total RSS during load can hit several GB. Cheapest fixes if this matters: pack `StyleSpan` to 8 B, drop per-attribute `PathEntry`s.
- **Panics on user-launch lose stderr** (Finder gives `/dev/null`). A panic hook in `main` writes them to `/tmp/rapid-view-panic.log` instead. Check there first when diagnosing crash reports from real launches.
- **`#[cold] #[inline(never)]` on `flush_progress`** in both parsers is load-bearing — keeps the hot `advance()` loop tight. Don't drop those attributes.

## Selectors and chrome

ObjC selectors on `RVAppDelegate`: `rvNewWindow:`, `rvOpenDocument:`, `rvTogglePrettify:`, `rvPaste:`, `rvClearDocument:`, `rvCopyPath:`, `rvCopySubtree:`, `rvShowSearch:`, `rvSearchNext:`, `rvSearchPrev:`, `rvDismissSearch:`, `rvSearchFieldAction:`, `rvSearchChanged:`, `rvWorkerTick:`.

Toolbar buttons "Copy jq" / "Copy XPath" / "Copy Path" and "Copy JSON" / "Copy XML" / "Copy Markdown" have their titles updated by `refresh_format_chrome` when a document loads.

## Fixtures

- `fixtures/sample.json`, `fixtures/sample.xml`, `fixtures/sample.md` — small clean inputs.
- `fixtures/reddit.json`, `fixtures/large.json` — bigger inputs.
- `~/Downloads/apple_health_export/export.xml` (1.4 GB) — a known stress test on Roshan's machine, not in the repo.

## Related KB

- `KB:RAPIDVIEW:docs/overview` — longer-form project overview in Taskmaster KB, fetched via the `taskmaster-kb` skill.
