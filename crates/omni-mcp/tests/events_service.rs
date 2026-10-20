//! The MCP Events service: outbox, delivery, owner checks and pruning.
//!
//! Time advances with `tokio::time::advance` on a paused runtime (the
//! service's `TestClock` follows it). The archive-move case uses a fake
//! `ArchiveEcho` port (`omni-imap` owns the archive persistence). The clock
//! starts at a real epoch, so the first prune is already due.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::{clock, test_store};
use futures::future::BoxFuture;
use omni_core::clock::{Clock as _, TestClock};
use omni_core::email::{EmailOrigin, FetchedEmail};
use omni_mcp::events::catalog::EventArguments;
use omni_mcp::events::dynamic::{DynamicEvents, EventExtensions, OwnerCheck};
use omni_mcp::events::persistence::{
    DeliveryFailure, DeliveryStatus, DeliveryWithhold, EventDelivery, EventReceipt, EventRequest,
    EventSubscription,
};
use omni_mcp::events::service::{
    Lapse, McpEventService, PublishInput, SubscribeInput, UnsubscribeInput,
};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_runtime::ports::{ArchiveEcho, PortError, Ports};
use omni_store::Store;
use omni_store::cbor::JsValue;
use omni_store::entity::{EntityOps, EntityWrite, UpsertOpts};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

const URL: &str = "https://chatgpt.example.com/events/callback";
/// The gateway principal every subscription here belongs to.
const OWNER: &str = "gateway:client:events-test";

fn secret() -> String {
    format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    )
}

fn email(message_id: &str, folder: &str) -> FetchedEmail {
    serde_json::from_value(json!({
        "id": message_id,
        "messageId": message_id,
        "subject": "private fixture",
        "from": "sender@example.test",
        "textBody": "private body fixture",
        "links": [],
        "receivedAt": "2026-10-01T12:00:00.000Z",
        "attachments": [],
        "origin": {"folder": folder, "uidValidity": "123", "uid": 42},
    }))
    .unwrap()
}

fn inbox(message_id: &str) -> FetchedEmail {
    email(message_id, "INBOX")
}

type VerifyFn = dyn Fn() -> BoxFuture<'static, Result<(), WebhookError>> + Send + Sync;
type DeliverFn =
    dyn Fn(&WebhookEvent) -> BoxFuture<'static, Result<u16, WebhookError>> + Send + Sync;

/// A webhook port whose behavior each test scripts.
struct FakeWebhook {
    verify: Box<VerifyFn>,
    deliver: Box<DeliverFn>,
}

impl WebhookPort for FakeWebhook {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        (self.verify)()
    }

    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        event: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        (self.deliver)(event)
    }
}

/// No integration events (the gateway's are covered by its own tests).
struct NoIntegrationEvents;

impl DynamicEvents for NoIntegrationEvents {
    fn knows(&self, _: &str) -> bool {
        false
    }

    fn parse_arguments(&self, _: &str, _: &serde_json::Value) -> Option<EventArguments> {
        None
    }
}

/// Gateway owners, active unless revoked here; every check is logged.
#[derive(Default)]
struct FakeOwners {
    revoked: Mutex<HashSet<String>>,
    checked: Mutex<Vec<String>>,
}

impl OwnerCheck for FakeOwners {
    fn is_active<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, bool> {
        self.checked.lock().unwrap().push(owner.to_owned());
        let active = owner.starts_with("gateway:") && !self.revoked.lock().unwrap().contains(owner);
        Box::pin(async move { active })
    }
}

/// Registers `owners` (all active by default) as the service's owner check.
fn with_owners(service: McpEventService, owners: Arc<FakeOwners>) -> McpEventService {
    service.extend(EventExtensions {
        dynamic: Arc::new(NoIntegrationEvents),
        owners,
    });
    service
}

struct Harness {
    service: McpEventService,
    sent: Arc<Mutex<Vec<String>>>,
    owners: Arc<FakeOwners>,
}

