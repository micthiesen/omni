//! `notify_owner_blocked` and `notify_owner_requested`: one push per
//! idempotency key, uncertain pushes never resent, a shared durable rate
//! limit, and validated links.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_alerts::{AppDelivery, AppPushRoute, Pushover, PushoverChannel, PushoverMessage};
use omni_core::clock::{SharedClock, TestClock};
use omni_http::{HttpClient, HttpConfig, SideEffectMode};
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput, ToolPhase};
use omni_personal::mcp::owner_notify::tools;
use omni_personal::owner_notify::{DAILY_LIMIT, HOURLY_LIMIT, OwnerNotification, OwnerNotifier};
use omni_store::EntityOps;
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;

const HOUR_MS: i64 = 60 * 60 * 1000;

struct ScriptedRoute {
    reply: AppDelivery,
    seen: Mutex<Vec<(PushoverChannel, PushoverMessage)>>,
}

impl AppPushRoute for ScriptedRoute {
    fn deliver<'a>(
        &'a self,
        channel: PushoverChannel,
        message: &'a PushoverMessage,
    ) -> BoxFuture<'a, AppDelivery> {
        self.seen.lock().unwrap().push((channel, message.clone()));
        let reply = self.reply.clone();
        Box::pin(async move { reply })
    }
}

struct Harness {
    tools: Vec<McpTool>,
    pushover: Pushover,
    route: Option<Arc<ScriptedRoute>>,
    clock: Arc<TestClock>,
    store: TestStore,
}

async fn harness(token: Option<&str>, route: Option<AppDelivery>) -> Harness {
    let clock = test_clock(TEST_EPOCH_MS);
    let shared: SharedClock = clock.clone();
    let store = TestStore::new(shared.clone()).await;
    let http = HttpClient::new(HttpConfig {
        connect_timeout: None,
        offline: true,
    })
    .unwrap();
    let pushover = Pushover::with_credentials(
        http,
        Some("user".to_owned()),
        token.map(|t| (PushoverChannel::General, t.to_owned())),
        SideEffectMode::Record,
    );
    let route = route.map(|reply| {
        let route = Arc::new(ScriptedRoute {
            reply,
            seen: Mutex::default(),
        });
        pushover.set_app_route(route.clone()).unwrap();
        route
    });
    let notifier = OwnerNotifier::new(
        store.store.clone(),
        shared,
        pushover.clone(),
        TaskTracker::new(),
    );
    Harness {
        tools: tools(notifier).unwrap(),
        pushover,
        route,
        clock,
        store,
    }
}

impl Harness {
    async fn call(&self, name: &str, input: Value) -> Result<Value, (ToolPhase, String)> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        match tool.handler.call(input, standalone_context("test")).await {
            Ok(ToolOutput::Structured(map)) => Ok(Value::Object(map)),
            Ok(ToolOutput::Custom { structured, .. }) => Ok(Value::Object(structured)),
            Err(e) => Err((e.phase, e.message)),
        }
    }

    async fn rows(&self) -> Vec<OwnerNotification> {
        self.store
            .store
            .read(|docs| docs.get_all::<OwnerNotification>())
            .await
            .unwrap()
    }
}

fn blocked(key: &str) -> Value {
    json!({
        "idempotencyKey": key,
        "task": "Omni deploy",
        "blocker": "The production deploy needs deploy approval.",
        "steps": ["1. Open the deploy approval page", "Approve the omni deploy request"],
        "fromPhone": true,
        "meanwhile": "Running the remaining tests; checking every 10 minutes",
        "url": "https://deploy.example/approvals/42",
        "urlTitle": "Open approval",
    })
}

fn requested(key: &str) -> Value {
    json!({
        "idempotencyKey": key,
        "request": "ping me when the import finishes",
        "title": "Import finished",
        "message": "All 1,204 rows imported; 3 skipped as duplicates.",
    })
}

