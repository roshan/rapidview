//! Rendered view — build a styled `NSAttributedString` from markdown
//! bytes + the structure parse from markdown-core.
//!
//! Block-level layout is driven by the parser's `BlockKind`
//! classification per source line. Inline marks (`code`, `**bold**`,
//! `*italic*`, `[text](url)`) are scanned here by a small state
//! machine — they aren't tracked by the parser because the source view
//! doesn't need them.
//!
//! Fenced code blocks render as monospace pre blocks with a soft
//! background. Tables lay out as real columns via `NSTextTable`, with
//! per-column alignment driven by the `| :--- | :---: | ---: |`
//! separator row when present.

use markdown_core::{BlockKind, BlockLine, CellAlign, ParseOutput};
use objc2::AnyThread;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSBackgroundColorAttributeName, NSColor, NSFont, NSFontAttributeName, NSFontManager,
    NSFontTraitMask, NSFontWeightBold, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSLinkAttributeName, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSTextAlignment, NSTextBlock, NSTextBlockDimension,
    NSTextBlockLayer,
    NSTextBlockValueType, NSTextTab, NSTextTabType, NSTextTable, NSTextTableBlock,
    NSTextTableLayoutAlgorithm, NSUnderlineStyle, NSUnderlineStyleAttributeName,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSNumber, NSRectEdge,
    NSString,
};

/// Base sizes at zoom 1.0. Everything below is multiplied by the
/// caller-supplied `scale` so ⌘+/⌘− reflow the whole document rather
/// than bitmap-magnifying it.
const BODY_SIZE: f64 = 15.0;
const MONO_SIZE: f64 = 13.0;

/// Line height as a multiple of the font's natural height. Body text
/// gets a generous measure; code stays tighter so blocks read as one.
const BODY_LINE_HEIGHT: f64 = 1.35;
const CODE_LINE_HEIGHT: f64 = 1.25;

/// Paragraph-style geometry (at zoom 1.0). Kept in one place so the
/// rendered output stays consistent across block kinds.
const BLOCK_SPACING: f64 = 12.0;
const HEADING_SPACING_BEFORE: f64 = 22.0;
const HEADING_SPACING_AFTER: f64 = 8.0;
const LIST_INDENT: f64 = 26.0;
const LIST_NEST_INDENT: f64 = 22.0;
const LIST_ITEM_SPACING: f64 = 3.0;
const QUOTE_INDENT: f64 = 14.0;
const QUOTE_RULE_WIDTH: f64 = 3.0;
const PRE_PADDING: f64 = 10.0;
const HR_SPACING: f64 = 16.0;

pub fn build(
    mtm: MainThreadMarker,
    bytes: &[u8],
    parse: &ParseOutput,
    scale: f64,
) -> Retained<NSAttributedString> {
    let s = std::str::from_utf8(bytes).unwrap_or("");
    let lines = slice_lines(s, parse);
    let b = Builder::new(mtm, scale);

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
                // Consecutive source lines are one markdown paragraph:
                // reflow them as a single paragraph so the text wraps
                // to the column instead of to the author's editor.
                let end = run_end(parse.blocks.as_slice(), i, |k| {
                    matches!(k, BlockKind::Paragraph)
                });
                let joined = join_soft_lines((i..end).map(|k| {
                    let li = parse.blocks[k].line_index as usize;
                    lines.get(li).copied().unwrap_or("")
                }));
                b.emit_paragraph(&joined);
                i = end;
                continue;
            }
            BlockKind::BlockquoteLine => {
                let end = run_end(parse.blocks.as_slice(), i, |k| {
                    matches!(k, BlockKind::BlockquoteLine)
                });
                let joined = join_soft_lines((i..end).map(|k| {
                    let li = parse.blocks[k].line_index as usize;
                    strip_blockquote(lines.get(li).copied().unwrap_or(""))
                }));
                b.emit_blockquote(&joined);
                i = end;
                continue;
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
                b.emit_table(&run_lines);
                i = end;
                continue;
            }
        }
        i += 1;
    }

    Retained::into_super(b.out)
}

