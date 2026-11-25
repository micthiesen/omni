//! The event journal: stored routes send events to stored targets, notices
//! wait for a missing target, every attempt is recorded, a dead delivery
//! logs at ERROR and can be retried, and the admin API never shows a key.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;
use crate::gateway_support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{clock, test_store};
use futures::future::BoxFuture;
use omni_api::outbound::EventTargetUpsert;
use omni_core::clock::{Clock as _, TestClock};
use omni_core::email::FetchedEmail;
use omni_mcp::events::persistence::{DeliveryFailure, DeliveryStatus, EventDelivery};
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::targets::{
    LiveTargetTransport, OpenTarget, TargetError, TargetResponse, TargetTransport,
};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_runtime::ports::{ArchiveEcho, NoticeStatus, PortError, Ports, TargetNotice};
use omni_store::Store;
use omni_store::entity::EntityOps as _;
use serde_json::{Value, json};

const KEY: &str = "inbox-bot-key-0123456789";
const URL: &str = "https://rakazo.example/api/v1/bots/inbox/webhook";

/// `(target, event, idempotency key, body)` of each post.
type Sent = Vec<(String, String, String, Value)>;

/// Records what the journal sends and answers from a script (then 200).
#[derive(Clone, Default)]
struct Targets {
    sent: Arc<Mutex<Sent>>,
    answers: Arc<Mutex<VecDeque<Result<TargetResponse, TargetError>>>>,
}

impl Targets {
    fn answer(&self, status: u16) {
        self.answers.lock().unwrap().push_back(Ok(TargetResponse {
            status,
            ignored: false,
        }));
    }

    fn sent(&self) -> Sent {
        self.sent.lock().unwrap().clone()
    }
}

impl TargetTransport for Targets {
    fn post<'a>(
        &'a self,
        target: &'a OpenTarget,
        event: &'a str,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<TargetResponse, TargetError>> {
        assert_eq!(target.key, KEY, "the key is opened for the send");
        self.sent.lock().unwrap().push((
            target.id.clone(),
            event.to_owned(),
            idempotency_key.to_owned(),
            serde_json::from_str(body).unwrap(),
        ));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(TargetResponse {
                status: 200,
                ignored: false,
            }));
        Box::pin(async move { answer })
    }
}

struct NoSubscribers;

impl WebhookPort for NoSubscribers {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        _: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        panic!("no subscription exists in these tests")
    }
}

#[derive(Default)]
struct NoMoves;

impl ArchiveEcho for NoMoves {
    fn is_archive_action_message<'a>(
        &'a self,
        _message_id: &'a str,
        _origin: Option<&'a omni_core::email::EmailOrigin>,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        Box::pin(async { Ok(false) })
    }
}

struct Journal {
    events: McpEventService,
    targets: Targets,
    clock: Arc<TestClock>,
    store: Store,
    _db: omni_testkit::TestStore,
}

impl Journal {
    async fn new() -> Self {
        let clock = clock();
        let db = test_store(&clock).await;
        let ports = Ports::default();
        ports.set_archive_echo(Arc::new(NoMoves)).unwrap();
        let events = common::with_active_owners(McpEventService::new(
            "test-omni-bearer",
            db.store.clone(),
            clock.clone(),
            Arc::new(NoSubscribers),
            ports,
        ));
        let targets = Targets::default();
        events.set_target_transport(Arc::new(targets.clone()));
        Self {
            events,
            targets,
            clock,
            store: db.store.clone(),
            _db: db,
        }
    }

    async fn target(&self, id: &str) {
        self.events
            .upsert_target(
                id,
                &EventTargetUpsert {
                    label: "Inbox bot".to_owned(),
                    url: URL.to_owned(),
                    key: Some(KEY.to_owned()),
                    connect: Some("npm.compose_default".to_owned()),
                },
            )
            .await
            .unwrap();
    }

    fn advance(&self, ms: i64) {
        self.clock.set(self.clock.now_ms() + ms);
    }