fn service_with(
    store: &Store,
    clock: &Arc<TestClock>,
    token: &str,
    status: u16,
    ports: Ports,
) -> Harness {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let recorded = sent.clone();
    let webhook = FakeWebhook {
        verify: Box::new(|| Box::pin(async { Ok(()) })),
        deliver: Box::new(move |event| {
            recorded.lock().unwrap().push(event.event_id.clone());
            Box::pin(async move { Ok(status) })
        }),
    };
    let owners = Arc::new(FakeOwners::default());
    Harness {
        service: with_owners(
            McpEventService::new(
                token,
                store.clone(),
                clock.clone(),
                Arc::new(webhook),
                ports,
            ),
            owners.clone(),
        ),
        sent,
        owners,
    }
}

/// `service` with every gateway owner active.
fn owned(service: McpEventService) -> McpEventService {
    with_owners(service, Arc::new(FakeOwners::default()))
}

/// Ports with an `ArchiveEcho` that claims no moves (production always sets one).
fn echo_ports() -> Ports {
    let ports = Ports::default();
    ports
        .set_archive_echo(Arc::new(ClaimedMoves::default()))
        .unwrap();
    ports
}

fn service(store: &Store, clock: &Arc<TestClock>) -> Harness {
    service_with(store, clock, "test-omni-bearer", 204, echo_ports())
}

fn subscribe_input(folder: &str) -> SubscribeInput {
    SubscribeInput {
        name: "email.received".to_owned(),
        arguments: json!({ "folder": folder }),
        url: URL.to_owned(),
        secret: secret(),
        ttl_ms: None,
    }
}

fn unsubscribe_input() -> UnsubscribeInput {
    UnsubscribeInput {
        name: "email.received".to_owned(),
        arguments: json!({"folder": "inbox"}),
        url: URL.to_owned(),
    }
}

async fn deliveries(store: &Store) -> Vec<EventDelivery> {
    store
        .read(|docs| docs.get_all::<EventDelivery>())
        .await
        .unwrap()
}

async fn subscriptions(store: &Store) -> Vec<EventSubscription> {
    store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap()
}

async fn receipts(store: &Store) -> Vec<EventReceipt> {
    store
        .read(|docs| docs.get_all::<EventReceipt>())
        .await
        .unwrap()
}

fn iso_ms(text: &str) -> i64 {
    text.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}

#[tokio::test(start_paused = true)]
async fn stores_encrypted_credentials_and_delivers_each_receipt_once_across_restart() {
    let clock = clock();
    let db = test_store(&clock).await;
    let first = service(&db.store, &clock);
    let subscription = first
        .service
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    let stored = first
        .service
        .store()
        .subscription(&subscription.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.encrypted_secret.contains(&secret()));
    assert!(!stored.encrypted_url.contains(URL));

    first
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    first
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    let queued = deliveries(&db.store).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        Value::Object(queued[0].data_json()),
        json!({"messageId": "<one@example.test>", "folder": "inbox", "uidValidity": "123", "uid": 42})
    );
    assert!(!format!("{:?}", queued[0]).contains("private body fixture"));

    let restarted = service(&db.store, &clock);
    assert_eq!(restarted.service.drain().await.unwrap(), 1);
    assert_eq!(
        *restarted.sent.lock().unwrap(),
        vec![queued[0].event_id.clone()]
    );
    assert_eq!(restarted.service.drain().await.unwrap(), 0);
    let delivered = restarted
        .service
        .store()
        .delivery(&queued[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.status, DeliveryStatus::Delivered);
}

#[tokio::test(start_paused = true)]
async fn keeps_first_origin_and_does_not_backfill_a_new_subscriber_on_replay() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    assert!(deliveries(&db.store).await.is_empty());
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    events
        .record_email(&email("<one@example.test>", "Archive"))
        .await
        .unwrap();
    assert!(deliveries(&db.store).await.is_empty());
    let receipts = receipts(&db.store).await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].folder.as_deref(), Some("inbox"));
}

