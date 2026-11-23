//! The Kick stream relay against a simulated Kick: a livestream lookup, a
//! master playlist with several variants and a live media playlist whose
//! 100 ms segments advance with wall time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use omni_api::streamers::{StreamRelayState, StreamRelayStatus, StreamerTier, paths};
use omni_core::clock::{SharedClock, SystemClock};
use omni_live::relay::buffer::BufferConfig;
use omni_live::relay::source::KickPlaybackSource;
use omni_live::relay::{KickRelays, PlaylistLookup, RelayConfig};
use omni_live::routes::{LiveRoutesState, router};
use omni_live::{Platform, PlatformBinding, Roster, Streamer};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;
use wiremock::{Mock, Request as MockRequest, Respond, ResponseTemplate, matchers};

const SEGMENT_MS: u64 = 100;
const WINDOW: u64 = 14;

/// Simulated Kick state shared with the responders.
struct Kick {
    start: Instant,
    /// Token the media playlist accepts; the lookup hands out the current one.
    token: AtomicU64,
    livestream_id: AtomicU64,
    /// Subtracted from the live sequence (a restarted broadcast starts low).
    sequence_base: AtomicU64,
    offline: AtomicBool,
    /// While set, the media playlist answers 503 (a Kick stall).
    stalled: AtomicBool,
    lookups: AtomicUsize,
    media_reads: AtomicUsize,
    /// A segment sequence that always answers 404.
    missing: AtomicU64,
}

impl Kick {
    fn live_sequence(&self) -> u64 {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).unwrap();
        (elapsed / SEGMENT_MS + 1000).saturating_sub(self.sequence_base.load(Ordering::SeqCst))
    }
}

struct Lookup(Arc<Kick>);
impl Respond for Lookup {
    fn respond(&self, _: &MockRequest) -> ResponseTemplate {
        self.0.lookups.fetch_add(1, Ordering::SeqCst);
        if self.0.offline.load(Ordering::SeqCst) {
            return ResponseTemplate::new(200).set_body_string(r#"{"data":null}"#);
        }
        let token = self.0.token.load(Ordering::SeqCst);
        let id = self.0.livestream_id.load(Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "id": id,
                "playback_url": format!("https://cdn.test/master.m3u8?token={token}"),
            }
        }))
    }
}

struct Master;
impl Respond for Master {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let token = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        ResponseTemplate::new(200).set_body_string(format!(
            "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720,FRAME-RATE=60.000\n\
https://cdn.test/v1/720.m3u8?t={token}\n\
#EXT-X-STREAM-INF:BANDWIDTH=8992204,RESOLUTION=1920x1080,FRAME-RATE=60.000,VIDEO=\"chunked\"\n\
https://cdn.test/v1/source.m3u8?t={token}\n"
        ))
    }
}

struct Media(Arc<Kick>);
impl Respond for Media {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        self.0.media_reads.fetch_add(1, Ordering::SeqCst);
        let token: u64 = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "t")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        if token != self.0.token.load(Ordering::SeqCst) {
            return ResponseTemplate::new(403);
        }
        if self.0.stalled.load(Ordering::SeqCst) {
            return ResponseTemplate::new(503);
        }
        let last = self.0.live_sequence();
        let first = last.saturating_sub(WINDOW - 1);
        let id = self.0.livestream_id.load(Ordering::SeqCst);
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
        for n in first..=last {
            body.push_str(&format!(
                "#EXTINF:0.100,live\nhttps://cdn.test/seg/{id}-{n}.ts\n"
            ));
        }
        body.push_str(&format!(
            "#EXT-X-PREFETCH:https://cdn.test/seg/{id}-{}.ts\n",
            last + 1
        ));
        ResponseTemplate::new(200).set_body_string(body)
    }
}

struct Segment(Arc<Kick>);
impl Respond for Segment {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let name = request.url.path().trim_start_matches("/seg/").to_owned();
        if kick_sequence(&name) == self.0.missing.load(Ordering::SeqCst) {
            return ResponseTemplate::new(404);
        }
        ResponseTemplate::new(200).set_body_string(format!("ts:{name}"))
    }
}

fn test_config() -> RelayConfig {
    RelayConfig {
        buffer: BufferConfig {
            cushion_ms: 1_000,
            startup_release_ms: 300,
            rebuffer_ms: 500,
            excess_ms: 600,
            window_ms: 800,
            retain_ms: 1_200,
            max_bytes: 1024 * 1024,
        },
        idle_ms: 400,
        prebuffer_hold_ms: 400,
        refresh_ms: 60_000,
        poll_ms: 25,
        offline_poll_ms: 50,
        max_relays: 2,
    }
}

