//! `podcast_transcript_search` and `podcast_transcript_get` over Exa's podcast
//! index. A local wiremock server stands in for `api.exa.ai`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use omni_ai::costs::{CostEventData, CostRecorder};
use omni_core::clock::SharedClock;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_podcasts::castro::NoAccount;
use omni_podcasts::exa::ExaPodcasts;
use omni_podcasts::mcp::{McpState, tools};
use omni_store::EntityOps;
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, mock_server, test_clock};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EPISODE: &str = "https://exa.ai/library/podcast/tpr3b0f359y/episode/s20kmckm3v9";
const SHOW: &str = "https://exa.ai/library/podcast/tpr3b0f359y";

struct Setup {
    store: TestStore,
    server: MockServer,
    tools: Vec<McpTool>,
}

async fn setup(configured: bool) -> Setup {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let server = mock_server().await;
    let exa = configured.then(|| {
        Arc::new(ExaPodcasts::with_http(
            mock_http(&server, &["https://api.exa.ai"]),
            "exa-test".to_owned(),
            CostRecorder::new(store.store.clone(), clock.clone()),
        ))
    });
    let tools = tools(McpState {
        store: store.store.clone(),
        clock,
        accounts: Arc::new(NoAccount),
        directory: Arc::new(omni_podcasts::sources::OfflineDirectory),
        public_url: "https://recs.example".to_owned(),
        exa,
    })
    .unwrap();
    Setup {
        store,
        server,
        tools,
    }
}

impl Setup {
    async fn call(&self, name: &str, input: Value) -> Result<Map<String, Value>, ToolError> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        let cx = ToolContext {
            call_id: "test".into(),
            cancel: CancellationToken::new(),
        };
        match tool.handler.call(input, cx).await? {
            ToolOutput::Structured(map) => Ok(map),
            ToolOutput::Custom { structured, .. } => Ok(structured),
        }
    }

    async fn costs(&self) -> Vec<CostEventData> {
        self.store
            .store
            .read(|docs| docs.get_all::<CostEventData>())
            .await
            .unwrap()
    }

    async fn contents_reply(&self, reply: ResponseTemplate, times: u64) {
        Mock::given(method("POST"))
            .and(path("/contents"))
            .and(header("x-api-key", "exa-test"))
            .respond_with(reply)
            .expect(times)
            .mount(&self.server)
            .await;
    }
}

fn transcript() -> String {
    // The text format is Exa's; Omni passes it through untouched.
    let mut text = String::from("Carla Lalli Says Food Is A Feeling\n");
    for i in 0..400 {
        text.push_str(&format!("Jay Lee {i}:00\nTurn {i} about pricing.\n"));
    }
    text
}

fn contents_body(text: &str) -> Value {
    json!({
        "requestId": "req-contents",
        "results": [{
            "id": EPISODE,
            "url": EPISODE,
            "title": "Carla Lalli Says Food Is A Feeling, from Expense Account",
            "publishedDate": "2026-09-23T00:00:00.000Z",
            "author": null,
            "text": text,
            "extras": { "links": ["https://feed.therecipepodcast.com/", "https://open.spotify.com/show/x"] }
        }],
        "statuses": [{ "id": EPISODE, "status": "success", "source": "cached" }],
        "costDollars": { "total": 0.001 }
    })
}

#[tokio::test]
async fn search_is_limited_to_the_podcast_index_and_preserves_provenance() {
    let s = setup(true).await;
    let long = "x".repeat(700);
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("x-api-key", "exa-test"))
        .and(body_json(json!({
            "query": "founders discuss pricing mistakes",
            "type": "auto",
            "numResults": 5,
            "includeDomains": ["exa.ai/library/podcast"],
            "startPublishedDate": "2026-09-01T00:00:00Z",
            "contents": { "highlights": { "maxCharacters": 600 } },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "requestId": "req-search",
            "results": [
                {
                    "id": EPISODE,
                    "url": EPISODE,
                    "title": "Carla Lalli Says Food Is A Feeling",
                    "publishedDate": "2026-09-23T00:00:00.000Z",
                    "author": null,
                    "highlights": ["Jay Lee: pricing", long, "c", "d"]
                },
                { "url": SHOW, "title": "The Recipe with Kenji and Deb", "highlights": null },
                { "url": "https://exa.ai/library/podcast/", "title": "Index" },
                { "url": "https://example.com/blog", "title": "Not a podcast" }
            ],
            "costDollars": { "total": 0.005 }
        })))
        .expect(1)
        .mount(&s.server)
        .await;

    let out = s
        .call(
            "podcast_transcript_search",
            json!({ "query": "  founders discuss pricing mistakes ", "publishedAfter": "2026-09-01" }),
        )
        .await
        .unwrap();

    assert_eq!(out["index"], "exa.ai/library/podcast");
    assert_eq!(out["requestId"], "req-search");
    assert_eq!(out["droppedResults"], 1);
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let episode = &results[0];
    assert_eq!(episode["kind"], "episode");
    assert_eq!(episode["showId"], "tpr3b0f359y");
    assert_eq!(episode["episodeId"], "s20kmckm3v9");
    assert_eq!(episode["publishedDate"], "2026-09-23T00:00:00.000Z");
    assert_eq!(episode["author"], Value::Null);
    let highlights = episode["highlights"].as_array().unwrap();
    assert_eq!(highlights.len(), 3);
    assert_eq!(highlights[0], "Jay Lee: pricing");
    assert_eq!(highlights[1].as_str().unwrap().chars().count(), 600);
    assert_eq!(episode["highlightsTruncated"], true);
    assert_eq!(results[1]["kind"], "show");
    assert_eq!(results[1]["episodeId"], Value::Null);
    assert_eq!(results[1]["highlights"], json!([]));
    assert_eq!(results[2]["kind"], "other");
    assert_eq!(results[2]["showId"], Value::Null);

    let costs = s.costs().await;
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].service, "exa");
    assert_eq!(costs[0].operation, "search");
    assert_eq!(costs[0].cost_cents, Some(0.5));
}

