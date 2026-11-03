//! The podcast MCP tools: golden metadata,
//! input defaults/trimming, typed failures and golden output schemas.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{FakeAccount, FakeAccounts, rec};
use omni_core::clock::SharedClock;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_podcasts::account::{ListenedEpisode, PodcastSubscription, Unavailable};
use omni_podcasts::castro::NoAccount;
use omni_podcasts::mcp::{McpState, tools};
use omni_podcasts::persistence::{
    PodcastFeedback, PodcastRecommendationData, insert_podcast_recommendation,
};
use omni_podcasts::reflection::derive_listen_evidence;
use omni_podcasts::reflection::store::insert_podcast_taste_evidence;
use omni_testkit::{TestStore, test_clock};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

const NOW: i64 = 1_784_160_000_000;

async fn state(account: Option<Arc<FakeAccount>>) -> (TestStore, McpState) {
    let clock: SharedClock = test_clock(NOW);
    let store = TestStore::new(clock.clone()).await;
    let state = McpState {
        store: store.store.clone(),
        clock,
        accounts: match account {
            Some(account) => Arc::new(FakeAccounts(account)),
            None => Arc::new(NoAccount),
        },
        directory: Arc::new(omni_podcasts::sources::OfflineDirectory),
        public_url: "https://recs.example".to_owned(),
        exa: None,
    };
    (store, state)
}

fn tool(tools: &[McpTool], name: &str) -> McpTool {
    tools.iter().find(|t| t.meta.name == name).unwrap().clone()
}

async fn call(tool: &McpTool, input: Value) -> Result<Map<String, Value>, ToolError> {
    let cx = ToolContext {
        call_id: "test".into(),
        cancel: CancellationToken::new(),
    };
    match tool.handler.call(input, cx).await? {
        ToolOutput::Structured(map) => Ok(map),
        ToolOutput::Custom { structured, .. } => Ok(structured),
    }
}

#[tokio::test]
async fn registers_the_ten_tools_in_serving_order() {
    let (_store, state) = state(None).await;
    let names: Vec<String> = tools(state)
        .unwrap()
        .iter()
        .map(|t| t.meta.name.clone())
        .collect();
    assert_eq!(
        names,
        vec![
            "podcast_account_list",
            "podcast_account_search",
            "podcast_account_update",
            "podcast_recommendations_list",
            "podcast_recommendation_get",
            "podcast_recommendation_feedback",
            "podcast_recommendation_record",
            "podcast_taste_read",
            "podcast_transcript_search",
            "podcast_transcript_get",
        ]
    );
}

