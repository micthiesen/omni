//! The durable job-attempt history: written on every failed attempt, kept
//! when the job is deleted or resubmitted, resolved by a later success,
//! expired after 180 days and listed read-only through `presspods_list`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{FakeRetriever, FakeTts, Harness, HarnessOptions, article, harness, metadata_json};
use futures::future::BoxFuture;
use omni_ai::{GenerateResponse, ModelRole};
use omni_core::clock::TestClock;
use omni_http::HttpError;
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolError, ToolOutput, ToolPhase};
use omni_presspods::error::PressPodsError;
use omni_presspods::model::{
    ATTEMPT_HISTORY_TTL_MS, AttemptOutcome, JobStatus, PressPodsJob, PressPodsJobAttempt,
    RetrieverErrorClass,
};
use omni_presspods::persistence::{AttemptHistoryEntry, Persistence, resolving_success};
use omni_presspods::retrievers::ArticleRetriever;
use omni_presspods::task::{PressPodsTask, SCHEDULE};
use omni_presspods::types::Article;
use omni_store::DocWrite;
use omni_store::entity::{EntityWrite, UpsertOpts};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};
use serde_json::{Value, json};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const BODY: &str = "Body text for the narration pipeline. ";
const CHALLENGE: &str = "<html>challenge page markup</html>";

/// A retriever the site answers with a 403 and a challenge page.
struct BlockedRetriever;

impl ArticleRetriever for BlockedRetriever {
    fn name(&self) -> &str {
        "fetch"
    }
    fn retrieve<'a>(
        &'a self,
        _url: &'a str,
        _ua: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async {
            Err(PressPodsError::http(
                "retrieve article with fetch",
                HttpError::Status {
                    status: 403,
                    body: CHALLENGE.to_owned(),
                },
            ))
        })
    }
}

fn options(tts: FakeTts) -> HarnessOptions {
    HarnessOptions {
        retrievers: vec![
            Arc::new(BlockedRetriever),
            Arc::new(FakeRetriever {
                name: "readability",
                result: Ok(article(&BODY.repeat(8), "story")),
            }),
        ],
        tts,
        ..HarnessOptions::default()
    }
}

fn task(h: &Harness) -> PressPodsTask {
    let schedule = omni_tasks::CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    PressPodsTask::new(h.service.clone(), schedule)
}

fn script_rejection(h: &Harness) {
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(false, 1.0))],
    );
}

