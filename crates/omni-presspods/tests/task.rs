//! The `PressPods` queue worker over in-process
//! fakes: the happy path, transient and permanent failures, crash recovery
//! and record mode.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeRetriever, FakeTts, Harness, HarnessOptions, article, harness, metadata_json};
use omni_ai::{GenerateResponse, ModelRole};
use omni_presspods::error::PressPodsError;
use omni_presspods::model::{JobStatus, PressPodsJob};
use omni_presspods::persistence::STALE_CLAIM_MS;
use omni_presspods::task::{PressPodsTask, SCHEDULE};
use omni_store::entity::{EntityWrite, UpsertOpts};

fn task(h: &Harness) -> PressPodsTask {
    let schedule = omni_tasks::CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    PressPodsTask::new(h.service.clone(), schedule)
}

fn options(tts: FakeTts) -> HarnessOptions {
    HarnessOptions {
        retrievers: vec![Arc::new(FakeRetriever {
            name: "readability",
            result: Ok(article(
                &"Body text for the narration pipeline. ".repeat(8),
                "story",
            )),
        })],
        tts,
        ..HarnessOptions::default()
    }
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

async fn put_job(h: &Harness, job: PressPodsJob) {
    h.service
        .persistence()
        .store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
}

#[tokio::test]
async fn drains_a_queued_job_into_an_episode() {
    let h = harness(options(FakeTts::clean())).await;
    script_models(&h);
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let summary = task(&h).drain(Some("PressPods:run-1")).await.unwrap();
    assert_eq!(summary, "1 episode(s) created, 0 requeued, 0 failed");
    assert!(
        h.service
            .persistence()
            .get_job(&job.job_id)
            .await
            .unwrap()
            .is_none()
    );
    let episodes = h.service.persistence().get_all_episodes().await.unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].run_id.as_deref(), Some("PressPods:run-1"));
    assert_eq!(task(&h).drain(None).await.unwrap(), "No episode jobs due");
}

#[tokio::test]
async fn requeues_a_transient_tts_outage_with_backoff() {
    let tts = FakeTts::clean();
    *tts.fail_with.lock().unwrap() =
        Some(|| PressPodsError::status("synthesize chunk", 503, "busy"));
    let h = harness(options(tts)).await;
    script_models(&h);
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
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_error.as_deref(), Some("503: busy"));
    assert_eq!(row.last_run_id.as_deref(), Some("PressPods:run-1"));
    assert!(row.next_attempt_at > row.updated_at);
}

#[tokio::test(start_paused = true)]
async fn requeues_when_the_rating_model_is_down_instead_of_failing_the_article() {
    let h = harness(options(FakeTts::clean())).await;
    for _ in 0..8 {
        h.app.ai.script_failure(
            ModelRole::PressPodsMetadata,
            omni_testkit::FakeFailure {
                status: 503,
                message: "overloaded".to_owned(),
            },
        );
    }
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    assert_eq!(
        task(&h).drain(None).await.unwrap(),
        "0 episode(s) created, 1 requeued, 0 failed"
    );
    let row = h
        .service
        .persistence()
        .get_job(&job.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, JobStatus::Queued);
    assert_eq!(
        row.last_error.as_deref(),
        Some("Error: All article retrievers failed (readability: provider error 503: overloaded)")
    );
}

#[tokio::test]
async fn fails_a_permanently_bad_article() {
    let h = harness(options(FakeTts::clean())).await;
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(false, 1.0))],
    );
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    assert_eq!(
        task(&h).drain(None).await.unwrap(),
        "0 episode(s) created, 0 requeued, 1 failed"
    );
    let row = h
        .service
        .persistence()
        .get_job(&job.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert_eq!(
        row.last_error.as_deref(),
        Some("Error: All article retrievers failed (readability: Invalid article)")
    );
}

fn processing(id: &str, url: &str, created_at: i64) -> PressPodsJob {
    PressPodsJob {
        job_id: id.into(),
        url: url.into(),
        normalized_url: None,
        status: JobStatus::Processing,
        attempts: 0,
        next_attempt_at: 0,
        last_error: None,
        created_at,
        updated_at: created_at,
        claimed_at: Some(created_at),
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn completes_a_crashed_job_whose_episode_already_landed() {
    let h = harness(HarnessOptions::default()).await;
    let older = common::episode("https://example.com/story?utm_source=a", 1);
    let landed = common::episode("https://example.com/story", 1_000);
    for e in [&older, &landed] {
        h.service
            .persist_episode_with_audio(e, b"mp3")
            .await
            .unwrap();
    }
    put_job(&h, processing("crashed", "https://example.com/story", 500)).await;
    let work_id = omni_presspods::storage::checkpoint_work_id("https://example.com/story");
    h.service
        .audio()
        .write_chunk_checkpoint(&work_id, "k.wav", b"wav")
        .await;

    // Boot reclaim makes the fresh-looking claim immediately stale.
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "1 episode(s) created, 0 requeued, 0 failed");
    assert!(
        h.service
            .persistence()
            .get_job("crashed")
            .await
            .unwrap()
            .is_none()
    );
    let ids: Vec<String> = h
        .service
        .persistence()
        .get_all_episodes()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.episode_id)
        .collect();
    assert_eq!(ids, std::slice::from_ref(&landed.episode_id));
    assert!(
        h.service
            .audio()
            .read_chunk_checkpoint(&work_id, "k.wav")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn counts_a_crash_without_an_episode_as_an_attempt() {
    let h = harness(HarnessOptions::default()).await;
    let now = h.app.ctx.clock.now_ms();
    put_job(
        &h,
        processing(
            "crashed",
            "https://example.com/story",
            now - STALE_CLAIM_MS - 10,
        ),
    )
    .await;
    let summary = task(&h).drain(None).await.unwrap();
    assert_eq!(summary, "0 episode(s) created, 1 requeued, 0 failed");
    let row = h
        .service
        .persistence()
        .get_job("crashed")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.attempts, 1);
    assert_eq!(
        row.last_error.as_deref(),
        Some("Process crashed or restarted mid-run")
    );
}

#[tokio::test]
async fn record_mode_leaves_the_queue_to_the_live_service() {
    let h = harness(HarnessOptions {
        mode: omni_http::SideEffectMode::Record,
        ..options(FakeTts::clean())
    })
    .await;
    let job = h
        .service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let summary = task(&h).drain(None).await.unwrap();
    assert!(summary.starts_with("Record mode"));
    assert_eq!(
        h.service
            .persistence()
            .get_job(&job.job_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Queued
    );
    assert_eq!(h.tts.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
