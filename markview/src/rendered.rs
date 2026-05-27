//! Rendered view — build a styled `NSAttributedString` from markdown
//! bytes + the structure parse from markdown-core.
//!
//! Block-level layout is driven by the parser's `BlockKind`
//! classification per source line. Inline marks (`code`, `**bold**`,
//! `*italic*`, `[text](url)`) are scanned here by a small state
//! machine — they aren't tracked by the parser because the source view
//! doesn't need them.
//!
//! Tables and fenced code blocks both render as monospace pre blocks
//! with a soft background. Tables aren't laid out as real columns —
//! the source bytes are echoed verbatim and rely on the author having
//! aligned them, which is how everyone writes markdown tables anyway.

use markdown_core::{BlockKind, BlockLine, ParseOutput};
use objc2::AnyThread;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSBackgroundColorAttributeName, NSColor, NSFont, NSFontAttributeName, NSFontManager,
    NSFontTraitMask, NSForegroundColorAttributeName, NSLinkAttributeName,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSUnderlineStyle,
    NSUnderlineStyleAttributeName,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSNumber, NSString,
};

const BODY_SIZE: f64 = 14.0;
const MONO_SIZE: f64 = 13.0;

/// Paragraph-style geometry. Kept in one place so the rendered output
/// stays consistent across block kinds.
const BLOCK_SPACING: f64 = 8.0;
const HEADING_SPACING_BEFORE: f64 = 16.0;
const HEADING_SPACING_AFTER: f64 = 6.0;
const LIST_INDENT: f64 = 24.0;
const QUOTE_INDENT: f64 = 16.0;
const PRE_INDENT: f64 = 12.0;

pub fn build(
    mtm: MainThreadMarker,
    bytes: &[u8],
    parse: &ParseOutput,
) -> Retained<NSAttributedString> {
    let s = std::str::from_utf8(bytes).unwrap_or("");
    let lines = slice_lines(s, parse);
    let b = Builder::new(mtm);

    let mut i = 0;
    while i < parse.blocks.len() {
        let block = parse.blocks[i];
        let line = lines.get(block.line_index as usize).copied().unwrap_or("");
        match block.kind {
            BlockKind::Blank => {
                b.append_newline_default();
            }
            BlockKind::Heading { level } => {
                b.emit_heading(line, level);
            }
            BlockKind::Paragraph => {
                b.emit_paragraph(line);
            }
            BlockKind::BlockquoteLine => {
                b.emit_blockquote(line);
            }
            BlockKind::ListItem {
                ordered,
                marker_len,
            } => {
                b.emit_list_item(line, ordered, marker_len as usize);
            }
            BlockKind::HorizontalRule => {
                b.emit_hr();
            }
            BlockKind::FencedCode => {
                let end = run_end(parse.blocks.as_slice(), i, |k| {
                    matches!(k, BlockKind::FencedCode)
                });
                let run_lines: Vec<&str> = (i..end)
                    .map(|k| {
                        let li = parse.blocks[k].line_index as usize;
                        lines.get(li).copied().unwrap_or("")
                    })
                    .collect();
                b.emit_fenced_code(&run_lines);
                i = end;
                continue;
            }
            BlockKind::TableLine => {
                let end = run_end(parse.blocks.as_slice(), i, |k| {
                    matches!(k, BlockKind::TableLine)
                });
                let run_lines: Vec<&str> = (i..end)
                    .map(|k| {
                        let li = parse.blocks[k].line_index as usize;
                        lines.get(li).copied().unwrap_or("")
                    })
                    .collect();
                b.emit_pre_block(&run_lines);
                i = end;
                continue;
            }
        }
        i += 1;
    }

    Retained::into_super(b.out)
}

fn run_end(blocks: &[BlockLine], start: usize, mut matches_kind: impl FnMut(BlockKind) -> bool) -> usize {
    let mut j = start;
    while j < blocks.len() && matches_kind(blocks[j].kind) {
        j += 1;
    }
    j
}

