//! The `claude.session.turn_finished` watcher.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::{clock, test_store};
use futures::future::BoxFuture;
use omni_mcp::events::claude_sessions::{
    ClaudeSessionWatch, ClaudeSessionWatcher, StartedTurn, TurnBaseline, finished_turn, is_settled,
};
use omni_mcp::events::persistence::EventDelivery;
use omni_mcp::events::service::{McpEventService, SubscribeInput};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::host::{ClaudeHost, HostCommand, HostError, HostLinkStatus};
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
        let result = if command == HostCommand::List {
            Ok(json!({"sessions": state.sessions})
                .as_object()
                .cloned()
                .unwrap())
        } else {
            state
                .sessions
                .iter()
                .chain(&state.stopped)
                .find(|row| row["session_id"] == args["session"])
                .and_then(|row| row.as_object().cloned())
                .ok_or_else(|| HostError::new("not_found", "gone", false))
        };
        Box::pin(async move { result })
    }
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

struct Setup {
    store: Store,
    host: FakeHost,
    watcher: ClaudeSessionWatcher,
    events: McpEventService,
    project: Option<&'static str>,
    _db: omni_testkit::TestStore,
}

impl Setup {
    async fn new(project: Option<&'static str>) -> Self {
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
        let watcher = ClaudeSessionWatcher::new(
            events.clone(),
            Arc::new(host.clone()),
            db.store.clone(),
            clock,
        );
        Self {
            store: db.store.clone(),
            host,
            watcher,
            events,
            project,
            _db: db,
        }
    }

    async fn subscribe(&self) {
        let secret = format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 32])
        );
        self.events
            .subscribe_as(
                &common::owner(),
                &SubscribeInput {
                    name: "claude.session.turn_finished".to_owned(),
                    arguments: match self.project {
                        Some(project) => json!({ "project": project }),
                        None => json!({}),
                    },
                    url: "https://chatgpt.example.com/events/claude".to_owned(),
                    secret,
                    ttl_ms: None,
                },
            )
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

#[test]
fn treats_busy_or_working_sessions_as_unsettled() {
    assert!(!is_settled("busy", Some("working")));
    assert!(!is_settled("idle", Some("working")));
    assert!(is_settled("idle", Some("idle")));
    assert!(is_settled("stopped", Some("working")));
}

#[test]
fn needs_a_baseline_and_a_settled_session_that_moved_on() {
    let previous = |revision: f64, settled: bool| {
        Some(TurnBaseline {
            revision,
            settled,
            started: false,
        })
    };
    assert!(!finished_turn(None, "idle", Some("idle"), 5.0));
    assert!(finished_turn(
        previous(3.0, false),
        "idle",
        Some("idle"),
        5.0
    ));
    assert!(finished_turn(
        previous(3.0, true),
        "idle",
        Some("idle"),
        5.0
    ));
    assert!(!finished_turn(
        previous(5.0, true),
        "idle",
        Some("idle"),
        5.0
    ));
    assert!(!finished_turn(
        previous(3.0, false),
        "busy",
        Some("idle"),
        5.0
    ));
}

#[tokio::test(start_paused = true)]
async fn stays_idle_without_a_subscription_or_while_the_host_is_offline() {
    let setup = Setup::new(None).await;
    setup.watcher.poll().await.unwrap();
    assert!(setup.host.0.lock().unwrap().calls.is_empty());
    setup.subscribe().await;
    setup.host.0.lock().unwrap().online = false;
    setup.watcher.poll().await.unwrap();
    assert!(setup.host.0.lock().unwrap().calls.is_empty());
}

#[tokio::test(start_paused = true)]
async fn publishes_each_finished_turn_once_after_a_baseline() {
    let setup = Setup::new(None).await;
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
    let setup = Setup::new(Some("omni")).await;
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
    let setup = Setup::new(None).await;
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
    let setup = Setup::new(Some("dotfiles")).await;
    setup.subscribe().await;
    setup.set_sessions(vec![summary(
        json!({"status": "busy", "state": "working", "revision": 3}),
    )]);
    setup.watcher.poll().await.unwrap();
    setup.set_sessions(vec![summary(json!({"revision": 6}))]);
    setup.watcher.poll().await.unwrap();
    assert!(setup.deliveries().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn waits_for_a_new_revision_after_a_turn_omni_started() {
    let setup = Setup::new(None).await;
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
    let setup = Setup::new(None).await;
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
