//! MCP gateway, MCP Events and MCP activity.
//!
//! - [`gateway`]: `/gateway/mcp`, Omni's only MCP endpoint, serving every
//!   package's tools with the configured upstream integrations'
//!   (`docs/gateway.md`);
//! - [`rpc`]: the protocol layer it serves in both protocol eras, plus `events/*`;
//! - [`events`]: the shared durable outbox, webhook delivery and the Claude
//!   session and task run watchers;
//! - [`activity`] / [`activity_routes`]: MCP call recording and the
//!   `/api/mcp/activity` and `/api/claude/*` routes;
//! - [`tools`]: the `system`, `events` and `claude-sessions` tool groups;
//! - [`policy`]: the `docs/mcp-policy.json` inventory from registered tools.
//!
//! # Wiring
//!
//! 1. Build `omni_device_link::DeviceLink::from_context` first (it sets the
//!    [`host::ClaudeHost`] port when `OMNI_DEVICE_LINK_TOKEN` is set), set the
//!    `ArchiveEcho` port (`omni_imap::transport::StoreArchiveEcho`), then build
//!    [`McpPackage::new`].
//! 2. Register [`McpPackage::email_handler`] first on the email dispatcher,
//!    and set the `EventPublisher` port from [`McpPackage::event_publisher`].
//! 3. Collect every other subsystem's `mcp_tools`, append
//!    [`McpPackage::tools`], and call [`McpPackage::subsystem`]; it serves
//!    them in [`tools::TOOL_ORDER`] and fails when the set differs from that
//!    list. The returned subsystem carries the routes, tasks, entities,
//!    boot steps and delivery worker; its `mcp_tools` is empty because the
//!    gateway already serves every tool.

pub mod activity;
pub mod activity_routes;
pub mod events;
pub mod gateway;
pub mod host;
pub mod json;
pub mod policy;
pub mod rpc;
pub mod tasks;
pub mod tools;

use std::sync::Arc;

use futures::FutureExt as _;
use omni_core::email::EmailHandler;
use omni_mcp_kit::{McpTool, ToolMetaError};
use omni_runtime::{AppContext, BackgroundService, BootError, BootPhase, BootStep, Subsystem};
use omni_store::entity::EntityDescriptor;
use omni_tasks::CronSchedule;

use crate::activity::{ActivityRecorder, McpCallData};
use crate::events::claude_sessions::{ClaudeSessionWatch, ClaudeSessionWatcher};
use crate::events::persistence::{EventDelivery, EventReceipt, EventRequest, EventSubscription};
use crate::events::publisher::McpEventPublisher;
use crate::events::service::McpEventService;
use crate::events::task_runs::TaskRunWatcher;
use crate::events::webhook::{LiveWebhookTransport, WebhookClient};
use crate::gateway::access::DownstreamAccess;
use crate::gateway::bundled::{BundledRegistry, default_root};
use crate::gateway::connect::OAuthRefresher;
use crate::gateway::oauth::client::OAuthClient;
use crate::gateway::oauth::server::AuthorizationServer;
use crate::gateway::routes::AdminState;
use crate::gateway::secrets::{GatewaySecrets, SecretsError, key_path};
use crate::gateway::{Gateway, GatewayParts};
use crate::host::ClaudeHost;
use crate::rpc::{McpProtocol, ProtocolSetupError};
use crate::tasks::{
    CLAUDE_SESSION_EVENTS_SCHEDULE, ClaudeSessionEventsTask, GATEWAY_EVENT_FEEDS_SCHEDULE,
    GATEWAY_REFRESH_SCHEDULE, GatewayEventFeedsTask, GatewayRefreshTask,
    MCP_EVENT_DELIVERY_SCHEDULE, McpEventDeliveryTask, TASK_RUN_EVENTS_SCHEDULE, TaskRunEventsTask,
};
use crate::tools::claude_sessions::ClaudeDeps;
use crate::tools::system::{ConfiguredFeatures, SystemDeps};

