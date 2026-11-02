//! `podcast_recommendation_record`: RSS-verified episode identity, the
//! read-only client mapping, atomic reservations across keys and identities,
//! and receipt-only delivery. Inputs and outputs are validated against the
//! golden schemas by `typed_tool`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{DAY, FakeAccount, FakeAccounts, FakeDirectory, NOW, rec};
use omni_core::clock::TestClock;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_podcasts::account::{
    EpisodeLookup, EpisodeMatchKind, ListenedEpisode, PodcastSubscription, QueuedEpisode,
};
use omni_podcasts::castro::NoAccount;
use omni_podcasts::mcp::{McpState, tools};
use omni_podcasts::persistence::{
    PodcastRecommendationData, PodcastRecommendationStatus, get_all_podcast_recommendations,
    insert_podcast_recommendation,
};
use omni_podcasts::rss::FeedEpisode;
use omni_podcasts::sync::{SyncSummary, run_podcast_recommendation_sync};
use omni_testkit::{TestStore, test_clock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const FEED: &str = "https://feeds.example.com/barpod";
const HOUR: i64 = 60 * 60 * 1000;

fn episode() -> FeedEpisode {
    FeedEpisode {
        guid: "guid-1".into(),
        title: "Episode One".into(),
        published_at: NOW - 2 * DAY,
        duration_minutes: Some(70),
        description: "desc".into(),
        link: Some("https://barpod.example/1".into()),
        enclosure_url: Some("https://cdn.example.com/ep1.mp3?token=abc".into()),
    }
}

struct Fixture {
    store: TestStore,
    clock: Arc<TestClock>,
    account: Option<Arc<FakeAccount>>,
    tools: Vec<McpTool>,
}

impl Fixture {
    async fn new(account: Option<FakeAccount>) -> Self {
        Self::with_feed(account, Ok(vec![episode()])).await
    }

    async fn with_feed(
        account: Option<FakeAccount>,
        feed: Result<Vec<FeedEpisode>, String>,
    ) -> Self {
        let clock = test_clock(NOW);
        let store = TestStore::new(clock.clone()).await;
        let account = account.map(Arc::new);
        let state = McpState {
            store: store.store.clone(),
            clock: clock.clone(),
            accounts: match &account {
                Some(account) => Arc::new(FakeAccounts(account.clone())),
                None => Arc::new(NoAccount),
            },
            directory: Arc::new(FakeDirectory::with_feeds(Vec::new(), vec![feed])),
            public_url: "https://recs.example/".into(),
        };
        Self {
            store,
            clock,
            account,
            tools: tools(state).unwrap(),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.meta.name == "podcast_recommendation_record")
            .unwrap();
        let cx = ToolContext {
            call_id: "1".into(),
            cancel: CancellationToken::new(),
        };
        match tool.handler.call(input, cx).await? {
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            } => Ok(Value::Object(map)),
        }
    }

    async fn rows(&self) -> Vec<PodcastRecommendationData> {
        get_all_podcast_recommendations(&self.store.store)
            .await
            .unwrap()
    }
}

fn verified_account() -> FakeAccount {
    FakeAccount {
        episode_lookup: Some(Ok(EpisodeLookup::Found {
            client_episode_id: "castro-ep-9".into(),
            matched_by: EpisodeMatchKind::Guid,
        })),
        ..FakeAccount::default()
    }
}

fn prepare(key: &str, dry_run: Option<bool>) -> Value {
    let mut input = json!({
        "action": "prepare",
        "idempotencyKey": key,
        "feedUrl": FEED,
        "episodeGuid": "guid-1",
        "showTitle": "Barpod",
        "whyForUser": "A juicy internet-drama deep dive.",
    });
    if let Some(dry_run) = dry_run {
        input["dryRun"] = json!(dry_run);
    }
    input
}

