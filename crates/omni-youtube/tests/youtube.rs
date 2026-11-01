//! The YouTube integration against a mock Google: OAuth connection and
//! refresh, quota diagnostics, and the write safety rules (duplicate checks,
//! idempotency keys, uncertain outcomes settled only by reading, delete
//! confirmation, ownership).
#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use omni_core::clock::SharedClock;
use omni_http::SideEffectMode;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, test_clock};
use omni_youtube::error::YoutubeError;
use omni_youtube::model::{AddOutcome, ConnectionState, CreateOutcome, Privacy};
use omni_youtube::records::{OWNER, YoutubeAccountData};
use omni_youtube::secrets::Sealer;
use omni_youtube::service::{
    CreatePlaylist, Deps, PlaylistScope, Settings, UpdatePlaylist, Youtube,
};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHANNEL: &str = "UCownerownerownerownerow";
const KEY: [u8; 32] = [9; 32];

struct Fixture {
    server: MockServer,
    youtube: Youtube,
    store: TestStore,
}

async fn fixture(mode: SideEffectMode) -> Fixture {
    let server = MockServer::start().await;
    let http = mock_http(
        &server,
        &[
            "https://www.googleapis.com",
            "https://oauth2.googleapis.com",
        ],
    );
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let youtube = Youtube::new(Deps {
        settings: Settings {
            client_id: Some("client-id".into()),
            client_secret: Some("client-secret".into()),
            redirect_uri: "https://omni.syas.ca/api/youtube/oauth/callback".into(),
        },
        store: store.store.clone(),
        http,
        sealer: Ok(Sealer::from_key(KEY)),
        mode,
    });
    Fixture {
        server,
        youtube,
        store,
    }
}

/// A connected account whose refresh token Google accepts.
async fn connected(mode: SideEffectMode) -> Fixture {
    let f = fixture(mode).await;
    let row = YoutubeAccountData {
        id: OWNER.into(),
        refresh_token: Sealer::from_key(KEY)
            .seal(&YoutubeAccountData::seal_context(OWNER), "1//refresh"),
        scopes: vec![omni_youtube::service::SCOPE.into()],
        channel_id: Some(CHANNEL.into()),
        channel_title: Some("Owner".into()),
        connected_at: TEST_EPOCH_MS,
        problem: None,
        problem_at: None,
        extra: Extra::default(),
    };
    f.store
        .store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.access", "expires_in": 3599, "scope": omni_youtube::service::SCOPE, "token_type": "Bearer"
        })))
        .mount(&f.server)
        .await;
    f
}

fn playlist(id: &str, title: &str, channel: &str) -> Value {
    json!({
        "id": id,
        "snippet": {"title": title, "description": "kept", "channelId": channel, "channelTitle": "Owner",
                    "publishedAt": "2026-01-01T00:00:00Z", "tags": ["trip"]},
        "status": {"privacyStatus": "unlisted"},
        "contentDetails": {"itemCount": 2}
    })
}

async fn mount_playlist(f: &Fixture, id: &str, title: &str, channel: &str) {
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .and(query_param("id", id))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [playlist(id, title, channel)]})),
        )
        .mount(&f.server)
        .await;
}

