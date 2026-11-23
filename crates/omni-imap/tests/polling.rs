//! Transport polling: per-folder UID cursors committed
//! only by the poll's commit, the seven-day INTERNALDATE bulk-import guard,
//! the per-pass cap, UIDVALIDITY recovery from the dispatch watermark,
//! auto-read protection of archive receipts, `SideEffectMode::Record`, and the
//! start/IDLE/reconnect lifecycle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use omni_http::SideEffectMode;
use omni_imap::cursor::{DISPATCH_WATERMARK_PK, get_folder_cursor};
use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_imap::ops::auto_read::AutoReadPlan;
use omni_imap::protocol::record::RecordedImapWrite;
use omni_imap::sync::FolderState;
use omni_store::cbor::JsValue;
use omni_store::{DocMeta, DocWrite as _};

const DAY: i64 = 86_400_000;

fn server() -> FakeServer {
    let server = FakeServer::default();
    server
        .folder("INBOX", 100, Some("\\Inbox"))
        .folder("Archive", 200, Some("\\Archive"))
        .folder("Junk", 300, Some("\\Junk"));
    server
}

fn mail(uid: u32, date: i64) -> FakeMessage {
    FakeMessage::new(common::source(
        &format!("<m{uid}@example.test>"),
        &format!("m{uid}"),
    ))
    .date(date)
}

async fn cursor(h: &common::Harness, folder: &str) -> Option<FolderState> {
    get_folder_cursor(&h.store.store, folder).await.unwrap()
}

#[tokio::test]
async fn first_poll_initializes_cursors_without_history_and_commits_only_on_request() {
    let server = server();
    server.put("INBOX", 5, mail(5, common::NOW_MS));
    let h = common::harness(server).await;
    h.transport.set_auto_read_plan(AutoReadPlan::default());
    let poll = h.transport.poll_new_emails().await.unwrap();
    assert!(poll.emails.is_empty());
    assert_eq!(
        cursor(&h, "INBOX").await,
        None,
        "nothing persisted before commit"
    );
    (poll.commit)().await.unwrap();
    assert_eq!(
        cursor(&h, "INBOX").await,
        Some(FolderState {
            uid_validity: "100".to_owned(),
            uid_next: 6
        })
    );
    assert_eq!(cursor(&h, "Archive").await.unwrap().uid_next, 1);
}

#[tokio::test]
async fn fetches_new_mail_and_skips_bulk_imports_older_than_seven_days() {
    let server = server();
    let h = common::harness(server.clone()).await;
    h.transport.set_auto_read_plan(AutoReadPlan::default());
    (h.transport.poll_new_emails().await.unwrap().commit)()
        .await
        .unwrap();
    server.put("INBOX", 1, mail(1, common::NOW_MS - 8 * DAY));
    server.put("INBOX", 2, mail(2, common::NOW_MS - DAY));
    server.put("Archive", 1, mail(10, common::NOW_MS));
    let poll = h.transport.poll_new_emails().await.unwrap();
    let ids: Vec<&str> = poll.emails.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["<m2@example.test>", "<m10@example.test>"]);
    let origin = poll.emails[1].origin.clone().unwrap();
    assert_eq!(
        (
            origin.folder.as_str(),
            origin.uid_validity.as_str(),
            origin.uid
        ),
        ("Archive", "200", 1)
    );
    (poll.commit)().await.unwrap();
    assert_eq!(cursor(&h, "INBOX").await.unwrap().uid_next, 3);
    let again = h.transport.poll_new_emails().await.unwrap();
    assert!(again.emails.is_empty());
}

#[tokio::test]
async fn a_capped_pass_advances_only_past_what_was_fetched() {
    let server = server();
    let h = common::harness(server.clone()).await;
    h.transport.set_auto_read_plan(AutoReadPlan::default());
    (h.transport.poll_new_emails().await.unwrap().commit)()
        .await
        .unwrap();
    for uid in 1..=205 {
        server.put("INBOX", uid, mail(uid, common::NOW_MS));
    }
    let poll = h.transport.poll_new_emails().await.unwrap();
    assert_eq!(poll.emails.len(), 200);
    (poll.commit)().await.unwrap();
    assert_eq!(cursor(&h, "INBOX").await.unwrap().uid_next, 201);
    let rest = h.transport.poll_new_emails().await.unwrap();
    assert_eq!(rest.emails.len(), 5);
}