struct Harness {
    kick: Arc<Kick>,
    relays: KickRelays,
    _server: wiremock::MockServer,
}

async fn harness(config: RelayConfig) -> Harness {
    let server = omni_testkit::mock_server().await;
    let kick = Arc::new(Kick {
        start: Instant::now(),
        token: AtomicU64::new(1),
        livestream_id: AtomicU64::new(7),
        sequence_base: AtomicU64::new(0),
        offline: AtomicBool::new(false),
        stalled: AtomicBool::new(false),
        lookups: AtomicUsize::new(0),
        media_reads: AtomicUsize::new(0),
        missing: AtomicU64::new(u64::MAX),
    });
    Mock::given(matchers::path("/api/v2/channels/xqc/livestream"))
        .respond_with(Lookup(kick.clone()))
        .mount(&server)
        .await;
    Mock::given(matchers::path("/master.m3u8"))
        .respond_with(Master)
        .mount(&server)
        .await;
    Mock::given(matchers::path("/v1/source.m3u8"))
        .respond_with(Media(kick.clone()))
        .mount(&server)
        .await;
    Mock::given(matchers::path("/v1/720.m3u8"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("lower variant")
        .mount(&server)
        .await;
    Mock::given(matchers::path_regex("^/seg/"))
        .respond_with(Segment(kick.clone()))
        .mount(&server)
        .await;
    let http = common::public_client(&server, &["https://kick.com", "https://cdn.test"]);
    let clock: SharedClock = Arc::new(SystemClock);
    let relays = KickRelays::new(
        KickPlaybackSource::new(http),
        clock,
        TaskTracker::new(),
        CancellationToken::new(),
        config,
    );
    Harness {
        kick,
        relays,
        _server: server,
    }
}

fn status(relays: &KickRelays) -> StreamRelayStatus {
    relays.status("xqc", |generation| {
        paths::streamer_relay_playlist("xqc", generation)
    })
}

fn start(relays: &KickRelays) -> StreamRelayStatus {
    relays.start("xqc", |generation| {
        paths::streamer_relay_playlist("xqc", generation)
    })
}

fn generation(status: &StreamRelayStatus) -> u64 {
    let url = status.playlist_url.as_deref().unwrap();
    url.split('/').rev().nth(1).unwrap().parse().unwrap()
}

/// Polls `status` (which also keeps a starting relay alive) until `done`.
async fn wait_for(
    relays: &KickRelays,
    done: impl Fn(&StreamRelayStatus) -> bool,
) -> StreamRelayStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = status(relays);
        if done(&current) {
            return current;
        }
        assert!(Instant::now() < deadline, "timed out; last {current:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn playlist(relays: &KickRelays, generation: u64) -> String {
    match relays.playlist("xqc", generation) {
        PlaylistLookup::Ready(text) => text,
        other => panic!("playlist {other:?}"),
    }
}

fn sequences(text: &str) -> Vec<u64> {
    text.lines()
        .filter_map(|l| l.strip_suffix(".ts"))
        .map(|s| s.parse().unwrap())
        .collect()
}

/// Plays like a player: reads the playlist and fetches each new segment,
/// returning the Kick segment names served in order.
async fn play(relays: &KickRelays, generation: u64, for_ms: u64) -> (Vec<String>, Vec<String>) {
    let mut served = Vec::new();
    let mut playlists = Vec::new();
    let mut next = 0;
    let deadline = Instant::now() + Duration::from_millis(for_ms);
    while Instant::now() < deadline {
        let text = playlist(relays, generation);
        for sequence in sequences(&text) {
            if sequence >= next {
                let bytes = relays.segment("xqc", generation, sequence).unwrap();
                served.push(String::from_utf8(bytes.to_vec()).unwrap());
                next = sequence + 1;
            }
        }
        playlists.push(text);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    (served, playlists)
}

fn kick_sequence(name: &str) -> u64 {
    name.rsplit('-')
        .next()
        .unwrap()
        .trim_end_matches(".ts")
        .parse()
        .unwrap()
}

fn assert_contiguous(served: &[String]) {
    for pair in served.windows(2) {
        assert_eq!(
            kick_sequence(&pair[1]),
            kick_sequence(&pair[0]) + 1,
            "gap between {} and {}",
            pair[0],
            pair[1]
        );
    }
}

#[tokio::test]
async fn starts_on_demand_and_serves_only_the_top_variant_after_the_cushion() {
    let h = harness(test_config()).await;
    assert!(h.relays.running().is_empty());
    // A status read never starts a relay.
    assert_eq!(status(&h.relays).state, StreamRelayState::Stopped);
    assert!(h.relays.running().is_empty());
    let first = start(&h.relays);
    assert_eq!(first.state, StreamRelayState::Starting);
    assert!(first.playlist_url.is_none());
    assert_eq!(h.relays.running(), ["xqc"]);

    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    assert_eq!(ready.resolution.as_deref(), Some("1920x1080"));
    assert_eq!(ready.frame_rate, Some(60.0));
    assert!(ready.cushion_ms >= 900, "{ready:?}");
    let (served, _) = play(&h.relays, generation(&ready), 600).await;
    assert!(served.len() >= 4, "{served:?}");
    assert!(served.iter().all(|s| s.starts_with("ts:7-")));
    assert_contiguous(&served);
}

#[tokio::test]
async fn an_expired_playlist_url_is_re_resolved_without_a_gap() {
    let h = harness(test_config()).await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    let generation = generation(&ready);
    let player = {
        let relays = h.relays.clone();
        tokio::spawn(async move { play(&relays, generation, 1_500).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Kick rotates the token: the playlist URL in hand now answers 403.
    h.kick.token.store(2, Ordering::SeqCst);
    let (served, playlists) = player.await.unwrap();
    let after = status(&h.relays);
    assert!(after.refreshes >= 1, "{after:?}");
    assert_eq!(after.stalls, 0);
    assert_eq!(after.discontinuities, 0);
    assert!(
        playlists
            .iter()
            .all(|p| !p.contains("#EXT-X-DISCONTINUITY\n"))
    );
    assert_contiguous(&served);
}

#[tokio::test]
async fn proactive_refresh_resolves_before_the_token_expires() {
    let h = harness(RelayConfig {
        refresh_ms: 200,
        ..test_config()
    })
    .await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    play(&h.relays, generation(&ready), 700).await;
    assert!(status(&h.relays).refreshes >= 2);
    assert!(h.kick.lookups.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn the_cushion_rides_out_a_kick_stall() {
    let h = harness(test_config()).await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    let generation = generation(&ready);
    let player = {
        let relays = h.relays.clone();
        tokio::spawn(async move { play(&relays, generation, 1_800).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.kick.stalled.store(true, Ordering::SeqCst);
    // Shorter than the 1 s cushion and the 1.4 s Kick window.
    tokio::time::sleep(Duration::from_millis(600)).await;
    h.kick.stalled.store(false, Ordering::SeqCst);
    let (served, playlists) = player.await.unwrap();
    assert_eq!(status(&h.relays).stalls, 0);
    assert_contiguous(&served);
    // The served edge kept moving through the stall.
    let edges: Vec<u64> = playlists
        .iter()
        .map(|p| *sequences(p).last().unwrap())
        .collect();
    let longest_flat = edges
        .windows(2)
        .fold((0, 0), |(run, best), w| {
            let run = if w[0] == w[1] { run + 1 } else { 0 };
            (run, best.max(run))
        })
        .1;
    assert!(longest_flat < 8, "edge stuck for {longest_flat} reads");
}

#[tokio::test]
async fn a_restarted_broadcast_is_a_discontinuity() {
    let h = harness(test_config()).await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    let generation = generation(&ready);
    let player = {
        let relays = h.relays.clone();
        tokio::spawn(async move { play(&relays, generation, 2_000).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The streamer reconnects: a new livestream id, a new token and
    // sequence numbers starting over.
    h.kick.livestream_id.store(8, Ordering::SeqCst);
    h.kick.sequence_base.store(900, Ordering::SeqCst);
    h.kick.token.store(2, Ordering::SeqCst);
    let (served, playlists) = player.await.unwrap();
    assert!(served.iter().any(|s| s.starts_with("ts:8-")), "{served:?}");
    assert!(status(&h.relays).discontinuities >= 1);
    assert!(
        playlists
            .iter()
            .any(|p| p.contains("#EXT-X-DISCONTINUITY\n"))
    );
}

#[tokio::test]
async fn stops_pulling_and_frees_the_buffer_once_no_player_fetches() {
    let h = harness(test_config()).await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    let generation = generation(&ready);
    play(&h.relays, generation, 200).await;
    // The player goes away; status polls alone do not keep it alive once
    // playing.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !h.relays.running().is_empty() {
        assert!(Instant::now() < deadline, "relay still running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.relays.playlist("xqc", generation), PlaylistLookup::Gone);
    let reads = h.kick.media_reads.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.kick.media_reads.load(Ordering::SeqCst), reads);
    assert_eq!(status(&h.relays).state, StreamRelayState::Stopped);
    // The next visit starts a fresh relay under a new generation.
    start(&h.relays);
    let again = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    assert_ne!(self::generation(&again), generation);
}

#[tokio::test]
async fn a_segment_that_stays_missing_is_skipped_without_a_lookup_storm() {
    let h = harness(test_config()).await;
    start(&h.relays);
    let ready = wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    let generation = generation(&ready);
    let lookups = h.kick.lookups.load(Ordering::SeqCst);
    h.kick
        .missing
        .store(h.kick.live_sequence() + 3, Ordering::SeqCst);
    // The served edge trails live by the cushion: play past it.
    let (served, _) = play(&h.relays, generation, 3_000).await;
    let missing = h.kick.missing.load(Ordering::SeqCst);
    assert!(served.iter().all(|s| kick_sequence(s) != missing));
    assert!(
        served.iter().any(|s| kick_sequence(s) > missing),
        "{served:?}"
    );
    // One re-resolve for the refusal, not one per poll.
    assert!(h.kick.lookups.load(Ordering::SeqCst) - lookups <= 2);
    assert_eq!(status(&h.relays).discontinuities, 1);
}

#[tokio::test]
async fn status_polls_hold_an_unwatched_relay_only_for_the_prebuffer_window() {
    let h = harness(RelayConfig {
        idle_ms: 200,
        prebuffer_hold_ms: 1_500,
        ..test_config()
    })
    .await;
    start(&h.relays);
    wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
    // The page polls status while the viewer has not pressed Play.
    let begun = Instant::now();
    while status(&h.relays).state != StreamRelayState::Stopped {
        assert!(begun.elapsed() < Duration::from_secs(5), "never released");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(begun.elapsed() >= Duration::from_millis(500));
    assert!(h.relays.running().is_empty());
}

#[tokio::test]
async fn an_offline_channel_reports_offline() {
    let h = harness(test_config()).await;
    h.kick.offline.store(true, Ordering::SeqCst);
    start(&h.relays);
    let offline = wait_for(&h.relays, |s| s.state == StreamRelayState::Offline).await;
    assert!(offline.playlist_url.is_none());
    assert!(offline.message.is_some());
    h.kick.offline.store(false, Ordering::SeqCst);
    wait_for(&h.relays, |s| s.state == StreamRelayState::Playing).await;
}

#[tokio::test]
async fn the_routes_serve_hls_with_its_content_types() {
    let h = harness(test_config()).await;
    let roster = Roster::new(vec![
        Streamer::new(
            "xqc",
            "xQc",
            vec![PlatformBinding {
                platform: Platform::Kick,
                username: "xQc".into(),
                url_override: None,
            }],
            StreamerTier::Primary,
        ),
        Streamer::new(
            "jerma",
            "Jerma",
            vec![PlatformBinding {
                platform: Platform::Twitch,
                username: "jerma985".into(),
                url_override: None,
            }],
            StreamerTier::Primary,
        ),
    ]);
    let store = omni_testkit::TestStore::new(Arc::new(SystemClock)).await;
    let app = router(LiveRoutesState {
        store: store.store.clone(),
        roster,
        relays: h.relays.clone(),
    });
    let get = |path: String| {
        let app = app.clone();
        async move {
            app.oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    };
    assert_eq!(
        get("/api/streamers/jerma/relay".into()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get("/api/streamers/nobody/relay".into()).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = get("/api/streamers/xqc/relay".into()).await;
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let before: StreamRelayStatus = serde_json::from_slice(&body).unwrap();
    assert_eq!(before.state, StreamRelayState::Stopped);
    let started = app
        .clone()
        .oneshot(
            Request::post("/api/streamers/xqc/relay")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let deadline = Instant::now() + Duration::from_secs(10);
    let ready: StreamRelayStatus = loop {
        let response = get("/api/streamers/xqc/relay".into()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: StreamRelayStatus = serde_json::from_slice(&body).unwrap();
        if status.state == StreamRelayState::Playing {
            break status;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let url = ready.playlist_url.unwrap();
    let response = get(url.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let first = sequences(&text)[0];
    let segment_url = url.replace("index.m3u8", &format!("{first}.ts"));
    let response = get(segment_url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(
        get(url.replace("index.m3u8", "99999999.ts")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get("/api/streamers/xqc/relay/1/index.m3u8".into())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}
