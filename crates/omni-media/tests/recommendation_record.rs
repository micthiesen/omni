//! `media_recommendation_record`: exact-identity previews, atomic
//! reservations, receipt-only delivery and explicit withdrawal, plus the
//! guarantees around them: legacy reconciliation never touches assistant
//! rows, pending rows never gain playback timestamps, and the extracted sync
//! uses no model, notifier or acquisition. Inputs and outputs are validated
//! against the golden schemas by `typed_tool`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use omni_api::media::RecommendationStatus;
use omni_core::clock::TestClock;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_media::mcp::media_tools;
use omni_media::persistence::{
    IdentityAliasData, NotificationState, RecommendationData, get_all_recommendations,
    select_on_deck,
};
use omni_media::pipeline::{PipelineOptions, run_recommendation_pipeline};
use omni_media::sync::{MediaRecommendationSyncTask, SyncSummary, run_recommendation_sync};
use omni_media::types::{FetchResult, InProgressItem, MediaType, WatchedItem};
use omni_runtime::chat_record::{ChatDelivery, RecommendationOrigin};
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::{TEST_EPOCH_MS, test_clock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const HOUR: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * HOUR;

struct Fixture {
    h: common::Harness,
    clock: Arc<TestClock>,
    tools: Vec<McpTool>,
}

impl Fixture {
    async fn new() -> Self {
        let mut h = common::Harness::new().await;
        let clock = test_clock(TEST_EPOCH_MS);
        h.services.clock = clock.clone();
        for (id, title, media_type) in [
            (10, "Exact Movie", MediaType::Movie),
            (11, "Second Movie", MediaType::Movie),
            (20, "Exact Series", MediaType::Tv),
            (1, "Watched", MediaType::Movie),
            (3, "Tracked", MediaType::Movie),
            (4, "Started", MediaType::Movie),
            (5, "Twin", MediaType::Movie),
        ] {
            let mut t = common::tmdb_title(id, media_type);
            t.title = title.to_owned();
            t.year = Some(2020);
            h.catalog.titles.lock().unwrap().insert((media_type, id), t);
        }
        *h.library.history.lock().unwrap() = FetchResult::Ok(vec![
            WatchedItem {
                item: common::media("plex://watched", "Watched", MediaType::Movie, Some(1)),
                viewed_at: TEST_EPOCH_MS - DAY,
                view_count: 1,
                completion: Some(1.0),
            },
            // No external id and no cached alias: identity is unknown.
            WatchedItem {
                item: common::media("plex://twin", "Twin", MediaType::Movie, None),
                viewed_at: TEST_EPOCH_MS - DAY,
                view_count: 1,
                completion: Some(1.0),
            },
        ]);
        *h.library.in_progress.lock().unwrap() = FetchResult::Ok(vec![InProgressItem {
            item: common::media("plex://started", "Started", MediaType::Movie, Some(4)),
            progress: 0.3,
            last_viewed_at: TEST_EPOCH_MS - HOUR,
        }]);
        *h.watchlist.items.lock().unwrap() = FetchResult::Ok(vec![common::media(
            "radarr:3",
            "Tracked",
            MediaType::Movie,
            Some(3),
        )]);
        let tools = media_tools(Arc::new(h.services.clone())).expect("tools");
        Self { h, clock, tools }
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.meta.name == "media_recommendation_record")
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

    async fn get(&self, id: &str) -> Value {
        let tool = self
            .tools
            .iter()
            .find(|t| t.meta.name == "media_recommendation_get")
            .expect("tool");
        let cx = ToolContext {
            call_id: "1".to_owned(),
            cancel: CancellationToken::new(),
        };
        match tool
            .handler
            .call(json!({"recommendationId": id}), cx)
            .await
            .expect("get")
        {
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            } => Value::Object(map),
        }
    }

    async fn rows(&self) -> Vec<RecommendationData> {
        get_all_recommendations(&self.h.services.store)
            .await
            .expect("rows")
    }

    async fn reserve(&self, key: &str, tmdb_id: i64) -> Value {
        self.call(prepare(key, tmdb_id, false))
            .await
            .expect("prepare")
    }
}

