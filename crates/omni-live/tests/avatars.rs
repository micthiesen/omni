//! Streamer avatars: the platform lookups (Twitch GQL, Kick public API,
//! YouTube channel `og:image`), the bounded refresh and its retry rules, and
//! how views and other crates resolve a streamer's avatar.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_api::streamers::{StreamerTier, StreamerView};
use omni_core::clock::{Clock, SharedClock};
use omni_live::avatars::{
    AvatarError, AvatarFetcher, AvatarRefresh, MAX_FETCHES_PER_RUN, RETRY_AFTER_MS, StreamerAvatar,
    avatar_urls,
};
use omni_live::platforms::{
    KickClient, KickCredentials, TwitchClient, YouTubeClient, extract_og_image,
    twitch_avatar_query, youtube_channel_path,
};
use omni_live::status::{LiveStatus, StreamerStatus, upsert_status};
use omni_live::streamers::{DiscoverySource, Roster, Streamer};
use omni_live::{Platform, PlatformBinding};
use omni_store::EntityOps;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

const T0: i64 = 1_790_000_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

async fn advance(ms: i64) {
    tokio::time::advance(std::time::Duration::from_millis(u64::try_from(ms).unwrap())).await;
}

// --- platform lookups --------------------------------------------------------

#[tokio::test]
async fn twitch_reads_the_profile_image_through_gql() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .and(body_json(json!({"query": twitch_avatar_query("vinesauce")})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"user": {"profileImageURL": "https://static-cdn.jtvnw.net/v-300x300.png"}}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .and(body_json(json!({"query": twitch_avatar_query("nobody")})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"user": null}})))
        .mount(&server)
        .await;
    let client = TwitchClient::new(common::public_client(&server, &["https://gql.twitch.tv"]));
    assert_eq!(
        client.fetch_avatar("vinesauce").await.unwrap().as_deref(),
        Some("https://static-cdn.jtvnw.net/v-300x300.png")
    );
    assert_eq!(client.fetch_avatar("nobody").await.unwrap(), None);
    assert!(client.fetch_avatar("unmocked").await.is_err());
}

#[test]
fn twitch_avatar_query_escapes_the_login() {
    assert_eq!(
        twitch_avatar_query("a\"b"),
        r#"query{user(login:"a\"b"){profileImageURL(width:300)}}"#
    );
}

fn kick_credentials() -> Option<KickCredentials> {
    Some(KickCredentials {
        client_id: "id".into(),
        client_secret: "secret".into(),
    })
}

