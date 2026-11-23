//! Podcast taste reflection, pinned evidence-id and fingerprint values, and a
//! store-backed reflection run with scripted models.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::rec;
use omni_ai::{GenerateResponse, ModelRole};
use omni_podcasts::account::ListenedEpisode;
use omni_podcasts::models::Models;
use omni_podcasts::persistence::{
    PodcastFeedback, PodcastRecommendationData, PodcastRecommendationStatus,
};
use omni_podcasts::reflection::core::{
    PodcastTasteReflectionInput, PodcastTasteReflectionResult, RawClaim, RawProfile,
    run_podcast_taste_reflection,
};
use omni_podcasts::reflection::types::{
    FeedbackCounts, PodcastBehavioralStats, PodcastTasteClaim, PodcastTasteEvidenceData,
    PodcastTasteEvidenceKind, RecommendationCounts,
};
use omni_podcasts::reflection::{
    PodcastTasteProfileData, compute_podcast_behavioral_stats, derive_listen_evidence,
    derive_recommendation_evidence, fingerprint_evidence, format_podcast_taste_profile_digest,
    normalize_show_key, select_podcast_reflection_evidence, validate_podcast_profile,
};
use omni_store::cbor::Extra;
use omni_testkit::TestApp;

fn evidence(id: &str) -> PodcastTasteEvidenceData {
    PodcastTasteEvidenceData {
        evidence_id: id.into(),
        kind: PodcastTasteEvidenceKind::Listen,
        show_key: "search engine".into(),
        show_title: "Search Engine".into(),
        episode_title: Some("What is money?".into()),
        recommendation_id: None,
        discovered_via: None,
        matched_voices: None,
        duration_minutes: None,
        observed_at: 1_000,
        completion: Some(1.0),
        starred: None,
        recommendation_status: None,
        feedback: None,
        note: None,
        extra: Extra::default(),
    }
}

fn listen() -> ListenedEpisode {
    ListenedEpisode {
        show_title: "Search Engine".into(),
        episode_title: "What is money?".into(),
        episode_guid: Some("guid-1".into()),
        listened_at: 1_000,
        completion: Some(0.92),
        starred: Some(true),
        ..ListenedEpisode::default()
    }
}

fn reflection_rec() -> PodcastRecommendationData {
    PodcastRecommendationData {
        recommendation_id: "rec-1".into(),
        episode_id: "itunes:1#guid".into(),
        show_id: "itunes:1".into(),
        show_title: "Blocked and Reported".into(),
        episode_title: "Episode 100".into(),
        feed_url: "https://example.com/feed".into(),
        episode_guid: "guid-100".into(),
        published_at: 900,
        status: PodcastRecommendationStatus::Listened,
        run_date: "2026-07-01".into(),
        recommended_at: 950,
        notified_at: Some(960),
        resolved_at: Some(990),
        ..rec()
    }
}

#[test]
fn lowercases_and_trims() {
    assert_eq!(normalize_show_key("  Search Engine "), "search engine");
}

#[test]
fn is_deterministic_for_identical_observations() {
    let a = &derive_listen_evidence(&[listen()])[0];
    let b = &derive_listen_evidence(&[listen()])[0];
    assert_eq!(a.evidence_id, b.evidence_id);
    assert_eq!(a.kind, PodcastTasteEvidenceKind::Listen);
    assert_eq!(a.show_key, "search engine");
}

#[test]
fn changes_id_when_the_observation_changes() {
    let a = &derive_listen_evidence(&[listen()])[0];
    let b = &derive_listen_evidence(&[ListenedEpisode {
        completion: Some(0.5),
        ..listen()
    }])[0];
    assert_ne!(a.evidence_id, b.evidence_id);
}

#[test]
fn emits_an_outcome_row_and_a_feedback_row_only_when_feedback_exists() {
    assert_eq!(derive_recommendation_evidence(&[reflection_rec()]).len(), 1);
    let with_feedback = derive_recommendation_evidence(&[PodcastRecommendationData {
        feedback: Some(PodcastFeedback::GoodPick),
        feedback_at: Some(995),
        ..reflection_rec()
    }]);
    assert_eq!(with_feedback.len(), 2);
    assert_eq!(
        with_feedback[1].kind,
        PodcastTasteEvidenceKind::ExplicitFeedback
    );
    assert_eq!(with_feedback[1].feedback, Some(PodcastFeedback::GoodPick));
}

#[test]
fn is_order_independent() {
    let a = evidence("listen:a");
    let b = PodcastTasteEvidenceData {
        show_key: "other show".into(),
        ..evidence("listen:b")
    };
    assert_eq!(
        fingerprint_evidence(&[a.clone(), b.clone()]).unwrap(),
        fingerprint_evidence(&[b, a]).unwrap()
    );
}

