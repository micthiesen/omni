//! The `TasteReflection` task.

use std::sync::Mutex;

use futures::future::BoxFuture;
use omni_ai::ModelRole;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use super::reflection::{TasteReflectionInput, TasteReflectionResult, run_taste_reflection};
use crate::error::{IntegrationError, RecommendationError};
use crate::persistence::get_all_recommendations;
use crate::services::{MediaServices, configured, provider_credential};
use crate::types::FetchResult;

pub const TASK_NAME: &str = "TasteReflection";
const LOG: &str = "Main:TasteReflection";

/// Missing settings that disable the task.
pub fn missing_settings(services: &MediaServices) -> Vec<String> {
    let config = &services.config;
    let model_id = config.model(ModelRole::TasteReflection);
    let (_, credential) = provider_credential(config, model_id);
    let provider = model_id.split(':').next().unwrap_or_default();
    let mut missing = Vec::new();
    if !configured(config.tmdb_api_key.as_deref()) {
        missing.push("TMDB_API_KEY".to_owned());
    }
    if !configured(config.plex_url.as_deref()) {
        missing.push("PLEX_URL".to_owned());
    }
    if !configured(config.plex_token.as_deref()) {
        missing.push("PLEX_TOKEN".to_owned());
    }
    if !configured(credential) {
        missing.push(format!("{} model credential", provider.to_uppercase()));
    }
    missing
}

/// Weekly taste reflection; never on startup (LLM-backed).
pub struct MediaTasteReflectionTask {
    services: MediaServices,
    schedule: CronSchedule,
    last_run_summary: Mutex<Option<String>>,
}

impl MediaTasteReflectionTask {
    pub fn create(services: MediaServices, schedule: CronSchedule) -> Option<Self> {
        let missing = missing_settings(&services);
        if !missing.is_empty() {
            tracing::info!(
                target: "Main",
                "Taste reflection disabled: missing {}",
                missing.join(", ")
            );
            return None;
        }
        Some(Self {
            services,
            schedule,
            last_run_summary: Mutex::new(None),
        })
    }

    fn set_summary(&self, summary: String) {
        if let Ok(mut last) = self.last_run_summary.lock() {
            *last = Some(summary);
        }
    }

    async fn run_reflection(&self) -> Result<(), RecommendationError> {
        let services = &self.services;
        let history = match services.library.watch_history().await {
            FetchResult::Ok(history) => history,
            FetchResult::Unavailable { reason } => {
                self.set_summary(format!("skipped: {reason}"));
                tracing::warn!(target: LOG, "Taste reflection skipped: {reason}");
                omni_tasks::report_degraded(format!("watch history unavailable: {reason}"));
                return Ok(());
            }
        };
        let watched = build_canonical_watch_evidence(services, &history).await?;
        let model_id = services.config.model(ModelRole::TasteReflection).to_owned();
        let model = services
            .ai
            .model_for(&services.config, ModelRole::TasteReflection)
            .map_err(|e| IntegrationError::from_error("resolve taste reflection model", &e))?;
        let recommendations = get_all_recommendations(&services.store)
            .await
            .map_err(RecommendationError::persistence("read recommendations"))?;
        let result = run_taste_reflection(
            &services.store,
            &services.ai,
            TasteReflectionInput {
                watched,
                recommendations,
                model: model.as_ref(),
                model_id,
                now: services.now(),
                max_evidence: None,
            },
        )
        .await?;
        let summary = match result {
            TasteReflectionResult::Created {
                profile,
                rejected_claims,
                ..
            } => format!(
                "profile v{}: {} evidence items, {rejected_claims} unsupported claims removed",
                profile.profile.version, profile.profile.evidence_count
            ),
            TasteReflectionResult::Unchanged { profile, .. } => format!(
                "unchanged: profile v{}, no model call",
                profile.profile.version
            ),
            TasteReflectionResult::InsufficientEvidence { .. } => {
                "no completed watch or recommendation evidence".to_owned()
            }
        };
        tracing::info!(target: LOG, "Taste reflection finished: {summary}");
        self.set_summary(summary);
        Ok(())
    }
}

pub use crate::sync::build_canonical_watch_evidence;

impl Task for MediaTasteReflectionTask {
    fn name(&self) -> &str {
        TASK_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Media Taste Reflection")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: std::time::Duration::ZERO,
            run_on_startup: false,
        }
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move { self.run_reflection().await.map_err(TaskError::from_error) })
    }

    fn last_run_summary(&self) -> Option<String> {
        self.last_run_summary.lock().ok().and_then(|s| s.clone())
    }
}