    async fn rows(&self) -> Vec<EventDelivery> {
        self.store
            .read(|docs| docs.get_all::<EventDelivery>())
            .await
            .unwrap()
    }
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

#[tokio::test(start_paused = true)]
async fn a_route_sends_each_matching_event_to_its_target_as_subscribers_get_it() {
    let journal = Journal::new().await;
    journal.target("inbox").await;
    let args = journal
        .events
        .parse_route_arguments("email.received", &json!({"folder": "inbox"}))
        .unwrap();
    let route = journal
        .events
        .create_route("email.received", args.clone(), "inbox")
        .await
        .unwrap();
    // A route keeps its event "subscribed" (integration feeds poll for it).
    assert_eq!(
        journal
            .events
            .active_arguments("email.received")
            .await
            .unwrap(),
        [args]
    );

    journal
        .events
        .record_email(&email("<one@example.test>", "INBOX"))
        .await
        .unwrap();
    // Another folder does not match the route's arguments.
    journal
        .events
        .record_email(&email("<two@example.test>", "Archive"))
        .await
        .unwrap();
    journal.events.drain().await.unwrap();
    let sent = journal.targets.sent();
    assert_eq!(sent.len(), 1);
    let (target, event, key, body) = &sent[0];
    assert_eq!(target, "inbox");
    assert_eq!(event, "email.received");
    // The same body and id an `events/subscribe` webhook carries, so the
    // receiver can collapse it with a delivery that arrived another way.
    assert_eq!(key, body["eventId"].as_str().unwrap());
    assert_eq!(body["name"], "email.received");
    assert_eq!(body["data"]["messageId"], "<one@example.test>");
    assert_eq!(body["data"]["folder"], "inbox");
    assert_eq!(body["cursor"], Value::Null);

    // A disabled route receives nothing.
    journal
        .events
        .set_route_enabled(&route.id, false)
        .await
        .unwrap();
    journal
        .events
        .record_email(&email("<three@example.test>", "INBOX"))
        .await
        .unwrap();
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 1);
    assert!(
        journal
            .events
            .active_arguments("email.received")
            .await
            .unwrap()
            .is_empty()
    );
    // A target a route uses cannot be removed.
    assert!(journal.events.delete_target("inbox").await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_refused_delivery_dies_loudly_and_a_retry_sends_it_again() {
    let journal = Journal::new().await;
    journal.target("inbox").await;
    let args = journal
        .events
        .parse_route_arguments("email.received", &json!({"folder": "inbox"}))
        .unwrap();
    journal
        .events
        .create_route("email.received", args, "inbox")
        .await
        .unwrap();
    journal.targets.answer(403);
    let logs = omni_testkit::capture_logs();
    journal
        .events
        .record_email(&email("<one@example.test>", "INBOX"))
        .await
        .unwrap();
    journal.events.drain().await.unwrap();
    let row = journal.rows().await.remove(0);
    assert_eq!(row.status, DeliveryStatus::Failed);
    assert_eq!(row.failure, Some(DeliveryFailure::Rejected));
    assert_eq!(row.history.len(), 1);
    assert_eq!(row.history[0].status, Some(403));
    assert_eq!(row.history[0].error.as_deref(), Some("http_403"));
    // The dead letter is an ERROR (which alerts), naming the target, never its key.
    let dead: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e.level == tracing::Level::ERROR)
        .collect();
    assert_eq!(dead.len(), 1, "{dead:?}");
    assert!(dead[0].message.contains("failed for good"));
    assert!(!format!("{dead:?}").contains(KEY));

    let overview = journal.events.outbound(true).await.unwrap();
    assert_eq!(overview.counts.failed, 1);
    assert_eq!(overview.deliveries.len(), 1);
    let retried = journal
        .events
        .retry_delivery(&overview.deliveries[0].id)
        .await
        .unwrap();
    assert_eq!(retried.attempts, 1);
    journal.events.drain().await.unwrap();
    let row = journal.rows().await.remove(0);
    assert_eq!(row.status, DeliveryStatus::Delivered);
    assert_eq!(row.history.len(), 2);
    let keys: Vec<String> = journal
        .targets
        .sent()
        .into_iter()
        .map(|(_, _, key, _)| key)
        .collect();
    assert_eq!(
        keys[0], keys[1],
        "the same Idempotency-Key on every attempt"
    );
    // A delivered row is not retried.
    assert!(journal.events.retry_delivery(&row.id).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn retries_back_off_and_stop_a_day_after_the_event() {
    let journal = Journal::new().await;
    journal.target("inbox").await;
    for _ in 0..200 {
        journal.targets.answer(503);
    }
    journal
        .events
        .notify_target(TargetNotice {
            target: "inbox".to_owned(),
            key: "notice-1".to_owned(),
            name: "recipe.request".to_owned(),
            body: json!({"event": "recipe.request"}),
        })
        .await
        .unwrap();
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 1);
    // Not before 30 s, then the wait doubles.
    journal.advance(29_000);
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 1);
    journal.advance(1_000);
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 2);
    journal.advance(59_000);
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 2);
    journal.advance(1_000);
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 3);
    for _ in 0..60 {
        journal.advance(31 * 60_000);
        journal.events.drain().await.unwrap();
    }
    let row = journal.rows().await.remove(0);
    assert_eq!(row.status, DeliveryStatus::Failed);
    assert_eq!(row.failure, Some(DeliveryFailure::AttemptsExhausted));
    // About 50 attempts in the day; the row keeps the newest 20.
    let attempts = journal.targets.sent().len();
    assert!((45..=60).contains(&attempts), "{attempts}");
    assert_eq!(row.history.len(), 20);
    let states = journal
        .events
        .notice_states(&["notice-1".to_owned()])
        .await
        .unwrap();
    assert_eq!(states["notice-1"].status, NoticeStatus::Failed);
}

