//! Builds the process-wide [`AppContext`] from configuration.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_ai::Ai;
use omni_ai::costs::CostRecorder;
use omni_alerts::Pushover;
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, HttpConfig, HttpError, SideEffectMode};
use omni_mailer::Mailer;
use omni_runtime::{AppContext, AppPaths, Ports};
use omni_store::{Store, StoreError, StoreOptions};
use omni_tasks::{EventBus, RunLogs, TaskRegistry};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Where the Docker image keeps the built frontend (`OMNI_WEB_DIST`).
pub const DEFAULT_WEB_DIST: &str = "/app/web";

/// Process-level pieces created before the context (the logging stack needs
/// the bus, run-log buffers and Pushover before the store opens).
#[derive(Clone)]
pub struct Foundation {
    pub config: Arc<Config>,
    pub clock: SharedClock,
    pub http: HttpClient,
    pub pushover: Pushover,
    pub bus: EventBus,
    pub run_logs: RunLogs,
    pub side_effects: SideEffectMode,
}

impl Foundation {
    pub fn new(
        config: Arc<Config>,
        clock: SharedClock,
        side_effects: SideEffectMode,
    ) -> Result<Self, HttpError> {
        let http = HttpClient::new(HttpConfig::default())?;
        Ok(Self::with_http(config, clock, side_effects, http))
    }

    /// Over an explicit HTTP client (tests pass `omni_testkit::no_network`).
    pub fn with_http(
        config: Arc<Config>,
        clock: SharedClock,
        side_effects: SideEffectMode,
        http: HttpClient,
    ) -> Self {
        let pushover = Pushover::new(http.clone(), &config, side_effects);
        let bus = EventBus::default();
        let run_logs = RunLogs::new(bus.clone(), clock.clone());
        Self {
            config,
            clock,
            http,
            pushover,
            bus,
            run_logs,
            side_effects,
        }
    }
}

/// Filesystem layout derived from the configuration.
pub fn app_paths(config: &Config, web_dist: PathBuf) -> AppPaths {
    let db_path = config.db_path();
    let data_dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    AppPaths {
        data_dir,
        assets_dir: PathBuf::from("assets"),
        web_dist,
        reminders_private: if config.dockerized {
            PathBuf::from("/data/reminders-private")
        } else {
            PathBuf::from(".local/reminders-private")
        },
        // Empty: PressPods resolves `PRESSPODS_AUDIO_DIR` or the DB directory itself.
        presspods_audio: PathBuf::new(),
        stylist_private: if config.dockerized {
            PathBuf::from("/data/stylist-private")
        } else {
            PathBuf::from(".local/stylist-private")
        },
    }
}

/// Opens the store at `db_path` and assembles the context.
pub async fn build_context(
    foundation: &Foundation,
    db_path: &Path,
    paths: AppPaths,
) -> Result<AppContext, StoreError> {
    let store = Store::open(db_path, StoreOptions::new(foundation.clock.clone())).await?;
    Ok(context_over(foundation, store, paths))
}

/// The context over an already open store.
pub fn context_over(foundation: &Foundation, store: Store, paths: AppPaths) -> AppContext {
    let config = foundation.config.clone();
    let clock = foundation.clock.clone();
    let tracker = TaskTracker::new();
    let costs = CostRecorder::new(store.clone(), clock.clone());
    let ai = Ai::new(foundation.http.clone(), &config, costs.clone());
    let mailer = omni_mailer::resolve_compose_config(&config)
        .map(|smtp| Mailer::new(smtp, foundation.side_effects));
    let tasks = TaskRegistry::new(
        store.clone(),
        clock.clone(),
        foundation.bus.clone(),
        tracker.clone(),
        foundation.run_logs.clone(),
    );
    AppContext {
        public_http: PublicHttpClient::new(&foundation.http),
        http: foundation.http.clone(),
        pushover: foundation.pushover.clone(),
        mailer,
        ai,
        costs,
        tasks,
        bus: foundation.bus.clone(),
        shutdown: CancellationToken::new(),
        tracker,
        ports: Ports::default(),
        side_effects: foundation.side_effects,
        paths,
        config,
        store,
        clock,
    }
}
