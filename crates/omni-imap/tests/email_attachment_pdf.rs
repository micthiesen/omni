//! Which MIME parts `email_attachment_get` returns as a PDF, end to end:
//! a synthetic message in the fake mailbox, the real transport and parser,
//! and the tool. The sender's declaration is often loose (a generic type, an
//! unusual transfer encoding, stray bytes before the header), so identity
//! rests on the content, confirmed by the declared type or a `.pdf` name.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use omni_imap::archive_service::ArchiveService;
use omni_imap::attachments::encode_stable_attachment_id;
use omni_imap::compose::ComposeService;
use omni_imap::compose_attachments::{OutgoingAttachmentReference, resolve_outgoing_attachments};
use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_imap::mcp_tools::{AttachmentReader, ToolDeps, email_tools};
use omni_imap::mime::parse_message;
use omni_mcp_kit::{ToolContext, ToolOutput};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

const MESSAGE_ID: &str = "<5cab0000.1c69fb81.a4bf2.0000SMTPIN_ADDED_MISSING@mx.example.test>";
/// The boundary itself starts with two hyphens and is declared unquoted.
const BOUNDARY: &str = "--boundary_0_00000000-0000-4000-8000-000000000000";
const PDF_NAME: &str = "00000000-0000-4000-8000-000000000001.pdf";

/// A binary-looking synthetic PDF: header, the customary high-byte comment
/// line, bare LF line ends and a CRLF after `%%EOF`.
fn pdf() -> Vec<u8> {
    let mut data = b"%PDF-1.7 \n%\xe2\xe3\xcf\xd3 \n1 0 obj \n<< \n/Type /Catalog \n>> \n".to_vec();
    data.extend((0..=255u8).cycle().take(900));
    data.extend_from_slice(b"\nstartxref \n42 \n%%EOF\r\n");
    data
}

/// multipart/mixed: a quoted-printable text part, then part 2 with the given
/// headers and encoded body. Part 2 has no Content-Disposition unless given.
fn message(part_headers: &[&str], encoded_body: &str) -> Vec<u8> {
    let mut lines = vec![
        format!("Message-ID: {MESSAGE_ID}"),
        "From: sender@example.test".to_owned(),
        "Subject: synthetic".to_owned(),
        "MIME-Version: 1.0".to_owned(),
        format!("Content-Type: multipart/mixed; boundary={BOUNDARY}"),
        String::new(),
        String::new(),
        format!("--{BOUNDARY}"),
        "Content-Type: text/plain; charset=us-ascii".to_owned(),
        "Content-Transfer-Encoding: quoted-printable".to_owned(),
        String::new(),
        "Synthetic body=".to_owned(),
        " text.".to_owned(),
        format!("--{BOUNDARY}"),
    ];
    lines.extend(part_headers.iter().map(|h| (*h).to_owned()));
    lines.push(String::new());
    lines.push(encoded_body.to_owned());
    lines.push(format!("--{BOUNDARY}--"));
    lines.push(String::new());
    lines.join("\r\n").into_bytes()
}

