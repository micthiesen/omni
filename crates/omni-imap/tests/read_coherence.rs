//! Read cache coherence, driven by `TestClock::set` on the shared clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use omni_core::clock::Clock as _;
use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_runtime::ports::{EmailFolderScope, EmailSearch};
use tokio::sync::Notify;

fn fixture() -> (FakeServer, Arc<Mutex<Vec<u32>>>) {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    for uid in [1_u32, 2] {
        server.put(
            "INBOX",
            uid,
            FakeMessage::new(format!(
                "Message-ID: <{uid}@example.test>\r\nSubject: message-{uid}\r\n\r\nBody"
            ))
            .date(common::NOW_MS),
        );
    }
    let ids = Arc::new(Mutex::new(vec![1]));
    let current = ids.clone();
    server.lock().search_override = Some(Box::new(move |_, criteria| {
        criteria
            .header
            .is_none()
            .then(|| Ok(Some(current.lock().unwrap().clone())))
    }));
    (server, ids)
}

fn inbox() -> EmailSearch {
    EmailSearch {
        folder: Some(EmailFolderScope::Inbox),
        limit: 1,
        ..EmailSearch::default()
    }
}

fn count(server: &FakeServer, f: impl Fn(&FakeCall) -> bool) -> usize {
    server.count(f)
}

fn advance(h: &common::Harness, ms: i64) {
    h.clock.set(h.clock.now_ms() + ms);
}

#[tokio::test]
async fn expires_search_snapshots_on_the_test_clock_while_retaining_immutable_parsed_bodies() {
    let (server, _) = fixture();
    let h = common::harness(server).await;
    h.transport.search_emails(&inbox()).await.unwrap();
    h.transport.search_emails(&inbox()).await.unwrap();
    assert_eq!(
        count(&h.server, |c| matches!(c, FakeCall::Search { .. })),
        1
    );
    advance(&h, 30_001);
    h.transport.search_emails(&inbox()).await.unwrap();
    assert_eq!(
        count(&h.server, |c| matches!(c, FakeCall::Search { .. })),
        2
    );
    assert_eq!(count(&h.server, |c| matches!(c, FakeCall::Fetch { .. })), 1);
    advance(&h, 300_001);
    h.transport.search_emails(&inbox()).await.unwrap();
    assert_eq!(count(&h.server, |c| matches!(c, FakeCall::Fetch { .. })), 2);
}

#[tokio::test]
async fn replaces_a_stale_search_snapshot_after_a_fresh_read_and_separates_uidvalidity_generations()
{
    let (server, ids) = fixture();
    let h = common::harness(server).await;
    h.transport.search_emails(&inbox()).await.unwrap();
    *ids.lock().unwrap() = vec![2];
    assert_eq!(
        h.transport.search_emails(&inbox()).await.unwrap()[0].subject,
        "message-1"
    );
    h.transport
        .search_emails(&EmailSearch {
            fresh: true,
            ..inbox()
        })
        .await
        .unwrap();
    assert_eq!(
        h.transport.search_emails(&inbox()).await.unwrap()[0].subject,
        "message-2"
    );
    advance(&h, 30_001);
    h.server.lock().folder_mut("INBOX").unwrap().uid_validity = 2;
    h.transport.search_emails(&inbox()).await.unwrap();
    assert_eq!(count(&h.server, |c| matches!(c, FakeCall::Fetch { .. })), 3);
}

#[tokio::test]
async fn serves_cached_reads_while_background_processing_owns_the_mailbox_permit() {
    let (server, _) = fixture();
    let h = common::harness(server).await;
    h.transport.search_emails(&inbox()).await.unwrap();
    h.transport
        .fetch_email_by_id("<1@example.test>", false)
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let background = {
        let t = h.transport.clone();
        let (entered, release) = (entered.clone(), release.clone());
        tokio::spawn(async move {
            t.with_permit(async move {
                entered.notify_one();
                release.notified().await;
            })
            .await;
        })
    };
    entered.notified().await;
    assert_eq!(h.transport.search_emails(&inbox()).await.unwrap().len(), 1);
    let email = h
        .transport
        .fetch_email_by_id("<1@example.test>", false)
        .await
        .unwrap();
    assert_eq!(email.unwrap().subject, "message-1");
    release.notify_one();
    background.await.unwrap();
}
