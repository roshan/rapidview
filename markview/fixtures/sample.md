# Markview

A native macOS viewer for Markdown that flips between **rendered**
output and the *raw source* in a monospace font. Both views support
text selection, copy, and find — they're real `NSTextView`s.

## Why

The same author writes JSON and XML viewers (Rapid View), and the
markdown viewer started life as a third format inside that app.
Rendered markdown wants a *very different* presentation, so it lives
in its own app sharing the structural parser via `markdown-core`.

## Features

- Headings sized by level (H1 → H6)
- Bold, italic, inline `code`, fenced code blocks
- Bullet (`- item`) and numbered (`1. item`) lists
- Blockquotes
- [Clickable links](https://example.com) (open in the default browser)
- Tables render as monospace pre blocks — no fancy column layout

## Code

Fenced code blocks render in monospace with a contrasting background:

```rust
fn main() {
    println!("hello, markview");
}
```

> Blockquotes are styled with a left rule and slightly muted text.
> Multi-line quotes continue across consecutive `>` lines.

## Tables

Tables are deliberately *not* laid out as real tables. They render
exactly as written, in monospace, which works well for
already-aligned source:

| left   | right   |
| ------ | ------- |
| alpha  | one     |
| beta   | two     |
| gamma  | three   |

---

## Limitations

Images, footnotes, math, and task list checkboxes are not handled in
this version. Setext-style headings (`===` / `---` underlines) are
also skipped — only ATX headings (`#` through `######`) build the
section tree.
