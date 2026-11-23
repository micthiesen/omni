//! The controls-changed push service. Transport retries and interruption go
//! through the `ApnsSender` seam.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeApns, failed};
use omni_ios_controls::apns::{ApnsPushResult, ApnsTransportError};
use omni_ios_controls::persistence::{list_registrations, replace_device_registrations};
use omni_live::status::{LiveStatus, StreamerStatus, upsert_status};
use omni_store::cbor::Extra;

fn controls() -> Vec<omni_ios_controls::persistence::ControlInput> {
    vec![common::control("slot-one", 1, &"ab".repeat(32))]
}

#[tokio::test(start_paused = true)]
async fn pushes_a_new_token_once_but_not_on_an_unchanged_app_resync() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::Sent));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn persists_delivery_hashes_across_service_restarts() {
    let (store, clock) = common::store(0).await;
    let first_apns = FakeApns::new(Ok(ApnsPushResult::Sent));
    let first = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(first_apns.clone()),
    );
    first
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(first_apns.calls(), 1);

    let restarted_apns = FakeApns::new(Ok(ApnsPushResult::Sent));
    let restarted = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(restarted_apns.clone()),
    );
    restarted.reconcile().await.unwrap();
    assert_eq!(restarted_apns.calls(), 0);
    assert_eq!(restarted.diagnostics().await.unwrap().undelivered_count, 0);
}

#[tokio::test(start_paused = true)]
async fn retries_one_transient_apns_response() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::Sent)).then(failed(503, "Shutdown"));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 2);
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 0);
}

#[tokio::test(start_paused = true)]
async fn carries_an_exhausted_transient_push_into_later_live_check_ticks() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(failed(503, "Shutdown"));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 2);
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 1);

    service.reconcile().await.unwrap();
    assert_eq!(apns.calls(), 4);
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 1);

    apns.set_default(Ok(ApnsPushResult::Sent));
    service.reconcile().await.unwrap();
    assert_eq!(apns.calls(), 5);
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 0);
}

#[tokio::test(start_paused = true)]
async fn retries_one_transport_error() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::Sent)).then(Err(ApnsTransportError {
        operation: "request".into(),
        detail: "socket reset".into(),
        not_sent: false,
    }));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn deletes_a_token_apple_reports_as_invalid() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::InvalidToken {
        reason: "Unregistered".into(),
    }));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 1);
    assert!(list_registrations(&store.store).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn does_not_retry_a_permanent_apns_rejection() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(failed(403, "Forbidden"));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 1);
    assert_eq!(list_registrations(&store.store).await.unwrap().len(), 1);
    // The same state is not retried on later ticks either.
    service.reconcile().await.unwrap();
    assert_eq!(apns.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn retries_a_missing_apns_status_as_a_transient_transport_failure() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::Sent)).then(failed(0, "No response"));
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    assert_eq!(apns.calls(), 2);
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 0);
}

#[tokio::test(start_paused = true)]
async fn pushes_only_registered_slots_whose_displayed_state_changed() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::new(Ok(ApnsPushResult::Sent));
    replace_device_registrations(
        &store.store,
        "device-one",
        vec![
            common::control("slot-one", 1, &"ab".repeat(32)),
            common::control("slot-four", 4, &"cd".repeat(32)),
        ],
    )
    .await
    .unwrap();
    let service = common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    );
    service.reconcile().await.unwrap();
    // Restored rows are reconciled once, then delivered hashes suppress duplicates.
    assert_eq!(apns.calls(), 2);
    apns.clear();

    upsert_status(
        &store.store,
        StreamerStatus::Live(LiveStatus {
            streamer_id: "alpha".into(),
            primary: common::alpha().bindings[0].clone(),
            primary_title: "Alpha is live".into(),
            started_at: 0,
            max_viewer_count: 12,
            viewer_count: Some(10),
            sources: None,
            category: None,
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
    service.reconcile().await.unwrap();
    let calls = apns.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].slot, 1);
}

#[tokio::test]
async fn interrupts_an_in_flight_apns_request_with_its_parent_reconciliation() {
    let (store, clock) = common::store(0).await;
    let apns = FakeApns::hanging();
    replace_device_registrations(&store.store, "device-one", controls())
        .await
        .unwrap();
    let service = Arc::new(common::service(
        &store.store,
        &clock,
        vec![common::alpha()],
        Some(apns.clone()),
    ));
    let running = tokio::spawn({
        let service = service.clone();
        async move { service.reconcile().await }
    });
    while apns.calls() == 0 {
        tokio::task::yield_now().await;
    }
    running.abort();
    assert!(running.await.is_err_and(|e| e.is_cancelled()));
    assert_eq!(service.diagnostics().await.unwrap().undelivered_count, 1);
}

#[tokio::test]
async fn reports_diagnostics_without_apns() {
    let (store, clock) = common::store(0).await;
    let service = common::service(&store.store, &clock, vec![common::alpha()], None);
    service
        .register_device("device-one", controls())
        .await
        .unwrap();
    service.reconcile().await.unwrap();
    let diagnostics = service.diagnostics().await.unwrap();
    assert!(!diagnostics.apns_enabled);
    assert_eq!(diagnostics.registration_count, 1);
    assert_eq!(diagnostics.undelivered_count, 1);
    assert!(diagnostics.last_reconciled_at.is_some());
}
