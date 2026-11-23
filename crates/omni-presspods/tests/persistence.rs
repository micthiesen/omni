//! PressPods persistence. Each case runs on its own temporary store; times
//! come from the test clock (`TEST_EPOCH_MS`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use indexmap::IndexMap;
use omni_presspods::error::PressPodsError;
use omni_presspods::model::{
    Chapter, ChunkStat, Costs, JobStatus, PressPodsEpisode, PressPodsJob, RetrieverAttempt,
    TokenCounts,
};
use omni_presspods::persistence::{
    MAX_JOB_ATTEMPTS, Persistence, STALE_CLAIM_MS, retry_delay_ms, select_due_jobs,
};
use omni_store::cbor::{JsValue, to_value};
use omni_store::entity::{EntityWrite, UpsertOpts};
use omni_store::{DocOps, DocWrite, StoreError};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

const NOW: i64 = 1_700_000_000_000;

fn job(job_id: &str) -> PressPodsJob {
    PressPodsJob {
        job_id: job_id.into(),
        url: "https://example.com/a".into(),
        normalized_url: None,
        status: JobStatus::Queued,
        attempts: 0,
        next_attempt_at: 0,
        last_error: None,
        created_at: NOW - 60_000,
        updated_at: NOW - 60_000,
        claimed_at: None,
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    }
}

async fn store() -> (Persistence, TestStore) {
    let test = TestStore::new(test_clock(TEST_EPOCH_MS)).await;
    (Persistence::new(test.store.clone()), test)
}

async fn put_job(p: &Persistence, job: PressPodsJob) {
    p.store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
}

async fn put_episode(p: &Persistence, episode: PressPodsEpisode) {
    p.store()
        .write(move |tx| tx.upsert(&episode, UpsertOpts::default()))
        .await
        .unwrap();
}

// selectDueJobs

#[test]
fn selects_queued_jobs_that_are_due() {
    assert_eq!(select_due_jobs(vec![job("j1")], NOW).len(), 1);
}

#[test]
fn excludes_queued_jobs_with_a_future_next_attempt_at() {
    let j = PressPodsJob {
        next_attempt_at: NOW + 60_000,
        ..job("j1")
    };
    assert!(select_due_jobs(vec![j], NOW).is_empty());
}

#[test]
fn reclaims_stale_processing_claims() {
    let j = PressPodsJob {
        status: JobStatus::Processing,
        claimed_at: Some(NOW - STALE_CLAIM_MS - 1),
        ..job("j1")
    };
    assert_eq!(select_due_jobs(vec![j], NOW).len(), 1);
}

#[test]
fn leaves_fresh_processing_claims_alone() {
    let j = PressPodsJob {
        status: JobStatus::Processing,
        claimed_at: Some(NOW - 60_000),
        ..job("j1")
    };
    assert!(select_due_jobs(vec![j], NOW).is_empty());
}

#[test]
fn excludes_failed_jobs() {
    let j = PressPodsJob {
        status: JobStatus::Failed,
        ..job("j1")
    };
    assert!(select_due_jobs(vec![j], NOW).is_empty());
}

#[test]
fn orders_by_submission_time() {
    let due = select_due_jobs(
        vec![
            PressPodsJob {
                created_at: NOW - 1000,
                ..job("newer")
            },
            PressPodsJob {
                created_at: NOW - 2000,
                ..job("older")
            },
        ],
        NOW,
    );
    let ids: Vec<&str> = due.iter().map(|j| j.job_id.as_str()).collect();
    assert_eq!(ids, ["older", "newer"]);
}

// retryDelayMs

#[test]
fn doubles_per_attempt() {
    assert_eq!(retry_delay_ms(1), 60_000);
    assert_eq!(retry_delay_ms(2), 120_000);
    assert_eq!(retry_delay_ms(3), 240_000);
}

// recordJobFailure

