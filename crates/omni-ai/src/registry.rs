//! Per-role cost attribution and the per-call timeout.
//!
//! Code-default models live with the env keys in [`omni_config::ModelRole::default_model`]
//! (deploys use code defaults; AGENTS.md). This module adds what each role records
//! alongside its model: the cost feature and the default operation.

use std::time::Duration;

use omni_config::ModelRole;

/// `LANGUAGE_MODEL_TIMEOUT`: every provider call is bounded by five minutes.
pub const LANGUAGE_MODEL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Every role, in registry order.
pub const ALL_ROLES: [ModelRole; 13] = [
    ModelRole::LivestreamIntelligence,
    ModelRole::Extraction,
    ModelRole::CalendarExtraction,
    ModelRole::Triage,
    ModelRole::ObserverRepair,
    ModelRole::ArrRecovery,
    ModelRole::RecsShortlist,
    ModelRole::RecsSelection,
    ModelRole::TasteReflection,
    ModelRole::PodcastTasteReflection,
    ModelRole::PressPodsMetadata,
    ModelRole::PressPodsCleaning,
    ModelRole::RecipeReview,
];

/// `(feature, default operation)` of each role. Roles whose callers name a specific
/// operation use [`crate::CostTag::with_operation`].
pub fn role_cost(role: ModelRole) -> (&'static str, &'static str) {
    match role {
        // getLivestreamIntelligenceModel(operation) has no default; callers pass one.
        ModelRole::LivestreamIntelligence => ("livestream-intelligence", "generate"),
        ModelRole::Extraction => ("parcel-tracker", "extract-deliveries"),
        ModelRole::CalendarExtraction => ("calendar-events", "extract-events"),
        ModelRole::Triage => ("email-triage", "classify"),
        ModelRole::ObserverRepair => ("observer-repair", "repair-issue"),
        ModelRole::ArrRecovery => ("arr-recovery", "assess-import"),
        ModelRole::RecsShortlist => ("media-recommendations", "shortlist"),
        ModelRole::RecsSelection => ("media-recommendations", "select"),
        ModelRole::TasteReflection => ("media-recommendations", "taste-reflection"),
        ModelRole::PodcastTasteReflection => ("podcast-recommendations", "taste-reflection"),
        ModelRole::PressPodsMetadata => ("press-pods", "rate-retrieval"),
        ModelRole::PressPodsCleaning => ("press-pods", "clean-narration"),
        ModelRole::RecipeReview => ("recipes", "review-extraction"),
    }
}
