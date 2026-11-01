//! The gateway service: integrations, the served catalog, routing and
//! administration. Cheap to clone.
//!
//! Catalogs load from the store at boot, so the endpoint serves the last
//! known tools at once while every enabled integration refreshes in the
//! background. A refresh that changes the served tools or resources
//! notifies connected clients (`notifications/tools/list_changed`).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use omni_api::gateway::{
    CredentialsUpdate, GatewayErrorKind, IntegrationCreate, IntegrationPatch, TransportInput,
};
use omni_core::clock::SharedClock;
use omni_http::SideEffectMode;
use omni_http::public::PublicHttpClient;
use omni_mcp_kit::{Annotations, ApprovalPolicy, ToolMeta, ToolPolicy};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::bundled::BundledRegistry;
use super::catalog::{self, Contribution, ORIGIN_META_KEY, ServedCatalog};
use super::client::RpcError;
use super::credentials::{Credentials, TokenRefresher, validate_env, validate_headers};
use super::error::{GatewayError, kind_name};
use super::model::{
    AuthKind, CatalogRow, CredentialRow, FeedRow, HealthRow, IntegrationRow, RefreshRow, Transport,
};
use super::names::integration_id_problem;
use super::runtime::{Deps, Runtime};
use crate::activity::{ActivityRecorder, CallOutcome};

const LOG: &str = "MCP:Gateway";
pub const DEFAULT_TIMEOUT_MS: i64 = 120_000;
const MIN_TIMEOUT_S: u32 = 5;
const MAX_TIMEOUT_S: u32 = 600;
const MAX_INTEGRATIONS: usize = 50;
/// Resource URIs seen in tool results, remembered for `resources/read`.
const SEEN_RESOURCES: usize = 2_000;
/// Refresh requests arriving together are handled once.
const REFRESH_DEBOUNCE: Duration = Duration::from_secs(2);
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(60);

/// What changed in the served catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogChange {
    Tools,
    Resources,
}

