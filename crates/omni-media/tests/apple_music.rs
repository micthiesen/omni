//! `media_apple_music_resolve` over a mock iTunes Lookup API: link parsing,
//! storefront and type handling, unsupported items, and provider failures.
//! Every response is checked against the tool's output schema by `typed_tool`.
#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;

use omni_http::public::PublicHttpClient;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_media::apple_music::AppleMusic;
use omni_media::mcp::media_tools;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SONG: u64 = 1_440_818_652;
const ALBUM: u64 = 1_440_818_584;
const ARTIST: u64 = 602_767_352;
const JP_ALBUM: u64 = 1_738_867_909;

fn song_result(storefront: &str) -> Value {
    json!({
        "wrapperType": "track", "kind": "song",
        "artistId": ARTIST, "collectionId": ALBUM, "trackId": SONG,
        "artistName": "Lorde", "collectionName": "Pure Heroine", "trackName": "Tennis Court",
        "artistViewUrl": format!("https://music.apple.com/{storefront}/artist/lorde/{ARTIST}?uo=4"),
        "collectionViewUrl": format!("https://music.apple.com/{storefront}/album/tennis-court/{ALBUM}?i={SONG}&uo=4"),
        "trackViewUrl": format!("https://music.apple.com/{storefront}/album/tennis-court/{ALBUM}?i={SONG}&uo=4"),
        "previewUrl": "https://audio-ssl.itunes.apple.com/preview.m4a",
        "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/Music211/x/100x100bb.jpg",
        "releaseDate": "2013-06-07T07:00:00Z",
        "trackExplicitness": "explicit", "collectionExplicitness": "explicit",
        "discCount": 1, "discNumber": 1, "trackCount": 10, "trackNumber": 1,
        "trackTimeMillis": 198_907, "country": "USA", "currency": "USD",
        "primaryGenreName": "Alternative", "isStreamable": true
    })
}

fn album_result() -> Value {
    json!({
        "wrapperType": "collection", "collectionType": "Album",
        "artistId": ARTIST, "collectionId": ALBUM,
        "artistName": "Lorde", "collectionName": "Pure Heroine",
        "artistViewUrl": format!("https://music.apple.com/us/artist/lorde/{ARTIST}?uo=4"),
        "collectionViewUrl": format!("https://music.apple.com/us/album/pure-heroine/{ALBUM}?uo=4"),
        "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/Music211/x/100x100bb.jpg",
        "collectionExplicitness": "explicit", "trackCount": 10,
        "releaseDate": "2013-09-27T07:00:00Z", "primaryGenreName": "Alternative"
    })
}

fn artist_result() -> Value {
    json!({
        "wrapperType": "artist", "artistType": "Artist", "artistName": "Lorde",
        "artistLinkUrl": format!("https://music.apple.com/us/artist/lorde/{ARTIST}?uo=4"),
        "artistId": ARTIST, "primaryGenreName": "Alternative"
    })
}

fn lookup(results: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/javascript; charset=utf-8")
        .set_body_string(json!({"resultCount": results.len(), "results": results}).to_string())
}

