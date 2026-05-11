//! `JsonView` — custom `NSView` that renders JSON text line by line.
//!
//! Layout: fixed monospace font → constant line height and char advance,
//! so the document view is sized once (`line_count * line_height` tall,
//! `max_line_bytes * advance` wide) and `drawRect:` reduces to picking a
//! visible line range and drawing each one with CoreText.


use crate::doc::Document;
use crate::parser::{self, StyleKind, StyleSpan};
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
/// larger than any realistic single JSON line.
const MAX_LINE_BYTES_FOR_LAYOUT: f64 = 8_000.0;

/// Search match state. Byte offsets into the current document.
#[derive(Default)]
pub struct SearchState {
    /// Byte offset of each match start.
    pub matches: Vec<u32>,
    /// Length of the search query in bytes.
    pub match_len: u32,
    /// Index into `matches` of the "current" (focused) match.
    pub current: usize,
}

pub struct JsonViewIvars {
    doc: RefCell<Option<Arc<Document>>>,
    #[allow(dead_code)] // retained to keep the font alive for CoreText
    font: Retained<NSFont>,
    line_height: f64,
    #[allow(dead_code)] // used implicitly via line_height calculation
    ascent: f64,
    /// Width of one monospace character ("M" advance).
    advance: f64,
    /// Pre-built attribute dictionary for the default text colour.
    default_attrs: Retained<NSDictionary<NSString>>,
    colors: Colors,
    /// Byte offset of the last click. None until the user clicks.
    last_click_offset: Cell<Option<u32>>,
    /// Breadcrumb label set by main.rs after the header bar is built.
    breadcrumb: RefCell<Option<Retained<NSTextField>>>,
    /// Active search matches.
    search: RefCell<SearchState>,
}

struct Colors {
    #[allow(dead_code)] // retained; default_attrs references the underlying NSColor
    fg: Retained<NSColor>,
    bg: Retained<NSColor>,
    /// Background band drawn behind the line containing the last click.
    row_highlight: Retained<NSColor>,
    key: Retained<NSColor>,
    string: Retained<NSColor>,
    number: Retained<NSColor>,
    bool_: Retained<NSColor>,
    null: Retained<NSColor>,
    /// Background for non-current search matches.
    search_match: Retained<NSColor>,
    /// Background for the current (focused) search match.
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
        }
    }
}

fn rgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, 1.0)
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RVJsonView"]
    #[ivars = JsonViewIvars]
    pub struct JsonView;

    impl JsonView {
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
            // Don't intercept keys with Cmd held — those are menu shortcuts.
            if has_cmd {
                return;
            }
            // Use characters() (not IgnoringModifiers) so Shift+G = "G"
            // and Shift+4 = "$" are preserved.
            let Some(chars) = event.characters() else { return };
            let s = chars.to_string();
            // Arrow keys (function key range) — check ignoring modifiers
            // so they work regardless of input method.
            let Some(raw) = event.charactersIgnoringModifiers() else { return };
            let raw_s = raw.to_string();
            match raw_s.as_str() {
                "\u{f702}" => { self.move_cursor(0, -1); return; } // ←
                "\u{f703}" => { self.move_cursor(0, 1); return; }  // →
                "\u{f700}" => { self.move_cursor(-1, 0); return; } // ↑
                "\u{f701}" => { self.move_cursor(1, 0); return; }  // ↓
                _ => {}
            }
            match s.as_str() {
                "h" => self.move_cursor(0, -1),
                "l" => self.move_cursor(0, 1),
                "k" => self.move_cursor(-1, 0),
                "j" => self.move_cursor(1, 0),
                "0" => self.move_cursor_bol(),
                "$" => self.move_cursor_eol(),
                "n" => {
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: () = unsafe {
                        objc2::msg_send![&app, sendAction: objc2::sel!(rvSearchNext:), to: std::ptr::null::<objc2::runtime::AnyObject>(), from: &*self]
                    };
                }
                "N" => {
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: () = unsafe {
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
                    // Open search — send action up the responder chain.
                    let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm());
                    let _: () = unsafe {
                        objc2::msg_send![&app, sendAction: objc2::sel!(rvShowSearch:), to: std::ptr::null::<objc2::runtime::AnyObject>(), from: &*self]
                    };
                }
                _ => {} // swallow — no beep
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let window_point = event.locationInWindow();
            let local = self.convertPoint_fromView(window_point, None);
            if let Some(offset) = self.point_to_byte_offset(local) {
                self.ivars().last_click_offset.set(Some(offset));
                self.refresh_path_display();
                // Repaint so the row-highlight band follows the click.
                self.setNeedsDisplay(true);
            }
        }
    }
);