#[tokio::test]
async fn account_tools_fail_without_a_configured_account() {
    let (_store, state) = state(None).await;
    let tools = tools(state).unwrap();
    let error = call(
        &tool(&tools, "podcast_account_list"),
        json!({ "resource": "queue" }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Execute);
    assert_eq!(error.message, "Podcast account is not configured");
}

#[tokio::test]
async fn lists_filters_and_paginates_account_resources() {
    let account = Arc::new(FakeAccount {
        subscriptions: Ok(vec![
            PodcastSubscription {
                title: "Hard Fork".into(),
                feed_url: Some("https://feeds/hf".into()),
                itunes_id: Some(1),
            },
            PodcastSubscription {
                title: "The Daily".into(),
                ..PodcastSubscription::default()
            },
        ]),
        history: Ok(vec![ListenedEpisode {
            show_title: "Hard Fork".into(),
            episode_title: "AI".into(),
            episode_guid: Some("g".into()),
            media_url: Some("https://cdn/hidden.mp3".into()),
            listened_at: NOW - 1000,
            completion: Some(0.5),
            starred: Some(false),
            ..ListenedEpisode::default()
        }]),
        queue: Err(Unavailable::new("Castro timed out")),
        ..FakeAccount::default()
    });
    let (_store, state) = state(Some(account.clone())).await;
    let tools = tools(state).unwrap();
    let list = tool(&tools, "podcast_account_list");
    let page = call(
        &list,
        json!({ "resource": "subscriptions", "query": "  hard ", "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(
        Value::Object(page),
        json!({
            "account": "Castro",
            "resource": "subscriptions",
            "items": [{ "title": "Hard Fork", "feedUrl": "https://feeds/hf", "itunesId": 1 }],
            "nextCursor": null,
            "total": 1
        })
    );
    let history = call(
        &list,
        json!({ "resource": "listen_history", "sinceDays": 2 }),
    )
    .await
    .unwrap();
    assert_eq!(history["items"][0].get("mediaUrl"), None);
    // The test clock follows real time, so the cutoff is a few ms after NOW - 2 days.
    let since: i64 = account
        .calls()
        .iter()
        .find_map(|c| {
            c.strip_prefix("fetch_listen_history:Some(")?
                .strip_suffix(')')?
                .parse()
                .ok()
        })
        .unwrap();
    assert!((NOW - 2 * 86_400_000..NOW - 2 * 86_400_000 + 60_000).contains(&since));
    let error = call(&list, json!({ "resource": "queue" }))
        .await
        .unwrap_err();
    assert_eq!(error.message, "Castro timed out");
}

#[tokio::test]
async fn updates_the_account_with_defaults_and_trimmed_titles() {
    let account = Arc::new(FakeAccount::default());
    let (_store, state) = state(Some(account.clone())).await;
    let tools = tools(state).unwrap();
    let update = tool(&tools, "podcast_account_update");
    let out = call(
        &update,
        json!({
            "action": "enqueue",
            "feedUrl": "https://feeds.example.com/x",
            "episodeGuid": "g",
            "showTitle": "  Show ",
            "episodeTitle": " Ep "
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        Value::Object(out),
        json!({ "account": "Castro", "action": "enqueue", "result": "added" })
    );
    let request = account.enqueued.lock().unwrap()[0].clone();
    assert_eq!(request.show_title, "Show");
    assert_eq!(request.episode_title, "Ep");
    assert_eq!(
        request.position,
        Some(omni_podcasts::account::PodcastQueuePosition::Next)
    );
    let error = call(&update, json!({ "action": "explode" }))
        .await
        .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Input);
    let cleared = call(
        &update,
        json!({ "action": "clear_inbox", "clientEpisodeId": "c1" }),
    )
    .await
    .unwrap();
    assert_eq!(cleared["result"], "removed");
}

#[tokio::test]
async fn reads_lists_and_rates_recommendations() {
    let (store, state) = state(None).await;
    insert_podcast_recommendation(&store.store, rec())
        .await
        .unwrap();
    insert_podcast_recommendation(
        &store.store,
        PodcastRecommendationData {
            recommendation_id: "r2".into(),
            feedback: Some(PodcastFeedback::GoodPick),
            confidence: Some(0.5),
            ..rec()
        },
    )
    .await
    .unwrap();
    let tools = tools(state).unwrap();
    let list = call(
        &tool(&tools, "podcast_recommendations_list"),
        json!({ "feedback": "none" }),
    )
    .await
    .unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["recommendationId"], "r1");
    assert_eq!(list["items"][0]["resolvedAt"], Value::Null);

    let missing = call(
        &tool(&tools, "podcast_recommendation_get"),
        json!({ "recommendationId": "nope" }),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.message, "Podcast recommendation not found");

    let feedback = tool(&tools, "podcast_recommendation_feedback");
    let required = call(&feedback, json!({ "recommendationId": "r1" }))
        .await
        .unwrap_err();
    assert_eq!(required.message, "feedback or note is required");
    let rated = call(
        &feedback,
        json!({ "recommendationId": "r1", "note": "  fine " }),
    )
    .await
    .unwrap();
    assert_eq!(rated["recommendation"]["feedbackNote"], "fine");
    let at = rated["recommendation"]["feedbackAt"].as_i64().unwrap();
    assert!((NOW..NOW + 60_000).contains(&at));
}

#[tokio::test]
async fn reads_the_profile_and_paginated_evidence() {
    let (store, state) = state(None).await;
    let evidence = derive_listen_evidence(&[ListenedEpisode {
        show_title: "Hard Fork".into(),
        episode_title: "AI".into(),
        listened_at: NOW,
        completion: Some(1.0),
        ..ListenedEpisode::default()
    }]);
    insert_podcast_taste_evidence(&store.store, evidence)
        .await
        .unwrap();
    let tools = tools(state).unwrap();
    let taste = tool(&tools, "podcast_taste_read");
    assert_eq!(
        Value::Object(
            call(&taste, json!({ "resource": "profile" }))
                .await
                .unwrap()
        ),
        json!({ "resource": "profile", "profile": null })
    );
    let page = call(&taste, json!({ "resource": "evidence", "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["completion"], json!(1));
    assert_eq!(page["items"][0]["recommendationId"], Value::Null);
}