#[tokio::test]
async fn search_rejects_an_unparseable_date_before_calling_exa() {
    let s = setup(true).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s.server)
        .await;
    let error = s
        .call(
            "podcast_transcript_search",
            json!({ "query": "pricing", "publishedBefore": "last week" }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.phase, ToolPhase::Input));
    assert!(
        error.message.contains("publishedBefore"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn both_tools_report_missing_configuration() {
    let s = setup(false).await;
    for (name, input) in [
        ("podcast_transcript_search", json!({ "query": "pricing" })),
        ("podcast_transcript_get", json!({ "url": EPISODE })),
    ] {
        let error = s.call(name, input).await.unwrap_err();
        assert!(matches!(error.phase, ToolPhase::Execute));
        assert!(error.message.contains("EXA_API_KEY"), "{}", error.message);
    }
}

#[tokio::test]
async fn get_pages_through_one_cached_read_of_the_verbatim_transcript() {
    let s = setup(true).await;
    let text = transcript();
    let total = text.chars().count();
    Mock::given(method("POST"))
        .and(path("/contents"))
        .and(body_json(json!({
            "urls": [EPISODE],
            "text": { "maxCharacters": 1_000_001 },
            "extras": { "links": 51 },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(contents_body(&text)))
        .expect(1)
        .mount(&s.server)
        .await;

    let first = s
        .call(
            "podcast_transcript_get",
            json!({ "url": format!("{EPISODE}?utm=x"), "maxChars": 5000 }),
        )
        .await
        .unwrap();
    assert_eq!(first["status"], "available");
    assert_eq!(first["url"], EPISODE);
    assert_eq!(first["kind"], "episode");
    assert_eq!(first["publishedDate"], "2026-09-23T00:00:00.000Z");
    assert_eq!(first["contentSource"], "cached");
    assert_eq!(first["requestId"], "req-contents");
    assert_eq!(first["totalChars"], total);
    assert_eq!(first["returnedChars"], 5000);
    assert_eq!(first["nextOffset"], 5000);
    assert_eq!(first["sourceTruncated"], false);
    assert_eq!(
        first["links"],
        json!([
            "https://feed.therecipepodcast.com/",
            "https://open.spotify.com/show/x"
        ])
    );

    let mut joined = first["text"].as_str().unwrap().to_owned();
    let mut offset = first["nextOffset"].as_u64();
    while let Some(next) = offset {
        let page = s
            .call(
                "podcast_transcript_get",
                json!({ "url": EPISODE, "offset": next, "maxChars": 5000 }),
            )
            .await
            .unwrap();
        joined.push_str(page["text"].as_str().unwrap());
        offset = page["nextOffset"].as_u64();
    }
    assert_eq!(joined, text);
    assert_eq!(s.costs().await.len(), 1);

    let past_end = s
        .call(
            "podcast_transcript_get",
            json!({ "url": EPISODE, "offset": total + 1 }),
        )
        .await
        .unwrap_err();
    assert!(matches!(past_end.phase, ToolPhase::Input));
}

#[tokio::test]
async fn get_reports_exa_page_errors_as_unavailable_and_retries_later() {
    let s = setup(true).await;
    s.contents_reply(
        ResponseTemplate::new(200).set_body_json(json!({
            "requestId": "req-missing",
            "results": [],
            "statuses": [{
                "id": EPISODE,
                "status": "error",
                "source": "crawled",
                "error": { "tag": "CRAWL_NOT_FOUND", "httpStatusCode": 404 }
            }]
        })),
        2,
    )
    .await;
    for _ in 0..2 {
        let out = s
            .call("podcast_transcript_get", json!({ "url": EPISODE }))
            .await
            .unwrap();
        assert_eq!(
            Value::Object(out),
            json!({
                "status": "unavailable",
                "url": EPISODE,
                "kind": "episode",
                "showId": "tpr3b0f359y",
                "episodeId": "s20kmckm3v9",
                "reason": "exa_status_error",
                "exaErrorTag": "CRAWL_NOT_FOUND",
                "exaHttpStatus": 404,
                "requestId": "req-missing",
            })
        );
    }
}

#[tokio::test]
async fn get_reports_empty_text_as_unavailable() {
    let s = setup(true).await;
    s.contents_reply(
        ResponseTemplate::new(200).set_body_json(contents_body("  \n")),
        1,
    )
    .await;
    let out = s
        .call("podcast_transcript_get", json!({ "url": EPISODE }))
        .await
        .unwrap();
    assert_eq!(out["status"], "unavailable");
    assert_eq!(out["reason"], "empty_text");
}

#[tokio::test]
async fn get_marks_text_beyond_the_retrieval_cap() {
    let s = setup(true).await;
    let text = "a".repeat(1_000_001);
    s.contents_reply(
        ResponseTemplate::new(200).set_body_json(contents_body(&text)),
        1,
    )
    .await;
    let out = s
        .call(
            "podcast_transcript_get",
            json!({ "url": EPISODE, "offset": 990_000, "maxChars": 40000 }),
        )
        .await
        .unwrap();
    assert_eq!(out["totalChars"], 1_000_000);
    assert_eq!(out["returnedChars"], 10_000);
    assert_eq!(out["nextOffset"], Value::Null);
    assert_eq!(out["sourceTruncated"], true);
}

#[tokio::test]
async fn get_refuses_urls_outside_the_podcast_index_without_calling_exa() {
    let s = setup(true).await;
    s.contents_reply(ResponseTemplate::new(200), 0).await;
    for url in [
        "https://example.com/library/podcast/abc",
        "https://exa.ai/library/paper/abc",
        "http://exa.ai/library/podcast/abc",
    ] {
        let error = s
            .call("podcast_transcript_get", json!({ "url": url }))
            .await
            .unwrap_err();
        assert!(matches!(error.phase, ToolPhase::Input), "{url}");
    }
}

#[tokio::test]
async fn get_surfaces_exa_http_errors_by_status_and_tag_only() {
    let s = setup(true).await;
    s.contents_reply(
        ResponseTemplate::new(401).set_body_json(json!({
            "requestId": "r",
            "error": "Invalid API key exa-test",
            "tag": "INVALID_API_KEY"
        })),
        1,
    )
    .await;
    let error = s
        .call("podcast_transcript_get", json!({ "url": SHOW }))
        .await
        .unwrap_err();
    assert!(matches!(error.phase, ToolPhase::Execute));
    assert_eq!(
        error.message,
        "Exa request failed: HTTP 401 (INVALID_API_KEY)"
    );
}

#[tokio::test]
async fn get_reports_a_missing_page_and_caps_links() {
    let s = setup(true).await;
    Mock::given(method("POST"))
        .and(path("/contents"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "requestId": "r1", "results": [] })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&s.server)
        .await;
    let out = s
        .call("podcast_transcript_get", json!({ "url": EPISODE }))
        .await
        .unwrap();
    assert_eq!(out["status"], "unavailable");
    assert_eq!(out["reason"], "not_returned");
    assert_eq!(out["exaErrorTag"], Value::Null);

    let links: Vec<String> = (0..60).map(|i| format!("https://l.example/{i}")).collect();
    Mock::given(method("POST"))
        .and(path("/contents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "requestId": "bad id with spaces",
            "results": [{ "url": SHOW, "text": "Show notes", "extras": { "links": links } }],
        })))
        .expect(1)
        .mount(&s.server)
        .await;
    let show = s
        .call("podcast_transcript_get", json!({ "url": SHOW }))
        .await
        .unwrap();
    assert_eq!(show["status"], "available");
    assert_eq!(show["kind"], "show");
    assert_eq!(show["links"].as_array().unwrap().len(), 50);
    assert_eq!(show["linksTruncated"], true);
    assert_eq!(show["requestId"], Value::Null);
    assert_eq!(show["contentSource"], Value::Null);
}

#[tokio::test]
async fn search_rejects_reversed_dates_and_caps_results() {
    let s = setup(true).await;
    let error = s
        .call(
            "podcast_transcript_search",
            json!({ "query": "q", "publishedAfter": "2026-10-02", "publishedBefore": "2026-10-01" }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.phase, ToolPhase::Input));

    let results: Vec<Value> = (0..5)
        .map(|i| json!({ "url": format!("{SHOW}{i}") }))
        .chain([json!({ "title": "no url" })])
        .collect();
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
        .expect(1)
        .mount(&s.server)
        .await;
    let out = s
        .call(
            "podcast_transcript_search",
            json!({ "query": "q", "limit": 2 }),
        )
        .await
        .unwrap();
    assert_eq!(out["results"].as_array().unwrap().len(), 2);
}
