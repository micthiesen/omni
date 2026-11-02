//! The podcast recommendation pipeline, with end-to-end runs using
//! scripted models and fakes that pin the commit sequence
//! (pending row → Castro enqueue → `queueResult` → notify → `notified`) and
//! the three-state subscription rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{DAY, FakeAccount, FakeAccounts, FakeDirectory, NOW, rec};
use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_ai::tools::{SearchOptions, WebSearchResults};
use omni_ai::{GenerateResponse, ModelRole};
use omni_alerts::PushoverMessage;
use omni_config::Config;
use omni_podcasts::account::{PodcastQueuePosition, PodcastWriteResult, Unavailable};
use omni_podcasts::models::Models;
use omni_podcasts::persistence::{
    PodcastQueueResult, PodcastRecommendationData, PodcastRecommendationStatus,
    get_all_podcast_recommendations, insert_podcast_recommendation,
};
use omni_podcasts::pipeline::{
    Notifier, PipelineError, PodcastDeps, PodcastPipelineOptions, enqueue_and_persist,
    listen_history_since, run_podcast_pipeline, to_queue_result,
};
use omni_podcasts::podcastindex::{PersonSearch, PodcastIndexEpisode, PodcastIndexRequestError};
use omni_podcasts::sources::WebSearcher;
use omni_podcasts::taste::{TasteSeedFailure, load_taste_seed};
use omni_runtime::chat_record::{ChatRecord, RecommendationOrigin};
use omni_testkit::{TEST_EPOCH_MS, TestApp, test_app_env};

#[tokio::test]
async fn maps_a_malformed_seed_to_a_recoverable_pipeline_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("taste.md");
    std::fs::write(&path, " \n ").unwrap();
    let error: PipelineError = load_taste_seed(path.to_str()).await.unwrap_err().into();
    assert!(
        matches!(&error, PipelineError::TasteSeed(cause) if cause.reason == TasteSeedFailure::Malformed),
        "{error:?}"
    );
}

#[tokio::test]
async fn persists_the_castro_result_before_returning_to_notification() {
    let events = Arc::new(Mutex::new(vec!["enqueue".to_owned()]));
    let recorded = events.clone();
    let result = enqueue_and_persist(Some(PodcastWriteResult::Added), |queue| async move {
        recorded.lock().unwrap().push(format!("persist:{queue:?}"));
        Ok(())
    })
    .await
    .unwrap();
    events.lock().unwrap().push("notify-boundary".into());
    assert_eq!(result, PodcastQueueResult::Queued);
    assert_eq!(
        *events.lock().unwrap(),
        vec!["enqueue", "persist:Queued", "notify-boundary"]
    );
}

#[test]
fn maps_added_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::Added),
        PodcastQueueResult::Queued
    );
}

#[test]
fn maps_already_exists_already_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::AlreadyExists),
        PodcastQueueResult::AlreadyQueued
    );
}

#[test]
fn maps_not_found_not_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::NotFound),
        PodcastQueueResult::NotQueued
    );
}

#[test]
fn maps_unavailable_not_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::Unavailable),
        PodcastQueueResult::NotQueued
    );
}

#[test]
fn maps_error_not_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::Error),
        PodcastQueueResult::NotQueued
    );
}

#[test]
fn maps_removed_not_queued() {
    assert_eq!(
        to_queue_result(PodcastWriteResult::Removed),
        PodcastQueueResult::NotQueued
    );
}

fn open_rec(notified_at: Option<i64>, recommended_at: i64) -> PodcastRecommendationData {
    PodcastRecommendationData {
        recommendation_id: "r".into(),
        episode_id: "itunes:1#g".into(),
        show_title: "Show".into(),
        episode_title: "Ep".into(),
        feed_url: "https://feeds.example.com/x".into(),
        episode_guid: "g".into(),
        published_at: NOW,
        run_date: "2026-07-16".into(),
        recommended_at,
        notified_at,
        ..rec()
    }
}

#[test]
fn looks_back_to_just_before_the_oldest_open_delivery() {
    let open = [
        open_rec(Some(NOW - 10 * DAY), NOW),
        open_rec(Some(NOW - 3 * DAY), NOW),
    ];
    assert_eq!(listen_history_since(&open, NOW), NOW - 10 * DAY - DAY);
}

#[test]
fn covers_a_lingering_open_row_fully_rather_than_capping_short_of_it() {
    // A cutoff after the delivery would hide real playback and mislabel it ignored.
    let open = [open_rec(Some(NOW - 200 * DAY), NOW)];
    assert_eq!(listen_history_since(&open, NOW), NOW - 200 * DAY - DAY);
}

