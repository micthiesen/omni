//! Stable, read-only attachment identity.

use std::sync::LazyLock;

use futures::future::BoxFuture;
use omni_core::email::DownloadedAttachment;
use regex::Regex;
use sha2::{Digest as _, Sha256};

use crate::attachment_images::sniff_image;
use crate::mime::Attachment;
use crate::protocol::ImapError;

/// Decoded attachment bytes returned to callers.
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// Source message bytes read for an attachment lookup.
pub const MAX_ATTACHMENT_MESSAGE_BYTES: usize = 20 * 1024 * 1024;

fn regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

static MESSAGE_ID: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r"^<[^<>\s\x00-\x1f\x7f]+@[^<>\s\x00-\x1f\x7f]+>$"));
static PART_ID: LazyLock<Option<Regex>> = LazyLock::new(|| regex(r"^[1-9]\d*(?:\.[1-9]\d*)*$"));
static MIME_TYPE: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r"(?i)^[a-z0-9!#$&^_.+-]+/[a-z0-9!#$&^_.+-]+$"));
static STABLE_ID: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r"^imap-attachment:[a-f0-9]{64}$"));

fn is_match(re: &LazyLock<Option<Regex>>, s: &str) -> bool {
    re.as_ref().is_some_and(|re| re.is_match(s))
}

/// An exact bracketed RFC Message-ID of at most 998 UTF-16 units.
pub fn valid_attachment_message_id(message_id: &str) -> bool {
    omni_core::js::utf16_len(message_id) <= 998 && is_match(&MESSAGE_ID, message_id)
}

/// `imap-attachment:<64 hex>`.
pub fn valid_stable_attachment_id(attachment_id: &str) -> bool {
    is_match(&STABLE_ID, attachment_id)
}

/// mailparser's MIME tree coordinate: a root part without a parent boundary is `1`.
pub fn attachment_part_id(attachment: &Attachment) -> Option<String> {
    match &attachment.part_id {
        None => Some("1".to_owned()),
        Some(id) if is_match(&PART_ID, id) => Some(id.clone()),
        Some(_) => None,
    }
}

/// `imap-attachment:` + sha256(JSON.stringify([messageId, partId])).
pub fn encode_stable_attachment_id(message_id: &str, part_id: &str) -> String {
    let json = omni_core::js::json_stringify(&serde_json::json!([message_id, part_id]));
    format!(
        "imap-attachment:{}",
        hex::encode(Sha256::digest(json.as_bytes()))
    )
}

/// The declared `Content-Type` (never inferred from a filename), lowercased,
/// or `application/octet-stream`.
pub fn declared_attachment_mime_type(attachment: &Attachment) -> String {
    match &attachment.declared_content_type {
        Some(value) if is_match(&MIME_TYPE, value) => value.to_lowercase(),
        _ => "application/octet-stream".to_owned(),
    }
}

/// A PDF header must start within this many leading bytes (the tolerance
/// PDF readers apply to junk before `%PDF-`).
const PDF_HEADER_WINDOW: usize = 1024;
const PDF_MARKER: &[u8] = b"%PDF-";

/// Declared types that say nothing about the format, plus nonstandard names
/// for PDF. A `.pdf` filename has to confirm them.
const UNSPECIFIC_PDF_TYPES: [&str; 8] = [
    "application/octet-stream",
    "binary/octet-stream",
    "application/binary",
    "application/x-pdf",
    "application/acrobat",
    "application/vnd.pdf",
    "text/pdf",
    "text/x-pdf",
];

/// Leading signatures of formats that must never be relabelled as a PDF even
/// when `%PDF-` appears shortly after them.
const FOREIGN_SIGNATURES: [&[u8]; 15] = [
    b"<",
    b"MZ",
    b"\x7fELF",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
    b"\xce\xfa\xed\xfe",
    b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"PK\x03\x04",
    b"#!",
    b"{\\rtf",
    b"\xd0\xcf\x11\xe0",
    b"Rar!",
    b"7z\xbc\xaf",
    b"\x1f\x8b",
];
const MARKUP_TAGS: [&[u8]; 4] = [b"<html", b"<!doctype", b"<script", b"<?xml"];

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Whether the bytes before `%PDF-` belong to another document, program,
/// archive or image, so the attachment is that format and not a PDF.
fn is_foreign_prefix(prefix: &[u8], data: &[u8]) -> bool {
    let body = prefix.strip_prefix(b"\xef\xbb\xbf").unwrap_or(prefix);
    let body = body.trim_ascii_start();
    FOREIGN_SIGNATURES
        .iter()
        .any(|signature| body.starts_with(signature))
        || MARKUP_TAGS
            .iter()
            .any(|tag| contains_ignore_case(prefix, tag))
        || sniff_image(data).is_some()
}

