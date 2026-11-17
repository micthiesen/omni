//! Content-Transfer-Encoding decoders with the leniency of libbase64 and
//! Node's `Buffer.from(s, "base64")`, a byte-preserving quoted-printable
//! decoder, plus RFC 3676 format=flowed unfolding (libmime `decodeFlowed`).

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Node `Buffer.from(s, "base64")`: both alphabets, characters outside them
/// ignored, missing padding tolerated, a dangling sixth bit group dropped.
pub(crate) fn node_base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut quad = [0u8; 4];
    let mut filled = 0usize;
    for &c in input {
        if c == b'=' {
            flush_partial(&quad, filled, &mut out);
            filled = 0;
            continue;
        }
        let Some(value) = b64_value(c) else { continue };
        quad[filled] = value;
        filled += 1;
        if filled == 4 {
            out.push((quad[0] << 2) | (quad[1] >> 4));
            out.push((quad[1] << 4) | (quad[2] >> 2));
            out.push((quad[2] << 6) | quad[3]);
            filled = 0;
        }
    }
    flush_partial(&quad, filled, &mut out);
    out
}

fn flush_partial(quad: &[u8; 4], filled: usize, out: &mut Vec<u8>) {
    if filled >= 2 {
        out.push((quad[0] << 2) | (quad[1] >> 4));
    }
    if filled >= 3 {
        out.push((quad[1] << 4) | (quad[2] >> 2));
    }
}

/// libbase64 `Decoder`: keeps only `[A-Za-z0-9+/=]`, then decodes.
pub(crate) fn decode_base64_body(input: &[u8]) -> Vec<u8> {
    let filtered: Vec<u8> = input
        .iter()
        .copied()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
        .collect();
    node_base64(&filtered)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Lenient quoted-printable decoding over bytes: trailing tabs/spaces are
/// removed from every line, soft breaks (`=` + optional blanks + line end, or
/// a final `=`) are dropped, `=XX` becomes a byte, and everything else,
/// including a malformed escape and raw 8-bit bytes, is kept as it is. The
/// input is never read as text: real servers send unencoded bytes >= 0x80 in
/// quoted-printable parts, and the part's charset is applied afterwards.
pub(crate) fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let is_blank = |b: u8| matches!(b, b'\t' | b' ');
    let blanks_end = |from: usize| {
        input[from..]
            .iter()
            .position(|b| !is_blank(*b))
            .map_or(input.len(), |offset| from + offset)
    };
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let byte = input[i];
        if is_blank(byte) {
            let end = blanks_end(i);
            if end < input.len() && !matches!(input[end], b'\r' | b'\n') {
                out.extend_from_slice(&input[i..end]);
            }
            i = end;
            continue;
        }
        if byte == b'=' {
            let rest = &input[blanks_end(i + 1)..];
            if rest.is_empty() {
                break;
            }
            if rest.starts_with(b"\n") || rest.starts_with(b"\r\n") {
                i = input.len() - rest.len() + if rest[0] == b'\n' { 1 } else { 2 };
                continue;
            }
            if let (Some(&a), Some(&b)) = (input.get(i + 1), input.get(i + 2))
                && let (Some(high), Some(low)) = (hex(a), hex(b))
            {
                out.push((high << 4) | low);
                i += 3;
                continue;
            }
        }
        out.push(byte);
        i += 1;
    }
    out
}

/// A uuencoded body: the lines between `begin <mode> <name>` and `end`.
/// `None` without a `begin` line or when a line is truncated, so the caller
/// keeps the body as it is. Each line's first character is its decoded
/// length; a line up to one group short of it (trailing blanks stripped in
/// transit) is zero-filled. Anything shorter is refused: padding on trust
/// would let a body of bare length characters decode to 30 times its size.
pub(crate) fn decode_uuencode(input: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| c.wrapping_sub(b' ') & 0x3f;
    let mut lines = input
        .split(|b| *b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    lines.find(|line| line.starts_with(b"begin "))?;
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for line in lines {
        if line == b"end" {
            break;
        }
        let Some((&length, encoded)) = line.split_first() else {
            continue;
        };
        let length = usize::from(value(length));
        let mut decoded = Vec::with_capacity(length + 2);
        for group in encoded.chunks(4) {
            let mut quad = [0u8; 4];
            for (slot, c) in quad.iter_mut().zip(group) {
                *slot = value(*c);
            }
            decoded.push((quad[0] << 2) | (quad[1] >> 4));
            decoded.push((quad[1] << 4) | (quad[2] >> 2));
            decoded.push((quad[2] << 6) | quad[3]);
        }
        if decoded.len() + 3 < length {
            return None;
        }
        decoded.resize(length, 0);
        out.extend_from_slice(&decoded);
    }
    Some(out)
}

/// libmime `decodeFlowed` over a JS string.
pub(crate) fn decode_flowed(text: &str, del_sp: bool) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut buffer: Option<String> = None;
    for raw in split_crlf_or_lf(text) {
        let soft_break = buffer
            .as_deref()
            .is_some_and(|b| b.ends_with(' ') && !b.ends_with("\n-- ") && b != "-- ");
        if soft_break {
            if let Some(b) = buffer.as_mut() {
                if del_sp {
                    b.pop();
                }
                b.push_str(raw);
            }
        } else {
            if let Some(b) = buffer.take() {
                result.push(b);
            }
            buffer = Some(raw.to_owned());
        }
    }
    if let Some(b) = buffer
        && !b.is_empty()
    {
        result.push(b);
    }
    let joined = result.join("\n");
    // `.replace(/^ /gm, "")`: one leading space at the start of every line.
    let mut out = String::with_capacity(joined.len());
    let mut at_line_start = true;
    for c in joined.chars() {
        if at_line_start && c == ' ' {
            at_line_start = false;
            continue;
        }
        at_line_start = matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
        out.push(c);
    }
    out
}

