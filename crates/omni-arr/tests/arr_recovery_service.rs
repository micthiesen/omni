//! The Arr recovery service with an in-memory Arr client, a scripted assessor
//! and a recording notifier over a file-backed store. A `TestClock` shared
//! with the store is set explicitly, and the import settle delay is shortened
//! to 10 ms.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_alerts::PushoverError;
use omni_arr::arr_recovery::persistence::{
    ActionPhase, NotificationState, Observation, RecoveryAction, RecoveryState, acquire_state,
    release_state, save_state,
};
use omni_arr::arr_recovery::policy::observation_fingerprint;
use omni_arr::arr_recovery::service::{
    Assessor, Notifier, RecoveryContext, STUCK_GRACE_MS, can_replace, run_recovery,
};
use omni_arr::arr_recovery::{
    ArrCause, ArrClient, ArrKind, ArrRecoveryError, ArrResult, CommandStatus, Decision,
    DecisionSource, Evidence, Grab, ImportFile, QueueItem, StatusMessage, Target,
};
use omni_core::clock::{SharedClock, TestClock};
use omni_store::EntityOps as _;
use omni_testkit::TestStore;

const NOW: i64 = 1_800_000_000_000;

fn grab() -> Grab {
    Grab {
        download_id: "download-1".into(),
        source_title: "Example.Movie.2026".into(),
        series_id: None,
        movie_id: Some(10),
        episode_id: None,
        event_type: "grabbed".into(),
        date: "2026-09-12T00:00:00Z".into(),
    }
}

fn stuck_item() -> QueueItem {
    QueueItem {
        id: 1,
        download_id: "download-1".into(),
        title: "Example.Movie.2026".into(),
        status: "completed".into(),
        tracked_download_status: "warning".into(),
        tracked_download_state: "importBlocked".into(),
        status_messages: vec![StatusMessage {
            title: "Import failed".into(),
            messages: vec!["Could not find a matching movie".into()],
        }],
        size: 1_000.0,
        sizeleft: 0.0,
        output_path: Some("/downloads/example".into()),
        added: None,
        series_id: None,
        episode_id: None,
        movie_id: Some(10),
        protocol: None,
        download_client: None,
    }
}

struct MockState {
    queue: Vec<QueueItem>,
    target: Target,
    files: Vec<ImportFile>,
    grabs: Vec<Grab>,
    imported: bool,
    removed: bool,
    verify_imported: Option<bool>,
    calls: HashMap<&'static str, usize>,
    searched: Vec<Target>,
}

#[derive(Clone)]
struct MockClient(Arc<Mutex<MockState>>);

