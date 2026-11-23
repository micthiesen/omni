//! Taste evidence and profiles, plus scripted
//! reflection runs (created, then unchanged without a model call).
#![allow(clippy::expect_used)]

use crate::common;

use omni_ai::{GenerateResponse, ModelRole};
use omni_api::media::{
    CommitmentAssessment, CommitmentPreference, CommitmentPreferences, MediaType,
    RecommendationFeedback, RecommendationStatus, SourcePerformance, TasteBehaviorStats,
    TasteClaim, TasteProfile,
};
use omni_media::persistence::RecommendationData;
use omni_media::taste::reflection::{
    CommitmentPreferenceSchema, RawClaim, RawCommitment, RawCommitmentPreferences, RawProfile,
    TasteReflectionInput, TasteReflectionResult,
};
use omni_media::taste::{
    CanonicalWatchObservation, TasteEvidenceData, TasteEvidenceKind, compute_behavioral_stats,
    derive_recommendation_evidence, derive_watch_evidence, fingerprint_evidence,
    format_taste_profile_digest, run_taste_reflection, select_reflection_evidence,
    validate_profile,
};
use omni_media::types::{MediaItem, WatchedItem};
use serde_json::json;

const NOW: i64 = 1_800_000_000_000;

fn observation(
    canonical_id: &str,
    guid: &str,
    title: &str,
    media_type: MediaType,
    viewed_at: i64,
    view_count: i64,
    completion: f64,
) -> CanonicalWatchObservation {
    CanonicalWatchObservation {
        canonical_id: canonical_id.to_owned(),
        item: WatchedItem {
            item: MediaItem {
                guid: guid.to_owned(),
                title: title.to_owned(),
                year: None,
                media_type,
                external_ids: None,
                title_slug: None,
            },
            viewed_at,
            view_count,
            completion: Some(completion),
        },
        metadata: None,
    }
}

fn watch(evidence_id: &str, observed_at: i64, view_count: i64) -> TasteEvidenceData {
    TasteEvidenceData {
        evidence_id: evidence_id.to_owned(),
        kind: TasteEvidenceKind::PlexWatch,
        canonical_id: "tmdb:movie:1".to_owned(),
        title: "Arrival".to_owned(),
        media_type: MediaType::Movie,
        observed_at,
        view_count: Some(view_count),
        completion: Some(1.0),
        ..TasteEvidenceData::default()
    }
}

fn rec_evidence(
    evidence_id: &str,
    kind: TasteEvidenceKind,
    update: impl FnOnce(&mut TasteEvidenceData),
) -> TasteEvidenceData {
    let mut item = TasteEvidenceData {
        evidence_id: evidence_id.to_owned(),
        kind,
        canonical_id: "tmdb:movie:1".to_owned(),
        title: "Arrival".to_owned(),
        media_type: MediaType::Movie,
        observed_at: NOW,
        recommendation_id: Some("rec-1".to_owned()),
        recommended_at: Some(NOW - 60 * 60 * 1000),
        ..TasteEvidenceData::default()
    };
    update(&mut item);
    item
}

#[test]
fn derives_deterministic_watch_ids_and_fingerprints_independent_of_order() {
    let observations = vec![
        observation(
            "tmdb:movie:1",
            "tmdb://1",
            "Arrival",
            MediaType::Movie,
            NOW,
            2,
            0.98,
        ),
        observation(
            "tmdb:tv:2",
            "tmdb://2",
            "Severance",
            MediaType::Tv,
            NOW - 1,
            1,
            1.0,
        ),
    ];
    let first = derive_watch_evidence(&observations);
    let second = derive_watch_evidence(&observations);
    assert_eq!(first, second);
    let mut reversed = first.clone();
    reversed.reverse();
    assert_eq!(
        fingerprint_evidence(&first),
        fingerprint_evidence(&reversed)
    );
}

