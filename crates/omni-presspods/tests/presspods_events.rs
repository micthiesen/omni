//! `presspods.job_finished`: published for finished episodes (including one
//! recovered from a crashed job, with the same key) and for permanent
//! failures; retryable failures publish nothing and callbacks carry only the
//! article's hostname.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{FakeRetriever, FakeTts, Harness, HarnessOptions, article, harness, metadata_json};
use omni_ai::{GenerateResponse, ModelRole};
use omni_api::events::PRESSPODS_JOB_FINISHED;
use omni_presspods::error::PressPodsError;
use omni_presspods::model::{JobStatus, PressPodsJob};
use omni_presspods::task::{PressPodsTask, SCHEDULE};
use omni_runtime::ports::Ports;
use omni_store::entity::{EntityWrite, UpsertOpts};
use omni_testkit::RecordedEvents;
use serde_json::json;

fn task(h: &Harness) -> (PressPodsTask, RecordedEvents) {
    let ports = Ports::default();
    let events = RecordedEvents::install(&ports);
    let schedule = omni_tasks::CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    (
        PressPodsTask::new(h.service.clone().with_ports(ports), schedule),
        events,
    )
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

#[tokio::test]
async fn publishes_a_finished_episode_with_the_article_host_only() {
    let h = harness(options(FakeTts::clean())).await;
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
    let job = h
        .service
        .submit_episode_url("https://example.com/story?secret=1")
        .await
        .unwrap();
    let (task, events) = task(&h);
    task.drain(None).await.unwrap();

    let episode = h.service.persistence().get_all_episodes().await.unwrap()[0].clone();
    let published = events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].name, PRESSPODS_JOB_FINISHED);
    assert_eq!(
        published[0].dedup_key,
        format!("episode:{}", episode.episode_id)
    );
    let data = &published[0].data;
    assert_eq!(data["outcome"], "published");
    assert_eq!(data["jobId"], json!(job.job_id));
    assert_eq!(data["articleUrlHost"], "example.com");
    assert!(!serde_json::to_string(data).unwrap().contains("secret"));
}

#[tokio::test]
async fn publishes_permanent_failures_but_not_retryable_ones() {
    let tts = FakeTts::clean();
    *tts.fail_with.lock().unwrap() =
        Some(|| PressPodsError::status("synthesize chunk", 503, "busy"));
    let h = harness(options(tts)).await;
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(true, 8.0))],
    );
    h.app.ai.script(
        ModelRole::PressPodsCleaning,
        vec![GenerateResponse::text(
            "<cleaned_article>Hook.</cleaned_article>",
        )],
    );
    h.service
        .submit_episode_url("https://example.com/story")
        .await
        .unwrap();
    let (task, events) = task(&h);
    assert_eq!(
        task.drain(None).await.unwrap(),
        "0 episode(s) created, 1 requeued, 0 failed"
    );
    assert!(events.published().is_empty());

    let bad = harness(options(FakeTts::clean())).await;
    bad.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(false, 1.0))],
    );
    let job = bad
        .service
        .submit_episode_url("https://news.example.org/a")
        .await
        .unwrap();
    let (task, events) = self::task(&bad);
    task.drain(None).await.unwrap();
    let published = events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].dedup_key, format!("failed:{}:1", job.job_id));
    assert_eq!(
        serde_json::Value::Object(published[0].data.clone()),
        json!({
            "outcome": "failed",
            "episodeId": null,
            "jobId": job.job_id,
            "title": null,
            "articleUrlHost": "news.example.org",
            "durationSeconds": null,
            "attempts": 1,
        })
    );
}

#[tokio::test]
async fn a_crashed_job_whose_episode_landed_publishes_that_episode() {
    let h = harness(HarnessOptions::default()).await;
    let landed = common::episode("https://example.com/story", 1_000);
    h.service
        .persist_episode_with_audio(&landed, b"mp3")
        .await
        .unwrap();
    let job = PressPodsJob {
        job_id: "crashed".into(),
        url: "https://example.com/story".into(),
        normalized_url: None,
        status: JobStatus::Processing,
        attempts: 0,
        next_attempt_at: 0,
        last_error: None,
        created_at: 500,
        updated_at: 500,
        claimed_at: Some(500),
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    };
    h.service
        .persistence()
        .store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
    let (task, events) = task(&h);
    task.drain(None).await.unwrap();
    let published = events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].dedup_key,
        format!("episode:{}", landed.episode_id)
    );
}
