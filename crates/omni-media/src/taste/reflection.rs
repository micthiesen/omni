//! Two-pass taste reflection with evidence guardrails.

use std::collections::{HashMap, HashSet};

use omni_ai::{Ai, CostTag, GenerateRequest, LanguageModel, ModelRole};
use omni_api::media::{
    CommitmentAssessment, CommitmentPreference, CommitmentPreferences, MediaType,
    RecommendationFeedback, RecommendationStatus, TasteBehaviorStats, TasteClaim, TasteProfile,
};
use omni_core::clock::utc_date_stamp;
use omni_core::js::{json_stringify_pretty2, locale_compare};
use omni_store::Store;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::evidence::fingerprint_evidence;
use super::persistence::{get_all_taste_evidence, get_latest_taste_profile, insert_taste_profile};
use super::stats::compute_behavioral_stats;
use super::types::{
    CanonicalWatchObservation, TasteEvidenceData, TasteEvidenceKind, TasteProfileData,
};
use crate::error::{IntegrationError, RecommendationError};
use crate::persistence::RecommendationData;
use crate::sync::capture_taste_evidence;

pub const TASTE_PROMPT_VERSION: &str = "taste-reflection-v1";
const DEFAULT_MAX_EVIDENCE: usize = 160;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawClaim {
    #[schemars(length(min = 1, max = 240))]
    pub claim: String,
    #[schemars(range(min = 0, max = 1))]
    pub confidence: f64,
    #[schemars(length(max = 12))]
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawCommitment {
    pub preference: CommitmentPreferenceSchema,
    #[schemars(range(min = 0, max = 1))]
    pub confidence: f64,
    #[schemars(length(max = 12))]
    pub evidence_ids: Vec<String>,
}

/// The schema-facing copy of [`CommitmentPreference`] (omni-api stays serde-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CommitmentPreferenceSchema {
    Positive,
    Neutral,
    Negative,
    Uncertain,
}