const LOG: &str = "MCP";

/// Every entity this package persists (for `migrate_all` and the compat audit).
pub fn entities() -> Vec<EntityDescriptor> {
    let mut entities = vec![
        EntityDescriptor::of::<McpCallData>(),
        EntityDescriptor::of::<EventSubscription>(),
        EntityDescriptor::of::<EventReceipt>(),
        EntityDescriptor::of::<EventDelivery>(),
        EntityDescriptor::of::<EventRequest>(),
        EntityDescriptor::of::<ClaudeSessionWatch>(),
    ];
    entities.extend(gateway::model::entities());
    entities
}

#[derive(Debug, thiserror::Error)]
pub enum McpSetupError {
    #[error(transparent)]
    ToolMeta(#[from] ToolMetaError),
    #[error(transparent)]
    Protocol(#[from] ProtocolSetupError),
    #[error("invalid schedule {0}")]
    Schedule(String),
    #[error(
        "OMNI_DEVICE_LINK_TOKEN is set but the Claude Code host port is not; build the device link first"
    )]
    MissingClaudeHost,
    #[error(transparent)]
    GatewaySecrets(#[from] SecretsError),
    #[error("invalid OMNI_GATEWAY_PUBLIC_URL: {0}")]
    GatewayPublicUrl(String),
}

fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

fn time_zone(config: &omni_config::Config) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(&config.tz).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// Approval requests reach the owner's phone at most this often.
const APPROVAL_PUSH_INTERVAL_MS: i64 = 5 * 60 * 1000;

/// Pushes "approve this client" to the owner, throttled: anyone on the
/// internet can start an authorization request, so a burst cannot spam.
fn approval_notifier(ctx: &AppContext) -> crate::gateway::oauth::server::AuthorizationNotifier {
    let pushover = ctx.pushover.clone();
    let tracker = ctx.tracker.clone();
    let clock = ctx.clock.clone();
    let page = format!(
        "{}/gateway",
        ctx.config.ios_control_home_url.trim_end_matches('/')
    );
    let last = Arc::new(std::sync::atomic::AtomicI64::new(i64::MIN / 2));
    Arc::new(move |row: &gateway::model::AuthorizationRow| {
        let now = clock.now_ms();
        let previous = last.load(std::sync::atomic::Ordering::Relaxed);
        if now - previous < APPROVAL_PUSH_INTERVAL_MS
            || last
                .compare_exchange(
                    previous,
                    now,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }
        // The app names itself, so only the redirect host is trustworthy here,
        // and the user code stays on the sign-in page that started the request.
        let host = url::Url::parse(&row.redirect_uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "an unknown address".to_owned());
        let message = omni_alerts::PushoverMessage {
            title: Some("MCP gateway sign-in request".to_owned()),
            message: format!(
                "An app returning to {host} asks to connect to Omni's MCP gateway. If you started it, enter the code from its sign-in page on the MCP gateway page; otherwise deny it."
            ),
            url: Some(page.clone()),
            url_title: Some("Open the MCP gateway".to_owned()),
            ..omni_alerts::PushoverMessage::default()
        };
        let pushover = pushover.clone();
        omni_core::spawn::spawn_tracked(&tracker, "mcp-gateway-approval-push", async move {
            if let Err(error) = pushover
                .send(omni_alerts::PushoverChannel::General, message)
                .await
            {
                tracing::warn!(target: LOG, %error, "Gateway approval push failed");
            }
        });
    })
}

/// The MCP package for one process.
pub struct McpPackage {
    ctx: AppContext,
    events: McpEventService,
    watcher: Option<ClaudeSessionWatcher>,
    host: Option<Arc<dyn ClaudeHost>>,
    recorder: ActivityRecorder,
    features: ConfiguredFeatures,
    gateway: Gateway,
    access: DownstreamAccess,
    oauth: OAuthClient,
    authorization: Option<AuthorizationServer>,
    public_origin: Option<String>,
}

impl McpPackage {
    /// Fails boot when the gateway or MCP Events key file cannot be read or
    /// created, and when a configured device link has not set the
    /// `ClaudeHost` port yet. The events key is migrated once from the raw
    /// `OMNI_MCP_TOKEN` environment variable when its file does not exist yet
    /// (`events::crypto::load_event_key`); the variable is not configuration.
    pub fn new(ctx: &AppContext) -> Result<Self, McpSetupError> {
        let config = &ctx.config;
        let host: Option<Arc<dyn ClaudeHost>> = ctx.ports.claude_host();
        if non_empty(config.omni_device_link_token.as_ref()).is_some() && host.is_none() {
            return Err(McpSetupError::MissingClaudeHost);
        }
        let secrets = GatewaySecrets::load_or_create(&key_path(&ctx.paths.data_dir))?;
        let legacy_token = std::env::var(events::crypto::LEGACY_TOKEN_VAR).ok();
        let (event_key, source) =
            events::crypto::load_event_key(&ctx.paths.data_dir, legacy_token.as_deref())?;
        match source {
            events::crypto::KeySource::MigratedFromToken => tracing::info!(
                target: LOG,
                "MCP Events key migrated from OMNI_MCP_TOKEN into mcp-events.key; the variable can be removed"
            ),
            events::crypto::KeySource::Generated => {
                tracing::info!(target: LOG, "MCP Events key created in mcp-events.key");
            }
            events::crypto::KeySource::File => {}
        }
        let transport = LiveWebhookTransport::new(ctx.public_http.clone(), ctx.side_effects);
        let events = McpEventService::from_key(
            event_key,
            ctx.store.clone(),
            ctx.clock.clone(),
            Arc::new(WebhookClient::new(Arc::new(transport), ctx.clock.clone())),
            ctx.ports.clone(),
        );
        let oauth = OAuthClient::new(ctx.public_http.clone());
        let recorder =
            ActivityRecorder::new(ctx.store.clone(), ctx.clock.clone(), ctx.tracker.clone());
        let gateway = Gateway::new(GatewayParts {
            store: ctx.store.clone(),
            clock: ctx.clock.clone(),
            http: ctx.public_http.clone(),
            secrets: secrets.clone(),
            refresher: Some(OAuthRefresher::new(oauth.clone())),
            bundled: BundledRegistry::installed(&default_root(config.dockerized)),
            tracker: ctx.tracker.clone(),
            shutdown: ctx.shutdown.clone(),
            recorder: recorder.clone(),
            allow_loopback_http: false,
            side_effects: ctx.side_effects,
        });
        let access = DownstreamAccess::new(
            ctx.store.clone(),
            ctx.clock.clone(),
            secrets.clone(),
            ctx.tracker.clone(),
        );
        gateway::events::extend_events(&events, &gateway, &access);
        let public_origin = non_empty(config.omni_gateway_public_url.as_ref()).map(str::to_owned);
        let authorization = match &public_origin {
            Some(origin) => {
                let resource = url::Url::parse(&format!("{origin}{}", gateway::endpoint::PATH))
                    .map_err(|e| McpSetupError::GatewayPublicUrl(e.to_string()))?;
                Some(AuthorizationServer::new(
                    ctx.store.clone(),
                    ctx.clock.clone(),
                    ctx.public_http.clone(),
                    secrets,
                    resource,
                    Some(approval_notifier(ctx)),
                    ctx.tracker.clone(),
                ))
            }
            None => None,
        };
        let watcher = host.as_ref().map(|host| {
            ClaudeSessionWatcher::new(
                events.clone(),
                host.clone(),
                ctx.store.clone(),
                ctx.clock.clone(),
            )
        });
        Ok(Self {
            recorder,
            features: ConfiguredFeatures::from_config(config),
            ctx: ctx.clone(),
            events,
            watcher,
            host,
            gateway,
            access,
            oauth,
            authorization,
            public_origin,
        })
    }