async fn mount_my_playlists(f: &Fixture, items: Value) {
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .and(query_param("mine", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": items})))
        .mount(&f.server)
        .await;
}

fn create(key: &str, title: &str) -> CreatePlaylist {
    CreatePlaylist {
        title: title.into(),
        description: String::new(),
        privacy: Privacy::Private,
        idempotency_key: key.into(),
        allow_duplicate_title: false,
    }
}

async fn requests(f: &Fixture, verb: &str, route: &str) -> Vec<wiremock::Request> {
    f.server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .collect()
}

#[tokio::test]
async fn connecting_exchanges_the_code_once_and_seals_the_refresh_token() {
    let f = fixture(SideEffectMode::Live).await;
    let consent = url::Url::parse(&f.youtube.start_authorization().await.unwrap()).unwrap();
    let query: std::collections::HashMap<_, _> = consent.query_pairs().into_owned().collect();
    assert_eq!(consent.host_str(), Some("accounts.google.com"));
    assert_eq!(query["scope"], "https://www.googleapis.com/auth/youtube");
    assert_eq!(query["access_type"], "offline");
    assert_eq!(query["prompt"], "consent");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(
        query["redirect_uri"],
        "https://omni.syas.ca/api/youtube/oauth/callback"
    );
    let state = query["state"].clone();

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.first", "refresh_token": "1//granted", "expires_in": 3599,
            "scope": "https://www.googleapis.com/auth/youtube", "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/channels"))
        .and(query_param("mine", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"id": CHANNEL, "snippet": {"title": "Owner"}}]
        })))
        .mount(&f.server)
        .await;

    let channel = f
        .youtube
        .complete_authorization("4/code", &state)
        .await
        .unwrap();
    assert_eq!(channel.channel_id, CHANNEL);
    let again = f.youtube.complete_authorization("4/code", &state).await;
    assert!(matches!(again, Err(YoutubeError::Refused(_))));

    let row = f
        .store
        .store
        .read(|tx| tx.get::<YoutubeAccountData>(&OWNER.to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert!(!row.refresh_token.contains("granted"));
    let status = f.youtube.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(
        status.connect_url,
        "https://omni.syas.ca/api/youtube/oauth/start"
    );
}

#[tokio::test]
async fn a_grant_without_the_youtube_scope_is_refused() {
    let f = fixture(SideEffectMode::Live).await;
    let consent = url::Url::parse(&f.youtube.start_authorization().await.unwrap()).unwrap();
    let state = consent
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.x", "refresh_token": "1//x", "expires_in": 3599, "scope": "openid"
        })))
        .mount(&f.server)
        .await;
    let result = f.youtube.complete_authorization("4/code", &state).await;
    assert!(matches!(result, Err(YoutubeError::Refused(m)) if m.contains("not granted")));
    assert_eq!(
        f.youtube.status().await.unwrap().state,
        ConnectionState::NotConnected
    );
}

#[tokio::test]
async fn a_revoked_refresh_token_asks_for_reconnection_without_retrying() {
    let f = fixture(SideEffectMode::Live).await;
    let row = YoutubeAccountData {
        id: OWNER.into(),
        refresh_token: Sealer::from_key(KEY)
            .seal(&YoutubeAccountData::seal_context(OWNER), "1//old"),
        scopes: vec![],
        channel_id: Some(CHANNEL.into()),
        channel_title: None,
        connected_at: TEST_EPOCH_MS,
        problem: None,
        problem_at: None,
        extra: Extra::default(),
    };
    f.store
        .store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant", "error_description": "Token has been expired or revoked."
        })))
        .expect(1)
        .mount(&f.server)
        .await;
    let first = f.youtube.playlists(PlaylistScope::Mine, None, 25).await;
    assert!(
        matches!(&first, Err(YoutubeError::Reconnect { reason, .. }) if reason.contains("Testing"))
    );
    let second = f.youtube.playlists(PlaylistScope::Mine, None, 25).await;
    assert!(matches!(second, Err(YoutubeError::Reconnect { .. })));
    let status = f.youtube.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::ReconnectRequired);
    assert!(status.problem.unwrap().contains("expired or revoked"));
}

#[tokio::test]
async fn an_exhausted_quota_is_reported_with_todays_usage() {
    let f = connected(SideEffectMode::Live).await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": 403, "message": "The request cannot be completed because you have exceeded your <a href=\"/youtube/v3/getting-started#quota\">quota</a>.",
                      "errors": [{"reason": "quotaExceeded", "domain": "youtube.quota"}]}
        })))
        .mount(&f.server)
        .await;
    let error = f
        .youtube
        .playlists(PlaylistScope::Mine, None, 25)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "quota_exceeded");
    let text = error.to_string();
    assert!(text.contains("midnight Pacific"), "{text}");
    assert!(
        text.contains(
            "the shared pool (default 10000 units a day); Omni's requests came to about 1 units"
        ),
        "{text}"
    );
    assert!(
        text.contains("other apps of the same Google Cloud project also count"),
        "{text}"
    );
    let quota = f.youtube.status().await.unwrap().quota;
    assert_eq!(quota.estimated_units_used, 1);
    assert_eq!(quota.calls.get("playlists.list"), Some(&1));
    assert!(quota.quota_exceeded_at.is_some());
    assert!(quota.search_quota_exceeded_at.is_none());
}