#[tokio::test]
async fn requeues_a_retryable_failure_with_backoff() {
    let (p, _s) = store().await;
    put_job(&p, job("r1")).await;
    let updated = p
        .record_job_failure(&job("r1"), "boom", true)
        .await
        .unwrap();
    assert_eq!(updated.status, JobStatus::Queued);
    assert_eq!(updated.attempts, 1);
    // The test clock follows real time from TEST_EPOCH_MS.
    assert!((TEST_EPOCH_MS + 60_000..TEST_EPOCH_MS + 70_000).contains(&updated.next_attempt_at));
    assert_eq!(updated.last_error.as_deref(), Some("boom"));
    assert_eq!(p.get_job("r1").await.unwrap(), Some(updated));
}

#[tokio::test]
async fn fails_permanently_on_a_non_retryable_error() {
    let (p, _s) = store().await;
    put_job(&p, job("r2")).await;
    let updated = p
        .record_job_failure(&job("r2"), "bad article", false)
        .await
        .unwrap();
    assert_eq!(updated.status, JobStatus::Failed);
}

#[tokio::test]
async fn fails_permanently_once_attempts_are_exhausted() {
    let (p, _s) = store().await;
    let exhausted = PressPodsJob {
        attempts: MAX_JOB_ATTEMPTS - 1,
        ..job("r3")
    };
    put_job(&p, exhausted.clone()).await;
    let updated = p
        .record_job_failure(&exhausted, "still broken", true)
        .await
        .unwrap();
    assert_eq!(updated.status, JobStatus::Failed);
    assert_eq!(updated.attempts, MAX_JOB_ATTEMPTS);
}

#[tokio::test]
async fn does_not_resurrect_a_concurrently_deleted_job() {
    let (p, _s) = store().await;
    p.record_job_failure(&job("gone"), "boom", true)
        .await
        .unwrap();
    assert_eq!(p.get_job("gone").await.unwrap(), None);
}

#[tokio::test]
async fn keeps_fields_written_since_selection() {
    let (p, _s) = store().await;
    put_job(&p, job("claimed")).await;
    p.claim_job("claimed", Some("PressPods:run-1".into()))
        .await
        .unwrap();
    let updated = p
        .record_job_failure(&job("claimed"), "boom", true)
        .await
        .unwrap();
    assert_eq!(updated.last_run_id.as_deref(), Some("PressPods:run-1"));
    assert_eq!(updated.claimed_at, None);
}

// requeueJobNow

#[tokio::test]
async fn requeues_a_failed_job_immediately() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            status: JobStatus::Failed,
            ..job("f1")
        },
    )
    .await;
    let updated = p.requeue_job_now("f1").await.unwrap().unwrap();
    assert_eq!(updated.status, JobStatus::Queued);
    assert_eq!(updated.next_attempt_at, 0);
}

#[tokio::test]
async fn refuses_non_failed_jobs() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            status: JobStatus::Processing,
            ..job("q1")
        },
    )
    .await;
    assert_eq!(p.requeue_job_now("q1").await.unwrap(), None);
}

#[tokio::test]
async fn resets_the_attempt_budget_so_an_exhausted_job_gets_a_fresh_retry_cycle() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            status: JobStatus::Failed,
            attempts: MAX_JOB_ATTEMPTS,
            last_error: Some("boom".into()),
            ..job("f2")
        },
    )
    .await;
    let updated = p.requeue_job_now("f2").await.unwrap().unwrap();
    assert_eq!(updated.attempts, 0);
    assert_eq!(updated.last_error, None);
}

// findEpisodeForJob

#[tokio::test]
async fn finds_an_episode_created_after_the_job_was_submitted() {
    let (p, _s) = store().await;
    let row = common::episode("https://example.com/a", NOW);
    put_episode(&p, row.clone()).await;
    let found = p
        .find_episode_for_job(&PressPodsJob {
            created_at: NOW - 1000,
            ..job("j")
        })
        .await
        .unwrap();
    assert_eq!(found.map(|e| e.episode_id), Some(row.episode_id));
}

