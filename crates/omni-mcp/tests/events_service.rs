//! The MCP Events service: outbox, delivery, authorization and pruning.
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
use omni_mcp::events::executor_auth::{EventAuthorizer, ExecutorAuthError};
use omni_mcp::events::persistence::{
    DeliveryFailure, DeliveryStatus, DeliveryWithhold, EventDelivery, EventReceipt, EventRequest,
    EventSubscription,
};
use omni_mcp::events::service::{
    EventPrincipal, McpEventService, PublishInput, SubscribeInput, UnsubscribeInput,
};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_runtime::ports::{ArchiveEcho, PortError, Ports};
use omni_store::Store;
use omni_store::cbor::JsValue;
use omni_store::entity::{EntityOps, EntityWrite, UpsertOpts};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

const URL: &str = "https://chatgpt.example.com/events/callback";
const TOKEN_LIFETIME_MS: i64 = 60 * 60_000;

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

type Validity = dyn Fn(&str, &str) -> Result<bool, ()> + Send + Sync;

/// Answers validity; a valid token expires an hour after the check.
struct FakeAuthorizer {
    clock: Arc<TestClock>,
    valid: Box<Validity>,
}

impl EventAuthorizer for FakeAuthorizer {
    fn authorize<'a>(
        &'a self,
        owner: &'a str,
        authorization: &'a str,
    ) -> BoxFuture<'a, Result<Option<i64>, ExecutorAuthError>> {
        let now = self.clock.now_ms();
        let verdict = (self.valid)(owner, authorization);
        Box::pin(async move {
            match verdict {
                Ok(true) => Ok(Some(now + TOKEN_LIFETIME_MS)),
                Ok(false) => Ok(None),
                Err(()) => Err(ExecutorAuthError),
            }
        })
    }
}

struct Harness {
    service: McpEventService,
    sent: Arc<Mutex<Vec<String>>>,
}

