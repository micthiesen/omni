//! An unreachable speech server: a failed preflight puts the job back
//! without spending an attempt or any paid work, gives up after the
//! configured deadline, and a connection failure mid-synthesis is named the
//! same way. A server that answers, even with an error, is not an outage.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{FakeTts, Harness, HarnessOptions, UNREACHABLE, article, harness, metadata_json};
use futures::future::BoxFuture;
use omni_ai::{GenerateResponse, ModelRole};
use omni_config::Config;
use omni_http::SideEffectMode;
use omni_presspods::error::PressPodsError;
use omni_presspods::model::{AttemptOutcome, JobStatus, PressPodsJob};
use omni_presspods::persistence::{
    BASE_SPEECH_RETRY_DELAY_MS, MAX_SPEECH_RETRY_DELAY_MS, Persistence, speech_deferred_job,
    speech_retry_delay_ms,
};
use omni_presspods::retrievers::ArticleRetriever;
use omni_presspods::speech::providers::{
    ConfiguredTts, HiggsProvider, SPEECH_PREFLIGHT_OPERATION, TtsFactory, TtsProvider,
};
use omni_presspods::task::{PressPodsTask, SCHEDULE};
use omni_presspods::types::Article;
use omni_store::entity::{EntityWrite, UpsertOpts};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// A retriever that counts how often it is asked for the article.
struct CountingRetriever(Arc<AtomicUsize>);

impl ArticleRetriever for CountingRetriever {
    fn name(&self) -> &str {
        "readability"
    }
    fn retrieve<'a>(
        &'a self,
        _url: &'a str,
        _ua: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(article(
                &"Body text for the narration pipeline. ".repeat(8),
                "story",
            ))
        })
    }
}

async fn outage_harness(tts: FakeTts) -> (Harness, Arc<AtomicUsize>) {
    let retrievals = Arc::new(AtomicUsize::new(0));
    let h = harness(HarnessOptions {
        retrievers: vec![Arc::new(CountingRetriever(retrievals.clone()))],
        tts,
        ..HarnessOptions::default()
    })
    .await;
    (h, retrievals)
}

fn unreachable_tts() -> FakeTts {
    let tts = FakeTts::clean();
    tts.unreachable.store(true, Ordering::SeqCst);
    tts
}

fn task(h: &Harness) -> PressPodsTask {
    let schedule = omni_tasks::CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    PressPodsTask::new(h.service.clone(), schedule)
}

fn script_models(h: &Harness) {
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(true, 8.0))],
    );
    h.app.ai.script(
        ModelRole::PressPodsCleaning,
        vec![GenerateResponse::text(
            "<cleaned_article>Hook. Then the body.</cleaned_article>",
        )],
    );
}

fn queued(id: &str, now: i64) -> PressPodsJob {
    PressPodsJob {
        job_id: id.into(),
        url: "https://example.com/story".into(),
        normalized_url: None,
        status: JobStatus::Queued,
        attempts: 0,
        next_attempt_at: 0,
        last_error: None,
        created_at: now,
        updated_at: now,
        claimed_at: None,
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    }
}

