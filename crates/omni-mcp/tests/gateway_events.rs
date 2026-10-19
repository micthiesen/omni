//! MCP Events through the gateway, inside Omni: integration events offered
//! under prefixed names, Omni-owned poll feeds with durable cursors, and
//! delivery through Omni's outbox, across restarts, crashes between
//! queueing and storing a cursor, upstream outages, replay gaps, revoked
//! clients and idle feeds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod gateway_support;

use std::sync::atomic::Ordering;

use gateway_support::{FakeUpstream, Fixture, Options, subscribe_params};
use omni_api::gateway::{AuthKind, IntegrationPatch};
use omni_core::clock::Clock as _;
use omni_mcp::events::persistence::{EventDelivery, EventSubscription};
use omni_mcp::events::service::{EventServiceError, SubscribeInput, SubscriptionRejection};
use omni_mcp::gateway::events::poll_feeds;
use omni_mcp::gateway::model::FeedRow;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde_json::{Value, json};

const EVENT: &str = "art__comment.created";

async fn events_fixture() -> (Fixture, FakeUpstream, String) {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.capabilities.lock().unwrap() =
        json!({"tools": {}, "extensions": {"io.modelcontextprotocol/events": {}}});
    *upstream.state.events.lock().unwrap() = vec![
        json!({
            "name": "comment.created",
            "description": "Someone commented",
            "delivery": ["poll", "webhook"],
            "inputSchema": {"type": "object", "properties": {"artifact": {"type": "string"}}, "additionalProperties": false},
            "payloadSchema": {"type": "object"},
        }),
        json!({"name": "push.only", "delivery": ["push"]}),
    ];
    fixture.add("art", &upstream.url, AuthKind::None).await;
    assert!(fixture.refresh("art").await.ok);
    let token = fixture.client_token().await;
    (fixture, upstream, token)
}

async fn subscribe(fixture: &Fixture, token: &str, arguments: Value) -> Value {
    fixture
        .post(
            token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "events/subscribe", "params": subscribe_params(EVENT, arguments)}),
        )
        .await
        .message()
}

async fn feeds(fixture: &Fixture) -> Vec<FeedRow> {
    fixture
        .store
        .read(|docs| docs.get_all::<FeedRow>())
        .await
        .unwrap()
}

async fn delivered(fixture: &Fixture) -> Vec<(String, Value)> {
    fixture.events.drain().await.unwrap();
    fixture
        .webhook
        .delivered
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.event_id.clone(), Value::Object(e.data.clone())))
        .collect()
}

