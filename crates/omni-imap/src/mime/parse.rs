//! `simpleParser` (mailparser 3.9) over the split node tree: processed root
//! headers, inline text and HTML bodies, attachments with mailparser's MIME
//! part ids, and `cid:` image links rewritten to data URIs.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::tz::TimeZone;

use super::address::{Address, decode_addresses, parse_addresses};
use super::charset::decode_body;
use super::date::parse_date;
use super::header_value::{decode_header_line, js_trim, parse_header_value};
use super::splitter::{HeaderLine, Node, SplitError, detect_mime_type, latin1, split};
use super::transfer::{
    decode_base64_body, decode_flowed, decode_quoted_printable, decode_uuencode,
};
use super::words::decode_words;

/// One attachment as mailparser reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// Decoded bytes (transfer encoding removed, no charset conversion).
    pub content: Vec<u8>,
    /// Effective type: the part's type, or one inferred from the filename
    /// when the part is `application/octet-stream`.
    pub content_type: String,
    /// The raw part id: `None` for an unnumbered root part (mailparser `null`).
    pub part_id: Option<String>,
    /// The part's own `Content-Disposition` value, lowercased.
    pub content_disposition: Option<String>,
    pub filename: Option<String>,
    pub content_id: Option<String>,
    pub cid: Option<String>,
    pub related: bool,
    /// Declared `Content-Type` value (last header), encoded words decoded, not lowercased.
    pub declared_content_type: Option<String>,
}

impl Attachment {
    pub fn size(&self) -> usize {
        self.content.len()
    }
}

/// The `ParsedMail` fields this crate consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMail {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// `None` without a References header.
    pub references: Option<Vec<String>>,
    pub subject: Option<String>,
    /// Epoch ms; an unparseable Date header yields `now_ms` like mailparser.
    pub date: Option<i64>,
    pub from: Option<Vec<Address>>,
    /// One entry per header occurrence.
    pub to: Option<Vec<Vec<Address>>>,
    pub cc: Option<Vec<Vec<Address>>>,
    pub bcc: Option<Vec<Vec<Address>>>,
    pub reply_to: Option<Vec<Address>>,
    /// Plain-text body parts joined with `\n`. HTML parts contribute no
    /// derived text here (mailparser would add html-to-text output); callers
    /// that need text for an HTML message render the HTML themselves.
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    pub header_lines: Vec<HeaderLine>,
}