#[test]
fn changes_when_evidence_changes() {
    let a = evidence("listen:a");
    let changed = PodcastTasteEvidenceData {
        completion: Some(0.2),
        ..a.clone()
    };
    assert_ne!(
        fingerprint_evidence(&[a]).unwrap(),
        fingerprint_evidence(&[changed]).unwrap()
    );
}

#[test]
fn evidence_ids_and_fingerprint_match_stored_values() {
    let listens = [
        listen(),
        ListenedEpisode {
            show_title: "  Other Show ".into(),
            episode_title: "No guid".into(),
            episode_guid: None,
            listened_at: 1_784_242_849_000,
            completion: Some(0.528_111_758_492_978_3),
            starred: Some(false),
            ..ListenedEpisode::default()
        },
        ListenedEpisode {
            show_title: "Third".into(),
            episode_title: "Unknown completion".into(),
            listened_at: 5,
            completion: Some(0.125),
            ..ListenedEpisode::default()
        },
    ];
    let full = PodcastRecommendationData {
        feedback: Some(PodcastFeedback::GoodPick),
        feedback_at: Some(995),
        feedback_note: Some("great \"one\"".into()),
        discovered_via: Some("guest: X (web)".into()),
        matched_voices: Some(vec!["X".into(), "Y".into()]),
        duration_minutes: Some(57),
        ..reflection_rec()
    };
    let sparse = PodcastRecommendationData {
        recommendation_id: "rec-2".into(),
        status: PodcastRecommendationStatus::Pending,
        resolved_at: None,
        notified_at: None,
        ..reflection_rec()
    };
    let mut all = derive_listen_evidence(&listens);
    all.extend(derive_recommendation_evidence(&[full, sparse]));
    let ids: Vec<&str> = all.iter().map(|e| e.evidence_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "listen:2681117a888150213c777a30",
            "listen:3db5335ecab943c96758b7d3",
            "listen:b19f976252c52eb485af617f",
            "recommendation:e093cf7f0517a9446cb733b8",
            "recommendation:ca65da7879897fb9495d2689",
            "recommendation:4dd2366c6264d5b5d63e2441",
        ]
    );
    assert_eq!(
        fingerprint_evidence(&all).unwrap(),
        "e329eb3603c80aff17bc7e15"
    );
}

#[test]
fn prefers_feedback_then_delivered_outcomes_then_listens() {
    let listen = PodcastTasteEvidenceData {
        observed_at: 3_000,
        ..evidence("listen:a")
    };
    let outcome = PodcastTasteEvidenceData {
        kind: PodcastTasteEvidenceKind::RecommendationOutcome,
        recommendation_status: Some(PodcastRecommendationStatus::Listened),
        observed_at: 2_000,
        ..evidence("recommendation:b")
    };
    let feedback = PodcastTasteEvidenceData {
        kind: PodcastTasteEvidenceKind::ExplicitFeedback,
        feedback: Some(PodcastFeedback::NotForMe),
        observed_at: 1_000,
        ..evidence("recommendation:c")
    };
    let selected = select_podcast_reflection_evidence(&[listen, outcome, feedback], 2);
    let ids: Vec<&str> = selected.iter().map(|e| e.evidence_id.as_str()).collect();
    assert_eq!(ids, vec!["recommendation:c", "recommendation:b"]);
}

#[test]
fn counts_listens_outcomes_and_feedback_with_dedup() {
    let stats = compute_podcast_behavioral_stats(&[
        PodcastTasteEvidenceData {
            completion: Some(1.0),
            starred: Some(true),
            ..evidence("l1")
        },
        // Newer observation of the same episode wins.
        PodcastTasteEvidenceData {
            completion: Some(0.1),
            observed_at: 2_000,
            ..evidence("l2")
        },
        PodcastTasteEvidenceData {
            show_key: "other".into(),
            show_title: "Other".into(),
            episode_title: Some("Ep 2".into()),
            completion: None,
            ..evidence("l3")
        },
        PodcastTasteEvidenceData {
            kind: PodcastTasteEvidenceKind::RecommendationOutcome,
            recommendation_id: Some("rec-1".into()),
            recommendation_status: Some(PodcastRecommendationStatus::Abandoned),
            ..evidence("o1")
        },
        PodcastTasteEvidenceData {
            kind: PodcastTasteEvidenceKind::ExplicitFeedback,
            recommendation_id: Some("rec-1".into()),
            feedback: Some(PodcastFeedback::NotForMe),
            ..evidence("f1")
        },
    ]);
    assert_eq!(stats.started_episodes, 2);
    // l2 (10% completion) superseded l1, so only the no-completion listen counts.
    assert_eq!(stats.listened_episodes, 1);
    assert_eq!(stats.distinct_shows, 2);
    assert_eq!(stats.recommendations.abandoned, 1);
    assert_eq!(stats.feedback.not_for_me, 1);
}