fn prepare(key: &str, tmdb_id: i64, dry_run: bool) -> Value {
    json!({
        "action": "prepare",
        "dryRun": dry_run,
        "idempotencyKey": key,
        "mediaType": "movie",
        "tmdbId": tmdb_id,
        "whyForUser": "A tense heist you have not seen.",
        "caveats": ["Subtitled"],
    })
}

fn sent(id: &str, revision: i64, message_ref: &str, sent_at: i64) -> Value {
    json!({
        "action": "finish",
        "dryRun": false,
        "recommendationId": id,
        "expectedRevision": revision,
        "delivery": {
            "state": "sent",
            "channel": "claude-chat",
            "messageRef": message_ref,
            "sentAt": sent_at,
            "provenance": "chat send tool receipt",
        },
    })
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
async fn a_default_prepare_is_a_dry_run_that_writes_nothing() {
    let f = Fixture::new().await;
    let out = f
        .call(json!({
            "action": "prepare",
            "idempotencyKey": "k1",
            "mediaType": "movie",
            "tmdbId": 10,
            "whyForUser": "Why",
        }))
        .await
        .expect("preview");
    assert_eq!(out["dryRun"], json!(true));
    assert_eq!(out["result"], json!("eligible"));
    assert_eq!(out["identity"]["canonicalId"], json!("tmdb:movie:10"));
    assert_eq!(out["identity"]["title"], json!("Exact Movie"));
    assert_eq!(out["recommendation"], Value::Null);
    assert!(f.rows().await.is_empty());
    // Even identity resolution caches are left alone.
    let aliases =
        f.h.services
            .store
            .read(|docs| docs.count::<IdentityAliasData>())
            .await
            .unwrap();
    assert_eq!(aliases, 0);
    assert!(f.h.watchlist.adds().is_empty());
    assert!(f.h.notifier.pushes().is_empty());
    assert!(f.h.app.ai.requests().is_empty());
}

#[tokio::test]
async fn a_reservation_is_private_pending_and_labeled_as_assistant_chat() {
    let f = Fixture::new().await;
    let out = f.reserve("k1", 10).await;
    assert_eq!(out["result"], json!("reserved"));
    let rec = &out["recommendation"];
    assert_eq!(rec["status"], json!("pending"));
    assert_eq!(rec["origin"], json!("assistant_chat"));
    assert_eq!(rec["confidence"], Value::Null);
    assert_eq!(rec["chat"]["delivery"], json!("reserved"));
    assert_eq!(rec["chat"]["revision"], json!(1));
    assert_eq!(rec["notifiedAt"], Value::Null);
    let id = rec["recommendationId"].as_str().unwrap();
    assert_eq!(
        out["feedbackUrl"].as_str().unwrap(),
        format!(
            "{}/feedback/recommendations/{id}",
            f.h.services.config.recs_public_url.trim_end_matches('/')
        )
    );
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, Some(RecommendationOrigin::AssistantChat));
    assert_eq!(
        rows[0].source, None,
        "never labeled as a trending candidate"
    );
    assert_eq!(
        rows[0].caveats.as_deref(),
        Some(&["Subtitled".to_owned()][..])
    );
    assert!(f.h.watchlist.adds().is_empty());
    assert!(f.h.notifier.pushes().is_empty());
    assert!(f.h.app.pushes.all().is_empty());
}

#[tokio::test]
async fn the_same_key_replays_and_a_different_key_cannot_reserve_the_same_title() {
    let f = Fixture::new().await;
    let first = f.reserve("k1", 10).await;
    let id = first["recommendation"]["recommendationId"].clone();
    let replay = f.reserve("k1", 10).await;
    assert_eq!(replay["result"], json!("replayed"));
    assert_eq!(replay["recommendation"]["recommendationId"], id);
    let other = f.reserve("k2", 10).await;
    assert_eq!(other["result"], json!("ineligible"));
    assert_eq!(codes(&other), vec!["pending_recommendation"]);
    let misuse = f
        .call(prepare("k1", 11, false))
        .await
        .expect_err("conflict");
    assert!(
        misuse
            .message
            .contains("idempotencyKey already reserved tmdb:movie:10")
    );
    assert_eq!(f.rows().await.len(), 1);
}

