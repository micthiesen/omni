//! The `claude.session.turn_finished` watcher and session `notify` delivery.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::{clock, test_store};
use futures::future::BoxFuture;
use omni_config::{ClaudeNotifyTarget, ClaudeNotifyTargets};
use omni_core::clock::TestClock;
use omni_mcp::events::claude_notify::{
    ClaudeNotify, ClaudeTurnNotification, FinishedTurn, NotificationStatus, NotifyResponse,
    NotifyTransport, NotifyTransportError,
};
use omni_mcp::events::claude_sessions::{
    ClaudeSessionWatch, ClaudeSessionWatcher, StartedTurn, TurnBaseline, finished_turn,
    settled_by_tail, settled_from_status,
};
use omni_mcp::events::persistence::EventDelivery;
use omni_mcp::events::service::{McpEventService, SubscribeInput};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::host::{ClaudeHost, HostCommand, HostError, HostLinkStatus};
use omni_mcp::tools::claude_sessions::{ClaudeDeps, claude_session_tools};
use omni_mcp_kit::{McpTool, ToolOutput, registry::standalone_context};
use omni_runtime::ports::Ports;
use omni_store::Store;
use omni_store::entity::EntityOps;
use serde_json::{Map, Value, json};

const SESSION: &str = "25ffb449-1111-4222-8333-444455556666";

/// A host whose session list and per-session status the test controls.
#[derive(Default)]
struct HostState {
    sessions: Vec<Value>,
    /// Sessions only a direct status lookup finds, such as stopped ones.
    stopped: Vec<Value>,
    online: bool,
    calls: Vec<HostCommand>,
}

#[derive(Clone, Default)]
struct FakeHost(Arc<Mutex<HostState>>);

impl ClaudeHost for FakeHost {
    fn status(&self) -> HostLinkStatus {
        HostLinkStatus {
            online: self.0.lock().unwrap().online,
            disabled: false,
            host: None,
            last_seen_at: None,
            pending_jobs: 0,
        }
    }

    fn execute(
        &self,
        command: HostCommand,
        args: Map<String, Value>,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<Map<String, Value>, HostError>> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(command);
        let find = |state: &HostState, session: &Value| {
            state
                .sessions
                .iter()
                .chain(&state.stopped)
                .find(|row| row["session_id"] == *session || row["id"] == *session)
                .cloned()
        };
        let result = match command {
            HostCommand::List => Ok(json!({"sessions": state.sessions})),
            // The transcript's last item: `tail` names its kind (default an
            // assistant reply), `null` means an empty transcript.
            HostCommand::Read => find(&state, &args["session"])
                .map(|row| {
                    let items = match row.get("tail") {
                        Some(Value::Null) => json!([]),
                        tail => json!([{
                            "kind": tail.and_then(Value::as_str).unwrap_or("assistant"),
                            "timestamp": "2026-11-12T21:15:29.804Z",
                        }]),
                    };
                    json!({"session_id": row["session_id"], "revision": row["revision"], "items": items})
                })
                .ok_or_else(|| HostError::new("not_found", "gone", false)),
            HostCommand::Start => state
                .sessions
                .first()
                .cloned()
                .map(|row| with(row, json!({"reused": false})))
                .ok_or_else(|| HostError::new("bad_request", "no session", false)),
            HostCommand::Send => find(&state, &args["session"])
                .map(|row| {
                    let previous = row["revision"].clone();
                    with(row, json!({"previous_revision": previous, "reused": false}))
                })
                .ok_or_else(|| HostError::new("not_found", "gone", false)),
            _ => find(&state, &args["session"])
                .ok_or_else(|| HostError::new("not_found", "gone", false)),
        }
        .map(|value| value.as_object().cloned().unwrap());
        Box::pin(async move { result })
    }
}

fn with(base: Value, extra: Value) -> Value {
    let mut base = base;
    for (key, value) in extra.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

struct Accept;

impl WebhookPort for Accept {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        _: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        Box::pin(async { Ok(204) })
    }
}

fn summary(overrides: Value) -> Value {
    let mut base = json!({
        "id": "25ffb449",
        "session_id": SESSION,
        "project": "omni",
        "status": "idle",
        "state": "idle",
        "revision": 4,
    });
    for (key, value) in overrides.as_object().unwrap() {
        if value.is_null() {
            base.as_object_mut().unwrap().remove(key);
        } else {
            base[key] = value.clone();
        }
    }
    base
}

