//! Parcel pipeline reliability and the submission flow (dedup,
//! near-duplicates, ranked fallbacks, terminal rejections, transient
//! failures) against the real store.
//!
//! The filter, activity, retry and persistence modules are real, with the
//! candidate admitted by a stub triage and a post-acceptance crash injected as
//! a SQLite trigger on the confirmation write. That crash is a typed
//! persistence failure, so the email is also recorded as `error` and enqueued
//! for retry.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use omni_email::activity::{self, AdmitTier, EmailActivityOutcome, LlmCost};
use omni_email::retry;
use omni_parcel::error::ParcelError;
use omni_parcel::parcel_api::SubmitResult;
use omni_parcel::persistence::{self, DeliveryAttempt, SubmissionStatus};
use tokio::sync::Notify;

use common::{
    FakeExtractor, FakeSubmitter, always_extract, break_updates, deliveries, harness, heal,
    shipment, ups_only,
};

#[tokio::test]
async fn durably_queues_admitted_email_after_transient_extraction_failure() {
    let extractor = Arc::new(FakeExtractor(Box::new(|_| {
        Box::pin(async {
            Err(ParcelError::Extraction {
                message: "model timeout".to_owned(),
                transient: true,
            })
        })
    })));
    let h = harness(
        extractor,
        FakeSubmitter::new(&[SubmitResult::Success]),
        ups_only(),
    )
    .await;
    h.pipeline
        .handle_emails(&[shipment("mail-2")])
        .await
        .unwrap();
    let row = retry::get(&h.store.store, "ParcelTracker#mail-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.pipeline, "ParcelTracker");
    assert_eq!(row.email_id, "mail-2");
    assert_eq!(row.reason, "Parcel extraction failed: model timeout");
    let activity = activity::get(&h.store.store, "ParcelTracker#mail-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(activity.outcome, EmailActivityOutcome::Error);
    assert_eq!(activity.admit_tier, Some(AdmitTier::Triage));
    assert_eq!(activity.cost_cents, LlmCost::Cents(0.25));
}

#[tokio::test]
async fn parks_emails_whose_extraction_request_was_rejected_and_alerts_once() {
    let extractor = Arc::new(FakeExtractor(Box::new(|_| {
        Box::pin(async {
            let message = "provider error 400: Invalid schema for response_format 'delivery_extraction': 'oneOf' is not permitted.";
            Err(ParcelError::SystemicExtraction {
                message: message.to_owned(),
                signature: omni_email::systemic::Signature::from_text(message),
            })
        })
    })));
    let h = harness(
        extractor,
        FakeSubmitter::new(&[SubmitResult::Success]),
        ups_only(),
    )
    .await;
    h.pipeline
        .handle_emails(&[shipment("mail-a"), shipment("mail-b")])
        .await
        .unwrap();
    let build = omni_email::systemic::current_build().await;
    for id in ["mail-a", "mail-b"] {
        let row = retry::get(&h.store.store, &format!("ParcelTracker#{id}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.awaiting_build.as_deref(), Some(build.as_str()));
        assert_eq!(row.attempts, 0);
        let activity = activity::get(&h.store.store, &format!("ParcelTracker#{id}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(activity.outcome, EmailActivityOutcome::Error);
    }
    let pushes = h.pushover.recorded();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0].message.title.as_deref(),
        Some("Parcel extraction failing")
    );
}

#[tokio::test]
async fn preserves_interruption_while_delivery_extraction_is_in_progress() {
    let started = Arc::new(Notify::new());
    let signal = started.clone();
    let extractor = Arc::new(FakeExtractor(Box::new(move |_| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.notify_one();
            std::future::pending().await
        })
    })));
    let h = harness(
        extractor,
        FakeSubmitter::new(&[SubmitResult::Success]),
        ups_only(),
    )
    .await;
    let emails = [shipment("mail-interrupted")];
    {
        let handling = h.pipeline.handle_emails(&emails);
        tokio::pin!(handling);
        tokio::select! {
            _ = &mut handling => panic!("extraction never completes"),
            () = started.notified() => {}
        }
    }
    assert!(
        activity::get(&h.store.store, "ParcelTracker#mail-interrupted")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn persists_a_replayable_reservation_before_parcel_and_retries_an_unacknowledged_request() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Success]);
    let h = harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    let mail = shipment("mail-replay");

    break_updates(&h.store.store, "parcel-submitted-delivery").await;
    h.pipeline
        .handle_emails(std::slice::from_ref(&mail))
        .await
        .unwrap();
    heal(&h.store.store).await;
    let reservation = persistence::get(&h.store.store, "1Z999AA10123456784")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.carrier_code, "ups");
    assert_eq!(reservation.status, Some(SubmissionStatus::Pending));
    assert_eq!(reservation.attempts, Some(1));
    assert_eq!(submitter.calls().len(), 1);

    h.pipeline
        .handle_emails(std::slice::from_ref(&mail))
        .await
        .unwrap();
    assert_eq!(submitter.calls().len(), 2);
    let confirmed = persistence::get(&h.store.store, "1Z999AA10123456784")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.status, Some(SubmissionStatus::Submitted));
    assert_eq!(confirmed.attempts, Some(2));
}