#[tokio::test(start_paused = true)]
async fn a_notice_waits_for_its_target_and_is_sent_once_per_key() {
    let journal = Journal::new().await;
    let notice = TargetNotice {
        target: "kitchen".to_owned(),
        key: "req_aaaaaaaa".to_owned(),
        name: "recipe.request".to_owned(),
        body: json!({"event": "recipe.request", "requestId": "req_aaaaaaaa"}),
    };
    assert!(journal.events.notify_target(notice.clone()).await.unwrap());
    assert!(!journal.events.notify_target(notice.clone()).await.unwrap());
    journal.events.drain().await.unwrap();
    assert!(journal.targets.sent().is_empty());
    let state = |events: McpEventService| async move {
        events
            .notice_states(&["req_aaaaaaaa".to_owned()])
            .await
            .unwrap()["req_aaaaaaaa"]
    };
    assert_eq!(
        state(journal.events.clone()).await.status,
        NoticeStatus::Unconfigured
    );
    journal.target("kitchen").await;
    journal.events.drain().await.unwrap();
    let sent = journal.targets.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].2, "req_aaaaaaaa");
    assert_eq!(sent[0].3["requestId"], "req_aaaaaaaa");
    assert_eq!(
        state(journal.events.clone()).await.status,
        NoticeStatus::Delivered
    );
    // A settled notice cannot be cancelled; a pending one can.
    assert!(!journal.events.cancel_notice("req_aaaaaaaa").await.unwrap());
    journal
        .events
        .notify_target(TargetNotice {
            key: "req_bbbbbbbb".to_owned(),
            ..notice
        })
        .await
        .unwrap();
    assert!(journal.events.cancel_notice("req_bbbbbbbb").await.unwrap());
    journal.events.drain().await.unwrap();
    assert_eq!(journal.targets.sent().len(), 1);
}

