//! Stable read-only attachments: every attachment read selects its mailbox
//! read-only (EXAMINE) and fetches with `BODY.PEEK[]<0.N>`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use omni_imap::attachments::{
    MAX_ATTACHMENT_MESSAGE_BYTES, attachment_part_id, encode_stable_attachment_id,
    safe_attachment_filename,
};
use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_imap::map_message::{MessageCoords, map_parsed_message};
use omni_imap::mime::parse_message;
use omni_imap::protocol::{FetchQuery, SourceRange, UidSet};

const MESSAGE_ID: &str = "<attachment@example.test>";

fn source() -> Vec<u8> {
    [
        format!("Message-ID: {MESSAGE_ID}"),
        "Content-Type: multipart/mixed; boundary=\"outer\"".to_owned(),
        String::new(),
        "--outer".to_owned(),
        "Content-Type: text/plain".to_owned(),
        String::new(),
        "body".to_owned(),
        "--outer".to_owned(),
        "Content-Type: multipart/related; boundary=\"inner\"".to_owned(),
        String::new(),
        "--inner".to_owned(),
        "Content-Type: text/html".to_owned(),
        String::new(),
        "<img src=\"cid:pic\">".to_owned(),
        "--inner".to_owned(),
        "Content-Type: image/png".to_owned(),
        "Content-Disposition: inline; filename=\"pic.png\"".to_owned(),
        "Content-ID: <pic>".to_owned(),
        "Content-Transfer-Encoding: base64".to_owned(),
        String::new(),
        "aW1hZ2U=".to_owned(),
        "--inner--".to_owned(),
        "--outer".to_owned(),
        "Content-Type: application/pdf; name=\"agreement.pdf\"".to_owned(),
        "Content-Disposition: attachment; filename=\"agreement.pdf\"".to_owned(),
        "Content-Transfer-Encoding: base64".to_owned(),
        String::new(),
        STANDARD.encode(b"%PDF-1.4\nprivate"),
        "--outer--".to_owned(),
        String::new(),
    ]
    .join("\r\n")
    .into_bytes()
}

/// INBOX holds UID 7 with `actual` source; `envelope` is the ENVELOPE id.
async fn setup(actual: Vec<u8>, envelope: &str, size_override: Option<usize>) -> common::Harness {
    let server = FakeServer::default();
    server
        .folder("INBOX", 1, None)
        .folder("Archive", 2, Some("\\Archive"));
    let mut message = FakeMessage::new(actual).envelope_id(Some(envelope));
    if let Some(size) = size_override {
        message.source = vec![b'x'; size];
        message.envelope_message_id = Some(envelope.to_owned());
    }
    server.put("INBOX", 7, message);
    common::harness(server).await
}

#[test]
fn uses_root_mime_part_1_and_preserves_declared_type_instead_of_filename_inference() {
    let root = [
        format!("Message-ID: {MESSAGE_ID}"),
        "Content-Type: application/octet-stream".to_owned(),
        "Content-Disposition: attachment; filename=\"disguised.pdf\"".to_owned(),
        "Content-Transfer-Encoding: base64".to_owned(),
        String::new(),
        STANDARD.encode(b"%PDF-1.4"),
        String::new(),
    ]
    .join("\r\n");
    let parsed = parse_message(root.as_bytes(), common::NOW_MS).unwrap();
    assert_eq!(parsed.attachments[0].content_type, "application/pdf");
    assert_eq!(
        attachment_part_id(&parsed.attachments[0]).as_deref(),
        Some("1")
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let h = setup(root.into_bytes(), MESSAGE_ID, None).await;
        let result = h
            .transport
            .fetch_attachment_by_id(
                MESSAGE_ID,
                &encode_stable_attachment_id(MESSAGE_ID, "1"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.mime_type, "application/octet-stream");
    });
}

#[test]
fn retains_nested_inline_mime_identity_and_distinct_stable_handles_across_moves() {
    let parsed = parse_message(&source(), common::NOW_MS).unwrap();
    let ids: Vec<Option<String>> = parsed.attachments.iter().map(attachment_part_id).collect();
    assert_eq!(ids, vec![Some("2.2".to_owned()), Some("3".to_owned())]);
    let coords = |folder: &str, uv: &str, uid| MessageCoords {
        folder: folder.to_owned(),
        uid_validity: uv.to_owned(),
        uid,
    };
    let first = map_parsed_message(
        &parsed,
        &coords("INBOX", "1", 7),
        None,
        &common::PlainEnricher,
    );
    let moved = map_parsed_message(
        &parsed,
        &coords("Archive", "2", 9),
        None,
        &common::PlainEnricher,
    );
    let handles = |e: &omni_core::email::FetchedEmail| -> Vec<Option<String>> {
        e.attachments
            .iter()
            .map(|a| a.attachment_id.clone())
            .collect()
    };
    assert_eq!(handles(&first), handles(&moved));
    assert_eq!(first.attachments[0].part_id.as_deref(), Some("2.2"));
    assert_eq!(first.attachments[0].disposition.as_deref(), Some("inline"));
    assert_eq!(first.attachments[0].content_id.as_deref(), Some("<pic>"));
    let distinct: std::collections::HashSet<_> = handles(&first).into_iter().collect();
    assert_eq!(distinct.len(), 2);
}