fn script_success(h: &Harness) {
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

async fn history(h: &Harness) -> Vec<AttemptHistoryEntry> {
    h.service.persistence().get_attempt_history().await.unwrap()
}

/// Submits `url`, lets the rating model reject it and drains the queue.
async fn fail_permanently(h: &Harness, url: &str) -> PressPodsJob {
    script_rejection(h);
    let job = h.service.submit_episode_url(url).await.unwrap();
    assert_eq!(
        task(h).drain(Some("PressPods:run-1")).await.unwrap(),
        "0 episode(s) created, 0 requeued, 1 failed"
    );
    job
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Result<Value, ToolError> {
    let tool = tools.iter().find(|t| t.meta.name == name).unwrap();
    match tool.handler.call(input, standalone_context("test")).await? {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Ok(Value::Object(map)),
    }
}

#[tokio::test]
async fn a_permanent_failure_records_every_retrievers_outcome() {
    let h = harness(options(FakeTts::clean())).await;
    let job = fail_permanently(&h, "https://www.Example.com/story?utm_source=feed").await;

    let entries = history(&h).await;
    assert_eq!(entries.len(), 1);
    let row = &entries[0].attempt;
    assert_eq!(row.job_id, job.job_id);
    assert_eq!(row.url, job.url);
    assert_eq!(row.normalized_url, "https://example.com/story");
    assert_eq!(row.domain.as_deref(), Some("example.com"));
    assert_eq!(row.attempt, 1);
    assert_eq!(row.outcome, AttemptOutcome::Failed);
    assert_eq!(row.operation.as_deref(), Some("retrieve PressPods article"));
    assert_eq!(row.run_id.as_deref(), Some("PressPods:run-1"));
    assert_eq!(
        row.error.as_deref(),
        Some(
            "Error: All article retrievers failed (fetch: HTTP 403; readability: Invalid article)"
        )
    );
    assert_eq!(entries[0].resolved_at, None);

    let retrievers = row.retrievers.as_ref().unwrap();
    assert_eq!(retrievers.len(), 2);
    let blocked = &retrievers[0];
    assert_eq!(blocked.name, "fetch");
    assert!(!blocked.ok);
    assert_eq!(blocked.error_class, Some(RetrieverErrorClass::HttpStatus));
    assert_eq!(blocked.http_status, Some(403));
    assert_eq!(blocked.error.as_deref(), Some("HTTP 403"));
    assert_eq!(blocked.text_chars, None);
    let rejected = &retrievers[1];
    assert_eq!(rejected.name, "readability");
    assert!(!rejected.ok);
    assert_eq!(
        rejected.error_class,
        Some(RetrieverErrorClass::InvalidArticle)
    );
    assert_eq!(
        rejected.text_chars,
        Some(i64::try_from(BODY.repeat(8).len()).unwrap())
    );
    assert_eq!(rejected.content_rating, None);

    // Small rows: neither the article text nor the response body is stored.
    let stored = serde_json::to_string(row).unwrap();
    assert!(!stored.contains("Body text"));
    assert!(!stored.contains("challenge page"));
}

#[tokio::test]
async fn a_requeued_attempt_is_recorded_as_retrying_with_the_winning_retriever() {
    let tts = FakeTts::clean();
    *tts.fail_with.lock().unwrap() =
        Some(|| PressPodsError::status("synthesize chunk", 503, "busy"));
    let h = harness(options(tts)).await;
    script_success(&h);
    h.service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    assert_eq!(
        task(&h).drain(None).await.unwrap(),
        "0 episode(s) created, 1 requeued, 0 failed"
    );

    let entries = history(&h).await;
    assert_eq!(entries.len(), 1);
    let row = &entries[0].attempt;
    assert_eq!(row.outcome, AttemptOutcome::Retrying);
    assert_eq!(row.attempt, 1);
    assert_eq!(row.error.as_deref(), Some("503: busy"));
    assert!(row.operation.is_some());
    let winner = &row.retrievers.as_ref().unwrap()[1];
    assert_eq!(winner.name, "readability");
    assert!(winner.ok);
    assert_eq!(winner.selected, Some(true));
    assert_eq!(winner.content_rating, Some(8.0));
}

#[tokio::test]
async fn history_survives_deleting_and_resubmitting_the_job() {
    let h = harness(options(FakeTts::clean())).await;
    let tools = omni_presspods::mcp::tools(&h.service).unwrap();
    let url = "https://example.com/story";
    let first = fail_permanently(&h, url).await;

    let deleted = call(
        &tools,
        "presspods_delete",
        json!({ "resource": "job", "jobId": first.job_id }),
    )
    .await
    .unwrap();
    assert_eq!(deleted, json!({ "resource": "job", "deleted": true }));
    assert!(
        h.service
            .persistence()
            .get_all_jobs()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(history(&h).await.len(), 1);

    // A resubmission after the delete is a new job; its failure is appended.
    let second = fail_permanently(&h, url).await;
    assert_ne!(second.job_id, first.job_id);
    let entries = history(&h).await;
    assert_eq!(entries.len(), 2);
    let mut job_ids: Vec<&str> = entries.iter().map(|e| e.attempt.job_id.as_str()).collect();
    job_ids.sort_unstable();
    let mut expected = [first.job_id.as_str(), second.job_id.as_str()];
    expected.sort_unstable();
    assert_eq!(job_ids, expected);

    // Resubmitting a failed job requeues it with a cleared error; the rows stay.
    let requeued = h.service.submit_episode_url(url).await.unwrap();
    assert_eq!(requeued.job_id, second.job_id);
    assert_eq!(requeued.status, JobStatus::Queued);
    assert_eq!(requeued.last_error, None);
    let after = history(&h).await;
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|e| e.attempt.error.is_some()));
}

#[tokio::test]
async fn a_later_success_is_recorded_and_resolves_the_earlier_failures() {
    let h = harness(options(FakeTts::clean())).await;
    let url = "https://example.com/story";
    let failed = fail_permanently(&h, url).await;

    script_success(&h);
    let retried = h.service.submit_episode_url(url).await.unwrap();
    assert_eq!(retried.job_id, failed.job_id);
    assert_eq!(
        task(&h).drain(None).await.unwrap(),
        "1 episode(s) created, 0 requeued, 0 failed"
    );
    let episodes = h.service.persistence().get_all_episodes().await.unwrap();
    assert_eq!(episodes.len(), 1);

    let entries = history(&h).await;
    assert_eq!(entries.len(), 2);
    let success = entries
        .iter()
        .find(|e| e.attempt.outcome == AttemptOutcome::Succeeded)
        .unwrap();
    assert_eq!(
        success.attempt.episode_id.as_deref(),
        Some(episodes[0].episode_id.as_str())
    );
    assert_eq!(success.attempt.error, None);
    assert_eq!(success.resolved_at, None);
    let selected: Vec<&str> = success
        .attempt
        .retrievers
        .as_ref()
        .unwrap()
        .iter()
        .filter(|r| r.selected == Some(true))
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(selected, ["readability"]);

    let failure = entries
        .iter()
        .find(|e| e.attempt.outcome == AttemptOutcome::Failed)
        .unwrap();
    assert_eq!(failure.resolved_at, Some(success.attempt.at));
    assert_eq!(
        failure.resolved_by_episode_id.as_deref(),
        Some(episodes[0].episode_id.as_str())
    );

    // An article that never failed adds nothing to the history.
    script_success(&h);
    h.service
        .submit_episode_url("https://example.com/other")
        .await
        .unwrap();
    assert_eq!(
        task(&h).drain(None).await.unwrap(),
        "1 episode(s) created, 0 requeued, 0 failed"
    );
    assert_eq!(history(&h).await.len(), 2);
}

#[tokio::test]
async fn presspods_list_reads_history_filtered_by_url_domain_and_date() {
    let h = harness(options(FakeTts::clean())).await;
    let tools = omni_presspods::mcp::tools(&h.service).unwrap();
    fail_permanently(&h, "https://www.example.com/story").await;
    fail_permanently(&h, "https://blog.other.test/post").await;

    let all = call(&tools, "presspods_list", json!({ "resource": "history" }))
        .await
        .unwrap();
    assert_eq!(all["resource"], "history");
    assert_eq!(all["total"], 2);
    let item = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["domain"] == "example.com")
        .unwrap();
    assert_eq!(item["outcome"], "failed");
    assert_eq!(item["attempt"], 1);
    assert_eq!(item["resolvedAt"], Value::Null);
    assert_eq!(item["retrievers"][0]["errorClass"], "http_status");
    assert_eq!(item["retrievers"][0]["httpStatus"], 403);
    assert_eq!(item["retrievers"][1]["errorClass"], "invalid_article");
    assert_eq!(item["retrievers"][1]["selected"], false);

    let list = |input: Value| {
        let tools = &tools;
        async move {
            let out = call(tools, "presspods_list", input).await.unwrap();
            out["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["url"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        list(json!({ "resource": "history", "domain": "WWW.Example.com." })).await,
        ["https://www.example.com/story"]
    );
    assert_eq!(
        list(json!({ "resource": "history", "domain": "https://example.com/another?x=1" })).await,
        ["https://www.example.com/story"]
    );
    // A domain matches its subdomains, never a longer name ending the same way.
    assert_eq!(
        list(json!({ "resource": "history", "domain": "other.test" })).await,
        ["https://blog.other.test/post"]
    );
    assert!(
        list(json!({ "resource": "history", "domain": "her.test" }))
            .await
            .is_empty()
    );
    assert_eq!(
        list(json!({ "resource": "history", "query": "/POST" })).await,
        ["https://blog.other.test/post"]
    );
    // The test clock starts at 2026-01-01T00:00:00Z.
    assert_eq!(
        list(json!({ "resource": "history", "since": "2025-12-30", "until": "2026-01-02" }))
            .await
            .len(),
        2
    );
    assert!(
        list(json!({ "resource": "history", "until": "2025-12-29" }))
            .await
            .is_empty()
    );
    assert!(
        list(json!({ "resource": "history", "since": "2026-01-03" }))
            .await
            .is_empty()
    );

    let page = call(
        &tools,
        "presspods_list",
        json!({ "resource": "history", "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["nextCursor"], 1);

    let bad_date = call(
        &tools,
        "presspods_list",
        json!({ "resource": "history", "since": "2026-13-40" }),
    )
    .await
    .unwrap_err();
    assert_eq!(bad_date.phase, ToolPhase::Input);
}

// Persistence, on a store whose clock the test moves.

fn job(job_id: &str) -> PressPodsJob {
    PressPodsJob {
        job_id: job_id.into(),
        url: "https://example.com/a".into(),
        normalized_url: None,
        status: JobStatus::Queued,
        attempts: 0,
        next_attempt_at: 0,
        last_error: None,
        created_at: TEST_EPOCH_MS,
        updated_at: TEST_EPOCH_MS,
        claimed_at: None,
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    }
}

async fn store() -> (Persistence, Arc<TestClock>, TestStore) {
    let clock = test_clock(TEST_EPOCH_MS);
    let test = TestStore::new(clock.clone()).await;
    (Persistence::new(test.store.clone()), clock, test)
}

async fn put_job(p: &Persistence, job: PressPodsJob) {
    p.store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
}

#[tokio::test]
async fn rows_are_kept_180_days_and_then_pruned() {
    let (p, clock, _test) = store().await;
    put_job(&p, job("old")).await;
    p.record_job_failure(&job("old"), "boom", false)
        .await
        .unwrap();

    clock.set(TEST_EPOCH_MS + 100 * DAY_MS);
    put_job(&p, job("newer")).await;
    p.record_job_failure(&job("newer"), "boom", false)
        .await
        .unwrap();

    // The clock also runs in real time, so leave a minute of margin.
    clock.set(TEST_EPOCH_MS + ATTEMPT_HISTORY_TTL_MS - 60_000);
    assert_eq!(p.get_attempt_history().await.unwrap().len(), 2);

    clock.set(TEST_EPOCH_MS + ATTEMPT_HISTORY_TTL_MS + 60_000);
    let kept = p.get_attempt_history().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].attempt.job_id, "newer");
    // The maintenance sweep physically removes the expired row only.
    let removed = p.store().write(|tx| tx.cleanup_expired(100)).await.unwrap();
    assert_eq!(removed, 1);
    assert_eq!(p.get_attempt_history().await.unwrap().len(), 1);
    assert_eq!(ATTEMPT_HISTORY_TTL_MS, 180 * DAY_MS);
}

#[tokio::test]
async fn every_attempt_of_a_job_is_appended_until_it_fails_for_good() {
    let (p, _clock, _test) = store().await;
    put_job(&p, job("flaky")).await;
    let mut live = job("flaky");
    for _ in 0..omni_presspods::persistence::MAX_JOB_ATTEMPTS {
        live = p
            .record_job_failure(&live, "503: busy", true)
            .await
            .unwrap();
    }
    assert_eq!(live.status, JobStatus::Failed);

    let mut rows: Vec<PressPodsJobAttempt> = p
        .get_attempt_history()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.attempt)
        .collect();
    rows.sort_by_key(|row| row.attempt);
    let outcomes: Vec<(i64, AttemptOutcome)> =
        rows.iter().map(|row| (row.attempt, row.outcome)).collect();
    assert_eq!(
        outcomes,
        [
            (1, AttemptOutcome::Retrying),
            (2, AttemptOutcome::Retrying),
            (3, AttemptOutcome::Retrying),
            (4, AttemptOutcome::Retrying),
            (5, AttemptOutcome::Retrying),
            (6, AttemptOutcome::Failed),
        ]
    );
}

#[tokio::test]
async fn a_failure_of_a_concurrently_deleted_job_is_still_recorded() {
    let (p, _clock, _test) = store().await;
    p.record_job_failure(&job("gone"), "boom", true)
        .await
        .unwrap();
    assert!(p.get_job("gone").await.unwrap().is_none());
    let entries = p.get_attempt_history().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].attempt.job_id, "gone");
}

