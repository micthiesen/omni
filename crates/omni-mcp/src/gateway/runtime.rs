//! One integration at run time: its session, call bounds, catalog and health.
//!
//! A session is opened on first use and shared by concurrent calls (at most
//! [`MAX_CONCURRENT_CALLS`] at once). Connecting is bounded by each call's
//! deadline and cancellation. A configuration or credential change bumps the
//! session epoch: later calls connect again, and the old session closes once
//! the calls running on it finish (at most [`DRAIN_LIMIT`]; at once on
//! shutdown). A session is replaced only when it expired, rejected its
//! credentials or died; one failed request leaves it shared. Retries happen
//! only when the request certainly did not run: an expired session
//! (re-initialize), a 401 that an OAuth refresh can fix (concurrent 401s
//! share one refresh), or a connection that failed before sending. Reads
//! (lists, `resources/read`, `events/poll`) are also retried once after a
//! network or 5xx failure; `tools/call` never is, and a call whose outcome is
//! unknown says so.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use omni_api::gateway::{GatewayErrorKind, IntegrationStatus};
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{SideEffectMode, Url};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::bundled::BundledRegistry;
use super::catalog::{self, IntegrationCatalog, offers_events};
use super::client::http::HttpSession;
use super::client::stdio::StdioSession;
use super::client::{ClientError, RequestOptions, RpcError, Session, UpstreamNotice};
use super::credentials::Credentials;
use super::error::{GatewayError, from_client};
use super::model::{AuthKind, CatalogRow, HealthRow, IntegrationRow, RefreshRow, Transport};

const LOG: &str = "MCP:Gateway";
pub const MAX_CONCURRENT_CALLS: usize = 8;
/// Each catalog page.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 100;

/// What every runtime shares.
#[derive(Clone)]
pub struct Deps {
    pub store: Store,
    pub clock: SharedClock,
    pub http: PublicHttpClient,
    pub credentials: Credentials,
    pub bundled: BundledRegistry,
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
    /// Integration ids whose catalog should be refreshed (notifications).
    pub refresh: mpsc::UnboundedSender<String>,
    /// `Record` never sends `tools/call` (its effects are unknown).
    pub side_effects: SideEffectMode,
}

struct Live {
    session: Arc<dyn Session>,
    refreshable: bool,
    epoch: u64,
    /// Requests running on this session.
    busy: Arc<AtomicUsize>,
}

/// Counts one running request on a session while held.
struct Busy(Arc<AtomicUsize>);

