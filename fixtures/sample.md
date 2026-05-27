# Rapid View Notes

A small markdown fixture for exercising the path tree and style spans.

## Getting Started

Open a file with `cargo run -- fixtures/sample.md`. Auto-detection
keys off the file extension because markdown has no reliable byte
signature.

### Build

```sh
cargo build --release
./bundle.sh release
```

### Run

```sh
cargo run -- fixtures/sample.md
```

## Reference

Headings define the section tree. Clicking inside a section's body
shows that section's heading path in the breadcrumb.

### Inline code

The toolbar reads `Copy Markdown` once a `.md` document is loaded,
mirroring `Copy JSON` and `Copy XML`.

### Fenced code

Fenced blocks (``` and ~~~) are styled as a single span so a stray
`# inside` doesn't get mistaken for a heading.

## Limitations

Emphasis, links, lists, and indented code blocks are not styled in
this version — they render as plain text. Folding sections is a
separate feature we plan to add later, on top of the heading tree
this parser produces.