async fn mount(server: &MockServer, id: u64, storefront: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .and(query_param("id", id.to_string()))
        .and(query_param("country", storefront))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn tools(server: &MockServer) -> Vec<McpTool> {
    let mut h = common::Harness::new().await;
    h.services.apple_music = AppleMusic::new(
        PublicHttpClient::new(&omni_testkit::mock_http(
            server,
            &["https://itunes.apple.com"],
        ))
        .allow_loopback_for_tests(),
    );
    media_tools(Arc::new(h.services)).expect("tools")
}

async fn resolve(tools: &[McpTool], input: Value) -> Result<Value, ToolError> {
    let tool = tools
        .iter()
        .find(|t| t.meta.name == "media_apple_music_resolve")
        .expect("tool");
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    match tool.handler.call(input, cx).await? {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Ok(Value::Object(map)),
    }
}

async fn request_count(server: &MockServer) -> usize {
    server.received_requests().await.expect("requests").len()
}

#[tokio::test]
async fn an_album_link_with_i_resolves_the_song_with_clean_links() {
    let server = MockServer::start().await;
    mount(&server, SONG, "us", lookup(vec![song_result("us")])).await;
    let tools = tools(&server).await;
    let out = resolve(
        &tools,
        json!({"reference": format!("https://music.apple.com/us/album/tennis-court/{ALBUM}?i={SONG}&ls")}),
    )
    .await
    .expect("resolve");
    assert_eq!(out["status"], "found");
    assert_eq!(out["type"], "song");
    assert_eq!(out["storefrontSource"], "link");
    let item = &out["item"];
    assert_eq!(item["name"], "Tennis Court");
    assert_eq!(item["artistName"], "Lorde");
    assert_eq!(item["albumName"], "Pure Heroine");
    assert_eq!(item["albumId"], ALBUM);
    assert_eq!(item["durationMs"], 198_907);
    assert_eq!(item["releaseDate"], "2013-06-07");
    assert_eq!(item["explicit"], true);
    assert_eq!(
        item["url"],
        format!("https://music.apple.com/us/album/tennis-court/{ALBUM}?i={SONG}")
    );
    assert_eq!(
        item["albumUrl"],
        format!("https://music.apple.com/us/album/tennis-court/{ALBUM}")
    );
    assert_eq!(
        item["artistUrl"],
        format!("https://music.apple.com/us/artist/lorde/{ARTIST}")
    );
    assert!(item.get("previewUrl").is_none());
}

#[tokio::test]
async fn type_album_on_a_song_link_resolves_its_album() {
    let server = MockServer::start().await;
    mount(&server, ALBUM, "us", lookup(vec![album_result()])).await;
    let tools = tools(&server).await;
    let out = resolve(
        &tools,
        json!({"reference": format!("https://music.apple.com/us/album/x/{ALBUM}?i={SONG}"), "type": "album"}),
    )
    .await
    .expect("resolve");
    assert_eq!(out["status"], "found");
    assert_eq!(out["id"], ALBUM);
    assert_eq!(out["item"]["type"], "album");
    assert_eq!(out["item"]["trackCount"], 10);
    assert_eq!(
        out["item"]["url"],
        format!("https://music.apple.com/us/album/pure-heroine/{ALBUM}")
    );
}

#[tokio::test]
async fn a_bare_id_defaults_to_ca_and_reports_wrong_storefront_as_not_found() {
    let server = MockServer::start().await;
    mount(&server, JP_ALBUM, "ca", lookup(vec![])).await;
    let mut jp = album_result();
    jp["collectionId"] = json!(JP_ALBUM);
    jp["collectionViewUrl"] = json!(format!(
        "https://music.apple.com/jp/album/x/{JP_ALBUM}?uo=4"
    ));
    mount(&server, JP_ALBUM, "jp", lookup(vec![jp])).await;
    let tools = tools(&server).await;

    let missing = resolve(&tools, json!({"reference": JP_ALBUM.to_string()}))
        .await
        .expect("resolve");
    assert_eq!(missing["status"], "not_found");
    assert_eq!(missing["storefront"], "ca");
    assert_eq!(missing["storefrontSource"], "default");
    assert!(missing.get("item").is_none());
    assert!(
        missing["message"]
            .as_str()
            .expect("message")
            .contains("storefront")
    );

    let found = resolve(
        &tools,
        json!({"reference": JP_ALBUM.to_string(), "storefront": "JP"}),
    )
    .await
    .expect("resolve");
    assert_eq!(found["status"], "found");
    assert_eq!(found["storefront"], "jp");
    assert_eq!(found["storefrontSource"], "input");

    // An explicit storefront overrides the link's.
    let overridden = resolve(
        &tools,
        json!({"reference": format!("https://music.apple.com/jp/album/x/{JP_ALBUM}"), "storefront": "ca"}),
    )
    .await
    .expect("resolve");
    assert_eq!(overridden["status"], "not_found");
    assert_eq!(overridden["storefrontSource"], "input");
}

#[tokio::test]
async fn a_bare_id_of_another_kind_is_a_type_mismatch_with_the_item() {
    let server = MockServer::start().await;
    mount(&server, ARTIST, "ca", lookup(vec![artist_result()])).await;
    let tools = tools(&server).await;
    let out = resolve(
        &tools,
        json!({"reference": ARTIST.to_string(), "type": "album"}),
    )
    .await
    .expect("resolve");
    assert_eq!(out["status"], "type_mismatch");
    assert_eq!(out["type"], "album");
    assert_eq!(out["item"]["type"], "artist");
    assert_eq!(out["item"]["name"], "Lorde");
    assert_eq!(
        out["item"]["url"],
        format!("https://music.apple.com/us/artist/lorde/{ARTIST}")
    );

    let untyped = resolve(&tools, json!({"reference": ARTIST.to_string()}))
        .await
        .expect("resolve");
    assert_eq!(untyped["status"], "found");
    assert!(untyped.get("type").is_none());
}

#[tokio::test]
async fn a_link_kind_that_contradicts_type_is_an_input_error() {
    let server = MockServer::start().await;
    let tools = tools(&server).await;
    let error = resolve(
        &tools,
        json!({"reference": format!("https://music.apple.com/us/artist/lorde/{ARTIST}"), "type": "song"}),
    )
    .await
    .expect_err("conflict");
    assert_eq!(error.phase, ToolPhase::Input);
    assert_eq!(request_count(&server).await, 0);
}

#[tokio::test]
async fn playlists_short_links_and_non_music_ids_are_unsupported() {
    let server = MockServer::start().await;
    mount(
        &server,
        1_200_000_000,
        "ca",
        lookup(vec![json!({
            "wrapperType": "track", "kind": "podcast", "trackId": 1_200_000_000u64,
            "collectionId": 1_200_000_000u64, "trackName": "A Podcast"
        })]),
    )
    .await;
    let tools = tools(&server).await;

    let playlist = resolve(
        &tools,
        json!({"reference": "https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb"}),
    )
    .await
    .expect("resolve");
    assert_eq!(playlist["status"], "unsupported");
    assert_eq!(playlist["unsupportedKind"], "playlist");
    assert_eq!(playlist["storefront"], "us");
    let short = resolve(&tools, json!({"reference": "https://apple.co/3xYz"}))
        .await
        .expect("resolve");
    assert_eq!(short["unsupportedKind"], "short_link");
    assert_eq!(request_count(&server).await, 0);

    let podcast = resolve(&tools, json!({"reference": "1200000000"}))
        .await
        .expect("resolve");
    assert_eq!(podcast["status"], "unsupported");
    assert_eq!(podcast["unsupportedKind"], "podcast");
    assert!(podcast.get("item").is_none());
}

#[tokio::test]
async fn non_apple_links_are_rejected_without_a_request() {
    let server = MockServer::start().await;
    let tools = tools(&server).await;
    for reference in [
        "https://open.spotify.com/track/abc",
        "https://music.apple.com.attacker.example/us/album/x/1",
        "http://10.0.0.1/us/album/x/1",
        "https://music.apple.com/us/album/no-id",
    ] {
        let error = resolve(&tools, json!({"reference": reference}))
            .await
            .expect_err(reference);
        assert_eq!(error.phase, ToolPhase::Input, "{reference}");
    }
    let bad_storefront = resolve(&tools, json!({"reference": "1", "storefront": "usa"}))
        .await
        .expect_err("storefront");
    assert_eq!(bad_storefront.phase, ToolPhase::Input);
    assert_eq!(request_count(&server).await, 0);
}

#[tokio::test]
async fn provider_failures_are_errors_not_empty_results() {
    let server = MockServer::start().await;
    mount(
        &server,
        SONG,
        "zz",
        ResponseTemplate::new(400).set_body_string(
            r#"{"errorMessage":"Invalid value(s) for key(s): [country]","queryParameters":{}}"#,
        ),
    )
    .await;
    mount(&server, SONG, "ca", ResponseTemplate::new(503)).await;
    mount(
        &server,
        SONG,
        "gb",
        ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"),
    )
    .await;
    let tools = tools(&server).await;

    let unknown = resolve(
        &tools,
        json!({"reference": SONG.to_string(), "storefront": "zz"}),
    )
    .await
    .expect_err("storefront");
    assert_eq!(unknown.phase, ToolPhase::Input);
    assert!(
        unknown.message.contains("storefront"),
        "{}",
        unknown.message
    );

    let outage = resolve(&tools, json!({"reference": SONG.to_string()}))
        .await
        .expect_err("503");
    assert_eq!(outage.phase, ToolPhase::Execute);
    assert!(outage.message.contains("HTTP 503"), "{}", outage.message);

    let garbage = resolve(
        &tools,
        json!({"reference": SONG.to_string(), "storefront": "gb"}),
    )
    .await
    .expect_err("garbage");
    assert_eq!(garbage.phase, ToolPhase::Execute);
}

#[tokio::test]
async fn a_result_for_another_id_is_not_a_match() {
    let server = MockServer::start().await;
    mount(&server, SONG, "ca", lookup(vec![album_result()])).await;
    let tools = tools(&server).await;
    let out = resolve(&tools, json!({"reference": SONG.to_string()}))
        .await
        .expect("resolve");
    assert_eq!(out["status"], "not_found");
}

#[tokio::test]
async fn a_music_video_link_resolves_with_its_artist() {
    let server = MockServer::start().await;
    mount(
        &server,
        1_445_856_407,
        "us",
        lookup(vec![json!({
            "wrapperType": "track", "kind": "music-video", "artistId": ARTIST,
            "trackId": 1_445_856_407u64, "artistName": "Lorde", "trackName": "Royals",
            "artistViewUrl": format!("https://music.apple.com/us/artist/lorde/{ARTIST}?uo=4"),
            "trackViewUrl": "https://music.apple.com/us/music-video/royals-edit/1445856407?uo=4",
            "trackTimeMillis": 199_000, "trackExplicitness": "notExplicit"
        })]),
    )
    .await;
    let tools = tools(&server).await;
    let out = resolve(
        &tools,
        json!({"reference": "https://music.apple.com/us/music-video/royals-edit/1445856407"}),
    )
    .await
    .expect("resolve");
    assert_eq!(out["status"], "found");
    assert_eq!(out["item"]["type"], "music_video");
    assert_eq!(out["item"]["explicit"], false);
    assert!(out["item"].get("albumUrl").is_none());
    assert_eq!(
        out["item"]["url"],
        "https://music.apple.com/us/music-video/royals-edit/1445856407"
    );
}

#[tokio::test]
async fn rate_limits_other_400s_and_link_storefronts_are_reported_distinctly() {
    let server = MockServer::start().await;
    mount(&server, SONG, "ca", ResponseTemplate::new(429)).await;
    mount(
        &server,
        SONG,
        "gb",
        ResponseTemplate::new(400)
            .set_body_string(r#"{"errorMessage":"Invalid value(s) for key(s): [itunesId]"}"#),
    )
    .await;
    mount(
        &server,
        SONG,
        "zz",
        ResponseTemplate::new(400)
            .set_body_string(r#"{"errorMessage":"Invalid value(s) for key(s): [country]"}"#),
    )
    .await;
    let tools = tools(&server).await;

    let limited = resolve(&tools, json!({"reference": SONG.to_string()}))
        .await
        .expect_err("429");
    assert_eq!(limited.phase, ToolPhase::Execute);
    assert!(
        limited.message.contains("rate limiting"),
        "{}",
        limited.message
    );

    let other = resolve(
        &tools,
        json!({"reference": SONG.to_string(), "storefront": "gb"}),
    )
    .await
    .expect_err("400");
    assert_eq!(other.phase, ToolPhase::Execute);
    assert!(other.message.contains("[itunesId]"), "{}", other.message);

    let from_link = resolve(
        &tools,
        json!({"reference": format!("https://music.apple.com/zz/song/x/{SONG}")}),
    )
    .await
    .expect_err("link storefront");
    assert_eq!(from_link.phase, ToolPhase::Input);
    assert!(
        from_link.message.contains("from the link"),
        "{}",
        from_link.message
    );
}