/// Join the lines of one logical paragraph. A soft break becomes a
/// space; a markdown hard break (two-plus trailing spaces or a trailing
/// backslash) becomes U+2028 LINE SEPARATOR, which `NSTextView` renders
/// as a line break inside the same paragraph.
fn join_soft_lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    let mut pending_break: Option<char> = None;
    for line in lines {
        let trimmed_end = line.trim_end();
        let content = trimmed_end.trim_start();
        if let Some(sep) = pending_break.take() {
            out.push(sep);
        }
        let hard = line.len() - trimmed_end.len() >= 2 || content.ends_with('\\');
        let content = if content.ends_with('\\') {
            &content[..content.len() - 1]
        } else {
            content
        };
        out.push_str(content);
        pending_break = Some(if hard { '\u{2028}' } else { ' ' });
    }
    out
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
    scale: f64,
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
    table_border_color: Retained<NSColor>,
    table_header_bg: Retained<NSColor>,
}

impl Builder {
    fn new(mtm: MainThreadMarker, scale: f64) -> Self {
        let body_font = NSFont::systemFontOfSize(BODY_SIZE * scale);
        let bold_font = NSFont::systemFontOfSize_weight(BODY_SIZE * scale, unsafe {
            NSFontWeightSemibold
        });
        let mono_font = NSFont::monospacedSystemFontOfSize_weight(MONO_SIZE * scale, unsafe {
            NSFontWeightRegular
        });
        let manager = NSFontManager::sharedFontManager(mtm);
        let italic_font =
            manager.convertFont_toHaveTrait(&body_font, NSFontTraitMask::ItalicFontMask);
        let bold_italic_font =
            manager.convertFont_toHaveTrait(&bold_font, NSFontTraitMask::ItalicFontMask);

        let out = NSMutableAttributedString::new();

        Self {
            out,
            scale,
            body_font,
            mono_font,
            italic_font,
            bold_font,
            bold_italic_font,
            text_color: NSColor::textColor(),
            secondary_color: NSColor::secondaryLabelColor(),
            code_bg: NSColor::colorWithCalibratedRed_green_blue_alpha(0.50, 0.50, 0.50, 0.16),
            pre_bg: NSColor::colorWithCalibratedRed_green_blue_alpha(0.50, 0.50, 0.50, 0.10),
            quote_rule_color: NSColor::colorWithCalibratedRed_green_blue_alpha(
                0.50, 0.50, 0.50, 0.55,
            ),
            link_color: NSColor::linkColor(),
            table_border_color: NSColor::colorWithCalibratedRed_green_blue_alpha(
                0.50, 0.50, 0.50, 0.45,
            ),
            table_header_bg: NSColor::colorWithCalibratedRed_green_blue_alpha(
                0.50, 0.50, 0.50, 0.15,
            ),
        }
    }

    /// Scale a zoom-1.0 length to the current zoom.
    fn sz(&self, v: f64) -> f64 {
        v * self.scale
    }

    fn pstyle(
        &self,
        first_indent: f64,
        head_indent: f64,
        spacing_before: f64,
        spacing_after: f64,
    ) -> Retained<NSMutableParagraphStyle> {
        let p = paragraph_style(
            self.sz(first_indent),
            self.sz(head_indent),
            self.sz(spacing_before),
            self.sz(spacing_after),
        );
        p.setLineHeightMultiple(BODY_LINE_HEIGHT);
        p
    }

    /// A standalone (non-table) `NSTextBlock` spanning the full column.
    /// Without an explicit width a bare block shrinks to its content and
    /// wraps one glyph per line.
    fn full_width_block(&self) -> Retained<NSTextBlock> {
        let block = NSTextBlock::new();
        block.setValue_type_forDimension(
            100.0,
            NSTextBlockValueType::PercentageValueType,
            NSTextBlockDimension::Width,
        );
        block
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
        let size = heading_font_size(level) * self.scale;
        let weight = if level <= 2 {
            unsafe { NSFontWeightBold }
        } else {
            unsafe { NSFontWeightSemibold }
        };
        let font = NSFont::systemFontOfSize_weight(size, weight);
        // Top-level headings get a little more air above; deeper ones
        // sit closer to the paragraph they introduce.
        let before = if level <= 2 { HEADING_SPACING_BEFORE * 1.2 } else { HEADING_SPACING_BEFORE };
        let pstyle = self.pstyle(0.0, 0.0, before, HEADING_SPACING_AFTER);
        pstyle.setLineHeightMultiple(1.15);
        let attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*font),
            (unsafe { NSForegroundColorAttributeName }, &*self.text_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append(stripped, &attrs);
        self.append("\n", &attrs);
    }

