//! Podcast taste reflection: append current
//! observations, checkpoint on their fingerprint (no model call when it is
//! unchanged), then a draft and a skeptical revision. Claims citing missing
//! or inadequate evidence are removed before persistence.

use std::collections::{HashMap, HashSet};

use omni_ai::{AiError, ModelRole};
use omni_core::clock::utc_date_stamp;
use omni_store::cbor::Extra;
use omni_store::{Store, StoreError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::evidence::fingerprint_evidence;
use super::stats::compute_podcast_behavioral_stats;
use super::store::{
    get_all_podcast_taste_evidence, get_latest_podcast_taste_profile, insert_podcast_taste_profile,
};
use super::types::{
    PodcastBehavioralStats, PodcastTasteClaim, PodcastTasteEvidenceData, PodcastTasteEvidenceKind,
    PodcastTasteProfileContent, PodcastTasteProfileData,
};
use crate::account::ListenedEpisode;
use crate::models::{Models, Refine, in_range};
use crate::persistence::{PodcastFeedback, PodcastRecommendationData, PodcastRecommendationStatus};
use crate::sync::capture_taste_evidence;

pub const PODCAST_TASTE_PROMPT_VERSION: &str = "podcast-taste-reflection-v1";
pub const DEFAULT_MAX_EVIDENCE: usize = 160;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawClaim {
    // A length bound here would fail the whole response at schema validation,
    // so the limit is described and enforced per claim.
    #[schemars(description = "One concise sentence of at most 240 characters.")]
    pub claim: String,
    #[schemars(range(min = 0, max = 1))]
    pub confidence: f64,
    #[schemars(length(max = 12))]
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawProfile {
    #[schemars(length(max = 10))]
    pub stable_preferences: Vec<RawClaim>,
    #[schemars(length(max = 10))]
    pub conditional_preferences: Vec<RawClaim>,
    #[schemars(length(max = 10))]
    pub aversions: Vec<RawClaim>,
    #[schemars(length(max = 8))]
    pub current_saturation: Vec<RawClaim>,
    #[schemars(length(max = 8))]
    pub exploration_targets: Vec<RawClaim>,
    #[schemars(length(max = 8))]
    pub uncertainties: Vec<RawClaim>,
}

impl Refine for RawProfile {
    fn refine(&self) -> Result<(), String> {
        let lists: [(&[RawClaim], usize, &str); 6] = [
            (&self.stable_preferences, 10, "stable_preferences"),
            (&self.conditional_preferences, 10, "conditional_preferences"),
            (&self.aversions, 10, "aversions"),
            (&self.current_saturation, 8, "current_saturation"),
            (&self.exploration_targets, 8, "exploration_targets"),
            (&self.uncertainties, 8, "uncertainties"),
        ];
        for (claims, max, name) in lists {
            if claims.len() > max {
                return Err(format!("{name} has more than {max} claims"));
            }
            for claim in claims {
                in_range(claim.confidence, 0.0, 1.0, "confidence")?;
                if claim.evidence_ids.len() > 12 {
                    return Err(format!("{name} claim cites more than 12 evidence ids"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {detail}")]
pub struct PodcastReflectionError {
    pub operation: &'static str,
    pub detail: String,
}

impl PodcastReflectionError {
    fn store(operation: &'static str) -> impl FnOnce(StoreError) -> Self {
        move |e| Self {
            operation,
            detail: e.to_string(),
        }
    }

    fn model(operation: &'static str) -> impl FnOnce(AiError) -> Self {
        move |e| Self {
            operation,
            detail: e.to_string(),
        }
    }
}

pub struct PodcastTasteReflectionInput {
    pub listened: Vec<ListenedEpisode>,
    pub recommendations: Vec<PodcastRecommendationData>,
    pub now: i64,
    /// Hard prompt bound after deterministic prioritization.
    pub max_evidence: usize,
}

#[derive(Debug)]
pub enum PodcastTasteReflectionResult {
    Unchanged {
        profile: Box<PodcastTasteProfileData>,
        inserted_evidence: usize,
    },
    Created {
        profile: Box<PodcastTasteProfileData>,
        inserted_evidence: usize,
        rejected_claims: usize,
    },
    InsufficientEvidence {
        inserted_evidence: usize,
    },
}

pub async fn run_podcast_taste_reflection(
    store: &Store,
    models: &Models,
    input: PodcastTasteReflectionInput,
) -> Result<PodcastTasteReflectionResult, PodcastReflectionError> {
    let inserted_evidence = capture_taste_evidence(store, &input.listened, &input.recommendations)
        .await
        .map_err(PodcastReflectionError::store(
            "insert podcast taste evidence",
        ))?;
    let all_evidence = get_all_podcast_taste_evidence(store)
        .await
        .map_err(PodcastReflectionError::store("read podcast taste evidence"))?;
    if all_evidence.is_empty() {
        return Ok(PodcastTasteReflectionResult::InsufficientEvidence { inserted_evidence });
    }
    let evidence_fingerprint =
        fingerprint_evidence(&all_evidence).map_err(|e| PodcastReflectionError {
            operation: "fingerprint podcast taste evidence",
            detail: e.to_string(),
        })?;
    let latest = get_latest_podcast_taste_profile(store)
        .await
        .map_err(PodcastReflectionError::store("read podcast taste profile"))?;
    if let Some(latest) = latest.as_ref()
        && latest.evidence_fingerprint == evidence_fingerprint
    {
        return Ok(PodcastTasteReflectionResult::Unchanged {
            profile: Box::new(latest.clone()),
            inserted_evidence,
        });
    }

    let bounded = select_podcast_reflection_evidence(&all_evidence, input.max_evidence);
    let stats = compute_podcast_behavioral_stats(&all_evidence);
    let evidence_json = omni_core::js::json_stringify_pretty2(&Value::Array(
        bounded.iter().map(compact_evidence).collect(),
    ));
    let stats_json = format_stats_json(&stats);

    let draft = models
        .object::<RawProfile>(
            ModelRole::PodcastTasteReflection,
            "taste-reflection",
            build_draft_prompt(&evidence_json, &stats_json),
        )
        .await
        .map_err(PodcastReflectionError::model("draft podcast taste profile"))?
        .output;
    let draft_json = serde_json::to_value(&draft)
        .map(|v| omni_core::js::json_stringify_pretty2(&v))
        .unwrap_or_default();
    let revised = models
        .object::<RawProfile>(
            ModelRole::PodcastTasteReflection,
            "taste-reflection",
            build_critic_prompt(&evidence_json, &stats_json, &draft_json),
        )
        .await
        .map_err(PodcastReflectionError::model(
            "revise podcast taste profile",
        ))?
        .output;

    let (content, rejected_claims) = validate_podcast_profile(&revised, &all_evidence);
    let version = latest.as_ref().map_or(0, |p| p.version) + 1;
    let profile = PodcastTasteProfileData {
        summary: content.summary,
        stable_preferences: content.stable_preferences,
        conditional_preferences: content.conditional_preferences,
        aversions: content.aversions,
        current_saturation: content.current_saturation,
        exploration_targets: content.exploration_targets,
        uncertainties: content.uncertainties,
        profile_id: format!("v{version}:{evidence_fingerprint}"),
        version,
        generated_at: input.now,
        evidence_fingerprint,
        evidence_count: all_evidence.len() as u64,
        model_id: models.model_id(ModelRole::PodcastTasteReflection),
        prompt_version: PODCAST_TASTE_PROMPT_VERSION.to_owned(),
        stats,
        extra: Extra::default(),
    };
    insert_podcast_taste_profile(store, profile.clone())
        .await
        .map_err(PodcastReflectionError::store(
            "insert podcast taste profile",
        ))?;
    Ok(PodcastTasteReflectionResult::Created {
        profile: Box::new(profile),
        inserted_evidence,
        rejected_claims,
    })
}

fn is_resolved_outcome(status: Option<PodcastRecommendationStatus>) -> bool {
    matches!(
        status,
        Some(
            PodcastRecommendationStatus::Listened
                | PodcastRecommendationStatus::Abandoned
                | PodcastRecommendationStatus::Ignored
        )
    )
}

/// Explicit feedback, then delivered outcomes, then listens; newest first; id.
pub fn select_podcast_reflection_evidence(
    evidence: &[PodcastTasteEvidenceData],
    limit: usize,
) -> Vec<PodcastTasteEvidenceData> {
    if limit == 0 {
        return Vec::new();
    }
    let weight = |item: &PodcastTasteEvidenceData| {
        if item.kind == PodcastTasteEvidenceKind::ExplicitFeedback {
            4
        } else if is_resolved_outcome(item.recommendation_status) {
            3
        } else if item.kind == PodcastTasteEvidenceKind::Listen {
            2
        } else {
            1
        }
    };
    let mut sorted: Vec<PodcastTasteEvidenceData> = evidence.to_vec();
    sorted.sort_by(|a, b| {
        weight(b)
            .cmp(&weight(a))
            .then(b.observed_at.cmp(&a.observed_at))
            .then_with(|| omni_core::js::locale_compare(&a.evidence_id, &b.evidence_id))
    });
    sorted.truncate(limit);
    sorted
}

/// Finished (>=80%) or completion-less listens, starred listens, explicit
/// feedback (or a note), and resolved outcomes support claims.
fn is_taste_bearing(item: &PodcastTasteEvidenceData) -> bool {
    match item.kind {
        PodcastTasteEvidenceKind::ExplicitFeedback => {
            item.feedback.is_some() || item.note.as_deref().is_some_and(|n| !n.is_empty())
        }
        PodcastTasteEvidenceKind::RecommendationOutcome => {
            is_resolved_outcome(item.recommendation_status)
        }
        PodcastTasteEvidenceKind::Listen => {
            item.starred == Some(true) || item.completion.is_none_or(|c| c >= 0.8)
        }
    }
}

/// Models sometimes overrun the described length; such a claim is rejected
/// on its own rather than failing the whole reflection.
fn claim_length_ok(claim: &str) -> bool {
    (1..=240).contains(&omni_core::js::utf16_len(claim))
}

pub fn validate_podcast_profile(
    raw: &RawProfile,
    evidence: &[PodcastTasteEvidenceData],
) -> (PodcastTasteProfileContent, usize) {
    let by_id: HashMap<&str, &PodcastTasteEvidenceData> = evidence
        .iter()
        .map(|e| (e.evidence_id.as_str(), e))
        .collect();
    let mut rejected = 0usize;
    let mut validate =
        |claims: &[RawClaim], minimum_shows: usize, allow_single_explicit_aversion: bool| {
            let mut kept = Vec::new();
            for claim in claims {
                if !claim_length_ok(&claim.claim) {
                    rejected += 1;
                    continue;
                }
                let mut seen = HashSet::new();
                let evidence_ids: Vec<String> = claim
                    .evidence_ids
                    .iter()
                    .filter(|id| seen.insert(id.as_str()))
                    .filter(|id| {
                        by_id
                            .get(id.as_str())
                            .is_some_and(|item| is_taste_bearing(item))
                    })
                    .cloned()
                    .collect();
                let has_explicit_negative = evidence_ids.iter().any(|id| {
                    by_id
                        .get(id.as_str())
                        .is_some_and(|i| i.feedback == Some(PodcastFeedback::NotForMe))
                });
                let independent_shows = evidence_ids
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()))
                    .map(|i| i.show_key.as_str())
                    .filter(|k| !k.is_empty())
                    .collect::<HashSet<_>>()
                    .len();
                let enough = independent_shows >= minimum_shows
                    || (allow_single_explicit_aversion && has_explicit_negative);
                if !enough {
                    rejected += 1;
                    continue;
                }
                kept.push(PodcastTasteClaim {
                    claim: claim.claim.clone(),
                    confidence: claim.confidence,
                    evidence_ids,
                    extra: Extra::default(),
                });
            }
            kept
        };
    let stable_preferences = validate(&raw.stable_preferences, 2, false);
    let conditional_preferences = validate(&raw.conditional_preferences, 2, false);
    let aversions = validate(&raw.aversions, 2, true);
    let current_saturation = validate(&raw.current_saturation, 2, false);
    let exploration_targets = validate(&raw.exploration_targets, 1, false);
    let uncertainties = validate(&raw.uncertainties, 1, false);

    let mut summary_parts = vec![if stable_preferences.is_empty() {
        "Podcast taste evidence is still limited.".to_owned()
    } else {
        format!(
            "Evidence-backed preferences: {}.",
            stable_preferences
                .iter()
                .map(|c| c.claim.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    }];
    if !aversions.is_empty() {
        summary_parts.push(format!(
            "Evidence-backed aversions: {}.",
            aversions
                .iter()
                .map(|c| c.claim.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    (
        PodcastTasteProfileContent {
            summary: summary_parts.join(" "),
            stable_preferences,
            conditional_preferences,
            aversions,
            current_saturation,
            exploration_targets,
            uncertainties,
        },
        rejected,
    )
}

/// Prompt digest of the latest profile.
pub fn format_podcast_taste_profile_digest(profile: Option<&PodcastTasteProfileData>) -> String {
    let Some(profile) = profile else {
        return "No reflective podcast taste profile is available yet.".to_owned();
    };
    let claim_lines = |label: &str, claims: &[PodcastTasteClaim]| {
        (!claims.is_empty()).then(|| {
            format!(
                "{label}: {}",
                claims
                    .iter()
                    .map(|c| c.claim.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
    };
    [
        Some(format!(
            "Reflective podcast taste profile v{}: {}",
            profile.version, profile.summary
        )),
        claim_lines("Stable preferences", &profile.stable_preferences),
        claim_lines("Conditional preferences", &profile.conditional_preferences),
        claim_lines("Aversions", &profile.aversions),
        claim_lines("Current saturation", &profile.current_saturation),
        claim_lines("Exploration targets", &profile.exploration_targets),
        claim_lines("Uncertainties", &profile.uncertainties),
    ]
    .into_iter()
    .flatten()
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

fn compact_evidence(item: &PodcastTasteEvidenceData) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::String(item.evidence_id.clone()));
    let kind = serde_json::to_value(item.kind).unwrap_or(Value::Null);
    out.insert("kind".into(), kind);
    out.insert("show".into(), Value::String(item.show_title.clone()));
    if let Some(episode) = &item.episode_title {
        out.insert("episode".into(), Value::String(episode.clone()));
    }
    out.insert(
        "observed_at".into(),
        Value::String(utc_date_stamp(item.observed_at)),
    );
    if let Some(completion) = item.completion {
        out.insert("completion".into(), Value::from(completion));
    }
    if let Some(starred) = item.starred {
        out.insert("starred".into(), Value::Bool(starred));
    }
    if let Some(status) = item.recommendation_status {
        out.insert(
            "recommendation_status".into(),
            Value::String(status.as_str().to_owned()),
        );
    }
    if let Some(feedback) = item.feedback {
        out.insert(
            "feedback".into(),
            serde_json::to_value(feedback).unwrap_or(Value::Null),
        );
    }
    if let Some(note) = &item.note {
        out.insert("note".into(), Value::String(note.clone()));
    }
    if let Some(via) = &item.discovered_via {
        out.insert("discovered_via".into(), Value::String(via.clone()));
    }
    if let Some(voices) = &item.matched_voices {
        out.insert(
            "matched_voices".into(),
            Value::Array(voices.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(duration) = item.duration_minutes {
        out.insert("duration_minutes".into(), Value::from(duration));
    }
    Value::Object(out)
}

fn build_draft_prompt(evidence_json: &str, stats_json: &str) -> String {
    format!(
        "Build a conservative, useful taste profile for one person's podcast-episode recommendation system.

Context: the system recommends episodes of shows the listener does NOT already follow (subscriptions are handled elsewhere). This profile is about what the evidence shows they finish, star, bail on, and explicitly rate.

Rules:
- Infer preferences from demonstrated behavior, not popularity or stereotypes.
- Starred episodes and explicit feedback are strongest. A finished episode is positive-but-ambiguous. A low-completion listen is weak; an abandoned recommendation is a real negative. Ignored is weak evidence. Pending, notified, and failed recommendation rows are operational context only and must not support taste claims.
- Sharp discussion, debate, and drama coverage can be genuine positives for this listener; do not flag them as aversions without explicit negative evidence.
- Separate stable preferences from conditional/contextual ones (mood, episode length, format).
- Preserve some exploration and state uncertainties instead of inventing certainty.
- Every profile field must cite evidence ids from the supplied ledger. Stable, conditional, and saturation claims need at least two independent shows. One explicit not_for_me item may support an aversion. Exploration and uncertainty entries need at least one taste-bearing item.
- Free-form notes on feedback rows are interpretive context to help you understand the structured feedback (or, if there is no good_pick/not_for_me on that row, the only signal). They are not independent evidence: never let a single vivid note carry more weight than one show's worth of evidence toward the independent-show requirement for a claim.
- Do not propose changes to code, prompts, weights, or automation.

DETERMINISTIC STATS:
{stats_json}

EVIDENCE LEDGER:
{evidence_json}"
    )
}

fn build_critic_prompt(evidence_json: &str, stats_json: &str, draft_json: &str) -> String {
    format!(
        "Act as a skeptical second-pass reviewer of a podcast taste profile. Return a corrected final profile.

Remove overfitting, unsupported format/genre claims, use of pending/notified/failed operational rows as taste evidence, and claims whose cited ids do not exist. Reduce confidence when evidence is ambiguous. Every field must cite taste-bearing evidence. Stable, conditional, and saturation claims need two independent shows, while one explicit not_for_me item may support an aversion. Exploration and uncertainty need at least one relevant item. Free-form notes are interpretive context, not independent evidence: flag any claim that leans on a note's wording instead of the independent-show count it actually has.

DETERMINISTIC STATS:
{stats_json}

EVIDENCE LEDGER:
{evidence_json}

DRAFT TO AUDIT:
{draft_json}"
    )
}

/// `JSON.stringify(stats, null, 2)` for the prompts.
fn format_stats_json(stats: &PodcastBehavioralStats) -> String {
    serde_json::to_value(stats)
        .map(|v| omni_core::js::json_stringify_pretty2(&v))
        .unwrap_or_default()
}