#[tokio::test]
async fn stored_error_text_is_capped() {
    let (p, _clock, _test) = store().await;
    put_job(&p, job("long")).await;
    p.record_job_failure(&job("long"), &"x".repeat(5_000), false)
        .await
        .unwrap();
    let entries = p.get_attempt_history().await.unwrap();
    assert_eq!(entries[0].attempt.error.as_ref().unwrap().len(), 500);
}

#[tokio::test]
async fn completing_a_job_that_failed_earlier_appends_the_success() {
    let (p, clock, _test) = store().await;
    put_job(&p, job("retry")).await;
    let failed = p
        .record_job_failure(&job("retry"), "503: busy", true)
        .await
        .unwrap();
    clock.set(TEST_EPOCH_MS + 60_000);
    p.complete_job(&failed, "episode-1", Vec::new())
        .await
        .unwrap();
    assert!(p.get_job("retry").await.unwrap().is_none());

    let entries = p.get_attempt_history().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Succeeded);
    assert_eq!(entries[0].attempt.attempt, 2);
    assert_eq!(entries[1].resolved_at, Some(TEST_EPOCH_MS + 60_000));
    assert_eq!(
        entries[1].resolved_by_episode_id.as_deref(),
        Some("episode-1")
    );

    // A failure after that success is a new, unresolved one.
    clock.set(TEST_EPOCH_MS + 120_000);
    put_job(&p, job("again")).await;
    p.record_job_failure(&job("again"), "boom", false)
        .await
        .unwrap();
    let entries = p.get_attempt_history().await.unwrap();
    assert_eq!(entries[0].attempt.job_id, "again");
    assert_eq!(entries[0].resolved_at, None);
    assert_eq!(entries[2].resolved_at, Some(TEST_EPOCH_MS + 60_000));
}

#[test]
fn a_success_resolves_only_failures_of_the_same_article_at_or_before_it() {
    let row = |url: &str, at: i64, outcome: AttemptOutcome| PressPodsJobAttempt {
        attempt_id: format!("{url}-{at}"),
        job_id: "job".into(),
        url: url.into(),
        normalized_url: url.into(),
        domain: None,
        attempt: 1,
        at,
        outcome,
        error: None,
        operation: None,
        retrievers: None,
        run_id: None,
        episode_id: None,
        extra: Default::default(),
    };
    let rows = [
        row("a", 10, AttemptOutcome::Succeeded),
        row("a", 20, AttemptOutcome::Failed),
        row("a", 30, AttemptOutcome::Succeeded),
        row("a", 40, AttemptOutcome::Succeeded),
        row("a", 50, AttemptOutcome::Retrying),
        row("b", 5, AttemptOutcome::Failed),
    ];
    assert_eq!(resolving_success(&rows[1], &rows).map(|r| r.at), Some(30));
    assert_eq!(resolving_success(&rows[4], &rows), None);
    assert_eq!(resolving_success(&rows[5], &rows), None);
    assert_eq!(resolving_success(&rows[2], &rows), None);
}
