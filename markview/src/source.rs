//! Source view — render the raw markdown bytes as a colored
//! monospace `NSAttributedString` keyed off the `StyleSpan`s from
//! markdown-core. Used by the "Source" mode of the toggle.

use markdown_core::{ParseOutput, StyleKind};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSRange, NSString,
};

const SOURCE_FONT_SIZE: f64 = 13.0;

pub fn build_with_parse(
    bytes: &[u8],
    parse: Option<&ParseOutput>,
) -> Retained<NSAttributedString> {
    let s = std::str::from_utf8(bytes).unwrap_or("");
    let ns_str = NSString::from_str(s);
    let font = NSFont::userFixedPitchFontOfSize(SOURCE_FONT_SIZE)
        .expect("user fixed-pitch font is always available");
    let default_fg = NSColor::textColor();

    let keys: [&NSString; 2] =
        unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [
        font.as_ref() as &AnyObject,
        default_fg.as_ref() as &AnyObject,
    ];
    let default_attrs = NSDictionary::from_slices(&keys, &values);

    let attr = unsafe {
        NSMutableAttributedString::initWithString_attributes(
            NSMutableAttributedString::alloc(),
            &ns_str,
            Some(&default_attrs),
        )
    };

    if let Some(parse) = parse {
        apply_style_spans(&attr, s, parse);
    }

    // NSMutableAttributedString is a subclass of NSAttributedString;
    // upcast for the caller.
    Retained::into_super(attr)
}

fn apply_style_spans(attr: &NSMutableAttributedString, s: &str, parse: &ParseOutput) {
    let heading_color = NSColor::systemBlueColor();
    let code_color = NSColor::systemTealColor();

    // Walk the styles in source order; maintain a (byte, utf16) cursor
    // so byte ranges from the parser map to UTF-16 ranges for NSRange.
    let mut byte_cursor = 0usize;
    let mut utf16_cursor = 0u32;
    let mut chars = s.chars();
    let total_bytes = s.len();

    let advance_to = |target_byte: usize,
                      byte_cursor: &mut usize,
                      utf16_cursor: &mut u32,
                      chars: &mut std::str::Chars<'_>|
     -> u32 {
        let clamped = target_byte.min(total_bytes);
        while *byte_cursor < clamped {
            match chars.next() {
                Some(ch) => {
                    *byte_cursor += ch.len_utf8();
                    *utf16_cursor += ch.len_utf16() as u32;
                }
                None => break,
            }
        }
        *utf16_cursor
    };

    for span in &parse.styles {
        let span_start = span.start as usize;
        let span_end = span.end as usize;
        if span_end <= span_start || span_start < byte_cursor {
            continue;
        }
        let u16_start = advance_to(span_start, &mut byte_cursor, &mut utf16_cursor, &mut chars);
        let u16_end = advance_to(span_end, &mut byte_cursor, &mut utf16_cursor, &mut chars);
        if u16_end <= u16_start {
            continue;
        }

        let color: &NSColor = match span.kind {
            StyleKind::Heading => &heading_color,
            StyleKind::Code | StyleKind::CodeBlock => &code_color,
        };

        unsafe {
            attr.addAttribute_value_range(
                NSForegroundColorAttributeName,
                color.as_ref() as &AnyObject,
                NSRange {
                    location: u16_start as usize,
                    length: (u16_end - u16_start) as usize,
                },
            );
        }
    }
}