#[tokio::test]
async fn uidvalidity_change_replays_from_the_dispatch_watermark_minus_an_hour() {
    let server = server();
    let h = common::harness(server.clone()).await;
    h.transport.set_auto_read_plan(AutoReadPlan::default());
    (h.transport.poll_new_emails().await.unwrap().commit)()
        .await
        .unwrap();
    // The folder is renumbered: old mail from 3 days ago plus mail since the watermark.
    server.lock().folder_mut("INBOX").unwrap().uid_validity = 101;
    server.put("INBOX", 1, mail(1, common::NOW_MS - 3 * DAY));
    server.put("INBOX", 2, mail(2, common::NOW_MS - 30 * 60_000));
    let watermark = common::NOW_MS - 2 * 60 * 60_000;
    h.store
        .store
        .write(move |tx| {
            let mut doc = indexmap::IndexMap::new();
            doc.insert("key".to_owned(), JsValue::String("singleton".to_owned()));
            doc.insert(
                "lastDispatchedAt".to_owned(),
                JsValue::Int(i128::from(watermark)),
            );
            tx.upsert_doc(
                DISPATCH_WATERMARK_PK,
                &JsValue::Object(doc),
                DocMeta {
                    entity: Some("jmap-email-dispatch".to_owned()),
                    ..DocMeta::default()
                },
            )
        })
        .await
        .unwrap();
    let poll = h.transport.poll_new_emails().await.unwrap();
    let ids: Vec<&str> = poll.emails.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["<m2@example.test>"]);
    (poll.commit)().await.unwrap();
    assert_eq!(
        cursor(&h, "INBOX").await,
        Some(FolderState {
            uid_validity: "101".to_owned(),
            uid_next: 3
        })
    );
}

#[tokio::test]
async fn auto_read_marks_recent_unread_in_junk_and_archive() {
    let server = server();
    server.put("Junk", 4, mail(4, common::NOW_MS));
    server.put("Archive", 4, mail(5, common::NOW_MS));
    let h = common::harness(server.clone()).await;
    h.transport.poll_new_emails().await.unwrap();
    let stores: Vec<String> = server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Store { folder, .. } => Some(folder),
            _ => None,
        })
        .collect();
    assert_eq!(stores, vec!["Archive", "Junk"]);
}

#[tokio::test]
async fn record_mode_records_mailbox_writes_instead_of_sending_them() {
    let server = server();
    server.put("Junk", 4, mail(4, common::NOW_MS));
    let h = common::unconnected(server.clone(), SideEffectMode::Record).await;
    h.transport.start().await.unwrap();
    h.transport.poll_new_emails().await.unwrap();
    assert_eq!(server.count(|c| matches!(c, FakeCall::Store { .. })), 0);
    assert_eq!(
        h.transport.recorded_writes(),
        vec![RecordedImapWrite::StoreFlags {
            mailbox: "Junk".to_owned(),
            uids: vec![4],
            flags: vec!["\\Seen".to_owned()],
        }]
    );
    h.transport.stop().await;
}

#[tokio::test]
async fn start_emits_a_mail_event_idles_and_reconnects_after_a_dead_connection() {
    let server = server();
    let h = common::unconnected(server.clone(), SideEffectMode::Live).await;
    let mut events = omni_core::mail_source::MailSource::mail_events(&h.transport);
    h.transport.start().await.unwrap();
    assert!(h.transport.is_active());
    tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    // The IDLE loop takes the connection while nothing else needs it.
    for _ in 0..100 {
        if server.count(|c| matches!(c, FakeCall::Idle)) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(server.count(|c| matches!(c, FakeCall::Idle)) > 0);
    // An operation interrupts IDLE and runs.
    let found = h
        .transport
        .fetch_email_by_id("<missing@example.test>", true)
        .await
        .unwrap();
    assert!(found.is_none());
    // A dead connection is dropped and replaced (after up to 3 s jitter).
    let connects_before = server.lock().connects;
    server.disconnect_all();
    let error = h
        .transport
        .fetch_email_by_id("<missing@example.test>", true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not available"), "{error}");
    while events.try_recv().is_ok() {}
    let mut reconnected = false;
    for _ in 0..400 {
        if server.lock().connects > connects_before
            && h.transport.fetch_email_by_id("<x@y>", true).await.is_ok()
        {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(reconnected, "transport reconnected");
    tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    h.transport.stop().await;
    assert!(server.count(|c| matches!(c, FakeCall::Logout)) >= 1);
}
