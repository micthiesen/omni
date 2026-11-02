//! Composition types shared by every subsystem and the binary.
//!
//! Each subsystem crate exposes a constructor returning a [`Subsystem`]; the
//! binary merges routers, registers tasks, MCP tools and entities, runs
//! boot steps in phase order, starts services and wires [`ports::Ports`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_ai::Ai;
use omni_ai::costs::CostRecorder;
use omni_alerts::{AlertGate, Pushover};
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_core::email::EmailHandler;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, SideEffectMode};
use omni_mailer::Mailer;
use omni_mcp_kit::McpTool;
use omni_store::Store;
use omni_store::cbor::JsValue;
use omni_store::entity::EntityDescriptor;
use omni_tasks::{EventBus, Task, TaskRegistry};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub mod chat_record;
pub mod ports;

pub use ports::Ports;

/// Everything a subsystem needs at runtime; cheap to clone.
#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub store: Store,
    pub clock: SharedClock,
    pub http: HttpClient,
    pub public_http: PublicHttpClient,
    pub pushover: Pushover,
    /// `None` when no SMTP configuration resolves.
    pub mailer: Option<Mailer>,
    pub ai: Ai,
    pub costs: CostRecorder,
    pub tasks: TaskRegistry,
    pub bus: EventBus,
    pub shutdown: CancellationToken,
    pub tracker: TaskTracker,
    pub ports: Ports,
    pub side_effects: SideEffectMode,
    pub paths: AppPaths,
}

impl AppContext {
    /// The live run-log buffers shared with the task registry (email activity
    /// log capture attributes lines through these).
    pub fn run_logs(&self) -> omni_tasks::RunLogs {
        self.tasks.run_log_buffers()
    }
}

/// Filesystem locations resolved at boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub web_dist: PathBuf,
    pub reminders_private: PathBuf,
    pub presspods_audio: PathBuf,
    /// Stylist evidence files and archives (mode 0700).
    pub stylist_private: PathBuf,
}

/// What one subsystem contributes to the app.
pub struct Subsystem {
    pub name: &'static str,
    /// Routes at their absolute paths, state already applied.
    pub router: axum::Router,
    pub tasks: Vec<Arc<dyn Task>>,
    pub mcp_tools: Vec<McpTool>,
    /// For `migrate_all` and the compat audit.
    pub entities: Vec<EntityDescriptor>,
    /// Data manager rows.
    pub managed_entities: Vec<ManagedEntity>,
    /// Run in declared phase order before the server starts.
    pub boot_steps: Vec<BootStep>,
    /// Long-lived loops: dispatcher, delivery worker, IMAP actor.
    pub services: Vec<BackgroundService>,
    pub email_handlers: Vec<Arc<dyn EmailHandler>>,
    pub alert_gates: Vec<Arc<dyn AlertGate>>,
}

impl Default for Subsystem {
    fn default() -> Self {
        Self {
            name: "",
            router: axum::Router::new(),
            tasks: Vec::new(),
            mcp_tools: Vec::new(),
            entities: Vec::new(),
            managed_entities: Vec::new(),
            boot_steps: Vec::new(),
            services: Vec::new(),
            email_handlers: Vec::new(),
            alert_gates: Vec::new(),
        }
    }
}

impl Subsystem {
    /// An empty subsystem with a name, filled in with struct update syntax.
    pub fn named(name: &'static str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }
}

/// Refuses deletion of a row with a reason (`canDelete`).
pub type CanDelete = Arc<dyn Fn(&JsValue) -> Option<String> + Send + Sync>;
/// Follow-up after a row is deleted (`afterDelete`).
pub type AfterDelete =
    Arc<dyn Fn(JsValue, Store) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// One entity exposed in the data manager.
#[derive(Clone)]
pub struct ManagedEntity {
    pub slug: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub warning: Option<&'static str>,
    /// The entity behind the rows.
    pub entity: EntityDescriptor,
    /// Primary-key property names, in key order.
    pub primary_key: &'static [&'static str],
    pub can_delete: Option<CanDelete>,
    pub after_delete: Option<AfterDelete>,
}

/// Boot ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BootPhase {
    Migrate,
    Reconcile,
    Services,
    AfterServer,
}

/// A one-shot boot action.
pub struct BootStep {
    pub phase: BootPhase,
    pub name: &'static str,
    pub run: Box<dyn FnOnce(AppContext) -> BoxFuture<'static, Result<(), BootError>> + Send>,
}

#[derive(Debug, thiserror::Error)]
#[error("{step}: {message}")]
pub struct BootError {
    pub step: &'static str,
    pub message: String,
    #[source]
    pub source: Option<omni_core::BoxError>,
}

impl BootError {
    pub fn new(step: &'static str, message: impl Into<String>) -> Self {
        Self {
            step,
            message: message.into(),
            source: None,
        }
    }
}

/// Restart backoff for a service whose `start` future ends (email: 30 s to 300 s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub initial: Duration,
    pub max: Duration,
}

/// A long-lived loop started after boot steps. `start` is `Fn` because a
/// service with a [`RetryPolicy`] is started again when its future ends.
pub struct BackgroundService {
    pub name: &'static str,
    pub start: Box<dyn Fn(AppContext) -> BoxFuture<'static, ()> + Send + Sync>,
    pub retry: Option<RetryPolicy>,
}