impl MockClient {
    fn new(queue: Vec<QueueItem>) -> Self {
        Self(Arc::new(Mutex::new(MockState {
            queue,
            target: common::movie_target(),
            files: vec![],
            grabs: vec![grab()],
            imported: false,
            removed: false,
            verify_imported: None,
            calls: HashMap::new(),
            searched: vec![],
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn call(&self, name: &'static str) -> std::sync::MutexGuard<'_, MockState> {
        let mut state = self.0.lock().unwrap();
        *state.calls.entry(name).or_default() += 1;
        state
    }

    fn calls(&self, name: &str) -> usize {
        self.with(|s| s.calls.get(name).copied().unwrap_or(0))
    }
}

impl ArrClient for MockClient {
    fn kind(&self) -> ArrKind {
        ArrKind::Radarr
    }
    async fn queue(&self) -> ArrResult<Vec<QueueItem>> {
        Ok(self.call("queue").queue.clone())
    }
    async fn preview(&self, _download_id: &str) -> ArrResult<Vec<ImportFile>> {
        Ok(self.call("preview").files.clone())
    }
    async fn target(&self, _items: &[QueueItem]) -> ArrResult<Target> {
        Ok(self.call("target").target.clone())
    }
    async fn history(&self, _download_id: &str) -> ArrResult<Vec<Grab>> {
        Ok(self.call("history").grabs.clone())
    }
    async fn import_files(&self, _download_id: &str, _files: &[ImportFile]) -> ArrResult<i64> {
        self.call("import_files").imported = true;
        Ok(101)
    }
    async fn command(&self, _id: i64) -> ArrResult<CommandStatus> {
        drop(self.call("command"));
        Ok(CommandStatus {
            status: "completed".into(),
            message: None,
        })
    }
    async fn remove(&self, id: i64, _blocklist: bool) -> ArrResult<()> {
        let mut state = self.call("remove");
        state.removed = true;
        state.queue.retain(|item| item.id != id);
        Ok(())
    }
    async fn verify_removed(&self, _output_path: &str) -> ArrResult<bool> {
        Ok(self.call("verify_removed").removed)
    }
    async fn search(&self, target: &Target) -> ArrResult<i64> {
        self.call("search").searched.push(target.clone());
        Ok(202)
    }
    async fn verify_imported(&self, _target: &Target, _files: &[ImportFile]) -> ArrResult<bool> {
        let state = self.call("verify_imported");
        Ok(state.verify_imported.unwrap_or(state.imported))
    }
}

type AssessFn = dyn Fn(&Evidence) -> Decision + Send + Sync;

struct ScriptedAssessor {
    decide: Box<AssessFn>,
    calls: Mutex<usize>,
}

impl ScriptedAssessor {
    fn fixed(decision: Decision) -> Self {
        Self::with(move |_| decision.clone())
    }
    fn with(f: impl Fn(&Evidence) -> Decision + Send + Sync + 'static) -> Self {
        Self {
            decide: Box::new(f),
            calls: Mutex::new(0),
        }
    }
}

impl Assessor for ScriptedAssessor {
    fn assess<'a>(&'a self, evidence: &'a Evidence) -> BoxFuture<'a, ArrResult<Decision>> {
        *self.calls.lock().unwrap() += 1;
        let decision = (self.decide)(evidence);
        Box::pin(async move { Ok(decision) })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    sent: Mutex<Vec<String>>,
    reject_with: Mutex<Option<u16>>,
}

impl Notifier for RecordingNotifier {
    fn send<'a>(&'a self, _kind: ArrKind, message: &'a str) -> BoxFuture<'a, ArrResult<()>> {
        let rejection = *self.reject_with.lock().unwrap();
        if rejection.is_none() {
            self.sent.lock().unwrap().push(message.to_owned());
        }
        Box::pin(async move {
            match rejection {
                Some(status) => Err(ArrRecoveryError::new(
                    "notify",
                    ArrCause::Pushover(PushoverError {
                        status: Some(status),
                        body: "rate limited".into(),
                    }),
                )),
                None => Ok(()),
            }
        })
    }
}

struct Harness {
    store: TestStore,
    clock: Arc<TestClock>,
    shared: SharedClock,
    notifier: RecordingNotifier,
}

impl Harness {
    async fn new() -> Self {
        let clock = TestClock::new(NOW);
        let shared: SharedClock = clock.clone();
        Self {
            store: TestStore::new(shared.clone()).await,
            clock,
            shared,
            notifier: RecordingNotifier::default(),
        }
    }

    fn advance(&self, ms: i64) {
        self.clock.set(self.shared.now_ms() + ms);
    }

    async fn run(&self, client: &MockClient, assessor: &ScriptedAssessor) -> ArrResult<String> {
        let cx = RecoveryContext {
            store: &self.store.store,
            clock: &self.shared,
            assessor,
            notifier: &self.notifier,
            health: None,
            import_settle_delay: Duration::from_millis(10),
        };
        run_recovery(std::slice::from_ref(client), &cx).await
    }

    async fn state(&self) -> RecoveryState {
        self.store
            .store
            .read(|docs| docs.get::<RecoveryState>(&"radarr".to_owned()))
            .await
            .unwrap()
            .unwrap()
    }

    async fn seed_mature(&self, item: &QueueItem, actions: Vec<RecoveryAction>) {
        let now = self.shared.now_ms();
        let owner = "test-seed";
        let mut stored = acquire_state(&self.store.store, ArrKind::Radarr, owner, now)
            .await
            .unwrap()
            .unwrap();
        stored.observations.insert(
            item.download_id.clone(),
            Observation {
                fingerprint: observation_fingerprint(std::slice::from_ref(item)),
                first_seen_at: now - STUCK_GRACE_MS,
                last_seen_at: now,
                observations: 2,
                last_assessed_at: None,
                reason: None,
            },
        );
        stored.actions = actions;
        save_state(&self.store.store, &stored, owner, now)
            .await
            .unwrap();
        release_state(&self.store.store, ArrKind::Radarr, owner, now)
            .await
            .unwrap();
    }
}

fn duplicate() -> Decision {
    Decision::Remove {
        reason: "duplicate".into(),
        source: DecisionSource::Llm,
        replace: false,
    }
}

#[tokio::test]
async fn retries_a_confirmed_pushover_rejection_without_repeating_the_action() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    h.seed_mature(&stuck_item(), vec![]).await;
    *h.notifier.reject_with.lock().unwrap() = Some(429);

    assert!(
        h.run(&client, &ScriptedAssessor::fixed(duplicate()))
            .await
            .is_err()
    );
    assert_eq!(
        h.state().await.actions[0].notification,
        NotificationState::Pending
    );

    *h.notifier.reject_with.lock().unwrap() = None;
    let unused = ScriptedAssessor::fixed(Decision::defer("unused", DecisionSource::Llm));
    h.run(&client, &unused).await.unwrap();
    assert_eq!(
        h.state().await.actions[0].notification,
        NotificationState::Sent
    );
    assert_eq!(client.calls("remove"), 1);
}

#[tokio::test]
async fn waits_for_a_second_unchanged_observation_spanning_15_minutes() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    let assessor = ScriptedAssessor::fixed(duplicate());

    h.run(&client, &assessor).await.unwrap();
    assert_eq!(client.calls("remove"), 0);
    assert!(h.state().await.actions.is_empty());

    h.advance(STUCK_GRACE_MS);
    h.run(&client, &assessor).await.unwrap();
    assert_eq!(client.calls("remove"), 1);
    assert_eq!(h.state().await.actions[0].phase, ActionPhase::Done);
}

#[tokio::test]
async fn restarts_the_grace_period_when_queue_progress_changes() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    let assessor = ScriptedAssessor::fixed(duplicate());

    h.run(&client, &assessor).await.unwrap();
    h.advance(STUCK_GRACE_MS);
    client.with(|s| {
        s.queue = vec![QueueItem {
            status_messages: vec![StatusMessage {
                title: "Import failed".into(),
                messages: vec!["A different import failure".into()],
            }],
            ..stuck_item()
        }];
    });
    h.run(&client, &assessor).await.unwrap();
    assert_eq!(client.calls("remove"), 0);

    h.advance(STUCK_GRACE_MS);
    h.run(&client, &assessor).await.unwrap();
    assert_eq!(client.calls("remove"), 1);
}

#[tokio::test]
async fn excludes_normal_active_downloads() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![QueueItem {
        status: "downloading".into(),
        tracked_download_status: "ok".into(),
        tracked_download_state: "downloading".into(),
        sizeleft: 500.0,
        status_messages: vec![],
        ..stuck_item()
    }]);
    let assessor = ScriptedAssessor::fixed(Decision::Remove {
        reason: "unused".into(),
        source: DecisionSource::Llm,
        replace: false,
    });

    h.run(&client, &assessor).await.unwrap();

    assert_eq!(client.calls("target"), 0);
    assert_eq!(client.calls("remove"), 0);
    assert!(h.state().await.observations.is_empty());
}

#[tokio::test]
async fn does_not_report_an_import_successful_until_its_files_verify() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    client.with(|s| {
        s.files = vec![common::movie_file()];
        s.verify_imported = Some(false);
    });
    h.seed_mature(&stuck_item(), vec![]).await;
    let assessor = ScriptedAssessor::fixed(Decision::Import {
        reason: "safe import".into(),
        source: DecisionSource::Llm,
    });

    h.run(&client, &assessor).await.unwrap();

    assert_eq!(client.calls("import_files"), 1);
    assert_eq!(h.state().await.actions[0].phase, ActionPhase::Uncertain);
    let sent = h.notifier.sent.lock().unwrap().clone();
    assert!(
        sent.iter().any(|m| m.contains("Needs inspection")),
        "{sent:?}"
    );
}

#[tokio::test]
async fn removes_a_duplicate_without_requesting_a_replacement_search() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    h.seed_mature(&stuck_item(), vec![]).await;