fn subscription(arguments: Value) -> SubscribeInput {
    SubscribeInput {
        name: "claude.session.turn_finished".to_owned(),
        arguments,
        url: "https://chatgpt.example.com/events/claude".to_owned(),
        secret: format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 32])
        ),
        ttl_ms: None,
    }
}

/// Records each notification and answers from a script (then 200).
#[derive(Clone, Default)]
struct Transport {
    sent: Arc<Mutex<Vec<(String, String, Value)>>>,
    answers: Arc<Mutex<VecDeque<Result<NotifyResponse, NotifyTransportError>>>>,
}

impl Transport {
    fn answer(&self, answer: Result<u16, NotifyTransportError>) {
        self.answers
            .lock()
            .unwrap()
            .push_back(answer.map(|status| NotifyResponse {
                status,
                ignored: false,
            }));
    }

    fn sent(&self) -> Vec<(String, String, Value)> {
        self.sent.lock().unwrap().clone()
    }
}

impl NotifyTransport for Transport {
    fn post<'a>(
        &'a self,
        target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<NotifyResponse, NotifyTransportError>> {
        self.sent.lock().unwrap().push((
            target.url.to_string(),
            idempotency_key.to_owned(),
            serde_json::from_str(body).unwrap(),
        ));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(NotifyResponse {
                status: 200,
                ignored: false,
            }));
        Box::pin(async move { answer })
    }
}

const CHIEF_URL: &str = "https://rakazo.example/api/v1/bots/chief-bot/webhook";

fn targets() -> ClaudeNotifyTargets {
    ClaudeNotifyTargets(BTreeMap::from([(
        "chief".to_owned(),
        ClaudeNotifyTarget {
            url: url::Url::parse(CHIEF_URL).unwrap(),
            token: "chief-webhook-key-0123456789".to_owned(),
        },
    )]))
}

struct Setup {
    store: Store,
    host: FakeHost,
    watcher: ClaudeSessionWatcher,
    events: McpEventService,
    arguments: Value,
    clock: Arc<TestClock>,
    notify: ClaudeNotify,
    transport: Transport,
    tools: HashMap<String, McpTool>,
    _db: omni_testkit::TestStore,
}

impl Setup {
    async fn new(arguments: Value) -> Self {
        let clock = clock();
        let db = test_store(&clock).await;
        let events = common::with_active_owners(McpEventService::new(
            "test-omni-bearer",
            db.store.clone(),
            clock.clone(),
            Arc::new(Accept),
            Ports::default(),
        ));
        let host = FakeHost::default();
        host.0.lock().unwrap().online = true;
        let transport = Transport::default();
        let notify = ClaudeNotify::new(
            db.store.clone(),
            clock.clone(),
            Some(targets()),
            Arc::new(transport.clone()),
        );
        let watcher = ClaudeSessionWatcher::new(
            events.clone(),
            Arc::new(host.clone()),
            db.store.clone(),
            clock.clone(),
        )
        .with_notify(notify.clone());
        let tools = claude_session_tools(&ClaudeDeps {
            host: Some(Arc::new(host.clone())),
            watcher: Some(Arc::new(watcher.clone())),
            notify: Some(notify.clone()),
        })
        .unwrap()
        .into_iter()
        .map(|tool| (tool.meta.name.clone(), tool))
        .collect();
        Self {
            store: db.store.clone(),
            host,
            watcher,
            events,
            arguments,
            clock,
            notify,
            transport,
            tools,
            _db: db,
        }
    }

    async fn call(&self, name: &str, input: Value) -> Value {
        match self.tools[name]
            .handler
            .call(input, standalone_context("test"))
            .await
            .unwrap()
        {
            ToolOutput::Structured(map) => Value::Object(map),
            ToolOutput::Custom { structured, .. } => Value::Object(structured),
        }
    }

    async fn notifications(&self) -> Vec<ClaudeTurnNotification> {
        self.store
            .read(|docs| docs.get_all::<ClaudeTurnNotification>())
            .await
            .unwrap()
    }

    fn advance(&self, ms: i64) {
        use omni_core::clock::Clock as _;
        self.clock.set(self.clock.now_ms() + ms);
    }

    fn host_calls(&self) -> Vec<HostCommand> {
        self.host.0.lock().unwrap().calls.clone()
    }