    /// Overrides the configuration-derived `system_status` facts.
    pub fn with_features(mut self, features: ConfiguredFeatures) -> Self {
        self.features = features;
        self
    }

    pub fn events(&self) -> &McpEventService {
        &self.events
    }

    pub fn gateway(&self) -> &Gateway {
        &self.gateway
    }

    pub fn access(&self) -> &DownstreamAccess {
        &self.access
    }

    pub fn authorization(&self) -> Option<&AuthorizationServer> {
        self.authorization.as_ref()
    }

    pub fn watcher(&self) -> Option<&ClaudeSessionWatcher> {
        self.watcher.as_ref()
    }

    /// The `EventPublisher` port over this outbox.
    pub fn event_publisher(&self) -> McpEventPublisher {
        McpEventPublisher::new(self.events.clone())
    }

    /// The `McpEvents` email handler (register it before every other handler).
    pub fn email_handler(&self) -> Arc<dyn EmailHandler> {
        self.events.email_handler()
    }

    /// This package's tools: `system`, `events`, `claude-sessions` groups.
    pub fn tools(&self) -> Result<Vec<McpTool>, ToolMetaError> {
        let mut tools = tools::system::system_tools(&SystemDeps {
            tasks: Arc::new(self.ctx.tasks.clone()),
            ports: self.ctx.ports.clone(),
            features: self.features,
            clock: self.ctx.clock.clone(),
        })?;
        tools.extend(tools::events::event_tools(Some(self.events.clone()))?);
        tools.extend(tools::claude_sessions::claude_session_tools(&ClaudeDeps {
            host: self.host.clone(),
            watcher: self
                .watcher
                .clone()
                .map(|w| Arc::new(w) as Arc<dyn omni_runtime::ports::ClaudeSessionNotifier>),
        })?);
        Ok(tools)
    }

