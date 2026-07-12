//! `DocView` — custom `NSView` that renders the open document line by
//! line. Format-agnostic: it just draws spans and looks up paths via
//! the shared `format` API. JSON shows jq paths in the breadcrumb; XML
//! shows XPath.
//!
//! Layout: fixed monospace font → constant line height and char
//! advance, so the document view is sized once
//! (`line_count * line_height` tall, `max_line_bytes * advance` wide)
//! and `drawRect:` reduces to picking a visible line range and drawing
//! each one with CoreText.

use crate::doc::Document;
use crate::format::{self, StyleKind, StyleSpan};
use objc2::rc::Retained;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSColor, NSEvent, NSEventModifierFlags, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSTextField, NSView,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSSize,
    NSString,
};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

const PAD_LEFT: f64 = 12.0;
const PAD_TOP: f64 = 8.0;
const PAD_RIGHT: f64 = 12.0;
const PAD_BOTTOM: f64 = 8.0;
const FONT_SIZE: f64 = 13.0;
/// AppKit starts tiling poorly around 2^24 pixels; we clamp the frame
/// width at ~8k monospace characters, well under that limit and still
/// larger than any realistic single line.
const MAX_LINE_BYTES_FOR_LAYOUT: f64 = 8_000.0;

/// Search match state. Byte offsets into the current document.
#[derive(Default)]
pub struct SearchState {
    pub matches: Vec<u32>,
    pub match_len: u32,
    pub current: usize,
}

pub struct DocViewIvars {
    doc: RefCell<Option<Arc<Document>>>,
    #[allow(dead_code)] // retained to keep the font alive for CoreText
    font: Retained<NSFont>,
    line_height: f64,
    #[allow(dead_code)]
    ascent: f64,
    advance: f64,
    default_attrs: Retained<NSDictionary<NSString>>,
    colors: Colors,
    last_click_offset: Cell<Option<u32>>,
    breadcrumb: RefCell<Option<Retained<NSTextField>>>,
    search: RefCell<SearchState>,
    /// CSV/TSV only: draw as an aligned table (true, the default) or as
    /// the raw source. Toggled by the window's Prettify button.
    csv_table: Cell<bool>,
}

struct Colors {
    #[allow(dead_code)]
    fg: Retained<NSColor>,
    bg: Retained<NSColor>,
    row_highlight: Retained<NSColor>,
    // JSON
    key: Retained<NSColor>,
    string: Retained<NSColor>,
    number: Retained<NSColor>,
    bool_: Retained<NSColor>,
    null: Retained<NSColor>,
    // XML
    tag: Retained<NSColor>,
    attr_name: Retained<NSColor>,
    attr_value: Retained<NSColor>,
    comment: Retained<NSColor>,
    // Search
    search_match: Retained<NSColor>,
    search_current: Retained<NSColor>,
}

impl Colors {
    fn for_kind(&self, k: StyleKind) -> &NSColor {
        match k {
            StyleKind::Key => &self.key,
            StyleKind::String => &self.string,
            StyleKind::Number => &self.number,
            StyleKind::Bool => &self.bool_,
            StyleKind::Null => &self.null,
            StyleKind::Tag => &self.tag,
            StyleKind::AttrName => &self.attr_name,
            StyleKind::AttrValue => &self.attr_value,
            StyleKind::Comment => &self.comment,
            // CDATA reads like a string literal; PI like a comment.
            StyleKind::CData => &self.string,
            StyleKind::Pi => &self.comment,
        }
    }
}