#[tokio::test(start_paused = true)]
async fn unsubscribe_cancels_queued_delivery_and_is_idempotent() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    let subscribed = events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    events
        .unsubscribe_as(OWNER, &unsubscribe_input())
        .await
        .unwrap();
    events
        .unsubscribe_as(OWNER, &unsubscribe_input())
        .await
        .unwrap();
    assert!(
        events
            .store()
            .subscription(&subscribed.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(events.drain().await.unwrap(), 1);
    assert_eq!(
        deliveries(&db.store).await[0].status,
        DeliveryStatus::Failed
    );
}

#[tokio::test(start_paused = true)]
async fn asks_subscribers_to_refresh_by_their_subscriptions_expiry() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    let now = clock.now_ms();
    let daily = events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    assert_eq!(iso_ms(&daily.refresh_before), now + 24 * 60 * 60_000);
    let stored = events
        .store()
        .subscription(&daily.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.expires_at, now + 24 * 60 * 60_000);
    assert_eq!(stored.encrypted_authorization, None);
    let brief = events
        .subscribe_as(
            OWNER,
            &SubscribeInput {
                ttl_ms: Some(10 * 60_000),
                ..subscribe_input("archive")
            },
        )
        .await
        .unwrap();
    assert_eq!(iso_ms(&brief.refresh_before), now + 10 * 60_000);
}

#[tokio::test(start_paused = true)]
async fn withholds_a_revoked_owners_delivery_until_it_is_active_again() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let events = &harness.service;
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    for id in ["<one@example.test>", "<two@example.test>", "<three@x.test>"] {
        events.record_email(&inbox(id)).await.unwrap();
    }
    harness
        .owners
        .revoked
        .lock()
        .unwrap()
        .insert(OWNER.to_owned());
    harness.owners.checked.lock().unwrap().clear();
    assert_eq!(events.drain().await.unwrap(), 3);
    // One check per subscription per pass.
    assert_eq!(
        *harness.owners.checked.lock().unwrap(),
        vec![OWNER.to_owned()]
    );
    assert_eq!(events.drain().await.unwrap(), 0);
    tokio::time::advance(Duration::from_secs(14 * 60)).await;
    assert_eq!(events.drain().await.unwrap(), 0);
    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    assert_eq!(events.drain().await.unwrap(), 3);
    assert!(harness.sent.lock().unwrap().is_empty());
    let held = deliveries(&db.store).await;
    assert_eq!(held.len(), 3);
    for row in &held {
        assert_eq!(row.status, DeliveryStatus::Pending);
        assert_eq!(row.withheld, Some(DeliveryWithhold::AuthorizationInvalid));
        assert_eq!(row.attempts, 0);
    }

    harness.owners.revoked.lock().unwrap().clear();
    tokio::time::advance(Duration::from_secs(16 * 60)).await;
    assert_eq!(events.drain().await.unwrap(), 3);
    assert_eq!(harness.sent.lock().unwrap().len(), 3);
    for row in deliveries(&db.store).await {
        assert_eq!(row.status, DeliveryStatus::Delivered);
        assert_eq!(row.withheld, None);
    }
}

#[tokio::test(start_paused = true)]
async fn fails_withheld_events_only_when_their_subscription_ends() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    harness
        .service
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    harness
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    harness
        .owners
        .revoked
        .lock()
        .unwrap()
        .insert(OWNER.to_owned());
    harness.service.drain().await.unwrap();
    tokio::time::advance(Duration::from_secs(25 * 60 * 60)).await;
    harness.service.drain().await.unwrap();
    assert!(harness.sent.lock().unwrap().is_empty());
    let failed = &deliveries(&db.store).await[0];
    assert_eq!(failed.status, DeliveryStatus::Failed);
    assert_eq!(failed.failure, Some(DeliveryFailure::SubscriptionInactive));
    assert_eq!(failed.withheld, None);
}