#[tokio::test]
async fn downloads_only_the_selected_mime_part_with_read_only_locks() {
    let h = setup(source(), MESSAGE_ID, None).await;
    let result = h
        .transport
        .fetch_attachment_by_id(
            MESSAGE_ID,
            &encode_stable_attachment_id(MESSAGE_ID, "3"),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.name, "agreement.pdf");
    assert_eq!(result.mime_type, "application/pdf");
    assert_eq!(result.data, b"%PDF-1.4\nprivate");
    let calls = h.server.calls();
    assert!(calls.contains(&FakeCall::Select {
        path: "INBOX".to_owned(),
        read_only: true
    }));
    assert!(calls.contains(&FakeCall::Fetch {
        folder: "INBOX".to_owned(),
        uids: UidSet::List(vec![7]),
        query: FetchQuery {
            source: Some(SourceRange::Prefix {
                max_length: MAX_ATTACHMENT_MESSAGE_BYTES + 1
            }),
            ..FetchQuery::default()
        }
    }));
    assert!(calls.iter().all(|c| !matches!(
        c,
        FakeCall::Select {
            read_only: false,
            ..
        }
    )));
}

#[tokio::test]
async fn rejects_oversized_messages_before_fetching_source() {
    let h = setup(source(), MESSAGE_ID, Some(MAX_ATTACHMENT_MESSAGE_BYTES + 1)).await;
    let error = h
        .transport
        .fetch_attachment_by_id(
            MESSAGE_ID,
            &encode_stable_attachment_id(MESSAGE_ID, "3"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("byte limit"), "{error}");
    let fetches = h.server.count(|c| matches!(c, FakeCall::Fetch { .. }));
    assert_eq!(fetches, 1);
}

#[tokio::test]
async fn checks_exact_envelope_and_parsed_identity_and_returns_undefined_for_missing_parts() {
    let wrong = setup(source(), "<other@example.test>", None).await;
    let found = wrong
        .transport
        .fetch_attachment_by_id(
            MESSAGE_ID,
            &encode_stable_attachment_id(MESSAGE_ID, "3"),
            None,
        )
        .await
        .unwrap();
    assert!(found.is_none());
    assert!(wrong.server.calls().iter().all(|c| match c {
        FakeCall::Fetch { query, .. } => query.source.is_none(),
        _ => true,
    }));

    let changed_source = String::from_utf8(source())
        .unwrap()
        .replace(MESSAGE_ID, "<other@example.test>")
        .into_bytes();
    let changed = setup(changed_source, MESSAGE_ID, None).await;
    assert!(
        changed
            .transport
            .fetch_attachment_by_id(
                MESSAGE_ID,
                &encode_stable_attachment_id(MESSAGE_ID, "3"),
                None
            )
            .await
            .unwrap()
            .is_none()
    );

    let absent = setup(source(), MESSAGE_ID, None).await;
    assert!(
        absent
            .transport
            .fetch_attachment_by_id(
                MESSAGE_ID,
                &encode_stable_attachment_id(MESSAGE_ID, "99"),
                None
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn enforces_decoded_byte_limits_and_validates_inputs_before_imap() {
    let h = setup(source(), MESSAGE_ID, None).await;
    let error = h
        .transport
        .fetch_attachment_by_id(
            MESSAGE_ID,
            &encode_stable_attachment_id(MESSAGE_ID, "3"),
            Some(1),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("byte limit"), "{error}");
    h.server.clear_calls();
    let error = h
        .transport
        .fetch_attachment_by_id("bad\r\nidentity", "bad", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Invalid attachment"), "{error}");
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::Fetch { .. })), 0);
}

#[test]
fn sanitizes_path_controls_and_bidi_filenames_without_interpreting_sender_paths() {
    assert_eq!(
        safe_attachment_filename(Some("../../folder\\offer\u{202e}.pdf\r\n")),
        "offer.pdf"
    );
    assert_eq!(safe_attachment_filename(Some("..\u{0}")), "attachment");
    assert_eq!(
        safe_attachment_filename(Some("\u{feff}scan\u{200b}\u{85}\u{61c}.pdf")),
        "scan.pdf"
    );
    assert_eq!(safe_attachment_filename(Some(&"a".repeat(300))).len(), 180);
}
