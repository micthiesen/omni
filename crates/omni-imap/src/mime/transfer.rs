//! Content-Transfer-Encoding decoders with the leniency of libbase64, libqp and
//! Node's `Buffer.from(s, "base64")`, plus RFC 3676 format=flowed unfolding
//! (libmime `decodeFlowed`).

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

fn hex(c: u16) -> Option<u8> {
    let c = u8::try_from(c).ok()?;
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// libqp `decode`: the input is read as UTF-8 (lossy), trailing tabs/spaces
/// are removed from every line, soft breaks (`=` + line end, or a final `=`)
/// are dropped, `=XX` becomes a byte, and every other UTF-16 unit is written
/// as its low byte (so raw 8-bit text is not preserved, exactly like libqp).
pub(crate) fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(input);
    let units: Vec<u16> = text.encode_utf16().collect();

    // `.replace(/[\t ]+$/gm, "")`: strip blanks before any JS line terminator or the end.
    let is_terminator = |u: u16| matches!(u, 0x0a | 0x0d | 0x2028 | 0x2029);
    let mut stripped: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if u == u16::from(b'\t') || u == u16::from(b' ') {
            let start = i;
            while i < units.len() && (units[i] == u16::from(b'\t') || units[i] == u16::from(b' ')) {
                i += 1;
            }
            if i == units.len() || is_terminator(units[i]) {
                continue;
            }
            stripped.extend_from_slice(&units[start..i]);
            continue;
        }
        stripped.push(u);
        i += 1;
    }

    // `.replace(/\=(?:\r?\n|$)/g, "")`.
    let mut soft: Vec<u16> = Vec::with_capacity(stripped.len());
    let mut i = 0;
    while i < stripped.len() {
        if stripped[i] == u16::from(b'=') {
            if i + 1 == stripped.len() {
                break;
            }
            if stripped[i + 1] == 0x0a {
                i += 2;
                continue;
            }
            if stripped[i + 1] == 0x0d && stripped.get(i + 2) == Some(&0x0a) {
                i += 3;
                continue;
            }
        }
        soft.push(stripped[i]);
        i += 1;
    }

    let mut out = Vec::with_capacity(soft.len());
    let mut i = 0;
    while i < soft.len() {
        if soft[i] == u16::from(b'=')
            && let (Some(&a), Some(&b)) = (soft.get(i + 1), soft.get(i + 2))
            && let (Some(high), Some(low)) = (hex(a), hex(b))
        {
            out.push((high << 4) | low);
            i += 3;
            continue;
        }
        out.push(soft[i].to_le_bytes()[0]);
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
    fn quoted_printable_matches_libqp() {
        assert_eq!(
            decode_quoted_printable(b"a=3Db  \r\nc=\r\nd="),
            b"a=b\r\ncd"
        );
        assert_eq!(decode_quoted_printable(b"=XYz"), b"=XYz");
        // Raw UTF-8 text is reduced to the low byte of each UTF-16 unit.
        assert_eq!(decode_quoted_printable("é".as_bytes()), vec![0xe9]);
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