/// Makes every feed due now.
async fn due_now(fixture: &Fixture) {
    let now = fixture.clock.now_ms();
    for mut feed in feeds(fixture).await {
        feed.next_poll_at = now;
        fixture
            .store
            .write(move |tx| tx.upsert(&feed, UpsertOpts::default()))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn integration_events_are_offered_with_prefixed_names() {
    let (fixture, _upstream, token) = events_fixture().await;
    let listed = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "events/list"}),
        )
        .await
        .message();
    let names: Vec<&str> = listed["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"email.received"),
        "Omni's own events stay listed"
    );
    assert!(names.contains(&EVENT));
    assert!(
        !names.contains(&"art__push.only"),
        "events Omni cannot follow are not offered"
    );
    let entry = listed["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == EVENT)
        .unwrap();
    assert_eq!(entry["delivery"], json!(["webhook"]));
    assert!(
        entry["description"]
            .as_str()
            .unwrap()
            .starts_with("[art name]")
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_subscription_follows_new_events_only() {
    let (fixture, upstream, token) = events_fixture().await;
    upstream
        .state
        .emit("comment.created", json!({"text": "before"}));
    let reply = subscribe(&fixture, &token, json!({})).await;
    assert!(reply["result"]["id"].is_string(), "{reply}");
    let feed = &feeds(&fixture).await[0];
    assert_eq!(
        feed.cursor.as_deref(),
        Some("1"),
        "the feed starts at the current cursor"
    );
    let subscription = fixture
        .store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap();
    assert!(subscription[0].owner.starts_with("gateway:client:"));

    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert!(
        delivered(&fixture).await.is_empty(),
        "history is not delivered"
    );
    upstream
        .state
        .emit("comment.created", json!({"text": "after"}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let sent = delivered(&fixture).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, json!({"text": "after"}));
    assert_eq!(feeds(&fixture).await[0].cursor.as_deref(), Some("2"));

    // Unknown arguments are refused before any subscription is stored.
    let refused = subscribe(&fixture, &token, json!({"other": "x"})).await;
    assert_eq!(refused["error"]["code"], -32602);
    fixture.stop().await;
}

#[tokio::test]
async fn a_restart_resumes_from_the_cursor_and_a_replay_delivers_nothing_twice() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.emit("comment.created", json!({"n": 1}));
    upstream.state.emit("comment.created", json!({"n": 2}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let first = delivered(&fixture).await;
    assert_eq!(first.len(), 2);

    // A crash after queueing but before the cursor was stored: the stored
    // cursor is older than what was delivered.
    let mut feed = feeds(&fixture).await.remove(0);
    feed.cursor = Some("0".into());
    fixture
        .store
        .write(move |tx| tx.upsert(&feed, UpsertOpts::default()))
        .await
        .unwrap();
    let store = fixture.store.clone();
    let clock = fixture.clock.clone();
    let before = fixture;
    let restarted = Fixture::over(store, clock, Options::default()).await;
    assert!(restarted.refresh("art").await.ok);
    upstream.state.emit("comment.created", json!({"n": 3}));
    due_now(&restarted).await;
    poll_feeds(&restarted.gateway, &restarted.events)
        .await
        .unwrap();
    let after = delivered(&restarted).await;
    assert_eq!(after.len(), 1, "only the new event: {after:?}");
    assert_eq!(after[0].1, json!({"n": 3}));
    let ids: Vec<&String> = first.iter().map(|(id, _)| id).collect();
    assert!(!ids.contains(&&after[0].0));
    assert_eq!(feeds(&restarted).await[0].cursor.as_deref(), Some("3"));
    restarted.stop().await;
    before.stop().await;
}

#[tokio::test]
async fn an_outage_backs_off_keeps_the_cursor_and_replays_after_recovery() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.emit("comment.created", json!({"n": 1}));
    upstream.state.emit("comment.created", json!({"n": 2}));
    upstream.state.poll_failures.store(1, Ordering::Relaxed);
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.cursor.as_deref(), Some("0"));
    assert_eq!(feed.failures, 1);
    assert!(feed.error.is_some());
    assert!(
        feed.next_poll_at > fixture.clock.now_ms() + 30_000,
        "backs off"
    );
    assert!(delivered(&fixture).await.is_empty());
    // Not due yet: nothing is polled.
    let polled = poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(polled, 0);
    fixture.clock.set(fixture.clock.now_ms() + 120_000);
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let sent = delivered(&fixture).await;
    assert_eq!(sent.len(), 2, "both missed events arrive once");
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.failures, 0);
    assert!(feed.error.is_none());
    fixture.stop().await;
}

#[tokio::test]
async fn a_replay_the_integration_cannot_complete_is_recorded_as_a_gap() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.emit("comment.created", json!({"n": 1}));
    upstream.state.truncate_next.store(true, Ordering::Relaxed);
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.gaps, 1);
    assert!(feed.last_gap_at.is_some());
    assert_eq!(
        delivered(&fixture).await.len(),
        1,
        "what it could replay still arrives"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_revoked_client_receives_nothing_more() {
    let (fixture, upstream, _) = events_fixture().await;
    let (row, token) = fixture.access.create_client("watcher").await.unwrap();
    subscribe(&fixture, &token, json!({})).await;
    fixture.access.revoke_client(&row.id).await.unwrap();
    upstream.state.emit("comment.created", json!({"n": 1}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert!(delivered(&fixture).await.is_empty());
    let rows = fixture
        .store
        .read(|docs| docs.get_all::<EventDelivery>())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].withheld.is_some(),
        "held, not delivered: {:?}",
        rows[0]
    );
    fixture.stop().await;
}

#[tokio::test]
async fn argument_sets_get_their_own_feeds_and_deliveries() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    subscribe(&fixture, &token, json!({"artifact": "abc"})).await;
    assert_eq!(feeds(&fixture).await.len(), 2);
    upstream.state.emit("comment.created", json!({"n": 1}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    // The fake ignores arguments, so both feeds see the event; each
    // subscription gets exactly its feed's delivery.
    let sent = delivered(&fixture).await;
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0].0, sent[1].0);
    let polls = upstream.state.calls("events/poll");
    assert!(
        polls
            .iter()
            .any(|p| p["arguments"] == json!({"artifact": "abc"}))
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_feed_idle_for_over_an_hour_starts_fresh() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "events/unsubscribe", "params": {"name": EVENT, "arguments": {}, "delivery": {"mode": "webhook", "url": "https://hooks.example/omni"}}}),
        )
        .await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert!(feeds(&fixture).await[0].idle_since.is_some());
    upstream
        .state
        .emit("comment.created", json!({"n": "while away"}));
    fixture.clock.set(fixture.clock.now_ms() + 2 * 3_600_000);
    subscribe(&fixture, &token, json!({})).await;
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert!(
        delivered(&fixture).await.is_empty(),
        "events while nobody listened are history"
    );
    upstream.state.emit("comment.created", json!({"n": "now"}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(delivered(&fixture).await.len(), 1);
    fixture.stop().await;
}

#[tokio::test]
async fn omni_events_can_be_subscribed_through_the_gateway() {
    let (fixture, _upstream, token) = events_fixture().await;
    let reply = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "events/subscribe", "params": subscribe_params("email.received", json!({"folder": "inbox"}))}),
        )
        .await
        .message();
    assert!(reply["result"]["refreshBefore"].is_string(), "{reply}");
    let subscriptions = fixture
        .store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap();
    assert_eq!(subscriptions[0].name, "email.received");
    assert!(subscriptions[0].owner.starts_with("gateway:client:"));
    assert!(
        feeds(&fixture).await.is_empty(),
        "Omni's own events need no feed"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_backlog_longer_than_one_pass_is_read_on_the_next_pass() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    for n in 0..260 {
        upstream.state.emit("comment.created", json!({"n": n}));
    }
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.cursor.as_deref(), Some("250"), "five pages of fifty");
    assert!(
        feed.next_poll_at <= fixture.clock.now_ms() + 15_000,
        "the rest is read at the shortest interval"
    );
    fixture.clock.set(fixture.clock.now_ms() + 15_000);
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(feeds(&fixture).await[0].cursor.as_deref(), Some("260"));
    fixture.stop().await;
}

