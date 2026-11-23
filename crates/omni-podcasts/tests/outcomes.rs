//! Podcast recommendation outcomes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{DAY, NOW, rec};
use omni_podcasts::account::ListenedEpisode;
use omni_podcasts::outcomes::{ABANDONED_INACTIVITY_MS, IGNORE_WINDOW_MS, decide_episode_outcomes};
use omni_podcasts::persistence::{PodcastRecommendationData, PodcastRecommendationStatus};

fn open_rec() -> PodcastRecommendationData {
    PodcastRecommendationData {
        published_at: NOW - 10 * DAY,
        run_date: "2026-07-06".into(),
        recommended_at: NOW - 10 * DAY,
        notified_at: Some(NOW - 10 * DAY),
        ..rec()
    }
}

fn listened() -> ListenedEpisode {
    ListenedEpisode {
        show_title: "The Gray Area".into(),
        episode_title: "What is consciousness?".into(),
        episode_guid: Some("guid-1".into()),
        listened_at: NOW - DAY,
        ..ListenedEpisode::default()
    }
}

#[test]
fn labels_listened_at_above_the_completion_threshold() {
    let changes = decide_episode_outcomes(
        &[open_rec()],
        &[ListenedEpisode {
            completion: Some(0.95),
            ..listened()
        }],
        NOW,
    );
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Listened);
}

#[test]
fn counts_a_playback_event_without_completion_data_as_listened() {
    let changes = decide_episode_outcomes(&[open_rec()], &[listened()], NOW);
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Listened);
    assert_eq!(changes[0].reason, "playback recorded");
}

#[test]
fn matches_by_normalized_titles_when_the_guid_differs() {
    let changes = decide_episode_outcomes(
        &[open_rec()],
        &[ListenedEpisode {
            episode_guid: Some("castro-internal-id".into()),
            show_title: "the gray area".into(),
            episode_title: "What Is Consciousness?!".into(),
            completion: Some(0.9),
            ..listened()
        }],
        NOW,
    );
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Listened);
    assert_eq!(changes[0].reason, "completion=0.90");
}

#[test]
fn matches_a_rewritten_guid_by_media_url_before_title_fallback() {
    let changes = decide_episode_outcomes(
        &[PodcastRecommendationData {
            episode_title: "An Ambiguous Episode Title".into(),
            media_url: Some("https://cdn.example.com/audio/episode.mp3?source=rss".into()),
            ..open_rec()
        }],
        &[ListenedEpisode {
            episode_guid: Some("castro-rewritten-guid".into()),
            media_url: Some("http://cdn.example.com/audio/episode.mp3?source=castro".into()),
            show_title: "Different display title".into(),
            episode_title: "Different episode title".into(),
            completion: Some(0.9),
            ..listened()
        }],
        NOW,
    );
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Listened);
}

#[test]
fn labels_abandoned_after_stalling_below_the_threshold() {
    let changes = decide_episode_outcomes(
        &[PodcastRecommendationData {
            recommended_at: NOW - 20 * DAY,
            notified_at: Some(NOW - 20 * DAY),
            ..open_rec()
        }],
        &[ListenedEpisode {
            completion: Some(0.3),
            listened_at: NOW - ABANDONED_INACTIVITY_MS - DAY,
            ..listened()
        }],
        NOW,
    );
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Abandoned);
    assert_eq!(changes[0].reason, "stalled at 30%");
}

#[test]
fn leaves_a_recently_started_episode_open() {
    let changes = decide_episode_outcomes(
        &[open_rec()],
        &[ListenedEpisode {
            completion: Some(0.3),
            listened_at: NOW - DAY,
            ..listened()
        }],
        NOW,
    );
    assert!(changes.is_empty());
}

#[test]
fn ignores_playback_that_predates_delivery() {
    let changes = decide_episode_outcomes(
        &[PodcastRecommendationData {
            notified_at: Some(NOW - DAY),
            ..open_rec()
        }],
        &[ListenedEpisode {
            completion: Some(1.0),
            listened_at: NOW - 2 * DAY,
            ..listened()
        }],
        NOW,
    );
    assert!(changes.is_empty());
}

#[test]
fn labels_ignored_after_the_window_with_no_engagement() {
    let old = PodcastRecommendationData {
        recommended_at: NOW - IGNORE_WINDOW_MS - DAY,
        notified_at: Some(NOW - IGNORE_WINDOW_MS - DAY),
        ..open_rec()
    };
    let changes = decide_episode_outcomes(&[old], &[], NOW);
    assert_eq!(changes[0].status, PodcastRecommendationStatus::Ignored);
}

#[test]
fn only_labels_notified_rows() {
    let pending = PodcastRecommendationData {
        status: PodcastRecommendationStatus::Pending,
        ..open_rec()
    };
    let changes = decide_episode_outcomes(
        &[pending],
        &[ListenedEpisode {
            completion: Some(1.0),
            ..listened()
        }],
        NOW,
    );
    assert!(changes.is_empty());
}
