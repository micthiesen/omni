//! The PressPods service: shared state behind routes, the task and MCP tools.

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_ai::costs::CostRecorder;
use omni_alerts::Pushover;
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{SideEffectMode, Url};
use omni_runtime::{AppContext, Ports};
use omni_tasks::{RunNowError, TaskRegistry};

use crate::agents::Agents;
use crate::error::PressPodsError;
use crate::karakeep::{Bookmarker, Karakeep};
use crate::persistence::Persistence;
use crate::retrievers::obscura::{ObscuraRenderer, PageRenderer};
use crate::retrievers::{ArticleRetriever, RetrieverContext, article_retrievers};
use crate::speech::audio_chain::{AudioChain, DENOISE_MODEL_ASSET};
use crate::speech::providers::{ConfiguredTts, TtsFactory};
use crate::speech::stt::{HttpStt, SttClient};
use crate::storage::{AudioStore, resolve_audio_dir};

/// The task name, shared by registration and the worker kick.
pub const TASK_NAME: &str = "PressPods";

/// The image installs Obscura on `PATH` (Dockerfile `obscura` stage).
const OBSCURA_PROGRAM: &str = "obscura";

/// Validates that a submitted URL is public before work is accepted.
pub trait UrlGuard: Send + Sync {
    fn check<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Url, PressPodsError>>;
}

/// The production guard: URL syntax plus the host's current DNS answers.
pub struct DnsUrlGuard;

impl UrlGuard for DnsUrlGuard {
    fn check<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Url, PressPodsError>> {
        Box::pin(async move {
            omni_http::public::assert_public_http_url(url)
                .await
                .map_err(|e| PressPodsError::http("validate public PressPods URL", e))
        })
    }
}

/// Chooses the retrievers for a URL.
pub trait RetrieverSource: Send + Sync {
    fn for_url(&self, url: &str) -> Vec<Arc<dyn ArticleRetriever>>;
}

impl RetrieverSource for Arc<RetrieverContext> {
    fn for_url(&self, url: &str) -> Vec<Arc<dyn ArticleRetriever>> {
        article_retrievers(self, url)
    }
}

/// Starts the worker task now (submissions kick it instead of waiting for the sweep).
pub trait WorkerKick: Send + Sync {
    fn kick(&self) -> Result<(), PressPodsError>;
}

impl WorkerKick for TaskRegistry {
    fn kick(&self) -> Result<(), PressPodsError> {
        match self.run_now(TASK_NAME, None) {
            // Already running (the drain loop picks the job up) or server-only
            // mode (no task registered; the job waits for a worker).
            Ok(_) | Err(RunNowError::AlreadyRunning { .. } | RunNowError::NotFound { .. }) => {
                Ok(())
            }
            Err(error) => Err(PressPodsError::failed(
                "start PressPods worker",
                error.to_string(),
            )),
        }
    }
}

/// Everything PressPods uses; tests replace the external seams.
pub struct PressPodsDeps {
    pub config: Arc<Config>,
    pub clock: SharedClock,
    pub persistence: Persistence,
    pub audio: AudioStore,
    pub agents: Agents,
    pub retrievers: Arc<dyn RetrieverSource>,
    pub tts: Arc<dyn TtsFactory>,
    pub stt: Option<Arc<dyn SttClient>>,
    pub chain: AudioChain,
    pub public_http: PublicHttpClient,
    pub url_guard: Arc<dyn UrlGuard>,
    pub bookmarks: Arc<dyn Bookmarker>,
    pub pushover: Pushover,
    pub costs: CostRecorder,
    pub worker: Arc<dyn WorkerKick>,
    pub intro_path: PathBuf,
    pub logo_path: PathBuf,
    pub mode: SideEffectMode,
}

/// The shared PressPods state; cheap to clone.
#[derive(Clone)]
pub struct PressPods {
    pub(crate) deps: Arc<PressPodsDeps>,
    /// MCP Events publishing (through the `EventPublisher` port).
    pub(crate) ports: Ports,
}

impl PressPods {
    pub fn new(deps: PressPodsDeps) -> Self {
        Self {
            deps: Arc::new(deps),
            ports: Ports::default(),
        }
    }

    /// Publishes `presspods.job_finished` through `ports`' event publisher.
    pub fn with_ports(mut self, ports: Ports) -> Self {
        self.ports = ports;
        self
    }