#[tokio::test(start_paused = true)]
async fn never_delivers_to_an_owner_that_is_not_a_gateway_principal() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let legacy = format!("executor:{}", "a".repeat(64));
    harness
        .service
        .subscribe_as(&legacy, &subscribe_input("inbox"))
        .await
        .unwrap();
    harness
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    harness.service.drain().await.unwrap();
    assert!(harness.sent.lock().unwrap().is_empty());
    let failed = &deliveries(&db.store).await[0];
    assert_eq!(failed.status, DeliveryStatus::Failed);
    assert_eq!(
        failed.failure,
        Some(DeliveryFailure::CredentialsUnavailable)
    );
    assert!(harness.owners.checked.lock().unwrap().is_empty());
    assert_eq!(
        harness.service.drop_legacy_subscriptions().await.unwrap(),
        1
    );
    assert!(subscriptions(&db.store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn serves_the_status_tool_from_the_service_without_secrets() {
    let clock = clock();
    let db = test_store(&clock).await;
    let disabled = omni_mcp::tools::events::events_status(None).await.unwrap();
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["subscriptions"], json!([]));
    let events = service(&db.store, &clock).service;
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<tool@example.test>"))
        .await
        .unwrap();
    let status = omni_mcp::tools::events::events_status(Some(&events))
        .await
        .unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["subscriptionTotal"], 1);
    assert_eq!(status["subscriptions"][0]["name"], "email.received");
    assert_eq!(
        status["subscriptions"][0]["arguments"],
        json!({"folder": "inbox"})
    );
    assert_eq!(
        status["subscriptions"][0]["callbackHost"],
        "chatgpt.example.com"
    );
    assert_eq!(status["deliveries"]["pending"], 1);
    assert_eq!(status["deliveries"]["withheld"], 0);
    let text = status.to_string();
    assert!(!text.contains("<tool@example.test>"));
    assert!(!text.contains(&secret()));
    let keys: Vec<&str> = status
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "enabled",
            "checkedAt",
            "requests",
            "subscriptionTotal",
            "subscriptions",
            "deliveries"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn records_bounded_event_requests_without_credentials_or_paths() {
    let clock = clock();
    let db = test_store(&clock).await;
    let owner = format!("gateway:client:{}", "b".repeat(36));
    let events = service(&db.store, &clock).service;
    let second = || tokio::time::advance(Duration::from_secs(1));
    events.record_discovery_as(&owner).await;
    second().await;
    events
        .subscribe_as(&owner, &subscribe_input("inbox"))
        .await
        .unwrap();
    second().await;
    events
        .subscribe_as(&owner, &subscribe_input("inbox"))
        .await
        .unwrap();
    second().await;
    let rejected = events
        .subscribe_as(
            &owner,
            &SubscribeInput {
                url: "http://plain.example.com/x".to_owned(),
                ..subscribe_input("archive")
            },
        )
        .await;
    assert!(rejected.is_err());
    second().await;
    events
        .unsubscribe_as(&owner, &unsubscribe_input())
        .await
        .unwrap();
    second().await;
    events
        .unsubscribe_as(&owner, &unsubscribe_input())
        .await
        .unwrap();

    let status = events.status().await.unwrap();
    let outcomes: Vec<String> = status["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r["method"].as_str().unwrap(),
                r["outcome"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            "events/unsubscribe not_found",
            "events/unsubscribe removed",
            "events/subscribe invalid_callback",
            "events/subscribe refreshed",
            "events/subscribe accepted",
            "events/list listed",
        ]
    );
    let accepted = &status["requests"][4];
    assert_eq!(
        accepted["owner"],
        format!("gateway:client:{}", "b".repeat(13))
    );
    assert_eq!(accepted["name"], "email.received");
    assert_eq!(accepted["arguments"], json!({"folder": "inbox"}));
    assert_eq!(accepted["callbackHost"], "chatgpt.example.com");
    let text = Value::Object(status).to_string();
    for hidden in [secret().as_str(), "/events/callback", owner.as_str()] {
        assert!(!text.contains(hidden), "{hidden}");
    }
    for _ in 0..35 {
        events.record_discovery_as(&owner).await;
    }
    assert_eq!(events.store().requests().await.unwrap().len(), 30);
}

#[tokio::test(start_paused = true)]
async fn keeps_secret_rotated_rows_terminal_without_decrypting_with_the_new_key() {
    let clock = clock();
    let db = test_store(&clock).await;
    let original = service(&db.store, &clock).service;
    original
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    original
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    let rotated = service_with(&db.store, &clock, "rotated-token", 204, echo_ports());
    rotated.service.drain().await.unwrap();
    assert!(rotated.sent.lock().unwrap().is_empty());
    assert_eq!(
        deliveries(&db.store).await[0].status,
        DeliveryStatus::Failed
    );
}

#[tokio::test(start_paused = true)]
async fn keeps_previous_queued_deliveries_cancelled_after_resubscribe() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let events = &harness.service;
    let first = events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    let old_generation = events
        .store()
        .subscription(&first.id)
        .await
        .unwrap()
        .unwrap()
        .generation;
    events
        .unsubscribe_as(OWNER, &unsubscribe_input())
        .await
        .unwrap();
    let second = events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_ne!(
        events
            .store()
            .subscription(&second.id)
            .await
            .unwrap()
            .unwrap()
            .generation,
        old_generation
    );
    events.drain().await.unwrap();
    assert!(harness.sent.lock().unwrap().is_empty());
    assert_eq!(
        deliveries(&db.store).await[0].status,
        DeliveryStatus::Failed
    );
}

