//! Profile link extraction and fetching, including redirect refusal. reqwest
//! releases the connection when the response or the future is dropped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_live::identity::{binding_key, canonical_binding_key};
use omni_live::profile_links::{
    LearnInput, ProfileFetcher, ProfileIdentityEvidence, ProfileIdentityLearner,
    binding_from_profile_url, extract_profile_links, profile_identity_evidence, profile_page_url,
};
use omni_live::{Platform, PlatformBinding};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

const UA: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";

fn source() -> PlatformBinding {
    PlatformBinding::new(Platform::YouTube, "aBcdEFgh_12")
}

fn target() -> PlatformBinding {
    PlatformBinding::new(Platform::YouTube, "@LonerBoxLive")
}

fn metadata() -> Value {
    json!({"type": "video", "author_name": "LonerBox Live", "author_url": "https://www.youtube.com/@lonerboxlive"})
}

struct Harness {
    server: wiremock::MockServer,
    store: omni_testkit::TestStore,
    learner: ProfileIdentityLearner,
    fetcher: ProfileFetcher,
}

async fn harness() -> Harness {
    let server = omni_testkit::mock_server().await;
    let (store, _clock) = common::test_store(0).await;
    let http = omni_testkit::mock_http(
        &server,
        &[
            "https://www.youtube.com",
            "https://kick.com",
            "https://www.twitch.tv",
        ],
    );
    let fetcher = ProfileFetcher::new(http);
    let learner = ProfileIdentityLearner::new(store.store.clone(), fetcher.clone());
    Harness {
        server,
        store,
        learner,
        fetcher,
    }
}

fn input(now: i64, force_refresh: bool) -> LearnInput {
    LearnInput {
        source: source(),
        configured_bindings: vec![target()],
        now,
        force_refresh,
    }
}

#[tokio::test]
async fn learns_the_configured_owner_from_oembed_and_revalidates_only_when_requested() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .and(query_param(
            "url",
            "https://www.youtube.com/watch?v=aBcdEFgh_12",
        ))
        .and(query_param("format", "json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata()))
        .expect(2)
        .mount(&h.server)
        .await;
    let first = h
        .learner
        .learn_identity(input(100, false))
        .await
        .unwrap()
        .unwrap();
    let cached = h
        .learner
        .learn_identity(input(200, false))
        .await
        .unwrap()
        .unwrap();
    let refreshed = h
        .learner
        .learn_identity(input(300, true))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.source_binding, "youtube:aBcdEFgh_12");
    assert_eq!(first.target_binding, "youtube:@lonerboxlive");
    assert_eq!((first.discovered_at, first.verified_at), (100, 100));
    assert_eq!(cached, first);
    assert_eq!((refreshed.discovered_at, refreshed.verified_at), (100, 300));
    let requests = h.server.received_requests().await.unwrap();
    assert_header(&requests[0], "user-agent", UA);
    assert_header(&requests[0], "accept", "application/json");
    drop(h.store);
}

