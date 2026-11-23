//! The end-to-end Castro enqueue check
//! against the LIVE account. Never runs in CI; it changes the real queue
//! (enqueue, verify, dequeue, verify) and reads a public RSS feed:
//!
//! `npx @dotenvx/dotenvx run -- cargo test -p omni-podcasts --test it castro_smoke:: -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;

use jiff::tz::TimeZone;
use omni_core::clock::{SharedClock, SystemClock};
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, HttpConfig, SideEffectMode};
use omni_podcasts::account::{
    EnqueueEpisodeRequest, PodcastAccount, PodcastQueuePosition, PodcastWriteResult,
};
use omni_podcasts::castro::api::shared_castro_api;
use omni_podcasts::castro::client::CastroClient;
use omni_podcasts::rss::fetch_feed_episodes;

// Radiolab: not subscribed by the owner; Simplecast rewrites RSS guids, so
// this also covers media-URL matching.
const SHOW_TITLE: &str = "Radiolab";
const FEED_URL: &str = "https://feeds.simplecast.com/EmVW7VGp";

#[tokio::test]
#[ignore = "talks to the live Castro account; run manually with dotenvx"]
async fn castro_enqueue_round_trip_leaves_the_queue_untouched() {
    let access_id = std::env::var("CASTRO_ACCESS_ID").expect("CASTRO_ACCESS_ID");
    let secret = std::env::var("CASTRO_SECRET_KEY").expect("CASTRO_SECRET_KEY");
    let http = PublicHttpClient::new(&HttpClient::new(HttpConfig::default()).unwrap());
    let clock: SharedClock = Arc::new(SystemClock);
    let api = shared_castro_api(&http, &clock, SideEffectMode::Live, &access_id, &secret);
    let account = CastroClient::new(api, clock);

    let episode = fetch_feed_episodes(&http, FEED_URL, 1, &TimeZone::UTC)
        .await
        .expect("SMOKE FAILED: could not read the test feed")
        .into_iter()
        .next()
        .expect("SMOKE FAILED: could not read the test feed");
    println!("Test episode: {SHOW_TITLE} — {}", episode.title);

    let enqueue = account
        .enqueue_episode(EnqueueEpisodeRequest {
            feed_url: FEED_URL.into(),
            itunes_id: None,
            episode_guid: episode.guid.clone(),
            media_url: episode.enclosure_url.clone(),
            show_title: SHOW_TITLE.into(),
            episode_title: episode.title.clone(),
            position: Some(PodcastQueuePosition::Next),
        })
        .await;
    assert!(
        matches!(
            enqueue,
            PodcastWriteResult::Added | PodcastWriteResult::AlreadyExists
        ),
        "SMOKE FAILED: enqueueEpisode returned {enqueue:?}"
    );

    // An "added" only means the POST returned 200; verify the queue.
    let queue = account
        .fetch_queue()
        .await
        .expect("SMOKE FAILED: fetchQueue unavailable");
    let ours = queue
        .iter()
        .find(|item| item.show_title == SHOW_TITLE && item.episode_title == episode.title)
        .expect("SMOKE FAILED: episode was NOT in the queue after enqueue (write did not land)")
        .clone();
    println!("Verified in queue ({} items)", queue.len());

    let guid = ours.episode_guid.clone().unwrap_or_default();
    assert_eq!(
        account.dequeue_episode(&guid).await,
        PodcastWriteResult::Removed
    );
    let after = account
        .fetch_queue()
        .await
        .expect("SMOKE FAILED: fetchQueue unavailable");
    assert!(
        !after
            .iter()
            .any(|item| item.show_title == SHOW_TITLE && item.episode_title == episode.title),
        "SMOKE FAILED: episode still in the queue after dequeue"
    );
    println!("SMOKE PASSED: enqueue landed, verified, and cleaned up. Queue untouched.");
}
