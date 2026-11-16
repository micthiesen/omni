//! The X retriever against a local mock of the
//! FxTwitter API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use omni_ai::costs::CostRecorder;
use omni_presspods::retrievers::x::{
    XArticleBlock, XRetriever, XStatusUrl, article_blocks_to_markdown, parse_x_status_url,
    thread_title,
};
use omni_presspods::retrievers::{ArticleRetriever, RetrieverContext};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, test_clock};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ROOT_ID: &str = "2079904005652893709";
const ROOT_TEXT: &str = "Dinitz-Garg-Goemans conjecture is false. This graph theory problem was open for ~30 years.

The graph below has fractional flow cost 58. Any unsplittable flow (with capacity violation <=15) has cost at least 60.

Chat with GPT 5.6 Pro where this was found: https://chatgpt.com/share/6a60b2eb-0b64-83ee-9c76-7931ca1de063";
const SECOND_TEXT: &str = "I know counterexamples to old conjectures are becoming a meme at this point. But I really cared about this problem and spent many weeks thinking about it a while ago (in both directions, proof and disproof).

I think almost all graph flows experts thought about this problem.";
const THIRD_TEXT: &str = "The conjecture was based on absolutely stunning result of Dinitz, Garg, and Goemans: any fractional flow can be routed to unsplittable flow by violating graph capacities by at most max(demand).

The chat with gpt pro here is an absolute meme";

fn author() -> Value {
    json!({ "id": "author-1", "name": "Dmitry Rybin", "screen_name": "DmitryRybin1" })
}

async fn retriever(server: &MockServer) -> (XRetriever, TestStore) {
    let clock: omni_core::clock::SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let http = mock_http(server, &["https://api.fxtwitter.com"]);
    let ctx = Arc::new(RetrieverContext {
        public_http: omni_http::public::PublicHttpClient::new(&http).allow_loopback_for_tests(),
        http,
        jina_api_key: None,
        renderer: None,
        costs: CostRecorder::new(store.store.clone(), clock),
        tz: jiff::tz::TimeZone::UTC,
    });
    (XRetriever(ctx), store)
}

