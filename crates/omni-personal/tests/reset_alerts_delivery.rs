//! Durable reset alert delivery, plus the
//! Pushover adapter's 4xx/uncertain classification and record mode.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::FakeNotifier;
use omni_core::clock::SharedClock;
use omni_personal::reset_alerts::delivery::{
    Claude, ClaudeResetDelivery, Codex, CodexResetDelivery, DeliveryStatus, NotifyError,
    PushoverNotifier, ResetDelivery,
};
use omni_personal::reset_alerts::{DeliveryCounts, ResetAlert, ResetDeliveryLedger, ResetNotifier};
use omni_store::entity::Entity;
use omni_store::{EntityWrite, entity::UpsertOpts};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, mock_server, test_clock};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn isolates_providers_while_honoring_persisted_codex_reservations() {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let now = TEST_EPOCH_MS;
    let alert = ResetAlert {
        key: "same-event".into(),
        aliases: vec!["same-source-post".into()],
        title: "Reset report".into(),
        message: "Reported reset".into(),
        url: "https://example.com/source".into(),
        occurred_at: now,
    };
    #[allow(clippy::cast_precision_loss)]
    let legacy = CodexResetDelivery::new(
        alert.key.clone(),
        DeliveryStatus::Sent,
        now as f64,
        now as f64,
    );
    store
        .store
        .write(move |tx| tx.upsert(&legacy, UpsertOpts::default()))
        .await
        .unwrap();
    let notifier = Arc::new(FakeNotifier::default());
    let codex = ResetDeliveryLedger::<Codex>::new(store.store.clone(), notifier.clone());
    let claude = ResetDeliveryLedger::<Claude>::new(store.store.clone(), notifier.clone());
    let fresh_claude = ResetDeliveryLedger::<Claude>::new(store.store.clone(), notifier.clone());
    let counts = |sent, skipped, uncertain| DeliveryCounts {
        sent,
        skipped,
        uncertain,
    };
    assert_eq!(
        codex
            .deliver(std::slice::from_ref(&alert), now)
            .await
            .unwrap(),
        counts(0, 1, 0)
    );
    assert_eq!(
        claude
            .deliver(std::slice::from_ref(&alert), now)
            .await
            .unwrap(),
        counts(1, 0, 0)
    );
    assert_eq!(
        fresh_claude.deliver(&[alert], now).await.unwrap(),
        counts(0, 1, 0)
    );
    assert_eq!(notifier.calls(), 1);
    assert_eq!(CodexResetDelivery::NAME, "codex-reset-delivery");
    assert_eq!(ClaudeResetDelivery::NAME, "claude-reset-delivery");
    assert_eq!(
        ResetDelivery::<Codex>::DEFAULT_TTL_MS,
        Some(90 * 24 * 60 * 60 * 1000)
    );
}

fn alert() -> ResetAlert {
    ResetAlert {
        key: "k".into(),
        aliases: vec![],
        title: "Codex reset landed".into(),
        message: "Reported complete.".into(),
        url: "https://x.com/a/status/1".into(),
        occurred_at: 0,
    }
}

fn pushover(
    server: &wiremock::MockServer,
    mode: omni_http::SideEffectMode,
) -> omni_alerts::Pushover {
    omni_alerts::Pushover::with_credentials(
        mock_http(server, &["https://api.pushover.net"]),
        Some("user".into()),
        [(omni_alerts::PushoverChannel::General, "token".to_owned())],
        mode,
    )
}

#[tokio::test]
async fn pushover_adapter_classifies_rejections_and_sends_the_source_button() {
    let server = mock_server().await;
    Mock::given(method("POST"))
        .and(path("/1/messages.json"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/1/messages.json"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/1/messages.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"status\":1}"))
        .mount(&server)
        .await;
    let notifier = PushoverNotifier::new(pushover(&server, omni_http::SideEffectMode::Live));
    assert!(notifier.enabled());
    assert!(matches!(
        notifier.notify(&alert()).await,
        Err(NotifyError::Rejected { status: 429, .. })
    ));
    assert_eq!(
        notifier.notify(&alert()).await,
        Err(NotifyError::Uncertain(
            "Pushover API returned status code 500: ".into()
        ))
    );
    assert_eq!(notifier.notify(&alert()).await, Ok(()));
    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&requests[2].body).into_owned();
    assert!(body.contains("url_title=View+source"), "{body}");
    assert!(body.contains("token=token"));
    let userless = omni_alerts::Pushover::with_credentials(
        mock_http(&server, &["https://api.pushover.net"]),
        None,
        [(omni_alerts::PushoverChannel::General, "t".to_owned())],
        omni_http::SideEffectMode::Live,
    );
    assert!(!PushoverNotifier::new(userless).enabled());
}

#[tokio::test]
async fn pushover_adapter_records_instead_of_sending_in_record_mode() {
    let server = mock_server().await;
    let push = pushover(&server, omni_http::SideEffectMode::Record);
    let notifier = PushoverNotifier::new(push.clone());
    assert_eq!(notifier.notify(&alert()).await, Ok(()));
    assert_eq!(push.recorded().len(), 1);
    assert_eq!(
        push.recorded()[0].message.url_title.as_deref(),
        Some("View source")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