    h.run(&client, &ScriptedAssessor::fixed(duplicate()))
        .await
        .unwrap();

    assert_eq!(client.calls("remove"), 1);
    assert_eq!(client.calls("search"), 0);
    assert_eq!(h.state().await.actions[0].phase, ActionPhase::Done);
}

#[tokio::test]
async fn never_replays_an_import_whose_submission_outcome_is_unknown() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![]);
    client.with(|s| s.verify_imported = Some(false));
    let decision = Decision::Import {
        reason: "safe import".into(),
        source: DecisionSource::Llm,
    };
    let mut action = common::action(
        "download-1",
        common::movie_target(),
        decision.clone(),
        ActionPhase::Uncertain,
        NOW,
    );
    action.files = vec![common::movie_file()];
    action.output_path = "/downloads/example".into();
    action.error = Some("submission interrupted".into());
    h.seed_mature(&stuck_item(), vec![action]).await;

    let _ = h.run(&client, &ScriptedAssessor::fixed(decision)).await;

    assert_eq!(client.calls("import_files"), 0);
    assert_eq!(client.calls("command"), 0);
    assert_eq!(h.state().await.actions[0].phase, ActionPhase::Uncertain);
}

#[tokio::test]
async fn searches_once_after_confirming_a_replacement_removal() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    h.seed_mature(&stuck_item(), vec![]).await;
    let assessor =
        ScriptedAssessor::fixed(common::replacement("failed download", DecisionSource::Llm));

    h.run(&client, &assessor).await.unwrap();
    h.run(&client, &assessor).await.unwrap();

    assert_eq!(client.calls("remove"), 1);
    assert!(client.calls("verify_removed") > 0);
    assert_eq!(client.calls("search"), 1);
    assert_eq!(client.with(|s| s.searched[0].id), 10);
}