impl From<CommitmentPreferenceSchema> for CommitmentPreference {
    fn from(value: CommitmentPreferenceSchema) -> Self {
        match value {
            CommitmentPreferenceSchema::Positive => CommitmentPreference::Positive,
            CommitmentPreferenceSchema::Neutral => CommitmentPreference::Neutral,
            CommitmentPreferenceSchema::Negative => CommitmentPreference::Negative,
            CommitmentPreferenceSchema::Uncertain => CommitmentPreference::Uncertain,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawCommitmentPreferences {
    pub movies: RawCommitment,
    pub limited_series: RawCommitment,
    pub long_series: RawCommitment,
}

/// The model's profile (`profileSchema`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    pub commitment_preferences: RawCommitmentPreferences,
}

impl RawProfile {
    /// Bounds beyond the JSON schema, checked on parse.
    fn validate(&self) -> Result<(), String> {
        let lists: [(&str, &Vec<RawClaim>, usize); 6] = [
            ("stable_preferences", &self.stable_preferences, 10),
            ("conditional_preferences", &self.conditional_preferences, 10),
            ("aversions", &self.aversions, 10),
            ("current_saturation", &self.current_saturation, 8),
            ("exploration_targets", &self.exploration_targets, 8),
            ("uncertainties", &self.uncertainties, 8),
        ];
        for (name, claims, max) in lists {
            if claims.len() > max {
                return Err(format!(
                    "No object generated: {name} has more than {max} items"
                ));
            }
            for claim in claims {
                let length = omni_core::js::utf16_len(&claim.claim);
                if !(1..=240).contains(&length)
                    || !(0.0..=1.0).contains(&claim.confidence)
                    || claim.evidence_ids.len() > 12
                {
                    return Err(format!("No object generated: invalid claim in {name}"));
                }
            }
        }
        let commitments = &self.commitment_preferences;
        for commitment in [
            &commitments.movies,
            &commitments.limited_series,
            &commitments.long_series,
        ] {
            if !(0.0..=1.0).contains(&commitment.confidence) || commitment.evidence_ids.len() > 12 {
                return Err("No object generated: invalid commitment assessment".to_owned());
            }
        }
        Ok(())
    }
}

/// The validated, evidence-backed part of a profile.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProfile {
    pub summary: String,
    pub stable_preferences: Vec<TasteClaim>,
    pub conditional_preferences: Vec<TasteClaim>,
    pub aversions: Vec<TasteClaim>,
    pub current_saturation: Vec<TasteClaim>,
    pub exploration_targets: Vec<TasteClaim>,
    pub uncertainties: Vec<TasteClaim>,
    pub commitment_preferences: CommitmentPreferences,
}

pub struct TasteReflectionInput<'a> {
    pub watched: Vec<CanonicalWatchObservation>,
    pub recommendations: Vec<RecommendationData>,
    pub model: &'a dyn LanguageModel,
    pub model_id: String,
    pub now: i64,
    /// Hard prompt bound after deterministic prioritization.
    pub max_evidence: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TasteReflectionResult {
    Unchanged {
        profile: Box<TasteProfileData>,
        inserted_evidence: u64,
    },
    Created {
        profile: Box<TasteProfileData>,
        inserted_evidence: u64,
        rejected_claims: u64,
    },
    InsufficientEvidence {
        inserted_evidence: u64,
    },
}

/// Appends current observations, checkpoints on their fingerprint, then
/// runs a draft and a skeptical revision; claims citing missing or
/// inadequate evidence are removed before persistence.
pub async fn run_taste_reflection(
    store: &Store,
    ai: &Ai,
    input: TasteReflectionInput<'_>,
) -> Result<TasteReflectionResult, RecommendationError> {
    let inserted_evidence =
        capture_taste_evidence(store, &input.watched, &input.recommendations).await?;
    let all = get_all_taste_evidence(store)
        .await
        .map_err(RecommendationError::persistence("read taste evidence"))?;
    if all.is_empty() {
        return Ok(TasteReflectionResult::InsufficientEvidence { inserted_evidence });
    }
    let fingerprint = fingerprint_evidence(&all);
    let latest =
        get_latest_taste_profile(store)
            .await
            .map_err(RecommendationError::persistence(
                "read latest taste profile",
            ))?;
    if let Some(latest) = latest
        .as_ref()
        .filter(|p| p.profile.evidence_fingerprint == fingerprint)
    {
        return Ok(TasteReflectionResult::Unchanged {
            profile: Box::new(latest.clone()),
            inserted_evidence,
        });
    }

    let bounded =
        select_reflection_evidence(&all, input.max_evidence.unwrap_or(DEFAULT_MAX_EVIDENCE));
    let stats = compute_behavioral_stats(&all);
    let evidence_json = json_stringify_pretty2(&Value::Array(
        bounded.iter().map(compact_evidence).collect(),
    ));
    let stats_json = serde_json::to_value(&stats)
        .map(|v| json_stringify_pretty2(&v))
        .unwrap_or_default();

    let draft = generate_profile(
        ai,
        input.model,
        build_draft_prompt(&evidence_json, &stats_json),
        "generate taste reflection draft",
    )
    .await?;
    let draft_json = serde_json::to_value(&draft)
        .map(|v| json_stringify_pretty2(&v))
        .unwrap_or_default();
    let revised = generate_profile(
        ai,
        input.model,
        build_critic_prompt(&evidence_json, &stats_json, &draft_json),
        "generate taste reflection revision",
    )
    .await?;

    let (validated, rejected_claims) = validate_profile(&revised, &all);
    let version = latest.as_ref().map_or(0, |p| p.profile.version) + 1;
    let profile = TasteProfileData {
        profile: TasteProfile {
            summary: validated.summary,
            stable_preferences: validated.stable_preferences,
            conditional_preferences: validated.conditional_preferences,
            aversions: validated.aversions,
            current_saturation: validated.current_saturation,
            exploration_targets: validated.exploration_targets,
            uncertainties: validated.uncertainties,
            commitment_preferences: validated.commitment_preferences,
            profile_id: format!("v{version}:{fingerprint}"),
            version,
            generated_at: input.now,
            evidence_fingerprint: fingerprint,
            evidence_count: all.len() as u64,
            model_id: input.model_id,
            prompt_version: TASTE_PROMPT_VERSION.to_owned(),
            stats,
        },
        extra: Default::default(),
    };
    insert_taste_profile(store, profile.clone())
        .await
        .map_err(RecommendationError::persistence("insert taste profile"))?;
    Ok(TasteReflectionResult::Created {
        profile: Box::new(profile),
        inserted_evidence,
        rejected_claims,
    })
}

async fn generate_profile(
    ai: &Ai,
    model: &dyn LanguageModel,
    prompt: String,
    operation: &'static str,
) -> Result<RawProfile, RecommendationError> {
    let (profile, _usage) = ai
        .generate_object::<RawProfile>(
            model,
            GenerateRequest::prompt(prompt),
            CostTag::for_role(ModelRole::TasteReflection),
        )
        .await
        .map_err(|e| IntegrationError::from_error(operation, &e))?;
    profile
        .validate()
        .map_err(|e| IntegrationError::new(operation, e))?;
    Ok(profile)
}

fn weight(item: &TasteEvidenceData) -> u8 {
    if item.kind == TasteEvidenceKind::ExplicitFeedback {
        return 4;
    }
    if matches!(
        item.recommendation_status,
        Some(
            RecommendationStatus::Watched
                | RecommendationStatus::Abandoned
                | RecommendationStatus::Ignored
        )
    ) {
        return 3;
    }
    if item.kind == TasteEvidenceKind::PlexWatch {
        return 2;
    }
    1
}

/// Direct feedback first, then recommendation outcomes, then recent watches.
pub fn select_reflection_evidence(
    evidence: &[TasteEvidenceData],
    limit: usize,
) -> Vec<TasteEvidenceData> {
    let mut sorted: Vec<TasteEvidenceData> = evidence.to_vec();
    sorted.sort_by(|a, b| {
        weight(b)
            .cmp(&weight(a))
            .then(b.observed_at.cmp(&a.observed_at))
            .then_with(|| locale_compare(&a.evidence_id, &b.evidence_id))
    });
    sorted.truncate(limit);
    sorted
}

fn is_taste_bearing(item: &TasteEvidenceData) -> bool {
    match item.kind {
        // A note-only row reaches reflection for its context, but it is bound
        // to one title, so it cannot alone satisfy a two-title threshold.
        TasteEvidenceKind::ExplicitFeedback => {
            matches!(
                item.feedback,
                Some(RecommendationFeedback::GoodPick | RecommendationFeedback::NotForMe)
            ) || item.note.as_deref().is_some_and(|n| !n.is_empty())
        }
        TasteEvidenceKind::RecommendationOutcome => matches!(
            item.recommendation_status,
            Some(
                RecommendationStatus::Watched
                    | RecommendationStatus::Abandoned
                    | RecommendationStatus::Ignored
            )
        ),
        TasteEvidenceKind::PlexWatch => match item.completion {
            Some(completion) => completion >= 0.8,
            None => item.media_type == MediaType::Movie && item.view_count.unwrap_or(0) >= 1,
        },
    }
}

/// Keeps only claims whose cited evidence exists, bears on taste and spans
/// enough independent titles. Returns the profile and the rejected count.
pub fn validate_profile(
    raw: &RawProfile,
    evidence: &[TasteEvidenceData],
) -> (ValidatedProfile, u64) {
    let by_id: HashMap<&str, &TasteEvidenceData> = evidence
        .iter()
        .map(|item| (item.evidence_id.as_str(), item))
        .collect();
    let mut rejected = 0u64;

    let supported_ids = |ids: &[String]| -> Vec<String> {
        let mut seen = HashSet::new();
        ids.iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter(|id| {
                by_id
                    .get(id.as_str())
                    .is_some_and(|item| is_taste_bearing(item))
            })
            .cloned()
            .collect()
    };
    let independent_titles = |ids: &[String]| -> usize {
        ids.iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .map(|item| item.canonical_id.as_str())
            .filter(|c| !c.is_empty())
            .collect::<HashSet<_>>()
            .len()
    };

    let mut validate_claims = |claims: &[RawClaim], minimum: usize, single_negative_ok: bool| {
        claims
            .iter()
            .filter_map(|claim| {
                let ids = supported_ids(&claim.evidence_ids);
                let explicit_negative = ids.iter().any(|id| {
                    by_id
                        .get(id.as_str())
                        .is_some_and(|item| item.feedback == Some(RecommendationFeedback::NotForMe))
                });
                if independent_titles(&ids) >= minimum || (single_negative_ok && explicit_negative)
                {
                    Some(TasteClaim {
                        claim: claim.claim.clone(),
                        confidence: claim.confidence,
                        evidence_ids: ids,
                    })
                } else {
                    rejected += 1;
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let stable_preferences = validate_claims(&raw.stable_preferences, 2, false);
    let conditional_preferences = validate_claims(&raw.conditional_preferences, 2, false);
    let aversions = validate_claims(&raw.aversions, 2, true);
    let current_saturation = validate_claims(&raw.current_saturation, 2, false);
    let exploration_targets = validate_claims(&raw.exploration_targets, 1, false);
    let uncertainties = validate_claims(&raw.uncertainties, 1, false);

    let mut validate_commitment = |assessment: &RawCommitment| {
        let ids = supported_ids(&assessment.evidence_ids);
        if independent_titles(&ids) < 2 {
            rejected += 1;
            return CommitmentAssessment {
                preference: CommitmentPreference::Uncertain,
                confidence: 0.0,
                evidence_ids: Vec::new(),
            };
        }
        CommitmentAssessment {
            preference: assessment.preference.into(),
            confidence: assessment.confidence,
            evidence_ids: ids,
        }
    };
    let commitment_preferences = CommitmentPreferences {
        movies: validate_commitment(&raw.commitment_preferences.movies),
        limited_series: validate_commitment(&raw.commitment_preferences.limited_series),
        long_series: validate_commitment(&raw.commitment_preferences.long_series),
    };

    let join_claims = |claims: &[TasteClaim]| {
        claims
            .iter()
            .map(|c| c.claim.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut summary = vec![if stable_preferences.is_empty() {
        "Taste evidence is still limited.".to_owned()
    } else {
        format!(
            "Evidence-backed preferences: {}.",
            join_claims(&stable_preferences)
        )
    }];
    if !aversions.is_empty() {
        summary.push(format!(
            "Evidence-backed aversions: {}.",
            join_claims(&aversions)
        ));
    }

    (
        ValidatedProfile {
            summary: summary.join(" "),
            stable_preferences,
            conditional_preferences,
            aversions,
            current_saturation,
            exploration_targets,
            uncertainties,
            commitment_preferences,
        },
        rejected,
    )
}

/// The profile digest injected into recommendation prompts.
pub fn format_taste_profile_digest(profile: Option<&TasteProfile>) -> String {
    let Some(profile) = profile else {
        return "No reflective taste profile is available yet.".to_owned();
    };
    let claim_line = |label: &str, claims: &[TasteClaim]| {
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
    let commitments = &profile.commitment_preferences;
    [
        Some(format!(
            "Reflective taste profile v{}: {}",
            profile.version, profile.summary
        )),
        claim_line("Stable preferences", &profile.stable_preferences),
        claim_line("Conditional preferences", &profile.conditional_preferences),
        claim_line("Aversions", &profile.aversions),
        claim_line("Current saturation", &profile.current_saturation),
        claim_line("Exploration targets", &profile.exploration_targets),
        claim_line("Uncertainties", &profile.uncertainties),
        Some(format!(
            "Commitment fit: movies={}, limited-series={}, long-series={}",
            commitments.movies.preference.as_str(),
            commitments.limited_series.preference.as_str(),
            commitments.long_series.preference.as_str()
        )),
    ]
    .into_iter()
    .flatten()
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

fn compact_evidence(item: &TasteEvidenceData) -> Value {
    let mut map = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            map.insert(key.to_owned(), value);
        }
    };
    let json = |v: &Vec<String>| serde_json::to_value(v).unwrap_or(Value::Null);
    put("id", Some(Value::String(item.evidence_id.clone())));
    put("kind", Some(Value::String(item.kind.as_str().to_owned())));
    put("title", Some(Value::String(item.title.clone())));
    put("year", item.year.map(Value::from));
    put(
        "media_type",
        Some(Value::String(item.media_type.as_str().to_owned())),
    );
    put(
        "observed_at",
        Some(Value::String(utc_date_stamp(item.observed_at))),
    );
    put("view_count", item.view_count.map(Value::from));
    put("completion", item.completion.map(Value::from));
    put(
        "recommendation_status",
        item.recommendation_status
            .map(|s| Value::String(s.as_str().to_owned())),
    );
    put(
        "feedback",
        item.feedback.map(|f| Value::String(f.as_str().to_owned())),
    );
    put("note", item.note.clone().map(Value::String));
    put("source", item.source.clone().map(Value::String));
    put("genres", item.genres.as_ref().map(json));
    put("runtime_minutes", item.runtime_minutes.map(Value::from));
    put("season_count", item.season_count.map(Value::from));
    put("episode_count", item.episode_count.map(Value::from));
    put(
        "series_status",
        item.series_status.clone().map(Value::String),
    );
    put(
        "original_language",
        item.original_language.clone().map(Value::String),
    );
    put("origin_countries", item.origin_countries.as_ref().map(json));
    put("creators", item.creators.as_ref().map(json));
    put("cast", item.cast.as_ref().map(json));
    put("keywords", item.keywords.as_ref().map(json));
    put(
        "certification",
        item.certification.clone().map(Value::String),
    );
    Value::Object(map)
}

fn build_draft_prompt(evidence_json: &str, stats_json: &str) -> String {
    format!(
        "Build a conservative, useful taste profile for one person's movie and TV recommendation system.

Rules:
- Infer preferences from demonstrated behavior, not popularity or stereotypes.
- Rewatches and explicit feedback are strongest. A completed watch is positive-but-ambiguous. Ignored is weak evidence. \"already_watched\" is not negative taste evidence. Pending, notified, and failed recommendation rows are operational context only and must not support taste claims.
- Separate stable preferences from conditional/contextual ones.
- Preserve some exploration and state uncertainties instead of inventing certainty.
- Every profile field must cite evidence ids from the supplied ledger, including saturation, exploration, uncertainty, and commitment assessments. Stable, conditional, saturation, and commitment claims need at least two independent titles. One explicit not_for_me item may support an aversion. Exploration and uncertainty entries need at least one taste-bearing item.
- Free-form notes on feedback rows are interpretive context to help you understand the structured feedback (or, if there is no good_pick/not_for_me on that row, the only signal). They are not independent evidence: never let a single vivid note carry more weight than one title's worth of evidence toward the independent-title requirement for a claim.
- Do not propose changes to code, prompts, weights, or automation.

DETERMINISTIC STATS:
{stats_json}

EVIDENCE LEDGER:
{evidence_json}"
    )
}

fn build_critic_prompt(evidence_json: &str, stats_json: &str, draft_json: &str) -> String {
    format!(
        "Act as a skeptical second-pass reviewer of a movie/TV taste profile. Return a corrected final profile.

Remove overfitting, unsupported genre claims, accidental treatment of \"already_watched\" as dislike, use of pending/notified/failed operational rows as taste evidence, and claims whose cited ids do not exist. Reduce confidence when evidence is ambiguous. Every field must cite taste-bearing evidence, including saturation, exploration, uncertainty, and commitment assessments. Stable, conditional, saturation, and commitment claims need two independent titles, while one explicit not_for_me item may support an aversion. Exploration and uncertainty need at least one relevant item. Free-form notes are interpretive context, not independent evidence: flag any claim that leans on a note's wording instead of the independent-title count it actually has.

DETERMINISTIC STATS:
{stats_json}

EVIDENCE LEDGER:
{evidence_json}

DRAFT TO AUDIT:
{draft_json}"
    )
}

/// Stats are serialized for prompts only; exposed for tests.
pub fn stats_json(stats: &TasteBehaviorStats) -> String {
    serde_json::to_value(stats)
        .map(|v| json_stringify_pretty2(&v))
        .unwrap_or_default()
}