async fn put_job(persistence: &Persistence, job: PressPodsJob) {
    persistence
        .store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unreachable_speech_server_requeues_the_job_before_any_paid_work() {
    let (h, retrievals) = outage_harness(unreachable_tts()).await;
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let summary = task(&h).drain(Some("PressPods:run-1")).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 1 requeued, 0 failed");

    let row = h
        .service
        .persistence()
        .get_job(&job.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, JobStatus::Queued);
    assert_eq!(row.attempts, 0, "the outage must not use up an attempt");
    assert_eq!(row.last_error.as_deref(), Some(UNREACHABLE));
    assert_eq!(
        row.next_attempt_at,
        row.updated_at + BASE_SPEECH_RETRY_DELAY_MS
    );
    assert_eq!(row.speech_unreachable_since, Some(row.updated_at));
    assert_eq!(row.claimed_at, None);

    assert_eq!(retrievals.load(Ordering::SeqCst), 0, "no retrieval");
    assert!(h.app.ai.requests().is_empty(), "no model call");
    assert_eq!(h.tts.calls.load(Ordering::SeqCst), 0, "no synthesis");

    let history = h.service.persistence().get_attempt_history().await.unwrap();
    assert_eq!(history.len(), 1);
    let attempt = &history[0].attempt;
    assert_eq!(attempt.outcome, AttemptOutcome::Retrying);
    assert_eq!(
        attempt.operation.as_deref(),
        Some(SPEECH_PREFLIGHT_OPERATION)
    );
    assert_eq!(attempt.error.as_deref(), Some(UNREACHABLE));
    assert_eq!(attempt.attempt, 1);
    assert_eq!(attempt.run_id.as_deref(), Some("PressPods:run-1"));
    assert_eq!(attempt.retrievers, None);
}

#[tokio::test]
async fn one_failed_preflight_defers_every_due_job_in_the_pass() {
    let (h, retrievals) = outage_harness(unreachable_tts()).await;
    let now = h.app.ctx.clock.now_ms();
    for id in ["a", "b", "c"] {
        let mut job = queued(id, now);
        job.url = format!("https://example.com/{id}");
        put_job(h.service.persistence(), job).await;
    }
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 3 requeued, 0 failed");
    assert_eq!(h.tts.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(retrievals.load(Ordering::SeqCst), 0);
    for job in h.service.persistence().get_all_jobs().await.unwrap() {
        assert_eq!((job.status, job.attempts), (JobStatus::Queued, 0));
    }
}

#[tokio::test]
async fn the_wait_grows_to_a_cap_and_the_history_keeps_one_row_per_outage() {
    let clock = test_clock(TEST_EPOCH_MS);
    let test = TestStore::new(clock.clone()).await;
    let persistence = Persistence::new(test.store.clone());
    put_job(&persistence, queued("j", TEST_EPOCH_MS)).await;

    let mut waits = Vec::new();
    for _ in 0..7 {
        let row = persistence
            .record_speech_unreachable("j", UNREACHABLE, None, DAY_MS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((row.status, row.attempts), (JobStatus::Queued, 0));
        waits.push((row.next_attempt_at - row.updated_at) / 60_000);
        clock.set(row.next_attempt_at);
    }
    assert_eq!(waits, [2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(speech_retry_delay_ms(1_000), MAX_SPEECH_RETRY_DELAY_MS);
    assert_eq!(persistence.get_attempt_history().await.unwrap().len(), 1);

    // A passing preflight claims the job and ends the outage; the next
    // outage starts its own clock and gets its own history row.
    persistence.claim_job("j", None).await.unwrap();
    let mut claimed = persistence.get_job("j").await.unwrap().unwrap();
    assert_eq!(claimed.speech_unreachable_since, None);
    assert_eq!(claimed.speech_unreachable_checks, None);
    claimed.status = JobStatus::Queued;
    put_job(&persistence, claimed).await;
    let again = persistence
        .record_speech_unreachable("j", UNREACHABLE, None, DAY_MS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.speech_unreachable_since, Some(again.updated_at));
    assert_eq!(
        again.next_attempt_at,
        again.updated_at + BASE_SPEECH_RETRY_DELAY_MS
    );
    assert_eq!(persistence.get_attempt_history().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_crashed_job_is_still_counted_as_an_attempt_during_an_outage() {
    let (h, _retrievals) = outage_harness(unreachable_tts()).await;
    let now = h.app.ctx.clock.now_ms();
    let mut job = queued("crashed", now - DAY_MS);
    job.status = JobStatus::Processing;
    job.claimed_at = Some(0);
    put_job(h.service.persistence(), job).await;
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 1 requeued, 0 failed");
    let row = h
        .service
        .persistence()
        .get_job("crashed")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.status, row.attempts), (JobStatus::Queued, 1));
    assert_eq!(
        row.last_error.as_deref(),
        Some("Process crashed or restarted mid-run")
    );
    assert_eq!(row.speech_unreachable_since, None);
}

#[tokio::test]
async fn a_job_that_is_no_longer_queued_is_left_alone() {
    let test = TestStore::new(test_clock(TEST_EPOCH_MS)).await;
    let persistence = Persistence::new(test.store.clone());
    put_job(&persistence, queued("j", TEST_EPOCH_MS)).await;
    persistence.claim_job("j", None).await.unwrap();
    for id in ["j", "missing"] {
        assert_eq!(
            persistence
                .record_speech_unreachable(id, UNREACHABLE, None, DAY_MS)
                .await
                .unwrap(),
            None
        );
    }
    assert!(persistence.get_attempt_history().await.unwrap().is_empty());
}

#[test]
fn the_deadline_counts_from_the_first_failed_preflight() {
    let now = TEST_EPOCH_MS;
    let mut job = queued("j", now - 2 * DAY_MS);
    // Submitted two days ago, but the outage began just under a day ago.
    job.speech_unreachable_since = Some(now - DAY_MS + 1);
    job.speech_unreachable_checks = Some(40);
    let waiting = speech_deferred_job(job.clone(), UNREACHABLE, now, DAY_MS);
    assert_eq!(waiting.status, JobStatus::Queued);
    assert_eq!(waiting.next_attempt_at, now + MAX_SPEECH_RETRY_DELAY_MS);

    job.speech_unreachable_since = Some(now - DAY_MS);
    let failed = speech_deferred_job(job, UNREACHABLE, now, DAY_MS);
    assert_eq!((failed.status, failed.attempts), (JobStatus::Failed, 0));
    assert_eq!(failed.next_attempt_at, 0);
}

#[tokio::test]
async fn a_speech_server_unreachable_past_the_deadline_fails_the_job_with_a_clear_message() {
    let (h, retrievals) = outage_harness(unreachable_tts()).await;
    let now = h.app.ctx.clock.now_ms();
    let mut job = queued("stuck", now - 2 * DAY_MS);
    job.speech_unreachable_since = Some(now - DAY_MS);
    job.speech_unreachable_checks = Some(50);
    job.last_error = Some(UNREACHABLE.to_owned());
    put_job(h.service.persistence(), job).await;

    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 0 requeued, 1 failed");
    let row = h
        .service
        .persistence()
        .get_job("stuck")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert_eq!(row.attempts, 0);
    let message = format!("{UNREACHABLE} (gave up after 24 h unreachable)");
    assert_eq!(row.last_error.as_deref(), Some(message.as_str()));
    assert_eq!(retrievals.load(Ordering::SeqCst), 0);
    assert!(h.app.ai.requests().is_empty());

    let history = h.service.persistence().get_attempt_history().await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].attempt.outcome, AttemptOutcome::Failed);
    assert_eq!(
        history[0].attempt.operation.as_deref(),
        Some(SPEECH_PREFLIGHT_OPERATION)
    );
    assert_eq!(history[0].attempt.error.as_deref(), Some(message.as_str()));

    // A manual retry starts a fresh outage clock as well as a fresh budget.
    let retried = h
        .service
        .persistence()
        .requeue_job_now("stuck")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retried.speech_unreachable_since, None);
}

#[tokio::test]
async fn the_deadline_is_configurable() {
    let retrievals = Arc::new(AtomicUsize::new(0));
    let h = harness(HarnessOptions {
        retrievers: vec![Arc::new(CountingRetriever(retrievals))],
        tts: unreachable_tts(),
        env: vec![("PRESSPODS_TTS_UNREACHABLE_DEADLINE_HOURS", "2".to_owned())],
        ..HarnessOptions::default()
    })
    .await;
    let now = h.app.ctx.clock.now_ms();
    let mut job = queued("stuck", now - DAY_MS);
    job.speech_unreachable_since = Some(now - 2 * HOUR_MS);
    put_job(h.service.persistence(), job).await;
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 0 requeued, 1 failed");
    let row = h
        .service
        .persistence()
        .get_job("stuck")
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.last_error
            .as_deref()
            .unwrap()
            .ends_with("(gave up after 2 h unreachable)")
    );
}

#[tokio::test]
async fn a_reachable_speech_server_leaves_the_normal_path_unchanged() {
    let (h, retrievals) = outage_harness(FakeTts::clean()).await;
    script_models(&h);
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let summary = task(&h).drain(Some("PressPods:run-1")).await.unwrap();
    assert_eq!(summary, "1 episode(s) created, 0 requeued, 0 failed");
    assert_eq!(h.tts.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(retrievals.load(Ordering::SeqCst), 1);
    assert!(
        h.service
            .persistence()
            .get_job(&job.job_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        h.service
            .persistence()
            .get_all_episodes()
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        h.service
            .persistence()
            .get_attempt_history()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_deferred_job_runs_once_the_speech_server_is_back_and_resolves_its_history() {
    let (h, _retrievals) = outage_harness(unreachable_tts()).await;
    script_models(&h);
    let now = h.app.ctx.clock.now_ms();
    put_job(h.service.persistence(), queued("j", now)).await;
    task(&h).drain(None).await.unwrap();

    h.tts.unreachable.store(false, Ordering::SeqCst);
    let mut waiting = h.service.persistence().get_job("j").await.unwrap().unwrap();
    waiting.next_attempt_at = 0;
    put_job(h.service.persistence(), waiting).await;
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "1 episode(s) created, 0 requeued, 0 failed");

    let history = h.service.persistence().get_attempt_history().await.unwrap();
    let outcomes: Vec<(AttemptOutcome, i64)> = history
        .iter()
        .map(|entry| (entry.attempt.outcome, entry.attempt.attempt))
        .collect();
    assert!(outcomes.contains(&(AttemptOutcome::Succeeded, 1)));
    assert!(outcomes.contains(&(AttemptOutcome::Retrying, 1)));
    let deferred = history
        .iter()
        .find(|entry| entry.attempt.outcome == AttemptOutcome::Retrying)
        .unwrap();
    assert!(deferred.resolved_by_episode_id.is_some());
}

#[tokio::test]
async fn a_connection_failure_mid_synthesis_uses_an_attempt_and_names_the_speech_server() {
    let tts = FakeTts::clean();
    *tts.fail_with.lock().unwrap() = Some(|| PressPodsError::SpeechUnreachable {
        operation: "synthesize Higgs chunk".to_owned(),
        url: "http://tts.test:8000".to_owned(),
        reason: "tcp connect error: No route to host".to_owned(),
        timed_out: false,
    });
    let (h, _retrievals) = outage_harness(tts).await;
    script_models(&h);
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 1 requeued, 0 failed");
    let row = h
        .service
        .persistence()
        .get_job(&job.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.status, row.attempts), (JobStatus::Queued, 1));
    assert_eq!(row.last_error.as_deref(), Some(UNREACHABLE));
    let history = h.service.persistence().get_attempt_history().await.unwrap();
    assert_eq!(
        history[0].attempt.operation.as_deref(),
        Some("synthesize Higgs chunk")
    );
}

// The real Higgs client against a local socket.

fn higgs_config(base_url: &str) -> Arc<Config> {
    let mut env = omni_testkit::test_app_env();
    env.insert("PRESSPODS_TTS_URL".into(), base_url.to_owned());
    Arc::new(Config::from_env(&env).unwrap())
}

fn configured(base_url: &str) -> ConfiguredTts {
    ConfiguredTts {
        http: omni_testkit::no_network(),
        config: higgs_config(base_url),
        mode: SideEffectMode::Live,
    }
}

/// A loopback address nothing listens on.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

#[tokio::test]
async fn the_preflight_passes_whenever_the_speech_server_answers() {
    for status in [200, 404, 500] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        assert!(configured(&server.uri()).preflight().await.is_ok());
    }
}

#[tokio::test]
async fn the_preflight_reports_a_refused_connection_as_unreachable() {
    let base = closed_port();
    let error = configured(&base).preflight().await.unwrap_err();
    assert!(error.is_speech_unreachable(), "{error}");
    assert_eq!(error.operation(), SPEECH_PREFLIGHT_OPERATION);
    let summary = error.summary();
    assert!(
        summary.starts_with(&format!("speech server unreachable at {base}: ")),
        "{summary}"
    );
    assert_eq!(summary.matches(&base).count(), 1, "{summary}");
}

#[tokio::test]
async fn the_preflight_is_skipped_for_elevenlabs_and_without_a_url() {
    let mut env = omni_testkit::test_app_env();
    env.insert("PRESSPODS_TTS_PROVIDER".into(), "elevenlabs".into());
    env.insert("PRESSPODS_TTS_URL".into(), closed_port());
    let elevenlabs = ConfiguredTts {
        http: omni_testkit::no_network(),
        config: Arc::new(Config::from_env(&env).unwrap()),
        mode: SideEffectMode::Live,
    };
    assert!(elevenlabs.preflight().await.is_ok());
    let unset = ConfiguredTts {
        http: omni_testkit::no_network(),
        config: Arc::new(Config::from_env(&omni_testkit::test_app_env()).unwrap()),
        mode: SideEffectMode::Live,
    };
    assert!(unset.preflight().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_refused_connection_mid_synthesis_is_reported_as_unreachable() {
    let base = closed_port();
    let provider = HiggsProvider::new(
        omni_testkit::no_network(),
        &higgs_config(&base),
        None,
        SideEffectMode::Live,
    )
    .unwrap();
    let error = provider.synthesize_chunk("Hello there.").await.unwrap_err();
    assert!(
        error.is_speech_unreachable() && error.is_retryable(),
        "{error}"
    );
    assert_eq!(error.operation(), "synthesize Higgs chunk");
    assert!(
        error
            .summary()
            .starts_with(&format!("speech server unreachable at {base}: ")),
        "{}",
        error.summary()
    );
}

#[tokio::test]
async fn an_error_answer_from_the_speech_server_keeps_its_own_classification() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = HiggsProvider::new(
        omni_testkit::no_network(),
        &higgs_config(&server.uri()),
        None,
        SideEffectMode::Live,
    )
    .unwrap();
    let error = provider.synthesize_chunk("Hello there.").await.unwrap_err();
    assert!(!error.is_speech_unreachable() && !error.is_retryable());
    assert_eq!(
        error.summary(),
        "HTTPError: Response code 400 (Bad Request)"
    );
}