#[tokio::test]
async fn an_exhausted_search_bucket_is_reported_separately_from_the_pool() {
    let f = connected(SideEffectMode::Live).await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/search"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": 403, "message": "quota", "errors": [{"reason": "quotaExceeded"}]}
        })))
        .mount(&f.server)
        .await;
    let error = f
        .youtube
        .search(omni_youtube::service::SearchParams {
            query: "x".into(),
            kind: omni_youtube::model::SearchType::Video,
            order: None,
            channel_id: None,
            published_after: None,
            published_before: None,
            duration: None,
            region_code: None,
            relevance_language: None,
            limit: 1,
            page_token: None,
        })
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("the search bucket (default 100 searches a day); Omni sent 1 searches"),
        "{text}"
    );
    let quota = f.youtube.quota_status().await.unwrap();
    assert_eq!(quota.search_calls, 1);
    assert_eq!(quota.estimated_units_used, 0);
    assert!(quota.search_quota_exceeded_at.is_some());
    assert!(quota.quota_exceeded_at.is_none());
}

#[tokio::test]
async fn rows_counted_under_the_old_search_cost_read_with_the_bucket_model() {
    let f = connected(SideEffectMode::Live).await;
    let (day, _) = omni_youtube::service::quota_day(TEST_EPOCH_MS);
    let row = omni_youtube::records::YoutubeQuotaData {
        day,
        units: 106,
        calls: [
            ("search.list", 1),
            ("videos.list", 2),
            ("channels.list", 2),
            ("playlists.list", 1),
            ("playlistItems.list", 1),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
        quota_exceeded_at: None,
        search_quota_exceeded_at: None,
        extra: Extra::default(),
    };
    f.store
        .store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
    let quota = f.youtube.quota_status().await.unwrap();
    assert_eq!(quota.estimated_units_used, 6);
    assert_eq!(quota.search_calls, 1);
}

#[tokio::test]
async fn a_disabled_api_names_the_fix() {
    let f = connected(SideEffectMode::Live).await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": 403, "message": "YouTube Data API v3 has not been used in project 1 before or it is disabled.",
                      "errors": [{"reason": "accessNotConfigured"}]}
        })))
        .mount(&f.server)
        .await;
    let error = f
        .youtube
        .videos(vec!["dQw4w9WgXcQ".into()])
        .await
        .unwrap_err();
    assert_eq!(error.code(), "api_disabled");
    assert!(error.to_string().contains("Enable \"YouTube Data API v3\""));
}

#[tokio::test]
async fn create_refuses_a_title_the_channel_already_uses() {
    let f = connected(SideEffectMode::Live).await;
    mount_my_playlists(&f, json!([playlist("PLexisting", "Road Trip", CHANNEL)])).await;
    let result = f
        .youtube
        .create_playlist(create("key-00000001", "road trip"))
        .await;
    assert!(matches!(result, Err(YoutubeError::Refused(m)) if m.contains("PLexisting")));
    assert!(
        requests(&f, "POST", "/youtube/v3/playlists")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn the_same_idempotency_key_never_creates_twice() {
    let f = connected(SideEffectMode::Live).await;
    mount_my_playlists(&f, json!([])).await;
    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playlist(
            "PLnew",
            "Road Trip",
            CHANNEL,
        )))
        .expect(1)
        .mount(&f.server)
        .await;
    mount_playlist(&f, "PLnew", "Road Trip", CHANNEL).await;
    let first = f
        .youtube
        .create_playlist(create("key-00000002", "Road Trip"))
        .await
        .unwrap();
    assert_eq!(first.outcome, CreateOutcome::Created);
    let body: Value = requests(&f, "POST", "/youtube/v3/playlists").await[0]
        .body_json()
        .unwrap();
    assert_eq!(body["status"]["privacyStatus"], "private");
    let second = f
        .youtube
        .create_playlist(create("key-00000002", "Road Trip"))
        .await
        .unwrap();
    assert_eq!(second.outcome, CreateOutcome::AlreadyCreated);
    assert_eq!(second.playlist.unwrap().playlist_id, "PLnew");
    let other = f
        .youtube
        .create_playlist(create("key-00000002", "Something else"))
        .await;
    assert!(matches!(other, Err(YoutubeError::Invalid(_))));
}

#[tokio::test]
async fn a_create_whose_answer_was_lost_is_settled_by_reading_and_never_resent() {
    let f = connected(SideEffectMode::Live).await;
    mount_my_playlists(&f, json!([])).await;
    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": {"code": 503, "message": "Backend error", "errors": [{"reason": "backendError"}]}
        })))
        .expect(1)
        .mount(&f.server)
        .await;
    let first = f
        .youtube
        .create_playlist(create("key-00000003", "Road Trip"))
        .await;
    assert!(matches!(first, Err(YoutubeError::Uncertain(_))));
    let second = f
        .youtube
        .create_playlist(create("key-00000003", "Road Trip"))
        .await;
    assert!(matches!(second, Err(YoutubeError::Uncertain(_))));
    assert_eq!(requests(&f, "POST", "/youtube/v3/playlists").await.len(), 1);
}

