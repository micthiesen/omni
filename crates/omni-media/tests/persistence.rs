//! Recommendation persistence, plus store-backed
//! feedback cases.
#![allow(clippy::expect_used)]

use crate::common;

use omni_api::media::{RecommendationFeedback, RecommendationStatus};
use omni_media::persistence::{
    FeedbackInput, ON_DECK_LIMIT, RecommendationData, compute_excluded_canonical_ids,
    format_feedback_digest_from, get_recommendation, select_on_deck, set_recommendation_feedback,
};
use omni_media::types::MediaType;
use omni_store::entity::{EntityWrite as _, UpsertOpts};

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

fn rec(
    recommendation_id: &str,
    canonical_id: &str,
    update: impl FnOnce(&mut RecommendationData),
) -> RecommendationData {
    let mut rec = RecommendationData {
        recommendation_id: recommendation_id.to_owned(),
        canonical_id: canonical_id.to_owned(),
        tmdb_id: canonical_id
            .split(':')
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        media_type: MediaType::Movie,
        title: recommendation_id.to_owned(),
        status: RecommendationStatus::Ignored,
        run_date: "2026-01-01".to_owned(),
        recommended_at: NOW - 200 * DAY,
        ..RecommendationData::default()
    };
    update(&mut rec);
    rec
}

#[test]
fn preserves_multiple_attempts_for_the_same_canonical_title_in_calculations() {
    let records = vec![
        rec("first", "tmdb:movie:1", |_| {}),
        rec("second", "tmdb:movie:1", |r| r.recommended_at = NOW - 1000),
    ];
    assert_eq!(
        records
            .iter()
            .map(|r| r.recommendation_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    assert!(compute_excluded_canonical_ids(&records, NOW).contains("tmdb:movie:1"));
}

#[test]
fn lets_the_latest_explicit_feedback_correct_an_older_negative_response() {
    let records = vec![
        rec("old", "tmdb:movie:2", |r| {
            r.feedback = Some(RecommendationFeedback::NotForMe);
            r.feedback_at = Some(NOW - 10_000);
        }),
        rec("new", "tmdb:movie:2", |r| {
            r.feedback = Some(RecommendationFeedback::GoodPick);
            r.feedback_at = Some(NOW - 5_000);
        }),
    ];
    assert!(!compute_excluded_canonical_ids(&records, NOW).contains("tmdb:movie:2"));
}

#[test]
fn uses_only_good_and_not_for_me_feedback_as_taste_evidence() {
    let digest = format_feedback_digest_from(&[
        rec("Loved It", "tmdb:movie:3", |r| {
            r.feedback = Some(RecommendationFeedback::GoodPick)
        }),
        rec("No Thanks", "tmdb:movie:4", |r| {
            r.feedback = Some(RecommendationFeedback::NotForMe)
        }),
        rec("Seen It", "tmdb:movie:5", |r| {
            r.feedback = Some(RecommendationFeedback::AlreadyWatched);
        }),
    ]);
    assert!(digest.contains("Good picks: Loved It"));
    assert!(digest.contains("Not for me: No Thanks"));
    assert!(!digest.contains("Seen It"));
}

#[test]
fn uses_a_short_retry_backoff_for_failed_acquisition_attempts() {
    let recent = rec("recent", "tmdb:movie:6", |r| {
        r.status = RecommendationStatus::Failed;
        r.recommended_at = NOW - 12 * 60 * 60 * 1000;
    });
    let old = rec("old", "tmdb:movie:7", |r| {
        r.status = RecommendationStatus::Failed;
        r.recommended_at = NOW - 2 * DAY;
    });
    let excluded = compute_excluded_canonical_ids(&[recent, old], NOW);
    assert!(excluded.contains("tmdb:movie:6"));
    assert!(!excluded.contains("tmdb:movie:7"));
}

fn notified(id: &str, age_ms: i64) -> RecommendationData {
    rec(id, &format!("tmdb:movie:{id}"), |r| {
        r.status = RecommendationStatus::Notified;
        r.recommended_at = NOW - age_ms;
    })
}

#[test]
fn excludes_pending_rows_not_yet_delivered() {
    let picks = select_on_deck(&[
        notified("a", 1000),
        rec("p", "tmdb:movie:9", |r| {
            r.status = RecommendationStatus::Pending;
            r.recommended_at = NOW;
        }),
    ]);
    assert_eq!(
        picks
            .iter()
            .map(|r| r.recommendation_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
}

#[test]
fn returns_the_newest_first_and_caps_at_the_limit() {
    let picks = select_on_deck(&[
        notified("oldest", 5000),
        notified("newest", 1000),
        notified("mid", 3000),
        notified("older", 4000),
        notified("newer", 2000),
    ]);
    assert_eq!(picks.len(), ON_DECK_LIMIT);
    assert_eq!(
        picks
            .iter()
            .map(|r| r.recommendation_id.as_str())
            .collect::<Vec<_>>(),
        vec!["newest", "newer", "mid", "older"]
    );
}

#[test]
fn includes_note_only_feedback_with_its_note() {
    let digest = format_feedback_digest_from(&[rec("Quiet One", "tmdb:movie:8", |r| {
        r.year = Some(2020);
        r.feedback_note = Some("too slow".to_owned());
    })]);
    assert_eq!(
        digest,
        "Explicit recommendation feedback:\n- Notes (no rating): Quiet One (2020) [movie] — note: \"too slow\""
    );
}

#[tokio::test]
async fn records_feedback_and_note_with_a_timestamp() {
    let h = common::Harness::new().await;
    let row = rec("rec-1", "tmdb:movie:1", |r| {
        r.status = RecommendationStatus::Notified
    });
    h.services
        .store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .expect("seed");
    let updated = set_recommendation_feedback(
        &h.services.store,
        NOW,
        "rec-1",
        FeedbackInput {
            feedback: Some(RecommendationFeedback::GoodPick),
            note: Some("great".to_owned()),
        },
    )
    .await
    .expect("feedback")
    .expect("row");
    assert_eq!(updated.feedback, Some(RecommendationFeedback::GoodPick));
    assert_eq!(updated.feedback_note.as_deref(), Some("great"));
    assert_eq!(updated.feedback_at, Some(NOW));
    let missing =
        set_recommendation_feedback(&h.services.store, NOW, "nope", FeedbackInput::default())
            .await
            .expect("feedback");
    assert!(missing.is_none());
    assert!(
        get_recommendation(&h.services.store, "rec-1")
            .await
            .expect("get")
            .is_some()
    );
}
