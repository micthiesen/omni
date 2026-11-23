//! Podcast recommendation persistence, plus store-backed
//! feedback and voice-cursor checks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::HashSet;

use common::{DAY, NOW, rec};
use omni_podcasts::persistence::{
    PodcastFeedback, PodcastRecommendationData, PodcastRecommendationStatus, advance_voice_cursor,
    compute_podcast_exclusions, compute_voice_batch, format_podcast_feedback_digest_from,
    insert_podcast_recommendation, next_voice_batch, set_podcast_recommendation_feedback,
};
use omni_testkit::{TestStore, test_clock};

#[test]
fn excludes_delivered_episodes_permanently_and_their_show_during_cooldown() {
    let exclusions = compute_podcast_exclusions(&[rec()], NOW);
    assert!(exclusions.episode_ids.contains("itunes:1#guid-1"));
    assert!(exclusions.show_ids.contains("itunes:1"));
}

#[test]
fn keeps_the_episode_excluded_after_the_show_cooldown_lapses() {
    let old = PodcastRecommendationData {
        recommended_at: NOW - 45 * DAY,
        ..rec()
    };
    let exclusions = compute_podcast_exclusions(&[old], NOW);
    assert!(exclusions.episode_ids.contains("itunes:1#guid-1"));
    assert!(!exclusions.show_ids.contains("itunes:1"));
}

#[test]
fn gives_failed_rows_only_a_short_retry_exclusion() {
    let failed_recent = PodcastRecommendationData {
        status: PodcastRecommendationStatus::Failed,
        recommended_at: NOW - DAY / 2,
        ..rec()
    };
    assert_eq!(
        compute_podcast_exclusions(&[failed_recent], NOW)
            .episode_ids
            .len(),
        1
    );
    let failed_old = PodcastRecommendationData {
        status: PodcastRecommendationStatus::Failed,
        recommended_at: NOW - 2 * DAY,
        ..rec()
    };
    assert!(
        compute_podcast_exclusions(&[failed_old], NOW)
            .episode_ids
            .is_empty()
    );
}

#[test]
fn excludes_not_for_me_shows_permanently() {
    let old = PodcastRecommendationData {
        recommended_at: NOW - 200 * DAY,
        feedback: Some(PodcastFeedback::NotForMe),
        feedback_at: Some(NOW - 199 * DAY),
        ..rec()
    };
    assert!(
        compute_podcast_exclusions(&[old], NOW)
            .show_ids
            .contains("itunes:1")
    );
}

#[test]
fn lets_newer_feedback_correct_an_earlier_not_for_me() {
    let bad = PodcastRecommendationData {
        recommended_at: NOW - 200 * DAY,
        feedback: Some(PodcastFeedback::NotForMe),
        feedback_at: Some(NOW - 199 * DAY),
        ..rec()
    };
    let good = PodcastRecommendationData {
        recommendation_id: "r2".into(),
        episode_id: "itunes:1#guid-2".into(),
        episode_guid: "guid-2".into(),
        recommended_at: NOW - 100 * DAY,
        feedback: Some(PodcastFeedback::GoodPick),
        feedback_at: Some(NOW - 99 * DAY),
        ..rec()
    };
    assert!(
        !compute_podcast_exclusions(&[bad, good], NOW)
            .show_ids
            .contains("itunes:1")
    );
}

#[test]
fn reports_latest_feedback_per_show_grouped_by_polarity() {
    let digest = format_podcast_feedback_digest_from(&[
        PodcastRecommendationData {
            feedback: Some(PodcastFeedback::GoodPick),
            feedback_at: Some(NOW - DAY),
            ..rec()
        },
        PodcastRecommendationData {
            recommendation_id: "r2".into(),
            show_id: "itunes:2".into(),
            show_title: "Some Grifty Show".into(),
            episode_id: "itunes:2#g".into(),
            feedback: Some(PodcastFeedback::NotForMe),
            feedback_at: Some(NOW - 2 * DAY),
            ..rec()
        },
    ]);
    assert!(digest.contains("Good picks: The Gray Area"));
    assert!(digest.contains("Not for me: Some Grifty Show"));
}

#[test]
fn handles_the_empty_case() {
    assert_eq!(
        format_podcast_feedback_digest_from(&[]),
        "No explicit podcast feedback yet."
    );
}

fn voices() -> Vec<String> {
    ["A", "B", "C", "D", "E"].map(String::from).to_vec()
}

#[test]
fn returns_all_voices_cursor_reset_when_the_list_fits_in_one_batch() {
    let batch = compute_voice_batch(&voices(), 10, 3);
    assert_eq!(batch.batch, voices());
    assert_eq!(batch.next_cursor, 0);
}

#[test]
fn returns_a_bounded_batch_and_advances_the_cursor() {
    let first = compute_voice_batch(&voices(), 2, 0);
    assert_eq!(first.batch, vec!["A", "B"]);
    assert_eq!(first.next_cursor, 2);
    let second = compute_voice_batch(&voices(), 2, 2);
    assert_eq!(second.batch, vec!["C", "D"]);
    assert_eq!(second.next_cursor, 4);
}

#[test]
fn wraps_around_and_covers_every_voice_across_runs() {
    let mut cursor = 0;
    let mut seen = HashSet::new();
    for _ in 0..3 {
        let batch = compute_voice_batch(&voices(), 2, cursor);
        seen.extend(batch.batch);
        cursor = batch.next_cursor;
    }
    assert_eq!(seen, voices().into_iter().collect());
}

#[test]
fn handles_empty_voices_and_non_positive_max() {
    assert!(compute_voice_batch(&[], 3, 0).batch.is_empty());
    assert!(compute_voice_batch(&voices(), 0, 0).batch.is_empty());
}

#[tokio::test]
async fn voice_cursor_advances_only_when_committed() {
    let store = TestStore::new(test_clock(NOW)).await;
    assert_eq!(
        next_voice_batch(&store.store, &voices(), 2).await.unwrap(),
        vec!["A", "B"]
    );
    // A discovery failure never commits, so the same batch is searched again.
    assert_eq!(
        next_voice_batch(&store.store, &voices(), 2).await.unwrap(),
        vec!["A", "B"]
    );
    advance_voice_cursor(&store.store, &voices(), 2)
        .await
        .unwrap();
    assert_eq!(
        next_voice_batch(&store.store, &voices(), 2).await.unwrap(),
        vec!["C", "D"]
    );
}

#[tokio::test]
async fn feedback_patch_keeps_other_fields_and_reports_missing_rows() {
    let store = TestStore::new(test_clock(NOW)).await;
    insert_podcast_recommendation(&store.store, rec())
        .await
        .unwrap();
    let updated =
        set_podcast_recommendation_feedback(&store.store, "r1", None, Some("loved it".into()), NOW)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(updated.feedback, None);
    assert_eq!(updated.feedback_note.as_deref(), Some("loved it"));
    assert_eq!(updated.feedback_at, Some(NOW));
    assert_eq!(updated.status, PodcastRecommendationStatus::Notified);
    assert!(
        set_podcast_recommendation_feedback(&store.store, "missing", None, None, NOW)
            .await
            .unwrap()
            .is_none()
    );
}