/// Message-IDs whose archive actions are claimed (the `ArchiveEcho` port).
#[derive(Default)]
struct ClaimedMoves(Mutex<HashSet<String>>);

impl ArchiveEcho for ClaimedMoves {
    fn is_archive_action_message<'a>(
        &'a self,
        message_id: &'a str,
        _origin: Option<&'a EmailOrigin>,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        let claimed = self.0.lock().unwrap().contains(message_id);
        Box::pin(async move { Ok(claimed) })
    }
}

#[tokio::test(start_paused = true)]
async fn refuses_to_publish_mail_without_the_archive_echo_port() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service_with(&db.store, &clock, "test-omni-bearer", 204, Ports::default()).service;
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    let error = events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "archive echo check failed: ArchiveEcho is unavailable"
    );
    assert!(deliveries(&db.store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn keeps_original_inbox_receipts_and_suppresses_claimed_move_feedback() {
    let clock = clock();
    let db = test_store(&clock).await;
    let moves = Arc::new(ClaimedMoves::default());
    let ports = Ports::default();
    ports.set_archive_echo(moves.clone()).unwrap();
    let events = service_with(&db.store, &clock, "test-omni-bearer", 204, ports).service;
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .subscribe_as(OWNER, &subscribe_input("archive"))
        .await
        .unwrap();
    let message_id = "<archived@example.test>";
    events.record_email(&inbox(message_id)).await.unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 1);
    moves.0.lock().unwrap().insert(message_id.to_owned());
    events
        .record_email(&email(message_id, "Archive"))
        .await
        .unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 1);
    let unseen = "<archived-before-observation@example.test>";
    moves.0.lock().unwrap().insert(unseen.to_owned());
    events
        .record_email(&email(unseen, "Archive"))
        .await
        .unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn retries_429_and_5xx_but_terminates_4xx_and_exhausted_claims() {
    for (status, expected) in [
        (400, DeliveryStatus::Failed),
        (429, DeliveryStatus::Pending),
        (503, DeliveryStatus::Pending),
    ] {
        let clock = clock();
        let db = test_store(&clock).await;
        let events =
            service_with(&db.store, &clock, "test-omni-bearer", status, echo_ports()).service;
        events
            .subscribe_as(OWNER, &subscribe_input("inbox"))
            .await
            .unwrap();
        events
            .record_email(&inbox("<one@example.test>"))
            .await
            .unwrap();
        events.drain().await.unwrap();
        let row = &deliveries(&db.store).await[0];
        assert_eq!(row.status, expected);
        assert_eq!(row.last_status, Some(i64::from(status)));
        assert_eq!(
            row.failure,
            (expected == DeliveryStatus::Failed).then_some(DeliveryFailure::Rejected)
        );
    }
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    harness
        .service
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    harness
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    let queued = deliveries(&db.store).await.remove(0);
    harness
        .service
        .store()
        .upsert_delivery(EventDelivery {
            attempts: 8,
            ..queued.clone()
        })
        .await
        .unwrap();
    harness.service.drain().await.unwrap();
    assert!(harness.sent.lock().unwrap().is_empty());
    let row = harness
        .service
        .store()
        .delivery(&queued.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, DeliveryStatus::Failed);
    assert_eq!(row.failure, Some(DeliveryFailure::AttemptsExhausted));
}

#[tokio::test(start_paused = true)]
async fn publishes_while_a_slow_webhook_is_still_in_flight() {
    let clock = clock();
    let db = test_store(&clock).await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (started_hook, release_hook) = (started.clone(), release.clone());
    let events = owned(McpEventService::new(
        "test-omni-bearer",
        db.store.clone(),
        clock.clone(),
        Arc::new(FakeWebhook {
            verify: Box::new(|| Box::pin(async { Ok(()) })),
            deliver: Box::new(move |_| {
                let (started, release) = (started_hook.clone(), release_hook.clone());
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(204)
                })
            }),
        }),
        echo_ports(),
    ));
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<slow@example.test>"))
        .await
        .unwrap();
    let draining = {
        let events = events.clone();
        tokio::spawn(async move { events.drain().await })
    };
    started.notified().await;
    // The webhook is blocked, yet the dispatcher's publish still commits.
    events
        .record_email(&inbox("<next@example.test>"))
        .await
        .unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 2);
    release.notify_one();
    draining.await.unwrap().unwrap();
    let mut statuses: Vec<DeliveryStatus> = deliveries(&db.store)
        .await
        .iter()
        .map(|r| r.status)
        .collect();
    statuses.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(
        statuses,
        [DeliveryStatus::Delivered, DeliveryStatus::Pending]
    );
}