#[tokio::test]
async fn kick_reads_the_broadcaster_profile_picture_with_the_app_token() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "token", "token_type": "Bearer", "expires_in": 3600}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/channels"))
        .and(query_param("slug", "destiny"))
        .and(header("Authorization", "Bearer token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"slug": "destiny", "broadcaster_user_id": 4_242, "stream_title": ""}],
            "message": "OK"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/channels"))
        .and(query_param("slug", "ghost"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [], "message": "OK"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/users"))
        .and(query_param("id", "4242"))
        .and(header("Authorization", "Bearer token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"user_id": 4_242, "name": "Destiny", "email": "", "profile_picture": "https://files.kick.com/images/user/4242/profile.webp"}],
            "message": "OK"
        })))
        .mount(&server)
        .await;
    let http = common::public_client(&server, &["https://id.kick.com", "https://api.kick.com"]);
    let client = KickClient::new(http, kick_credentials());
    assert!(client.has_credentials());
    assert_eq!(
        client.fetch_avatar("destiny").await.unwrap().as_deref(),
        Some("https://files.kick.com/images/user/4242/profile.webp")
    );
    assert_eq!(client.fetch_avatar("ghost").await.unwrap(), None);
}

#[tokio::test]
async fn kick_without_credentials_reports_no_avatar_support() {
    let client = KickClient::new(
        omni_http::public::PublicHttpClient::new(&omni_testkit::no_network()),
        None,
    );
    assert!(!client.has_credentials());
    assert!(client.fetch_avatar("destiny").await.is_err());
}

#[test]
fn youtube_avatar_is_the_decoded_og_image_from_the_head() {
    let html = r#"<html><head><title>x</title>
        <meta property="og:title" content="Destiny">
        <meta property="og:image" content="https://yt3.googleusercontent.com/abc=s900-c-k?x=1&amp;y=2">
        </head><body><meta property="og:image" content="https://evil.example/late.png"></body></html>"#;
    assert_eq!(
        extract_og_image(html).as_deref(),
        Some("https://yt3.googleusercontent.com/abc=s900-c-k?x=1&y=2")
    );
    assert_eq!(extract_og_image("<html><head></head></html>"), None);
    let script_first = r#"<html><head><script>var s = "</head>";</script>
        <meta property="og:image" content="https://yt3.googleusercontent.com/real=s900">
        </head><body></body></html>"#;
    assert_eq!(
        extract_og_image(script_first).as_deref(),
        Some("https://yt3.googleusercontent.com/real=s900")
    );
    assert_eq!(
        extract_og_image(r#"<head><meta property="og:image" content="  "></head>"#),
        None
    );
}

#[test]
fn youtube_avatar_rejects_generic_images_and_other_hosts() {
    let page = |content: &str| {
        format!(r#"<html><head><meta property="og:image" content="{content}"></head></html>"#)
    };
    assert_eq!(
        extract_og_image(&page("https://yt3.ggpht.com/abc=s900")).as_deref(),
        Some("https://yt3.ggpht.com/abc=s900")
    );
    for generic in [
        "https://www.youtube.com/img/desktop/yt_1200.png",
        "https://s.ytimg.com/yts/img/favicon_144-vfliLAfaB.png",
        "https://i.ytimg.com/vi/abc/maxresdefault.jpg",
        "http://yt3.ggpht.com/abc",
        "https://yt3.ggpht.com.evil.example/abc",
        "/img/avatar.png",
    ] {
        assert_eq!(extract_og_image(&page(generic)), None, "{generic}");
    }
}

#[test]
fn youtube_channel_paths_accept_only_binding_username_shapes() {
    for valid in [
        "@destiny",
        "@Some.Name-1_x",
        "channel/UCx4y2Rj9vB5PTRLwQ9w2xKQ",
        "legacyname",
    ] {
        assert_eq!(
            youtube_channel_path(valid).as_deref(),
            Some(valid),
            "{valid}"
        );
    }
    for invalid in [
        "",
        "@",
        "@a/b",
        "@a?x=1",
        "@a#x",
        "channel/UCshort",
        "channel/XX4y2Rj9vB5PTRLwQ9w2xKQ0",
        "channel/UCx4y2Rj9vB5PTRLwQ9w2xKQ/videos",
        "c/legacy",
        "user/legacy",
        "watch?v=abc",
        "../etc",
        ".hidden",
    ] {
        assert_eq!(youtube_channel_path(invalid), None, "{invalid}");
    }
}

#[tokio::test]
async fn youtube_never_requests_a_rejected_username() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .expect(0)
        .mount(&server)
        .await;
    let client = YouTubeClient::new(common::public_client(&server, &["https://www.youtube.com"]));
    assert_eq!(client.fetch_avatar("watch?v=abc").await.unwrap(), None);
    assert_eq!(client.fetch_avatar("@a/b").await.unwrap(), None);
}

#[tokio::test]
async fn youtube_fetches_the_channel_page() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/@destiny"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html><head><meta property="og:image" content="https://yt3.googleusercontent.com/d"></head></html>"#,
        ))
        .mount(&server)
        .await;
    let client = YouTubeClient::new(common::public_client(&server, &["https://www.youtube.com"]));
    assert_eq!(
        client.fetch_avatar("@destiny").await.unwrap().as_deref(),
        Some("https://yt3.googleusercontent.com/d")
    );
    let error = client.fetch_avatar("@missing").await.unwrap_err();
    assert!(
        error
            .message
            .starts_with("Failed to fetch YouTube channel page https://www.youtube.com/@missing"),
        "{}",
        error.message
    );
}

// --- refresh -------------------------------------------------------------------

type Answer = Result<Option<String>, String>;

/// Answers by account key; unknown accounts fail. Records every lookup.
#[derive(Clone, Default)]
struct FakeAvatars {
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    calls: Arc<Mutex<Vec<String>>>,
    no_kick: bool,
}

impl FakeAvatars {
    fn answer(&self, key: &str, answer: Result<Option<&str>, &str>) {
        self.answers.lock().unwrap().insert(
            key.to_owned(),
            answer
                .map(|url| url.map(str::to_owned))
                .map_err(str::to_owned),
        );
    }

    fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl AvatarFetcher for FakeAvatars {
    fn supports(&self, platform: Platform) -> bool {
        !(self.no_kick && platform == Platform::Kick)
    }

    fn fetch<'a>(
        &'a self,
        binding: &'a PlatformBinding,
    ) -> BoxFuture<'a, Result<Option<String>, AvatarError>> {
        let key = omni_live::identity::binding_key(binding);
        self.calls.lock().unwrap().push(key.clone());
        let answer = self
            .answers
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Err("unreachable".into()));
        Box::pin(async move { answer.map_err(|message| AvatarError { message }) })
    }
}