#[tokio::test]
async fn concurrent_callers_with_different_keys_reserve_one_title_once() {
    let f = Fixture::new().await;
    let outs =
        futures::future::join_all((0..8).map(|i| f.call(prepare(&format!("key-{i}"), 10, false))))
            .await;
    let results: Vec<String> = outs
        .iter()
        .map(|o| o.as_ref().unwrap()["result"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(results.iter().filter(|r| *r == "reserved").count(), 1);
    assert_eq!(results.iter().filter(|r| *r == "ineligible").count(), 7);
    assert_eq!(f.rows().await.len(), 1);
}

#[tokio::test]
async fn seen_tracked_in_progress_and_unverifiable_titles_are_ineligible() {
    let f = Fixture::new().await;
    for (tmdb_id, code) in [
        (1, "already_watched"),
        (3, "on_watchlist"),
        (4, "in_progress"),
        (5, "possible_title_match"),
    ] {
        let out = f.call(prepare("k", tmdb_id, false)).await.expect("prepare");
        assert_eq!(out["result"], json!("ineligible"), "{tmdb_id}");
        assert_eq!(codes(&out), vec![code], "{tmdb_id}");
    }
    assert!(f.rows().await.is_empty());
}

#[tokio::test]
async fn an_unavailable_provider_blocks_reservation() {
    let f = Fixture::new().await;
    *f.h.watchlist.items.lock().unwrap() = FetchResult::unavailable("Radarr offline");
    let out = f.reserve("k1", 10).await;
    assert_eq!(out["result"], json!("unavailable"));
    assert_eq!(
        out["unavailable"],
        json!([{"source": "watchlist", "reason": "Radarr offline"}])
    );
    let unknown_title = f.reserve("k2", 999).await;
    assert_eq!(unknown_title["result"], json!("unavailable"));
    assert_eq!(unknown_title["identity"], Value::Null);
    assert!(f.rows().await.is_empty());
}

#[tokio::test]
async fn only_a_send_receipt_marks_a_recommendation_notified_and_retries_are_safe() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let sent_at = TEST_EPOCH_MS + 1000;
    f.clock.set(TEST_EPOCH_MS + 2000);

    let preview = f
        .call(json!({
            "action": "finish",
            "recommendationId": id,
            "expectedRevision": 1,
            "delivery": {"state": "sent", "channel": "claude-chat", "messageRef": "m1",
                         "sentAt": sent_at, "provenance": "receipt"},
        }))
        .await
        .expect("dry run");
    assert_eq!(preview["result"], json!("would_change"));
    assert_eq!(f.rows().await[0].status, RecommendationStatus::Pending);

    let done = f.call(sent(&id, 1, "m1", sent_at)).await.expect("finish");
    assert_eq!(done["result"], json!("changed"));
    let rec = &done["recommendation"];
    assert_eq!(rec["status"], json!("notified"));
    assert_eq!(rec["notifiedAt"], json!(sent_at));
    assert_eq!(rec["chat"]["delivery"], json!("sent"));
    assert_eq!(rec["chat"]["messageRef"], json!("m1"));
    assert_eq!(rec["chat"]["revision"], json!(2));
    let row = &f.rows().await[0];
    assert_eq!(row.notification_state, Some(NotificationState::Sent));

    // The caller lost the response and retries with the old revision.
    let retry = f.call(sent(&id, 1, "m1", sent_at)).await.expect("retry");
    assert_eq!(retry["result"], json!("unchanged"));
    let different = f
        .call(sent(&id, 2, "m2", sent_at))
        .await
        .expect_err("conflict");
    assert!(different.message.contains("different send receipt"));
    assert!(f.h.notifier.pushes().is_empty());
}

#[tokio::test]
async fn stale_revisions_and_implausible_receipts_are_rejected() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let stale = f
        .call(sent(&id, 5, "m1", TEST_EPOCH_MS))
        .await
        .expect_err("stale");
    assert_eq!(
        stale.message,
        "Stale revision: expected 5, current revision is 1"
    );
    let early = f
        .call(sent(&id, 1, "m1", TEST_EPOCH_MS - DAY))
        .await
        .expect_err("early");
    assert_eq!(early.phase, ToolPhase::Input);
    let legacy = RecommendationData {
        recommendation_id: "legacy".to_owned(),
        canonical_id: "tmdb:movie:11".to_owned(),
        tmdb_id: 11,
        media_type: MediaType::Movie,
        title: "Second Movie".to_owned(),
        status: RecommendationStatus::Pending,
        run_date: "2026-01-01".to_owned(),
        recommended_at: TEST_EPOCH_MS,
        ..RecommendationData::default()
    };
    f.h.services
        .store
        .write(move |tx| tx.upsert(&legacy, UpsertOpts::default()))
        .await
        .unwrap();
    let not_chat = f
        .call(sent("legacy", 1, "m1", TEST_EPOCH_MS))
        .await
        .expect_err("legacy");
    assert!(
        not_chat
            .message
            .contains("was not recorded through the chat record tool")
    );
}