    fn emit_paragraph(&self, raw: &str) {
        let pstyle = self.pstyle(0.0, 0.0, 0.0, BLOCK_SPACING);
        self.render_inline(raw, &pstyle, BaseStyle::Body);
        let trailing_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &trailing_attrs);
    }

    fn emit_blockquote(&self, stripped: &str) {
        // `> ` prefixes already stripped and lines joined by the caller;
        // render in muted text behind a left rule drawn as an
        // `NSTextBlock` border.
        let pstyle = self.pstyle(0.0, 0.0, 2.0, BLOCK_SPACING);
        let block = self.full_width_block();
        block.setWidth_type_forLayer_edge(
            self.sz(QUOTE_RULE_WIDTH),
            NSTextBlockValueType::AbsoluteValueType,
            NSTextBlockLayer::Border,
            NSRectEdge::MinX,
        );
        block.setWidth_type_forLayer_edge(
            self.sz(QUOTE_INDENT),
            NSTextBlockValueType::AbsoluteValueType,
            NSTextBlockLayer::Padding,
            NSRectEdge::MinX,
        );
        block.setBorderColor(Some(&self.quote_rule_color));
        pstyle.setTextBlocks(&NSArray::from_retained_slice(&[block]));
        self.render_inline(stripped, &pstyle, BaseStyle::Quote);
        let trailing_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.secondary_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &trailing_attrs);
    }

    fn emit_list_item(&self, raw: &str, ordered: bool, marker_len: usize) {
        // Nesting depth from leading whitespace (2 or 4 spaces per level
        // both land on the same visual step; tabs count as one level).
        let leading = raw.len() - raw.trim_start().len();
        let depth = if leading == 0 {
            0
        } else {
            let tabs = raw[..leading].matches('\t').count();
            let spaces = leading - tabs;
            (tabs + (spaces + 1) / 3).max(1)
        } as f64;
        let trimmed = raw.trim_start();
        let marker_end = marker_len.saturating_sub(leading).min(trimmed.len());
        let body = trimmed.get(marker_end..).unwrap_or("").trim_start();
        let prefix = if ordered {
            // Preserve the numeric marker as given.
            let raw_marker: String = trimmed.chars().take_while(|c| !c.is_whitespace()).collect();
            format!("{}\t", raw_marker)
        } else {
            "•\t".to_string()
        };
        // Hanging indent: marker sits in the gutter, body text (and any
        // wrapped continuation) aligns at `text_x` via a tab stop.
        let gutter = depth * LIST_NEST_INDENT;
        let text_x = gutter + LIST_INDENT;
        let pstyle = self.pstyle(gutter + 4.0, text_x, 0.0, LIST_ITEM_SPACING);
        let tab = NSTextTab::initWithType_location(
            NSTextTab::alloc(),
            NSTextTabType::LeftTabStopType,
            self.sz(text_x),
        );
        pstyle.setTabStops(Some(&NSArray::from_retained_slice(&[tab])));
        let prefix_attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.text_color),
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
        // A near-empty paragraph whose text block draws a hairline top
        // border — a real full-width rule, not a row of glyphs.
        let pstyle = self.pstyle(0.0, 0.0, HR_SPACING, HR_SPACING);
        pstyle.setLineHeightMultiple(1.0);
        let block = self.full_width_block();
        block.setWidth_type_forLayer_edge(
            1.0,
            NSTextBlockValueType::AbsoluteValueType,
            NSTextBlockLayer::Border,
            NSRectEdge::MinY,
        );
        block.setBorderColor(Some(&self.quote_rule_color));
        pstyle.setTextBlocks(&NSArray::from_retained_slice(&[block]));
        let tiny = NSFont::systemFontOfSize(1.0);
        let attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*tiny),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        self.append("\n", &attrs);
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

    fn emit_table(&self, lines: &[&str]) {
        // Parse cells per row. If the second row is a valid separator
        // (`| :--- | ---: |`), treat the first row as a header and use
        // its alignments. Otherwise render every line as a body row with
        // left alignment.
        let rows: Vec<Vec<String>> = lines
            .iter()
            .map(|l| markdown_core::split_table_row(l))
            .collect();
        if rows.is_empty() || rows.iter().all(|r| r.is_empty()) {
            return;
        }
        let (has_header, aligns, body_start) = if rows.len() >= 2 {
            if let Some(a) = markdown_core::parse_table_separator(lines[1]) {
                (true, a, 2)
            } else {
                (false, Vec::new(), 0)
            }
        } else {
            (false, Vec::new(), 0)
        };
        let mut ncols = aligns.len();
        for row in &rows {
            ncols = ncols.max(row.len());
        }
        if ncols == 0 {
            return;
        }
        let mut aligns = aligns;
        aligns.resize(ncols, CellAlign::Left);

        let table = NSTextTable::new();
        table.setNumberOfColumns(ncols);
        table.setLayoutAlgorithm(NSTextTableLayoutAlgorithm::AutomaticLayoutAlgorithm);
        table.setCollapsesBorders(true);
        table.setHidesEmptyCells(false);

        let total_rows = if has_header {
            1 + rows.len().saturating_sub(body_start)
        } else {
            rows.len()
        };

        let mut row_idx: usize = 0;
        if has_header {
            self.emit_table_row(&table, &rows[0], ncols, &aligns, row_idx, true, total_rows);
            row_idx += 1;
        }
        for body in rows.iter().skip(body_start) {
            self.emit_table_row(&table, body, ncols, &aligns, row_idx, false, total_rows);
            row_idx += 1;
        }
    }

    fn emit_table_row(
        &self,
        table: &NSTextTable,
        cells: &[String],
        ncols: usize,
        aligns: &[CellAlign],
        row: usize,
        header: bool,
        total_rows: usize,
    ) {
        let is_last_row = row + 1 == total_rows;
        for c in 0..ncols {
            let cell_text = cells.get(c).map(String::as_str).unwrap_or("");
            let block = NSTextTableBlock::initWithTable_startingRow_rowSpan_startingColumn_columnSpan(
                NSTextTableBlock::alloc(),
                table,
                row as isize,
                1,
                c as isize,
                1,
            );
            block.setWidth_type_forLayer_edge(
                1.0,
                NSTextBlockValueType::AbsoluteValueType,
                NSTextBlockLayer::Border,
                NSRectEdge::MinX,
            );
            block.setWidth_type_forLayer_edge(
                1.0,
                NSTextBlockValueType::AbsoluteValueType,
                NSTextBlockLayer::Border,
                NSRectEdge::MinY,
            );
            block.setWidth_type_forLayer_edge(
                if c + 1 == ncols { 1.0 } else { 0.0 },
                NSTextBlockValueType::AbsoluteValueType,
                NSTextBlockLayer::Border,
                NSRectEdge::MaxX,
            );
            block.setWidth_type_forLayer_edge(
                if is_last_row { 1.0 } else { 0.0 },
                NSTextBlockValueType::AbsoluteValueType,
                NSTextBlockLayer::Border,
                NSRectEdge::MaxY,
            );
            block.setWidth_type_forLayer(
                self.sz(7.0),
                NSTextBlockValueType::AbsoluteValueType,
                NSTextBlockLayer::Padding,
            );
            block.setBorderColor(Some(&self.table_border_color));
            if header {
                block.setBackgroundColor(Some(&self.table_header_bg));
            }

            let pstyle = NSMutableParagraphStyle::new();
            pstyle.setAlignment(ns_alignment(aligns[c]));
            // Tight vertical rhythm inside cells.
            pstyle.setParagraphSpacing(0.0);
            pstyle.setParagraphSpacingBefore(0.0);
            pstyle.setLineHeightMultiple(1.25);
            let super_block: Retained<NSTextBlock> = Retained::into_super(block);
            let blocks_array = NSArray::from_retained_slice(&[super_block]);
            pstyle.setTextBlocks(&blocks_array);

            let base = if header {
                BaseStyle::HeaderCell
            } else {
                BaseStyle::Body
            };
            self.render_inline(cell_text, &pstyle, base);
            let trailing_font: &NSFont = if header { &self.bold_font } else { &self.body_font };
            let trailing_attrs = attrs_for(&[
                (unsafe { NSFontAttributeName }, trailing_font),
                (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
            ]);
            self.append("\n", &trailing_attrs);
        }
    }

    fn emit_pre_block(&self, lines: &[&str]) {
        // The whole block is ONE paragraph: lines joined by U+2028 LINE
        // SEPARATOR so a single `NSTextBlock` paints one flush background
        // rectangle (per-line paragraphs leave hairline seams between
        // their backgrounds). `MVTextView` turns U+2028 back into `\n`
        // on copy so pasted code is unchanged.
        let pstyle = self.pstyle(0.0, 0.0, 0.0, BLOCK_SPACING);
        let line_h = (MONO_SIZE * self.scale * CODE_LINE_HEIGHT * 1.2).round();
        pstyle.setLineHeightMultiple(1.0);
        pstyle.setMinimumLineHeight(line_h);
        pstyle.setMaximumLineHeight(line_h);
        let block = self.full_width_block();
        block.setWidth_type_forLayer(
            self.sz(PRE_PADDING),
            NSTextBlockValueType::AbsoluteValueType,
            NSTextBlockLayer::Padding,
        );
        block.setBackgroundColor(Some(&self.pre_bg));
        pstyle.setTextBlocks(&NSArray::from_retained_slice(&[block]));
        let attrs = attrs_for(&[
            (unsafe { NSFontAttributeName }, &*self.mono_font),
            (unsafe { NSForegroundColorAttributeName }, &*self.text_color),
            (unsafe { NSParagraphStyleAttributeName }, &*pstyle),
        ]);
        let joined = lines.join("\u{2028}");
        self.append(&joined, &attrs);
        self.append("\n", &attrs);
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
    Quote,
    HeaderCell,
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
        BaseStyle::Quote => attrs_for(&[
            (unsafe { NSFontAttributeName }, &*b.body_font),
            (unsafe { NSForegroundColorAttributeName }, &*b.secondary_color),
            (unsafe { NSParagraphStyleAttributeName }, pstyle),
        ]),
        BaseStyle::HeaderCell => attrs_for(&[
            (unsafe { NSFontAttributeName }, &*b.bold_font),
            (unsafe { NSForegroundColorAttributeName }, &*b.text_color),
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
        BaseStyle::Body | BaseStyle::HeaderCell | BaseStyle::Quote => &b.bold_font,
    };
    let color: &NSColor = match base {
        BaseStyle::Body | BaseStyle::HeaderCell => &b.text_color,
        BaseStyle::Quote => &b.secondary_color,
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
        BaseStyle::Body | BaseStyle::Quote => &b.italic_font,
        BaseStyle::HeaderCell => &b.bold_italic_font,
    };
    let color: &NSColor = match base {
        BaseStyle::Body | BaseStyle::HeaderCell => &b.text_color,
        BaseStyle::Quote => &b.secondary_color,
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
    let no_underline = NSNumber::new_isize(NSUnderlineStyle::None.0 as isize);
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
        no_underline.as_ref() as &AnyObject,
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

fn ns_alignment(a: CellAlign) -> NSTextAlignment {
    match a {
        CellAlign::Left => NSTextAlignment::Left,
        CellAlign::Center => NSTextAlignment::Center,
        CellAlign::Right => NSTextAlignment::Right,
    }
}

fn heading_font_size(level: u32) -> f64 {
    match level {
        1 => 30.0,
        2 => 23.0,
        3 => 19.0,
        4 => 17.0,
        5 => 15.5,
        _ => 15.0,
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
