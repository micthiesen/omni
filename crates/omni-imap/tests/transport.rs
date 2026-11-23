//! Mailbox serialization. Locks and restores are both `select` calls
//! (restores select INBOX read-only), so cases assert selection sequences.
//! A dropped client closes its socket, so interrupted-connect cases assert
//! only that no client is installed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use omni_http::SideEffectMode;
use omni_imap::fake::{FakeCall, FakeMessage, FakeOp, FakeServer};
use omni_imap::protocol::{FetchQuery, ImapError, UidSet};
use omni_runtime::ports::{EmailFolderScope, EmailSearch};
use tokio::sync::Notify;

fn search_options(folder: EmailFolderScope, limit: u32) -> EmailSearch {
    EmailSearch {
        folder: Some(folder),
        limit,
        ..EmailSearch::default()
    }
}

fn selects(server: &FakeServer) -> Vec<(String, bool)> {
    server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Select { path, read_only } => Some((path, read_only)),
            _ => None,
        })
        .collect()
}

fn fetches(server: &FakeServer) -> Vec<UidSet> {
    server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Fetch { uids, .. } => Some(uids),
            _ => None,
        })
        .collect()
}

fn search_count(server: &FakeServer) -> usize {
    server.count(|c| matches!(c, FakeCall::Search { .. }))
}

#[tokio::test]
async fn batches_uid_downloads_and_reuses_bounded_parsed_search_results() {
    let server = FakeServer::default();
    server
        .folder("INBOX", 12, None)
        .folder("Archive", 12, Some("\\Archive"));
    for uid in [1_u32, 2] {
        server.put(
            "Archive",
            uid,
            FakeMessage::new(common::source(
                &format!("<message-{uid}@example.test>"),
                &format!("message-{uid}"),
            ))
            .date(1_788_256_800_000 + i64::from(uid) * 1000),
        );
    }
    let h = common::harness(server).await;
    h.transport.set_parsed_cache_limits(1, 1_000_000);

    let options = search_options(EmailFolderScope::Archive, 2);
    let first = h.transport.search_emails(&options).await.unwrap();
    let second = h.transport.search_emails(&options).await.unwrap();
    let fresh = h
        .transport
        .search_emails(&EmailSearch {
            fresh: true,
            ..options.clone()
        })
        .await
        .unwrap();
    let subjects: Vec<&str> = first.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(subjects, vec!["message-2", "message-1"]);
    assert_eq!(second, first);
    assert_eq!(fresh, first);
    assert_eq!(
        fetches(&h.server),
        vec![UidSet::List(vec![2, 1]), UidSet::List(vec![2, 1])]
    );
    assert!(h.server.calls().contains(&FakeCall::Fetch {
        folder: "Archive".to_owned(),
        uids: UidSet::List(vec![2, 1]),
        query: FetchQuery::source_and_date(),
    }));
    assert_eq!(search_count(&h.server), 2);

    h.transport
        .search_emails(&search_options(EmailFolderScope::Archive, 1))
        .await
        .unwrap();
    let mixed = h
        .transport
        .search_emails(&EmailSearch {
            query: Some("body".to_owned()),
            ..options
        })
        .await
        .unwrap();
    let subjects: Vec<&str> = mixed.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(subjects, vec!["message-2", "message-1"]);
    assert_eq!(fetches(&h.server).last(), Some(&UidSet::List(vec![1])));
}

#[tokio::test]
async fn serves_repeated_direct_reads_from_a_short_cache_and_honors_fresh() {
    let server = FakeServer::default();
    server
        .folder("INBOX", 12, None)
        .folder("Archive", 13, Some("\\Archive"));
    server.put(
        "INBOX",
        7,
        FakeMessage::new(b"Message-ID: <direct@example.test>\r\nSubject: direct\r\nFrom: sender@example.test\r\n\r\nbody".to_vec())
            .date(1_788_256_800_000),
    );
    let h = common::harness(server).await;
    let first = h
        .transport
        .fetch_email_by_id("<direct@example.test>", false)
        .await
        .unwrap();
    let second = h
        .transport
        .fetch_email_by_id("<direct@example.test>", false)
        .await
        .unwrap();
    let fresh = h
        .transport
        .fetch_email_by_id("<direct@example.test>", true)
        .await
        .unwrap();
    assert_eq!(first.as_ref().unwrap().subject, "direct");
    assert_eq!(second, first);
    assert_eq!(fresh, first);
    assert_eq!(search_count(&h.server), 2);
    assert_eq!(fetches(&h.server).len(), 2);
}