#[tokio::test]
async fn an_unknown_send_stays_pending_and_legacy_reconciliation_never_touches_it() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 3).await;
    // Title 3 is tracked by Radarr, so it is ineligible; reserve 10 instead.
    assert_eq!(id["result"], json!("ineligible"));
    let id = f.reserve("k2", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let unknown = f
        .call(json!({
            "action": "finish",
            "dryRun": false,
            "recommendationId": id,
            "expectedRevision": 1,
            "delivery": {"state": "unknown", "reason": "chat send timed out"},
        }))
        .await
        .expect("unknown");
    assert_eq!(unknown["recommendation"]["status"], json!("pending"));
    assert_eq!(
        unknown["recommendation"]["chat"]["delivery"],
        json!("unknown")
    );

    // A legacy pending row the reconciler must still repair, next to ours.
    let legacy = RecommendationData {
        recommendation_id: "legacy".to_owned(),
        canonical_id: "tmdb:movie:3".to_owned(),
        tmdb_id: 3,
        media_type: MediaType::Movie,
        title: "Tracked".to_owned(),
        status: RecommendationStatus::Pending,
        why_for_user: Some("Because".to_owned()),
        run_date: "2026-01-01".to_owned(),
        recommended_at: TEST_EPOCH_MS,
        ..RecommendationData::default()
    };
    f.h.services
        .store
        .write(move |tx| tx.upsert(&legacy, UpsertOpts::default()))
        .await
        .unwrap();
    // Our row is now in the library and the watchlist: the conditions under
    // which the legacy reconciler would re-notify it.
    f.h.watchlist
        .items
        .lock()
        .unwrap()
        .clone_from(&FetchResult::Ok(vec![
            common::media("radarr:3", "Tracked", MediaType::Movie, Some(3)),
            common::media("radarr:10", "Exact Movie", MediaType::Movie, Some(10)),
        ]));
    f.clock.set(TEST_EPOCH_MS + 3 * HOUR);
    run_recommendation_pipeline(&f.h.services, None, PipelineOptions::default())
        .await
        .expect("legacy run");

    let pushes = f.h.notifier.pushes();
    assert_eq!(pushes.len(), 1, "only the legacy row is re-notified");
    assert_eq!(pushes[0].message, "Because");
    let rows = f.rows().await;
    let ours = rows.iter().find(|r| r.recommendation_id == id).unwrap();
    assert_eq!(ours.status, RecommendationStatus::Pending);
    assert_eq!(ours.notification_state, Some(NotificationState::Unknown));
    assert_eq!(
        ours.chat_record.as_ref().unwrap().delivery,
        ChatDelivery::Unknown
    );
    let legacy = rows
        .iter()
        .find(|r| r.recommendation_id == "legacy")
        .unwrap();
    assert_eq!(legacy.status, RecommendationStatus::Notified);

    // Only the caller can reconcile it: a later receipt finishes it.
    let later = f
        .call(sent(&id, 2, "m1", TEST_EPOCH_MS + HOUR))
        .await
        .expect("reconciled");
    assert_eq!(later["recommendation"]["status"], json!("notified"));
}