impl ParsedMail {
    /// mapMessage `parsedAddresses`: top-level addresses with an address.
    pub fn flat_addresses(groups: Option<&Vec<Vec<Address>>>) -> Vec<String> {
        groups
            .map(|groups| {
                groups
                    .iter()
                    .flatten()
                    .filter_map(|a| a.address.clone().filter(|s| !s.is_empty()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error(transparent)]
    Split(#[from] SplitError),
}

#[derive(Default)]
struct RootHeaders {
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Option<Vec<String>>,
    subject: Option<String>,
    date: Option<i64>,
    from: Option<Vec<Address>>,
    to: Option<Vec<Vec<Address>>>,
    cc: Option<Vec<Vec<Address>>>,
    bcc: Option<Vec<Vec<Address>>>,
    reply_to: Option<Vec<Address>>,
}

/// mailparser `ensureMessageIDFormat`.
fn ensure_message_id(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(value.len() + 2);
    if !value.starts_with('<') {
        out.push('<');
    }
    out.push_str(value);
    if !value.ends_with('>') {
        out.push('>');
    }
    Some(out)
}

/// `Buffer.from(value, "binary").toString()` on a latin1 view.
fn latin1_to_utf8(value: &str) -> String {
    let bytes: Vec<u8> = value
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn processed_value(line: &HeaderLine) -> (String, String) {
    let (key, value) = decode_header_line(&latin1(&line.raw));
    (key, js_trim(&latin1_to_utf8(js_trim(&value))).to_owned())
}

fn parse_address_value(value: &str) -> Vec<Address> {
    let mut parsed = parse_addresses(value);
    decode_addresses(&mut parsed);
    parsed
}

fn process_root_headers(lines: &[HeaderLine], now_ms: i64, tz: &TimeZone) -> RootHeaders {
    let mut out = RootHeaders::default();
    for line in lines {
        let (_, value) = processed_value(line);
        match line.key.as_str() {
            "message-id" => {
                if let Some(id) = ensure_message_id(&decode_words(&value)) {
                    out.message_id = Some(id);
                }
            }
            "in-reply-to" => {
                if let Some(id) = ensure_message_id(&decode_words(&value)) {
                    out.in_reply_to = Some(id);
                }
            }
            "references" => {
                let ids: Vec<String> = decode_words(&value)
                    .split(|c: char| c.is_whitespace() || c == '\u{feff}')
                    .filter_map(ensure_message_id)
                    .collect();
                out.references.get_or_insert_with(Vec::new).extend(ids);
            }
            "subject" => {
                let decoded = decode_words(&value);
                if !decoded.is_empty() {
                    out.subject = Some(decoded);
                }
            }
            "date" => out.date = Some(parse_date(&value, tz).unwrap_or(now_ms)),
            "from" => out.from = Some(parse_address_value(&value)),
            "reply-to" => out.reply_to = Some(parse_address_value(&value)),
            "to" => out
                .to
                .get_or_insert_with(Vec::new)
                .push(parse_address_value(&value)),
            "cc" => out
                .cc
                .get_or_insert_with(Vec::new)
                .push(parse_address_value(&value)),
            "bcc" => out
                .bcc
                .get_or_insert_with(Vec::new)
                .push(parse_address_value(&value)),
            _ => {}
        }
    }
    out
}

/// Last `content-id` and last structured `content-type` value of a part.
fn part_header_values(lines: &[HeaderLine]) -> (Option<String>, Option<String>) {
    let mut content_id = None;
    let mut content_type = None;
    for line in lines {
        match line.key.as_str() {
            "content-id" => {
                let (_, value) = processed_value(line);
                if !value.is_empty() {
                    content_id = Some(value);
                }
            }
            "content-type" => {
                let (_, value) = processed_value(line);
                let parsed = parse_header_value(&value);
                content_type = Some(parsed.value.map(|v| decode_words(&v)).unwrap_or_default());
            }
            _ => {}
        }
    }
    (content_id, content_type)
}

/// mailparser `_getPartId` over first-seen boundaries.
struct PartIds {
    boundaries: Vec<(usize, u64)>,
}

impl PartIds {
    fn next(&mut self, owner: usize) -> String {
        let index = match self.boundaries.iter().position(|(o, _)| *o == owner) {
            Some(index) => {
                self.boundaries[index].1 += 1;
                index
            }
            None => {
                self.boundaries.push((owner, 1));
                self.boundaries.len() - 1
            }
        };
        self.boundaries[..=index]
            .iter()
            .map(|(_, count)| count.to_string())
            .collect::<Vec<_>>()
            .join(".")
    }
}

const TEXT_TYPES: [&str; 3] = ["text/plain", "text/html", "message/delivery-status"];

fn decode_transfer(node: &Node) -> Vec<u8> {
    match node.encoding.as_str() {
        "base64" => decode_base64_body(&node.body),
        "quoted-printable" => decode_quoted_printable(&node.body),
        _ => node.body.clone(),
    }
}

/// mailparser leaves uuencoded parts encoded, and text bodies keep that; an
/// attachment is only usable decoded.
fn decode_attachment(node: &Node) -> Vec<u8> {
    match node.encoding.as_str() {
        "uuencode" | "x-uuencode" | "x-uue" | "uue" => {
            decode_uuencode(&node.body).unwrap_or_else(|| node.body.clone())
        }
        _ => decode_transfer(node),
    }
}

struct TextNode {
    content: String,
    content_type: String,
}

/// mailparser `textToHtml` without linkification.
fn text_to_html(text: &str) -> String {
    let encoded = encode_html(text);
    let normalized = encoded.replace("\r\n", "\n");
    let trimmed = js_trim(&normalized);
    let mut lines: Vec<&str> = trimmed.split('\n').collect();
    for line in &mut lines {
        *line = line.trim_end_matches([' ', '\t']);
    }
    let joined = lines.join("\n");
    let trimmed = js_trim(&joined);
    let mut out = String::new();
    let mut newline_run = 0usize;
    for c in trimmed.chars() {
        if c == '\n' {
            newline_run += 1;
            continue;
        }
        if newline_run == 1 {
            out.push_str("<br/>");
        } else if newline_run > 1 {
            out.push_str("</p><p>");
        }
        newline_run = 0;
        out.push(c);
    }
    format!("<p>{out}</p>")
}

/// `he.encode(str, { useNamedReferences: true })` for the characters it
/// escapes in practice: markup-significant ASCII and non-ASCII as named or
/// hexadecimal references.
fn encode_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            '`' => out.push_str("&#x60;"),
            c if c.is_ascii() => out.push(c),
            c => out.push_str(&format!("&#x{:X};", u32::from(c))),
        }
    }
    out
}

/// Parses a complete RFC 822 message like `simpleParser(source)`, reading
/// zone-less Date headers in the process time zone as node does.
pub fn parse_message(source: &[u8], now_ms: i64) -> Result<ParsedMail, ParseError> {
    parse_message_in(source, now_ms, &TimeZone::system())
}

/// [`parse_message`] with zone-less Date headers read as local time in `tz`.
pub fn parse_message_in(
    source: &[u8],
    now_ms: i64,
    tz: &TimeZone,
) -> Result<ParsedMail, ParseError> {
    let split = split(source)?;
    let nodes = &split.nodes;

    let mut part_ids = PartIds {
        boundaries: Vec::new(),
    };
    let mut attachments: Vec<(usize, Attachment)> = Vec::new();
    let mut texts: Vec<(usize, TextNode)> = Vec::new();
    let mut has_html = false;
    let mut has_text = false;

    for &index in &split.emitted {
        let node = &nodes[index];
        let part_id = node.boundary_owner.map(|owner| part_ids.next(owner));
        let root = index == 0;
        if node.content_type.as_deref() == Some("message/rfc822") && node.message_node == Some(true)
        {
            continue;
        }
        let content_type = node
            .content_type
            .clone()
            .or_else(|| root.then(|| "text/plain".to_owned()));
        let Some(content_type) = content_type else {
            // A part with an empty Content-Type value is neither text nor multipart.
            push_attachment(&mut attachments, index, node, String::new(), part_id);
            continue;
        };
        if content_type.starts_with("multipart/") {
            continue;
        }
        let mut disposition = node.disposition.clone();
        if disposition
            .as_deref()
            .is_some_and(|d| d != "attachment" && d != "inline")
        {
            disposition = Some("attachment".to_owned());
        }
        let is_text_type = TEXT_TYPES.contains(&content_type.as_str());
        let effective = match disposition {
            None if !is_text_type => "attachment".to_owned(),
            Some(d) => d,
            None => "inline".to_owned(),
        };
        let is_attachment = !is_text_type || effective != "inline";
        if is_attachment {
            push_attachment(&mut attachments, index, node, content_type, part_id);
            continue;
        }
        match content_type.as_str() {
            "text/html" => has_html = true,
            "text/plain" | "message/delivery-status" => has_text = true,
            _ => {}
        }
        let mut decoded = decode_transfer(node);
        if node.flowed {
            let flowed = decode_flowed(&latin1(&decoded), node.del_sp);
            decoded = flowed
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                .collect();
        }
        let content = decode_body(&decoded, node.charset.as_deref()).replace("\r\n", "\n");
        if content.is_empty() {
            continue;
        }
        texts.push((
            index,
            TextNode {
                content,
                content_type,
            },
        ));
    }

    // getTextContent: depth-first over the node tree in child order.
    let mut text_parts: Vec<String> = Vec::new();
    let mut html_parts: Vec<String> = Vec::new();
    let emitted: std::collections::HashSet<usize> = split.emitted.iter().copied().collect();
    let children = |parent: usize| -> Vec<usize> {
        split
            .emitted
            .iter()
            .copied()
            .filter(|&i| i != parent && effective_parent(nodes, &emitted, i) == Some(parent))
            .collect()
    };
    let mut stack: Vec<(usize, bool)> = vec![(0, false)];
    while let Some((index, alternative)) = stack.pop() {
        let show_meta = nodes[index]
            .parent
            .is_some_and(|p| nodes[p].content_type.as_deref() == Some("message/rfc822"));
        if show_meta {
            let meta = meta_entries(&nodes[index].headers, now_ms, tz);
            if has_html {
                html_parts.push(meta_html(&meta));
            }
            if has_text {
                text_parts.push(meta_text(&meta));
            }
        }
        if let Some((_, text)) = texts.iter().find(|(i, _)| *i == index) {
            match text.content_type.as_str() {
                "text/plain" | "message/delivery-status" => {
                    text_parts.push(text.content.clone());
                    if !alternative && has_html {
                        html_parts.push(text_to_html(&text.content));
                    }
                }
                "text/html" => html_parts.push(text.content.clone()),
                _ => {}
            }
        }
        let alternative =
            alternative || nodes[index].content_type.as_deref() == Some("multipart/alternative");
        for child in children(index).into_iter().rev() {
            stack.push((child, alternative));
        }
    }

    let mut html = (!html_parts.is_empty()).then(|| html_parts.join("<br/>\n"));
    let text = (!text_parts.is_empty()).then(|| text_parts.join("\n"));

    let attachments: Vec<Attachment> = attachments
        .into_iter()
        .map(|(index, mut attachment)| {
            let mut parent = effective_parent(nodes, &emitted, index);
            while let Some(p) = parent {
                if nodes[p].content_type.as_deref() == Some("multipart/related")
                    && attachment.content_id.is_some()
                {
                    attachment.related = true;
                }
                parent = effective_parent(nodes, &emitted, p);
            }
            attachment
        })
        .collect();

    if let Some(body) = html.take() {
        html = Some(replace_cid_links(&body, &attachments));
    }

    let root = &nodes[0];
    let headers = process_root_headers(&root.headers, now_ms, tz);
    Ok(ParsedMail {
        message_id: headers.message_id,
        in_reply_to: headers.in_reply_to,
        references: headers.references,
        subject: headers.subject,
        date: headers.date,
        from: headers.from,
        to: headers.to,
        cc: headers.cc,
        bcc: headers.bcc,
        reply_to: headers.reply_to,
        text,
        html,
        attachments,
        header_lines: root.headers.clone(),
    })
}

/// One `showMeta` row of an embedded message.
enum MetaValue {
    Addresses(Vec<Address>),
    Text(String),
    Date(i64),
}

/// From, Subject, Date, To, Cc, Bcc of an embedded message (last occurrence).
fn meta_entries(
    lines: &[HeaderLine],
    now_ms: i64,
    tz: &TimeZone,
) -> Vec<(&'static str, MetaValue)> {
    let headers = process_root_headers(lines, now_ms, tz);
    let mut out = Vec::new();
    if let Some(from) = headers.from {
        out.push(("From", MetaValue::Addresses(from)));
    }
    if let Some(subject) = headers.subject {
        out.push(("Subject", MetaValue::Text(subject)));
    }
    if let Some(date) = headers.date {
        out.push(("Date", MetaValue::Date(date)));
    }
    for (key, value) in [("To", headers.to), ("Cc", headers.cc), ("Bcc", headers.bcc)] {
        if let Some(last) = value.and_then(|mut v| v.pop()) {
            out.push((key, MetaValue::Addresses(last)));
        }
    }
    out
}

/// `Date#toUTCString`.
fn utc_string(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|ts| ts.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_else(|_| "Invalid Date".to_owned())
}

/// mailparser `getAddressesText`.
fn addresses_text(list: &[Address]) -> String {
    list.iter()
        .map(|a| {
            let mut out = String::new();
            if !a.name.is_empty() {
                out.push_str(&format!("\"{}\"", a.name));
                if a.group.is_some() {
                    out.push_str(": ");
                }
            }
            if let Some(address) = a.address.as_deref().filter(|s| !s.is_empty()) {
                if a.name.is_empty() {
                    out.push_str(address);
                } else {
                    out.push_str(&format!(" <{address}>"));
                }
            }
            if let Some(group) = &a.group {
                out.push_str(&addresses_text(group));
                out.push(';');
            }
            out
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `he.encode` with its default hexadecimal references.
fn he_hex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' | '<' | '>' | '"' | '\'' | '`' => out.push_str(&format!("&#x{:X};", u32::from(c))),
            c if c.is_ascii() => out.push(c),
            c => out.push_str(&format!("&#x{:X};", u32::from(c))),
        }
    }
    out
}

/// mailparser `getAddressesHTML`.
fn addresses_html(list: &[Address]) -> String {
    list.iter()
        .map(|a| {
            let mut out = "<span class=\"mp_address_group\">".to_owned();
            if !a.name.is_empty() {
                out.push_str("<span class=\"mp_address_name\">");
                out.push_str(&he_hex(&a.name));
                if a.group.is_some() {
                    out.push_str(": ");
                }
                out.push_str("</span>");
            }
            if let Some(address) = a.address.as_deref().filter(|s| !s.is_empty()) {
                let link = format!(
                    "<a href=\"mailto:{0}\" class=\"mp_address_email\">{0}</a>",
                    he_hex(address)
                );
                if a.name.is_empty() {
                    out.push_str(&link);
                } else {
                    out.push_str(&format!(" &lt;{link}&gt;"));
                }
            }
            if let Some(group) = &a.group {
                out.push_str(&addresses_html(group));
                out.push(';');
            }
            out.push_str("</span>");
            out
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn meta_text(meta: &[(&'static str, MetaValue)]) -> String {
    let rows: Vec<String> = meta
        .iter()
        .map(|(key, value)| {
            let value = match value {
                MetaValue::Addresses(list) => addresses_text(list),
                MetaValue::Date(ms) => utc_string(*ms),
                MetaValue::Text(text) => text.clone(),
            };
            format!("{key}: {value}")
        })
        .collect();
    format!("\n{}\n", rows.join("\n"))
}

fn meta_html(meta: &[(&'static str, MetaValue)]) -> String {
    let rows: Vec<String> = meta
        .iter()
        .map(|(key, value)| {
            let value = match value {
                MetaValue::Addresses(list) => addresses_html(list),
                MetaValue::Date(ms) => utc_string(*ms),
                MetaValue::Text(text) => format!("<strong>{}</strong>", he_hex(text)),
            };
            format!(
                "<tr><td class=\"mp_head_key\">{}:</td><td class=\"mp_head_value\">{value}<td></tr>",
                he_hex(key)
            )
        })
        .collect();
    format!("<table class=\"mp_head\">{}<table>", rows.join("\n"))
}

fn effective_parent(
    nodes: &[Node],
    emitted: &std::collections::HashSet<usize>,
    index: usize,
) -> Option<usize> {
    let mut parent = nodes[index].parent;
    while let Some(p) = parent {
        if emitted.contains(&p) || p == 0 {
            return Some(p);
        }
        parent = nodes[p].parent;
    }
    None
}

fn push_attachment(
    attachments: &mut Vec<(usize, Attachment)>,
    index: usize,
    node: &Node,
    content_type: String,
    part_id: Option<String>,
) {
    let mut content_type = content_type;
    if content_type == "application/octet-stream"
        && let Some(filename) = &node.filename
    {
        content_type = detect_mime_type(filename);
    }
    let (content_id, declared) = part_header_values(&node.headers);
    let cid = content_id.as_deref().map(|id| {
        let trimmed = id.trim();
        let stripped = trimmed.strip_prefix('<').unwrap_or(trimmed);
        let stripped = stripped.strip_suffix('>').unwrap_or(stripped);
        stripped.trim().to_owned()
    });
    attachments.push((
        index,
        Attachment {
            content: decode_attachment(node),
            content_type,
            part_id,
            content_disposition: node.disposition.clone(),
            filename: node.filename.clone(),
            content_id,
            cid,
            related: false,
            declared_content_type: declared,
        },
    ));
}

/// mailparser `updateImageLinks`: `cid:` references to image attachments
/// become `data:` URIs.
fn replace_cid_links(html: &str, attachments: &[Attachment]) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i + 4 <= bytes.len() {
        let at_word_boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        if at_word_boundary && &bytes[i..i + 4] == b"cid:" {
            let start = i + 4;
            let mut end = start;
            while end < bytes.len()
                && end - start < 256
                && !matches!(
                    bytes[end],
                    b'\'' | b'"' | b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c
                )
            {
                end += 1;
            }
            // Keep to char boundaries when a non-ASCII sequence is cut.
            while end > start && !html.is_char_boundary(end) {
                end -= 1;
            }
            if end > start {
                let cid = &html[start..end];
                if let Some(attachment) = attachments
                    .iter()
                    .find(|a| a.cid.as_deref() == Some(cid) && is_image_type(&a.content_type))
                {
                    out.push_str(&html[last..i]);
                    out.push_str("data:");
                    out.push_str(&attachment.content_type);
                    out.push_str(";base64,");
                    out.push_str(&STANDARD.encode(&attachment.content));
                    last = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&html[last..]);
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_image_type(content_type: &str) -> bool {
    content_type
        .to_ascii_lowercase()
        .strip_prefix("image/")
        .is_some_and(|sub| !sub.is_empty() && sub.bytes().all(is_word_byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_to_html_paragraphs() {
        assert_eq!(
            text_to_html("a\nb\n\nc <x>"),
            "<p>a<br/>b</p><p>c &lt;x&gt;</p>"
        );
    }
}