#[tokio::test]
async fn adding_skips_videos_already_in_the_playlist() {
    let f = connected(SideEffectMode::Live).await;
    mount_playlist(&f, "PLmine", "Mine", CHANNEL).await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"id": "aaaaaaaaaaa", "snippet": {"title": "A"}},
            {"id": "bbbbbbbbbbb", "snippet": {"title": "B"}}
        ]})))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlistItems"))
        .and(query_param("playlistId", "PLmine"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"id": "UExpdGVtLWE=", "contentDetails": {"videoId": "aaaaaaaaaaa"}}
        ]})))
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlistItems"))
        .and(body_string_contains("bbbbbbbbbbb"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "UExpdGVtLWI="})))
        .expect(1)
        .mount(&f.server)
        .await;
    let out = f
        .youtube
        .add_items(
            "PLmine",
            vec![
                "aaaaaaaaaaa".into(),
                "bbbbbbbbbbb".into(),
                "ccccccccccc".into(),
            ],
            None,
        )
        .await
        .unwrap();
    let outcomes: Vec<_> = out.results.iter().map(|r| r.outcome).collect();
    assert_eq!(
        outcomes,
        [
            AddOutcome::AlreadyPresent,
            AddOutcome::Added,
            AddOutcome::NotFound
        ]
    );
    assert_eq!(out.added, 1);
    // maxResults is not supported together with id lookups.
    let lookup = &requests(&f, "GET", "/youtube/v3/videos").await[0];
    assert!(!lookup.url.query().unwrap_or("").contains("maxResults"));
}

#[tokio::test]
async fn delete_needs_the_exact_current_title() {
    let f = connected(SideEffectMode::Live).await;
    mount_playlist(&f, "PLmine", "Road Trip", CHANNEL).await;
    let result = f.youtube.delete_playlist("PLmine", "Road trip").await;
    assert!(matches!(result, Err(YoutubeError::Refused(m)) if m.contains("nothing was deleted")));
    assert!(
        requests(&f, "DELETE", "/youtube/v3/playlists")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn writes_refuse_playlists_of_other_channels() {
    let f = connected(SideEffectMode::Live).await;
    mount_playlist(&f, "PLtheirs", "Theirs", "UCotherotherotherotherot").await;
    let result = f
        .youtube
        .update_playlist(UpdatePlaylist {
            playlist_id: "PLtheirs".into(),
            title: Some("Mine now".into()),
            description: None,
            privacy: None,
        })
        .await;
    assert!(matches!(result, Err(YoutubeError::Refused(m)) if m.contains("another channel")));
    assert!(
        requests(&f, "PUT", "/youtube/v3/playlists")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_rename_resends_the_fields_it_does_not_change() {
    let f = connected(SideEffectMode::Live).await;
    mount_playlist(&f, "PLmine", "Old", CHANNEL).await;
    Mock::given(method("PUT"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playlist("PLmine", "New", CHANNEL)))
        .expect(1)
        .mount(&f.server)
        .await;
    f.youtube
        .update_playlist(UpdatePlaylist {
            playlist_id: "PLmine".into(),
            title: Some("New".into()),
            description: None,
            privacy: None,
        })
        .await
        .unwrap();
    let put = &requests(&f, "PUT", "/youtube/v3/playlists").await[0];
    let body: Value = put.body_json().unwrap();
    assert_eq!(body["snippet"]["title"], "New");
    assert_eq!(body["snippet"]["description"], "kept");
    assert_eq!(body["snippet"]["tags"], json!(["trip"]));
    assert_eq!(body["status"]["privacyStatus"], "unlisted");
    assert!(put.url.query().unwrap().contains("part=snippet%2Cstatus"));
}

#[tokio::test]
async fn record_mode_sends_no_writes() {
    let f = connected(SideEffectMode::Record).await;
    mount_my_playlists(&f, json!([])).await;
    let out = f
        .youtube
        .create_playlist(create("key-00000004", "Road Trip"))
        .await
        .unwrap();
    assert_eq!(out.outcome, CreateOutcome::Recorded);
    assert_eq!(f.youtube.recorded().len(), 1);
    assert!(
        requests(&f, "POST", "/youtube/v3/playlists")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn search_unescapes_titles_adds_video_details_and_counts_quota() {
    let f = connected(SideEffectMode::Live).await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/search"))
        .and(query_param("type", "video"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"id": {"kind": "youtube#video", "videoId": "aaaaaaaaaaa"},
                       "snippet": {"title": "Rock &amp; Roll", "channelTitle": "Ch"}}],
            "nextPageToken": "CAoQAA", "pageInfo": {"totalResults": 1000}
        })))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"id": "aaaaaaaaaaa", "contentDetails": {"duration": "PT4M2S"}, "statistics": {"viewCount": "42"}}
        ]})))
        .mount(&f.server)
        .await;
    let out = f
        .youtube
        .search(omni_youtube::service::SearchParams {
            query: "rock".into(),
            kind: omni_youtube::model::SearchType::Video,
            order: None,
            channel_id: None,
            published_after: None,
            published_before: None,
            duration: None,
            region_code: None,
            relevance_language: None,
            limit: 10,
            page_token: None,
        })
        .await
        .unwrap();
    assert_eq!(out.results[0].title, "Rock & Roll");
    assert_eq!(out.results[0].duration_seconds, Some(242));
    assert_eq!(out.results[0].view_count, Some(42));
    assert_eq!((out.search_calls, out.pool_units), (1, 1));
    let quota = f.youtube.quota_status().await.unwrap();
    assert_eq!((quota.search_calls, quota.estimated_units_used), (1, 1));
}

