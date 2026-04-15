//! Lenient JSON pretty-printer.
//!
//! Byte-level state machine: tokenises the input just enough to recognise
//! structural punctuation and string boundaries, and re-emits with
//! 2-space indentation and a newline after each comma. Whitespace in the
//! input is ignored; everything else is copied verbatim, so malformed
//! input produces the closest thing to "re-indented original" we can
//! give without re-parsing.
//!
//! Runs on the worker thread, same order of magnitude as the parser
//! (linear over the input, one pass, no allocations per token beyond
//! the growing output buffer).

const INDENT: &[u8] = b"  ";

pub fn prettify(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 4);
    let mut depth: usize = 0;
    let mut i: usize = 0;
    let n = input.len();

    while i < n {
        let b = input[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'{' | b'[' => {
                out.push(b);
                i += 1;
                // Peek past whitespace — if the next non-ws byte closes
                // this container, emit an empty `{}`/`[]` on one line.
                let mut j = i;
                while j < n && matches!(input[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                let close = if b == b'{' { b'}' } else { b']' };
                if j < n && input[j] == close {
                    out.push(close);
                    i = j + 1;
                } else {
                    depth += 1;
                    write_indent(&mut out, depth);
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                write_indent(&mut out, depth);
                out.push(b);
                i += 1;
            }
            b',' => {
                out.push(b',');
                write_indent(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.push(b':');
                out.push(b' ');
                i += 1;
            }
            b'"' => {
                copy_string(input, &mut i, &mut out);
            }
            _ => {
                // Number, literal, or garbage — copy through to the next
                // structural character.
                while i < n {
                    let c = input[i];
                    if matches!(
                        c,
                        b' ' | b'\t'
                            | b'\n'
                            | b'\r'
                            | b','
                            | b'}'
                            | b']'
                            | b':'
                            | b'"'
                            | b'{'
                            | b'['
                    ) {
                        break;
                    }
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
    out
}

fn write_indent(out: &mut Vec<u8>, depth: usize) {
    out.push(b'\n');
    for _ in 0..depth {
        out.extend_from_slice(INDENT);
    }
}

fn copy_string(input: &[u8], pos: &mut usize, out: &mut Vec<u8>) {
    // Opening quote.
    out.push(input[*pos]);
    *pos += 1;
    while *pos < input.len() {
        let c = input[*pos];
        out.push(c);
        *pos += 1;
        if c == b'\\' && *pos < input.len() {
            // Copy the escape byte (or the first byte of \u…).
            out.push(input[*pos]);
            *pos += 1;
        } else if c == b'"' {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &[u8]) -> String {
        String::from_utf8(prettify(src)).unwrap()
    }

    #[test]
    fn empty_containers_stay_inline() {
        assert_eq!(run(b"{}"), "{}");
        assert_eq!(run(b"[]"), "[]");
        assert_eq!(run(b"{  }"), "{}");
        assert_eq!(run(b"[ \n ]"), "[]");
    }

    #[test]
    fn object_with_one_field() {
        assert_eq!(run(br#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn nested_object() {
        let got = run(br#"{"a":{"b":1}}"#);
        assert_eq!(got, "{\n  \"a\": {\n    \"b\": 1\n  }\n}");
    }

    #[test]
    fn array_with_items() {
        let got = run(b"[1,2,3]");
        assert_eq!(got, "[\n  1,\n  2,\n  3\n]");
    }

    #[test]
    fn mixed_nesting() {
        let got = run(br#"{"a":1,"b":[1,2,{"c":3}]}"#);
        let expected = "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    {\n      \"c\": 3\n    }\n  ]\n}";
        assert_eq!(got, expected);
    }

    #[test]
    fn strings_are_copied_verbatim_including_escapes() {
        let got = run(br#"{"k":"hello\"world\\x"}"#);
        assert_eq!(got, "{\n  \"k\": \"hello\\\"world\\\\x\"\n}");
    }

    #[test]
    fn input_whitespace_is_collapsed() {
        let got = run(b"{\n  \"a\"  :   1  }");
        assert_eq!(got, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn already_pretty_stays_pretty() {
        let input = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        let got = run(input.as_bytes());
        assert_eq!(got, input);
    }

    #[test]
    fn literals_and_numbers() {
        let got = run(b"[true,false,null,-1.5e10]");
        assert_eq!(got, "[\n  true,\n  false,\n  null,\n  -1.5e10\n]");
    }

    #[test]
    fn empty_nested() {
        let got = run(br#"{"a":{},"b":[]}"#);
        assert_eq!(got, "{\n  \"a\": {},\n  \"b\": []\n}");
    }

    #[test]
    #[ignore] // run with `cargo test --release -- --ignored bench_prettify --nocapture`
    fn bench_prettify() {
        // Synthetic ~40 MB minified array of small objects — the shape
        // that kills interactive JSON viewers.
        let mut src = String::with_capacity(64 * 1024 * 1024);
        src.push('[');
        for i in 0..400_000 {
            if i > 0 {
                src.push(',');
            }
            src.push_str(&format!(
                "{{\"id\":{},\"name\":\"row-{}\",\"value\":{}.{},\"ok\":{}}}",
                i,
                i,
                i,
                i % 1000,
                i % 2 == 0
            ));
        }
        src.push(']');
        let bytes = src.as_bytes();
        let size_mb = bytes.len() as f64 / (1024.0 * 1024.0);

        let t0 = std::time::Instant::now();
        let out = prettify(bytes);
        let dt = t0.elapsed();

        let out_mb = out.len() as f64 / (1024.0 * 1024.0);
        eprintln!(
            "prettified {:.1} MB -> {:.1} MB in {:?} ({:.0} MB/s in)",
            size_mb,
            out_mb,
            dt,
            size_mb / dt.as_secs_f64()
        );
    }
}