#[tokio::test]
async fn an_unknown_send_cannot_be_withdrawn_and_stays_excluded_until_a_receipt() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.call(json!({
        "action": "finish", "dryRun": false, "recommendationId": id, "expectedRevision": 1,
        "delivery": {"state": "unknown", "reason": "chat send timed out"},
    }))
    .await
    .expect("unknown");
    let before = f.rows().await;
    for (dry_run, revision) in [(true, 2), (false, 2), (false, 1)] {
        let refused = f
            .call(json!({
                "action": "withdraw", "dryRun": dry_run, "recommendationId": id,
                "expectedRevision": revision, "reason": "never sent", "notPresented": true,
            }))
            .await
            .expect_err("unknown is not withdrawable");
        assert_eq!(refused.phase, ToolPhase::Execute);
        assert!(refused.message.contains("unknown send cannot be withdrawn"));
    }
    let after = f.rows().await;
    assert_eq!(after, before, "no status, revision or field changed");
    assert_eq!(after[0].status, RecommendationStatus::Pending);
    assert_eq!(after[0].chat_record.as_ref().unwrap().revision, 2);
    assert_eq!(
        codes(&f.reserve("k2", 10).await),
        vec!["pending_recommendation"]
    );
    // A day later the title is still excluded: an unknown send never ages out.
    f.clock.set(TEST_EPOCH_MS + 3 * DAY);
    assert_eq!(
        codes(&f.reserve("k2", 10).await),
        vec!["pending_recommendation"]
    );
    let sent_at = TEST_EPOCH_MS + HOUR;
    let done = f.call(sent(&id, 2, "m1", sent_at)).await.expect("receipt");
    assert_eq!(done["recommendation"]["status"], json!("notified"));
    assert_eq!(done["recommendation"]["notifiedAt"], json!(sent_at));
}

#[tokio::test]
async fn withdrawing_a_reserved_row_again_with_the_same_reason_is_unchanged() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let withdraw = |reason: &str| {
        json!({
            "action": "withdraw", "dryRun": false, "recommendationId": id,
            "expectedRevision": 1, "reason": reason, "notPresented": true,
        })
    };
    assert_eq!(
        f.call(withdraw("never sent")).await.unwrap()["result"],
        json!("changed")
    );
    assert_eq!(
        f.call(withdraw("never sent")).await.unwrap()["result"],
        json!("unchanged")
    );
    let other = f.call(withdraw("other")).await.expect_err("conflict");
    assert!(other.message.contains("different reason"));
}

#[tokio::test]
async fn a_stale_assistant_reservation_is_never_auto_failed() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.clock.set(TEST_EPOCH_MS + 2 * HOUR);
    run_recommendation_pipeline(&f.h.services, None, PipelineOptions::default())
        .await
        .expect("legacy run");
    let row = f
        .rows()
        .await
        .into_iter()
        .find(|r| r.recommendation_id == id)
        .unwrap();
    assert_eq!(row.status, RecommendationStatus::Pending);
    assert_eq!(row.resolved_at, None);
    assert!(f.h.notifier.pushes().is_empty());
}

