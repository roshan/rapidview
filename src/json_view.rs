//! `JsonView` — custom `NSView` that renders JSON text line by line.
//!
//! Layout: fixed monospace font → constant line height and char advance,
//! so the document view is sized once (`line_count * line_height` tall,
//! `max_line_bytes * advance` wide) and `drawRect:` reduces to picking a
//! visible line range and drawing each one with CoreText.
//!
//! Phases:
//!   * T3a — solid background (done)
//!   * T3b — per-line draw with one colour (this turn)
//!   * T3c — per-token colours from the style-span table

#![allow(dead_code)]

use crate::doc::Document;
use crate::parser::{self, StyleKind, StyleSpan};
use objc2::rc::Retained;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSTextField, NSView,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSSize,
    NSString,
};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const PAD_LEFT: f64 = 12.0;
const PAD_TOP: f64 = 8.0;
const PAD_RIGHT: f64 = 12.0;
const PAD_BOTTOM: f64 = 8.0;
const FONT_SIZE: f64 = 13.0;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ViewMode {
    Cursor = 0,
    ScrollLock = 1,
}

impl ViewMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ViewMode::ScrollLock,
            _ => ViewMode::Cursor,
        }
    }
}

/// Shared mode flag. Click/scroll handlers both read it, the toolbar
/// button writes it. Single byte, no synchronisation cost.
pub static VIEW_MODE: AtomicU8 = AtomicU8::new(ViewMode::Cursor as u8);

pub fn set_view_mode(m: ViewMode) {
    VIEW_MODE.store(m as u8, Ordering::Relaxed);
}

pub fn view_mode() -> ViewMode {
    ViewMode::from_u8(VIEW_MODE.load(Ordering::Relaxed))
}

pub struct JsonViewIvars {
    doc: RefCell<Option<Arc<Document>>>,
    font: Retained<NSFont>,
    line_height: f64,
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
}

struct Colors {
    fg: Retained<NSColor>,
    bg: Retained<NSColor>,
    key: Retained<NSColor>,
    string: Retained<NSColor>,
    number: Retained<NSColor>,
    bool_: Retained<NSColor>,
    null: Retained<NSColor>,
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

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let window_point = event.locationInWindow();
            let local = self.convertPoint_fromView(window_point, None);
            if let Some(offset) = self.point_to_byte_offset(local) {
                self.ivars().last_click_offset.set(Some(offset));
                if view_mode() == ViewMode::Cursor {
                    self.refresh_path_display();
                }
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
        let key = rgb(0.40, 0.85, 0.94);
        let string_ = rgb(0.90, 0.86, 0.45);
        let number = rgb(0.68, 0.50, 1.00);
        let bool_ = rgb(0.99, 0.59, 0.12);
        let null = rgb(0.97, 0.15, 0.45);

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
                key,
                string: string_,
                number,
                bool_,
                null,
            },
            last_click_offset: Cell::new(None),
            breadcrumb: RefCell::new(None),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_document(&self, doc: Arc<Document>) {
        let ivars = self.ivars();

        // Size the view to fit the document.
        let line_count = doc.line_count() as f64;
        let max_bytes = doc.max_line_bytes as f64;
        let content_h = (line_count * ivars.line_height) + PAD_TOP + PAD_BOTTOM;
        let content_w = (max_bytes * ivars.advance) + PAD_LEFT + PAD_RIGHT;

        // Don't shrink below the current clip view so we don't scroll by
        // accident on small documents.
        let min_h = self.bounds().size.height.max(content_h);
        let min_w = self.bounds().size.width.max(content_w);
        self.setFrameSize(NSSize::new(min_w, min_h));

        *ivars.doc.borrow_mut() = Some(doc);
        ivars.last_click_offset.set(None);
        self.setNeedsDisplay(true);
        self.refresh_path_display();
    }

    pub fn set_breadcrumb(&self, label: Retained<NSTextField>) {
        *self.ivars().breadcrumb.borrow_mut() = Some(label);
        self.refresh_path_display();
    }

    /// Byte offset that currently drives the breadcrumb, chosen by
    /// `view_mode()`. Returns None if nothing has been clicked in cursor
    /// mode; scroll-lock mode always returns the topmost visible line.
    pub fn current_offset(&self) -> Option<u32> {
        let ivars = self.ivars();
        match view_mode() {
            ViewMode::Cursor => ivars.last_click_offset.get(),
            ViewMode::ScrollLock => self.viewport_top_offset(),
        }
    }

    /// jq expression for the current offset. `.` when no offset exists.
    pub fn current_jq_expression(&self) -> String {
        let ivars = self.ivars();
        let Some(doc) = ivars.doc.borrow().as_ref().cloned() else {
            return String::from(".");
        };
        let Some(offset) = self.current_offset() else {
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

    fn point_to_byte_offset(&self, local: NSPoint) -> Option<u32> {
        let ivars = self.ivars();
        let doc = ivars.doc.borrow().as_ref().cloned()?;
        let line_h = ivars.line_height;
        let y = (local.y - PAD_TOP).max(0.0);
        let line_idx = (y / line_h) as usize;
        if line_idx >= doc.line_count() {
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

    /// Byte offset of the first character on the line visible at the top
    /// of the clip view. Used by scroll-lock mode.
    fn viewport_top_offset(&self) -> Option<u32> {
        let ivars = self.ivars();
        let doc = ivars.doc.borrow().as_ref().cloned()?;
        let visible = self.visibleRect();
        let y = (visible.origin.y - PAD_TOP).max(0.0);
        let line_idx = (y / ivars.line_height) as usize;
        if line_idx >= doc.line_count() {
            return None;
        }
        Some(doc.output.line_starts[line_idx])
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

        for line_idx in first..=last {
            let line_start_byte = line_starts[line_idx];
            let bytes = doc.line_bytes(line_idx);
            let s = std::str::from_utf8(bytes).unwrap_or("");
            if s.is_empty() {
                continue;
            }
            let ns_str = NSString::from_str(s);
            // Mutable so we can paint per-token colours onto the default.
            let attr_str = unsafe {
                NSMutableAttributedString::initWithString_attributes(
                    NSMutableAttributedString::alloc(),
                    &ns_str,
                    Some(&ivars.default_attrs),
                )
            };

            // Paint tokens that intersect this line. Spans are in emit
            // order = byte order, so a range lookup is two binary searches.
            let line_byte_len = bytes.len() as u32;
            let line_end_byte = line_start_byte + line_byte_len;
            let lo = styles.partition_point(|sp| sp.end <= line_start_byte);
            let hi = styles.partition_point(|sp| sp.start < line_end_byte);
            if lo == hi {
                // Still a valid line, just no coloured tokens.
            } else {
                paint_spans(&attr_str, s, line_start_byte, &styles[lo..hi], &ivars.colors);
            }

            let y = PAD_TOP + line_idx as f64 * line_h;
            let pt = NSPoint::new(PAD_LEFT, y);
            attr_str.drawAtPoint(pt);
        }
    }
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