#[tokio::test]
async fn submits_records_processed_and_dedups_resubmission() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Success]);
    let h = harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    h.pipeline.handle_emails(&[shipment("m1")]).await.unwrap();
    let first = activity::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.outcome, EmailActivityOutcome::Processed);
    assert_eq!(
        first.items.as_deref(),
        Some(&["1Z999AA10123456784 (ups): submitted".to_owned()][..])
    );
    assert_eq!(first.cost_cents, LlmCost::Cents(1.25));
    assert_eq!(first.admit_reason.as_deref(), Some("triage: tracking"));

    h.pipeline.handle_emails(&[shipment("m2")]).await.unwrap();
    assert_eq!(submitter.calls().len(), 1);
    let second = activity::get(&h.store.store, "ParcelTracker#m2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.outcome, EmailActivityOutcome::Processed);
    assert_eq!(
        second.items.as_deref(),
        Some(&["1Z999AA10123456784: already submitted".to_owned()][..])
    );
}

#[tokio::test]
async fn skips_near_duplicates_of_known_numbers() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Success]);
    let h = harness(
        always_extract(deliveries(&[("P52538065", &["ups"])])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    persistence::record(
        &h.store.store,
        DeliveryAttempt {
            tracking_number: "P5253806501".to_owned(),
            carrier_code: "ups".to_owned(),
            description: "Earlier".to_owned(),
            submitted_at: 1,
            email_id: "old".to_owned(),
        },
        SubmissionStatus::Submitted,
        Some(1),
    )
    .await
    .unwrap();
    h.pipeline.handle_emails(&[shipment("m1")]).await.unwrap();
    assert!(submitter.calls().is_empty());
    let row = activity::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.items.as_deref(),
        Some(&["P52538065: near-duplicate of P5253806501, skipped".to_owned()][..])
    );
}

#[tokio::test]
async fn falls_back_to_the_next_candidate_on_carrier_shaped_rejections() {
    let submitter = FakeSubmitter::new(&[
        SubmitResult::Rejected { status: 422 },
        SubmitResult::Success,
    ]);
    let h = harness(
        always_extract(deliveries(&[(
            "DCM123456789",
            &["dicom", "bogus", "canpost"],
        )])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    h.pipeline.handle_emails(&[shipment("m1")]).await.unwrap();
    let codes: Vec<String> = submitter
        .calls()
        .into_iter()
        .map(|c| c.carrier_code)
        .collect();
    assert_eq!(codes, ["dicom", "canpost"]);
    let row = persistence::get(&h.store.store, "DCM123456789")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, Some(SubmissionStatus::Submitted));
    assert_eq!(row.carrier_code, "canpost");
    assert_eq!(row.attempts, Some(2));
}

#[tokio::test]
async fn a_fully_rejected_submission_is_failed_never_processed() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Rejected { status: 401 }]);
    let h = harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups", "canpost"])])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    h.pipeline.handle_emails(&[shipment("m1")]).await.unwrap();
    assert_eq!(submitter.calls().len(), 1);
    let row = activity::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, EmailActivityOutcome::Failed);
    assert_eq!(
        row.items.as_deref(),
        Some(&["1Z999AA10123456784 (ups): rejected by Parcel (401)".to_owned()][..])
    );
    let gate = persistence::get(&h.store.store, "1Z999AA10123456784")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.status, Some(SubmissionStatus::Rejected));
}

#[tokio::test]
async fn transient_submission_failures_enqueue_a_retry_and_fail_the_item() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Error]);
    let h = harness(
        always_extract(deliveries(&[
            ("AAA11111111", &["bogus"]),
            ("1Z999AA10123456784", &["ups"]),
        ])),
        submitter.clone(),
        ups_only(),
    )
    .await;
    h.pipeline.handle_emails(&[shipment("m1")]).await.unwrap();
    let row = activity::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, EmailActivityOutcome::Failed);
    assert_eq!(
        row.items.as_deref(),
        Some(
            &[
                "AAA11111111: no valid carrier candidates".to_owned(),
                "1Z999AA10123456784 (ups): submission failed, will retry".to_owned(),
            ][..]
        )
    );
    let queued = retry::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        queued.reason,
        "Parcel submission network/5xx for 1Z999AA10123456784"
    );
    let gate = persistence::get(&h.store.store, "1Z999AA10123456784")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.status, Some(SubmissionStatus::Pending));
}

#[tokio::test]
async fn records_filtered_emails_with_the_triage_cost() {
    let h = harness(
        always_extract(deliveries(&[])),
        FakeSubmitter::new(&[SubmitResult::Success]),
        ups_only(),
    )
    .await;
    let mut npm = shipment("npm");
    npm.from = "support@npmjs.com".to_owned();
    h.pipeline
        .handle_emails(&[npm, shipment("empty")])
        .await
        .unwrap();
    let filtered = activity::get(&h.store.store, "ParcelTracker#npm")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(filtered.outcome, EmailActivityOutcome::Filtered);
    assert_eq!(filtered.detail.as_deref(), Some("blacklisted sender"));
    assert_eq!(filtered.cost_cents, LlmCost::Unpriced);
    let empty = activity::get(&h.store.store, "ParcelTracker#empty")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(empty.outcome, EmailActivityOutcome::NoMatches);
    assert_eq!(empty.detail.as_deref(), Some("no tracking numbers found"));
}

#[tokio::test]
async fn carrier_list_unavailable_fails_the_item_without_submitting() {
    let submitter = FakeSubmitter::new(&[SubmitResult::Success]);
    let h = harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        submitter.clone(),
        serde_json::json!(["not", "an", "object"]),
    )
    .await;
    tokio::time::timeout(
        Duration::from_secs(30),
        h.pipeline.handle_emails(&[shipment("m1")]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(submitter.calls().is_empty());
    let row = activity::get(&h.store.store, "ParcelTracker#m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, EmailActivityOutcome::Failed);
    assert_eq!(
        row.items.as_deref(),
        Some(&["1Z999AA10123456784: carrier list unavailable".to_owned()][..])
    );
}