async fn serve(id: &str, status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/2/thread/{id}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn extracts_a_complete_x_article_with_headings_footer_and_metadata() {
    let status = json!({
        "id": "2077031491045929255",
        "type": "status",
        "text": "",
        "created_at": "Tue Jul 14 14:04:31 +0000 2026",
        "author": { "id": "author-ed", "name": "Ed Elson", "screen_name": "edels0n" },
        "media": { "photos": [{ "url": "https://pbs.twimg.com/media/fallback.jpg" }] },
        "article": {
            "title": "The Analysts Are Compromised",
            "created_at": "2026-07-14T14:04:31.000Z",
            "cover_media": { "media_info": { "original_img_url": "https://pbs.twimg.com/media/cover.jpg" } },
            "content": { "blocks": [
                { "type": "unstyled", "text": "The real reason Wall Street loves SpaceX" },
                { "type": "unstyled", "text": "Twenty-three years ago, a scandal emerged on Wall Street. Henry Blodget turned out to be privately bearish." },
                { "type": "atomic", "text": " " },
                { "type": "header-two", "text": "Déjà Vu" },
                { "type": "unstyled", "text": "According to JPMorgan, SpaceX is worth $2.9 trillion." },
                { "type": "divider", "text": "ignored" },
                { "type": "header-three", "text": "Here To Stay" },
                { "type": "unstyled", "text": "The solution is simple: Fix the incentives. Don’t hold your breath." },
                { "type": "unstyled", "text": "See you next week,\n\nEd" },
                { "type": "unstyled", "text": "Subscribe to Simply Put by Ed Elson on Substack.\n\nA newsletter about business and tech, from the host of Prof G Markets.\n\nOut every Tuesday." }
            ] }
        }
    });
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2/thread/2077031491045929255"))
        .and(header("user-agent", "PressPods Test"))
        .and(header("accept", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "code": 200, "status": status, "thread": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (x, _store) = retriever(&server).await;
    let result = x
        .retrieve(
            "https://x.com/edels0n/status/2077031491045929255?s=46&t=tracking",
            "PressPods Test",
        )
        .await
        .unwrap();
    assert_eq!(
        result.title.as_deref(),
        Some("The Analysts Are Compromised")
    );
    assert_eq!(
        result.text,
        "The real reason Wall Street loves SpaceX

Twenty-three years ago, a scandal emerged on Wall Street. Henry Blodget turned out to be privately bearish.

## Déjà Vu

According to JPMorgan, SpaceX is worth $2.9 trillion.

### Here To Stay

The solution is simple: Fix the incentives. Don’t hold your breath.

See you next week,

Ed

Subscribe to Simply Put by Ed Elson on Substack.

A newsletter about business and tech, from the host of Prof G Markets.

Out every Tuesday."
    );
    assert_eq!(result.author.as_deref(), Some("Ed Elson"));
    assert_eq!(result.domain.as_deref(), Some("x.com"));
    assert_eq!(
        result.url,
        "https://x.com/edels0n/status/2077031491045929255"
    );
    assert_eq!(result.published_at, Some(1_784_037_871_000));
    assert_eq!(
        result.lead_image_url.as_deref(),
        Some("https://pbs.twimg.com/media/cover.jpg")
    );
    assert!(!result.text.contains("The Analysts Are Compromised"));
}

#[tokio::test]
async fn assembles_only_the_root_authors_unique_non_tombstone_thread_posts() {
    let root = json!({
        "id": ROOT_ID,
        "type": "status",
        "text": ROOT_TEXT,
        "created_timestamp": 1_789_293_021,
        "author": author(),
        "media": { "photos": [{
            "url": "https://pbs.twimg.com/media/graph.jpg?name=orig",
            "altText": "A graph showing the flow counterexample"
        }] }
    });
    let server = serve(
        ROOT_ID,
        200,
        json!({
            "code": 200,
            "status": root,
            "thread": [
                root,
                { "id": "reply", "text": "Unrelated reply", "author": { "id": "someone-else" } },
                { "type": "tombstone", "text": "This post was deleted" },
                { "id": "second", "text": SECOND_TEXT, "author": author(), "media": {} },
                root,
                { "id": "third", "text": THIRD_TEXT, "author": author(), "media": {} }
            ]
        }),
    )
    .await;
    let (x, _store) = retriever(&server).await;
    let result = x
        .retrieve(
            &format!("https://x.com/DmitryRybin1/status/{ROOT_ID}"),
            "PressPods Test",
        )
        .await
        .unwrap();
    assert_eq!(
        result.text,
        format!(
            "{ROOT_TEXT}\n\nImage description: A graph showing the flow counterexample\n\n{SECOND_TEXT}\n\n{THIRD_TEXT}"
        )
    );
    assert_eq!(
        result.title.as_deref(),
        Some(
            "Dinitz-Garg-Goemans conjecture is false. This graph theory problem was open for ~30 years."
        )
    );
    assert_eq!(result.author.as_deref(), Some("Dmitry Rybin"));
    assert_eq!(
        result.url,
        format!("https://x.com/DmitryRybin1/status/{ROOT_ID}")
    );
    assert_eq!(result.published_at, Some(1_789_293_021_000));
    assert_eq!(
        result.lead_image_url.as_deref(),
        Some("https://pbs.twimg.com/media/graph.jpg?name=orig")
    );
    assert!(!result.text.contains("Unrelated reply"));
    assert!(!result.text.contains("This post was deleted"));
}

#[tokio::test]
async fn uses_the_status_alone_when_the_api_has_no_thread() {
    let server = serve(
        ROOT_ID,
        200,
        json!({ "code": 200, "status": { "id": ROOT_ID, "text": ROOT_TEXT, "author": author(), "media": {} } }),
    )
    .await;
    let (x, _store) = retriever(&server).await;
    let result = x
        .retrieve(
            &format!("https://twitter.com/DmitryRybin1/status/{ROOT_ID}"),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(result.text, ROOT_TEXT);
    assert_eq!(
        result.url,
        format!("https://x.com/DmitryRybin1/status/{ROOT_ID}")
    );
}

#[tokio::test]
async fn falls_back_to_connected_thread_text_for_an_empty_x_article_body() {
    let root = json!({
        "id": ROOT_ID,
        "text": "",
        "author": author(),
        "article": { "title": "Media-only article", "content": { "blocks": [{ "type": "atomic", "text": " " }] } }
    });
    let server = serve(
        ROOT_ID,
        200,
        json!({ "code": 200, "status": root, "thread": [root, { "id": "continuation", "text": SECOND_TEXT, "author": author() }] }),
    )
    .await;
    let (x, _store) = retriever(&server).await;
    let result = x
        .retrieve(&format!("https://x.com/i/web/status/{ROOT_ID}"), "test")
        .await
        .unwrap();
    assert_eq!(result.text, SECOND_TEXT);
    assert_eq!(result.title.as_deref(), Some("Media-only article"));
}

#[tokio::test]
async fn reports_url_http_and_api_errors_clearly() {
    assert!(
        parse_x_status_url("https://x.com/DmitryRybin1")
            .unwrap_err()
            .contains("not a status permalink")
    );
    assert!(
        parse_x_status_url("https://example.com/user/status/123")
            .unwrap_err()
            .contains("Unsupported X URL hostname")
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    let (x, _store) = retriever(&server).await;
    let error = x
        .retrieve("https://x.com/user/status/123", "test")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("HTTP 503"), "{error}");

    let server = serve(
        "123",
        200,
        json!({ "code": 404, "message": "Tweet not found" }),
    )
    .await;
    let (x, _store) = retriever(&server).await;
    let error = x
        .retrieve("https://x.com/user/status/123", "test")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("code 404: Tweet not found"),
        "{error}"
    );
}

#[test]
fn accepts_mobile_and_media_permalink_variants() {
    assert_eq!(
        parse_x_status_url(&format!(
            "https://mobile.twitter.com/user/status/{ROOT_ID}/photo/1"
        ))
        .unwrap(),
        XStatusUrl {
            id: ROOT_ID.into(),
            screen_name: "user".into(),
            canonical_url: format!("https://x.com/user/status/{ROOT_ID}"),
        }
    );
}

#[test]
fn does_not_split_a_word_when_shortening_a_thread_title() {
    assert_eq!(
        thread_title(&"word ".repeat(40), 40).as_deref(),
        Some("word word word word word word word…")
    );
}

#[test]
fn preserves_only_prose_and_supported_headings_from_article_blocks() {
    let block = |kind: &str, text: &str| XArticleBlock {
        text: Some(text.into()),
        kind: Some(kind.into()),
    };
    assert_eq!(
        article_blocks_to_markdown(&[
            block("header-two", "Section"),
            block("atomic", "media placeholder"),
            block("unstyled", "Body"),
        ]),
        "## Section\n\nImage description: media placeholder\n\nBody"
    );
}