#[tokio::test]
async fn an_event_the_integration_stops_offering_keeps_its_cursor() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.emit("comment.created", json!({"n": 1}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(delivered(&fixture).await.len(), 1);

    let offered = upstream.state.events.lock().unwrap().clone();
    upstream.state.events.lock().unwrap().clear();
    assert!(fixture.refresh("art").await.ok);
    upstream.state.emit("comment.created", json!({"n": 2}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.cursor.as_deref(), Some("1"), "nothing was skipped");
    let error = feed.error.expect("the feed says why it stopped");
    assert!(error.message.contains("does not offer"), "{error:?}");

    *upstream.state.events.lock().unwrap() = offered;
    assert!(fixture.refresh("art").await.ok);
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let sent = delivered(&fixture).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].1, json!({"n": 2}));
    fixture.stop().await;
}

#[tokio::test]
async fn an_event_without_an_id_is_counted_as_a_gap() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.log.lock().unwrap().push((
        1,
        json!({"name": "comment.created", "timestamp": "2026-10-18T12:00:00.000Z", "data": {"n": 1}}),
    ));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.gaps, 1);
    assert_eq!(feed.cursor.as_deref(), Some("1"));
    assert!(delivered(&fixture).await.is_empty());
    fixture.stop().await;
}

#[tokio::test]
async fn integration_events_are_not_offered_to_mcp_callers() {
    let (fixture, _upstream, _token) = events_fixture().await;
    let params = subscribe_params(EVENT, json!({}));
    let input = SubscribeInput {
        name: EVENT.to_owned(),
        arguments: json!({}),
        url: params["delivery"]["url"].as_str().unwrap().to_owned(),
        secret: params["delivery"]["secret"].as_str().unwrap().to_owned(),
        ttl_ms: None,
    };
    for owner in ["direct", "executor:principal"] {
        let refused = fixture.events.subscribe_as(owner, &input).await;
        assert!(
            matches!(
                refused,
                Err(EventServiceError::Rejected(
                    SubscriptionRejection::InvalidEvent
                ))
            ),
            "{owner}: {refused:?}"
        );
    }
    fixture.stop().await;
}