/// Where the PDF starts in an attachment, or `None` when the attachment is
/// not a PDF. Format identity only, not document safety.
///
/// The `%PDF-` header must start within the first `PDF_HEADER_WINDOW`
/// bytes; callers drop the bytes before it. The declared type (parameters
/// and case ignored) must be `application/pdf`, or an unspecific type
/// confirmed by a `.pdf` filename. Content that leads with another format's
/// signature is refused whatever it is called.
pub fn pdf_start(mime_type: &str, filename: &str, data: &[u8]) -> Option<usize> {
    let declared = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let named_pdf = filename
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.trim().eq_ignore_ascii_case("pdf"));
    let claims_pdf = declared == "application/pdf"
        || (named_pdf && UNSPECIFIC_PDF_TYPES.contains(&declared.as_str()));
    if !claims_pdf {
        return None;
    }
    let window = &data[..data.len().min(PDF_HEADER_WINDOW)];
    let start = window
        .windows(PDF_MARKER.len())
        .position(|bytes| bytes == PDF_MARKER)?;
    (!is_foreign_prefix(&data[..start], data)).then_some(start)
}

/// The stable attachment reader seam (the IMAP transport in production).
pub trait AttachmentReader: Send + Sync {
    /// False until the transport has started.
    fn available(&self) -> bool {
        true
    }
    fn fetch_attachment<'a>(
        &'a self,
        message_id: &'a str,
        attachment_id: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Option<DownloadedAttachment>, ImapError>>;
}

/// A display/download filename, never a sender-supplied filesystem path.
pub fn safe_attachment_filename(filename: Option<&str>) -> String {
    let raw = filename.unwrap_or("attachment").replace('\\', "/");
    let last = raw.rsplit('/').next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| {
            let code = u32::from(*c);
            !(code <= 0x1f
                || (0x7f..=0x9f).contains(&code)
                || code == 0x061c
                || (0x200b..=0x200f).contains(&code)
                || (0x202a..=0x202e).contains(&code)
                || (0x2066..=0x2069).contains(&code)
                || code == 0xfeff)
        })
        .collect();
    let trimmed = omni_core::js::trim(&cleaned);
    let without_dots = trimmed.trim_start_matches('.');
    let limited = omni_core::js::utf16_slice(without_dots, 0, 180).into_owned();
    if limited.is_empty() {
        "attachment".to_owned()
    } else {
        limited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_hashes_js_json() {
        let id = encode_stable_attachment_id("<a@b>", "2.1");
        assert!(valid_stable_attachment_id(&id));
        let expected = hex::encode(Sha256::digest(br#"["<a@b>","2.1"]"#));
        assert_eq!(id, format!("imap-attachment:{expected}"));
    }

    #[test]
    fn pdf_identity_needs_a_pdf_claim_and_a_header_near_the_start() {
        let pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n";
        assert_eq!(pdf_start("application/pdf", "scan", pdf), Some(0));
        assert_eq!(
            pdf_start("Application/PDF; name=\"a.pdf\"", "a", pdf),
            Some(0)
        );
        for generic in UNSPECIFIC_PDF_TYPES {
            assert_eq!(pdf_start(generic, "report.PDF", pdf), Some(0), "{generic}");
            assert_eq!(pdf_start(generic, "report.bin", pdf), None, "{generic}");
            assert_eq!(pdf_start(generic, "report", pdf), None, "{generic}");
        }
        assert_eq!(pdf_start("text/html", "report.pdf", pdf), None);
        assert_eq!(pdf_start("image/png", "report.pdf", pdf), None);
        assert_eq!(pdf_start("application/pdf", "a.pdf", b"not a pdf"), None);
        assert_eq!(pdf_start("application/pdf", "a.pdf", b"%PDF"), None);
    }

    #[test]
    fn pdf_header_may_follow_junk_within_the_window_only() {
        let with_prefix = |prefix: &[u8]| [prefix, b"%PDF-1.4\n".as_slice()].concat();
        let bom = with_prefix(b"\xef\xbb\xbf\r\n");
        assert_eq!(pdf_start("application/pdf", "a.pdf", &bom), Some(5));
        let last = with_prefix(&[0u8; PDF_HEADER_WINDOW - 5]);
        assert_eq!(
            pdf_start("application/pdf", "a.pdf", &last),
            Some(PDF_HEADER_WINDOW - 5)
        );
        let late = with_prefix(&[0u8; PDF_HEADER_WINDOW - 4]);
        assert_eq!(pdf_start("application/pdf", "a.pdf", &late), None);
    }

    #[test]
    fn other_formats_carrying_a_pdf_marker_are_not_pdfs() {
        let prefixes: [&[u8]; 9] = [
            b"<html><body>",
            b"\xef\xbb\xbf \n<!DOCTYPE html>",
            b"text first <ScRiPt>alert(1)</script>",
            b"MZ\x90\x00\x03",
            b"\x7fELF\x02\x01",
            b"PK\x03\x04\x14\x00",
            b"#!/bin/sh\n",
            b"\xff\xd8\xff\xe0\x00\x10JFIF",
            b"\x89PNG\r\n\x1a\n",
        ];
        for prefix in prefixes {
            let data = [prefix, b"\n%PDF-1.4\n".as_slice()].concat();
            for (mime, name) in [
                ("application/pdf", "a.pdf"),
                ("application/octet-stream", "a.pdf"),
            ] {
                assert_eq!(pdf_start(mime, name, &data), None, "{prefix:?} {mime}");
            }
        }
    }

    #[test]
    fn message_id_validation() {
        assert!(valid_attachment_message_id("<attachment@example.test>"));
        assert!(!valid_attachment_message_id("bad\r\nidentity"));
        assert!(!valid_attachment_message_id("<no-at>"));
    }
}