#[test]
fn uses_recommended_at_when_never_notified() {
    let open = [open_rec(None, NOW - 5 * DAY)];
    assert_eq!(listen_history_since(&open, NOW), NOW - 5 * DAY - DAY);
}

#[test]
fn returns_now_when_nothing_is_open_nothing_to_cover() {
    assert_eq!(listen_history_since(&[], NOW), NOW);
}

// ---- end-to-end runs ----

struct EmptyWeb;

impl WebSearcher for EmptyWeb {
    fn search(&self, _options: SearchOptions) -> BoxFuture<'_, Result<WebSearchResults, String>> {
        Box::pin(async {
            Ok(WebSearchResults {
                results: Vec::new(),
                response_time: 0.0,
            })
        })
    }
}

struct OnePerson(PodcastIndexEpisode);

impl PersonSearch for OnePerson {
    fn search_by_person<'a>(
        &'a self,
        _name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError>> {
        Box::pin(async { Ok(vec![self.0.clone()]) })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    fail: bool,
    sent: Mutex<Vec<PushoverMessage>>,
}

impl Notifier for RecordingNotifier {
    fn notify(&self, message: PushoverMessage) -> BoxFuture<'_, Result<(), String>> {
        self.sent.lock().unwrap().push(message);
        Box::pin(async move {
            if self.fail {
                Err("pushover Some(500): down".to_owned())
            } else {
                Ok(())
            }
        })
    }
}

struct Harness {
    app: TestApp,
    deps: PodcastDeps,
    account: Arc<FakeAccount>,
    notifier: Arc<RecordingNotifier>,
    _dir: tempfile::TempDir,
}

fn guest_episode() -> PodcastIndexEpisode {
    PodcastIndexEpisode {
        title: "Jane on Minds".into(),
        feed_title: "Some Show".into(),
        feed_url: "https://feeds.example.com/show".into(),
        feed_itunes_id: Some(42),
        guid: "guid-abc".into(),
        enclosure_url: "https://cdn.example.com/audio.mp3".into(),
        episode_url: None,
        published_at: TEST_EPOCH_MS - DAY,
        duration_minutes: Some(55),
        description: "A conversation.".into(),
        artwork_url: None,
    }
}

async fn harness(account: FakeAccount, notifier: RecordingNotifier) -> Harness {
    let app = TestApp::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seed = dir.path().join("taste.md");
    std::fs::write(&seed, "# Taste\n\n## Voices\n\n- Jane Doe\n").unwrap();
    let mut env: BTreeMap<String, String> = test_app_env();
    env.insert(
        "PODCAST_TASTE_PATH".into(),
        seed.to_str().unwrap().to_owned(),
    );
    env.insert("RECS_PUBLIC_URL".into(), "http://omni.test/".into());
    let config = Arc::new(Config::from_env(&env).unwrap());
    let account = Arc::new(account);
    let notifier = Arc::new(notifier);
    let deps = PodcastDeps {
        store: app.ctx.store.clone(),
        clock: app.ctx.clock.clone(),
        config: config.clone(),
        tz: TimeZone::UTC,
        models: Models {
            ai: app.ctx.ai.clone(),
            config,
        },
        notifier: notifier.clone(),
        accounts: Arc::new(FakeAccounts(account.clone())),
        directory: Arc::new(FakeDirectory::default()),
        web: Arc::new(EmptyWeb),
        person_search: Some(Arc::new(OnePerson(guest_episode()))),
    };
    Harness {
        app,
        deps,
        account,
        notifier,
        _dir: dir,
    }
}

fn include_guest() -> GenerateResponse {
    GenerateResponse::text(
        serde_json::json!({
            "decisions": [{
                "candidate_id": "itunes:42#guid-abc",
                "include": true,
                "reason": "followed voice",
                "why_for_user": "You follow Jane Doe.",
                "caveats": [],
                "confidence": 0.9,
                "notification": { "title": "🧠 Some Show — Jane Doe", "message": "Jane talks minds." }
            }]
        })
        .to_string(),
    )
}

/// Returns the guest episode after an assistant reserves it, i.e. after the
/// run has already read its exclusions.
struct ReservedMidRun(omni_store::Store);