#[tokio::test]
async fn ignores_older_episodes_for_the_same_url() {
    let (p, _s) = store().await;
    put_episode(&p, common::episode("https://example.com/a", NOW - 60_000)).await;
    let found = p
        .find_episode_for_job(&PressPodsJob {
            created_at: NOW - 1000,
            ..job("j")
        })
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn matches_on_canonical_identity_despite_tracking_param_differences() {
    let (p, _s) = store().await;
    let row = common::episode("https://example.com/story?utm_source=rss", NOW);
    put_episode(&p, row.clone()).await;
    let found = p
        .find_episode_for_job(&PressPodsJob {
            created_at: NOW - 1000,
            url: "https://example.com/story?ref=twitter".into(),
            ..job("j")
        })
        .await
        .unwrap();
    assert_eq!(found.map(|e| e.episode_id), Some(row.episode_id));
}

// PressPodsPersistence episode decoding

fn diagnostic_episode() -> PressPodsEpisode {
    let mut detail_cents = IndexMap::new();
    detail_cents.insert("metadata".to_owned(), 1.2);
    let mut detail_tokens = IndexMap::new();
    detail_tokens.insert(
        "metadata".to_owned(),
        TokenCounts {
            input: 100.0,
            output: 20.0,
            extra: Default::default(),
        },
    );
    let mut detail_chars = IndexMap::new();
    detail_chars.insert("speech".to_owned(), 1000.0);
    PressPodsEpisode {
        title: "Persisted episode".into(),
        article_url: "https://decode.example/article".into(),
        content: "Narration".into(),
        file_bytes: 100,
        chapters: Some(vec![Chapter::new(1.5, "Opening")]),
        chunks: Some(vec![ChunkStat {
            index: 0,
            section_index: 1,
            section_title: Some("Lead".into()),
            text: "Narration".into(),
            char_count: 9,
            duration_seconds: 2.5,
            start_time_seconds: 1.5,
            sec_per_char: 0.27,
            attempts: 2,
            coverage: Some(0.98),
            word_ratio: Some(1.0),
            expected_words: Some(1.0),
            resplit: Some(true),
            resplit_depth: Some(1),
            extra: Default::default(),
        }]),
        retriever_attempts: Some(vec![
            RetrieverAttempt::Success {
                name: "readability".into(),
                content_rating: 9.0,
                text_chars: 1000,
                extra: Default::default(),
            },
            RetrieverAttempt::Failure {
                name: "fetch".into(),
                error: "HTTP 500".into(),
                extra: Default::default(),
            },
        ]),
        costs: Some(Costs {
            llm_cents: 1.2,
            tts_cents: 3.4,
            detail_cents,
            detail_tokens,
            detail_chars,
            extra: Default::default(),
        }),
        ..common::episode("https://decode.example/article", NOW)
    }
}

#[tokio::test]
async fn decodes_every_nested_persisted_diagnostic_field() {
    let (p, _s) = store().await;
    let row = diagnostic_episode();
    put_episode(&p, row.clone()).await;
    assert_eq!(p.get_episode(&row.episode_id).await.unwrap(), Some(row));
}

/// Writes `row` with `field` replaced by `malformed` (raw, bypassing the model).
async fn put_malformed(p: &Persistence, id: &str, field: &str, malformed: serde_json::Value) {
    let mut value = to_value(&PressPodsEpisode {
        episode_id: id.into(),
        ..diagnostic_episode()
    })
    .unwrap();
    let malformed: JsValue = omni_store::cbor::to_value(&malformed).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert(field.into(), malformed);
    let pk = format!("$press-pods-episode#s{}:{id}", id.len());
    p.store()
        .write(move |tx| {
            tx.upsert_doc(
                &pk,
                &value,
                omni_store::DocMeta {
                    entity: Some("press-pods-episode".into()),
                    version: 0,
                    expires_at: None,
                    updated_at: Some(tx.now_ms()),
                },
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn rejects_malformed_persisted_field() {
    let cases = [
        (
            "chapters",
            serde_json::json!([{ "startTimeSeconds": "soon", "title": "Opening" }]),
        ),
        (
            "chunks",
            serde_json::json!([{
                "index": 0, "sectionIndex": 0, "charCount": 9, "durationSeconds": 2,
                "startTimeSeconds": 0, "secPerChar": 0.2, "attempts": 1
            }]),
        ),
        (
            "retrieverAttempts",
            serde_json::json!([{ "name": "fetch", "success": true, "textChars": 100 }]),
        ),
        (
            "costs",
            serde_json::json!({
                "llmCents": 1, "ttsCents": 2, "detailCents": {},
                "detailTokens": { "metadata": { "input": 10, "output": "twenty" } },
                "detailChars": {}
            }),
        ),
    ];
    let (p, _s) = store().await;
    for (field, malformed) in cases {
        let id = format!("malformed-{field}");
        put_malformed(&p, &id, field, malformed).await;
        let error = p.get_episode(&id).await.unwrap_err();
        assert!(
            matches!(&error, PressPodsError::InvalidData { operation, .. } if operation == "decode PressPods episode"),
            "{field}: {error}"
        );
    }
}

// URL-based dedup lookups

const URL_A: &str = "https://dedup.example/piece?utm_source=x";
const NORM: &str = "https://dedup.example/piece";

#[tokio::test]
async fn finds_a_queued_or_processing_job_by_canonical_url() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            url: URL_A.into(),
            status: JobStatus::Processing,
            ..job("active")
        },
    )
    .await;
    assert_eq!(
        p.find_active_job_by_normalized_url(NORM)
            .await
            .unwrap()
            .map(|j| j.job_id)
            .as_deref(),
        Some("active")
    );
    assert!(
        p.find_failed_job_by_normalized_url(NORM)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn finds_a_failed_job_by_canonical_url() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            url: URL_A.into(),
            status: JobStatus::Failed,
            ..job("failed")
        },
    )
    .await;
    assert_eq!(
        p.find_failed_job_by_normalized_url(NORM)
            .await
            .unwrap()
            .map(|j| j.job_id)
            .as_deref(),
        Some("failed")
    );
    assert!(
        p.find_active_job_by_normalized_url(NORM)
            .await
            .unwrap()
            .is_none()
    );
}

// reclaimProcessingJobsAtBoot

#[tokio::test]
async fn makes_orphaned_processing_claims_immediately_reclaimable() {
    let (p, _s) = store().await;
    put_job(
        &p,
        PressPodsJob {
            status: JobStatus::Processing,
            claimed_at: Some(TEST_EPOCH_MS),
            ..job("p1")
        },
    )
    .await;
    put_job(&p, job("q2")).await;
    assert_eq!(p.reclaim_processing_jobs_at_boot().await.unwrap(), 1);
    let p1 = p.get_job("p1").await.unwrap().unwrap();
    assert_eq!(p1.claimed_at, Some(0));
    assert_eq!(
        p.get_job("q2").await.unwrap().unwrap().status,
        JobStatus::Queued
    );
    assert_eq!(select_due_jobs(vec![p1], TEST_EPOCH_MS).len(), 1);
}

// deleteEpisodesByNormalizedUrlExcept

#[tokio::test]
async fn replaces_older_episodes_sharing_a_canonical_url_keeping_the_newest() {
    let (p, _s) = store().await;
    let older = common::episode("https://replace.example/x?utm_source=a", NOW - 1000);
    let newer = common::episode("https://replace.example/x?ref=b", NOW);
    put_episode(&p, older.clone()).await;
    put_episode(&p, newer.clone()).await;
    let removed = p
        .delete_episodes_by_normalized_url_except("https://replace.example/x", &newer.episode_id)
        .await
        .unwrap();
    let ids: Vec<String> = removed.into_iter().map(|e| e.episode_id).collect();
    assert_eq!(ids, std::slice::from_ref(&older.episode_id));
    assert!(p.get_episode(&older.episode_id).await.unwrap().is_none());
    assert!(p.get_episode(&newer.episode_id).await.unwrap().is_some());
}

#[tokio::test]
async fn enqueued_jobs_carry_their_canonical_url() {
    let (p, store) = store().await;
    let job = p
        .enqueue_episode_job("https://www.example.com/a?utm_source=x")
        .await
        .unwrap();
    assert_eq!(job.normalized_url.as_deref(), Some("https://example.com/a"));
    assert!((TEST_EPOCH_MS..TEST_EPOCH_MS + 10_000).contains(&job.created_at));
    assert_eq!(job.job_id.len(), 22);
    let keys = store
        .store
        .read(|docs| docs.get_keys_by_prefix("$press-pods-job#"))
        .await
        .unwrap();
    assert_eq!(keys, [format!("$press-pods-job#s22:{}", job.job_id)]);
}
