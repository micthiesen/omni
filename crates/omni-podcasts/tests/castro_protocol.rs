//! Castro sync protocol decoding.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_podcasts::castro::protocol::{
    CastroActionBatch, CastroActionType, CastroEpisodeSearchResult, CastroLastPlayedEventData,
    CastroPodcastSearchResult, CastroPodcastState, CastroProfileSubscription,
    CastroProgressEventData, CastroQueue, CastroQueueEventData, CastroSubscriptionResponse, decode,
    parse_castro_event_data,
};
use serde_json::{Value, json};

const EPISODE_ID: &str = "11111111-1111-4111-8111-111111111111";

fn action(id: u64, action_type: &str, event_data: Option<Value>) -> Value {
    let mut value = json!({
        "id": id,
        "episode_id": EPISODE_ID,
        "origin_event_id": format!("22222222-2222-4222-8222-{id:012}"),
        "origin_timestamp": 1_784_223_113_517_u64,
        "source": "user",
        "action_type": action_type,
    });
    if let Some(data) = event_data {
        value["event_data"] = Value::String(data.to_string());
    }
    value
}

#[test]
fn parses_the_captured_queue_next_action_pair() {
    let batch: CastroActionBatch = decode(json!({
        "actions": [
            action(1, "episode_queued", Some(json!({ "fractional_position": "ZME" }))),
            action(2, "clear_episode_new", None),
        ]
    }))
    .unwrap();
    let types: Vec<_> = batch.actions.iter().map(|a| a.action_type).collect();
    assert_eq!(
        types,
        vec![
            CastroActionType::EpisodeQueued,
            CastroActionType::ClearEpisodeNew
        ]
    );
    let data: CastroQueueEventData = parse_castro_event_data(&batch.actions[0]).unwrap();
    assert_eq!(data.fractional_position, "ZME");
}

#[test]
fn parses_the_captured_queue_last_position() {
    let batch: CastroActionBatch = decode(json!({
        "actions": [action(1, "episode_queued", Some(json!({ "fractional_position": "aE" })))]
    }))
    .unwrap();
    let data: CastroQueueEventData = parse_castro_event_data(&batch.actions[0]).unwrap();
    assert_eq!(data.fractional_position, "aE");
}

#[test]
fn parses_playback_activity_event_data() {
    let batch: CastroActionBatch = decode(json!({
        "actions": [
            action(1, "episode_last_played", Some(json!({ "last_played": 1_784_223_188_u64 }))),
            action(2, "episode_progress", Some(json!({ "seconds": 3.819_682_539_682_54 }))),
        ]
    }))
    .unwrap();
    let played: CastroLastPlayedEventData = parse_castro_event_data(&batch.actions[0]).unwrap();
    assert_eq!(played.last_played, 1_784_223_188);
    let progress: CastroProgressEventData = parse_castro_event_data(&batch.actions[1]).unwrap();
    assert_eq!(progress.seconds, 3.819_682_539_682_54);
}

#[test]
fn parses_a_captured_subscription_mutation_response_shape() {
    let value = json!({
        "subscribed": [{
            "feed_id": "33333333-3333-4333-8333-333333333333",
            "feed_url": "https://example.com/feed.xml",
        }],
        "latest_event_id": 42,
    });
    let decoded: CastroSubscriptionResponse = decode(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn parses_live_subscription_queue_and_playback_state_snapshots() {
    let subscriptions: Vec<CastroProfileSubscription> = decode(json!([{
        "podcast_id": "33333333-3333-4333-8333-333333333333",
        "private": false,
        "will_notify_device": true,
    }]))
    .unwrap();
    assert_eq!(subscriptions.len(), 1);
    let queue: CastroQueue = decode(json!({
        "queue_items": [{
            "fractional_position": "ZM8",
            "episode_id": EPISODE_ID,
            "podcast_id": "33333333-3333-4333-8333-333333333333",
        }]
    }))
    .unwrap();
    assert_eq!(queue.queue_items[0].fractional_position, "ZM8");
    let state: CastroPodcastState = decode(json!({
        "public_id": "33333333-3333-4333-8333-333333333333",
        "episode_states": [{
            "episode_id": EPISODE_ID,
            "is_new": false,
            "is_starred": true,
            "is_played": false,
            "last_played": "2026-07-16T17:30:00.000Z",
            "progress_seconds": 42.5,
        }]
    }))
    .unwrap();
    assert_eq!(state.episode_states[0].progress_seconds, 42.5);
    assert!(state.episode_states[0].is_starred);
}

#[test]
fn an_episode_with_a_malformed_link_still_decodes() {
    let episode: omni_podcasts::castro::protocol::CastroEpisode = decode(json!({
        "guid": "guid-1",
        "public_id": EPISODE_ID,
        "short_id": "s",
        "title": "Episode",
        "media_size": null,
        "media_url": "https://cdn.example.com/ep.mp3",
        "artwork_url": null,
        "author_name": null,
        "link_url": "www.example.com/episode without scheme",
        "duration": {"seconds": 100.0},
        "description": "",
        "published_at": "2026-07-16T17:30:00.000Z",
        "predecessor_public_id": null,
        "season_number": null,
        "episode_number": null,
        "episode_type": "full",
        "people": [],
    }))
    .unwrap();
    assert_eq!(episode.guid, "guid-1");
}

#[test]
fn parses_captured_podcast_and_episode_search_results() {
    let shows: Vec<CastroPodcastSearchResult> = decode(json!([{
        "artwork_url": {
            "large": "https://example.com/large.jpg",
            "medium": "https://example.com/medium.jpg",
            "small": "https://example.com/small.jpg",
        },
        "author": "Example Author",
        "explicit": "clean",
        "feed_url": "https://example.com/feed.xml",
        "itunes_id": 1234,
        "last_episode_date": null,
        "result_position": 0,
        "summary": "Example podcast",
        "tentacles_id": "33333333-3333-4333-8333-333333333333",
        "title": "Example Podcast",
    }]))
    .unwrap();
    assert_eq!(shows.len(), 1);
    let episodes: Vec<CastroEpisodeSearchResult> = decode(json!([{
        "artwork_url": "https://example.com/episode.jpg",
        "author": "Example Author",
        "podcast_artwork_url": "https://example.com/podcast.jpg",
        "podcast_name": "Example Podcast",
        "published_at": "2026-07-16T17:30:00.000Z",
        "tentacles_id": EPISODE_ID,
        "title": "Example Episode",
    }]))
    .unwrap();
    assert_eq!(episodes.len(), 1);
}

#[test]
fn rejects_refinement_failures() {
    assert!(decode::<CastroActionBatch>(json!({ "actions": [] })).is_err());
    let mut bad = action(1, "clear_episode_new", None);
    bad["episode_id"] = json!("not-a-uuid");
    assert!(decode::<CastroActionBatch>(json!({ "actions": [bad] })).is_err());
    let missing = decode::<CastroActionBatch>(json!({
        "actions": [action(1, "clear_episode_new", None)]
    }))
    .unwrap();
    let error = parse_castro_event_data::<CastroQueueEventData>(&missing.actions[0]).unwrap_err();
    assert_eq!(error.to_string(), "clear_episode_new has no event_data");
}