/// A failed gateway request in JSON-RPC terms.
#[derive(Clone, Debug, PartialEq)]
pub struct CallFailure {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl From<RpcError> for CallFailure {
    fn from(error: RpcError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

/// An admin request failure with its HTTP meaning.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Gateway(GatewayError),
}

pub(super) struct Inner {
    pub(super) deps: Deps,
    secrets: super::secrets::GatewaySecrets,
    pub(super) recorder: ActivityRecorder,
    pub(super) runtimes: RwLock<BTreeMap<String, Arc<Runtime>>>,
    served: RwLock<Arc<ServedCatalog>>,
    changes: broadcast::Sender<CatalogChange>,
    refresh_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    seen_resources: Mutex<VecDeque<(String, String)>>,
    admin: tokio::sync::Mutex<()>,
    native_tools: std::sync::atomic::AtomicU32,
    allow_loopback_http: bool,
    /// In-flight `tools/call`s by owner and JSON-RPC id (`notifications/cancelled`);
    /// each entry carries its own sequence number.
    inflight: Mutex<HashMap<String, Vec<(u64, CancellationToken)>>>,
    next_call: std::sync::atomic::AtomicU64,
    /// Serializes computing and swapping the served catalog.
    rebuild_lock: Mutex<()>,
    /// Integrations with a notification-driven refresh running, and whether
    /// another was asked for meanwhile (it runs once afterwards).
    refreshing: Mutex<HashMap<String, bool>>,
}

/// Unregisters an in-flight call when the request finishes.
pub struct InflightCall {
    gateway: Gateway,
    key: String,
    seq: u64,
    pub cancel: CancellationToken,
}

impl Drop for InflightCall {
    fn drop(&mut self) {
        let mut inflight = self
            .gateway
            .inner
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(calls) = inflight.get_mut(&self.key) {
            calls.retain(|(seq, _)| *seq != self.seq);
            if calls.is_empty() {
                inflight.remove(&self.key);
            }
        }
    }
}

fn inflight_key(owner: &str, id: &Value) -> String {
    format!("{owner}\u{0}{id}")
}

/// The gateway; cheap to clone.
#[derive(Clone)]
pub struct Gateway {
    pub(super) inner: Arc<Inner>,
}

/// Everything the gateway is built from.
pub struct GatewayParts {
    pub store: Store,
    pub clock: SharedClock,
    pub http: PublicHttpClient,
    pub secrets: super::secrets::GatewaySecrets,
    pub refresher: Option<Arc<dyn TokenRefresher>>,
    pub bundled: BundledRegistry,
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
    pub recorder: ActivityRecorder,
    /// Accepts `http://` loopback integration URLs (tests with local fixtures).
    pub allow_loopback_http: bool,
    /// `Record` (preview, shadow runs) never sends `tools/call` or rotates tokens.
    pub side_effects: SideEffectMode,
}

fn now_ms(deps: &Deps) -> i64 {
    deps.clock.now_ms()
}

/// `https://` URL checks for an integration endpoint.
fn check_url(url: &str, allow_loopback_http: bool) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.len() > 2_048 {
        return Err("The URL is too long.".into());
    }
    if allow_loopback_http
        && let Ok(parsed) = omni_http::Url::parse(trimmed)
        && parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
    {
        return Ok(parsed.to_string());
    }
    let parsed = omni_http::public::assert_public_http_url_syntax(trimmed)
        .map_err(|e| format!("The URL is not allowed: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Integrations must use https://.".into());
    }
    if parsed.fragment().is_some() {
        return Err("The URL must not have a fragment.".into());
    }
    Ok(parsed.to_string())
}

impl Gateway {
    pub fn new(parts: GatewayParts) -> Self {
        let (refresh, refresh_rx) = mpsc::unbounded_channel();
        let credentials = Credentials::new(
            parts.store.clone(),
            parts.clock.clone(),
            parts.secrets.clone(),
            parts.refresher,
            parts.tracker.clone(),
            parts.side_effects,
        );
        let secrets = parts.secrets;
        let (changes, _) = broadcast::channel(16);
        Self {
            inner: Arc::new(Inner {
                deps: Deps {
                    store: parts.store,
                    clock: parts.clock,
                    http: parts.http,
                    credentials,
                    bundled: parts.bundled,
                    tracker: parts.tracker,
                    shutdown: parts.shutdown,
                    refresh,
                    side_effects: parts.side_effects,
                },
                secrets,
                recorder: parts.recorder,
                runtimes: RwLock::new(BTreeMap::new()),
                served: RwLock::new(Arc::new(ServedCatalog::default())),
                changes,
                refresh_rx: Mutex::new(Some(refresh_rx)),
                seen_resources: Mutex::new(VecDeque::new()),
                admin: tokio::sync::Mutex::new(()),
                native_tools: std::sync::atomic::AtomicU32::new(0),
                allow_loopback_http: parts.allow_loopback_http,
                inflight: Mutex::new(HashMap::new()),
                next_call: std::sync::atomic::AtomicU64::new(0),
                rebuild_lock: Mutex::new(()),
                refreshing: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(super) fn deps(&self) -> &Deps {
        &self.inner.deps
    }

    pub fn store(&self) -> &Store {
        &self.inner.deps.store
    }

    pub fn clock(&self) -> &SharedClock {
        &self.inner.deps.clock
    }

    pub fn secrets(&self) -> &super::secrets::GatewaySecrets {
        &self.inner.secrets
    }

    pub fn bundled(&self) -> &BundledRegistry {
        &self.inner.deps.bundled
    }

    pub fn credentials(&self) -> &Credentials {
        &self.inner.deps.credentials
    }

    pub fn set_native_tool_count(&self, count: usize) {
        self.inner.native_tools.store(
            u32::try_from(count).unwrap_or(u32::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn native_tool_count(&self) -> u32 {
        self.inner
            .native_tools
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Registers a call so a later `notifications/cancelled` from the same
    /// owner for `id` cancels it.
    pub fn track_call(&self, owner: &str, id: &Value, cancel: CancellationToken) -> InflightCall {
        let key = inflight_key(owner, id);
        let seq = self
            .inner
            .next_call
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_default()
            .push((seq, cancel.clone()));
        InflightCall {
            gateway: self.clone(),
            key,
            seq,
            cancel,
        }
    }

    /// `notifications/cancelled` from `owner`. The protocol is stateless, so
    /// two clients sharing one token and one request id are indistinguishable:
    /// both calls are cancelled (give each harness its own token).
    pub fn cancel_call(&self, owner: &str, id: &Value) {
        let tokens: Vec<CancellationToken> = self
            .inner
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&inflight_key(owner, id))
            .map(|calls| calls.iter().map(|(_, token)| token.clone()).collect())
            .unwrap_or_default();
        for token in tokens {
            token.cancel();
        }
    }

    pub fn runtime(&self, id: &str) -> Option<Arc<Runtime>> {
        self.inner
            .runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn runtimes(&self) -> Vec<Arc<Runtime>> {
        self.inner
            .runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Loads every integration with its last catalog and health (boot).
    pub async fn load(&self) -> Result<(), StoreError> {
        let (rows, catalogs, health) = self
            .store()
            .read(|docs| {
                Ok((
                    docs.get_all::<IntegrationRow>()?,
                    docs.get_all::<CatalogRow>()?,
                    docs.get_all::<HealthRow>()?,
                ))
            })
            .await?;
        let mut catalogs: HashMap<String, CatalogRow> = catalogs
            .into_iter()
            .map(|row| (row.integration.clone(), row))
            .collect();
        let mut health: HashMap<String, HealthRow> = health
            .into_iter()
            .map(|row| (row.integration.clone(), row))
            .collect();
        let mut runtimes = BTreeMap::new();
        for row in rows {
            let catalog = catalogs
                .remove(&row.id)
                .map(|stored| catalog::from_row(&row.id, &stored));
            let health = health.remove(&row.id);
            let enabled = row.enabled;
            let id = row.id.clone();
            runtimes.insert(id.clone(), Arc::new(Runtime::new(row, catalog, health)));
            if enabled {
                self.request_refresh(&id);
            }
        }
        *self
            .inner
            .runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner) = runtimes;
        self.rebuild(false);
        Ok(())
    }

    /// The served integration catalog.
    pub fn served(&self) -> Arc<ServedCatalog> {
        self.inner
            .served
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn changes(&self) -> broadcast::Receiver<CatalogChange> {
        self.inner.changes.subscribe()
    }

    /// Recomputes the served catalog; `notify` announces differences.
    pub(super) fn rebuild(&self, notify: bool) {
        let _serialized = self
            .inner
            .rebuild_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let runtimes = self.runtimes();
        let configs: Vec<(Arc<Runtime>, IntegrationRow)> = runtimes
            .iter()
            .map(|runtime| (runtime.clone(), runtime.config()))
            .filter(|(_, config)| config.enabled)
            .collect();
        let catalogs: Vec<_> = configs
            .iter()
            .filter_map(|(runtime, config)| runtime.catalog().map(|c| (config, c)))
            .collect();
        let contributions: Vec<Contribution<'_>> = catalogs
            .iter()
            .map(|(config, catalog)| Contribution {
                id: &config.id,
                catalog: catalog.clone(),
                hidden: &config.hidden_tools,
            })
            .collect();
        let next = Arc::new(ServedCatalog::merge(&contributions));
        let previous = {
            let mut served = self
                .inner
                .served
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *served, next.clone())
        };
        if notify {
            if previous.tools_digest != next.tools_digest {
                let _ = self.inner.changes.send(CatalogChange::Tools);
            }
            if previous.resources_digest != next.resources_digest {
                let _ = self.inner.changes.send(CatalogChange::Resources);
            }
        }
    }

    pub fn request_refresh(&self, id: &str) {
        let _ = self.inner.deps.refresh.send(id.to_owned());
    }

    /// Refreshes integrations as requests arrive (debounced), until shutdown.
    pub async fn refresh_worker(&self) {
        let receiver = self
            .inner
            .refresh_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut receiver) = receiver else {
            return;
        };
        let shutdown = self.inner.deps.shutdown.clone();
        loop {
            let first = tokio::select! {
                () = shutdown.cancelled() => {
                    self.close().await;
                    return;
                }
                next = receiver.recv() => match next {
                    Some(id) => id,
                    None => return,
                },
            };
            let mut ids = vec![first];
            let window = tokio::time::sleep(REFRESH_DEBOUNCE);
            tokio::pin!(window);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => {
                        self.close().await;
                        return;
                    }
                    () = &mut window => break,
                    next = receiver.recv() => match next {
                        Some(id) if !ids.contains(&id) => ids.push(id),
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            // Each integration refreshes on its own task, so a slow one never
            // delays the others. Requests that arrive while one runs collapse
            // into a single rerun after it.
            for id in ids {
                {
                    let mut running = self
                        .inner
                        .refreshing
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(again) = running.get_mut(&id) {
                        *again = true;
                        continue;
                    }
                    running.insert(id.clone(), false);
                }
                let gateway = self.clone();
                omni_core::spawn::spawn_tracked(
                    &self.inner.deps.tracker,
                    "mcp-gateway-refresh",
                    async move {
                        loop {
                            if let Err(error) = gateway.refresh_now(&id).await {
                                tracing::warn!(target: LOG, integration = %id, %error, "Gateway refresh failed");
                            }
                            let mut running = gateway
                                .inner
                                .refreshing
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            match running.get_mut(&id) {
                                Some(again) if *again => *again = false,
                                _ => {
                                    running.remove(&id);
                                    break;
                                }
                            }
                        }
                    },
                );
            }
        }
    }

    /// Refreshes one integration now; the stored report.
    pub async fn refresh_now(&self, id: &str) -> Result<RefreshRow, AdminError> {
        let runtime = self
            .runtime(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        if !runtime.config().enabled {
            return Err(AdminError::Invalid(format!("{id} is disabled")));
        }
        let (changed, report) = runtime.refresh(self.deps()).await?;
        if changed {
            self.rebuild(true);
        }
        if report.ok {
            tracing::info!(target: LOG, integration = %id, tools = runtime.catalog().map_or(0, |c| c.tools.len()), "Gateway catalog refreshed");
        }
        Ok(report)
    }

    /// Queues a refresh of every enabled integration (the scheduled task).
    pub fn refresh_all(&self) {
        for runtime in self.runtimes() {
            if runtime.config().enabled {
                self.request_refresh(&runtime.id);
            }
        }
    }

    /// Closes one integration's session; the next request reconnects.
    pub async fn reset_session(&self, id: &str) {
        if let Some(runtime) = self.runtime(id) {
            runtime.reset(self.deps());
        }
    }

    /// Closes every session (shutdown): HTTP sessions are deleted upstream
    /// and stdio servers stop.
    pub async fn close(&self) {
        for runtime in self.runtimes() {
            runtime.reset(self.deps());
        }
    }

    fn remember_resources(&self, integration: &str, result: &Map<String, Value>) {
        let Some(Value::Array(content)) = result.get("content") else {
            return;
        };
        let uris: Vec<String> = content
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("resource_link") => block.get("uri"),
                Some("resource") => block.get("resource").and_then(|r| r.get("uri")),
                _ => None,
            })
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if uris.is_empty() {
            return;
        }
        let mut seen = self
            .inner
            .seen_resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for uri in uris {
            seen.retain(|(known, _)| known != &uri);
            seen.push_back((uri, integration.to_owned()));
            while seen.len() > SEEN_RESOURCES {
                seen.pop_front();
            }
        }
    }

    fn seen_resource_owner(&self, uri: &str) -> Option<String> {
        self.inner
            .seen_resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(known, _)| known == uri)
            .map(|(_, owner)| owner.clone())
    }

    /// `tools/call` for a served integration tool; `None` when `name` is not one.
    /// The integration's result is returned unchanged; a gateway failure is an
    /// `isError` result that says what to do.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Option<Result<Map<String, Value>, CallFailure>> {
        let served = self.served();
        let (integration, upstream) = served.routes.get(name)?.clone();
        let runtime = self.runtime(&integration)?;
        let tool = runtime
            .catalog()
            .and_then(|catalog| catalog.tool(&upstream).cloned());
        let config = runtime.config();
        let meta = ToolMeta {
            name: name.to_owned(),
            title: tool
                .as_ref()
                .and_then(|t| t.title())
                .unwrap_or(&upstream)
                .to_owned(),
            description: String::new(),
            input_schema: Arc::new(Map::new()),
            output_schema: Arc::new(Map::new()),
            annotations: Annotations {
                read_only_hint: tool.as_ref().and_then(|t| t.read_only()).unwrap_or(false),
                destructive_hint: tool.as_ref().and_then(|t| t.destructive()).unwrap_or(true),
                idempotent_hint: tool.as_ref().and_then(|t| t.idempotent()).unwrap_or(false),
                open_world_hint: tool.as_ref().and_then(|t| t.open_world()).unwrap_or(true),
            },
            policy: ToolPolicy {
                side_effects: Vec::new(),
                cost: String::new(),
                recommended_policy: if tool.as_ref().and_then(|t| t.read_only()) == Some(true) {
                    ApprovalPolicy::Allow
                } else {
                    ApprovalPolicy::RequireApproval
                },
            },
        };
        if self.inner.deps.side_effects == SideEffectMode::Record {
            // A tool's effects are unknown, so a recording process never sends it.
            return Some(Ok(gateway_failure(
                &config.name,
                &GatewayError::new(
                    GatewayErrorKind::Config,
                    "Omni is recording side effects, so integration tools are not called.",
                ),
            )));
        }
        // Upstream arguments may hold anything; activity keeps only the call.
        let call = self.inner.recorder.start(&meta, &json!({})).await;
        let mut params = Map::new();
        params.insert("name".into(), json!(upstream));
        params.insert(
            "arguments".into(),
            match arguments {
                Value::Null => json!({}),
                other => other,
            },
        );
        let timeout = Duration::from_millis(u64::try_from(config.timeout_ms).unwrap_or(120_000));
        let outcome = runtime
            .rpc(self.deps(), "tools/call", params, timeout, &cancel, false)
            .await;
        Some(match outcome {
            Ok(Ok(Value::Object(result))) => {
                self.remember_resources(&integration, &result);
                let failed = result.get("isError").and_then(Value::as_bool) == Some(true);
                call.finish(if failed {
                    CallOutcome::Error(tool_error_text(&result))
                } else {
                    CallOutcome::Ok(None)
                })
                .await;
                Ok(result)
            }
            Ok(Ok(other)) => {
                call.finish(CallOutcome::Error(
                    "The integration returned a non-object result".into(),
                ))
                .await;
                Ok(gateway_failure(
                    &config.name,
                    &GatewayError::new(
                        GatewayErrorKind::Protocol,
                        format!(
                            "The integration returned a {} instead of a tool result.",
                            value_kind(&other)
                        ),
                    ),
                ))
            }
            Ok(Err(error)) => {
                call.finish(CallOutcome::Error(format!(
                    "{} ({})",
                    error.message, error.code
                )))
                .await;
                Err(error.into())
            }
            Err(error) => {
                if cancel.is_cancelled() {
                    drop(call);
                } else {
                    call.finish(CallOutcome::Error(error.describe())).await;
                }
                Ok(gateway_failure(&config.name, &error))
            }
        })
    }

    /// `resources/read`, routed to the integration that listed the URI (or a
    /// template matching it), else the one whose tool result returned it,
    /// else the only integration offering resources.
    pub async fn read_resource(
        &self,
        params: &Map<String, Value>,
        cancel: CancellationToken,
    ) -> Result<Value, CallFailure> {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return Err(CallFailure {
                code: -32602,
                message: "Invalid params for resources/read: uri: expected string".into(),
                data: None,
            });
        };
        let served = self.served();
        let owner = served
            .listed_owner(uri)
            .map(str::to_owned)
            .or_else(|| self.seen_resource_owner(uri))
            .or_else(|| served.template_owner(uri).map(str::to_owned))
            .or_else(|| served.sole_resource_integration().map(str::to_owned));
        let not_found = || CallFailure {
            code: -32002,
            message: format!("Resource not found: {uri}"),
            data: Some(json!({"uri": uri})),
        };
        let runtime = owner
            .and_then(|owner| self.runtime(&owner))
            .filter(|runtime| runtime.config().enabled)
            .ok_or_else(not_found)?;
        let mut forwarded = Map::new();
        forwarded.insert("uri".into(), json!(uri));
        match runtime
            .rpc(
                self.deps(),
                "resources/read",
                forwarded,
                RESOURCE_TIMEOUT,
                &cancel,
                true,
            )
            .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(error.into()),
            Err(error) => Err(CallFailure {
                code: -32603,
                message: format!("{}: {}", runtime.config().name, error.describe()),
                data: Some(json!({ORIGIN_META_KEY: {"error": kind_name(error.kind)}})),
            }),
        }
    }

    // --- administration -------------------------------------------------

    pub async fn create(&self, input: IntegrationCreate) -> Result<IntegrationRow, AdminError> {
        let _admin = self.inner.admin.lock().await;
        let id = input.id.trim().to_owned();
        if let Some(problem) = integration_id_problem(&id) {
            return Err(AdminError::Invalid(format!("The id {problem}.")));
        }
        if self.runtime(&id).is_some() {
            return Err(AdminError::Conflict(format!(
                "An integration named {id} exists."
            )));
        }
        if self.runtimes().len() >= MAX_INTEGRATIONS {
            return Err(AdminError::Invalid(format!(
                "At most {MAX_INTEGRATIONS} integrations."
            )));
        }
        let name = check_name(&input.name)?;
        let transport = match input.transport {
            TransportInput::Http { url } => Transport::Http {
                url: check_url(&url, self.inner.allow_loopback_http)
                    .map_err(AdminError::Invalid)?,
            },
            TransportInput::Stdio { server } => {
                if self.bundled().get(&server).is_none() {
                    return Err(AdminError::Invalid(format!("No bundled server {server}.")));
                }
                Transport::Stdio { server }
            }
        };
        let auth = AuthKind::from_api(input.auth);
        check_auth(&transport, auth)?;
        let now = now_ms(self.deps());
        let row = IntegrationRow {
            id: id.clone(),
            name,
            enabled: input.enabled.unwrap_or(true),
            transport,
            auth,
            timeout_ms: timeout_ms(input.timeout_seconds)?,
            hidden_tools: Vec::new(),
            created_at: now,
            updated_at: now,
            extra: Extra::default(),
        };
        let stored = row.clone();
        self.store()
            .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
            .await?;
        // Leftovers from a deleted integration with the same id never apply.
        self.credentials().clear(&id).await?;
        self.forget_catalog(&id).await?;
        let runtime = Arc::new(Runtime::new(row.clone(), None, None));
        self.inner
            .runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), runtime);
        if row.enabled && (auth == AuthKind::None) {
            self.request_refresh(&id);
        }
        Ok(row)
    }

    pub async fn patch(
        &self,
        id: &str,
        patch: IntegrationPatch,
    ) -> Result<IntegrationRow, AdminError> {
        let _admin = self.inner.admin.lock().await;
        let runtime = self
            .runtime(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        let mut row = runtime.config();
        let before_transport = row.transport.clone();
        let before_origin = row.transport.credential_origin();
        let before_auth = row.auth;
        if let Some(name) = &patch.name {
            row.name = check_name(name)?;
        }
        if let Some(url) = &patch.url {
            match &mut row.transport {
                Transport::Http { url: current } => {
                    *current = check_url(url, self.inner.allow_loopback_http)
                        .map_err(AdminError::Invalid)?;
                }
                Transport::Stdio { .. } => {
                    return Err(AdminError::Invalid(
                        "A stdio integration has no URL.".into(),
                    ));
                }
            }
        }
        if let Some(auth) = patch.auth {
            row.auth = AuthKind::from_api(auth);
            check_auth(&row.transport, row.auth)?;
        }
        if patch.timeout_seconds.is_some() {
            row.timeout_ms = timeout_ms(patch.timeout_seconds)?;
        }
        if let Some(enabled) = patch.enabled {
            row.enabled = enabled;
        }
        row.updated_at = now_ms(self.deps());
        let stored = row.clone();
        self.store()
            .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
            .await?;
        if row.transport.credential_origin() != before_origin || row.auth != before_auth {
            self.credentials().clear(id).await?;
        }
        if row.transport != before_transport {
            self.forget_catalog(id).await?;
        }
        runtime.reconfigure(row.clone(), self.deps());
        self.rebuild(true);
        if row.enabled {
            self.request_refresh(id);
        }
        Ok(row)
    }

    /// Deletes what was learned from an integration's previous transport:
    /// its catalog, health and event feeds.
    /// Forgets what the old server offered. Its feeds keep their
    /// subscriptions but restart from the new server's current cursor (a
    /// recorded gap); the new `created_at` makes a poll still running against
    /// the old server discard its result.
    async fn forget_catalog(&self, id: &str) -> Result<(), StoreError> {
        let key = id.to_owned();
        let now = self.inner.deps.clock.now_ms();
        self.store()
            .write(move |tx| {
                tx.delete::<CatalogRow>(&key)?;
                tx.delete::<HealthRow>(&key)?;
                for mut feed in tx.get_all::<FeedRow>()? {
                    if feed.integration == key {
                        feed.cursor = None;
                        feed.created_at = now;
                        feed.next_poll_at = now;
                        feed.failures = 0;
                        feed.error = None;
                        feed.gaps += 1;
                        feed.last_gap_at = Some(now);
                        tx.upsert(&feed, UpsertOpts::default())?;
                    }
                }
                Ok::<_, StoreError>(())
            })
            .await
    }

    pub async fn delete(&self, id: &str) -> Result<(), AdminError> {
        let _admin = self.inner.admin.lock().await;
        let runtime = self
            .inner
            .runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        runtime.retire(self.deps());
        let key = id.to_owned();
        self.store()
            .write(move |tx| {
                tx.delete::<IntegrationRow>(&key)?;
                tx.delete::<CredentialRow>(&key)?;
                tx.delete::<CatalogRow>(&key)?;
                tx.delete::<HealthRow>(&key)?;
                for feed in tx.get_all::<FeedRow>()? {
                    if feed.integration == key {
                        tx.delete::<FeedRow>(&feed.id)?;
                    }
                }
                Ok::<_, StoreError>(())
            })
            .await?;
        self.rebuild(true);
        Ok(())
    }

    pub async fn set_credentials(
        &self,
        id: &str,
        update: CredentialsUpdate,
    ) -> Result<(), AdminError> {
        let _admin = self.inner.admin.lock().await;
        let runtime = self
            .runtime(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        let row = runtime.config();
        let pairs = |list: &[omni_api::gateway::SecretPair]| -> Vec<(String, String)> {
            list.iter()
                .map(|p| (p.name.trim().to_owned(), p.value.clone()))
                .collect()
        };
        match row.auth {
            AuthKind::Headers => {
                let headers = pairs(&update.headers);
                if headers.is_empty() {
                    return Err(AdminError::Invalid("Add at least one header.".into()));
                }
                validate_headers(&headers).map_err(AdminError::Invalid)?;
                self.credentials()
                    .save_static(&row, headers, Vec::new())
                    .await?;
            }
            AuthKind::Env => {
                let env = pairs(&update.env);
                let allowed = match &row.transport {
                    Transport::Stdio { server } => self
                        .bundled()
                        .get(server)
                        .map(|s| s.env.clone())
                        .unwrap_or_default(),
                    Transport::Http { .. } => Vec::new(),
                };
                let env = validate_env(&env, &allowed).map_err(AdminError::Invalid)?;
                if env.is_empty() {
                    return Err(AdminError::Invalid("Add at least one variable.".into()));
                }
                self.credentials()
                    .save_static(&row, Vec::new(), env)
                    .await?;
            }
            AuthKind::OAuth => {
                return Err(AdminError::Invalid(
                    "OAuth integrations are connected with Connect, not stored values.".into(),
                ));
            }
            AuthKind::None => {
                return Err(AdminError::Invalid(
                    "This integration uses no credentials.".into(),
                ));
            }
        }
        runtime.reset(self.deps());
        if row.enabled {
            self.request_refresh(id);
        }
        Ok(())
    }

    pub async fn clear_credentials(&self, id: &str) -> Result<(), AdminError> {
        let _admin = self.inner.admin.lock().await;
        let runtime = self
            .runtime(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        self.credentials().clear(id).await?;
        runtime.reset(self.deps());
        if runtime.config().auth != AuthKind::None {
            let missing = GatewayError::new(
                GatewayErrorKind::AuthRequired,
                "The integration's credentials are not set.",
            );
            runtime.note_failure(self.deps(), &missing).await;
        }
        Ok(())
    }

    pub async fn set_hidden(
        &self,
        id: &str,
        hidden: Vec<String>,
    ) -> Result<IntegrationRow, AdminError> {
        let _admin = self.inner.admin.lock().await;
        let runtime = self
            .runtime(id)
            .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for name in hidden {
            if name.len() <= 128 && seen.insert(name.clone()) {
                unique.push(name);
            }
        }
        if unique.len() > catalog::MAX_TOOLS {
            return Err(AdminError::Invalid("Too many tool names.".into()));
        }
        let mut row = runtime.config();
        row.hidden_tools = unique;
        row.updated_at = now_ms(self.deps());
        let stored = row.clone();
        self.store()
            .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
            .await?;
        runtime.reconfigure(row.clone(), self.deps());
        self.rebuild(true);
        Ok(row)
    }
}

fn check_name(name: &str) -> Result<String, AdminError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 || name.contains(char::is_control) {
        return Err(AdminError::Invalid(
            "The name needs 1 to 60 characters.".into(),
        ));
    }
    Ok(name.to_owned())
}

fn check_auth(transport: &Transport, auth: AuthKind) -> Result<(), AdminError> {
    match (transport, auth) {
        (Transport::Http { .. }, AuthKind::Env) => Err(AdminError::Invalid(
            "Environment variables apply only to bundled stdio servers.".into(),
        )),
        (Transport::Stdio { .. }, AuthKind::Headers | AuthKind::OAuth) => Err(AdminError::Invalid(
            "Bundled stdio servers authenticate with environment variables.".into(),
        )),
        _ => Ok(()),
    }
}

fn timeout_ms(seconds: Option<u32>) -> Result<i64, AdminError> {
    match seconds {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(s) if (MIN_TIMEOUT_S..=MAX_TIMEOUT_S).contains(&s) => Ok(i64::from(s) * 1_000),
        Some(_) => Err(AdminError::Invalid(format!(
            "The timeout must be {MIN_TIMEOUT_S} to {MAX_TIMEOUT_S} seconds."
        ))),
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// The first text block of an `isError` result, for activity.
fn tool_error_text(result: &Map<String, Value>) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("text").and_then(Value::as_str))
        })
        .unwrap_or("The tool reported an error")
        .chars()
        .take(500)
        .collect()
}

/// An `isError` tool result for a failure inside the gateway.
pub fn gateway_failure(integration_name: &str, error: &GatewayError) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert(
        "content".into(),
        json!([{"type": "text", "text": format!("{integration_name}: {}", error.describe())}]),
    );
    result.insert("isError".into(), json!(true));
    result.insert(
        "_meta".into(),
        json!({ORIGIN_META_KEY: {"error": kind_name(error.kind)}}),
    );
    result
}