fn service_with(
    store: &Store,
    clock: &Arc<TestClock>,
    token: &str,
    status: u16,
    valid: Option<Box<Validity>>,
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
    let authorizer = valid.map(|valid| {
        Arc::new(FakeAuthorizer {
            clock: clock.clone(),
            valid,
        }) as Arc<dyn EventAuthorizer>
    });
    Harness {
        service: McpEventService::new(
            token,
            store.clone(),
            clock.clone(),
            Arc::new(webhook),
            authorizer,
            ports,
        ),
        sent,
    }
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
    service_with(store, clock, "test-omni-bearer", 204, None, echo_ports())
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

fn principal(owner: &str, bearer: &str) -> EventPrincipal {
    EventPrincipal {
        owner: owner.to_owned(),
        authorization: bearer.to_owned(),
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
        .subscribe(&subscribe_input("inbox"), None)
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
        .subscribe(&subscribe_input("inbox"), None)
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
        .subscribe(&subscribe_input("inbox"), None)
        .await
        .unwrap();
    events
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    events
        .unsubscribe(&unsubscribe_input(), None)
        .await
        .unwrap();
    events
        .unsubscribe(&unsubscribe_input(), None)
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
async fn asks_delegated_subscribers_to_refresh_by_their_token_expiry() {
    let clock = clock();
    let db = test_store(&clock).await;
    let owner = format!("executor:{}", "c".repeat(64));
    let delegated = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(|_, _| Ok(true))),
        echo_ports(),
    )
    .service;
    let now = clock.now_ms();
    let who = principal(&owner, "Bearer delegated-fixture");
    let hourly = delegated
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    assert_eq!(
        iso_ms(&hourly.refresh_before),
        now + TOKEN_LIFETIME_MS - 60_000
    );
    assert_eq!(
        delegated
            .store()
            .subscription(&hourly.id)
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        now + 24 * 60 * 60_000
    );
    let brief = delegated
        .subscribe(
            &SubscribeInput {
                ttl_ms: Some(10 * 60_000),
                ..subscribe_input("archive")
            },
            Some(&who),
        )
        .await
        .unwrap();
    assert_eq!(iso_ms(&brief.refresh_before), now + 10 * 60_000);
    let direct = service(&db.store, &clock)
        .service
        .subscribe(&subscribe_input("inbox"), None)
        .await
        .unwrap();
    assert_eq!(iso_ms(&direct.refresh_before), now + 24 * 60 * 60_000);
}

struct SettableExpiry {
    expiry: Mutex<Option<i64>>,
}

impl EventAuthorizer for SettableExpiry {
    fn authorize<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<i64>, ExecutorAuthError>> {
        let expiry = *self.expiry.lock().unwrap();
        Box::pin(async move { Ok(expiry) })
    }
}

#[tokio::test(start_paused = true)]
async fn keeps_refresh_before_within_a_nearly_expired_tokens_lifetime() {
    let clock = clock();
    let db = test_store(&clock).await;
    let now = clock.now_ms();
    let authorizer = Arc::new(SettableExpiry {
        expiry: Mutex::new(Some(now + 30_000)),
    });
    let events = McpEventService::new(
        "test-omni-bearer",
        db.store.clone(),
        clock.clone(),
        Arc::new(FakeWebhook {
            verify: Box::new(|| Box::pin(async { Ok(()) })),
            deliver: Box::new(|_| Box::pin(async { Ok(204) })),
        }),
        Some(authorizer.clone()),
        echo_ports(),
    );
    let who = principal(
        &format!("executor:{}", "d".repeat(64)),
        "Bearer delegated-fixture",
    );
    let soon = events
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    assert_eq!(iso_ms(&soon.refresh_before), now);

    let expiry = now + 2 * TOKEN_LIFETIME_MS;
    *authorizer.expiry.lock().unwrap() = Some(expiry);
    let rotated = events
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    assert_eq!(iso_ms(&rotated.refresh_before), expiry - 60_000);

    let status = events.status().await.unwrap();
    assert_eq!(
        status["subscriptions"][0]["refreshBefore"],
        json!(omni_core::js::to_iso_string(expiry - 60_000))
    );
    assert_eq!(
        status["subscriptions"][0]["expiresAt"],
        json!(omni_core::js::to_iso_string(now + 24 * 60 * 60_000))
    );

    *authorizer.expiry.lock().unwrap() = None;
    assert!(
        events
            .subscribe(&subscribe_input("inbox"), Some(&who))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn withholds_delegated_delivery_until_the_stored_token_validates() {
    let clock = clock();
    let db = test_store(&clock).await;
    let valid = Arc::new(Mutex::new("Bearer delegated-fixture".to_owned()));
    let checked = Arc::new(Mutex::new(Vec::<String>::new()));
    let (valid_check, checked_log) = (valid.clone(), checked.clone());
    let owner = format!("executor:{}", "a".repeat(64));
    let harness = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(move |_, bearer| {
            checked_log.lock().unwrap().push(bearer.to_owned());
            Ok(bearer == *valid_check.lock().unwrap())
        })),
        echo_ports(),
    );
    let events = &harness.service;
    let subscribed = events
        .subscribe(
            &subscribe_input("inbox"),
            Some(&principal(&owner, "Bearer delegated-fixture")),
        )
        .await
        .unwrap();
    let stored = events
        .store()
        .subscription(&subscribed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !stored
            .encrypted_authorization
            .unwrap()
            .contains("delegated-fixture")
    );
    for id in ["<one@example.test>", "<two@example.test>", "<three@x.test>"] {
        events.record_email(&inbox(id)).await.unwrap();
    }
    *valid.lock().unwrap() = "Bearer refreshed-fixture".to_owned();
    checked.lock().unwrap().clear();
    assert_eq!(events.drain().await.unwrap(), 3);
    assert_eq!(
        *checked.lock().unwrap(),
        vec!["Bearer delegated-fixture".to_owned()]
    );
    assert_eq!(events.drain().await.unwrap(), 0);
    tokio::time::advance(Duration::from_secs(14 * 60)).await;
    assert_eq!(events.drain().await.unwrap(), 0);
    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    assert_eq!(events.drain().await.unwrap(), 3);
    assert_eq!(checked.lock().unwrap().len(), 2);
    assert!(harness.sent.lock().unwrap().is_empty());
    let held = deliveries(&db.store).await;
    assert_eq!(held.len(), 3);
    for row in &held {
        assert_eq!(row.status, DeliveryStatus::Pending);
        assert_eq!(row.withheld, Some(DeliveryWithhold::AuthorizationInvalid));
        assert_eq!(row.attempts, 0);
    }

    events
        .subscribe(
            &subscribe_input("inbox"),
            Some(&principal(&owner, "Bearer refreshed-fixture")),
        )
        .await
        .unwrap();
    checked.lock().unwrap().clear();
    assert_eq!(events.drain().await.unwrap(), 3);
    assert_eq!(
        *checked.lock().unwrap(),
        vec!["Bearer refreshed-fixture".to_owned()]
    );
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
    let active = Arc::new(Mutex::new(true));
    let flag = active.clone();
    let owner = format!("executor:{}", "a".repeat(64));
    let harness = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(move |_, _| Ok(*flag.lock().unwrap()))),
        echo_ports(),
    );
    harness
        .service
        .subscribe(
            &subscribe_input("inbox"),
            Some(&principal(&owner, "Bearer delegated-fixture")),
        )
        .await
        .unwrap();
    harness
        .service
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    *active.lock().unwrap() = false;
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
async fn holds_a_subscription_briefly_when_executor_cannot_authorize() {
    let clock = clock();
    let db = test_store(&clock).await;
    let owner = format!("executor:{}", "a".repeat(64));
    let who = principal(&owner, "Bearer delegated-fixture");
    let down = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(|_, _| Err(()))),
        echo_ports(),
    );
    assert!(
        down.service
            .subscribe(&subscribe_input("inbox"), Some(&who))
            .await
            .is_err()
    );

    let available = Arc::new(Mutex::new(true));
    let flag = available.clone();
    let flaky = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(move |_, _| {
            if *flag.lock().unwrap() {
                Ok(true)
            } else {
                Err(())
            }
        })),
        echo_ports(),
    );
    flaky
        .service
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    flaky
        .service
        .subscribe(&subscribe_input("archive"), None)
        .await
        .unwrap();
    flaky
        .service
        .record_email(&inbox("<inbox@example.test>"))
        .await
        .unwrap();
    flaky
        .service
        .record_email(&email("<archive@example.test>", "Archive"))
        .await
        .unwrap();
    *available.lock().unwrap() = false;
    assert_eq!(flaky.service.drain().await.unwrap(), 2);
    assert_eq!(flaky.sent.lock().unwrap().len(), 1);
    let rows = deliveries(&db.store).await;
    let folder = |row: &EventDelivery| row.data_json()["folder"].as_str().unwrap().to_owned();
    let inbox_row = rows.iter().find(|r| folder(r) == "inbox").unwrap();
    assert_eq!(inbox_row.status, DeliveryStatus::Pending);
    assert_eq!(
        inbox_row.withheld,
        Some(DeliveryWithhold::AuthorizationUnavailable)
    );
    assert_eq!(inbox_row.attempts, 0);
    assert_eq!(
        rows.iter().find(|r| folder(r) == "archive").unwrap().status,
        DeliveryStatus::Delivered
    );
    *available.lock().unwrap() = true;
    tokio::time::advance(Duration::from_secs(61)).await;
    flaky.service.drain().await.unwrap();
    assert_eq!(flaky.sent.lock().unwrap().len(), 2);
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
        .subscribe(&subscribe_input("inbox"), None)
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
    let owner = format!("executor:{}", "b".repeat(64));
    let who = principal(&owner, "Bearer delegated-fixture");
    let events = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        Some(Box::new(|_, _| Ok(true))),
        echo_ports(),
    )
    .service;
    let second = || tokio::time::advance(Duration::from_secs(1));
    events.record_discovery(Some(&owner)).await;
    second().await;
    events
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    second().await;
    events
        .subscribe(&subscribe_input("inbox"), Some(&who))
        .await
        .unwrap();
    second().await;
    let rejected = events
        .subscribe(
            &SubscribeInput {
                url: "http://plain.example.com/x".to_owned(),
                ..subscribe_input("archive")
            },
            Some(&who),
        )
        .await;
    assert!(rejected.is_err());
    second().await;
    events
        .unsubscribe(&unsubscribe_input(), Some(&who))
        .await
        .unwrap();
    second().await;
    events
        .unsubscribe(&unsubscribe_input(), Some(&who))
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
    assert_eq!(accepted["owner"], format!("executor:{}", "b".repeat(12)));
    assert_eq!(accepted["name"], "email.received");
    assert_eq!(accepted["arguments"], json!({"folder": "inbox"}));
    assert_eq!(accepted["callbackHost"], "chatgpt.example.com");
    let text = Value::Object(status).to_string();
    for hidden in [
        secret().as_str(),
        "/events/callback",
        "delegated-fixture",
        owner.as_str(),
    ] {
        assert!(!text.contains(hidden), "{hidden}");
    }
    for _ in 0..35 {
        events.record_discovery(None).await;
    }
    assert_eq!(events.store().requests().await.unwrap().len(), 30);
}