fn rgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, 1.0)
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RVDocView"]
    #[ivars = DocViewIvars]
    pub struct DocView;

    impl DocView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw_content(dirty);
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let flags = event.modifierFlags();
            let has_cmd = flags.contains(NSEventModifierFlags::Command);
            if has_cmd {
                return;
            }
            let Some(chars) = event.characters() else { return };
            let s = chars.to_string();
            let Some(raw) = event.charactersIgnoringModifiers() else { return };
            let raw_s = raw.to_string();
            match raw_s.as_str() {
                "\u{f702}" => { self.move_cursor(0, -1); return; }
                "\u{f703}" => { self.move_cursor(0, 1); return; }
                "\u{f700}" => { self.move_cursor(-1, 0); return; }
                "\u{f701}" => { self.move_cursor(1, 0); return; }
                _ => {}
            }
            match s.as_str() {
                "h" => self.move_cursor(0, -1),
                "l" => self.move_cursor(0, 1),
                "k" => self.move_cursor(-1, 0),
                "j" => self.move_cursor(1, 0),
                "0" => self.move_cursor_bol(),
                "$" => self.move_cursor_eol(),
                // -[NSApplication sendAction:to:from:] returns BOOL —
                // declaring `()` trips objc2's debug-build encoding
                // check and panics the app on first keypress.
                "n" => {
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: bool = unsafe {
                        objc2::msg_send![&app, sendAction: objc2::sel!(rvSearchNext:), to: std::ptr::null::<objc2::runtime::AnyObject>(), from: &*self]
                    };
                }
                "N" => {
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: bool = unsafe {
                        objc2::msg_send![&app, sendAction: objc2::sel!(rvSearchPrev:), to: std::ptr::null::<objc2::runtime::AnyObject>(), from: &*self]
                    };
                }
                "g" => self.move_cursor_to(0, 0),
                "G" => {
                    let last = self.ivars().doc.borrow().as_ref()
                        .map(|d| d.line_count().saturating_sub(1));
                    if let Some(last) = last {
                        self.move_cursor_to(last as i64, 0);
                    }
                }
                "/" => {
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: bool = unsafe {
                        objc2::msg_send![&app, sendAction: objc2::sel!(rvShowSearch:), to: std::ptr::null::<objc2::runtime::AnyObject>(), from: &*self]
                    };
                }
                _ => {}
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let window_point = event.locationInWindow();
            let local = self.convertPoint_fromView(window_point, None);
            if let Some(offset) = self.point_to_byte_offset(local) {
                self.ivars().last_click_offset.set(Some(offset));
                self.refresh_path_display();
                self.setNeedsDisplay(true);
            }
        }
    }
);