/// Slice the source string into &str per line index. line_starts are
/// byte offsets; markdown is UTF-8 and the parser only branches on
/// ASCII so the offsets always land at character boundaries.
fn slice_lines<'a>(s: &'a str, parse: &ParseOutput) -> Vec<&'a str> {
    let bytes = s.as_bytes();
    let starts = &parse.line_starts;
    let mut out = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end_excl_nl = if i + 1 < starts.len() {
            (starts[i + 1] as usize).saturating_sub(1)
        } else {
            bytes.len()
        };
        let mut end = end_excl_nl.min(bytes.len());
        if end > start as usize && bytes.get(end.saturating_sub(1)) == Some(&b'\r') {
            end -= 1;
        }
        let slice = &s[start as usize..end];
        out.push(slice);
    }
    out
}

// ----------------------------------------------------------------------

struct Builder {
    out: Retained<NSMutableAttributedString>,
    body_font: Retained<NSFont>,
    mono_font: Retained<NSFont>,
    italic_font: Retained<NSFont>,
    bold_font: Retained<NSFont>,
    bold_italic_font: Retained<NSFont>,
    text_color: Retained<NSColor>,
    secondary_color: Retained<NSColor>,
    code_bg: Retained<NSColor>,
    pre_bg: Retained<NSColor>,
    quote_rule_color: Retained<NSColor>,
    link_color: Retained<NSColor>,
}

impl Builder {
    fn new(mtm: MainThreadMarker) -> Self {
        let body_font = NSFont::systemFontOfSize(BODY_SIZE);
        let bold_font = NSFont::boldSystemFontOfSize(BODY_SIZE);
        let mono_font = NSFont::userFixedPitchFontOfSize(MONO_SIZE)
            .expect("user fixed-pitch font is always available");
        let manager = NSFontManager::sharedFontManager(mtm);
        let italic_font =
            manager.convertFont_toHaveTrait(&body_font, NSFontTraitMask::ItalicFontMask);
        let bold_italic_font =
            manager.convertFont_toHaveTrait(&bold_font, NSFontTraitMask::ItalicFontMask);

        let out = NSMutableAttributedString::new();

        Self {
            out,
            body_font,
            mono_font,
            italic_font,
            bold_font,
            bold_italic_font,
            text_color: NSColor::textColor(),
            secondary_color: NSColor::secondaryLabelColor(),
            code_bg: NSColor::colorWithCalibratedRed_green_blue_alpha(0.50, 0.50, 0.50, 0.18),
            pre_bg: NSColor::colorWithCalibratedRed_green_blue_alpha(0.50, 0.50, 0.50, 0.10),
            quote_rule_color: NSColor::tertiaryLabelColor(),
            link_color: NSColor::linkColor(),
        }
    }