#[tokio::test(start_paused = true)]
async fn delivers_as_soon_as_an_event_is_published() {
    let clock = clock();
    let db = test_store(&clock).await;
    let (sender, receiver) = oneshot::channel::<String>();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let events = owned(McpEventService::new(
        "test-omni-bearer",
        db.store.clone(),
        clock.clone(),
        Arc::new(FakeWebhook {
            verify: Box::new(|| Box::pin(async { Ok(()) })),
            deliver: Box::new(move |event| {
                if let Some(sender) = sender.lock().unwrap().take() {
                    sender.send(event.event_id.clone()).unwrap();
                }
                Box::pin(async { Ok(204) })
            }),
        }),
        echo_ports(),
    ));
    let worker = {
        let events = events.clone();
        tokio::spawn(async move {
            events
                .delivery_worker(tokio_util::sync::CancellationToken::new())
                .await;
        })
    };
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    events
        .record_email(&inbox("<fast@example.test>"))
        .await
        .unwrap();
    let event_id = receiver.await.unwrap();
    assert_eq!(deliveries(&db.store).await[0].event_id, event_id);
    worker.abort();
}

#[tokio::test(start_paused = true)]
async fn the_delivery_worker_stops_when_shutdown_begins() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker = {
        let events = harness.service.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { events.delivery_worker(shutdown).await })
    };
    tokio::task::yield_now().await;
    assert!(!worker.is_finished());
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("worker stops promptly")
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn prunes_finished_deliveries_old_receipts_and_ended_subscriptions() {
    let clock = clock();
    let db = test_store(&clock).await;
    tokio::time::advance(Duration::from_secs(3600)).await;
    let harness = service(&db.store, &clock);
    let events = &harness.service;
    events
        .subscribe_as(
            OWNER,
            &SubscribeInput {
                ttl_ms: Some(60_000),
                ..subscribe_input("inbox")
            },
        )
        .await
        .unwrap();
    events
        .record_email(&inbox("<old@example.test>"))
        .await
        .unwrap();
    events.drain().await.unwrap();
    tokio::time::advance(Duration::from_secs(8 * 24 * 3600)).await;
    events.drain().await.unwrap();
    assert!(deliveries(&db.store).await.is_empty());
    assert!(subscriptions(&db.store).await.is_empty());
    // Receipts outlive the IMAP guard, so a replay is still recognized.
    assert_eq!(receipts(&db.store).await.len(), 1);
    tokio::time::advance(Duration::from_secs(30 * 24 * 3600)).await;
    events.drain().await.unwrap();
    assert!(receipts(&db.store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn delivers_rows_stored_before_events_were_generic() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let events = &harness.service;
    let id = events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap()
        .id;
    let mut legacy = events.store().subscription(&id).await.unwrap().unwrap();
    legacy.arguments = None;
    legacy.folder = Some("inbox".to_owned());
    db.store
        .write(move |tx| tx.upsert(&legacy, UpsertOpts::default()))
        .await
        .unwrap();
    assert_eq!(
        events
            .store()
            .subscription(&id)
            .await
            .unwrap()
            .unwrap()
            .effective_arguments(),
        indexmap::IndexMap::from([("folder".to_owned(), "inbox".to_owned())])
    );
    let request: EventRequest = omni_store::cbor::from_value(JsValue::Object(
        [
            ("id", JsValue::String("legacy-request".into())),
            ("at", JsValue::Int(1)),
            ("method", JsValue::String("events/subscribe".into())),
            ("owner", JsValue::String("legacy".into())),
            ("folder", JsValue::String("archive".into())),
            ("outcome", JsValue::String("accepted".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
    ))
    .unwrap();
    db.store
        .write(move |tx| tx.upsert(&request, UpsertOpts::default()))
        .await
        .unwrap();
    let requests = events.store().requests().await.unwrap();
    let legacy_request = requests.iter().find(|r| r.id == "legacy-request").unwrap();
    assert_eq!(
        legacy_request.arguments,
        Some(indexmap::IndexMap::from([(
            "folder".to_owned(),
            "archive".to_owned()
        )]))
    );
    // Refreshing the legacy row keeps its identity.
    assert_eq!(
        events
            .subscribe_as(OWNER, &subscribe_input("inbox"))
            .await
            .unwrap()
            .id,
        id
    );
    events
        .record_email(&inbox("<legacy@example.test>"))
        .await
        .unwrap();
    events.drain().await.unwrap();
    assert_eq!(harness.sent.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn matches_claude_turn_events_to_subscribed_projects() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    let claude = |args: Value, url: &str| SubscribeInput {
        name: "claude.session.turn_finished".to_owned(),
        arguments: args,
        url: url.to_owned(),
        secret: secret(),
        ttl_ms: None,
    };
    let failure = events
        .subscribe_as(OWNER, &claude(json!({"project": "../etc"}), URL))
        .await;
    assert_eq!(
        failure.unwrap_err().to_string(),
        "Event subscription rejected: invalid_arguments"
    );
    events
        .subscribe_as(OWNER, &claude(json!({"project": "omni"}), URL))
        .await
        .unwrap();
    events
        .subscribe_as(OWNER, &claude(json!({}), &format!("{URL}/all")))
        .await
        .unwrap();
    assert!(
        events
            .has_active_subscription("claude.session.turn_finished")
            .await
            .unwrap()
    );
    let turn = |project: &str, revision: i64| {
        PublishInput {
        name: "claude.session.turn_finished".to_owned(),
        receipt_key: format!("turn:{project}:{revision}"),
        event_key: format!("turn:{project}:{revision}"),
        timestamp: "2026-10-05T00:00:00.000Z".to_owned(),
        data: json!({"sessionId": "s", "id": "s1", "project": project, "status": "idle", "revision": revision})
            .as_object()
            .cloned()
            .unwrap(),
    }
    };
    events.publish(turn("omni", 3)).await.unwrap();
    events.publish(turn("dotfiles", 4)).await.unwrap();
    events.publish(turn("dotfiles", 4)).await.unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn publishes_while_a_subscribers_challenge_is_still_in_flight() {
    let clock = clock();
    let db = test_store(&clock).await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let challenges = Arc::new(Mutex::new(0));
    let (started_hook, release_hook) = (started.clone(), release.clone());
    let events = owned(McpEventService::new(
        "test-omni-bearer",
        db.store.clone(),
        clock.clone(),
        Arc::new(FakeWebhook {
            verify: Box::new(move || {
                let count = {
                    let mut count = challenges.lock().unwrap();
                    *count += 1;
                    *count
                };
                let (started, release) = (started_hook.clone(), release_hook.clone());
                Box::pin(async move {
                    if count > 1 {
                        started.notify_one();
                        release.notified().await;
                    }
                    Ok(())
                })
            }),
            deliver: Box::new(|_| Box::pin(async { Ok(204) })),
        }),
        echo_ports(),
    ));
    events
        .subscribe_as(OWNER, &subscribe_input("inbox"))
        .await
        .unwrap();
    let slow = {
        let events = events.clone();
        tokio::spawn(async move {
            events
                .subscribe_as(
                    OWNER,
                    &SubscribeInput {
                        url: format!("{URL}/slow"),
                        ..subscribe_input("archive")
                    },
                )
                .await
        })
    };
    started.notified().await;
    events
        .record_email(&inbox("<during@example.test>"))
        .await
        .unwrap();
    assert_eq!(deliveries(&db.store).await.len(), 1);
    release.notify_one();
    slow.await.unwrap().unwrap();
    assert_eq!(subscriptions(&db.store).await.len(), 2);
}

fn brief(folder: &str) -> SubscribeInput {
    SubscribeInput {
        ttl_ms: Some(10 * 60_000),
        ..subscribe_input(folder)
    }
}

#[tokio::test(start_paused = true)]
async fn reports_a_subscription_that_lapses_unrefreshed_once_per_lapse() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    events.subscribe_as(OWNER, &brief("inbox")).await.unwrap();
    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    assert!(events.reserve_lapses().await.unwrap().is_empty());
    // A refresh before expiry moves it.
    events.subscribe_as(OWNER, &brief("inbox")).await.unwrap();
    let refreshed_expiry = clock.now_ms() + 10 * 60_000;
    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    assert!(events.reserve_lapses().await.unwrap().is_empty());

    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    let lapses = events.reserve_lapses().await.unwrap();
    assert_eq!(
        lapses,
        vec![Lapse {
            name: "email.received".to_owned(),
            arguments: EventArguments::from([("folder".to_owned(), "inbox".to_owned())]),
            owner: "gateway:client:events-test".to_owned(),
            callback_host: Some("chatgpt.example.com".to_owned()),
            expired_at: refreshed_expiry,
        }]
    );
    assert!(events.reserve_lapses().await.unwrap().is_empty());

    // Subscribing again after the lapse starts a fresh row that can lapse again.
    events.subscribe_as(OWNER, &brief("inbox")).await.unwrap();
    assert!(events.reserve_lapses().await.unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(11 * 60)).await;
    assert_eq!(events.reserve_lapses().await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn never_reports_an_unsubscribed_revoked_or_long_expired_subscription() {
    let clock = clock();
    let db = test_store(&clock).await;
    let harness = service(&db.store, &clock);
    let events = &harness.service;
    events.subscribe_as(OWNER, &brief("inbox")).await.unwrap();
    events.subscribe_as(OWNER, &brief("archive")).await.unwrap();
    events
        .unsubscribe_as(OWNER, &unsubscribe_input())
        .await
        .unwrap();
    harness
        .owners
        .revoked
        .lock()
        .unwrap()
        .insert(OWNER.to_owned());
    tokio::time::advance(Duration::from_secs(11 * 60)).await;
    assert!(events.reserve_lapses().await.unwrap().is_empty());

    // Lapses older than a day are history, not news.
    harness.owners.revoked.lock().unwrap().clear();
    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
    assert!(events.reserve_lapses().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_client_that_moved_its_callback_is_not_reported_for_the_old_one() {
    let clock = clock();
    let db = test_store(&clock).await;
    let events = service(&db.store, &clock).service;
    events.subscribe_as(OWNER, &brief("inbox")).await.unwrap();
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let moved = SubscribeInput {
        url: "https://chatgpt.example.com/events/v2".to_owned(),
        ..brief("inbox")
    };
    events.subscribe_as(OWNER, &moved).await.unwrap();
    tokio::time::advance(Duration::from_secs(6 * 60)).await;
    assert!(events.reserve_lapses().await.unwrap().is_empty());
    // When the replacement lapses too, only it is reported.
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let lapses = events.reserve_lapses().await.unwrap();
    assert_eq!(lapses.len(), 1);
    assert_eq!(lapses[0].expired_at, clock.now_ms() - 60_000);
}