impl DocView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let font = NSFont::userFixedPitchFontOfSize(FONT_SIZE)
            .expect("user fixed-pitch font is always available");

        let ascent = font.ascender();
        let descent = font.descender();
        let leading = font.leading();
        let line_height = (ascent - descent + leading).ceil();

        let advance = measure_char_advance(&font);

        // Monokai-ish palette on a dark background.
        let fg = rgb(0.97, 0.97, 0.95);
        let bg = rgb(0.12, 0.13, 0.15);
        let row_highlight = rgb(0.20, 0.26, 0.36);
        let key = rgb(0.40, 0.85, 0.94);
        let string_ = rgb(0.90, 0.86, 0.45);
        let number = rgb(0.68, 0.50, 1.00);
        let bool_ = rgb(0.99, 0.59, 0.12);
        let null = rgb(0.97, 0.15, 0.45);
        // XML — tag reuses the cyan key colour so structural names look
        // alike across formats; attribute names get the orange-ish bool
        // tone; attribute values reuse the string yellow; comments are
        // a muted grey-green so they recede.
        let tag = rgb(0.40, 0.85, 0.94);
        let attr_name = rgb(0.99, 0.59, 0.12);
        let attr_value = rgb(0.90, 0.86, 0.45);
        let comment = rgb(0.50, 0.55, 0.55);
        let search_match = rgb(0.55, 0.45, 0.10);
        let search_current = rgb(0.85, 0.65, 0.10);

        let default_attrs = attribute_dict(&font, &fg);

        let ivars = DocViewIvars {
            doc: RefCell::new(None),
            font,
            line_height,
            ascent,
            advance,
            default_attrs,
            colors: Colors {
                fg,
                bg,
                row_highlight,
                key,
                string: string_,
                number,
                bool_,
                null,
                tag,
                attr_name,
                attr_value,
                comment,
                search_match,
                search_current,
            },
            last_click_offset: Cell::new(None),
            breadcrumb: RefCell::new(None),
            search: RefCell::new(SearchState::default()),
            csv_table: Cell::new(true),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_document(&self, doc: Arc<Document>) {
        let ivars = self.ivars();
        self.resize_for_doc(&doc);
        *ivars.doc.borrow_mut() = Some(doc);
        self.setNeedsDisplay(true);
        self.refresh_path_display();
    }

    /// Reset to table mode without a resize — called when a *new*
    /// document is loaded (`reset_doc_state`), but not on the snapshot
    /// swaps of a progressive load, so a Table↔Original choice made
    /// while a huge CSV is still indexing sticks.
    pub fn reset_table_mode(&self) {
        self.ivars().csv_table.set(true);
    }

    /// Size the frame for `doc` under the current view mode. CSV table
    /// mode is as wide as the aligned table; everything else as wide as
    /// the longest source line.
    fn resize_for_doc(&self, doc: &Document) {
        let ivars = self.ivars();
        let width_chars = match doc.output.csv.as_ref() {
            Some(meta) if ivars.csv_table.get() => meta.table_width as f64,
            _ => doc.max_line_bytes as f64,
        };
        let line_count = doc.line_count() as f64;
        let content_h = (line_count * ivars.line_height) + PAD_TOP + PAD_BOTTOM;
        let content_w =
            (width_chars.min(MAX_LINE_BYTES_FOR_LAYOUT) * ivars.advance) + PAD_LEFT + PAD_RIGHT;
        // Fill at least the enclosing clip view so a short document
        // doesn't leave the scroll area partially painted.
        let base = unsafe { self.superview() }
            .map(|clip| clip.bounds().size)
            .unwrap_or(self.bounds().size);
        let min_h = base.height.max(content_h);
        let min_w = base.width.max(content_w);
        self.setFrameSize(NSSize::new(min_w, min_h));
    }

    pub fn csv_table_mode(&self) -> bool {
        self.ivars().csv_table.get()
    }

    pub fn set_csv_table_mode(&self, on: bool) {
        self.ivars().csv_table.set(on);
        let doc = self.ivars().doc.borrow().as_ref().cloned();
        if let Some(doc) = doc {
            self.resize_for_doc(&doc);
        }
        self.setNeedsDisplay(true);
    }

    pub fn last_click_offset(&self) -> Option<u32> {
        self.ivars().last_click_offset.get()
    }

    pub fn set_last_click_offset(&self, offset: Option<u32>) {
        self.ivars().last_click_offset.set(offset);
    }

    pub fn search(&self, query: &str) -> usize {
        let mut state = self.ivars().search.borrow_mut();
        state.matches.clear();
        state.current = 0;
        state.match_len = 0;

        if query.is_empty() {
            drop(state);
            self.setNeedsDisplay(true);
            return 0;
        }

        let doc_ref = self.ivars().doc.borrow();
        let Some(doc) = doc_ref.as_ref() else {
            return 0;
        };

        let haystack = doc.bytes.as_slice();
        let needle = query.as_bytes();
        let needle_lower: Vec<u8> = needle.iter().map(|b| b.to_ascii_lowercase()).collect();
        state.match_len = needle_lower.len() as u32;

        const MAX_MATCHES: usize = 100_000;
        let nlen = needle_lower.len();
        if nlen <= haystack.len() {
            let first_lower = needle_lower[0];
            let first_upper = first_lower.to_ascii_uppercase();
            let mut i = 0;
            let end = haystack.len() - nlen;
            while i <= end {
                let b = haystack[i];
                if b != first_lower && b != first_upper {
                    i += 1;
                    continue;
                }
                if haystack[i..i + nlen]
                    .iter()
                    .zip(needle_lower.iter())
                    .all(|(h, n)| h.to_ascii_lowercase() == *n)
                {
                    state.matches.push(i as u32);
                    if state.matches.len() >= MAX_MATCHES {
                        break;
                    }
                    i += nlen.max(1);
                } else {
                    i += 1;
                }
            }
        }

        let count = state.matches.len();
        drop(state);
        self.setNeedsDisplay(true);
        count
    }

    pub fn clear_search(&self) {
        let mut state = self.ivars().search.borrow_mut();
        state.matches.clear();
        state.current = 0;
        state.match_len = 0;
        drop(state);
        self.setNeedsDisplay(true);
    }

    pub fn search_next(&self) -> Option<(usize, usize)> {
        let mut state = self.ivars().search.borrow_mut();
        if state.matches.is_empty() {
            return None;
        }
        state.current = (state.current + 1) % state.matches.len();
        let offset = state.matches[state.current];
        let total = state.matches.len();
        let idx = state.current;
        drop(state);
        self.ivars().last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
        Some((idx, total))
    }

    pub fn search_prev(&self) -> Option<(usize, usize)> {
        let mut state = self.ivars().search.borrow_mut();
        if state.matches.is_empty() {
            return None;
        }
        if state.current == 0 {
            state.current = state.matches.len() - 1;
        } else {
            state.current -= 1;
        }
        let offset = state.matches[state.current];
        let total = state.matches.len();
        let idx = state.current;
        drop(state);
        self.ivars().last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
        Some((idx, total))
    }

    pub fn scroll_to_current_match(&self) {
        let search = self.ivars().search.borrow();
        if let Some(&offset) = search.matches.get(search.current) {
            drop(search);
            self.ivars().last_click_offset.set(Some(offset));
            self.scroll_to_byte_offset(offset);
            self.refresh_path_display();
        }
    }

    fn scroll_to_byte_offset(&self, offset: u32) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        let line = line_for_offset(offset, line_starts);
        let col = if line >= line_starts.len() {
            0.0
        } else if let Some(meta) = active_csv_meta(doc, ivars.csv_table.get()) {
            let bytes = doc.bytes.as_slice();
            let cells = format::csv::scan_cells(bytes, line_starts[line], meta.delimiter);
            format::csv::visual_col_of_byte(meta, &cells, bytes, offset) as f64
        } else {
            let line_bytes = doc.line_bytes(line);
            byte_offset_to_char_col(line_bytes, offset - line_starts[line]) as f64
        };
        drop(doc_ref);

        let y = PAD_TOP + line as f64 * ivars.line_height;
        let x = PAD_LEFT + col * ivars.advance;
        let visible_rect = NSRect::new(
            NSPoint::new(x.max(0.0) - 40.0, y - 40.0),
            NSSize::new(200.0, ivars.line_height + 80.0),
        );
        self.scrollRectToVisible(visible_rect);
    }

    pub fn set_breadcrumb(&self, label: Retained<NSTextField>) {
        *self.ivars().breadcrumb.borrow_mut() = Some(label);
        self.refresh_path_display();
    }

    pub fn clear_document(&self) {
        let ivars = self.ivars();
        *ivars.doc.borrow_mut() = None;
        ivars.last_click_offset.set(None);
        if let Some(clip) = unsafe { self.superview() } {
            let clip_bounds = clip.bounds();
            self.setFrameSize(clip_bounds.size);
        }
        self.setNeedsDisplay(true);
        self.refresh_path_display();
    }

    /// Sub-tree at the last-clicked offset. Falls back to the entire
    /// document when there's no click (or no entry containing it).
    /// Returns None when no document is loaded.
    pub fn current_subtree(&self) -> Option<String> {
        let ivars = self.ivars();
        let doc = ivars.doc.borrow().as_ref().cloned()?;
        let bytes = doc.bytes.as_slice();
        // CSV keeps no per-cell index — resolve the click by rescanning
        // its record: cell if the click landed in one, else the row.
        if let Some(meta) = doc.output.csv.as_ref() {
            let slice = match ivars.last_click_offset.get() {
                None => bytes,
                Some(offset) => {
                    let hit = format::csv::locate(
                        bytes,
                        &doc.output.line_starts,
                        meta.delimiter,
                        offset,
                    );
                    let (s, e) = hit.cell.unwrap_or(hit.record);
                    &bytes[s as usize..e as usize]
                }
            };
            return Some(String::from_utf8_lossy(slice).into_owned());
        }
        let offset = ivars.last_click_offset.get().unwrap_or(0);
        let slice = match doc.output.paths.lookup(offset) {
            Some(idx) => {
                let entry = doc.output.paths.entries[idx as usize];
                format::value_bytes_for_entry(doc.format, bytes, &entry)
            }
            None => bytes,
        };
        Some(String::from_utf8_lossy(slice).into_owned())
    }

    /// Path expression (jq for JSON, XPath for XML) for the last-clicked
    /// offset. Returns the format's empty-path sentinel when nothing
    /// has been clicked (`.` for JSON, `/` for XML, `.` when no doc).
    pub fn current_path_expression(&self) -> String {
        let ivars = self.ivars();
        let Some(doc) = ivars.doc.borrow().as_ref().cloned() else {
            return String::from(".");
        };
        if let Some(meta) = doc.output.csv.as_ref() {
            return match ivars.last_click_offset.get() {
                None => "xsv table".to_string(),
                Some(offset) => {
                    let hit = format::csv::locate(
                        doc.bytes.as_slice(),
                        &doc.output.line_starts,
                        meta.delimiter,
                        offset,
                    );
                    format::csv::expression_for(meta, &hit)
                }
            };
        }
        let segments = match ivars.last_click_offset.get() {
            Some(offset) => match doc.output.paths.lookup(offset) {
                Some(entry) => doc.output.paths.path_of(entry),
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        format::path_expression(doc.format, &segments, &doc.output.names)
    }

    pub fn refresh_path_display(&self) {
        let expr = self.current_path_expression();
        if let Some(label) = self.ivars().breadcrumb.borrow().as_ref() {
            let ns = NSString::from_str(&expr);
            label.setStringValue(&ns);
        }
    }

    fn move_cursor(&self, dline: i64, dcol: i64) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        if line_starts.is_empty() {
            return;
        }

        let (cur_line, cur_col) = match ivars.last_click_offset.get() {
            Some(off) => {
                let line = line_for_offset(off, line_starts);
                let col = (off - line_starts[line]) as i64;
                (line as i64, col)
            }
            None => (0i64, 0i64),
        };

        let new_line = (cur_line + dline).clamp(0, doc.line_count() as i64 - 1);
        let line_bytes = doc.line_bytes(new_line as usize);
        let max_col = line_bytes.len() as i64;
        let new_col = (cur_col + dcol).clamp(0, max_col);

        let offset = line_starts[new_line as usize] + new_col as u32;
        drop(doc_ref);

        ivars.last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
    }

    fn move_cursor_bol(&self) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        let off = ivars.last_click_offset.get().unwrap_or(0);
        let line = line_for_offset(off, line_starts);
        let offset = line_starts[line];
        drop(doc_ref);
        ivars.last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
    }

    fn move_cursor_eol(&self) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        let off = ivars.last_click_offset.get().unwrap_or(0);
        let line = line_for_offset(off, line_starts);
        let line_bytes = doc.line_bytes(line);
        let offset = line_starts[line] + line_bytes.len() as u32;
        drop(doc_ref);
        ivars.last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
    }

    fn move_cursor_to(&self, line: i64, col: i64) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        if line_starts.is_empty() {
            return;
        }

        let line = line.clamp(0, doc.line_count() as i64 - 1);
        let line_bytes = doc.line_bytes(line as usize);
        let col = col.clamp(0, line_bytes.len() as i64);
        let offset = line_starts[line as usize] + col as u32;
        drop(doc_ref);

        ivars.last_click_offset.set(Some(offset));
        self.scroll_to_byte_offset(offset);
        self.refresh_path_display();
        self.setNeedsDisplay(true);
    }

    fn point_to_byte_offset(&self, local: NSPoint) -> Option<u32> {
        let ivars = self.ivars();
        let doc = ivars.doc.borrow().as_ref().cloned()?;
        let line_h = ivars.line_height;
        let y = (local.y - PAD_TOP).max(0.0);
        let line_idx = (y / line_h) as usize;
        if line_idx >= doc.line_count() || doc.output.line_starts.is_empty() {
            return None;
        }
        let x = (local.x - PAD_LEFT).max(0.0);
        let col = (x / ivars.advance).round() as usize;

        if let Some(meta) = active_csv_meta(&doc, ivars.csv_table.get()) {
            let bytes = doc.bytes.as_slice();
            let rs = doc.output.line_starts[line_idx];
            let cells = format::csv::scan_cells(bytes, rs, meta.delimiter);
            return format::csv::byte_of_visual_col(meta, &cells, bytes, col as u32)
                .or(Some(rs));
        }

        let line_bytes = doc.line_bytes(line_idx);
        let line_str = std::str::from_utf8(line_bytes).unwrap_or("");
        let mut byte_in_line = 0usize;
        let mut cnt = 0usize;
        for ch in line_str.chars() {
            if cnt >= col {
                break;
            }
            byte_in_line += ch.len_utf8();
            cnt += 1;
        }
        Some(doc.output.line_starts[line_idx] + byte_in_line as u32)
    }

    fn draw_content(&self, dirty: NSRect) {
        self.ivars().colors.bg.setFill();
        objc2_app_kit::NSRectFill(dirty);

        let ivars = self.ivars();
        let Some(doc) = ivars.doc.borrow().as_ref().cloned() else {
            return;
        };

        let line_h = ivars.line_height;
        let line_count = doc.line_count();
        if line_count == 0 {
            return;
        }

        let top = (dirty.origin.y - PAD_TOP).max(0.0);
        let bottom = dirty.origin.y + dirty.size.height - PAD_TOP;
        let first = (top / line_h).floor() as usize;
        let last = (bottom / line_h).ceil() as usize;
        let last = last.min(line_count.saturating_sub(1));

        let styles = &doc.output.styles;
        let line_starts = &doc.output.line_starts;

        if let Some(click) = ivars.last_click_offset.get() {
            let hit = line_for_offset(click, line_starts);
            if hit >= first && hit <= last {
                let bounds = self.bounds();
                let y = PAD_TOP + hit as f64 * line_h;
                let band = NSRect::new(
                    NSPoint::new(0.0, y),
                    NSSize::new(bounds.size.width, line_h),
                );
                ivars.colors.row_highlight.setFill();
                objc2_app_kit::NSRectFill(band);
            }
        }

        let adv = ivars.advance;
        let csv_meta = active_csv_meta(&doc, ivars.csv_table.get());
        let all_bytes = doc.bytes.as_slice();

        let search = ivars.search.borrow();
        if !search.matches.is_empty() {
            let match_len = search.match_len;

            let vis_col_start_f = ((dirty.origin.x - PAD_LEFT) / adv).floor().max(0.0);
            let vis_col_end_f = ((dirty.origin.x + dirty.size.width - PAD_LEFT) / adv).ceil() + 2.0;

            for line_idx in first..=last {
                let l_start = line_starts[line_idx];
                let line_bytes = doc.line_bytes(line_idx);

                if let Some(meta) = csv_meta {
                    // Table mode: matches map through the column layout.
                    // A match truncated out of view collapses to zero
                    // width and is skipped.
                    let l_end = l_start + line_bytes.len() as u32;
                    let cells = format::csv::scan_cells(all_bytes, l_start, meta.delimiter);
                    let lo = search.matches.partition_point(|&m| m + match_len <= l_start);
                    let hi = search.matches.partition_point(|&m| m < l_end);
                    for idx in lo..hi {
                        let m_start = search.matches[idx].max(l_start);
                        let m_end = (search.matches[idx] + match_len).min(l_end);
                        let ca = format::csv::visual_col_of_byte(meta, &cells, all_bytes, m_start);
                        let cb = format::csv::visual_col_of_byte(meta, &cells, all_bytes, m_end);
                        if cb <= ca {
                            continue;
                        }
                        let color = if idx == search.current {
                            &ivars.colors.search_current
                        } else {
                            &ivars.colors.search_match
                        };
                        let x = PAD_LEFT + ca as f64 * adv;
                        let w = (cb - ca) as f64 * adv;
                        let y = PAD_TOP + line_idx as f64 * line_h;
                        color.setFill();
                        objc2_app_kit::NSRectFill(NSRect::new(
                            NSPoint::new(x, y),
                            NSSize::new(w, line_h),
                        ));
                    }
                    continue;
                }

                let line_str = std::str::from_utf8(line_bytes).unwrap_or("");

                let (clip_byte_start, clip_byte_end) = char_range_to_byte_range(
                    line_str,
                    vis_col_start_f as usize,
                    vis_col_end_f as usize,
                );
                let abs_clip_start = l_start + clip_byte_start as u32;
                let abs_clip_end = l_start + clip_byte_end as u32;

                let lo = search.matches.partition_point(|&m| m + match_len <= abs_clip_start);
                let hi = search.matches.partition_point(|&m| m < abs_clip_end);

                for idx in lo..hi {
                    let m_start = search.matches[idx];
                    let m_end = m_start + match_len;
                    let color = if idx == search.current {
                        &ivars.colors.search_current
                    } else {
                        &ivars.colors.search_match
                    };
                    let byte_start = m_start.max(l_start) - l_start;
                    let byte_end = m_end.min(l_start + line_bytes.len() as u32) - l_start;
                    let char_start = byte_offset_to_char_col(
                        &line_bytes[..clip_byte_end.min(line_bytes.len())],
                        byte_start,
                    );
                    let char_end = byte_offset_to_char_col(
                        &line_bytes[..clip_byte_end.min(line_bytes.len())],
                        byte_end,
                    );
                    let x = PAD_LEFT + char_start as f64 * adv;
                    let w = (char_end - char_start) as f64 * adv;
                    let y = PAD_TOP + line_idx as f64 * line_h;
                    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, line_h));
                    color.setFill();
                    objc2_app_kit::NSRectFill(rect);
                }
            }
        }
        drop(search);

        let vis_col_start = ((dirty.origin.x - PAD_LEFT) / adv).floor().max(0.0) as usize;
        let vis_col_end = ((dirty.origin.x + dirty.size.width - PAD_LEFT) / adv).ceil() as usize + 2;

        for line_idx in first..=last {
            if let Some(meta) = csv_meta {
                let cells =
                    format::csv::scan_cells(all_bytes, line_starts[line_idx], meta.delimiter);
                let rr = format::csv::render_row(
                    meta,
                    &cells,
                    all_bytes,
                    line_idx == 0,
                    vis_col_start as u32,
                    vis_col_end as u32,
                );
                if rr.text.is_empty() {
                    continue;
                }
                let ns_str = NSString::from_str(&rr.text);
                let attr_str = unsafe {
                    NSMutableAttributedString::initWithString_attributes(
                        NSMutableAttributedString::alloc(),
                        &ns_str,
                        Some(&ivars.default_attrs),
                    )
                };
                for &(u16_start, u16_end, kind) in &rr.spans {
                    let color = ivars.colors.for_kind(kind);
                    let range = NSRange {
                        location: u16_start,
                        length: u16_end - u16_start,
                    };
                    unsafe {
                        attr_str.addAttribute_value_range(
                            NSForegroundColorAttributeName,
                            color.as_ref() as &objc2::runtime::AnyObject,
                            range,
                        );
                    }
                }
                let y = PAD_TOP + line_idx as f64 * line_h;
                let x = PAD_LEFT + rr.origin_chars as f64 * adv;
                attr_str.drawAtPoint(NSPoint::new(x, y));
                continue;
            }

            let line_start_byte = line_starts[line_idx];
            let full_bytes = doc.line_bytes(line_idx);
            let full_str = std::str::from_utf8(full_bytes).unwrap_or("");
            if full_str.is_empty() {
                continue;
            }

            let (clip_byte_start, clip_byte_end) =
                char_range_to_byte_range(full_str, vis_col_start, vis_col_end);
            let clipped = &full_str[clip_byte_start..clip_byte_end];

            // CSV raw mode: a "line" is a whole record, so a quoted
            // field's embedded newline would wrap and overdraw the row
            // below. Substitute control chars with picture glyphs —
            // one glyph per char keeps the column math intact.
            let drawn: std::borrow::Cow<'_, str> = if doc.output.csv.is_some()
                && clipped.contains(|c: char| (c as u32) < 0x20)
            {
                std::borrow::Cow::Owned(
                    clipped.chars().map(format::csv::display_char).collect(),
                )
            } else {
                std::borrow::Cow::Borrowed(clipped)
            };

            let ns_str = NSString::from_str(&drawn);
            let attr_str = unsafe {
                NSMutableAttributedString::initWithString_attributes(
                    NSMutableAttributedString::alloc(),
                    &ns_str,
                    Some(&ivars.default_attrs),
                )
            };

            let abs_clip_start = line_start_byte + clip_byte_start as u32;
            let abs_clip_end = line_start_byte + clip_byte_end as u32;
            let lo = styles.partition_point(|sp| sp.end <= abs_clip_start);
            let hi = styles.partition_point(|sp| sp.start < abs_clip_end);
            if lo < hi {
                paint_spans(&attr_str, clipped, abs_clip_start, &styles[lo..hi], &ivars.colors);
            }

            let y = PAD_TOP + line_idx as f64 * line_h;
            let x = PAD_LEFT + vis_col_start as f64 * adv;
            let pt = NSPoint::new(x, y);
            attr_str.drawAtPoint(pt);
        }
    }
}