#[tokio::test]
async fn the_live_sender_posts_with_the_bearer_key_and_idempotency_key() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bots/kitchen/webhook"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(header("idempotency-key", "req_aaaaaaaa"))
        .and(header("x-omni-event", "recipe.request"))
        .and(header("x-fond-event", "recipe.request"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"event": "recipe.request"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ignored": true})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bots/proxy/webhook"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;
    let client = omni_testkit::mock_http(&server, &["https://rakazo.example"]);
    let target = |path: &str| OpenTarget {
        id: "kitchen".to_owned(),
        url: format!("https://rakazo.example{path}").parse().unwrap(),
        key: KEY.to_owned(),
        connect: None,
    };
    let live = LiveTargetTransport::new(client.clone(), omni_http::SideEffectMode::Live);
    let body = r#"{"event":"recipe.request"}"#;
    let answer = live
        .post(
            &target("/api/v1/bots/kitchen/webhook"),
            "recipe.request",
            "req_aaaaaaaa",
            body,
        )
        .await
        .unwrap();
    assert_eq!(
        answer,
        TargetResponse {
            status: 200,
            ignored: true
        }
    );
    // A page that is not a webhook's answer is not worth sending again.
    let page = live
        .post(
            &target("/api/v1/bots/proxy/webhook"),
            "recipe.request",
            "req_aaaaaaaa",
            body,
        )
        .await;
    assert_eq!(page, Err(TargetError::Unusable));
    // A preview or shadow run sends nothing.
    let recorded = LiveTargetTransport::new(client, omni_http::SideEffectMode::Record);
    assert!(
        recorded
            .post(
                &target("/api/v1/bots/kitchen/webhook"),
                "recipe.request",
                "req_aaaaaaaa",
                body
            )
            .await
            .is_ok()
    );
    server.verify().await;
}

async fn admin(
    fixture: &gateway_support::Fixture,
    method: &str,
    uri: &str,
    body: Option<Value>,
    trusted: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "omni.boris")
        .header("origin", "http://omni.boris")
        .header("content-type", "application/json");
    if trusted {
        builder = builder.header("cookie", fixture.owner_cookie.as_str());
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let exchange = common::send(&fixture.admin, request).await;
    (
        exchange.status,
        serde_json::from_str(&exchange.text).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn targets_and_routes_are_managed_by_a_trusted_browser_and_keys_never_come_back() {
    let fixture = gateway_support::Fixture::new().await;
    let target =
        json!({"label": "Inbox bot", "url": URL, "key": KEY, "connect": "npm.compose_default"});
    let (status, _) = admin(
        &fixture,
        "PUT",
        "/api/gateway/outbound/targets/inbox",
        Some(target.clone()),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an untrusted browser");
    let (status, saved) = admin(
        &fixture,
        "PUT",
        "/api/gateway/outbound/targets/inbox",
        Some(target),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["keySet"], true);
    // Without a key the stored one stays.
    let (status, relabeled) = admin(
        &fixture,
        "PUT",
        "/api/gateway/outbound/targets/inbox",
        Some(json!({"label": "Inbox", "url": URL, "connect": "npm.compose_default"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(relabeled["label"], "Inbox");
    assert_eq!(relabeled["keySet"], true);
    let (status, _) = admin(
        &fixture,
        "PUT",
        "/api/gateway/outbound/targets/Bad%20Id",
        Some(json!({"label": "x", "url": URL, "key": KEY})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, route) = admin(
        &fixture,
        "POST",
        "/api/gateway/outbound/routes",
        Some(
            json!({"event": "email.received", "arguments": {"folder": "inbox"}, "target": "inbox"}),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let (status, _) = admin(
        &fixture,
        "POST",
        "/api/gateway/outbound/routes",
        Some(
            json!({"event": "email.received", "arguments": {"folder": "spam"}, "target": "inbox"}),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "invalid arguments");
    let (status, _) = admin(
        &fixture,
        "POST",
        "/api/gateway/outbound/routes",
        Some(json!({"event": "nope.event", "target": "inbox"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown event");

    let (status, overview) = admin(&fixture, "GET", "/api/gateway/outbound", None, false).await;
    assert_eq!(status, StatusCode::OK, "reading needs no trust");
    assert_eq!(overview["targets"][0]["id"], "inbox");
    assert_eq!(overview["routes"][0]["event"], "email.received");
    assert!(
        overview["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "email.received" && e["arguments"] == json!(["folder"]))
    );
    assert!(!overview.to_string().contains(KEY));

    let id = route["id"].as_str().unwrap();
    let (status, _) = admin(
        &fixture,
        "DELETE",
        "/api/gateway/outbound/targets/inbox",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a route still uses it");
    let (status, _) = admin(
        &fixture,
        "DELETE",
        &format!("/api/gateway/outbound/routes/{id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = admin(
        &fixture,
        "DELETE",
        "/api/gateway/outbound/targets/inbox",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    fixture.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_environment_target_is_imported_once_and_stays_removed() {
    let journal = Journal::new().await;
    let import = || {
        journal
            .events
            .import_target("chief", "Chief", URL, KEY, Some("npm.compose_default"))
    };
    assert!(import().await.unwrap());
    assert!(!import().await.unwrap());
    // The owner removed it on /gateway: the old setting does not bring it back.
    journal.events.delete_target("chief").await.unwrap();
    assert!(!import().await.unwrap());
    assert!(
        journal
            .events
            .outbound(false)
            .await
            .unwrap()
            .targets
            .is_empty()
    );
    // Old names that were valid notify targets still import, keyless too.
    assert!(
        journal
            .events
            .import_target("my_bot", "Bot", URL, "", None)
            .await
            .unwrap()
    );
    let targets = journal.events.outbound(false).await.unwrap().targets;
    assert_eq!(targets[0].id, "my_bot");
    assert!(!targets[0].key_set);
}