#[tokio::test(start_paused = true)]
async fn a_blocker_is_sent_once_with_numbered_steps_and_a_link() {
    let h = harness(Some("general"), None).await;
    let first = h
        .call("notify_owner_blocked", blocked("blocked-key-0000001"))
        .await
        .unwrap();
    assert_eq!(first["status"], "recorded");
    assert_eq!(first["replayed"], false);
    assert_eq!(first["kind"], "blocked");
    assert_eq!(first["remainingThisHour"], HOURLY_LIMIT - 1);
    assert_eq!(first["remainingToday"], DAILY_LIMIT - 1);

    let pushes = h.pushover.recorded();
    assert_eq!(pushes.len(), 1);
    let message = &pushes[0].message;
    assert_eq!(message.title.as_deref(), Some("Blocked: Omni deploy"));
    assert_eq!(
        message.message,
        "The production deploy needs deploy approval.\n\nFrom your phone:\n1. Open the deploy approval page\n2. Approve the omni deploy request\n\nMeanwhile: Running the remaining tests; checking every 10 minutes"
    );
    assert_eq!(
        message.url.as_deref(),
        Some("https://deploy.example/approvals/42")
    );
    assert_eq!(message.url_title.as_deref(), Some("Open approval"));
    assert_eq!(message.priority, None);

    let again = h
        .call("notify_owner_blocked", blocked("blocked-key-0000001"))
        .await
        .unwrap();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["status"], "recorded");
    assert_eq!(again["reservedAt"], first["reservedAt"]);
    assert_eq!(h.pushover.recorded().len(), 1, "the same key never resends");
}

#[tokio::test(start_paused = true)]
async fn an_app_delivery_reports_sent_and_a_requested_note_quotes_the_request() {
    let h = harness(Some("general"), Some(AppDelivery::Delivered)).await;
    let out = h
        .call(
            "notify_owner_requested",
            json!({
                "idempotencyKey": "requested-key-00001",
                "request": "ping me when the import finishes",
                "title": "Import finished",
                "message": "All rows imported.",
                "url": "https://github.com/example/pr/1",
                "priority": "high",
            }),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "sent");
    assert_eq!(out["kind"], "requested");
    assert!(out["sentAt"].is_string());
    let seen = h.route.as_ref().unwrap().seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (channel, message) = &seen[0];
    assert_eq!(*channel, PushoverChannel::General);
    assert_eq!(message.title.as_deref(), Some("Import finished"));
    assert_eq!(
        message.message,
        "All rows imported.\n\nYou asked: \"ping me when the import finishes\""
    );
    assert_eq!(message.url_title.as_deref(), Some("Open"));
    assert_eq!(message.priority, Some(1));
    assert!(h.pushover.recorded().is_empty(), "no Pushover fallback");
}

#[tokio::test(start_paused = true)]
async fn an_uncertain_push_is_reported_and_never_retried() {
    let h = harness(
        Some("general"),
        Some(AppDelivery::Uncertain("APNs timed out".to_owned())),
    )
    .await;
    let first = h
        .call("notify_owner_requested", requested("uncertain-key-0001"))
        .await
        .unwrap();
    assert_eq!(first["status"], "uncertain");
    assert_eq!(first["replayed"], false);
    assert_eq!(first["sentAt"], Value::Null);
    let again = h
        .call("notify_owner_requested", requested("uncertain-key-0001"))
        .await
        .unwrap();
    assert_eq!(again["status"], "uncertain");
    assert_eq!(again["replayed"], true);
    let route = h.route.as_ref().unwrap();
    assert_eq!(route.seen.lock().unwrap().len(), 1, "never resent");
    assert!(h.pushover.recorded().is_empty(), "no fallback either");
    // The uncertain reservation still counts against the limit.
    assert_eq!(again["remainingThisHour"], HOURLY_LIMIT - 1);
}

#[tokio::test(start_paused = true)]
async fn a_definite_rejection_releases_the_key() {
    let h = harness(None, None).await;
    let (phase, message) = h
        .call("notify_owner_requested", requested("rejected-key-00001"))
        .await
        .unwrap_err();
    assert_eq!(phase, ToolPhase::Execute);
    assert!(message.contains("not delivered"), "{message}");
    assert!(message.contains("not configured"), "{message}");
    assert!(h.rows().await.is_empty(), "the reservation is released");
}

