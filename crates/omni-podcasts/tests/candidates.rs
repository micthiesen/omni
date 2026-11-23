//! Podcast candidate gathering over a scripted [`ShowDirectory`].
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeAccount, FakeDirectory, feed_episode};
use omni_podcasts::account::PodcastSearchResult;
use omni_podcasts::candidates::{
    pick_best_by_title, podcast_index_to_candidate, resolve_candidates,
};
use omni_podcasts::itunes::ItunesShow;
use omni_podcasts::podcastindex::PodcastIndexEpisode;
use omni_podcasts::types::DiscoveredEpisode;

fn discovered(episode_title: &str) -> DiscoveredEpisode {
    DiscoveredEpisode {
        show_title: "Show".into(),
        episode_title: episode_title.into(),
        context: "reddit".into(),
        ..DiscoveredEpisode::default()
    }
}

#[tokio::test]
async fn falls_back_to_castro_search_when_itunes_cannot_place_the_show() {
    let directory = FakeDirectory::with_feeds(Vec::new(), vec![Ok(vec![feed_episode()])]);
    let account = Arc::new(FakeAccount {
        search: Ok(vec![PodcastSearchResult {
            client_id: "c1".into(),
            title: "Show".into(),
            feed_url: "https://feeds/show".into(),
            itunes_id: Some(42),
            artwork_url: Some("https://art".into()),
            ..PodcastSearchResult::default()
        }]),
        ..FakeAccount::default()
    });
    let result = resolve_candidates(
        &[discovered("Ep")],
        Some(account.as_ref()),
        &directory,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].show_id, "itunes:42");
    assert_eq!(result[0].feed_url, "https://feeds/show");
    assert_eq!(result[0].itunes_id, Some(42));
    assert_eq!(result[0].media_url.as_deref(), Some("https://cdn/x.mp3"));
    assert!(result[0].show_genres.is_empty());
    assert_eq!(account.calls(), vec!["search_podcasts:Show"]);
}

#[tokio::test]
async fn drops_the_candidate_when_itunes_misses_and_no_account_is_available() {
    let directory = FakeDirectory::default();
    let result = resolve_candidates(&[discovered("Ep")], None, &directory, None)
        .await
        .unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn isolates_per_item_failures_and_dedupes_by_episode_id() {
    let directory = FakeDirectory::with_feeds(
        vec![ItunesShow {
            itunes_id: 7,
            title: "Show".into(),
            feed_url: Some("https://feeds/show".into()),
            artwork_url: None,
            genres: vec!["News".into()],
        }],
        // First resolves; second throws; third duplicates the first.
        vec![
            Ok(vec![feed_episode()]),
            Err("feed down".into()),
            Ok(vec![feed_episode()]),
        ],
    );
    let result = resolve_candidates(
        &[discovered("Ep"), discovered("Ep"), discovered("Ep")],
        None,
        &directory,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].show_id, "itunes:7");
}

struct Show {
    title: &'static str,
    feed_url: &'static str,
}

const SHOWS: [Show; 3] = [
    Show {
        title: "The Gray Area with Sean Illing",
        feed_url: "a",
    },
    Show {
        title: "Past Present Future",
        feed_url: "b",
    },
    Show {
        title: "Very Bad Wizards",
        feed_url: "c",
    },
];

fn pick(query: &str) -> Option<&'static str> {
    pick_best_by_title(&SHOWS, query, |s| s.title).map(|s| s.feed_url)
}

#[test]
fn prefers_an_exact_normalized_match() {
    assert_eq!(pick("very bad wizards!"), Some("c"));
}

#[test]
fn falls_back_to_containment_in_either_direction() {
    assert_eq!(pick("The Gray Area"), Some("a"));
    assert_eq!(pick("Past, Present & Future"), Some("b"));
}

#[test]
fn returns_undefined_when_nothing_matches() {
    assert_eq!(pick("Hardcore History"), None);
}

fn pi_episode() -> PodcastIndexEpisode {
    PodcastIndexEpisode {
        title: "The Guest Episode".into(),
        feed_title: "Some Show".into(),
        feed_url: "https://feeds.example.com/show".into(),
        feed_itunes_id: Some(42),
        guid: "guid-abc".into(),
        enclosure_url: "https://cdn/audio.mp3".into(),
        episode_url: Some("https://show/ep".into()),
        published_at: 1_700_000_000_000,
        duration_minutes: Some(55),
        description: "A conversation.".into(),
        artwork_url: Some("https://art".into()),
    }
}

#[test]
fn maps_a_pi_episode_to_a_candidate_tagged_with_the_voice() {
    let c = podcast_index_to_candidate(&pi_episode(), "Jesse Singal").unwrap();
    assert_eq!(c.show_id, "itunes:42");
    assert_eq!(c.episode_id, "itunes:42#guid-abc");
    assert_eq!(c.show_title, "Some Show");
    assert_eq!(c.feed_url, "https://feeds.example.com/show");
    assert_eq!(c.media_url.as_deref(), Some("https://cdn/audio.mp3"));
    assert_eq!(c.matched_voices, Some(vec!["Jesse Singal".to_owned()]));
    assert_eq!(c.discovered_via, "guest: Jesse Singal (Podcast Index)");
}

#[test]
fn falls_back_to_a_feed_based_show_id_when_no_itunes_id() {
    let episode = PodcastIndexEpisode {
        feed_itunes_id: None,
        ..pi_episode()
    };
    let c = podcast_index_to_candidate(&episode, "X").unwrap();
    assert_eq!(c.show_id, "feed:feeds.example.com/show");
}
