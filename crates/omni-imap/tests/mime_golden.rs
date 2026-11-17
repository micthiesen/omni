//! mailparser reference corpus: every `tests/golden/mime/*.eml` is parsed and
//! compared with the committed JSON that mailparser `simpleParser` produced.
//!
//! Compared: Message-ID normalization, In-Reply-To, References, subject, Date,
//! address lists, the HTML body, the plain-text body of messages without HTML,
//! header keys, every attachment (raw and normalized part ids, effective and
//! declared types, filename, disposition, Content-ID, cid, related flag, size,
//! content digest), and the mapped `FetchedEmail` except the enrichment fields
//! owned by omni-email (`textBody` of HTML messages, `links`, `linkMetadata`).
//!
//! One deliberate difference: `qp-soft-and-8bit.json` expects the raw 8-bit
//! bytes of a quoted-printable part to survive, where mailparser's libqp
//! mangles them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use omni_core::email::{EmailLink, EmailLinkMetadata, ListUnsubscribe};
use omni_imap::map_message::{BodyEnricher, LinkMetadataInput, MessageCoords, map_parsed_message};
use omni_imap::mime::{Address, ParsedMail, parse_message};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

struct NoEnrichment;

impl BodyEnricher for NoEnrichment {
    fn html_to_text(&self, _html: &str) -> String {
        String::new()
    }
    fn interesting_links(&self, _html: &str) -> Vec<String> {
        Vec::new()
    }
    fn link_metadata(&self, _input: LinkMetadataInput<'_>) -> EmailLinkMetadata {
        EmailLinkMetadata {
            links: Vec::<EmailLink>::new(),
            links_truncated: false,
            list_unsubscribe: ListUnsubscribe {
                urls: Vec::new(),
                post: None,
                present: false,
                truncated: false,
            },
        }
    }
}

fn address_json(address: &Address) -> Value {
    match &address.group {
        Some(group) => {
            json!({"name": address.name, "group": group.iter().map(address_json).collect::<Vec<_>>()})
        }
        None => {
            json!({"name": address.name, "address": address.address.clone().unwrap_or_default()})
        }
    }
}

fn single(list: Option<&Vec<Address>>) -> Value {
    list.map_or(Value::Null, |l| {
        json!([l.iter().map(address_json).collect::<Vec<_>>()])
    })
}

fn multi(lists: Option<&Vec<Vec<Address>>>) -> Value {
    lists.map_or(Value::Null, |ls| {
        Value::Array(
            ls.iter()
                .map(|l| Value::Array(l.iter().map(address_json).collect()))
                .collect(),
        )
    })
}

fn actual(parsed: &ParsedMail) -> Value {
    let html = parsed.html.clone();
    json!({
        "messageId": parsed.message_id,
        "inReplyTo": parsed.in_reply_to,
        "references": parsed.references,
        "subject": parsed.subject,
        "from": single(parsed.from.as_ref()),
        "to": multi(parsed.to.as_ref()),
        "cc": multi(parsed.cc.as_ref()),
        "bcc": multi(parsed.bcc.as_ref()),
        "replyTo": single(parsed.reply_to.as_ref()),
        "html": html,
        "text": if parsed.html.is_none() { json!(parsed.text) } else { Value::Null },
        "headerKeys": parsed.header_lines.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        "attachments": parsed.attachments.iter().map(|a| json!({
            "rawPartId": a.part_id,
            "partId": omni_imap::attachments::attachment_part_id(a),
            "contentType": a.content_type,
            "declaredType": omni_imap::attachments::declared_attachment_mime_type(a),
            "filename": a.filename,
            "contentDisposition": a.content_disposition,
            "contentId": a.content_id,
            "cid": a.cid,
            "related": a.related,
            "size": a.content.len(),
            "sha256": hex::encode(Sha256::digest(&a.content)),
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn mailparser_corpus_matches_the_reference() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mime");
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "eml"))
        .collect();
    fixtures.sort();
    assert!(fixtures.len() >= 17, "corpus is missing fixtures");
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let mut failures = Vec::new();
    for fixture in fixtures {
        let name = fixture.file_stem().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read(&fixture).unwrap();
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.with_extension("json")).unwrap())
                .unwrap();
        let parsed = parse_message(&source, now_ms).unwrap();
        let got = actual(&parsed);
        for key in got.as_object().unwrap().keys() {
            if got[key] != expected[key] {
                failures.push(format!(
                    "{name}: {key}\n  expected {}\n  actual   {}",
                    expected[key], got[key]
                ));
            }
        }
        // Date: exact unless mailparser substituted "now" for an invalid header.
        if expected["dateIsNow"] == json!(true) {
            assert!(
                parsed.date.is_some_and(|d| (d - now_ms).abs() < 60_000),
                "{name}: date"
            );
        } else {
            let date = parsed.date.map(omni_core::js::to_iso_string);
            if json!(date) != expected["date"] {
                failures.push(format!(
                    "{name}: date expected {} actual {:?}",
                    expected["date"], date
                ));
            }
        }
        let coords = MessageCoords {
            folder: "INBOX".to_owned(),
            uid_validity: "1".to_owned(),
            uid: 7,
        };
        let internal = jiff::Timestamp::from_second(1_790_769_600)
            .unwrap()
            .as_millisecond();
        let email = map_parsed_message(&parsed, &coords, Some(internal), &NoEnrichment);
        let mut mapped = serde_json::to_value(&email).unwrap();
        let object = mapped.as_object_mut().unwrap();
        object.remove("links");
        object.remove("linkMetadata");
        if parsed.html.is_some() {
            object.remove("textBody");
        }
        if mapped != expected["mapped"] {
            failures.push(format!(
                "{name}: mapped\n  expected {}\n  actual   {mapped}",
                expected["mapped"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