#[tokio::test(start_paused = true)]
async fn reusing_a_key_for_different_content_is_refused() {
    let h = harness(Some("general"), None).await;
    h.call("notify_owner_requested", requested("reused-key-0000001"))
        .await
        .unwrap();
    let mut other = requested("reused-key-0000001");
    other["title"] = json!("Import failed");
    let (phase, message) = h.call("notify_owner_requested", other).await.unwrap_err();
    assert_eq!(phase, ToolPhase::Execute);
    assert!(message.contains("already used"), "{message}");
    assert_eq!(h.pushover.recorded().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_hourly_and_daily_limits_are_shared_across_both_tools() {
    const _: () =
        assert!(HOURLY_LIMIT.is_multiple_of(2) && DAILY_LIMIT.is_multiple_of(HOURLY_LIMIT));
    let h = harness(Some("general"), None).await;
    for i in 0..HOURLY_LIMIT / 2 {
        h.call(
            "notify_owner_blocked",
            blocked(&format!("limit-blocked-{i:05}")),
        )
        .await
        .unwrap();
        h.call(
            "notify_owner_requested",
            requested(&format!("limit-request-{i:05}")),
        )
        .await
        .unwrap();
    }
    let (phase, message) = h
        .call("notify_owner_requested", requested("limit-request-99999"))
        .await
        .unwrap_err();
    assert_eq!(phase, ToolPhase::Execute);
    assert!(message.contains("limit reached"), "{message}");
    assert!(message.contains("2026-01-01T01:00:00.000Z"), "{message}");
    assert_eq!(h.pushover.recorded().len(), HOURLY_LIMIT);
    // A replay of an earlier key still answers while limited.
    let replay = h
        .call("notify_owner_blocked", blocked("limit-blocked-00000"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["remainingThisHour"], 0);

    // Hour by hour, the day fills at the daily limit.
    let full_hours = (DAILY_LIMIT / HOURLY_LIMIT) as i64;
    for hour in 1..full_hours {
        h.clock.set(TEST_EPOCH_MS + hour * HOUR_MS);
        for i in 0..HOURLY_LIMIT {
            h.call(
                "notify_owner_requested",
                requested(&format!("limit-h{hour}-{i:011}")),
            )
            .await
            .unwrap();
        }
    }
    h.clock.set(TEST_EPOCH_MS + full_hours * HOUR_MS);
    let (_, message) = h
        .call("notify_owner_requested", requested("limit-day-00000001"))
        .await
        .unwrap_err();
    assert!(message.contains("limit reached"), "{message}");
    assert!(message.contains("2026-01-02T00:00:00.000Z"), "{message}");
    assert_eq!(h.pushover.recorded().len(), DAILY_LIMIT);

    h.clock.set(TEST_EPOCH_MS + 24 * HOUR_MS);
    let out = h
        .call("notify_owner_requested", requested("limit-day-00000001"))
        .await
        .unwrap();
    assert_eq!(out["status"], "recorded");
}

#[tokio::test(start_paused = true)]
async fn links_must_be_https_and_priority_is_never_emergency() {
    let h = harness(Some("general"), None).await;
    for url in [
        "http://example.com/x",
        "https://user:secret@example.com/",
        "ftp://example.com",
    ] {
        let mut input = blocked("bad-url-key-000001");
        input["url"] = json!(url);
        let (phase, _) = h.call("notify_owner_blocked", input).await.unwrap_err();
        assert_eq!(phase, ToolPhase::Input, "{url}");
    }
    let mut input = requested("bad-priority-00001");
    input["priority"] = json!("emergency");
    let (phase, _) = h.call("notify_owner_requested", input).await.unwrap_err();
    assert_eq!(phase, ToolPhase::Input);
    let mut input = requested("orphan-title-00001");
    input["urlTitle"] = json!("Open");
    let (phase, _) = h.call("notify_owner_requested", input).await.unwrap_err();
    assert_eq!(phase, ToolPhase::Input);
    let mut input = blocked("blank-step-key-0001");
    input["steps"] = json!(["   "]);
    let (phase, _) = h.call("notify_owner_blocked", input).await.unwrap_err();
    assert_eq!(phase, ToolPhase::Input);
    assert!(h.pushover.recorded().is_empty());
    assert!(h.rows().await.is_empty(), "nothing was reserved");
}