fn streamer(id: &str, bindings: &[(Platform, &str)]) -> Streamer {
    Streamer::new(
        id,
        id,
        bindings
            .iter()
            .map(|(platform, username)| PlatformBinding::new(*platform, *username))
            .collect(),
        StreamerTier::Primary,
    )
}

fn discovered(id: &str, platform: Platform, username: &str) -> Streamer {
    let mut streamer = streamer(id, &[(platform, username)]);
    streamer.tier = StreamerTier::Background;
    streamer.discovery_source = Some(DiscoverySource::Dgg);
    streamer
}

async fn rows(store: &omni_store::Store) -> HashMap<String, StreamerAvatar> {
    store
        .read(|docs| docs.get_all::<StreamerAvatar>())
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.binding.clone(), row))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn refresh_stores_avatars_and_keeps_the_last_url_through_a_failure() {
    let (store, clock) = common::test_store(T0).await;
    let configured = vec![streamer(
        "destiny",
        &[(Platform::YouTube, "@destiny"), (Platform::Kick, "destiny")],
    )];
    let roster = Roster::new(configured.clone());
    let fetcher = FakeAvatars::default();
    fetcher.answer("youtube:@destiny", Ok(Some("https://yt3.example/d")));
    fetcher.answer("kick:destiny", Ok(None));
    let shared: SharedClock = clock.clone();
    let refresh = AvatarRefresh::new(
        store.store.clone(),
        shared,
        roster.clone(),
        Roster::new(configured),
        Arc::new(fetcher.clone()),
    );

    let summary = refresh.run().await.unwrap();
    assert_eq!(
        (summary.attempted, summary.found, summary.failed),
        (2, 1, 0)
    );
    let stored = rows(&store.store).await;
    assert_eq!(
        stored["youtube:@destiny"].url.as_deref(),
        Some("https://yt3.example/d")
    );
    assert_eq!(stored["kick:destiny"].url, None);

    // Nothing is due within a day.
    fetcher.take_calls();
    refresh.run().await.unwrap();
    assert!(fetcher.take_calls().is_empty());

    // After a day only the account without an avatar is rechecked.
    advance(RETRY_AFTER_MS).await;
    refresh.run().await.unwrap();
    assert_eq!(fetcher.take_calls(), ["kick:destiny"]);

    // After a week the known avatar refreshes; a failure keeps it and waits a day.
    advance(7 * DAY).await;
    fetcher.answer("youtube:@destiny", Err("HTTP 503"));
    let summary = refresh.run().await.unwrap();
    assert_eq!(summary.failed, 1);
    let stored = rows(&store.store).await;
    assert_eq!(
        stored["youtube:@destiny"].url.as_deref(),
        Some("https://yt3.example/d")
    );
    assert_eq!(stored["youtube:@destiny"].failed_at, Some(clock.now_ms()));
    fetcher.take_calls();
    advance(RETRY_AFTER_MS - 1).await;
    refresh.run().await.unwrap();
    assert!(
        !fetcher
            .take_calls()
            .contains(&"youtube:@destiny".to_owned())
    );
    advance(1).await;
    refresh.run().await.unwrap();
    assert!(
        fetcher
            .take_calls()
            .contains(&"youtube:@destiny".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_is_bounded_per_run_and_covers_discoveries_after_configured_accounts() {
    let (store, clock) = common::test_store(T0).await;
    let configured: Vec<Streamer> = (0..MAX_FETCHES_PER_RUN)
        .map(|i| streamer(&format!("s{i}"), &[(Platform::Twitch, &format!("s{i}"))]))
        .collect();
    let mut roster = configured.clone();
    roster.push(discovered("dgg:twitch:d", Platform::Twitch, "d"));
    roster.push(discovered("dgg:youtube:v", Platform::YouTube, "videoid"));
    roster.push(discovered("dgg:kick:k", Platform::Kick, "k"));
    let fetcher = FakeAvatars {
        no_kick: true,
        ..FakeAvatars::default()
    };
    for i in 0..MAX_FETCHES_PER_RUN {
        fetcher.answer(
            &format!("twitch:s{i}"),
            Ok(Some("https://cdn.example/s.png")),
        );
    }
    fetcher.answer("twitch:d", Ok(Some("https://cdn.example/d.png")));
    let shared: SharedClock = clock.clone();
    let refresh = AvatarRefresh::new(
        store.store.clone(),
        shared,
        Roster::new(roster),
        Roster::new(configured),
        Arc::new(fetcher.clone()),
    );
    refresh.run().await.unwrap();
    let first = fetcher.take_calls();
    assert_eq!(first.len(), MAX_FETCHES_PER_RUN);
    assert!(!first.contains(&"twitch:d".to_owned()));
    refresh.run().await.unwrap();
    // YouTube discoveries are videos and Kick has no credentials: skipped.
    assert_eq!(fetcher.take_calls(), ["twitch:d"]);
}

#[tokio::test(start_paused = true)]
async fn refresh_rejects_non_https_urls_and_prunes_untracked_accounts() {
    let (store, clock) = common::test_store(T0).await;
    let configured = vec![streamer("a", &[(Platform::Twitch, "a")])];
    let roster = Roster::new(configured.clone());
    let fetcher = FakeAvatars::default();
    fetcher.answer("twitch:a", Ok(Some("http://insecure.example/a.png")));
    let shared: SharedClock = clock.clone();
    let refresh = AvatarRefresh::new(
        store.store.clone(),
        shared,
        roster.clone(),
        Roster::new(configured),
        Arc::new(fetcher),
    );
    refresh.run().await.unwrap();
    assert_eq!(rows(&store.store).await["twitch:a"].url, None);

    roster.replace(Vec::new());
    let refresh_after_removal = AvatarRefresh::new(
        store.store.clone(),
        clock.clone(),
        roster,
        Roster::default(),
        Arc::new(FakeAvatars::default()),
    );
    advance(29 * DAY).await;
    assert_eq!(refresh_after_removal.run().await.unwrap().pruned, 0);
    advance(DAY).await;
    assert_eq!(refresh_after_removal.run().await.unwrap().pruned, 1);
    assert!(rows(&store.store).await.is_empty());
}

// --- resolution ------------------------------------------------------------------

async fn seed_avatar(store: &omni_store::Store, platform: Platform, username: &str, url: &str) {
    let row = StreamerAvatar {
        binding: omni_live::identity::canonical_binding_key(platform, username),
        platform,
        username: username.into(),
        url: Some(url.into()),
        fetched_at: Some(T0),
        failed_at: None,
        extra: omni_store::cbor::Extra::new(),
    };
    store
        .write(move |tx| {
            omni_store::EntityWrite::upsert(tx, &row, omni_store::entity::UpsertOpts::default())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn avatar_urls_prefer_the_live_primary_account() {
    let (store, _clock) = common::test_store(T0).await;
    let both = streamer(
        "both",
        &[(Platform::Twitch, "both"), (Platform::Kick, "both")],
    );
    let offline = streamer("off", &[(Platform::Kick, "off")]);
    let none = streamer("none", &[(Platform::Kick, "none")]);
    seed_avatar(&store.store, Platform::Twitch, "both", "https://t/both").await;
    seed_avatar(&store.store, Platform::Kick, "both", "https://k/both").await;
    seed_avatar(&store.store, Platform::Kick, "off", "https://k/off").await;
    let streamers = vec![both.clone(), offline, none];

    let urls = avatar_urls(&store.store, &streamers).await.unwrap();
    assert_eq!(urls.get("both").map(String::as_str), Some("https://t/both"));
    assert_eq!(urls.get("off").map(String::as_str), Some("https://k/off"));
    assert!(!urls.contains_key("none"));

    let live = LiveStatus {
        streamer_id: "both".into(),
        primary: PlatformBinding::new(Platform::Kick, "both"),
        primary_title: "title".into(),
        started_at: T0,
        max_viewer_count: 1,
        viewer_count: Some(1),
        sources: None,
        category: None,
        extra: omni_store::cbor::Extra::new(),
    };
    upsert_status(&store.store, StreamerStatus::Live(live))
        .await
        .unwrap();
    let urls = avatar_urls(&store.store, &streamers).await.unwrap();
    assert_eq!(urls.get("both").map(String::as_str), Some("https://k/both"));

    let views = omni_live::display::display_views(&store.store, &streamers)
        .await
        .unwrap();
    let avatar = |id: &str| {
        views.iter().find(|v| v.id() == id).and_then(|v| match v {
            StreamerView::Live(live) => live.avatar_url.clone(),
            StreamerView::Offline(offline) => offline.avatar_url.clone(),
        })
    };
    assert_eq!(avatar("both").as_deref(), Some("https://k/both"));
    assert_eq!(avatar("off").as_deref(), Some("https://k/off"));
    assert_eq!(avatar("none"), None);
    let json = serde_json::to_value(&views).unwrap();
    assert!(
        json.as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "none" && v.get("avatarUrl").is_none()),
        "absent, not null"
    );
}
