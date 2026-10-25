//! The I/O seams the recommendation workflows run against.

use std::sync::Arc;

use futures::future::BoxFuture;
use omni_ai::Ai;
use omni_alerts::{Pushover, PushoverChannel, PushoverMessage};
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_store::Store;

use crate::arr::inspect::ArrInspect;
use crate::arr::movies::RadarrMovies;
use crate::arr::seasons::SonarrSeasons;
use crate::media_library::MediaLibrary;
use crate::selection::Research;
use crate::tmdb::Catalog;
use crate::watchlist::Watchlist;

/// A recommendation push: title, body and the one-tap rating link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecommendationPush {
    pub title: String,
    pub message: String,
    pub url: String,
    pub url_title: String,
}

/// Recommendation notifications (Pushover `Recs` in production).
pub trait Notifier: Send + Sync {
    fn notify(&self, push: RecommendationPush) -> BoxFuture<'_, Result<(), String>>;
}

/// Sends through the recs Pushover token (falls back to the general token);
/// `SideEffectMode::Record` is honored by [`Pushover`] itself.
pub struct PushoverNotifier(pub Pushover);

impl Notifier for PushoverNotifier {
    fn notify(&self, push: RecommendationPush) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0
                .send(
                    PushoverChannel::Recs,
                    PushoverMessage {
                        message: push.message,
                        title: Some(push.title),
                        url: Some(push.url),
                        url_title: Some(push.url_title),
                        priority: None,
                        sound: None,
                        timestamp: None,
                        ..PushoverMessage::default()
                    },
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// Everything the pipeline, the taste task, the routes and the MCP tools use.
#[derive(Clone)]
pub struct MediaServices {
    pub config: Arc<Config>,
    pub store: Store,
    pub clock: SharedClock,
    pub ai: Ai,
    pub library: Arc<dyn MediaLibrary>,
    pub watchlist: Arc<dyn Watchlist>,
    /// Season-level Sonarr management for the series MCP tools.
    pub seasons: SonarrSeasons,
    /// Movie-level Radarr management for the movie MCP tools.
    pub movies: RadarrMovies,
    /// Read-only profile, status and queue views of both services.
    pub arr: ArrInspect,
    pub catalog: Arc<dyn Catalog>,
    pub research: Arc<dyn Research>,
    pub notifier: Arc<dyn Notifier>,
}

impl MediaServices {
    pub fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// `feedbackUrl("recommendations", id)`: the one-tap rating page.
    pub fn feedback_url(&self, recommendation_id: &str) -> String {
        feedback_url(&self.config.recs_public_url, recommendation_id)
    }
}

/// `${RECS_PUBLIC_URL without trailing slash}/feedback/recommendations/<id>`.
pub fn feedback_url(public_url: &str, recommendation_id: &str) -> String {
    format!(
        "{}/feedback/recommendations/{}",
        public_url.strip_suffix('/').unwrap_or(public_url),
        omni_core::js::encode_uri_component(recommendation_id)
    )
}

/// The credential env var each model provider needs (`requiredModelCredentials`).
pub fn provider_credential<'a>(config: &'a Config, model_id: &str) -> (String, Option<&'a str>) {
    let provider = model_id.split(':').next().unwrap_or_default();
    match provider {
        "openai" => (
            "OPENAI_API_KEY".to_owned(),
            config.openai_api_key.as_deref(),
        ),
        "anthropic" => (
            "ANTHROPIC_API_KEY".to_owned(),
            config.anthropic_api_key.as_deref(),
        ),
        "google" => (
            "GOOGLE_GENERATIVE_AI_API_KEY".to_owned(),
            config.google_generative_ai_api_key.as_deref(),
        ),
        other => (format!("{} model credential", other.to_uppercase()), None),
    }
}

/// `true` for a present, non-empty value.
pub fn configured(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}