    fn append(&self, text: &str, attrs: &NSDictionary<NSString>) {
        if text.is_empty() {
            return;
        }
        let ns = NSString::from_str(text);
        let s = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &ns,
                Some(attrs),
            )
        };
        self.out.appendAttributedString(&s);
    }

    fn append_newline_default(&self) {
        let attrs = attrs_for(&[(unsafe { NSFontAttributeName }, &*self.body_font)]);
        self.append("\n", &attrs);
    }

    // -- block emitters ---------------------------------------------

    fn emit_heading(&self, raw: &str, level: u32) {
        let stripped = strip_heading(raw);
        let size = heading_font_size(level);
        let font = NSFont::boldSystemFontOfSize(size);
        let pstyle = paragraph_style(0.0, 0.0, HEADING_SPACING_BEFORE, HEADING_SPACING_AFTER);
        let attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*font),
            (unsafe { NSForegroundColorAttributeName }, &*self.text_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append(stripped, &attrs);
        self.append("\n", &attrs);
    }

    fn emit_paragraph(&self, raw: &str) {
        let pstyle = paragraph_style(0.0, 0.0, 0.0, BLOCK_SPACING);
        self.render_inline(raw, &pstyle, BaseStyle::Body);
        let trailing_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &trailing_attrs);
    }

    fn emit_blockquote(&self, raw: &str) {
        // `> ` (or `>`) prefix stripped; render remainder as italic with indent.
        let stripped = strip_blockquote(raw);
        let pstyle = paragraph_style(QUOTE_INDENT, QUOTE_INDENT, 0.0, BLOCK_SPACING);
        self.render_inline(stripped, &pstyle, BaseStyle::QuoteItalic);
        let trailing_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.italic_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.secondary_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &trailing_attrs);
    }

    fn emit_list_item(&self, raw: &str, ordered: bool, marker_len: usize) {
        let body = raw.get(marker_len..).unwrap_or("").trim_start();
        let prefix = if ordered {
            // Preserve the numeric marker as given.
            let raw_marker: String = raw.chars().take_while(|c| !c.is_whitespace()).collect();
            format!("{} ", raw_marker)
        } else {
            "• ".to_string()
        };
        let pstyle = paragraph_style(LIST_INDENT, LIST_INDENT, 0.0, 2.0);
        let prefix_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.secondary_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append(&prefix, &prefix_attrs);
        self.render_inline(body, &pstyle, BaseStyle::Body);
        let trailing_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &trailing_attrs);
    }

    fn emit_hr(&self) {
        let pstyle = paragraph_style(0.0, 0.0, BLOCK_SPACING, BLOCK_SPACING);
        let attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.quote_rule_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("──────────────────────────────────────\n", &attrs);
    }

    fn emit_fenced_code(&self, lines: &[&str]) {
        // Hide the open + close fence lines, keep the content. Falls
        // back to verbatim if either end doesn't look like a fence.
        let start = if lines.first().map(|l| looks_like_fence(l)).unwrap_or(false) {
            1
        } else {
            0
        };
        let end = if lines.len() > start
            && lines.last().map(|l| looks_like_fence(l)).unwrap_or(false)
        {
            lines.len() - 1
        } else {
            lines.len()
        };
        let inner = &lines[start..end];
        self.emit_pre_block(inner);
    }

    fn emit_pre_block(&self, lines: &[&str]) {
        // One paragraph per line so a long block can wrap-or-not by
        // line. Spacing only on the last line for visual grouping.
        let last = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            let spacing_after = if i == last { BLOCK_SPACING } else { 0.0 };
            let pstyle = paragraph_style(PRE_INDENT, PRE_INDENT, 0.0, spacing_after);
            let attrs = attrs_for(&[
                (unsafe { NSFontAttributeName }, &*self.mono_font),
                (unsafe { NSForegroundColorAttributeName }, &*self.text_color),
                (unsafe { NSBackgroundColorAttributeName }, &*self.pre_bg),
                (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
            ]);
            // Pad the line with a trailing space so the background
            // colour extends a touch past short lines, matching how
            // people expect `<pre>` to look.
            let padded = format!("{} ", line);
            self.append(&padded, &attrs);
            self.append("\n", &attrs);
        }
    }

    // -- inline parser ----------------------------------------------

    fn render_inline(
        &self,
        line: &str,
        pstyle: &NSMutableParagraphStyle,
        base: BaseStyle,
    ) {
        let bytes = line.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        let mut text_start = 0;

        // Flush accumulated [text_start..i] as plain text in the base style.
        let flush = |this: &Builder, from: usize, to: usize| {
            if to > from {
                let slice = &line[from..to];
                this.append(slice, &base_attrs(this, pstyle, base));
            }
        };

        while i < n {
            let b = bytes[i];
            match b {
                b'`' => {
                    if let Some(end) = find_match_byte(bytes, i + 1, b'`') {
                        flush(self, text_start, i);
                        let inner = &line[i + 1..end];
                        self.append(inner, &inline_code_attrs(self, pstyle));
                        i = end + 1;
                        text_start = i;
                        continue;
                    }
                }
                b'*' | b'_' => {
                    if bytes.get(i + 1).copied() == Some(b) {
                        // Bold: **...** or __...__
                        if let Some(end) = find_double_marker(bytes, i + 2, b) {
                            flush(self, text_start, i);
                            let inner = &line[i + 2..end];
                            self.append(inner, &bold_attrs(self, pstyle, base));
                            i = end + 2;
                            text_start = i;
                            continue;
                        }
                    } else {
                        // Italic: *...* or _...
                        if let Some(end) = find_match_byte(bytes, i + 1, b) {
                            flush(self, text_start, i);
                            let inner = &line[i + 1..end];
                            self.append(inner, &italic_attrs(self, pstyle, base));
                            i = end + 1;
                            text_start = i;
                            continue;
                        }
                    }
                }
                b'[' => {
                    if let Some((text_end, url_start, url_end)) = parse_link(bytes, i) {
                        flush(self, text_start, i);
                        let link_text = &line[i + 1..text_end];
                        let url = &line[url_start..url_end];
                        self.append(link_text, &link_attrs(self, pstyle, url));
                        i = url_end + 1;
                        text_start = i;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        flush(self, text_start, n);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BaseStyle {
    Body,
    QuoteItalic,
}

fn base_attrs(
    b: &Builder,
    pstyle: &NSMutableParagraphStyle,
    base: BaseStyle,
) -> Retained<NSDictionary<NSString>> {
    match base {
        BaseStyle::Body => attrs_for(&[
            (unsafe { NSFontAttributeName }, &*b.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*b.text_color),
            (unsafe { NSParagraphStyleAttributeName }, pstyle),
        ]),
        BaseStyle::QuoteItalic => attrs_for(&[
            (unsafe { NSFontAttributeName }, &*b.italic_font),
            (unsafe { NSForegroundColorAttributeName }, &*b.secondary_color),
            (unsafe { NSParagraphStyleAttributeName }, pstyle),
        ]),
    }
}

fn inline_code_attrs(
    b: &Builder,
    pstyle: &NSMutableParagraphStyle,
) -> Retained<NSDictionary<NSString>> {
    attrs_for(&[
        (unsafe { NSFontAttributeName }, &*b.mono_font),
        (unsafe { NSForegroundColorAttributeName }, &*b.text_color),
        (unsafe { NSBackgroundColorAttributeName }, &*b.code_bg),
        (unsafe { NSParagraphStyleAttributeName }, pstyle),
    ])
}

fn bold_attrs(
    b: &Builder,
    pstyle: &NSMutableParagraphStyle,
    base: BaseStyle,
) -> Retained<NSDictionary<NSString>> {
    let font: &NSFont = match base {
        BaseStyle::Body => &b.bold_font,
        BaseStyle::QuoteItalic => &b.bold_italic_font,
    };
    let color: &NSColor = match base {
        BaseStyle::Body => &b.text_color,
        BaseStyle::QuoteItalic => &b.secondary_color,
    };
    attrs_for(&[
        (unsafe { NSFontAttributeName }, font),
        (unsafe { NSForegroundColorAttributeName }, color),
        (unsafe { NSParagraphStyleAttributeName }, pstyle),
    ])
}

fn italic_attrs(
    b: &Builder,
    pstyle: &NSMutableParagraphStyle,
    base: BaseStyle,
) -> Retained<NSDictionary<NSString>> {
    let font: &NSFont = match base {
        BaseStyle::Body => &b.italic_font,
        BaseStyle::QuoteItalic => &b.italic_font, // already italic; keep
    };
    let color: &NSColor = match base {
        BaseStyle::Body => &b.text_color,
        BaseStyle::QuoteItalic => &b.secondary_color,
    };
    attrs_for(&[
        (unsafe { NSFontAttributeName }, font),
        (unsafe { NSForegroundColorAttributeName }, color),
        (unsafe { NSParagraphStyleAttributeName }, pstyle),
    ])
}

fn link_attrs(
    b: &Builder,
    pstyle: &NSMutableParagraphStyle,
    url: &str,
) -> Retained<NSDictionary<NSString>> {
    // NSTextView opens NSLinkAttributeName values on click. An NSString
    // works just as well as an NSURL for that, and avoids dealing with
    // URL-parse failures here.
    let url_ns = NSString::from_str(url);
    let underline = NSNumber::new_isize(NSUnderlineStyle::Single.0 as isize);
    let keys: [&NSString; 5] = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
            NSLinkAttributeName,
            NSUnderlineStyleAttributeName,
        ]
    };
    let values: [&AnyObject; 5] = [
        b.body_font.as_ref() as &AnyObject,
        b.link_color.as_ref() as &AnyObject,
        pstyle as &NSMutableParagraphStyle as &AnyObject,
        url_ns.as_ref() as &AnyObject,
        underline.as_ref() as &AnyObject,
    ];
    NSDictionary::from_slices(&keys, &values)
}

// Build an attribute dict from a slice of (key, value) pairs.
fn attrs_for(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString>> {
    let keys: Vec<&NSString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|p| p.1).collect();
    NSDictionary::from_slices(&keys, &values)
}

fn paragraph_style(
    first_indent: f64,
    head_indent: f64,
    spacing_before: f64,
    spacing_after: f64,
) -> Retained<NSMutableParagraphStyle> {
    let p = NSMutableParagraphStyle::new();
    p.setFirstLineHeadIndent(first_indent);
    p.setHeadIndent(head_indent);
    p.setParagraphSpacingBefore(spacing_before);
    p.setParagraphSpacing(spacing_after);
    p
}

// ----------------------------------------------------------------------

fn heading_font_size(level: u32) -> f64 {
    match level {
        1 => 26.0,
        2 => 22.0,
        3 => 18.0,
        4 => 16.0,
        5 => 14.0,
        _ => 13.0,
    }
}

fn strip_heading(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let mut end = bytes.len();
    while end > i && matches!(bytes[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    if end > i {
        let mut k = end;
        while k > i && bytes[k - 1] == b'#' {
            k -= 1;
        }
        if k < end && (k == i || matches!(bytes[k - 1], b' ' | b'\t')) {
            end = k;
            while end > i && matches!(bytes[end - 1], b' ' | b'\t') {
                end -= 1;
            }
        }
    }
    &raw[i..end]
}

fn strip_blockquote(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'>' {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
    }
    &raw[i..]
}

fn looks_like_fence(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
        if i >= 4 {
            return false;
        }
    }
    if i >= bytes.len() {
        return false;
    }
    let c = bytes[i];
    if c != b'`' && c != b'~' {
        return false;
    }
    let mut count = 0;
    while i < bytes.len() && bytes[i] == c {
        i += 1;
        count += 1;
    }
    count >= 3
}

fn find_match_byte(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_double_marker(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == target && bytes[i + 1] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Returns (text_end_excl, url_start, url_end_excl) on a `[text](url)`
/// match starting at `i` (which points at `[`). Lazy: doesn't handle
/// escaped brackets or balanced parens inside the URL.
fn parse_link(bytes: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut k = i + 1;
    let mut text_end = None;
    while k < bytes.len() && bytes[k] != b'\n' {
        if bytes[k] == b']' {
            text_end = Some(k);
            break;
        }
        k += 1;
    }
    let text_end = text_end?;
    if bytes.get(text_end + 1) != Some(&b'(') {
        return None;
    }
    let url_start = text_end + 2;
    let mut m = url_start;
    while m < bytes.len() && bytes[m] != b')' && bytes[m] != b'\n' {
        m += 1;
    }
    if bytes.get(m) != Some(&b')') {
        return None;
    }
    Some((text_end, url_start, m))
}