/// Table layout to apply when drawing/mapping `doc`, if any: present
/// only for CSV/TSV documents with table mode on.
fn active_csv_meta(doc: &Document, table_on: bool) -> Option<&format::csv::CsvMeta> {
    if table_on { doc.output.csv.as_ref() } else { None }
}

fn byte_offset_to_char_col(s: &[u8], byte_off: u32) -> u32 {
    let clamped = (byte_off as usize).min(s.len());
    std::str::from_utf8(&s[..clamped])
        .map(|sl| sl.chars().count() as u32)
        .unwrap_or(clamped as u32)
}

fn char_range_to_byte_range(s: &str, col_start: usize, col_end: usize) -> (usize, usize) {
    let mut byte_start = s.len();
    let byte_end;
    for (i, (byte_off, _)) in s.char_indices().enumerate() {
        if i == col_start {
            byte_start = byte_off;
        }
        if i == col_end {
            byte_end = byte_off;
            return (byte_start, byte_end);
        }
    }
    (byte_start.min(s.len()), s.len())
}

fn paint_spans(
    attr_str: &NSMutableAttributedString,
    line_str: &str,
    line_start_byte: u32,
    spans: &[StyleSpan],
    colors: &Colors,
) {
    let line_bytes = line_str.len() as u32;
    let mut cursor_byte = 0u32;
    let mut cursor_u16 = 0u32;
    let mut char_iter = line_str.chars();

    let advance_to = |target_byte: u32,
                          cursor_byte: &mut u32,
                          cursor_u16: &mut u32,
                          it: &mut std::str::Chars<'_>|
     -> u32 {
        while *cursor_byte < target_byte {
            match it.next() {
                Some(ch) => {
                    *cursor_byte += ch.len_utf8() as u32;
                    *cursor_u16 += ch.len_utf16() as u32;
                }
                None => break,
            }
        }
        *cursor_u16
    };

    for span in spans {
        let span_start = span.start.saturating_sub(line_start_byte).min(line_bytes);
        let span_end = span.end.saturating_sub(line_start_byte).min(line_bytes);
        if span_end <= span_start {
            continue;
        }
        if span_start < cursor_byte {
            continue;
        }
        let u16_start = advance_to(span_start, &mut cursor_byte, &mut cursor_u16, &mut char_iter);
        let u16_end = advance_to(span_end, &mut cursor_byte, &mut cursor_u16, &mut char_iter);

        let color = colors.for_kind(span.kind);
        let range = NSRange {
            location: u16_start as usize,
            length: (u16_end - u16_start) as usize,
        };
        unsafe {
            attr_str.addAttribute_value_range(
                NSForegroundColorAttributeName,
                color.as_ref() as &objc2::runtime::AnyObject,
                range,
            );
        }
    }
}

