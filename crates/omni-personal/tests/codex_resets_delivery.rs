//! Codex reset alert delivery through a scripted `ResetNotifier`; a
//! `PushoverError` with a 4xx status is `NotifyError::Rejected`, one without a
//! status `Uncertain`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeNotifier, wait_for_calls};
use omni_core::clock::SharedClock;
use omni_personal::codex_resets::delivery::{CodexLedger, ResetDeliveryEntity};
use omni_personal::reset_alerts::delivery::DeliveryStatus;
use omni_personal::reset_alerts::{DeliveryCounts, DeliveryError, NotifyError, ResetAlert};
use omni_store::EntityOps;
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

fn alert() -> ResetAlert {
    ResetAlert {
        key: "session:codex-2026-10-02T12:00:00Z".into(),
        aliases: vec![],
        title: "Codex allowance reset".into(),
        message: "The Codex usage allowance reset.".into(),
        url: "http://omni.boris/tasks".into(),
        occurred_at: 1_790_940_000_000,
    }
}

fn with_key(key: &str) -> ResetAlert {
    ResetAlert {
        key: key.into(),
        ..alert()
    }
}

fn counts(sent: u32, skipped: u32, uncertain: u32) -> DeliveryCounts {
    DeliveryCounts {
        sent,
        skipped,
        uncertain,
    }
}

struct Fixture {
    store: TestStore,
    notifier: Arc<FakeNotifier>,
    ledger: CodexLedger,
}

async fn fixture(notifier: FakeNotifier) -> Fixture {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let notifier = Arc::new(notifier);
    let ledger = CodexLedger::new(store.store.clone(), notifier.clone());
    Fixture {
        store,
        notifier,
        ledger,
    }
}

const NOW: i64 = TEST_EPOCH_MS;

#[tokio::test]
async fn sends_once_and_persists_sent_state_across_later_runs() {
    let f = fixture(FakeNotifier::default()).await;
    assert_eq!(
        f.ledger.deliver(&[alert()], NOW).await.unwrap(),
        counts(1, 0, 0)
    );
    assert_eq!(
        f.ledger.deliver(&[alert()], NOW).await.unwrap(),
        counts(0, 1, 0)
    );
    assert_eq!(f.notifier.calls(), 1);
    let row = f
        .store
        .store
        .read(|docs| docs.get::<ResetDeliveryEntity>(&alert().key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, DeliveryStatus::Sent);
}

#[tokio::test(flavor = "multi_thread")]
async fn atomically_reserves_duplicate_concurrent_deliveries() {
    let f = fixture(FakeNotifier::default()).await;
    let concurrent = with_key("concurrent-alert");
    let release = f.notifier.block_next();
    let first = {
        let ledger = f.ledger.clone();
        let alert = concurrent.clone();
        tokio::spawn(async move { ledger.deliver(&[alert], NOW).await })
    };
    wait_for_calls(&f.notifier, 1).await;
    assert_eq!(
        f.ledger.deliver(&[concurrent], NOW).await.unwrap(),
        counts(0, 0, 1)
    );
    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), counts(1, 0, 0));
    assert_eq!(f.notifier.calls(), 1);
}

#[tokio::test]
async fn uses_an_existing_legacy_primary_key_to_suppress_an_alias_aware_alert() {
    let f = fixture(FakeNotifier::default()).await;
    f.ledger
        .deliver(&[with_key("legacy-alert-key")], NOW)
        .await
        .unwrap();
    let updated = ResetAlert {
        aliases: vec!["legacy-alert-key".into()],
        ..with_key("new-feed-key")
    };
    assert_eq!(
        f.ledger
            .deliver(std::slice::from_ref(&updated), NOW)
            .await
            .unwrap(),
        counts(0, 1, 0)
    );
    assert_eq!(f.notifier.calls(), 1);
    let row = f
        .store
        .store
        .read(move |docs| docs.get::<ResetDeliveryEntity>(&updated.key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, DeliveryStatus::Sent);
}

#[tokio::test]
async fn shares_an_alias_between_history_and_feed_primary_keys() {
    let f = fixture(FakeNotifier::default()).await;
    let history = ResetAlert {
        aliases: vec!["post:announcement-1:landed:non-banked".into()],
        ..with_key("history:completion-1")
    };
    let feed = ResetAlert {
        aliases: vec!["post:announcement-1:landed:non-banked".into()],
        ..with_key("feed-event:landed:non-banked")
    };
    assert_eq!(
        f.ledger.deliver(&[history, feed], NOW).await.unwrap(),
        counts(1, 1, 0)
    );
    assert_eq!(f.notifier.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn locks_concurrent_alerts_that_share_an_alias() {
    let f = fixture(FakeNotifier::default()).await;
    let first_alert = ResetAlert {
        aliases: vec!["post:concurrent:landed:non-banked".into()],
        ..with_key("history:concurrent")
    };
    let second_alert = ResetAlert {
        aliases: vec!["post:concurrent:landed:non-banked".into()],
        ..with_key("feed:concurrent")
    };
    let release = f.notifier.block_next();
    let first = {
        let ledger = f.ledger.clone();
        tokio::spawn(async move { ledger.deliver(&[first_alert], NOW).await })
    };
    wait_for_calls(&f.notifier, 1).await;
    assert_eq!(
        f.ledger
            .deliver(std::slice::from_ref(&second_alert), NOW)
            .await
            .unwrap(),
        counts(0, 0, 1)
    );
    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), counts(1, 0, 0));
    assert_eq!(
        f.ledger.deliver(&[second_alert], NOW).await.unwrap(),
        counts(0, 1, 0)
    );
    assert_eq!(f.notifier.calls(), 1);
}

#[tokio::test]
async fn retains_ambiguous_attempts_after_failure_so_a_restarted_run_cannot_resend() {
    let f = fixture(FakeNotifier::default()).await;
    let uncertain = with_key("uncertain-alert");
    f.notifier
        .reply_once(Err(NotifyError::Uncertain("socket closed".into())));
    let error = f
        .ledger
        .deliver(std::slice::from_ref(&uncertain), NOW)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DeliveryError::Delivery {
            uncertain: true,
            ..
        }
    ));
    assert_eq!(
        f.ledger.deliver(&[uncertain], NOW).await.unwrap(),
        counts(0, 0, 1)
    );
    assert_eq!(f.notifier.calls(), 1);
}

#[tokio::test]
async fn releases_a_definite_provider_rejection_for_retry_on_the_next_poll() {
    let f = fixture(FakeNotifier::default()).await;
    let rejected = with_key("rejected-alert");
    f.notifier.reply_once(Err(NotifyError::Rejected {
        status: 429,
        body: "rate limited".into(),
    }));
    let error = f
        .ledger
        .deliver(std::slice::from_ref(&rejected), NOW)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DeliveryError::Delivery {
            uncertain: false,
            ..
        }
    ));
    assert_eq!(
        f.ledger.deliver(&[rejected], NOW).await.unwrap(),
        counts(1, 0, 0)
    );
    assert_eq!(f.notifier.calls(), 2);
}

#[tokio::test]
async fn fails_clearly_when_the_pushover_service_is_disabled() {
    let f = fixture(FakeNotifier::disabled()).await;
    let error = f
        .ledger
        .deliver(&[with_key("disabled-alert")], NOW)
        .await
        .unwrap_err();
    match error {
        DeliveryError::Delivery {
            uncertain, cause, ..
        } => {
            assert!(!uncertain);
            assert!(cause.contains("Pushover is disabled"));
        }
        other => panic!("unexpected {other}"),
    }
    assert_eq!(f.notifier.calls(), 0);
}
