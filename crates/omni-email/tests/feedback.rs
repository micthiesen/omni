//! Email triage feedback.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_core::clock::TestClock;
use omni_email::activity::EmailPipelineName;
use omni_email::feedback::{self, EmailFeedbackData, EmailFeedbackVerdict, NewFeedback};
use omni_email::sender_rules::RuleTarget;
use omni_store::Store;

use common::{NOW, store_at};

fn entry(email_id: &str) -> NewFeedback {
    NewFeedback {
        pipeline: EmailPipelineName::ParcelTracker,
        email_id: email_id.to_owned(),
        subject: "Your order shipped".to_owned(),
        from: "orders@shop.com".to_owned(),
        verdict: EmailFeedbackVerdict::NotRelevant,
        note: None,
    }
}

async fn record(store: &Store, entry: NewFeedback) -> EmailFeedbackData {
    feedback::record(store, entry).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn derives_the_activity_id_from_pipeline_and_email_id() {
    let (store, _clock) = store_at(NOW).await;
    let row = record(&store.store, entry("e1")).await;
    assert_eq!(row.activity_id, "ParcelTracker#e1");
    assert!(row.created_at > 0);
}

#[tokio::test(start_paused = true)]
async fn upserts_re_recording_the_same_email_overwrites_the_verdict() {
    let (store, _clock) = store_at(NOW).await;
    record(&store.store, entry("e1")).await;
    record(
        &store.store,
        NewFeedback {
            verdict: EmailFeedbackVerdict::Missed,
            ..entry("e1")
        },
    )
    .await;
    let rows = feedback::list(&store.store, None, 50).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].verdict, EmailFeedbackVerdict::Missed);
}

#[tokio::test(start_paused = true)]
async fn returns_newest_first_filtered_by_pipeline_capped_by_limit() {
    let (store, clock): (_, Arc<TestClock>) = store_at(1_000).await;
    record(&store.store, entry("old")).await;
    clock.set(2_000);
    record(&store.store, entry("new")).await;
    clock.set(3_000);
    record(
        &store.store,
        NewFeedback {
            pipeline: EmailPipelineName::CalendarEvents,
            ..entry("cal")
        },
    )
    .await;

    let parcel = feedback::list(&store.store, Some(EmailPipelineName::ParcelTracker), 50)
        .await
        .unwrap();
    let ids: Vec<&str> = parcel.iter().map(|f| f.email_id.as_str()).collect();
    assert_eq!(ids, ["new", "old"]);
    assert_eq!(
        feedback::list(&store.store, Some(EmailPipelineName::CalendarEvents), 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        feedback::list(&store.store, None, 50).await.unwrap().len(),
        3
    );
    assert_eq!(
        feedback::list(&store.store, None, 2).await.unwrap().len(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn reports_whether_the_row_existed() {
    let (store, _clock) = store_at(NOW).await;
    let row = record(&store.store, entry("e1")).await;
    assert!(
        feedback::delete(&store.store, &row.activity_id)
            .await
            .unwrap()
    );
    assert!(
        !feedback::delete(&store.store, &row.activity_id)
            .await
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn returns_an_empty_string_when_there_is_no_feedback() {
    let (store, _clock) = store_at(NOW).await;
    assert_eq!(
        feedback::format_digest(&store.store, RuleTarget::Parcel, 15)
            .await
            .unwrap(),
        ""
    );
}

#[tokio::test(start_paused = true)]
async fn formats_not_relevant_and_missed_corrections_for_the_pipeline() {
    let (store, _clock) = store_at(NOW).await;
    record(&store.store, entry("e1")).await;
    record(
        &store.store,
        NewFeedback {
            subject: "Package ready".to_owned(),
            from: "ship@store.com".to_owned(),
            verdict: EmailFeedbackVerdict::Missed,
            note: Some("has a tracking link".to_owned()),
            ..entry("e2")
        },
    )
    .await;
    record(
        &store.store,
        NewFeedback {
            pipeline: EmailPipelineName::CalendarEvents,
            verdict: EmailFeedbackVerdict::Missed,
            ..entry("e3")
        },
    )
    .await;

    let digest = feedback::format_digest(&store.store, RuleTarget::Parcel, 15)
        .await
        .unwrap();
    assert!(
        digest.contains("- \"Your order shipped\" from orders@shop.com: user marked NOT relevant")
    );
    assert!(digest.contains(
        "- \"Package ready\" from ship@store.com: user marked as MISSED (should have been processed) (note: has a tracking link)"
    ));
    // Calendar feedback stays out of the parcel digest.
    assert_eq!(digest.split('\n').count(), 2);
}

#[tokio::test(start_paused = true)]
async fn caps_the_digest_at_the_given_limit() {
    let (store, _clock) = store_at(NOW).await;
    for i in 0..5 {
        record(&store.store, entry(&format!("e{i}"))).await;
    }
    assert_eq!(
        feedback::format_digest(&store.store, RuleTarget::Parcel, 3)
            .await
            .unwrap()
            .split('\n')
            .count(),
        3
    );
}
