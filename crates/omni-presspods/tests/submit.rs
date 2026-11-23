//! Article submission, plus the dedup paths of the shared
//! submission flow.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{HarnessOptions, PrivateGuard, harness};
use omni_presspods::model::JobStatus;

#[tokio::test]
async fn rejects_dns_private_hosts_before_durable_enqueue_or_bookmarking() {
    let h = harness(HarnessOptions {
        guard: Arc::new(PrivateGuard),
        ..HarnessOptions::default()
    })
    .await;
    let error = h
        .service
        .submit_episode_url("https://internal.example/article")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("host resolves to a private address")
    );
    assert!(
        h.service
            .persistence()
            .get_all_jobs()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.bookmarks.0.lock().unwrap().is_empty());
    assert_eq!(h.kicks.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn enqueues_bookmarks_and_kicks_a_new_submission() {
    let h = harness(HarnessOptions::default()).await;
    let job = h
        .service
        .submit_episode_url("https://example.com/a?utm_source=x")
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Queued);
    assert_eq!(job.url, "https://example.com/a?utm_source=x");
    assert_eq!(
        *h.bookmarks.0.lock().unwrap(),
        ["https://example.com/a?utm_source=x"]
    );
    assert_eq!(h.kicks.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn joins_an_in_flight_job_for_the_same_canonical_url() {
    let h = harness(HarnessOptions::default()).await;
    let first = h
        .service
        .submit_episode_url("https://example.com/a")
        .await
        .unwrap();
    let again = h
        .service
        .submit_episode_url("http://www.example.com/a/?ref=x")
        .await
        .unwrap();
    assert_eq!(again.job_id, first.job_id);
    assert_eq!(
        h.service.persistence().get_all_jobs().await.unwrap().len(),
        1
    );
    assert_eq!(h.bookmarks.0.lock().unwrap().len(), 1);
    assert_eq!(h.kicks.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn requeues_a_failed_job_instead_of_stacking_a_new_one() {
    let h = harness(HarnessOptions::default()).await;
    let first = h
        .service
        .submit_episode_url("https://example.com/a")
        .await
        .unwrap();
    let persistence = h.service.persistence();
    persistence
        .record_job_failure(&first, "bad", false)
        .await
        .unwrap();
    let again = h
        .service
        .submit_episode_url("https://example.com/a")
        .await
        .unwrap();
    assert_eq!(again.job_id, first.job_id);
    assert_eq!(again.status, JobStatus::Queued);
    assert_eq!(again.attempts, 0);
    assert_eq!(persistence.get_all_jobs().await.unwrap().len(), 1);
}