impl JsonView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let font = NSFont::userFixedPitchFontOfSize(FONT_SIZE)
            .expect("user fixed-pitch font is always available");

        // NSFont::ascender is positive; descender is negative; leading is
        // the extra gap. Sum gives the natural line advance.
        let ascent = font.ascender();
        let descent = font.descender();
        let leading = font.leading();
        let line_height = (ascent - descent + leading).ceil();

        // Monospace → every glyph has the same advance. Measure "M".
        let advance = measure_char_advance(&font);

        // Monokai-ish palette on a dark background.
        let fg = rgb(0.97, 0.97, 0.95);
        let bg = rgb(0.12, 0.13, 0.15);
        // Subtle bluish lift over the bg — visible but doesn't drown
        // out the syntax colours on top.
        let row_highlight = rgb(0.20, 0.26, 0.36);
        let key = rgb(0.40, 0.85, 0.94);
        let string_ = rgb(0.90, 0.86, 0.45);
        let number = rgb(0.68, 0.50, 1.00);
        let bool_ = rgb(0.99, 0.59, 0.12);
        let null = rgb(0.97, 0.15, 0.45);
        let search_match = rgb(0.55, 0.45, 0.10);
        let search_current = rgb(0.85, 0.65, 0.10);

        let default_attrs = attribute_dict(&font, &fg);

        let ivars = JsonViewIvars {
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
                search_match,
                search_current,
            },
            last_click_offset: Cell::new(None),
            breadcrumb: RefCell::new(None),
            search: RefCell::new(SearchState::default()),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_document(&self, doc: Arc<Document>) {
        let ivars = self.ivars();

        // Size the view to fit the document. Minified JSON can put the
        // entire file on one line — clamp width so AppKit doesn't choke
        // on a multi-million-pixel frame. The user can still toggle
        // Prettify to get a wrappable, well-formatted view.
        let line_count = doc.line_count() as f64;
        let max_bytes = (doc.max_line_bytes as f64).min(MAX_LINE_BYTES_FOR_LAYOUT);
        let content_h = (line_count * ivars.line_height) + PAD_TOP + PAD_BOTTOM;
        let content_w = (max_bytes * ivars.advance) + PAD_LEFT + PAD_RIGHT;

        // Don't shrink below the current clip view so we don't scroll by
        // accident on small documents.
        let min_h = self.bounds().size.height.max(content_h);
        let min_w = self.bounds().size.width.max(content_w);
        self.setFrameSize(NSSize::new(min_w, min_h));

        *ivars.doc.borrow_mut() = Some(doc);
        self.setNeedsDisplay(true);
        self.refresh_path_display();
    }

    pub fn last_click_offset(&self) -> Option<u32> {
        self.ivars().last_click_offset.get()
    }

    pub fn set_last_click_offset(&self, offset: Option<u32>) {
        self.ivars().last_click_offset.set(offset);
    }

    /// Run a case-insensitive search. Returns the match count.
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

        // Case-insensitive byte search, optimised with a first-byte
        // skip so we don't call to_ascii_lowercase on every byte.
        const MAX_MATCHES: usize = 100_000;
        let nlen = needle_lower.len();
        if nlen <= haystack.len() {
            let first_lower = needle_lower[0];
            let first_upper = first_lower.to_ascii_uppercase();
            let mut i = 0;
            let end = haystack.len() - nlen;
            while i <= end {
                // Skip to next candidate using the first byte.
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

    /// Clear all search highlights.
    pub fn clear_search(&self) {
        let mut state = self.ivars().search.borrow_mut();
        state.matches.clear();
        state.current = 0;
        state.match_len = 0;
        drop(state);
        self.setNeedsDisplay(true);
    }

    /// Move to the next match and scroll it into view. Returns new
    /// (current_index, total) or None if no matches.
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

    /// Move to the previous match and scroll it into view.
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

    /// Scroll to the current search match and update the breadcrumb.
    pub fn scroll_to_current_match(&self) {
        let search = self.ivars().search.borrow();
        if let Some(&offset) = search.matches.get(search.current) {
            drop(search);
            self.ivars().last_click_offset.set(Some(offset));
            self.scroll_to_byte_offset(offset);
            self.refresh_path_display();
        }
    }

    /// Scroll the enclosing scroll view so the byte offset is visible.
    fn scroll_to_byte_offset(&self, offset: u32) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        let line = line_for_offset(offset, line_starts);
        let col = if line < line_starts.len() {
            let line_bytes = doc.line_bytes(line);
            byte_offset_to_char_col(line_bytes, offset - line_starts[line]) as f64
        } else {
            0.0
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

    /// Drop the current document and reset click/selection state. The
    /// view paints as an empty dark canvas and the breadcrumb goes back
    /// to ".".
    pub fn clear_document(&self) {
        let ivars = self.ivars();
        *ivars.doc.borrow_mut() = None;
        ivars.last_click_offset.set(None);
        // Collapse the document view back to the clip-view size so the
        // scroll bars go away.
        if let Some(clip) = unsafe { self.superview() } {
            let clip_bounds = clip.bounds();
            self.setFrameSize(clip_bounds.size);
        }
        self.setNeedsDisplay(true);
        self.refresh_path_display();
    }

    /// JSON sub-tree at the last-clicked offset. Falls back to the entire
    /// document when there's no click (or no parseable entry containing
    /// it). Returns None when no document is loaded.
    pub fn current_json_subtree(&self) -> Option<String> {
        let ivars = self.ivars();
        let doc = ivars.doc.borrow().as_ref().cloned()?;
        let bytes = doc.bytes.as_slice();
        let offset = ivars.last_click_offset.get().unwrap_or(0);
        let slice = match doc.output.paths.lookup(offset) {
            Some(idx) => {
                let entry = doc.output.paths.entries[idx as usize];
                parser::value_bytes_for_entry(bytes, &entry)
            }
            None => bytes,
        };
        Some(String::from_utf8_lossy(slice).into_owned())
    }

    /// jq expression for the last-clicked offset. `.` when nothing clicked.
    pub fn current_jq_expression(&self) -> String {
        let ivars = self.ivars();
        let Some(doc) = ivars.doc.borrow().as_ref().cloned() else {
            return String::from(".");
        };
        let Some(offset) = ivars.last_click_offset.get() else {
            return String::from(".");
        };
        let Some(entry) = doc.output.paths.lookup(offset) else {
            return String::from(".");
        };
        let segments = doc.output.paths.path_of(entry);
        parser::jq_path(&segments, &doc.output.keys)
    }

    /// Push the current jq expression into the breadcrumb label (if any).
    /// Called by click, scroll observer, and document-load paths.
    pub fn refresh_path_display(&self) {
        let jq = self.current_jq_expression();
        if let Some(label) = self.ivars().breadcrumb.borrow().as_ref() {
            let ns = NSString::from_str(&jq);
            label.setStringValue(&ns);
        }
    }

    /// Move the cursor by a delta in lines and columns. Clamps to document
    /// bounds. If no cursor exists yet, starts at (0, 0).
    fn move_cursor(&self, dline: i64, dcol: i64) {
        let ivars = self.ivars();
        let doc_ref = ivars.doc.borrow();
        let Some(doc) = doc_ref.as_ref() else { return };
        let line_starts = &doc.output.line_starts;
        if line_starts.is_empty() {
            return;
        }

        // Current position → line + col.
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

    /// Move cursor to the beginning of the current line.
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

    /// Move cursor to the end of the current line.
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

    /// Move the cursor to an absolute line and column.
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

        let line_bytes = doc.line_bytes(line_idx);
        let line_str = std::str::from_utf8(line_bytes).unwrap_or("");
        // Monospace → column index = char index. Walk to that char to get
        // the byte offset, clamping to the line end.
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
        // Background.
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

        // Visible line range in flipped coords.
        let top = (dirty.origin.y - PAD_TOP).max(0.0);
        let bottom = dirty.origin.y + dirty.size.height - PAD_TOP;
        let first = (top / line_h).floor() as usize;
        let last = (bottom / line_h).ceil() as usize;
        let last = last.min(line_count.saturating_sub(1));

        let styles = &doc.output.styles;
        let line_starts = &doc.output.line_starts;

        // Row highlight band — drawn before the text so colours land on
        // top. Only drawn when the selected line is inside `dirty`.
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

        // Search match highlights — painted as background rects before text.
        // For single-line files (minified JSON), thousands of matches may
        // exist on the visible line range but most are off-screen
        // horizontally. We restrict to matches whose byte offset falls
        // within the visible column window to avoid O(all-matches) work.
        let search = ivars.search.borrow();
        if !search.matches.is_empty() {
            let match_len = search.match_len;

            // Byte range of the visible horizontal strip per line.
            // On a 1-line file this is the key optimisation — we only
            // paint the ~100 chars visible in the viewport.
            let vis_col_start_f = ((dirty.origin.x - PAD_LEFT) / adv).floor().max(0.0);
            let vis_col_end_f = ((dirty.origin.x + dirty.size.width - PAD_LEFT) / adv).ceil() + 2.0;

            for line_idx in first..=last {
                let l_start = line_starts[line_idx];
                let line_bytes = doc.line_bytes(line_idx);
                let line_str = std::str::from_utf8(line_bytes).unwrap_or("");

                // Convert visible columns to byte offsets within this line.
                let (clip_byte_start, clip_byte_end) = char_range_to_byte_range(
                    line_str,
                    vis_col_start_f as usize,
                    vis_col_end_f as usize,
                );
                let abs_clip_start = l_start + clip_byte_start as u32;
                let abs_clip_end = l_start + clip_byte_end as u32;

                // Binary search for matches overlapping the visible byte window.
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

        // Visible column range — for monospace we can convert the dirty
        // rect's x span to a character range and only create an
        // NSAttributedString for that slice. Without this, a single
        // 11 MB minified line would beach-ball the renderer.
        let vis_col_start = ((dirty.origin.x - PAD_LEFT) / adv).floor().max(0.0) as usize;
        // Extra margin so partial glyphs at the edges aren't clipped.
        let vis_col_end = ((dirty.origin.x + dirty.size.width - PAD_LEFT) / adv).ceil() as usize + 2;

        for line_idx in first..=last {
            let line_start_byte = line_starts[line_idx];
            let full_bytes = doc.line_bytes(line_idx);
            let full_str = std::str::from_utf8(full_bytes).unwrap_or("");
            if full_str.is_empty() {
                continue;
            }

            // Clip to visible columns. Walk chars to find byte offsets
            // since the string may contain multi-byte UTF-8.
            let (clip_byte_start, clip_byte_end) =
                char_range_to_byte_range(full_str, vis_col_start, vis_col_end);
            let clipped = &full_str[clip_byte_start..clip_byte_end];

            let ns_str = NSString::from_str(clipped);
            let attr_str = unsafe {
                NSMutableAttributedString::initWithString_attributes(
                    NSMutableAttributedString::alloc(),
                    &ns_str,
                    Some(&ivars.default_attrs),
                )
            };

            // Style spans use absolute byte offsets. Shift the clip
            // window into absolute coordinates for the binary search,
            // then pass the clipped substring and its absolute start
            // to paint_spans so it can translate back.
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

/// Convert a byte offset within a UTF-8 string to a character (column) count.
fn byte_offset_to_char_col(s: &[u8], byte_off: u32) -> u32 {
    let clamped = (byte_off as usize).min(s.len());
    std::str::from_utf8(&s[..clamped])
        .map(|sl| sl.chars().count() as u32)
        .unwrap_or(clamped as u32)
}

/// Convert a visible column range (char indices) to byte offsets in `s`.
/// Clamps to the string length.
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
    // col_end >= char count — clamp to string end.
    (byte_start.min(s.len()), s.len())
}

fn paint_spans(
    attr_str: &NSMutableAttributedString,
    line_str: &str,
    line_start_byte: u32,
    spans: &[StyleSpan],
    colors: &Colors,
) {
    // Walk the line once, maintaining a byte→utf16 cursor, and translate
    // each span's endpoints in sorted order. O(line-length + spans).
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
        // Clamp span to this line.
        let span_start = span.start.saturating_sub(line_start_byte).min(line_bytes);
        let span_end = span.end.saturating_sub(line_start_byte).min(line_bytes);
        if span_end <= span_start {
            continue;
        }
        // Endpoints should only walk forward. If a span starts before the
        // cursor (shouldn't happen in sorted input), skip — it's already
        // been painted by an earlier call.
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

/// Binary search `line_starts` for the line containing `offset`. Returns
/// 0 when `starts` is empty (no lines yet).
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