impl Busy {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The longest one catalog refresh may take, all pages included.
const REFRESH_LIMIT: Duration = Duration::from_secs(180);
/// How long a replaced session may keep running calls before it is closed.
const DRAIN_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Why connecting failed.
struct ConnectFailure {
    error: GatewayError,
    unauthorized: bool,
}

/// A fetched catalog before parsing; `None` lists failed and keep the
/// previous catalog's.
struct Fetched {
    server: Value,
    tools: Vec<Value>,
    resources: Option<Vec<Value>>,
    templates: Option<Vec<Value>>,
    events: Option<Vec<Value>>,
    warnings: Vec<String>,
}

pub struct Runtime {
    pub id: String,
    config: RwLock<IntegrationRow>,
    session: tokio::sync::Mutex<Option<Live>>,
    /// Bumped by [`Runtime::reset`]; a session from an older epoch is not reused.
    epoch: AtomicU64,
    /// Set when the integration is deleted: nothing connects or writes again.
    removed: AtomicBool,
    permits: Arc<Semaphore>,
    catalog: RwLock<Option<Arc<IntegrationCatalog>>>,
    health: RwLock<HealthRow>,
    refreshing: tokio::sync::Mutex<()>,
}

fn empty_health(id: &str) -> HealthRow {
    HealthRow {
        integration: id.to_owned(),
        last_refresh: None,
        last_success_at: None,
        error: None,
        added: Vec::new(),
        changed: Vec::new(),
        extra: Extra::default(),
    }
}

/// Closes a replaced session once its running requests finish.
fn close_later(deps: &Deps, live: Live) {
    let shutdown = deps.shutdown.clone();
    omni_core::spawn::spawn_tracked(&deps.tracker, "mcp-gateway-close", async move {
        let drained = async {
            while live.busy.load(Ordering::SeqCst) > 0 && live.session.alive() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::select! {
            () = drained => {}
            () = shutdown.cancelled() => {}
            () = tokio::time::sleep(DRAIN_LIMIT) => {}
        }
        live.session.close().await;
    });
}

impl Runtime {
    pub fn new(
        config: IntegrationRow,
        catalog: Option<IntegrationCatalog>,
        health: Option<HealthRow>,
    ) -> Self {
        Self {
            id: config.id.clone(),
            health: RwLock::new(health.unwrap_or_else(|| empty_health(&config.id))),
            config: RwLock::new(config),
            session: tokio::sync::Mutex::new(None),
            epoch: AtomicU64::new(0),
            removed: AtomicBool::new(false),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CALLS)),
            catalog: RwLock::new(catalog.map(Arc::new)),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn config(&self) -> IntegrationRow {
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn catalog(&self) -> Option<Arc<IntegrationCatalog>> {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn health(&self) -> HealthRow {
        self.health
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> IntegrationStatus {
        if !self.config().enabled {
            return IntegrationStatus::Disabled;
        }
        let health = self.health();
        let has_catalog = self.catalog().is_some();
        match &health.error {
            Some(error) if matches!(error.kind.as_str(), "auth_required" | "auth_rejected") => {
                IntegrationStatus::NeedsAuth
            }
            Some(_) if has_catalog => IntegrationStatus::Stale,
            Some(_) => IntegrationStatus::Error,
            None if has_catalog => IntegrationStatus::Ready,
            None => IntegrationStatus::Pending,
        }
    }

    /// Applies a new configuration. The session is replaced only when how
    /// Omni reaches or authenticates to the integration changed.
    pub fn reconfigure(self: &Arc<Self>, config: IntegrationRow, deps: &Deps) {
        let previous = self.config();
        let reconnect = previous.transport != config.transport
            || previous.auth != config.auth
            || previous.enabled != config.enabled;
        let transport_changed = previous.transport != config.transport;
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = config;
        if transport_changed {
            *self.catalog.write().unwrap_or_else(PoisonError::into_inner) = None;
            *self.health.write().unwrap_or_else(PoisonError::into_inner) = empty_health(&self.id);
        }
        if reconnect {
            self.reset(deps);
        }
    }

    /// Makes the next call connect again (credentials or configuration
    /// changed); calls already running finish on their session. Never blocks.
    pub fn reset(self: &Arc<Self>, deps: &Deps) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let runtime = self.clone();
        let deps_close = deps.clone();
        omni_core::spawn::spawn_tracked(&deps.tracker, "mcp-gateway-reset", async move {
            let mut slot = runtime.session.lock().await;
            if slot.as_ref().is_some_and(|live| live.epoch < epoch)
                && let Some(live) = slot.take()
            {
                close_later(&deps_close, live);
            }
        });
    }

    /// Stops the integration for good (it was deleted).
    pub fn retire(self: &Arc<Self>, deps: &Deps) {
        self.removed.store(true, Ordering::SeqCst);
        self.reset(deps);
    }

    async fn connect(&self, deps: &Deps, force_refresh: bool) -> Result<Live, ConnectFailure> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let config = self.config();
        let fail = |error: GatewayError| ConnectFailure {
            error,
            unauthorized: false,
        };
        if self.removed() {
            return Err(fail(GatewayError::config("The integration was removed.")));
        }
        if !config.enabled {
            return Err(fail(GatewayError::config("The integration is disabled.")));
        }
        let oauth = config.auth == AuthKind::OAuth;
        let resolved = deps
            .credentials
            .resolve(&config, &deps.bundled, force_refresh)
            .await
            .map_err(fail)?;
        let (notices, mut received) = mpsc::unbounded_channel();
        let stop = deps.shutdown.child_token();
        let session: Result<Arc<dyn Session>, ClientError> = match &config.transport {
            Transport::Http { url } => match Url::parse(url) {
                Ok(url) => HttpSession::connect(
                    deps.http.clone(),
                    url,
                    resolved.headers,
                    notices,
                    deps.tracker.clone(),
                    stop.clone(),
                )
                .await
                .map(|s| Arc::new(s) as Arc<dyn Session>),
                Err(error) => Err(ClientError::Blocked(format!("invalid URL: {error}"))),
            },
            Transport::Stdio { server } => match deps.bundled.get(server) {
                Some(bundled) if bundled.available() => StdioSession::connect(
                    &bundled.command(&resolved.env),
                    notices,
                    deps.tracker.clone(),
                    stop.clone(),
                )
                .await
                .map(|s| Arc::new(s) as Arc<dyn Session>),
                Some(_) => Err(ClientError::Unsupported(format!(
                    "The bundled server {server} is not installed in this image."
                ))),
                None => Err(ClientError::Unsupported(format!(
                    "Omni has no bundled server named {server}."
                ))),
            },
        };
        let session = session.map_err(|error| ConnectFailure {
            unauthorized: matches!(error, ClientError::Unauthorized { .. }),
            error: from_client(&error, oauth),
        })?;
        let id = self.id.clone();
        let refresh = deps.refresh.clone();
        omni_core::spawn::spawn_tracked(&deps.tracker, "mcp-gateway-notices", async move {
            loop {
                let notice = tokio::select! {
                    () = stop.cancelled() => return,
                    notice = received.recv() => notice,
                };
                match notice {
                    Some(UpstreamNotice::ToolsChanged | UpstreamNotice::ResourcesChanged) => {
                        let _ = refresh.send(id.clone());
                    }
                    Some(UpstreamNotice::Closed) | None => return,
                }
            }
        });
        Ok(Live {
            session,
            refreshable: resolved.refreshable,
            epoch,
            busy: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The live session. A session that is alive, current and not the one
    /// that just failed is shared; otherwise this caller connects (with a
    /// forced OAuth refresh after a 401). Concurrent callers wait for one
    /// connect instead of each opening their own.
    async fn live(
        &self,
        deps: &Deps,
        failed: Option<&Arc<dyn Session>>,
        force_refresh: bool,
    ) -> Result<(Arc<dyn Session>, bool, Arc<AtomicUsize>), GatewayError> {
        let mut slot = self.session.lock().await;
        let epoch = self.epoch.load(Ordering::SeqCst);
        if let Some(live) = slot.as_ref()
            && live.session.alive()
            && live.epoch == epoch
            && failed.is_none_or(|failed| !Arc::ptr_eq(failed, &live.session))
        {
            return Ok((live.session.clone(), live.refreshable, live.busy.clone()));
        }
        if let Some(stale) = slot.take() {
            close_later(deps, stale);
        }
        let live = match self.connect(deps, force_refresh).await {
            Ok(live) => live,
            Err(failure) if failure.unauthorized && !force_refresh => {
                match self.connect(deps, true).await {
                    Ok(live) => live,
                    Err(again) => return Err(again.error),
                }
            }
            Err(failure) => return Err(failure.error),
        };
        let handle = (live.session.clone(), live.refreshable, live.busy.clone());
        *slot = Some(live);
        Ok(handle)
    }

    /// One request with the retry rules above. `Ok(Err)` is the
    /// integration's JSON-RPC error, passed through.
    pub async fn rpc(
        &self,
        deps: &Deps,
        method: &str,
        params: Map<String, Value>,
        timeout: Duration,
        cancel: &CancellationToken,
        idempotent: bool,
    ) -> Result<Result<Value, RpcError>, GatewayError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let cancelled =
            || GatewayError::new(GatewayErrorKind::Timeout, "The request was cancelled.");
        let timed_out = || {
            GatewayError::new(
                GatewayErrorKind::Timeout,
                "The integration did not answer in time.",
            )
            .hint("Retry, or raise the integration's timeout if its tools are slow.")
        };
        let _permit = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            permit = tokio::time::timeout_at(deadline, self.permits.clone().acquire_owned()) => match permit {
                Ok(Ok(permit)) => permit,
                _ => return Err(GatewayError::new(
                    GatewayErrorKind::Timeout,
                    "The integration is busy with other calls.",
                ).hint("Retry shortly.")),
            },
        };
        let oauth = self.config().auth == AuthKind::OAuth;
        let (mut reconnected, mut refreshed, mut retried) = (false, false, false);
        let mut failed: Option<Arc<dyn Session>> = None;
        let mut force_refresh = false;
        loop {
            let force = std::mem::take(&mut force_refresh);
            let connecting = self.live(deps, failed.as_ref(), force);
            let (session, refreshable, busy) = tokio::select! {
                () = cancel.cancelled() => return Err(cancelled()),
                live = tokio::time::timeout_at(deadline, connecting) => match live {
                    Ok(Ok(live)) => live,
                    Ok(Err(error)) => {
                        self.note_failure(deps, &error).await;
                        return Err(error);
                    }
                    Err(_) => return Err(timed_out()),
                },
            };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let options = RequestOptions {
                timeout: remaining.max(Duration::from_millis(1)),
                cancel: cancel.clone(),
            };
            let running = Busy::new(&busy);
            let answer = session.request(method, params.clone(), options).await;
            drop(running);
            match answer {
                Ok(outcome) => {
                    self.note_success(deps).await;
                    return Ok(outcome);
                }
                Err(ClientError::SessionExpired) if !reconnected => {
                    reconnected = true;
                    failed = Some(session);
                }
                Err(ClientError::Unauthorized { .. }) if refreshable && !refreshed => {
                    refreshed = true;
                    force_refresh = true;
                    failed = Some(session);
                }
                Err(
                    error @ (ClientError::Network(_)
                    | ClientError::Http { .. }
                    | ClientError::Process(_)),
                ) if idempotent
                    && !retried
                    && !matches!(error, ClientError::Http { status, .. } if status < 500) =>
                {
                    retried = true;
                    // A live session stays shared; only a dead one is replaced.
                    failed = (!session.alive()).then_some(session);
                }
                Err(error) => {
                    let mut gateway_error = from_client(&error, oauth);
                    if !idempotent && !error.not_processed() {
                        gateway_error = gateway_error.uncertain();
                    }
                    if gateway_error.needs_auth() {
                        self.note_failure(deps, &gateway_error).await;
                    }
                    return Err(gateway_error);
                }
            }
        }
    }

    /// Records an error for the UI (persisted; at most one write per change).
    pub(super) async fn note_failure(&self, deps: &Deps, error: &GatewayError) {
        if self.removed() {
            return;
        }
        let row = {
            let mut health = self.health.write().unwrap_or_else(PoisonError::into_inner);
            let unchanged = health.error.as_ref().is_some_and(|e| {
                e.message == error.message && e.kind == super::error::kind_name(error.kind)
            });
            if unchanged {
                return;
            }
            health.error = Some(error.to_row(deps.clock.now_ms()));
            health.clone()
        };
        if let Err(store_error) = persist_health(&deps.store, row).await {
            tracing::warn!(target: LOG, error = %store_error, integration = %self.id, "Gateway health not recorded");
        }
    }

    /// Clears an authentication error once a request succeeds again.
    async fn note_success(&self, deps: &Deps) {
        if self.removed() {
            return;
        }
        let row = {
            let mut health = self.health.write().unwrap_or_else(PoisonError::into_inner);
            let auth_error = health
                .error
                .as_ref()
                .is_some_and(|e| matches!(e.kind.as_str(), "auth_required" | "auth_rejected"));
            if !auth_error || health.last_refresh.as_ref().is_some_and(|r| !r.ok) {
                return;
            }
            health.error = None;
            health.clone()
        };
        if let Err(store_error) = persist_health(&deps.store, row).await {
            tracing::warn!(target: LOG, error = %store_error, integration = %self.id, "Gateway health not recorded");
        }
    }

    async fn page(
        &self,
        deps: &Deps,
        method: &str,
        key: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>, GatewayError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut params = Map::new();
            if let Some(cursor) = &cursor {
                params.insert("cursor".into(), json!(cursor));
            }
            let result = self
                .rpc(deps, method, params, LIST_TIMEOUT, cancel, true)
                .await?
                .map_err(|error| {
                    GatewayError::new(
                        GatewayErrorKind::Upstream,
                        format!("{method} failed: {} ({})", error.message, error.code),
                    )
                })?;
            if let Some(Value::Array(page)) = result.get(key) {
                items.extend(page.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() && seen.insert(next.to_owned()) => {
                    cursor = Some(next.to_owned());
                }
                _ => return Ok(items),
            }
        }
        Err(GatewayError::new(
            GatewayErrorKind::Protocol,
            format!("{method} returned more than {MAX_PAGES} pages"),
        ))
    }

    /// A secondary list, or `None` (with a warning) when it failed.
    async fn optional_page(
        &self,
        deps: &Deps,
        method: &str,
        key: &str,
        cancel: &CancellationToken,
        warnings: &mut Vec<String>,
    ) -> Option<Vec<Value>> {
        match self.page(deps, method, key, cancel).await {
            Ok(items) => Some(items),
            Err(error) => {
                warnings.push(format!(
                    "{method} failed, keeping the previous list: {}",
                    error.message
                ));
                None
            }
        }
    }

    async fn fetch(
        &self,
        deps: &Deps,
        cancel: &CancellationToken,
    ) -> Result<Fetched, GatewayError> {
        let tools = self.page(deps, "tools/list", "tools", cancel).await?;
        let (session, _, _) = self.live(deps, None, false).await?;
        let server = session.initialized().clone();
        let capabilities = server.get("capabilities").cloned().unwrap_or(Value::Null);
        let mut warnings = Vec::new();
        let (mut resources, mut templates, mut events) =
            (Some(Vec::new()), Some(Vec::new()), Some(Vec::new()));
        if capabilities.get("resources").is_some() {
            resources = self
                .optional_page(deps, "resources/list", "resources", cancel, &mut warnings)
                .await;
            templates = self
                .optional_page(
                    deps,
                    "resources/templates/list",
                    "resourceTemplates",
                    cancel,
                    &mut warnings,
                )
                .await;
        }
        if offers_events(&capabilities) {
            events = self
                .optional_page(deps, "events/list", "events", cancel, &mut warnings)
                .await;
        }
        Ok(Fetched {
            server,
            tools,
            resources,
            templates,
            events,
            warnings,
        })
    }

    /// Fetches and stores the catalog. On failure the previous catalog stays
    /// served, and a secondary list that failed keeps its previous entries.
    /// Returns whether the stored catalog changed.
    pub async fn refresh(&self, deps: &Deps) -> Result<(bool, RefreshRow), StoreError> {
        let _serialized = self.refreshing.lock().await;
        let started = deps.clock.now_ms();
        let cancel = deps.shutdown.child_token();
        let fetched = match tokio::time::timeout(REFRESH_LIMIT, self.fetch(deps, &cancel)).await {
            Ok(fetched) => fetched,
            Err(_) => {
                cancel.cancel();
                Err(GatewayError::new(
                    GatewayErrorKind::Timeout,
                    "Reading the integration's catalog took over three minutes.",
                )
                .hint("Retry; if it keeps happening, the integration's lists are too slow or too long."))
            }
        };
        let now = deps.clock.now_ms();
        let previous = self.catalog();
        let mut health = self.health();
        let (changed, row) = match fetched {
            Ok(fetched) => {
                let mut warnings = fetched.warnings;
                let resources = fetched.resources.unwrap_or_else(|| {
                    previous
                        .as_ref()
                        .map(|p| p.resources.clone())
                        .unwrap_or_default()
                });
                let templates = fetched.templates.unwrap_or_else(|| {
                    previous
                        .as_ref()
                        .map(|p| p.templates.clone())
                        .unwrap_or_default()
                });
                let raw_events = fetched.events.unwrap_or_else(|| {
                    previous
                        .as_ref()
                        .map(|p| p.events.iter().map(|e| e.definition.clone()).collect())
                        .unwrap_or_default()
                });
                let tools = catalog::build_tools(&self.id, &fetched.tools, &mut warnings);
                let events = catalog::build_events(&self.id, &raw_events, &mut warnings);
                let (added, removed, changed) = catalog::diff(
                    previous.as_ref().map_or(&[][..], |p| p.tools.as_slice()),
                    &tools,
                );
                let parsed = IntegrationCatalog {
                    fetched_at: now,
                    server: fetched.server.clone(),
                    tools,
                    resources: resources.clone(),
                    templates: templates.clone(),
                    events,
                    warnings: warnings.clone(),
                };
                let changed_catalog = previous.as_deref().is_none_or(|p| {
                    p.tools != parsed.tools
                        || p.resources != parsed.resources
                        || p.templates != parsed.templates
                        || p.events != parsed.events
                        || p.server != parsed.server
                });
                if self.removed() {
                    return Ok((false, failed_report(now, started, "removed".into())));
                }
                let stored = CatalogRow {
                    integration: self.id.clone(),
                    fetched_at: now,
                    server_json: fetched.server.to_string(),
                    tools_json: Value::Array(fetched.tools).to_string(),
                    resources_json: Value::Array(resources).to_string(),
                    templates_json: Value::Array(templates).to_string(),
                    events_json: Value::Array(raw_events).to_string(),
                    extra: Extra::default(),
                };
                deps.store
                    .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
                    .await?;
                *self.catalog.write().unwrap_or_else(PoisonError::into_inner) =
                    Some(Arc::new(parsed));
                let report = RefreshRow {
                    at: now,
                    ok: true,
                    duration_ms: now - started,
                    added: added.clone(),
                    removed,
                    changed: changed.clone(),
                    warnings,
                    extra: Extra::default(),
                };
                health.last_refresh = Some(report.clone());
                health.last_success_at = Some(now);
                health.error = None;
                health.added = added;
                health.changed = changed;
                (changed_catalog, report)
            }
            Err(error) => {
                let report = failed_report(now, started, error.describe());
                health.last_refresh = Some(report.clone());
                health.error = Some(error.to_row(now));
                (false, report)
            }
        };
        if self.removed() {
            return Ok((false, row));
        }
        *self.health.write().unwrap_or_else(PoisonError::into_inner) = health.clone();
        persist_health(&deps.store, health).await?;
        Ok((changed, row))
    }
}

fn failed_report(now: i64, started: i64, warning: String) -> RefreshRow {
    RefreshRow {
        at: now,
        ok: false,
        duration_ms: now - started,
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
        warnings: vec![warning],
        extra: Extra::default(),
    }
}

async fn persist_health(store: &Store, row: HealthRow) -> Result<(), StoreError> {
    store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
}