    async fn subscribe(&self) {
        self.events
            .subscribe_as(&common::owner(), &subscription(self.arguments.clone()))
            .await
            .unwrap();
    }

    fn set_sessions(&self, sessions: Vec<Value>) {
        self.host.0.lock().unwrap().sessions = sessions;
    }

    async fn deliveries(&self) -> Vec<EventDelivery> {
        self.store
            .read(|docs| docs.get_all::<EventDelivery>())
            .await
            .unwrap()
    }
}

fn baseline(revision: f64, settled: bool, started: bool) -> Option<TurnBaseline> {
    Some(TurnBaseline {
        revision,
        settled,
        started,
        quiet: false,
    })
}

#[test]
fn decides_from_the_runtime_status_and_never_the_model_written_state() {
    let status = |previous, status, revision| settled_from_status(previous, status, revision);
    assert_eq!(status(baseline(3.0, true, false), "busy", 5.0), Some(false));
    assert_eq!(
        status(baseline(3.0, true, false), "running", 5.0),
        Some(false)
    );
    // First sight is a baseline; an unchanged settled session needs no read.
    assert_eq!(status(None, "idle", 5.0), Some(true));
    assert_eq!(status(baseline(5.0, true, false), "idle", 5.0), Some(true));
    // Omni's own turn is pending until the transcript moves.
    assert_eq!(status(baseline(5.0, false, true), "idle", 5.0), Some(false));
    // Anything else is decided by the transcript's last item.
    assert_eq!(status(baseline(3.0, false, false), "idle", 5.0), None);
    assert_eq!(status(baseline(5.0, false, true), "idle", 6.0), None);

    let fresh = baseline(3.0, false, false);
    assert!(settled_by_tail(fresh, 5.0, Some("assistant")));
    assert!(settled_by_tail(fresh, 5.0, Some("tool_use")));
    assert!(!settled_by_tail(fresh, 5.0, Some("user")));
    assert!(!settled_by_tail(fresh, 5.0, None));
    // A prompt still last when the session was already quiet at that revision.
    let quiet = Some(TurnBaseline {
        revision: 5.0,
        settled: false,
        started: false,
        quiet: true,
    });
    assert!(settled_by_tail(quiet, 5.0, Some("user")));
    assert!(!settled_by_tail(quiet, 6.0, Some("user")));
}

#[test]
fn needs_a_baseline_and_a_settled_session_that_moved_on() {
    assert!(!finished_turn(None, true, 5.0));
    assert!(finished_turn(baseline(3.0, false, false), true, 5.0));
    assert!(finished_turn(baseline(3.0, true, false), true, 5.0));
    assert!(finished_turn(baseline(5.0, false, false), true, 5.0));
    assert!(!finished_turn(baseline(5.0, true, false), true, 5.0));
    assert!(!finished_turn(baseline(3.0, false, false), false, 5.0));
    assert!(!finished_turn(baseline(5.0, false, true), true, 5.0));
    assert!(finished_turn(baseline(5.0, false, true), true, 6.0));
}

#[tokio::test(start_paused = true)]
async fn stays_idle_without_a_subscription_or_while_the_host_is_offline() {
    let setup = Setup::new(json!({})).await;
    setup.watcher.poll().await.unwrap();
    assert!(setup.host.0.lock().unwrap().calls.is_empty());
    setup.subscribe().await;
    setup.host.0.lock().unwrap().online = false;
    setup.watcher.poll().await.unwrap();
    assert!(setup.host.0.lock().unwrap().calls.is_empty());
}

#[tokio::test(start_paused = true)]
async fn publishes_each_finished_turn_once_after_a_baseline() {
    let setup = Setup::new(json!({})).await;
    setup.subscribe().await;
    setup.set_sessions(vec![summary(json!({"revision": 2}))]);
    setup.watcher.poll().await.unwrap();
    assert!(setup.deliveries().await.is_empty());
    setup.set_sessions(vec![summary(
        json!({"status": "busy", "state": "working", "revision": 3}),
    )]);
    setup.watcher.poll().await.unwrap();
    setup.set_sessions(vec![summary(json!({"revision": 6}))]);
    setup.watcher.poll().await.unwrap();
    setup.watcher.poll().await.unwrap();
    let deliveries = setup.deliveries().await;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].name, "claude.session.turn_finished");
    assert_eq!(
        Value::Object(deliveries[0].data_json()),
        json!({"sessionId": SESSION, "id": "25ffb449", "project": "omni", "status": "idle", "revision": 6})
    );
}