impl PersonSearch for ReservedMidRun {
    fn search_by_person<'a>(
        &'a self,
        _name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError>> {
        Box::pin(async {
            insert_podcast_recommendation(
                &self.0,
                PodcastRecommendationData {
                    recommendation_id: "assistant".into(),
                    episode_id: "itunes:42#guid-abc".into(),
                    show_id: "itunes:42".into(),
                    feed_url: "https://feeds.example.com/show".into(),
                    episode_guid: "guid-abc".into(),
                    status: PodcastRecommendationStatus::Pending,
                    recommended_at: TEST_EPOCH_MS,
                    origin: Some(RecommendationOrigin::AssistantChat),
                    chat_record: Some(ChatRecord::reserved("k".into(), TEST_EPOCH_MS)),
                    ..rec()
                },
            )
            .await
            .unwrap();
            Ok(vec![guest_episode()])
        })
    }
}

#[tokio::test]
async fn a_pick_an_assistant_reserved_mid_run_is_not_committed_twice() {
    let mut h = harness(FakeAccount::default(), RecordingNotifier::default()).await;
    h.deps.person_search = Some(Arc::new(ReservedMidRun(h.deps.store.clone())));
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![include_guest()]);
    let summary = run_podcast_pipeline(&h.deps, None, PodcastPipelineOptions::default())
        .await
        .unwrap();
    assert_eq!(summary, "no eligible picks");
    let rows = get_all_podcast_recommendations(&h.deps.store)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].recommendation_id, "assistant");
    assert!(h.account.enqueued.lock().unwrap().is_empty());
    assert!(h.notifier.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn commits_a_guest_pick_in_order_and_marks_it_notified() {
    let h = harness(FakeAccount::default(), RecordingNotifier::default()).await;
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![include_guest()]);
    let summary = run_podcast_pipeline(&h.deps, None, PodcastPipelineOptions::default())
        .await
        .unwrap();
    assert_eq!(
        summary,
        "recommended 1 (1 guest, 0 topic): Some Show — Jane on Minds"
    );

    let enqueued = h.account.enqueued.lock().unwrap().clone();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(enqueued[0].position, Some(PodcastQueuePosition::Next));
    assert_eq!(
        enqueued[0].media_url.as_deref(),
        Some("https://cdn.example.com/audio.mp3")
    );

    let rows = get_all_podcast_recommendations(&h.deps.store)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.status, PodcastRecommendationStatus::Notified);
    assert_eq!(row.queue_result, Some(PodcastQueueResult::Queued));
    assert_eq!(row.matched_voices, Some(vec!["Jane Doe".to_owned()]));
    assert_eq!(row.shortlist_scores, None);
    assert!(row.notified_at.is_some());

    let sent = h.notifier.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title.as_deref(), Some("🧠 Some Show — Jane Doe"));
    assert_eq!(
        sent[0].message,
        "Jane talks minds.\n\n🎧 Added to your Castro queue."
    );
    assert_eq!(
        sent[0].url,
        Some(format!(
            "http://omni.test/feedback/podcasts/{}",
            row.recommendation_id
        ))
    );
    assert_eq!(sent[0].url_title.as_deref(), Some("Rate this pick"));
}

#[tokio::test]
async fn a_failed_notification_fails_the_run_and_leaves_the_row_pending() {
    let h = harness(
        FakeAccount {
            enqueue_result: PodcastWriteResult::AlreadyExists,
            ..FakeAccount::default()
        },
        RecordingNotifier {
            fail: true,
            ..RecordingNotifier::default()
        },
    )
    .await;
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![include_guest()]);
    let error = run_podcast_pipeline(&h.deps, None, PodcastPipelineOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Podcast recommendation notification failed"
    );
    let rows = get_all_podcast_recommendations(&h.deps.store)
        .await
        .unwrap();
    assert_eq!(rows[0].status, PodcastRecommendationStatus::Pending);
    // The enqueue outcome was persisted before the notification was attempted.
    assert_eq!(
        rows[0].queue_result,
        Some(PodcastQueueResult::AlreadyQueued)
    );
}