#[tokio::test]
async fn withdrawal_needs_a_never_presented_confirmation_and_keeps_the_retry_exclusion() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let withdraw = |not_presented: bool, dry_run: bool| {
        json!({
            "action": "withdraw",
            "dryRun": dry_run,
            "recommendationId": id,
            "expectedRevision": 1,
            "reason": "chat crashed before sending",
            "notPresented": not_presented,
        })
    };
    let refused = f.call(withdraw(false, false)).await.expect_err("schema");
    assert_eq!(refused.phase, ToolPhase::Input);
    let preview = f.call(withdraw(true, true)).await.expect("dry run");
    assert_eq!(preview["result"], json!("would_change"));
    assert_eq!(f.rows().await[0].status, RecommendationStatus::Pending);

    let done = f.call(withdraw(true, false)).await.expect("withdraw");
    assert_eq!(done["recommendation"]["status"], json!("failed"));
    assert_eq!(
        done["recommendation"]["chat"]["delivery"],
        json!("withdrawn")
    );
    let blocked = f.reserve("k2", 10).await;
    assert_eq!(codes(&blocked), vec!["recently_recommended"]);
    f.clock.set(TEST_EPOCH_MS + DAY + HOUR);
    let again = f.reserve("k2", 10).await;
    assert_eq!(again["result"], json!("reserved"), "{again}");

    let sent_id = f.reserve("k3", 11).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.call(sent(&sent_id, 1, "m1", TEST_EPOCH_MS + DAY + HOUR))
        .await
        .expect("sent");
    let presented = f
        .call(json!({
            "action": "withdraw",
            "dryRun": false,
            "recommendationId": sent_id,
            "expectedRevision": 2,
            "reason": "changed my mind",
            "notPresented": true,
        }))
        .await
        .expect_err("presented");
    assert!(presented.message.contains("cannot be withdrawn"));
}

#[tokio::test]
async fn a_pending_reservation_never_gains_a_start_or_appears_on_deck() {
    let f = Fixture::new().await;
    f.reserve("k1", 10).await;
    *f.h.library.in_progress.lock().unwrap() = FetchResult::Ok(vec![InProgressItem {
        item: common::media("plex://exact", "Exact Movie", MediaType::Movie, Some(10)),
        progress: 0.5,
        last_viewed_at: TEST_EPOCH_MS + HOUR,
    }]);
    f.clock.set(TEST_EPOCH_MS + 2 * HOUR);
    let summary = run_recommendation_sync(&f.h.services).await.expect("sync");
    assert!(matches!(summary, SyncSummary::Synced { outcomes: 0, .. }));
    let rows = f.rows().await;
    assert_eq!(rows[0].status, RecommendationStatus::Pending);
    assert_eq!(rows[0].started_at, None);
    assert!(select_on_deck(&rows).is_empty());
}

