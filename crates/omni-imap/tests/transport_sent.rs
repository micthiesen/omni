//! Sent copies through the transport. Locks and restores are both `select`,
//! so the sequence `[Sent, INBOX, Archive, Sent]` appears with the INBOX
//! restores interleaved.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_imap::ops::auto_read::AutoReadPlan;
use omni_imap::protocol::{FetchQuery, UidSet};
use omni_runtime::ports::{EmailFolderScope, EmailSearch};

const SENT: &str = "Courrier envoy\u{e9}";
const MESSAGE_ID: &str = "<sent-reply@example.test>";
/// 2026-09-30T23:59:10Z
const RECEIVED: i64 = 1_790_812_750_000;
const TEXT_BODY: &str = "Hi,\n\nPlease book the follow-up.\n\nThanks,\nMichael";

fn source(id: &str) -> Vec<u8> {
    [
        "From: Michael <me@example.test>",
        "To: clinic@example.test",
        "Cc: copy@example.test",
        &format!("Message-ID: {id}"),
        "In-Reply-To: <parent@example.test>",
        "References: <root@example.test> <parent@example.test>",
        "Subject: Re: Follow-up",
        "Date: Wed, 30 Sep 2026 23:59:10 +0000",
        "Content-Type: text/plain; charset=utf-8",
        "",
        &TEXT_BODY.replace('\n', "\r\n"),
    ]
    .join("\r\n")
    .into_bytes()
}

fn server() -> FakeServer {
    let server = FakeServer::default();
    server
        .folder("INBOX", 42, Some("\\Inbox"))
        .folder("Archive", 43, Some("\\Archive"))
        .folder(SENT, 44, Some("\\Sent"));
    server.put(SENT, 7, FakeMessage::new(source(MESSAGE_ID)).date(RECEIVED));
    server
}

fn selects(server: &FakeServer) -> Vec<String> {
    server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Select { path, read_only } => {
                assert!(read_only);
                Some(path)
            }
            _ => None,
        })
        .collect()
}

fn sent_search() -> EmailSearch {
    EmailSearch {
        folder: Some(EmailFolderScope::Sent),
        limit: 5,
        fresh: true,
        ..EmailSearch::default()
    }
}

#[tokio::test]
async fn searches_the_designated_localized_sent_mailbox_and_directly_reads_the_complete_reply() {
    let h = common::harness(server()).await;
    let found = h.transport.search_emails(&sent_search()).await.unwrap();
    assert_eq!(found.len(), 1);
    let email = &found[0];
    assert_eq!(email.id, MESSAGE_ID);
    assert_eq!(email.message_id.as_deref(), Some(MESSAGE_ID));
    assert_eq!(email.from, "me@example.test");
    assert_eq!(
        email.to.as_deref(),
        Some(&["clinic@example.test".to_owned()][..])
    );
    assert_eq!(
        email.cc.as_deref(),
        Some(&["copy@example.test".to_owned()][..])
    );
    assert_eq!(email.subject, "Re: Follow-up");
    assert_eq!(email.text_body, TEXT_BODY);
    assert_eq!(email.in_reply_to.as_deref(), Some("<parent@example.test>"));
    assert_eq!(
        email.references.as_deref(),
        Some(
            &[
                "<root@example.test>".to_owned(),
                "<parent@example.test>".to_owned()
            ][..]
        )
    );
    assert_eq!(email.received_at, omni_core::js::to_iso_string(RECEIVED));
    assert_eq!(
        selects(&h.server),
        vec![SENT.to_owned(), "INBOX".to_owned()]
    );

    let direct = h
        .transport
        .fetch_email_by_id(&email.id, true)
        .await
        .unwrap();
    assert_eq!(direct.as_ref(), Some(email));
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::List)), 2);
    assert_eq!(
        selects(&h.server),
        vec![SENT, "INBOX", "INBOX", "Archive", SENT, "INBOX"]
    );
    assert!(h.server.calls().contains(&FakeCall::Fetch {
        folder: SENT.to_owned(),
        uids: UidSet::List(vec![7]),
        query: FetchQuery::source_and_date(),
    }));
}

#[tokio::test]
async fn continues_polling_only_inbox_and_archive_after_sent_search() {
    let h = common::harness(server()).await;
    h.transport.search_emails(&sent_search()).await.unwrap();
    h.transport.set_auto_read_plan(AutoReadPlan::default());
    let poll = h.transport.poll_new_emails().await.unwrap();
    assert!(poll.emails.is_empty());
    let statuses: Vec<String> = h
        .server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Status { path } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, vec!["INBOX", "Archive"]);
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::List)), 1);
    assert_eq!(selects(&h.server).last().map(String::as_str), Some("INBOX"));
}

#[tokio::test]
async fn does_not_use_a_substring_search_candidate_as_the_reply_parent() {
    let server = server();
    server.put(
        SENT,
        8,
        FakeMessage::new(source("<unrelated@example.test>")).date(RECEIVED),
    );
    server.lock().search_override = Some(Box::new(|folder, _| {
        Some(Ok(Some(if folder == SENT {
            vec![7, 8]
        } else {
            Vec::new()
        })))
    }));
    let h = common::harness(server).await;
    let found = h
        .transport
        .fetch_email_by_id(MESSAGE_ID, true)
        .await
        .unwrap();
    assert_eq!(
        found.and_then(|e| e.message_id).as_deref(),
        Some(MESSAGE_ID)
    );
    let fetched: Vec<UidSet> = h
        .server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Fetch { uids, .. } => Some(uids),
            _ => None,
        })
        .collect();
    assert_eq!(fetched, vec![UidSet::List(vec![8]), UidSet::List(vec![7])]);
}