fn base64_lines(data: &[u8], width: usize) -> String {
    STANDARD
        .encode(data)
        .as_bytes()
        .chunks(width)
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn octet_stream_headers() -> Vec<String> {
    vec![
        format!("Content-Type: application/octet-stream; name={PDF_NAME}"),
        "Content-Transfer-Encoding: base64".to_owned(),
    ]
}

/// Quoted-printable with every byte outside printable ASCII (and `=`)
/// escaped, alternating hex case, and soft breaks at uneven short widths.
fn quoted_printable_uneven(data: &[u8]) -> String {
    let mut out = String::new();
    let mut column = 0;
    let mut width = 7;
    for (index, byte) in data.iter().enumerate() {
        let token = match byte {
            b'!'..=b'<' | b'>'..=b'~' => (*byte as char).to_string(),
            _ if index % 2 == 0 => format!("={byte:02X}"),
            _ => format!("={byte:02x}"),
        };
        if column + token.len() > width {
            out.push_str("=\r\n");
            column = 0;
            width = if width == 7 { 61 } else { 7 };
        }
        column += token.len();
        out.push_str(&token);
    }
    out
}

fn uuencode(data: &[u8]) -> String {
    let symbol = |v: u8| if v == 0 { '`' } else { (v + 32) as char };
    let mut out = String::from("begin 644 synthetic.pdf\r\n");
    for line in data.chunks(45) {
        out.push(symbol(line.len() as u8));
        for group in line.chunks(3) {
            let b = [
                group[0],
                group.get(1).copied().unwrap_or(0),
                group.get(2).copied().unwrap_or(0),
            ];
            out.push(symbol(b[0] >> 2));
            out.push(symbol(((b[0] << 4) | (b[1] >> 4)) & 0x3f));
            out.push(symbol(((b[1] << 2) | (b[2] >> 6)) & 0x3f));
            out.push(symbol(b[2] & 0x3f));
        }
        out.push_str("\r\n");
    }
    out.push_str("`\r\nend");
    out
}

struct Read {
    harness: common::Harness,
    result: Result<Map<String, Value>, String>,
}

impl Read {
    fn pdf(&self) -> &Map<String, Value> {
        self.result.as_ref().unwrap()
    }

    fn blob(&self) -> Vec<u8> {
        STANDARD
            .decode(self.pdf()["blob"].as_str().unwrap())
            .unwrap()
    }

    fn error(&self) -> &str {
        self.result.as_ref().unwrap_err()
    }
}

/// Files the message in Archive and reads part 2 through the tool.
async fn read_part_2(source: Vec<u8>) -> Read {
    let server = FakeServer::default();
    server
        .folder("INBOX", 1, None)
        .folder("Archive", 2, Some("\\Archive"));
    server.put(
        "Archive",
        2209,
        FakeMessage::new(source).envelope_id(Some(MESSAGE_ID)),
    );
    let harness = common::harness(server).await;
    let tools = email_tools(ToolDeps {
        compose: ComposeService::new(
            harness.store.store.clone(),
            harness.clock.clone(),
            None,
            None,
        ),
        archive: ArchiveService::new(harness.store.store.clone(), harness.clock.clone()),
        archive_transport: None,
        attachments: Some(Arc::new(harness.transport.clone()) as Arc<dyn AttachmentReader>),
        tracker: harness.tracker.clone(),
    })
    .unwrap();
    let tool = tools
        .into_iter()
        .find(|t| t.meta.name == "email_attachment_get")
        .unwrap();
    let output = tool
        .handler
        .call(
            json!({
                "messageId": MESSAGE_ID,
                "attachmentId": encode_stable_attachment_id(MESSAGE_ID, "2"),
            }),
            ToolContext {
                call_id: "t".to_owned(),
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let result = match output {
        Ok(ToolOutput::Custom {
            structured,
            content,
        }) => {
            assert_eq!(content[1]["type"], json!("resource"));
            assert_eq!(content[1]["resource"]["mimeType"], json!("application/pdf"));
            assert_eq!(content[1]["resource"]["blob"], structured["blob"]);
            Ok(structured)
        }
        Ok(_) => panic!("expected custom content"),
        Err(error) => Err(error.message),
    };
    Read { harness, result }
}

fn assert_returns(read: &Read, expected: &[u8]) {
    let pdf = read.pdf();
    assert_eq!(read.blob(), expected);
    assert_eq!(pdf["mimeType"], json!("application/pdf"));
    assert_eq!(pdf["size"], json!(expected.len()));
    let sha256 = hex::encode(Sha256::digest(expected));
    assert_eq!(pdf["sha256"], json!(sha256));
    assert_eq!(
        pdf["attachmentReference"],
        json!({
            "messageId": MESSAGE_ID,
            "attachmentId": encode_stable_attachment_id(MESSAGE_ID, "2"),
            "sha256": sha256,
        })
    );
}

#[tokio::test]
async fn returns_an_octet_stream_part_named_pdf_with_no_disposition() {
    let headers = octet_stream_headers();
    let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
    let source = message(&headers, &base64_lines(&pdf(), 76));

    // The shape email_search reports: part 2, no disposition, a type inferred
    // from the name while the declared type stays generic.
    let parsed = parse_message(&source, common::NOW_MS).unwrap();
    assert_eq!(parsed.attachments.len(), 1);
    let part = &parsed.attachments[0];
    assert_eq!(part.part_id.as_deref(), Some("2"));
    assert_eq!(part.content_disposition, None);
    assert_eq!(part.content_type, "application/pdf");
    assert_eq!(
        part.declared_content_type.as_deref(),
        Some("application/octet-stream")
    );

    let read = read_part_2(source).await;
    assert_returns(&read, &pdf());
    assert_eq!(read.pdf()["filename"], json!(PDF_NAME));
    // Read-only: EXAMINE only, and nothing but searches and fetches.
    assert!(read.harness.server.calls().iter().all(|call| matches!(
        call,
        FakeCall::Select {
            read_only: true,
            ..
        } | FakeCall::Search { .. }
            | FakeCall::Fetch { .. }
            | FakeCall::List
    )));
}

#[tokio::test]
async fn accepts_a_generic_or_missing_type_only_with_a_pdf_filename() {
    let body = base64_lines(&pdf(), 76);
    let encoding = "Content-Transfer-Encoding: base64";
    for headers in [
        vec![
            "Content-Type: APPLICATION/PDF; name=\"scan\"; x-unix-mode=0644",
            encoding,
        ],
        vec!["Content-Type: application/x-pdf; name=\"a.PDF\"", encoding],
        vec!["Content-Type: binary/octet-stream; name=a.pdf", encoding],
        vec![
            "Content-Disposition: attachment; filename=\"a.pdf\"",
            encoding,
        ],
    ] {
        let read = read_part_2(message(&headers, &body)).await;
        assert_returns(&read, &pdf());
    }
    for headers in [
        vec![
            "Content-Type: application/octet-stream; name=a.bin",
            encoding,
        ],
        vec![
            "Content-Type: application/octet-stream",
            "Content-Disposition: attachment",
            encoding,
        ],
        vec!["Content-Type: application/zip; name=a.pdf", encoding],
    ] {
        let read = read_part_2(message(&headers, &body)).await;
        assert!(read.error().contains("not a PDF"), "{headers:?}");
    }
}

#[tokio::test]
async fn drops_bytes_before_the_pdf_header_and_pins_the_returned_bytes() {
    let mut stored = b"\xef\xbb\xbf\r\nX-Scanner-Job: 17\r\n\x00\x00".to_vec();
    stored.extend_from_slice(&pdf());
    let source = message(
        &[
            "Content-Type: application/pdf; name=\"scan.pdf\"",
            "Content-Transfer-Encoding: base64",
        ],
        &base64_lines(&stored, 76),
    );
    let read = read_part_2(source).await;
    assert_returns(&read, &pdf());

    // A send re-reads the part and must arrive at the reviewed bytes.
    let reader = Arc::new(read.harness.transport.clone()) as Arc<dyn AttachmentReader>;
    let reference: OutgoingAttachmentReference =
        serde_json::from_value(read.pdf()["attachmentReference"].clone()).unwrap();
    let resolved = resolve_outgoing_attachments(Some(&reader), &[reference])
        .await
        .unwrap();
    assert_eq!(resolved[0].file.content, pdf());
    assert_eq!(resolved[0].metadata.sha256, read.pdf()["sha256"]);
}

#[tokio::test]
async fn refuses_a_pdf_header_that_starts_after_the_first_1024_bytes() {
    let mut stored = vec![b' '; 1020];
    stored.extend_from_slice(&pdf());
    let source = message(
        &[
            "Content-Type: application/pdf; name=\"late.pdf\"",
            "Content-Transfer-Encoding: base64",
        ],
        &base64_lines(&stored, 76),
    );
    assert!(read_part_2(source).await.error().contains("not a PDF"));
}

#[tokio::test]
async fn decodes_base64_wrapped_at_odd_widths_with_blanks_and_no_padding() {
    let data = pdf();
    assert_ne!(data.len() % 3, 0, "the fixture must need padding");
    let unpadded = base64_lines(&data, 76);
    let unpadded = unpadded.trim_end_matches('=');
    let ragged: String = unpadded
        .replace("\r\n", "")
        .as_bytes()
        .chunks(5)
        .enumerate()
        .map(|(index, chunk)| {
            let end = match index % 3 {
                0 => " \r\n",
                1 => "\n\t",
                _ => "\r\n\r\n",
            };
            format!("{}{end}", String::from_utf8(chunk.to_vec()).unwrap())
        })
        .collect();
    for body in [base64_lines(&data, 998), base64_lines(&data, 1), ragged] {
        let headers = [
            "Content-Type: application/pdf; name=\"a.pdf\"",
            "Content-Transfer-Encoding: BASE64 (wrapped by a gateway)",
        ];
        assert_returns(&read_part_2(message(&headers, &body)).await, &data);
    }
}

#[tokio::test]
async fn decodes_quoted_printable_with_uneven_soft_breaks() {
    let headers = [
        "Content-Type: application/pdf; name=\"a.pdf\"",
        "Content-Transfer-Encoding: Quoted-Printable",
    ];
    let source = message(&headers, &quoted_printable_uneven(&pdf()));
    assert_returns(&read_part_2(source).await, &pdf());
}

#[tokio::test]
async fn decodes_uuencoded_and_unencoded_parts() {
    for encoding in ["x-uuencode", "uuencode"] {
        let encoding = format!("Content-Transfer-Encoding: {encoding}");
        let headers = [
            "Content-Type: application/octet-stream; name=\"a.pdf\"",
            encoding.as_str(),
        ];
        let source = message(&headers, &uuencode(&pdf()));
        assert_returns(&read_part_2(source).await, &pdf());
    }
    // 7bit, 8bit and binary bodies are the bytes themselves; the CRLF before
    // the closing boundary belongs to the boundary.
    let text_pdf = "%PDF-1.4\n1 0 obj\n<< >>\nendobj\n%%EOF";
    for encoding in ["7bit", "8bit", "binary"] {
        let encoding = format!("Content-Transfer-Encoding: {encoding}");
        let headers = ["Content-Type: application/pdf", encoding.as_str()];
        let read = read_part_2(message(&headers, text_pdf)).await;
        assert_returns(&read, text_pdf.as_bytes());
    }
}

#[tokio::test]
async fn refuses_other_content_labelled_as_a_pdf() {
    let html = b"<!DOCTYPE html>\n<html><body>%PDF-1.4 invoice</body></html>".to_vec();
    let mut executable = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    executable.extend_from_slice(&pdf());
    let mut archive = b"PK\x03\x04\x14\x00".to_vec();
    archive.extend_from_slice(&pdf());
    let script = [b"#!/bin/sh\necho ".as_slice(), &pdf()].concat();
    for payload in [html, executable, archive, script, b"plain text".to_vec()] {
        for content_type in [
            "Content-Type: application/pdf; name=\"invoice.pdf\"",
            "Content-Type: application/octet-stream; name=\"invoice.pdf\"",
        ] {
            let headers = [content_type, "Content-Transfer-Encoding: base64"];
            let read = read_part_2(message(&headers, &base64_lines(&payload, 76))).await;
            assert!(read.error().contains("not a PDF"), "{content_type}");
        }
    }
    // A real PDF under a type that names another format stays refused.
    let headers = [
        "Content-Type: application/msword; name=\"invoice.pdf\"",
        "Content-Transfer-Encoding: base64",
    ];
    let read = read_part_2(message(&headers, &base64_lines(&pdf(), 76))).await;
    assert!(read.error().contains("not a PDF"));
    let headers = [
        "Content-Type: image/png; name=\"invoice.pdf\"",
        "Content-Transfer-Encoding: base64",
    ];
    let read = read_part_2(message(&headers, &base64_lines(&pdf(), 76))).await;
    assert!(read.error().contains("not a PNG image"), "{}", read.error());
}