#[tokio::test(start_paused = true)]
async fn catches_a_turn_omni_started_that_ended_before_the_next_poll() {
    let setup = Setup::new(json!({"project": "omni"})).await;
    setup.subscribe().await;
    setup
        .watcher
        .note_turn_started(StartedTurn {
            session_id: SESSION.to_owned(),
            id: Some("25ffb449".to_owned()),
            project: None,
            revision: 4.0,
        })
        .await
        .unwrap();
    setup.set_sessions(vec![summary(json!({"revision": 7}))]);
    setup.watcher.poll().await.unwrap();
    assert_eq!(setup.deliveries().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn reads_a_mid_turn_session_that_stopped_and_left_the_running_list() {
    let setup = Setup::new(json!({})).await;
    setup.subscribe().await;
    setup.set_sessions(vec![summary(
        json!({"status": "busy", "state": "working", "revision": 3}),
    )]);
    setup.watcher.poll().await.unwrap();
    setup.set_sessions(Vec::new());
    setup.host.0.lock().unwrap().stopped = vec![summary(
        json!({"status": "stopped", "project": null, "revision": 5}),
    )];
    setup.watcher.poll().await.unwrap();
    assert!(
        setup
            .host
            .0
            .lock()
            .unwrap()
            .calls
            .contains(&HostCommand::Status)
    );
    let deliveries = setup.deliveries().await;
    assert_eq!(deliveries.len(), 1);
    let data = Value::Object(deliveries[0].data_json());
    assert_eq!(data["status"], "stopped");
    assert_eq!(data["project"], "omni");
    assert_eq!(data["revision"], 5);
}

#[tokio::test(start_paused = true)]
async fn ignores_turns_from_a_different_project() {
    let setup = Setup::new(json!({"project": "dotfiles"})).await;
    setup.subscribe().await;
    setup.set_sessions(vec![summary(
        json!({"status": "busy", "state": "working", "revision": 3}),
    )]);
    setup.watcher.poll().await.unwrap();
    setup.set_sessions(vec![summary(json!({"revision": 6}))]);
    setup.watcher.poll().await.unwrap();
    assert!(setup.deliveries().await.is_empty());
}

/// Two sessions in `omni` each finish a turn; returns the delivered session IDs.
async fn delivered_sessions(arguments: Value) -> Vec<String> {
    const OTHER: &str = "a47778af-36a9-4c0b-a5fb-0b87fcb8e4b0";
    let other = |overrides: Value| {
        let mut row = summary(overrides);
        row["id"] = json!("a47778af");
        row["session_id"] = json!(OTHER);
        row
    };
    let setup = Setup::new(arguments).await;
    setup.subscribe().await;
    let working = json!({"status": "busy", "state": "working", "revision": 3});
    setup.set_sessions(vec![summary(working.clone()), other(working)]);
    setup.watcher.poll().await.unwrap();
    let finished = json!({"revision": 6});
    setup.set_sessions(vec![summary(finished.clone()), other(finished)]);
    setup.watcher.poll().await.unwrap();
    setup
        .deliveries()
        .await
        .iter()
        .map(|delivery| {
            delivery.data_json()["sessionId"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn delivers_only_the_exact_session_when_one_is_named() {
    assert_eq!(delivered_sessions(json!({})).await.len(), 2);
    assert_eq!(
        delivered_sessions(json!({"exactSessionId": SESSION})).await,
        [SESSION]
    );
    assert_eq!(
        delivered_sessions(json!({"project": "omni", "exactSessionId": SESSION})).await,
        [SESSION]
    );
    assert!(
        delivered_sessions(json!({"project": "dotfiles", "exactSessionId": SESSION}))
            .await
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn rejects_a_short_or_partial_session_id() {
    let setup = Setup::new(json!({})).await;
    for session in [
        "25ffb449",
        "25ffb449-1111-4222-8333",
        "25FFB449-1111-4222-8333-444455556666",
    ] {
        let rejected = setup
            .events
            .subscribe_as(
                &common::owner(),
                &subscription(json!({"exactSessionId": session})),
            )
            .await;
        assert_eq!(
            rejected.unwrap_err().to_string(),
            "Event subscription rejected: invalid_arguments"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn waits_for_a_new_revision_after_a_turn_omni_started() {
    let setup = Setup::new(json!({})).await;
    setup.subscribe().await;
    setup
        .watcher
        .note_turn_started(StartedTurn {
            session_id: SESSION.to_owned(),
            id: Some("25ffb449".to_owned()),
            project: None,
            revision: 4.0,
        })
        .await
        .unwrap();
    // A resumed session can report idle before its turn begins.
    setup.set_sessions(vec![summary(json!({"revision": 4}))]);
    setup.watcher.poll().await.unwrap();
    assert!(setup.deliveries().await.is_empty());
    setup.set_sessions(vec![summary(json!({"revision": 6}))]);
    setup.watcher.poll().await.unwrap();
    assert_eq!(setup.deliveries().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn forgets_sessions_the_host_cannot_read() {
    let setup = Setup::new(json!({})).await;
    setup.subscribe().await;
    setup
        .watcher
        .note_turn_started(StartedTurn {
            session_id: "gone-session".to_owned(),
            id: None,
            project: None,
            revision: 1.0,
        })
        .await
        .unwrap();
    setup.watcher.poll().await.unwrap();
    let watches = setup
        .store
        .read(|docs| docs.get_all::<ClaudeSessionWatch>())
        .await
        .unwrap();
    assert!(watches.is_empty());
}

/// A session the tests start through `claude_session_start`.
fn launched(overrides: Value) -> Value {
    with(
        summary(json!({"title": "Map the Omni project", "revision": 0, "state": "working"})),
        overrides,
    )
}

async fn start(setup: &Setup, notify: Option<&str>) -> Value {
    setup.set_sessions(vec![launched(json!({"status": "running"}))]);
    let mut input =
        json!({"project": "omni", "prompt": "Map the project", "idempotencyKey": "chief:1"});
    if let Some(notify) = notify {
        input["notify"] = json!(notify);
    }
    setup.call("claude_session_start", input).await
}

/// One poll after the host reports `overrides` for the session.
async fn poll_with(setup: &Setup, overrides: Value) {
    setup.set_sessions(vec![launched(overrides)]);
    setup.watcher.poll().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn without_notify_nothing_is_posted_and_the_host_is_not_polled() {
    let setup = Setup::new(json!({})).await;
    let started = start(&setup, None).await;
    assert_eq!(started["notify"], Value::Null);
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 6}),
    )
    .await;
    assert!(setup.transport.sent().is_empty());
    assert!(setup.notifications().await.is_empty());
    // No subscription and no notify target: the watcher never asks the host.
    assert_eq!(setup.host_calls(), [HostCommand::Start]);
}

#[tokio::test(start_paused = true)]
async fn notifies_each_finished_turn_once_across_sends() {
    let setup = Setup::new(json!({})).await;
    let started = start(&setup, Some("chief")).await;
    assert_eq!(
        started["notify"],
        json!({"target": "chief", "configured": true})
    );

    // The launch reports idle before the turn starts; that is not a turn.
    poll_with(
        &setup,
        json!({"status": "idle", "revision": 0, "tail": null}),
    )
    .await;
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    assert!(setup.transport.sent().is_empty());
    let reply = "x".repeat(2_000);
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 6, "last_assistant": reply}),
    )
    .await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 6}),
    )
    .await;
    let sent = setup.transport.sent();
    assert_eq!(sent.len(), 1);
    let (url, key, body) = &sent[0];
    assert_eq!(url, CHIEF_URL);
    assert_eq!(key, &format!("omni-claude-turn:{SESSION}:6"));
    assert_eq!(body["event"], "claude.session.turn_finished");
    assert_eq!(body["sessionId"], SESSION);
    assert_eq!(body["shortId"], "25ffb449");
    assert_eq!(body["project"], "omni");
    assert_eq!(body["title"], "Map the Omni project");
    assert_eq!(body["status"], "idle");
    assert_eq!(body["state"], "done");
    assert_eq!(body["revision"], 6);
    assert_eq!(body["finishedAt"], "2026-11-12T21:15:29.804Z");
    assert_eq!(body["summary"].as_str().unwrap().chars().count(), 1_500);
    assert_eq!(body["summaryTruncated"], true);

    // A second turn through claude_session_send keeps the target.
    let sent_turn = setup
        .call(
            "claude_session_send",
            json!({"session": "25ffb449", "prompt": "More"}),
        )
        .await;
    assert_eq!(
        sent_turn["notify"],
        json!({"target": "chief", "configured": true})
    );
    poll_with(
        &setup,
        json!({"status": "busy", "state": "done", "revision": 7}),
    )
    .await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 9}),
    )
    .await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 9}),
    )
    .await;
    let revisions: Vec<Value> = setup
        .transport
        .sent()
        .iter()
        .map(|(_, _, body)| body["revision"].clone())
        .collect();
    assert_eq!(revisions, [json!(6), json!(9)]);

    // null clears it: the third turn is not posted.
    let cleared = setup
        .call(
            "claude_session_send",
            json!({"session": "25ffb449", "prompt": "Last", "notify": null}),
        )
        .await;
    assert_eq!(cleared["notify"], Value::Null);
    poll_with(&setup, json!({"status": "busy", "revision": 10})).await;
    poll_with(&setup, json!({"status": "idle", "revision": 12})).await;
    assert_eq!(setup.transport.sent().len(), 2);
    assert!(
        setup
            .notifications()
            .await
            .iter()
            .all(|row| row.status == NotificationStatus::Delivered && row.body.is_none())
    );
}