    /// Production wiring from the app context.
    pub fn from_context(ctx: &AppContext) -> Result<Self, PressPodsError> {
        let config = ctx.config.clone();
        let tz = TimeZone::get(&config.tz).unwrap_or(TimeZone::UTC);
        let audio_dir = if ctx.paths.presspods_audio.as_os_str().is_empty() {
            resolve_audio_dir(&config)
        } else {
            ctx.paths.presspods_audio.clone()
        };
        let ffmpeg = config.ffmpeg_bin().to_owned();
        let chain = AudioChain::new(
            ffmpeg.clone(),
            ffprobe_for(&ffmpeg),
            &ctx.paths.assets_dir.join(DENOISE_MODEL_ASSET),
            std::env::temp_dir(),
        )?;
        let retriever_ctx = Arc::new(RetrieverContext {
            public_http: ctx.public_http.clone(),
            http: ctx.http.clone(),
            jina_api_key: config.jina_api_key.clone(),
            renderer: config
                .presspods_obscura_enabled
                .then(|| Arc::new(ObscuraRenderer::new(OBSCURA_PROGRAM)) as Arc<dyn PageRenderer>),
            costs: ctx.costs.clone(),
            tz: tz.clone(),
        });
        let stt = HttpStt::from_config(ctx.http.clone(), ctx.costs.clone(), &config)
            .map(|stt| Arc::new(stt) as Arc<dyn SttClient>);
        Ok(Self::new(PressPodsDeps {
            persistence: Persistence::new(ctx.store.clone()),
            audio: AudioStore::new(audio_dir),
            agents: Agents::new(ctx.ai.clone(), config.clone(), tz),
            retrievers: Arc::new(retriever_ctx),
            tts: Arc::new(ConfiguredTts {
                http: ctx.http.clone(),
                config: config.clone(),
                mode: ctx.side_effects,
            }),
            stt,
            chain,
            public_http: ctx.public_http.clone(),
            url_guard: Arc::new(DnsUrlGuard),
            bookmarks: Arc::new(Karakeep::new(ctx.http.clone(), &config, ctx.side_effects)),
            pushover: ctx.pushover.clone(),
            costs: ctx.costs.clone(),
            worker: Arc::new(ctx.tasks.clone()),
            intro_path: ctx.paths.assets_dir.join("press-pods/intro.mp3"),
            logo_path: ctx.paths.assets_dir.join("press-pods/logo.jpeg"),
            mode: ctx.side_effects,
            clock: ctx.clock.clone(),
            config,
        })
        .with_ports(ctx.ports.clone()))
    }

    pub fn persistence(&self) -> &Persistence {
        &self.deps.persistence
    }

    pub fn audio(&self) -> &AudioStore {
        &self.deps.audio
    }

    pub fn config(&self) -> &Config {
        &self.deps.config
    }

    pub(crate) fn kick_worker(&self) -> Result<(), PressPodsError> {
        self.deps.worker.kick()
    }
}

/// `ffprobe` next to a configured `ffmpeg` path.
pub fn ffprobe_for(ffmpeg: &str) -> String {
    let path = std::path::Path::new(ffmpeg);
    match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => dir
            .join(name.replacen("ffmpeg", "ffprobe", 1))
            .to_string_lossy()
            .into_owned(),
        _ => "ffprobe".to_owned(),
    }
}

/// Credentials the worker needs beyond the auth token (`requiredModelCredentials`
/// plus the TTS credential), as `(env key, present)`.
pub fn worker_credentials(config: &Config) -> Vec<(&'static str, bool)> {
    let present = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    let mut out = vec![match config.presspods_tts_provider {
        omni_config::TtsProvider::Elevenlabs => {
            ("ELEVENLABS_API_KEY", present(&config.elevenlabs_api_key))
        }
        omni_config::TtsProvider::Higgs => {
            ("PRESSPODS_TTS_URL", present(&config.presspods_tts_url))
        }
    }];
    let providers: Vec<&str> = [
        omni_ai::ModelRole::PressPodsMetadata,
        omni_ai::ModelRole::PressPodsCleaning,
    ]
    .into_iter()
    .map(|role| config.model(role).split(':').next().unwrap_or(""))
    .collect();
    if providers.contains(&"google") {
        out.push((
            "GOOGLE_GENERATIVE_AI_API_KEY",
            present(&config.google_generative_ai_api_key),
        ));
    }
    if providers.contains(&"openai") {
        out.push(("OPENAI_API_KEY", present(&config.openai_api_key)));
    }
    if providers.contains(&"anthropic") {
        out.push(("ANTHROPIC_API_KEY", present(&config.anthropic_api_key)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_follows_a_configured_ffmpeg_path() {
        assert_eq!(ffprobe_for("ffmpeg"), "ffprobe");
        assert_eq!(ffprobe_for("/opt/bin/ffmpeg"), "/opt/bin/ffprobe");
    }

    #[test]
    fn worker_credentials_follow_the_configured_models() {
        let config = Config::from_env(&omni_testkit::test_app_env()).unwrap();
        let creds = worker_credentials(&config);
        assert_eq!(
            creds,
            [("PRESSPODS_TTS_URL", false), ("OPENAI_API_KEY", false)]
        );
    }
}
