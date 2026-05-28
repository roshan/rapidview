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
- GitHub-flavoured tables with per-column alignment

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

Tables lay out as real columns using `NSTextTable`. The separator row
controls per-column alignment: `:---` left, `---:` right, `:---:`
centred. Inline marks like **bold**, *italic*, `code`, and
[links](https://example.com) still apply inside cells.

| Name   | Count | Notes                |
| :----- | ----: | :------------------: |
| alpha  |     1 | first **letter**     |
| beta   |    20 | `greek`              |
| gamma  |   300 | [link](https://x.y)  |

---

## Limitations

Images, footnotes, math, and task list checkboxes are not handled in
this version. Setext-style headings (`===` / `---` underlines) are
also skipped — only ATX headings (`#` through `######`) build the
section tree.
