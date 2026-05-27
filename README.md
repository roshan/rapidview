# Rapid View · Markview

A Cargo workspace with two native macOS viewer apps written in Rust against AppKit (`objc2`). No Electron, no web view.

- **Rapid View** — JSON / XML viewer. Click anywhere → header shows the jq path (JSON) or XPath (XML). Auto-detects format. Handles multi-GB files (the 1.4 GB Apple Health export, for instance) without freezing. ⌘F search, vim-style navigation (`hjkl`, `gg`, `G`, `/`, `n`, `N`), prettify toggle. Determinate progress bar in the header during loads.
- **Markview** — Markdown viewer with a rendered/source toggle. Both views are real `NSTextView`s, so selection, copy, and the system Find bar work the way macOS users expect. README-quality typography (headings, bold/italic, inline code, fenced code, lists, blockquotes, clickable links); tables render as monospace pre blocks.
- **markdown-core** — Internal lib crate. Markdown structure parser used by Markview.

## Build and run

```sh
cargo run -p rapid-view -- path/to/file.json   # Rapid View, debug
cargo run -p markview   -- path/to/file.md     # Markview, debug
cargo test --workspace                          # unit + integration tests
mise run deploy                                 # install Rapid View.app
mise run deploy-markview                        # install Markview.app
mise run deploy-all                             # both
```

## Project notes

See [CLAUDE.md](./CLAUDE.md) for the file layout, the format-agnostic-renderer pattern, conventions, build/test/deploy specifics, and gotchas. It's written as briefing material for the AI agent that maintains this codebase, but it's the most useful map of the codebase for any reader.