fn validation_evidence() -> Vec<PodcastTasteEvidenceData> {
    vec![
        PodcastTasteEvidenceData {
            show_key: "show a".into(),
            completion: Some(1.0),
            ..evidence("l1")
        },
        PodcastTasteEvidenceData {
            show_key: "show b".into(),
            completion: Some(0.9),
            ..evidence("l2")
        },
        PodcastTasteEvidenceData {
            kind: PodcastTasteEvidenceKind::ExplicitFeedback,
            show_key: "show c".into(),
            feedback: Some(PodcastFeedback::NotForMe),
            ..evidence("f1")
        },
        // Not taste-bearing: a shallow, unstarred listen.
        PodcastTasteEvidenceData {
            show_key: "show d".into(),
            completion: Some(0.1),
            ..evidence("weak")
        },
    ]
}

fn claim(text: &str, ids: &[&str]) -> RawClaim {
    RawClaim {
        claim: text.into(),
        confidence: 0.8,
        evidence_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn keeps_claims_with_two_independent_shows() {
    let (profile, rejected) = validate_podcast_profile(
        &RawProfile {
            stable_preferences: vec![claim("Likes deep-dive interviews", &["l1", "l2"])],
            ..RawProfile::default()
        },
        &validation_evidence(),
    );
    assert_eq!(profile.stable_preferences.len(), 1);
    assert_eq!(rejected, 0);
}

#[test]
fn drops_claims_backed_by_one_show_or_weak_evidence() {
    let (profile, rejected) = validate_podcast_profile(
        &RawProfile {
            stable_preferences: vec![
                claim("One-show claim", &["l1"]),
                claim("Weak-evidence claim", &["weak"]),
                claim("Phantom ids", &["nope", "nada"]),
            ],
            ..RawProfile::default()
        },
        &validation_evidence(),
    );
    assert!(profile.stable_preferences.is_empty());
    assert_eq!(rejected, 3);
}

#[test]
fn allows_an_aversion_backed_by_a_single_explicit_not_for_me() {
    let (profile, _) = validate_podcast_profile(
        &RawProfile {
            aversions: vec![claim("Not into true crime", &["f1"])],
            ..RawProfile::default()
        },
        &validation_evidence(),
    );
    assert_eq!(profile.aversions.len(), 1);
}

#[test]
fn formats_a_profile_with_claims() {
    let profile = PodcastTasteProfileData {
        profile_id: "v2:abc".into(),
        version: 2,
        generated_at: 1_000,
        evidence_fingerprint: "abc".into(),
        evidence_count: 10,
        model_id: "openai:gpt-6-luna".into(),
        prompt_version: "podcast-taste-reflection-v1".into(),
        summary: "Likes sharp interview shows.".into(),
        stable_preferences: vec![PodcastTasteClaim {
            claim: "Deep-dive interviews".into(),
            confidence: 0.8,
            evidence_ids: vec!["l1".into()],
            extra: Extra::default(),
        }],
        conditional_preferences: Vec::new(),
        aversions: Vec::new(),
        current_saturation: Vec::new(),
        exploration_targets: Vec::new(),
        uncertainties: Vec::new(),
        stats: PodcastBehavioralStats {
            listened_episodes: 5,
            started_episodes: 6,
            starred_episodes: 1,
            distinct_shows: 4,
            recommendations: RecommendationCounts {
                total: 3,
                listened: 1,
                abandoned: 0,
                ignored: 1,
                failed: 0,
                awaiting_outcome: 1,
            },
            feedback: FeedbackCounts {
                good_pick: 1,
                not_for_me: 0,
            },
        },
        extra: Extra::default(),
    };
    let digest = format_podcast_taste_profile_digest(Some(&profile));
    assert!(digest.contains("Reflective podcast taste profile v2"));
    assert!(digest.contains("Stable preferences: Deep-dive interviews"));
}

fn profile_response(ids: &[&str]) -> GenerateResponse {
    let profile = RawProfile {
        stable_preferences: vec![claim("Finishes long interviews", ids)],
        ..RawProfile::default()
    };
    GenerateResponse::text(serde_json::to_string(&profile).unwrap())
}

#[tokio::test]
async fn checkpoints_a_profile_and_skips_the_model_when_evidence_is_unchanged() {
    let app = TestApp::new().await;
    let models = Models {
        ai: app.ctx.ai.clone(),
        config: app.ctx.config.clone(),
    };
    let listens = vec![
        listen(),
        ListenedEpisode {
            show_title: "Hard Fork".into(),
            episode_guid: Some("guid-2".into()),
            ..listen()
        },
    ];
    let ids: Vec<String> = derive_listen_evidence(&listens)
        .into_iter()
        .map(|e| e.evidence_id)
        .collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    app.ai.script(
        ModelRole::PodcastTasteReflection,
        vec![profile_response(&id_refs), profile_response(&id_refs)],
    );
    let input = || PodcastTasteReflectionInput {
        listened: listens.clone(),
        recommendations: Vec::new(),
        now: 5_000,
        max_evidence: 160,
    };
    let created = run_podcast_taste_reflection(&app.ctx.store, &models, input())
        .await
        .unwrap();
    let PodcastTasteReflectionResult::Created {
        profile,
        inserted_evidence,
        rejected_claims,
    } = created
    else {
        panic!("expected a new profile");
    };
    assert_eq!(inserted_evidence, 2);
    assert_eq!(rejected_claims, 0);
    assert_eq!(profile.version, 1);
    assert_eq!(
        profile.profile_id,
        format!("v1:{}", profile.evidence_fingerprint)
    );
    assert_eq!(profile.model_id, "openai:gpt-6-luna");
    assert_eq!(profile.stable_preferences.len(), 1);
    assert_eq!(app.ai.requests().len(), 2, "draft plus critic");

    let again = run_podcast_taste_reflection(&app.ctx.store, &models, input())
        .await
        .unwrap();
    assert!(matches!(
        again,
        PodcastTasteReflectionResult::Unchanged {
            inserted_evidence: 0,
            ..
        }
    ));
    assert_eq!(app.ai.requests().len(), 2, "no model call when unchanged");
}

#[tokio::test]
async fn an_overlong_model_claim_is_rejected_without_failing_the_reflection() {
    let app = TestApp::new().await;
    let models = Models {
        ai: app.ctx.ai.clone(),
        config: app.ctx.config.clone(),
    };
    let listens = vec![
        listen(),
        ListenedEpisode {
            show_title: "Hard Fork".into(),
            episode_guid: Some("guid-2".into()),
            ..listen()
        },
    ];
    let ids: Vec<String> = derive_listen_evidence(&listens)
        .into_iter()
        .map(|e| e.evidence_id)
        .collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let response = || {
        let profile = RawProfile {
            stable_preferences: vec![
                claim("Finishes long interviews", &id_refs),
                claim(&"x".repeat(279), &id_refs),
            ],
            ..RawProfile::default()
        };
        GenerateResponse::text(serde_json::to_string(&profile).unwrap())
    };
    app.ai.script(
        ModelRole::PodcastTasteReflection,
        vec![response(), response()],
    );
    let created = run_podcast_taste_reflection(
        &app.ctx.store,
        &models,
        PodcastTasteReflectionInput {
            listened: listens,
            recommendations: Vec::new(),
            now: 5_000,
            max_evidence: 160,
        },
    )
    .await
    .unwrap();
    let PodcastTasteReflectionResult::Created {
        profile,
        rejected_claims,
        ..
    } = created
    else {
        panic!("expected a new profile");
    };
    assert_eq!(rejected_claims, 1);
    assert_eq!(profile.stable_preferences.len(), 1);
    assert_eq!(
        profile.stable_preferences[0].claim,
        "Finishes long interviews"
    );
}

#[tokio::test]
async fn an_unreadable_listen_history_skips_the_reflection_as_degraded() {
    use std::sync::Arc;

    use common::{FakeAccount, FakeAccounts};
    use omni_podcasts::account::Unavailable;
    use omni_podcasts::reflection::task::PodcastTasteReflectionTask;
    use omni_tasks::{CronSchedule, Task as _};

    let app = TestApp::new().await;
    let account = FakeAccount {
        history: Err(Unavailable::new("Castro GET /episodes failed: HTTP 502")),
        ..FakeAccount::default()
    };
    let task = PodcastTasteReflectionTask::new(
        CronSchedule::parse("0 0 4 * * 0", &jiff::tz::TimeZone::UTC).unwrap(),
        app.ctx.store.clone(),
        app.ctx.clock.clone(),
        Models {
            ai: app.ctx.ai.clone(),
            config: app.ctx.config.clone(),
        },
        Arc::new(FakeAccounts(Arc::new(account))),
    );
    let (result, degraded) = omni_tasks::collect_degraded(task.run_once()).await;
    result.unwrap();
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("skipped: Castro GET /episodes failed: HTTP 502")
    );
    assert_eq!(
        degraded,
        vec!["listen history unavailable: Castro GET /episodes failed: HTTP 502"]
    );
    assert!(app.ai.requests().is_empty());
}