#[tokio::test]
async fn serializes_complete_select_use_restore_operations() {
    let server = FakeServer::default();
    server
        .folder("INBOX", 1, None)
        .folder("Archive", 2, Some("\\Archive"));
    let h = common::harness(server).await;
    let (entered, gate) = {
        let mut state = h.server.lock();
        state.paused.insert(FakeOp::Select);
        (state.entered.clone(), state.gate.clone())
    };
    let transport = h.transport.clone();
    let options = search_options(EmailFolderScope::Archive, 1);
    let first = {
        let transport = transport.clone();
        let options = options.clone();
        tokio::spawn(async move { transport.search_emails(&options).await })
    };
    entered.notified().await;
    let second = {
        let transport = transport.clone();
        let options = options.clone();
        tokio::spawn(async move { transport.search_emails(&options).await })
    };
    tokio::task::yield_now().await;
    h.server.lock().paused.clear();
    gate.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    // One complete select/search/restore; the queued search was served from
    // the snapshot the first one stored, so nothing interleaved.
    assert_eq!(
        selects(&h.server),
        vec![("Archive".to_owned(), true), ("INBOX".to_owned(), true)]
    );
}

#[tokio::test]
async fn shares_concurrent_connect_attempts() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    let gate = Arc::new(Notify::new());
    server.lock().connect_gate = Some(gate.clone());
    let h = common::unconnected(server, SideEffectMode::Live).await;
    let first = {
        let t = h.transport.clone();
        tokio::spawn(async move { t.connect_now().await })
    };
    let second = {
        let t = h.transport.clone();
        tokio::spawn(async move { t.connect_now().await })
    };
    for _ in 0..50 {
        if h.server.lock().connects == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    gate.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(h.server.lock().connects, 1);
}

#[tokio::test]
async fn queues_shutdown_behind_an_active_mailbox_operation() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    let h = common::harness(server).await;
    let release = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let operation = {
        let t = h.transport.clone();
        let release = release.clone();
        let started = started.clone();
        tokio::spawn(async move {
            t.with_permit(async move {
                started.notify_one();
                release.notified().await;
            })
            .await;
        })
    };
    started.notified().await;
    let stop = {
        let t = h.transport.clone();
        tokio::spawn(async move { t.stop().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::Logout)), 0);
    release.notify_one();
    operation.await.unwrap();
    stop.await.unwrap();
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::Logout)), 1);
}

#[tokio::test]
async fn closes_the_local_client_when_selecting_inbox_fails() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    server.fail_next(
        FakeOp::Select,
        None,
        ImapError::new("SELECT INBOX", "select failed"),
    );
    let h = common::unconnected(server, SideEffectMode::Live).await;
    let error = h.transport.connect_now().await.unwrap_err();
    assert!(error.to_string().contains("select failed"), "{error}");
    assert_eq!(h.server.count(|c| matches!(c, FakeCall::Logout)), 1);
    assert!(!h.transport.has_client().await);
}

#[tokio::test]
async fn closes_the_local_client_when_connect_fails() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    server
        .lock()
        .connect_failures
        .push(ImapError::new("connect", "connect failed"));
    let h = common::unconnected(server, SideEffectMode::Live).await;
    let error = h.transport.connect_now().await.unwrap_err();
    assert!(error.to_string().contains("connect failed"), "{error}");
    assert!(selects(&h.server).is_empty());
    assert!(!h.transport.has_client().await);
}

#[tokio::test]
async fn closes_a_non_signal_aware_local_client_when_connect_is_interrupted() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    let gate = Arc::new(Notify::new());
    server.lock().connect_gate = Some(gate.clone());
    let h = common::unconnected(server, SideEffectMode::Live).await;
    let attempt = tokio::time::timeout(Duration::from_millis(20), h.transport.connect_now()).await;
    assert!(attempt.is_err(), "connect was interrupted");
    // Resolving the underlying connect later cannot install the released client.
    gate.notify_one();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(selects(&h.server).is_empty());
    assert!(!h.transport.has_client().await);
}

#[tokio::test]
async fn closes_the_local_client_when_mailbox_selection_is_interrupted() {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    let (entered, gate) = {
        let mut state = server.lock();
        state.paused.insert(FakeOp::Select);
        (state.entered.clone(), state.gate.clone())
    };
    let h = common::unconnected(server, SideEffectMode::Live).await;
    let transport = h.transport.clone();
    let attempt = tokio::spawn(async move { transport.connect_now().await });
    entered.notified().await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    h.server.lock().paused.clear();
    gate.notify_one();
    assert!(!h.transport.has_client().await);
}