/// Session 80f00d5b: its second turn ended (host idle, a reply last) while
/// the model-written state stayed "working"; before the turn, the state
/// still said "done" from the first turn while the new prompt was last.
#[tokio::test(start_paused = true)]
async fn uses_the_runtime_status_when_the_written_state_is_stale() {
    let setup = Setup::new(json!({})).await;
    start(&setup, Some("chief")).await;
    poll_with(&setup, json!({"status": "busy", "revision": 20})).await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 23}),
    )
    .await;
    assert_eq!(setup.transport.sent().len(), 1);

    setup
        .call(
            "claude_session_send",
            json!({"session": "25ffb449", "prompt": "Resend"}),
        )
        .await;
    // The prompt is in the transcript but the turn has not begun.
    poll_with(
        &setup,
        json!({"status": "idle", "state": "done", "revision": 24, "tail": "user"}),
    )
    .await;
    assert_eq!(setup.transport.sent().len(), 1);
    poll_with(
        &setup,
        json!({"status": "busy", "state": "done", "revision": 24}),
    )
    .await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "working", "revision": 25}),
    )
    .await;
    poll_with(
        &setup,
        json!({"status": "idle", "state": "working", "revision": 25}),
    )
    .await;
    let sent = setup.transport.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].2["revision"], 25);
    assert_eq!(sent[1].2["state"], "working");
}

