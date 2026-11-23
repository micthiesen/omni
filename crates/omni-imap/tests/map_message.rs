//! Mapping parsed messages to the pipeline shape. The link metadata itself is
//! omni-email's; the stand-in enricher proves the root header lines reach it
//! intact.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_imap::map_message::{MessageCoords, map_parsed_message};
use omni_imap::mime::parse_message;

fn coords(uid: u32) -> MessageCoords {
    MessageCoords {
        folder: "INBOX".to_owned(),
        uid_validity: "1".to_owned(),
        uid,
    }
}

#[test]
fn retains_reply_recipients_and_threading_headers_without_treating_a_fallback_id_as_a_message_id() {
    let raw = [
        "From: Sender <sender@example.test>",
        "To: Reader <reader@example.test>, other@example.test",
        "Cc: copy@example.test",
        "Reply-To: replies@example.test",
        "Message-ID: <message@example.test>",
        "References: <root@example.test> <parent@example.test>",
        "Subject: A message",
        "List-Unsubscribe: <https://example.test/unsubscribe?token=private-test>",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click",
        "X-Private: must-not-leak",
        "",
        "Body",
    ]
    .join("\r\n");
    let parsed = parse_message(raw.as_bytes(), common::NOW_MS).unwrap();
    let email = map_parsed_message(
        &parsed,
        &coords(2),
        Some(common::NOW_MS),
        &common::PlainEnricher,
    );
    assert_eq!(email.id, "<message@example.test>");
    assert_eq!(email.message_id.as_deref(), Some("<message@example.test>"));
    assert_eq!(
        email.to.as_deref(),
        Some(
            &[
                "reader@example.test".to_owned(),
                "other@example.test".to_owned()
            ][..]
        )
    );
    assert_eq!(
        email.cc.as_deref(),
        Some(&["copy@example.test".to_owned()][..])
    );
    assert_eq!(
        email.reply_to.as_deref(),
        Some(&["replies@example.test".to_owned()][..])
    );
    assert_eq!(
        email.references.as_deref(),
        Some(
            &[
                "<root@example.test>".to_owned(),
                "<parent@example.test>".to_owned()
            ][..]
        )
    );
    let metadata = email.link_metadata.clone().unwrap();
    assert_eq!(
        metadata.list_unsubscribe.urls,
        vec!["https://example.test/unsubscribe?token=private-test".to_owned()]
    );
    assert_eq!(
        metadata.list_unsubscribe.post.as_deref(),
        Some("List-Unsubscribe=One-Click")
    );
    assert!(metadata.list_unsubscribe.present);
    assert!(!metadata.list_unsubscribe.truncated);
    assert!(
        !serde_json::to_string(&email)
            .unwrap()
            .contains("must-not-leak")
    );

    let fallback = parse_message(b"Subject: No ID\r\n\r\nBody", common::NOW_MS).unwrap();
    let mapped = map_parsed_message(&fallback, &coords(3), None, &common::PlainEnricher);
    assert_eq!(mapped.id, "imap|INBOX|1|3");
    assert_eq!(mapped.message_id, None);
}