#[tokio::test]
async fn unsubscribing_works_while_the_integration_is_disabled() {
    let (fixture, upstream, token) = events_fixture().await;
    let reply = subscribe(&fixture, &token, json!({})).await;
    assert!(reply["result"]["id"].is_string(), "{reply}");
    fixture
        .gateway
        .patch(
            "art",
            IntegrationPatch {
                enabled: Some(false),
                ..IntegrationPatch::default()
            },
        )
        .await
        .unwrap();
    let params = subscribe_params(EVENT, json!({}));
    let reply = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "events/unsubscribe", "params": {
                "name": EVENT,
                "arguments": {},
                "delivery": {"mode": "webhook", "url": params["delivery"]["url"]},
            }}),
        )
        .await
        .message();
    assert!(reply.get("error").is_none(), "{reply}");
    let left: Vec<EventSubscription> = fixture
        .store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");

    // Offered again, the event reaches nobody.
    fixture
        .gateway
        .patch(
            "art",
            IntegrationPatch {
                enabled: Some(true),
                ..IntegrationPatch::default()
            },
        )
        .await
        .unwrap();
    assert!(fixture.refresh("art").await.ok);
    upstream.state.emit("comment.created", json!({"n": 1}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert!(delivered(&fixture).await.is_empty());
    fixture.stop().await;
}

#[tokio::test]
async fn a_new_server_url_restarts_feeds_and_records_the_gap() {
    let (fixture, upstream, token) = events_fixture().await;
    subscribe(&fixture, &token, json!({})).await;
    upstream.state.emit("comment.created", json!({"n": 1}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(delivered(&fixture).await.len(), 1);

    let moved = FakeUpstream::start().await;
    *moved.state.capabilities.lock().unwrap() = upstream.state.capabilities.lock().unwrap().clone();
    *moved.state.events.lock().unwrap() = upstream.state.events.lock().unwrap().clone();
    moved.state.emit("comment.created", json!({"old": true}));
    fixture
        .gateway
        .patch(
            "art",
            IntegrationPatch {
                url: Some(moved.url.clone()),
                ..IntegrationPatch::default()
            },
        )
        .await
        .unwrap();
    let feed = feeds(&fixture).await.remove(0);
    assert_eq!(feed.cursor, None, "the old server's cursor is dropped");
    assert_eq!(feed.gaps, 1);
    assert!(fixture.refresh("art").await.ok);
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    assert_eq!(feeds(&fixture).await[0].cursor.as_deref(), Some("1"));
    moved.state.emit("comment.created", json!({"n": 2}));
    due_now(&fixture).await;
    poll_feeds(&fixture.gateway, &fixture.events).await.unwrap();
    let sent = delivered(&fixture).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].1, json!({"n": 2}));
    fixture.stop().await;
}