/// `str.split(/\r?\n/)`.
pub(crate) fn split_crlf_or_lf(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            parts.push(&text[start..end]);
            start = i + 1;
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_lenient_like_node() {
        assert_eq!(node_base64(b"aW1hZ2U="), b"image");
        assert_eq!(node_base64(b"aW1h\r\nZ2U"), b"image");
        assert_eq!(decode_base64_body(b"JVBE Ri0x\r\nLjQ=\r\n"), b"%PDF-1.4");
    }

    #[test]
    fn quoted_printable_decodes_escapes_and_soft_breaks() {
        assert_eq!(
            decode_quoted_printable(b"a=3Db  \r\nc=\r\nd=\ne=  \r\nf="),
            b"a=b\r\ncdef"
        );
    }

    #[test]
    fn quoted_printable_keeps_raw_8bit_bytes() {
        // Raw UTF-8 next to the same character escaped.
        assert_eq!(
            decode_quoted_printable("caf\u{e9} =C3=A9 \u{20ac}=\r\n\u{1f600}".as_bytes()),
            "caf\u{e9} \u{e9} \u{20ac}\u{1f600}".as_bytes()
        );
        // Raw Latin-1 and bytes that are not valid UTF-8 in any charset.
        assert_eq!(
            decode_quoted_printable(b"caf\xe9 =E9\r\n\x80\xff\xc3=\n\xa9"),
            b"caf\xe9 \xe9\r\n\x80\xff\xc3\xa9"
        );
        let every_high_byte: Vec<u8> = (0x80..=0xff).collect();
        assert_eq!(decode_quoted_printable(&every_high_byte), every_high_byte);
    }

    #[test]
    fn quoted_printable_keeps_malformed_escapes() {
        assert_eq!(decode_quoted_printable(b"=ZZ =XYz =4"), b"=ZZ =XYz =4");
        assert_eq!(decode_quoted_printable(b"a=4\r\nb=G1"), b"a=4\r\nb=G1");
        assert_eq!(decode_quoted_printable(b"=\xc3\xa9=41"), b"=\xc3\xa9A");
    }

    #[test]
    fn uuencode_decodes_between_begin_and_end() {
        let body = b"preamble\r\nbegin 644 a.pdf\r\n-)5!$1BTQ+C0*8F]D>0``\r\n`\r\nend\r\n";
        assert_eq!(decode_uuencode(body).unwrap(), b"%PDF-1.4\nbody");
        // Trailing padding stripped in transit.
        let stripped = b"begin 644 a\n#8V%T\n \nend\n";
        assert_eq!(decode_uuencode(stripped).unwrap(), b"cat");
        assert_eq!(decode_uuencode(b"no header here"), None);
        // No `end` line: everything after `begin` is decoded.
        assert_eq!(decode_uuencode(b"begin 644 a\n#8V%T").unwrap(), b"cat");
        // A length character with no data behind it is not padded out.
        assert_eq!(decode_uuencode(b"begin 644 a\n_\n_\nend\n"), None);
        assert_eq!(decode_uuencode(b"begin 644 a\n)8V%T\nend\n"), None);
    }

    #[test]
    fn flowed_text_unfolds_soft_breaks() {
        assert_eq!(
            decode_flowed("one \r\ntwo\r\n-- \r\nsig", false),
            "one two\n-- \nsig"
        );
        assert_eq!(decode_flowed("one  \ntwo", true), "one two");
        assert_eq!(decode_flowed(" >quoted", false), ">quoted");
    }
}