fn codes(out: &Value) -> Vec<String> {
    out["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_default_prepare_verifies_the_feed_and_client_mapping_without_writing() {
    let f = Fixture::new(Some(verified_account())).await;
    let out = f.call(prepare("k1", None)).await.unwrap();
    assert_eq!(out["dryRun"], json!(true));
    assert_eq!(out["result"], json!("eligible"));
    let identity = &out["identity"];
    assert_eq!(
        identity["episodeId"],
        json!("feed:feeds.example.com/barpod#guid-1")
    );
    assert_eq!(
        identity["episodeTitle"],
        json!("Episode One"),
        "title comes from the feed"
    );
    assert_eq!(
        identity["mediaUrl"],
        json!("https://cdn.example.com/ep1.mp3?token=abc")
    );
    assert_eq!(
        out["client"],
        json!({"status": "verified", "clientEpisodeId": "castro-ep-9", "matchedBy": "guid", "reason": null})
    );
    assert_eq!(out["enqueue"]["episodeGuid"], json!("guid-1"));
    assert_eq!(out["enqueue"]["feedUrl"], json!(FEED));
    assert!(f.rows().await.is_empty());
    let account = f.account.as_ref().unwrap();
    assert_eq!(account.count("enqueue_episode"), 0);
    assert_eq!(account.count("subscribe_to_show"), 0);
}

#[tokio::test]
async fn a_title_only_client_match_is_never_offered_for_queueing() {
    let account = FakeAccount {
        episode_lookup: Some(Ok(EpisodeLookup::Found {
            client_episode_id: "castro-ep-9".into(),
            matched_by: EpisodeMatchKind::Title,
        })),
        ..FakeAccount::default()
    };
    let f = Fixture::new(Some(account)).await;
    let out = f.call(prepare("k1", None)).await.unwrap();
    assert_eq!(out["result"], json!("eligible"));
    assert_eq!(out["client"]["status"], json!("unverified"));
    assert_eq!(out["client"]["clientEpisodeId"], Value::Null);
    assert_eq!(out["enqueue"], Value::Null);

    let unsupported = Fixture::new(Some(FakeAccount::default())).await;
    let out = unsupported.call(prepare("k1", None)).await.unwrap();
    assert_eq!(out["client"]["status"], json!("unsupported"));
    assert_eq!(out["enqueue"], Value::Null);
}

#[tokio::test]
async fn an_episode_must_match_the_feed_exactly() {
    let f = Fixture::new(Some(verified_account())).await;
    let mut wrong_guid = prepare("k1", Some(false));
    wrong_guid["episodeGuid"] = json!("guid-unknown");
    let out = f.call(wrong_guid).await.unwrap();
    assert_eq!(out["result"], json!("ineligible"));
    assert_eq!(codes(&out), vec!["episode_not_in_feed"]);
    assert_eq!(out["identity"], Value::Null);

    let mut wrong_enclosure = prepare("k1", Some(false));
    wrong_enclosure["enclosureUrl"] = json!("https://cdn.example.com/other.mp3");
    let out = f.call(wrong_enclosure).await.unwrap();
    assert_eq!(codes(&out), vec!["enclosure_mismatch"]);

    // Query strings and protocol do not change the enclosure's identity.
    let mut same_enclosure = prepare("k1", None);
    same_enclosure["enclosureUrl"] = json!("http://cdn.example.com/ep1.mp3");
    assert_eq!(
        f.call(same_enclosure).await.unwrap()["result"],
        json!("eligible")
    );
    assert!(f.rows().await.is_empty());
}

#[tokio::test]
async fn subscribed_listened_and_queued_episodes_are_ineligible() {
    let cases = [
        (
            FakeAccount {
                subscriptions: Ok(vec![PodcastSubscription {
                    title: "Barpod".into(),
                    feed_url: Some("http://FEEDS.example.com/barpod/".into()),
                    itunes_id: None,
                }]),
                ..verified_account()
            },
            "already_subscribed",
        ),
        (
            FakeAccount {
                history: Ok(vec![ListenedEpisode {
                    show_title: "Barpod".into(),
                    episode_title: "Episode One".into(),
                    episode_guid: Some("guid-1".into()),
                    listened_at: NOW - DAY,
                    ..ListenedEpisode::default()
                }]),
                ..verified_account()
            },
            "already_listened",
        ),
        (
            FakeAccount {
                queue: Ok(vec![QueuedEpisode {
                    show_title: "Barpod".into(),
                    episode_title: "Episode One".into(),
                    episode_guid: Some("guid-1".into()),
                    ..QueuedEpisode::default()
                }]),
                ..verified_account()
            },
            "in_queue",
        ),
    ];
    for (account, code) in cases {
        let f = Fixture::new(Some(account)).await;
        let out = f.call(prepare("k1", Some(false))).await.unwrap();
        assert_eq!(codes(&out), vec![code]);
        assert!(f.rows().await.is_empty());
    }
}

#[tokio::test]
async fn unreadable_account_or_feed_state_blocks_reservation() {
    let f = Fixture::new(None).await;
    let out = f.call(prepare("k1", Some(false))).await.unwrap();
    assert_eq!(out["result"], json!("unavailable"));
    assert_eq!(out["unavailable"][0]["source"], json!("podcast_account"));

    let account = FakeAccount {
        history: Err(omni_podcasts::account::Unavailable::new("Castro 503")),
        ..verified_account()
    };
    let f = Fixture::new(Some(account)).await;
    let out = f.call(prepare("k1", Some(false))).await.unwrap();
    assert_eq!(
        out["unavailable"],
        json!([{"source": "listen_history", "reason": "Castro 503"}])
    );

    let f = Fixture::with_feed(Some(verified_account()), Err("HTTP 500".into())).await;
    let out = f.call(prepare("k1", Some(false))).await.unwrap();
    assert_eq!(
        out["unavailable"],
        json!([{"source": "rss", "reason": "HTTP 500"}])
    );
    assert!(f.rows().await.is_empty());
}

#[tokio::test]
async fn a_legacy_recommendation_of_the_episode_blocks_it_under_any_show_identity() {
    let f = Fixture::new(Some(verified_account())).await;
    // The legacy pipeline keyed the show by its iTunes id; the caller here
    // names only the feed.
    insert_podcast_recommendation(
        &f.store.store,
        PodcastRecommendationData {
            recommendation_id: "legacy".into(),
            episode_id: "itunes:77#guid-1".into(),
            show_id: "itunes:77".into(),
            feed_url: FEED.into(),
            episode_guid: "guid-1".into(),
            recommended_at: NOW - 40 * DAY,
            ..rec()
        },
    )
    .await
    .unwrap();
    let out = f.call(prepare("k1", Some(false))).await.unwrap();
    assert_eq!(codes(&out), vec!["already_recommended"]);

    // A different episode of a show recommended last week cools down.
    let g = Fixture::new(Some(verified_account())).await;
    insert_podcast_recommendation(
        &g.store.store,
        PodcastRecommendationData {
            recommendation_id: "recent".into(),
            episode_id: "itunes:77#guid-0".into(),
            show_id: "itunes:77".into(),
            feed_url: FEED.into(),
            episode_guid: "guid-0".into(),
            recommended_at: NOW - 7 * DAY,
            ..rec()
        },
    )
    .await
    .unwrap();
    let out = g.call(prepare("k1", Some(false))).await.unwrap();
    assert_eq!(codes(&out), vec!["show_cooldown"]);
}

#[tokio::test]
async fn one_reservation_per_episode_across_keys_and_concurrent_callers() {
    let f = Fixture::new(Some(verified_account())).await;
    let outs =
        futures::future::join_all((0..6).map(|i| f.call(prepare(&format!("k{i}"), Some(false)))))
            .await;
    let reserved: Vec<&Value> = outs
        .iter()
        .map(|o| o.as_ref().unwrap())
        .filter(|o| o["result"] == json!("reserved"))
        .collect();
    assert_eq!(reserved.len(), 1);
    let rec = &reserved[0]["recommendation"];
    assert_eq!(rec["status"], json!("pending"));
    assert_eq!(rec["origin"], json!("assistant_chat"));
    assert_eq!(rec["discoveredVia"], Value::Null);
    assert_eq!(rec["confidence"], Value::Null);
    let id = rec["recommendationId"].as_str().unwrap();
    assert_eq!(
        reserved[0]["feedbackUrl"],
        json!(format!("https://recs.example/feedback/podcasts/{id}"))
    );
    let key = (0..6)
        .find(|i| outs[*i].as_ref().unwrap()["result"] == json!("reserved"))
        .unwrap();
    let replay = f
        .call(prepare(&format!("k{key}"), Some(false)))
        .await
        .unwrap();
    assert_eq!(replay["result"], json!("replayed"));
    assert_eq!(replay["recommendation"]["recommendationId"], json!(id));
    assert_eq!(f.rows().await.len(), 1);
    assert_eq!(f.account.as_ref().unwrap().count("enqueue_episode"), 0);
}

#[tokio::test]
async fn receipts_finish_unknown_sends_wait_and_withdrawals_keep_the_retry_exclusion() {
    let f = Fixture::new(Some(verified_account())).await;
    let id =
        f.call(prepare("k1", Some(false))).await.unwrap()["recommendation"]["recommendationId"]
            .as_str()
            .unwrap()
            .to_owned();
    let unknown = f
        .call(json!({
            "action": "finish", "dryRun": false, "recommendationId": id, "expectedRevision": 1,
            "delivery": {"state": "unknown", "reason": "send timed out"},
        }))
        .await
        .unwrap();
    assert_eq!(unknown["recommendation"]["status"], json!("pending"));
    assert_eq!(unknown["recommendation"]["notifiedAt"], Value::Null);
    for (dry_run, revision) in [(true, 2), (false, 2), (false, 1)] {
        let refused = f
            .call(json!({
                "action": "withdraw", "dryRun": dry_run, "recommendationId": id,
                "expectedRevision": revision, "reason": "never sent", "notPresented": true,
            }))
            .await
            .unwrap_err();
        assert_eq!(refused.phase, ToolPhase::Execute);
        assert!(refused.message.contains("unknown send cannot be withdrawn"));
    }
    let row = f.rows().await.into_iter().next().unwrap();
    assert_eq!(row.status, PodcastRecommendationStatus::Pending);
    assert_eq!(row.resolved_at, None);
    assert_eq!(row.chat_record.as_ref().unwrap().revision, 2);
    let blocked = f.call(prepare("k-other", Some(false))).await.unwrap();
    assert_eq!(
        codes(&blocked),
        vec!["pending_recommendation", "show_cooldown"]
    );
    let sent = json!({
        "action": "finish", "dryRun": false, "recommendationId": id, "expectedRevision": 2,
        "delivery": {"state": "sent", "channel": "claude-chat", "messageRef": "m1",
                     "sentAt": NOW + 1000, "provenance": "chat send receipt"},
    });
    f.clock.set(NOW + 2000);
    let done = f.call(sent.clone()).await.unwrap();
    assert_eq!(done["recommendation"]["status"], json!("notified"));
    assert_eq!(done["recommendation"]["notifiedAt"], json!(NOW + 1000));
    assert_eq!(f.call(sent).await.unwrap()["result"], json!("unchanged"));

    let g = Fixture::new(Some(verified_account())).await;
    let id =
        g.call(prepare("k1", Some(false))).await.unwrap()["recommendation"]["recommendationId"]
            .as_str()
            .unwrap()
            .to_owned();
    let withdraw = json!({
        "action": "withdraw", "dryRun": false, "recommendationId": id, "expectedRevision": 1,
        "reason": "never sent", "notPresented": true,
    });
    let out = g.call(withdraw).await.unwrap();
    assert_eq!(out["recommendation"]["status"], json!("failed"));
    let blocked = g.call(prepare("k2", Some(false))).await.unwrap();
    assert_eq!(codes(&blocked), vec!["recently_failed"]);
    g.clock.set(NOW + DAY + HOUR);
    let again = g.call(prepare("k2", Some(false))).await.unwrap();
    assert_eq!(again["result"], json!("reserved"));

    let bad = g
        .call(json!({
            "action": "finish", "dryRun": false, "recommendationId": "missing",
            "expectedRevision": 1, "delivery": {"state": "unknown", "reason": "x"},
        }))
        .await
        .unwrap_err();
    assert_eq!(bad.message, "Podcast recommendation not found");
    assert_eq!(bad.phase, ToolPhase::Execute);
}

#[tokio::test]
async fn the_extracted_sync_labels_only_notified_rows_without_models_or_writes_elsewhere() {
    let account = FakeAccount {
        history: Ok(vec![
            ListenedEpisode {
                show_title: "The Gray Area".into(),
                episode_title: "What is consciousness?".into(),
                episode_guid: Some("guid-1".into()),
                listened_at: NOW - DAY,
                completion: Some(0.95),
                ..ListenedEpisode::default()
            },
            ListenedEpisode {
                show_title: "Pending Show".into(),
                episode_title: "Pending".into(),
                episode_guid: Some("guid-pending".into()),
                listened_at: NOW - DAY,
                completion: Some(0.95),
                ..ListenedEpisode::default()
            },
        ]),
        ..FakeAccount::default()
    };
    let f = Fixture::new(Some(account)).await;
    insert_podcast_recommendation(&f.store.store, rec())
        .await
        .unwrap();
    insert_podcast_recommendation(
        &f.store.store,
        PodcastRecommendationData {
            recommendation_id: "pending".into(),
            episode_id: "itunes:2#guid-pending".into(),
            show_id: "itunes:2".into(),
            episode_guid: "guid-pending".into(),
            status: PodcastRecommendationStatus::Pending,
            recommended_at: NOW - 3 * DAY,
            ..rec()
        },
    )
    .await
    .unwrap();
    let accounts = FakeAccounts(f.account.clone().unwrap());
    let summary = run_podcast_recommendation_sync(&f.store.store, &accounts, NOW)
        .await
        .unwrap();
    let SyncSummary::Synced {
        outcomes,
        evidence_inserted,
    } = summary
    else {
        panic!("{summary:?}");
    };
    assert_eq!(outcomes, 1);
    assert!(evidence_inserted >= 3);
    let rows = f.rows().await;
    let by_id = |id: &str| rows.iter().find(|r| r.recommendation_id == id).unwrap();
    assert_eq!(by_id("r1").status, PodcastRecommendationStatus::Listened);
    assert_eq!(
        by_id("pending").status,
        PodcastRecommendationStatus::Pending
    );
    let account = f.account.as_ref().unwrap();
    assert_eq!(account.count("enqueue_episode"), 0);
    assert_eq!(account.count("clear_inbox_episode"), 0);
}