#[tokio::test(start_paused = true)]
async fn keeps_token_rotated_rows_terminal_without_decrypting_with_the_new_key() {
    let clock = clock();
    let db = test_store(&clock).await;
    let original = service(&db.store, &clock).service;
    original
        .subscribe(&subscribe_input("inbox"), None)
        .await
        .unwrap();
    original
        .record_email(&inbox("<one@example.test>"))
        .await
        .unwrap();
    let rotated = service_with(&db.store, &clock, "rotated-token", 204, None, echo_ports());
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
        .subscribe(&subscribe_input("inbox"), None)
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
        .unsubscribe(&unsubscribe_input(), None)
        .await
        .unwrap();
    let second = events
        .subscribe(&subscribe_input("inbox"), None)
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
    let events = service_with(
        &db.store,
        &clock,
        "test-omni-bearer",
        204,
        None,
        Ports::default(),
    )
    .service;
    events
        .subscribe(&subscribe_input("inbox"), None)
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
    let events = service_with(&db.store, &clock, "test-omni-bearer", 204, None, ports).service;
    events
        .subscribe(&subscribe_input("inbox"), None)
        .await
        .unwrap();
    events
        .subscribe(&subscribe_input("archive"), None)
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
        let events = service_with(
            &db.store,
            &clock,
            "test-omni-bearer",
            status,
            None,
            echo_ports(),
        )
        .service;
        events
            .subscribe(&subscribe_input("inbox"), None)
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
        .subscribe(&subscribe_input("inbox"), None)
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
    let events = McpEventService::new(
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
        None,
        echo_ports(),
    );
    events
        .subscribe(&subscribe_input("inbox"), None)
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
    let events = McpEventService::new(
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
        None,
        echo_ports(),
    );
    let worker = {
        let events = events.clone();
        tokio::spawn(async move {
            events
                .delivery_worker(tokio_util::sync::CancellationToken::new())
                .await;
        })
    };
    events
        .subscribe(&subscribe_input("inbox"), None)
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
        .subscribe(
            &SubscribeInput {
                ttl_ms: Some(60_000),
                ..subscribe_input("inbox")
            },
            None,
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
        .subscribe(&subscribe_input("inbox"), None)
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
            ("owner", JsValue::String("direct".into())),
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
            .subscribe(&subscribe_input("inbox"), None)
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
        .subscribe(&claude(json!({"project": "../etc"}), URL), None)
        .await;
    assert_eq!(
        failure.unwrap_err().to_string(),
        "Event subscription rejected: invalid_arguments"
    );
    events
        .subscribe(&claude(json!({"project": "omni"}), URL), None)
        .await
        .unwrap();
    events
        .subscribe(&claude(json!({}), &format!("{URL}/all")), None)
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
    let events = McpEventService::new(
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
        None,
        echo_ports(),
    );
    events
        .subscribe(&subscribe_input("inbox"), None)
        .await
        .unwrap();
    let slow = {
        let events = events.clone();
        tokio::spawn(async move {
            events
                .subscribe(
                    &SubscribeInput {
                        url: format!("{URL}/slow"),
                        ..subscribe_input("archive")
                    },
                    None,
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