#[tokio::test]
async fn aborts_when_the_queue_changes_during_assessment() {
    let h = Harness::new().await;
    let client = MockClient::new(vec![stuck_item()]);
    h.seed_mature(&stuck_item(), vec![]).await;
    let mutable = client.clone();
    let assessor = ScriptedAssessor::with(move |_| {
        mutable.with(|s| {
            s.queue = vec![QueueItem {
                status_messages: vec![StatusMessage {
                    title: "Import failed".into(),
                    messages: vec!["Failure changed".into()],
                }],
                ..stuck_item()
            }];
        });
        duplicate()
    });

    h.run(&client, &assessor).await.unwrap();

    assert_eq!(client.calls("remove"), 0);
    assert_eq!(client.calls("import_files"), 0);
    assert!(h.state().await.actions.is_empty());
}

#[tokio::test]
async fn defers_a_replacement_while_its_search_backoff_is_active() {
    let h = Harness::new().await;
    let item = QueueItem {
        id: 2,
        download_id: "download-2".into(),
        ..stuck_item()
    };
    let client = MockClient::new(vec![item.clone()]);
    let mut prior = common::action(
        "download-1",
        common::movie_target(),
        common::replacement("failed download", DecisionSource::Rules),
        ActionPhase::Done,
        NOW - 1,
    );
    prior.title = "Earlier failed download".into();
    prior.output_path = "/downloads/earlier".into();
    h.seed_mature(&item, vec![prior]).await;

    h.run(
        &client,
        &ScriptedAssessor::fixed(common::replacement(
            "another failed download",
            DecisionSource::Llm,
        )),
    )
    .await
    .unwrap();

    assert_eq!(client.calls("remove"), 0);
    assert_eq!(client.calls("search"), 0);
    let stored = h.state().await;
    assert_eq!(stored.actions.len(), 1);
    assert_eq!(
        stored.observations["download-2"].reason.as_deref(),
        Some("Replacement search budget/backoff reached")
    );
}

#[test]
fn allows_another_search_after_backoff_but_enforces_the_three_attempt_budget() {
    let replacement = |created_at: i64, download_id: &str| {
        common::action(
            download_id,
            common::movie_target(),
            common::replacement("failed download", DecisionSource::Rules),
            ActionPhase::Done,
            created_at,
        )
    };
    let six_hours = 6 * 60 * 60_000;
    let target = common::movie_target();

    assert!(!can_replace(
        &[replacement(NOW - 1, "one")],
        &target,
        NOW,
        None
    ));
    assert!(can_replace(
        &[replacement(NOW - six_hours, "one")],
        &target,
        NOW,
        None
    ));
    assert!(!can_replace(
        &[
            replacement(NOW - six_hours, "one"),
            replacement(NOW - six_hours, "two"),
            replacement(NOW - six_hours, "three"),
        ],
        &target,
        NOW,
        None
    ));
}