#[tokio::test]
async fn an_unconfigured_client_reports_itself() {
    let server = MockServer::start().await;
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let youtube = Youtube::new(Deps {
        settings: Settings {
            client_id: None,
            client_secret: None,
            redirect_uri: "https://omni.syas.ca/api/youtube/oauth/callback".into(),
        },
        store: store.store.clone(),
        http: mock_http(&server, &["https://www.googleapis.com"]),
        sealer: Ok(Sealer::from_key(KEY)),
        mode: SideEffectMode::Live,
    });
    assert_eq!(
        youtube.status().await.unwrap().state,
        ConnectionState::NotConfigured
    );
    assert!(matches!(
        youtube.start_authorization().await,
        Err(YoutubeError::NotConfigured)
    ));
}

async fn get_on(youtube: &Youtube, host: &str, uri: &str) -> axum::response::Response {
    let request = Request::builder().uri(uri).header(header::HOST, host);
    let router = omni_youtube::routes::router(youtube.clone());
    match router.oneshot(request.body(Body::empty()).unwrap()).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

async fn get(youtube: &Youtube, uri: &str, fetch_site: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .uri(uri)
        .header(header::HOST, "omni.syas.ca");
    if let Some(site) = fetch_site {
        request = request.header("sec-fetch-site", site);
    }
    let router = omni_youtube::routes::router(youtube.clone());
    match router.oneshot(request.body(Body::empty()).unwrap()).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

#[tokio::test]
async fn connecting_starts_only_from_a_direct_navigation() {
    let f = fixture(SideEffectMode::Live).await;
    let cross = get(&f.youtube, "/api/youtube/oauth/start", Some("cross-site")).await;
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);
    let direct = get(&f.youtube, "/api/youtube/oauth/start", Some("none")).await;
    assert_eq!(direct.status(), StatusCode::SEE_OTHER);
    let location = direct.headers()[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
    let cookie = direct.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax")
    );
    let state = url::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    assert!(cookie.starts_with(&format!("omni_youtube_state={state};")));
    // Another browser (no cookie, or a different one) cannot finish this sign-in.
    let foreign = get(
        &f.youtube,
        &format!("/api/youtube/oauth/callback?code=x&state={state}"),
        Some("cross-site"),
    )
    .await;
    assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);
    let other_host = get_on(&f.youtube, "omni.boris", "/api/youtube/oauth/start").await;
    assert_eq!(other_host.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        other_host.headers()[header::LOCATION],
        "https://omni.syas.ca/api/youtube/oauth/start"
    );
    let stale = get(
        &f.youtube,
        "/api/youtube/oauth/callback?code=x&state=unknown",
        Some("cross-site"),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::BAD_REQUEST);
    assert_eq!(stale.headers()[header::REFERRER_POLICY], "no-referrer");
    let denied = get(
        &f.youtube,
        "/api/youtube/oauth/callback?error=access_denied",
        None,
    )
    .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
}