#[tokio::test]
async fn returns_confirmed_no_match_when_a_cached_owner_no_longer_matches_on_refresh() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata()))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "video", "author_url": "https://youtube.com/@different"
        })))
        .mount(&h.server)
        .await;
    assert!(
        h.learner
            .learn_identity(input(1, false))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        h.learner
            .learn_identity(input(2, true))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn ignores_matching_names_and_unconfigured_or_non_owner_url() {
    for author_url in [
        "https://www.youtube.com/@someoneelse",
        "https://twitch.tv/lonerboxlive",
        "https://example.com/@lonerboxlive",
        "https://www.youtube.com/watch?v=aBcdEFgh_12",
    ] {
        let h = harness().await;
        let mut body = metadata();
        body["author_url"] = json!(author_url);
        Mock::given(method("GET"))
            .and(path("/oembed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&h.server)
            .await;
        assert!(
            h.learner
                .learn_identity(input(1, false))
                .await
                .unwrap()
                .is_none(),
            "{author_url}"
        );
    }
}

#[tokio::test]
async fn propagates_http_and_metadata_failures() {
    let responses = [
        ResponseTemplate::new(500).set_body_string("unavailable"),
        ResponseTemplate::new(200).set_body_string("not JSON"),
        ResponseTemplate::new(200)
            .set_body_json(json!({"type": "video", "author_name": "LonerBox Live"})),
        ResponseTemplate::new(200).set_body_json(
            json!({"type": "link", "author_url": "https://www.youtube.com/@lonerboxlive"}),
        ),
    ];
    for response in responses {
        let h = harness().await;
        Mock::given(method("GET"))
            .and(path("/oembed"))
            .respond_with(response)
            .mount(&h.server)
            .await;
        let error = h.learner.learn_identity(input(1, false)).await.unwrap_err();
        assert!(
            error.to_string().contains("fetch YouTube video owner"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn bounds_owner_responses_and_rejects_malformed_video_identifiers_before_fetching() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata()))
        .expect(1)
        .mount(&h.server)
        .await;
    let bounded = h.fetcher.clone().with_max_bytes(10);
    let error = bounded
        .fetch_youtube_video_owner("aBcdEFgh_12")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("10 byte limit"), "{error}");
    for id in ["@lonerboxlive", "abc", "abcdefghij/", "abcdefghijk?url=x"] {
        assert_eq!(h.fetcher.fetch_youtube_video_owner(id).await.unwrap(), None);
    }
}

#[test]
fn extracts_direct_supported_profiles_from_anchors_and_structured_json() {
    let html = r#"
      <a href="https://kick.com/ImReallyImportant?ref=youtube">Kick</a>
      <a href="https://www.youtube.com/redirect?q=https%3A%2F%2Fwww.twitch.tv%2FIRI_live">Twitch</a>
      <script>{"url":"https:\/\/www.youtube.com\/@ImReallyImportant"}</script>
      <a href="https://example.com/links">Elsewhere</a>
    "#;
    let mut keys: Vec<String> = extract_profile_links(html)
        .iter()
        .map(binding_key)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "kick:imreallyimportant",
            "twitch:iri_live",
            "youtube:@imreallyimportant"
        ]
    );
}

#[test]
fn rejects_videos_categories_malformed_handles_and_redirect_destinations_on_other_hosts() {
    for url in [
        "https://youtube.com/watch?v=abc",
        "https://youtube.com/@iri/live",
        "https://kick.com/categories/games",
        "https://twitch.tv/videos/123",
        "https://evil.test/redirect?q=https://kick.com/iri",
        "https://kick.com:444/iri",
        "https://attacker@kick.com/iri",
        "javascript:https://kick.com/iri",
    ] {
        assert_eq!(binding_from_profile_url(url), None, "{url}");
    }
}

#[test]
fn builds_only_canonical_bounded_profile_page_urls() {
    assert_eq!(
        profile_page_url(&PlatformBinding::new(Platform::YouTube, "@IRI")).as_deref(),
        Some("https://www.youtube.com/@iri/about")
    );
    assert_eq!(
        profile_page_url(&PlatformBinding::new(Platform::Kick, "IRI")).as_deref(),
        Some("https://kick.com/iri")
    );
    assert_eq!(
        profile_page_url(&PlatformBinding::new(Platform::YouTube, "channel/UC123")).as_deref(),
        Some("https://www.youtube.com/channel/UC123/about")
    );
}

fn kick_source() -> PlatformBinding {
    PlatformBinding::new(Platform::Kick, "imreallyimportant")
}

#[test]
fn accepts_a_direct_configured_link_when_normalized_handles_match() {
    let same = PlatformBinding::new(Platform::YouTube, "@ImReallyImportant");
    assert_eq!(
        profile_identity_evidence(&kick_source(), &same, std::slice::from_ref(&same), &[]),
        Some(ProfileIdentityEvidence::EqualHandle)
    );
}

#[test]
fn requires_a_reciprocal_link_when_handles_differ() {
    let different = PlatformBinding::new(Platform::YouTube, "@IRI");
    assert_eq!(
        profile_identity_evidence(
            &kick_source(),
            &different,
            std::slice::from_ref(&different),
            &[]
        ),
        None
    );
    assert_eq!(
        profile_identity_evidence(
            &kick_source(),
            &different,
            std::slice::from_ref(&different),
            &[kick_source()]
        ),
        Some(ProfileIdentityEvidence::Reciprocal)
    );
}

#[test]
fn never_accepts_handle_equality_without_a_direct_profile_link() {
    let same = PlatformBinding::new(Platform::YouTube, "@ImReallyImportant");
    assert_eq!(
        profile_identity_evidence(&kick_source(), &same, &[], &[kick_source()]),
        None
    );
}

#[tokio::test]
async fn uses_the_repo_user_agent_and_parses_the_bounded_response() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/iri"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="https://youtube.com/@IRI">IRI</a>"#),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let links = h
        .fetcher
        .fetch_profile_links(&PlatformBinding::new(Platform::Kick, "iri"))
        .await
        .unwrap();
    let keys: Vec<String> = links.iter().map(binding_key).collect();
    assert_eq!(keys, ["youtube:@iri"]);
    let requests = h.server.received_requests().await.unwrap();
    assert_header(&requests[0], "user-agent", UA);
    assert_header(&requests[0], "accept", "text/html,application/xhtml+xml");
}

fn assert_header(request: &wiremock::Request, name: &str, expected: &str) {
    let value = request.headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(value, Some(expected), "{name}");
}

#[tokio::test]
async fn stops_reading_responses_beyond_the_byte_cap() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/iri"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(20)))
        .mount(&h.server)
        .await;
    let error = h
        .fetcher
        .clone()
        .with_max_bytes(10)
        .fetch_profile_links(&PlatformBinding::new(Platform::Kick, "iri"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("10 byte limit"), "{error}");
}

#[tokio::test]
async fn refuses_redirects() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/iri"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "https://evil.test/"))
        .mount(&h.server)
        .await;
    assert!(
        h.fetcher
            .fetch_profile_links(&PlatformBinding::new(Platform::Kick, "iri"))
            .await
            .is_err()
    );
    assert_eq!(
        canonical_binding_key(Platform::YouTube, " channel/UC123/extra "),
        "youtube:channel/UC123"
    );
}
