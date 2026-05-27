# Rapid View

A native macOS viewer for JSON, XML, and Markdown files. Written in Rust against AppKit (`objc2`). No Electron, no web view.

- Click anywhere → header shows the jq path (JSON), XPath (XML), or heading path (Markdown) that points at it.
- Auto-detects JSON vs XML from the file content; Markdown is selected by extension (`.md`, `.markdown`, `.mdown`, `.mkd`).
- Handles multi-GB files (the 1.4 GB Apple Health export, for instance) without freezing.
- ⌘F search, vim-style navigation (`hjkl`, `gg`, `G`, `/`, `n`, `N`), prettify toggle.
- Determinate progress bar in the header during loads — doesn't push the document around.

## Build and run

```sh
cargo run -- path/to/file.json   # debug, against a file
cargo test                       # unit + integration tests
mise run deploy                  # build, ad-hoc sign, install to /Applications
```

## Project notes

See [CLAUDE.md](./CLAUDE.md) for the file layout, the format-agnostic-renderer pattern, conventions, build/test/deploy specifics, and gotchas. It's written as briefing material for the AI agent that maintains this codebase, but it's the most useful map of the codebase for any reader.