#[tokio::test(start_paused = true)]
async fn records_each_session_revision_once() {
    let setup = Setup::new(json!({})).await;
    setup
        .notify
        .set_target(SESSION, Some("chief"))
        .await
        .unwrap();
    let turn = FinishedTurn {
        session_id: SESSION.to_owned(),
        short_id: Some("25ffb449".to_owned()),
        project: Some("omni".to_owned()),
        title: None,
        status: "idle".to_owned(),
        state: Some("done".to_owned()),
        revision: 6.0,
        finished_at: common::EPOCH_MS,
        summary: Some("Done.".to_owned()),
    };
    assert!(setup.notify.turn_finished(&turn).await.unwrap());
    assert!(!setup.notify.turn_finished(&turn).await.unwrap());
    assert_eq!(setup.transport.sent().len(), 1);
    assert_eq!(setup.notifications().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unconfigured_target_never_fails_the_session() {
    let setup = Setup::new(json!({})).await;
    let started = start(&setup, Some("nobody")).await;
    assert_eq!(started["session"]["sessionId"], SESSION);
    assert_eq!(
        started["notify"],
        json!({"target": "nobody", "configured": false})
    );
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    poll_with(&setup, json!({"status": "idle", "revision": 6})).await;
    assert!(setup.transport.sent().is_empty());
    let rows = setup.notifications().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, NotificationStatus::Unconfigured);
    assert_eq!(rows[0].body, None);
}

#[tokio::test(start_paused = true)]
async fn retries_failed_deliveries_with_backoff_and_reports_them() {
    let setup = Setup::new(json!({})).await;
    start(&setup, Some("chief")).await;
    setup.transport.answer(Ok(503));
    setup.transport.answer(Err(NotifyTransportError::Timeout));
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    poll_with(&setup, json!({"status": "idle", "revision": 6})).await;
    assert_eq!(setup.transport.sent().len(), 1);
    let row = &setup.notifications().await[0];
    assert_eq!(row.status, NotificationStatus::Pending);
    assert_eq!(row.last_status, Some(503));

    // Not due before the 30 s backoff, even with the host offline.
    setup.host.0.lock().unwrap().online = false;
    setup.advance(10_000);
    setup.watcher.poll().await.unwrap();
    assert_eq!(setup.transport.sent().len(), 1);
    setup.advance(20_000);
    setup.watcher.poll().await.unwrap();
    assert_eq!(setup.transport.sent().len(), 2);
    assert_eq!(
        setup.notifications().await[0].last_error.as_deref(),
        Some("timeout")
    );
    setup.advance(60_000);
    setup.watcher.poll().await.unwrap();
    let sent = setup.transport.sent();
    assert_eq!(sent.len(), 3);
    // Every attempt carries the same idempotency key.
    assert!(sent.iter().all(|(_, key, _)| key == &sent[0].1));
    let row = &setup.notifications().await[0];
    assert_eq!(row.status, NotificationStatus::Delivered);
    assert_eq!(row.attempts, 3);

    let status = omni_mcp::tools::events::events_status(Some(&setup.events), Some(&setup.notify))
        .await
        .unwrap();
    let notifications = &status["claudeNotifications"];
    assert_eq!(notifications["targets"], json!(["chief"]));
    assert_eq!(notifications["delivered"], 1);
    assert_eq!(notifications["recent"][0]["status"], "delivered");
    assert_eq!(notifications["recent"][0]["attempts"], 3);
    assert!(!status.to_string().contains("chief-webhook-key"));
    assert!(!status.to_string().contains("chief-bot"));
}

#[tokio::test(start_paused = true)]
async fn gives_up_on_a_rejection_or_after_the_last_attempt() {
    let setup = Setup::new(json!({})).await;
    start(&setup, Some("chief")).await;
    setup.transport.answer(Ok(401));
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    poll_with(&setup, json!({"status": "idle", "revision": 6})).await;
    let row = &setup.notifications().await[0];
    assert_eq!(row.status, NotificationStatus::Failed);
    assert_eq!(row.last_error.as_deref(), Some("http_401"));
    assert_eq!(row.body, None);

    for _ in 0..8 {
        setup.transport.answer(Ok(500));
    }
    poll_with(&setup, json!({"status": "busy", "revision": 7})).await;
    poll_with(&setup, json!({"status": "idle", "revision": 9})).await;
    for _ in 0..10 {
        setup.advance(31 * 60_000);
        setup.watcher.poll().await.unwrap();
    }
    let rows = setup.notifications().await;
    let last = rows.iter().find(|row| row.revision == 9.0).unwrap();
    assert_eq!(last.status, NotificationStatus::Failed);
    assert_eq!(last.attempts, 8);
    assert_eq!(last.last_error.as_deref(), Some("http_500"));
    assert_eq!(setup.transport.sent().len(), 9);
}

#[tokio::test(start_paused = true)]
async fn rejects_a_malformed_notify_name_before_anything_runs() {
    let setup = Setup::new(json!({})).await;
    setup.set_sessions(vec![launched(json!({}))]);
    let result = setup.tools["claude_session_start"]
        .handler
        .call(
            json!({"project": "omni", "prompt": "Go", "idempotencyKey": "k", "notify": "Chief Bot"}),
            standalone_context("test"),
        )
        .await;
    assert!(result.is_err());
    let empty = setup.tools["claude_session_send"]
        .handler
        .call(
            json!({"session": "25ffb449", "prompt": "Go", "notify": ""}),
            standalone_context("test"),
        )
        .await;
    assert!(empty.is_err());
    assert!(setup.host_calls().is_empty());
}

/// An interrupted turn leaves its prompt last and the session idle: it ends
/// once, on the second quiet poll, without a reply to summarize.
#[tokio::test(start_paused = true)]
async fn an_interrupted_turn_settles_once_the_prompt_stays_last() {
    let setup = Setup::new(json!({})).await;
    start(&setup, Some("chief")).await;
    poll_with(&setup, json!({"status": "busy", "revision": 3})).await;
    let interrupted = json!({"status": "idle", "revision": 4, "tail": "user"});
    poll_with(&setup, interrupted.clone()).await;
    assert!(setup.transport.sent().is_empty());
    poll_with(&setup, interrupted.clone()).await;
    poll_with(&setup, interrupted).await;
    let sent = setup.transport.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].2["revision"], 4);
    assert_eq!(sent[0].2["summary"], Value::Null);
}