fn recommendation(update: impl FnOnce(&mut RecommendationData)) -> RecommendationData {
    let mut rec = RecommendationData {
        recommendation_id: "rec-1".to_owned(),
        canonical_id: "tmdb:movie:1".to_owned(),
        tmdb_id: 1,
        media_type: MediaType::Movie,
        title: "Arrival".to_owned(),
        status: RecommendationStatus::Watched,
        run_date: "2027-01-01".to_owned(),
        recommended_at: NOW - 1000,
        ..RecommendationData::default()
    };
    update(&mut rec);
    rec
}

#[test]
fn records_recommendation_outcome_and_explicit_feedback_separately() {
    let evidence = derive_recommendation_evidence(&[recommendation(|r| {
        r.resolved_at = Some(NOW);
        r.feedback = Some(RecommendationFeedback::GoodPick);
        r.feedback_at = Some(NOW + 1);
    })]);
    assert_eq!(
        evidence.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            TasteEvidenceKind::RecommendationOutcome,
            TasteEvidenceKind::ExplicitFeedback
        ]
    );
    assert_ne!(evidence[0].evidence_id, evidence[1].evidence_id);
}

#[test]
fn records_pending_recommendations_so_awaiting_outcome_stats_are_complete() {
    let evidence = derive_recommendation_evidence(&[recommendation(|r| {
        r.recommendation_id = "rec-pending".to_owned();
        r.canonical_id = "tmdb:movie:2".to_owned();
        r.tmdb_id = 2;
        r.title = "Pending".to_owned();
        r.status = RecommendationStatus::Notified;
        r.notified_at = Some(NOW);
    })]);
    let stats = compute_behavioral_stats(&evidence).recommendations;
    assert_eq!(stats.total, 1);
    assert_eq!(stats.awaiting_outcome, 1);
}

#[test]
fn deduplicates_watch_snapshots_and_uses_latest_recommendation_signals() {
    let evidence = vec![
        watch("watch-old", NOW - 100, 1),
        watch("watch-new", NOW, 3),
        rec_evidence("outcome", TasteEvidenceKind::RecommendationOutcome, |e| {
            e.recommendation_status = Some(RecommendationStatus::Watched);
            e.source = Some("similar".to_owned());
        }),
        rec_evidence("feedback", TasteEvidenceKind::ExplicitFeedback, |e| {
            e.feedback = Some(RecommendationFeedback::GoodPick);
            e.source = Some("similar".to_owned());
        }),
    ];
    let stats = compute_behavioral_stats(&evidence);
    assert_eq!(stats.completed_movies, 1);
    assert_eq!(stats.rewatched_titles, 1);
    assert_eq!(stats.recommendations.total, 1);
    assert_eq!(stats.recommendations.watched, 1);
    assert_eq!(stats.feedback.good_pick, 1);
    let similar = stats.source_performance.get("similar").expect("similar");
    assert_eq!(
        (similar.total, similar.watched, similar.good_pick),
        (1, 1, 1)
    );
}

#[test]
fn does_not_count_a_partial_plex_observation_as_a_completed_title() {
    let partial = TasteEvidenceData {
        completion: Some(0.35),
        ..watch("partial", NOW, 1)
    };
    let stats = compute_behavioral_stats(&[partial]);
    assert_eq!(stats.completed_movies, 0);
    assert_eq!(stats.rewatched_titles, 0);
}

#[test]
fn keeps_failed_attempts_out_of_delivered_totals_and_resolves_explicit_declines() {
    let failed = rec_evidence("failed", TasteEvidenceKind::RecommendationOutcome, |e| {
        e.recommendation_id = Some("rec-failed".to_owned());
        e.recommendation_status = Some(RecommendationStatus::Failed);
        e.source = Some("trending".to_owned());
    });
    let delivered = rec_evidence("delivered", TasteEvidenceKind::RecommendationOutcome, |e| {
        e.recommendation_id = Some("rec-declined".to_owned());
        e.recommendation_status = Some(RecommendationStatus::Notified);
        e.source = Some("similar".to_owned());
    });
    let declined = rec_evidence("declined", TasteEvidenceKind::ExplicitFeedback, |e| {
        e.recommendation_id = Some("rec-declined".to_owned());
        e.feedback = Some(RecommendationFeedback::NotForMe);
        e.source = Some("similar".to_owned());
    });
    let stats = compute_behavioral_stats(&[failed, delivered, declined]);
    assert_eq!(stats.recommendations.total, 1);
    assert_eq!(stats.recommendations.failed, 1);
    assert_eq!(stats.recommendations.awaiting_outcome, 0);
    assert_eq!(stats.source_performance.len(), 1);
    assert_eq!(
        stats.source_performance.get("similar"),
        Some(&SourcePerformance {
            total: 1,
            watched: 0,
            good_pick: 0,
            not_for_me: 1
        })
    );
}