    /// The subsystem serving `all_tools` (every package's, this one's included).
    pub fn subsystem(self, all_tools: Vec<McpTool>) -> Result<Subsystem, McpSetupError> {
        let ctx = &self.ctx;
        let protocol = McpProtocol::gateway(
            all_tools,
            self.recorder.clone(),
            Some(self.events.clone()),
            self.gateway.clone(),
        )?;
        let mut router =
            activity_routes::router(ctx.store.clone(), ctx.clock.clone(), self.host.clone())
                .merge(gateway::endpoint::router(
                    protocol,
                    self.access.clone(),
                    self.gateway.clone(),
                    self.authorization
                        .as_ref()
                        .map(AuthorizationServer::resource_metadata_url),
                    ctx.shutdown.clone(),
                ))
                .merge(gateway::routes::router(AdminState {
                    gateway: self.gateway.clone(),
                    access: self.access.clone(),
                    events: Some(self.events.clone()),
                    oauth: self.oauth.clone(),
                    authorization: self.authorization.clone(),
                    public_url: self.public_origin.clone(),
                }));
        if let Some(server) = &self.authorization {
            router = router.merge(server.router());
        }
        let tz = time_zone(&ctx.config);
        let schedule = |expr: &str| {
            CronSchedule::parse(expr, &tz).map_err(|_| McpSetupError::Schedule(expr.to_owned()))
        };
        let mut subsystem = Subsystem {
            router,
            entities: entities(),
            ..Subsystem::named("mcp")
        };
        subsystem.boot_steps.push(BootStep {
            phase: BootPhase::Reconcile,
            name: "markInterruptedCalls",
            run: Box::new(|ctx: AppContext| {
                async move {
                    let marked = activity::mark_interrupted_calls(&ctx.store, ctx.clock.now_ms())
                        .await
                        .map_err(|e| BootError::new("markInterruptedCalls", e.to_string()))?;
                    if marked > 0 {
                        tracing::warn!(target: "Main", "Marked {marked} interrupted MCP call(s)");
                    }
                    Ok(())
                }
                .boxed()
            }),
        });
        let legacy = self.events.clone();
        subsystem.boot_steps.push(BootStep {
            phase: BootPhase::Migrate,
            name: "dropLegacyEventSubscriptions",
            run: Box::new(move |_ctx: AppContext| {
                async move {
                    let dropped = legacy.drop_legacy_subscriptions().await.map_err(|e| {
                        BootError::new("dropLegacyEventSubscriptions", e.to_string())
                    })?;
                    if dropped > 0 {
                        tracing::info!(
                            target: LOG,
                            "Dropped {dropped} MCP event subscription(s) from before the gateway"
                        );
                    }
                    Ok(())
                }
                .boxed()
            }),
        });
        subsystem.boot_steps.push(BootStep {
            phase: BootPhase::Migrate,
            name: "requireArchiveEcho",
            run: Box::new(|ctx: AppContext| {
                async move {
                    if ctx.ports.archive_echo().is_some() {
                        Ok(())
                    } else {
                        Err(BootError::new(
                            "requireArchiveEcho",
                            "MCP email events need the ArchiveEcho port \
                             (omni_imap::transport::StoreArchiveEcho); refusing to start \
                             rather than publish an event for every archive move",
                        ))
                    }
                }
                .boxed()
            }),
        });
        subsystem.tasks.push(Arc::new(McpEventDeliveryTask::new(
            self.events.clone(),
            ctx.pushover.clone(),
            ctx.tracker.clone(),
            schedule(MCP_EVENT_DELIVERY_SCHEDULE)?,
        )));
        subsystem.tasks.push(Arc::new(TaskRunEventsTask::new(
            TaskRunWatcher::new(self.events.clone(), ctx.store.clone(), ctx.clock.clone()),
            schedule(TASK_RUN_EVENTS_SCHEDULE)?,
        )));
        let worker = self.events.clone();
        subsystem.services.push(BackgroundService {
            name: "McpEventDelivery worker",
            start: Box::new(move |ctx: AppContext| {
                let events = worker.clone();
                async move {
                    if events.drain().await.is_err() {
                        tracing::warn!(target: LOG, "MCP event recovery deferred to scheduled retry");
                    }
                    events.delivery_worker(ctx.shutdown.clone()).await;
                }
                .boxed()
            }),
            retry: None,
        });
        if let Some(watcher) = &self.watcher {
            subsystem.tasks.push(Arc::new(ClaudeSessionEventsTask::new(
                watcher.clone(),
                schedule(CLAUDE_SESSION_EVENTS_SCHEDULE)?,
            )));
        }
        let gateway = self.gateway.clone();
        subsystem.boot_steps.push(BootStep {
            phase: BootPhase::Services,
            name: "loadGateway",
            run: Box::new(move |_ctx: AppContext| {
                let gateway = gateway.clone();
                async move {
                    gateway
                        .load()
                        .await
                        .map_err(|e| BootError::new("loadGateway", e.to_string()))
                }
                .boxed()
            }),
        });
        let worker = self.gateway.clone();
        subsystem.services.push(BackgroundService {
            name: "McpGateway refresh worker",
            start: Box::new(move |_ctx: AppContext| {
                let gateway = worker.clone();
                async move { gateway.refresh_worker().await }.boxed()
            }),
            retry: None,
        });
        subsystem.tasks.push(Arc::new(GatewayRefreshTask::new(
            self.gateway.clone(),
            self.authorization.clone(),
            schedule(GATEWAY_REFRESH_SCHEDULE)?,
        )));
        subsystem.tasks.push(Arc::new(GatewayEventFeedsTask::new(
            self.gateway.clone(),
            self.events.clone(),
            schedule(GATEWAY_EVENT_FEEDS_SCHEDULE)?,
        )));
        Ok(subsystem)
    }
}