#[tokio::test]
async fn skips_the_run_when_configured_subscriptions_are_unavailable() {
    let h = harness(
        FakeAccount {
            subscriptions: Err(Unavailable::new("Castro timed out")),
            ..FakeAccount::default()
        },
        RecordingNotifier::default(),
    )
    .await;
    let (summary, degraded) = omni_tasks::collect_degraded(run_podcast_pipeline(
        &h.deps,
        None,
        PodcastPipelineOptions::default(),
    ))
    .await;
    assert_eq!(
        summary.unwrap(),
        "skipped: Castro subscriptions unavailable: Castro timed out"
    );
    assert_eq!(
        degraded,
        vec!["Castro subscriptions unavailable: Castro timed out"]
    );
    assert!(h.app.ai.requests().is_empty());
    assert!(h.notifier.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stale_pending_reconciliation_fails_only_legacy_rows() {
    let h = harness(FakeAccount::default(), RecordingNotifier::default()).await;
    let two_hours_ago = TEST_EPOCH_MS - 2 * 60 * 60 * 1000;
    insert_podcast_recommendation(
        &h.deps.store,
        PodcastRecommendationData {
            status: PodcastRecommendationStatus::Pending,
            recommended_at: two_hours_ago,
            ..rec()
        },
    )
    .await
    .unwrap();
    insert_podcast_recommendation(
        &h.deps.store,
        PodcastRecommendationData {
            recommendation_id: "chat".into(),
            episode_id: "itunes:9#guid-9".into(),
            show_id: "itunes:9".into(),
            episode_guid: "guid-9".into(),
            status: PodcastRecommendationStatus::Pending,
            recommended_at: two_hours_ago,
            origin: Some(RecommendationOrigin::AssistantChat),
            chat_record: Some(ChatRecord::reserved("k".into(), two_hours_ago)),
            ..rec()
        },
    )
    .await
    .unwrap();
    h.app.ai.script(
        ModelRole::RecsSelection,
        vec![GenerateResponse::text(r#"{"decisions":[]}"#)],
    );
    run_podcast_pipeline(&h.deps, None, PodcastPipelineOptions::default())
        .await
        .unwrap();
    let rows = get_all_podcast_recommendations(&h.deps.store)
        .await
        .unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r.recommendation_id == id).unwrap();
    assert_eq!(by_id("r1").status, PodcastRecommendationStatus::Failed);
    assert_eq!(by_id("chat").status, PodcastRecommendationStatus::Pending);
    assert_eq!(by_id("chat").resolved_at, None);
    assert!(h.notifier.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconciles_stale_pending_rows_to_failed_and_rejects_bad_caps() {
    let h = harness(FakeAccount::default(), RecordingNotifier::default()).await;
    insert_podcast_recommendation(
        &h.deps.store,
        PodcastRecommendationData {
            status: PodcastRecommendationStatus::Pending,
            recommended_at: TEST_EPOCH_MS - 2 * 60 * 60 * 1000,
            ..rec()
        },
    )
    .await
    .unwrap();
    // The guest gate excludes everything, so nothing new is committed.
    h.app.ai.script(
        ModelRole::RecsSelection,
        vec![GenerateResponse::text(r#"{"decisions":[]}"#)],
    );
    let summary = run_podcast_pipeline(&h.deps, None, PodcastPipelineOptions::default())
        .await
        .unwrap();
    assert_eq!(summary, "no eligible picks");
    let rows = get_all_podcast_recommendations(&h.deps.store)
        .await
        .unwrap();
    assert_eq!(rows[0].status, PodcastRecommendationStatus::Failed);
    assert!(rows[0].resolved_at.is_some_and(|at| at >= TEST_EPOCH_MS));

    let error = run_podcast_pipeline(
        &h.deps,
        None,
        PodcastPipelineOptions {
            max_recommendations: Some(6),
            ..PodcastPipelineOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "maxRecommendations must be an integer from 1 to 5"
    );
}

#[test]
fn deduplicates_finalists_scored_twice_like_a_js_map() {
    use omni_podcasts::pipeline::unique_finalists;
    use omni_podcasts::shortlist::ScoredEpisode;
    use omni_podcasts::types::EpisodeCandidate;
    let scored = |id: &str, composite: f64| ScoredEpisode {
        candidate: EpisodeCandidate {
            episode_id: id.to_owned(),
            ..EpisodeCandidate::default()
        },
        taste_match: composite,
        novelty: composite,
        confidence: 1.0,
        risks: Vec::new(),
        composite,
    };
    let finalists = vec![scored("a", 90.0), scored("b", 80.0), scored("a", 70.0)];
    let unique = unique_finalists(&finalists);
    let ids: Vec<(&str, f64)> = unique
        .iter()
        .map(|f| (f.candidate.episode_id.as_str(), f.composite))
        .collect();
    // First position, last value: the selector can pick "a" only once.
    assert_eq!(ids, vec![("a", 70.0), ("b", 80.0)]);
}