fn claim(text: &str, confidence: f64, ids: &[&str]) -> RawClaim {
    RawClaim {
        claim: text.to_owned(),
        confidence,
        evidence_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn commitment(preference: CommitmentPreferenceSchema, ids: &[&str]) -> RawCommitment {
    RawCommitment {
        preference,
        confidence: 0.7,
        evidence_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn rejects_unsupported_claims_while_allowing_one_explicit_negative_aversion() {
    let positive_a = watch("positive-a", NOW, 1);
    let positive_b = TasteEvidenceData {
        canonical_id: "tmdb:movie:2".to_owned(),
        ..watch("positive-b", NOW - 1, 1)
    };
    let negative = rec_evidence("negative", TasteEvidenceKind::ExplicitFeedback, |e| {
        e.feedback = Some(RecommendationFeedback::NotForMe);
    });
    let both = ["positive-a", "positive-b"];
    let raw = RawProfile {
        stable_preferences: vec![
            claim("Likes thoughtful science fiction", 0.8, &both),
            claim("Likes musicals", 0.9, &["made-up"]),
        ],
        conditional_preferences: vec![],
        aversions: vec![claim("Avoid this pattern", 0.7, &["negative"])],
        current_saturation: vec![],
        exploration_targets: vec![claim("Nearby speculative drama", 0.6, &["positive-a"])],
        uncertainties: vec![claim("Comedy preferences", 0.5, &["positive-b"])],
        commitment_preferences: RawCommitmentPreferences {
            movies: commitment(CommitmentPreferenceSchema::Positive, &both),
            limited_series: commitment(CommitmentPreferenceSchema::Uncertain, &both),
            long_series: commitment(CommitmentPreferenceSchema::Negative, &both),
        },
    };
    let (profile, rejected) = validate_profile(&raw, &[positive_a, positive_b, negative]);
    assert_eq!(rejected, 1);
    assert_eq!(profile.stable_preferences.len(), 1);
    assert_eq!(profile.aversions.len(), 1);
    assert_eq!(
        profile.summary,
        "Evidence-backed preferences: Likes thoughtful science fiction. Evidence-backed aversions: Avoid this pattern."
    );
}

#[test]
fn prioritizes_explicit_feedback_within_the_evidence_prompt_bound() {
    let selected = select_reflection_evidence(
        &[
            watch("watch", NOW, 1),
            rec_evidence("outcome", TasteEvidenceKind::RecommendationOutcome, |e| {
                e.recommendation_status = Some(RecommendationStatus::Watched);
            }),
            rec_evidence("feedback", TasteEvidenceKind::ExplicitFeedback, |e| {
                e.feedback = Some(RecommendationFeedback::GoodPick);
            }),
        ],
        2,
    );
    assert_eq!(
        selected.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            TasteEvidenceKind::ExplicitFeedback,
            TasteEvidenceKind::RecommendationOutcome
        ]
    );
}

fn assessment(preference: CommitmentPreference, confidence: f64) -> CommitmentAssessment {
    CommitmentAssessment {
        preference,
        confidence,
        evidence_ids: vec!["a".to_owned(), "b".to_owned()],
    }
}

#[test]
fn formats_a_compact_profile_digest_for_recommendation_prompts() {
    let profile = TasteProfile {
        profile_id: "v1:test".to_owned(),
        version: 1,
        generated_at: NOW,
        evidence_fingerprint: "test".to_owned(),
        evidence_count: 2,
        model_id: "test:model".to_owned(),
        prompt_version: "test".to_owned(),
        summary: "Likes precise speculative stories.".to_owned(),
        stable_preferences: vec![TasteClaim {
            claim: "Precise speculative stories".to_owned(),
            confidence: 0.8,
            evidence_ids: vec!["a".to_owned(), "b".to_owned()],
        }],
        conditional_preferences: vec![],
        aversions: vec![],
        current_saturation: vec![],
        exploration_targets: vec![TasteClaim {
            claim: "International science fiction".to_owned(),
            confidence: 0.7,
            evidence_ids: vec!["a".to_owned()],
        }],
        uncertainties: vec![],
        commitment_preferences: CommitmentPreferences {
            movies: assessment(CommitmentPreference::Positive, 0.8),
            limited_series: assessment(CommitmentPreference::Neutral, 0.5),
            long_series: assessment(CommitmentPreference::Uncertain, 0.3),
        },
        stats: TasteBehaviorStats {
            completed_movies: 2,
            ..TasteBehaviorStats::default()
        },
    };
    let digest = format_taste_profile_digest(Some(&profile));
    assert!(digest.contains("Reflective taste profile v1"));
    assert!(digest.contains("International science fiction"));
    assert!(digest.ends_with(
        "Commitment fit: movies=positive, limited-series=neutral, long-series=uncertain"
    ));
}

fn raw_profile_json() -> serde_json::Value {
    let commitment = json!({"preference": "uncertain", "confidence": 0.5, "evidence_ids": []});
    json!({
        "stable_preferences": [], "conditional_preferences": [], "aversions": [],
        "current_saturation": [], "exploration_targets": [], "uncertainties": [],
        "commitment_preferences": {"movies": commitment, "limited_series": commitment, "long_series": commitment}
    })
}

#[tokio::test]
async fn creates_a_profile_then_skips_the_model_when_evidence_is_unchanged() {
    let h = common::Harness::new().await;
    h.app.ai.script(
        ModelRole::TasteReflection,
        vec![
            GenerateResponse::text(raw_profile_json().to_string()),
            GenerateResponse::text(raw_profile_json().to_string()),
        ],
    );
    let model = h
        .services
        .ai
        .model_for(&h.services.config, ModelRole::TasteReflection)
        .expect("model");
    let input = || TasteReflectionInput {
        watched: vec![observation(
            "tmdb:movie:1",
            "tmdb://1",
            "Arrival",
            MediaType::Movie,
            NOW,
            1,
            1.0,
        )],
        recommendations: Vec::new(),
        model: model.as_ref(),
        model_id: "openai:gpt-6-luna".to_owned(),
        now: NOW,
        max_evidence: None,
    };
    let created = run_taste_reflection(&h.services.store, &h.services.ai, input())
        .await
        .expect("reflection");
    let TasteReflectionResult::Created {
        profile,
        inserted_evidence,
        rejected_claims,
    } = created
    else {
        panic!("expected a created profile");
    };
    assert_eq!(inserted_evidence, 1);
    assert_eq!(rejected_claims, 3, "three commitments without two titles");
    assert_eq!(profile.profile.version, 1);
    assert_eq!(profile.profile.summary, "Taste evidence is still limited.");
    assert!(profile.profile.profile_id.starts_with("v1:"));
    let requests = h.app.ai.requests();
    assert_eq!(requests.len(), 2);
    let critic = common::prompt_text(&requests[1].1);
    assert!(critic.contains("DRAFT TO AUDIT:"));
    assert!(critic.contains("\"observed_at\": \"2027-01-15\""));

    let unchanged = run_taste_reflection(&h.services.store, &h.services.ai, input())
        .await
        .expect("reflection");
    assert!(matches!(
        unchanged,
        TasteReflectionResult::Unchanged {
            inserted_evidence: 0,
            ..
        }
    ));
    assert_eq!(h.app.ai.requests().len(), 2, "no model call when unchanged");
}