#[tokio::test]
async fn the_extracted_sync_labels_outcomes_and_evidence_without_models_pushes_or_acquisition() {
    let f = Fixture::new().await;
    let id = f.reserve("k1", 10).await["recommendation"]["recommendationId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.call(sent(&id, 1, "m1", TEST_EPOCH_MS))
        .await
        .expect("sent");
    f.h.library
        .history
        .lock()
        .unwrap()
        .clone_from(&FetchResult::Ok(vec![WatchedItem {
            item: common::media("plex://exact", "Exact Movie", MediaType::Movie, Some(10)),
            viewed_at: TEST_EPOCH_MS + HOUR,
            view_count: 1,
            completion: Some(0.95),
        }]));
    f.clock.set(TEST_EPOCH_MS + 2 * HOUR);
    let task = MediaRecommendationSyncTask::create(
        f.h.services.clone(),
        omni_tasks::CronSchedule::parse("0 0 17 * * 1,3,5", &jiff::tz::TimeZone::UTC).unwrap(),
    );
    // The test config lacks Plex and Arr settings, so the task disables itself;
    // the sync function it runs is exercised directly.
    assert!(task.is_none());
    let summary = run_recommendation_sync(&f.h.services).await.expect("sync");
    let SyncSummary::Synced {
        outcomes,
        evidence_inserted,
    } = summary
    else {
        panic!("skipped: {summary:?}");
    };
    assert_eq!(outcomes, 1);
    assert!(evidence_inserted >= 2, "watch and outcome evidence");
    let row = &f.rows().await[0];
    assert_eq!(row.status, RecommendationStatus::Watched);
    assert_eq!(row.started_at, Some(TEST_EPOCH_MS + HOUR));
    assert!(f.h.app.ai.requests().is_empty());
    assert!(f.h.notifier.pushes().is_empty());
    assert!(f.h.app.pushes.all().is_empty());
    assert!(f.h.watchlist.adds().is_empty());
}

#[tokio::test]
async fn legacy_rows_still_read_with_a_legacy_origin() {
    let f = Fixture::new().await;
    let legacy = RecommendationData {
        recommendation_id: "legacy".to_owned(),
        canonical_id: "tmdb:movie:11".to_owned(),
        tmdb_id: 11,
        media_type: MediaType::Movie,
        title: "Second Movie".to_owned(),
        status: RecommendationStatus::Notified,
        confidence: Some(0.8),
        run_date: "2026-01-01".to_owned(),
        recommended_at: TEST_EPOCH_MS,
        ..RecommendationData::default()
    };
    f.h.services
        .store
        .write(move |tx| tx.upsert(&legacy, UpsertOpts::default()))
        .await
        .unwrap();
    let out = f.get("legacy").await;
    assert_eq!(out["recommendation"]["origin"], json!("legacy_pipeline"));
    assert_eq!(out["recommendation"]["chat"], Value::Null);
    assert_eq!(out["recommendation"]["confidence"], json!(0.8));
}

#[tokio::test]
async fn the_legacy_pipeline_skips_a_title_an_assistant_reserved_mid_run() {
    let f = Fixture::new().await;
    let mut candidate = common::tmdb_title(10, MediaType::Movie);
    candidate.title = "Exact Movie".to_owned();
    candidate.vote_average = 8.0;
    candidate.vote_count = 1000.0;
    *f.h.catalog.trending.lock().unwrap() = vec![candidate];
    // Candidate enrichment runs after the run read its exclusions.
    let store = f.h.services.store.clone();
    f.h.catalog.set_details(Arc::new(move |_, _| {
        let store = store.clone();
        Box::pin(async move {
            let reserved = RecommendationData {
                recommendation_id: "assistant".to_owned(),
                canonical_id: "tmdb:movie:10".to_owned(),
                tmdb_id: 10,
                media_type: MediaType::Movie,
                title: "Exact Movie".to_owned(),
                status: RecommendationStatus::Pending,
                run_date: "2026-01-01".to_owned(),
                recommended_at: TEST_EPOCH_MS,
                origin: Some(RecommendationOrigin::AssistantChat),
                chat_record: Some(omni_runtime::chat_record::ChatRecord::reserved(
                    "k".to_owned(),
                    TEST_EPOCH_MS,
                )),
                ..RecommendationData::default()
            };
            store
                .write(move |tx| tx.upsert(&reserved, UpsertOpts::default()))
                .await
                .unwrap();
            Ok(Default::default())
        })
    }));
    let scores = json!({"scores": [{"candidate_id": "tmdb:movie:10", "taste_match": 90,
        "novelty": 60, "effort_fit": 90, "confidence": 0.9, "risks": []}]});
    f.h.app.ai.script(
        omni_ai::ModelRole::RecsShortlist,
        vec![omni_ai::GenerateResponse::text(scores.to_string())],
    );
    let select = json!({
        "decision": "select",
        "selected": {"candidate_id": "tmdb:movie:10", "why_for_user": "A fit", "caveats": [],
                     "confidence": 0.9, "notification": {"title": "Exact", "message": "A fit"}},
        "backup": null,
        "no_add_reason": null
    });
    f.h.app.ai.script(
        omni_ai::ModelRole::RecsSelection,
        vec![omni_ai::GenerateResponse::text(select.to_string())],
    );
    let summary = run_recommendation_pipeline(&f.h.services, None, PipelineOptions::default())
        .await
        .expect("run");
    assert_eq!(summary, "no_add: selected and backup are already tracked");
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].recommendation_id, "assistant");
    assert!(f.h.watchlist.adds().is_empty());
    assert!(f.h.notifier.pushes().is_empty());
}

#[test]
fn legacy_rows_encode_without_the_new_fields() {
    let legacy = RecommendationData {
        recommendation_id: "legacy".to_owned(),
        canonical_id: "tmdb:movie:11".to_owned(),
        tmdb_id: 11,
        media_type: MediaType::Movie,
        title: "Second Movie".to_owned(),
        run_date: "2026-01-01".to_owned(),
        ..RecommendationData::default()
    };
    let value = serde_json::to_value(&legacy).unwrap();
    assert!(value.get("origin").is_none());
    assert!(value.get("chatRecord").is_none());
    let decoded: RecommendationData = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, legacy);
}