fn attribute_dict(font: &NSFont, color: &NSColor) -> Retained<NSDictionary<NSString>> {
    let keys: [&NSString; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&objc2::runtime::AnyObject; 2] = [
        font.as_ref() as &objc2::runtime::AnyObject,
        color.as_ref() as &objc2::runtime::AnyObject,
    ];
    NSDictionary::from_slices(&keys, &values)
}

fn measure_char_advance(font: &NSFont) -> f64 {
    let s = NSString::from_str("M");
    let fg = NSColor::textColor();
    let dict = attribute_dict(font, &fg);
    let attr = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &s,
            Some(&dict),
        )
    };
    attr.size().width
}

pub fn initial_frame() -> NSRect {
    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(960.0, 720.0))
}

fn line_for_offset(offset: u32, starts: &[u32]) -> usize {
    if starts.is_empty() {
        return 0;
    }
    starts.partition_point(|&s| s <= offset).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_for_offset_finds_containing_line() {
        let starts = vec![0u32, 10, 20, 35];
        assert_eq!(line_for_offset(0, &starts), 0);
        assert_eq!(line_for_offset(5, &starts), 0);
        assert_eq!(line_for_offset(9, &starts), 0);
        assert_eq!(line_for_offset(10, &starts), 1);
        assert_eq!(line_for_offset(15, &starts), 1);
        assert_eq!(line_for_offset(34, &starts), 2);
        assert_eq!(line_for_offset(35, &starts), 3);
        assert_eq!(line_for_offset(1000, &starts), 3);
    }

    #[test]
    fn line_for_offset_empty_is_zero() {
        assert_eq!(line_for_offset(0, &[]), 0);
        assert_eq!(line_for_offset(42, &[]), 0);
    }
}
