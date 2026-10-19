//! The gateway's management API (`/api/gateway/*`, see `omni_api::gateway`)
//! and the upstream OAuth callback (`GET /gateway/oauth/callback`).
//!
//! `/api/*` is LAN-only (the public proxy forwards only `/gateway/*` and
//! the OAuth well-known documents) and same-origin for mutations. Because
//! these routes mint internet-valid tokens and approve sign-ins, their
//! mutations also need an `Origin` and a LAN `Host` ([`lan_only`]), which
//! defeats DNS rebinding. Views carry header and variable names but never
//! their values.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, LOCATION, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use omni_api::gateway::{
    AuthorizationApproval, BundledServerView, ClientTokenCreate, ClientTokenCreated,
    ClientTokenView, CredentialsUpdate, EventFeedView, FeedState, GATEWAY_MCP_PATH,
    GatewayOverview, GatewayResourceView, GatewayToolView, IntegrationConfigView,
    IntegrationCreate, IntegrationDetail, IntegrationPatch, IntegrationStatus, IntegrationSummary,
    OAuthConnectionView, OAuthGrantView, OAuthStartResponse, PendingAuthorizationView,
    RefreshReport, ToolChange, ToolVisibilityUpdate, TransportKind, UpstreamEventView,
    UpstreamServerView,
};
use omni_server_kit::{ApiError, ApiResult, JsonBody};
use omni_store::entity::EntityOps as _;
use serde_json::Value;

use super::access::{DownstreamAccess, OWNER_PREFIX};
use super::connect;
use super::error::view;
use super::model::{FeedRow, Transport};
use super::oauth::client::OAuthClient;
use super::oauth::server::{Approval, AuthorizationServer};
use super::runtime::Runtime;
use super::service::{AdminError, Gateway};
use crate::events::service::McpEventService;

const LOG: &str = "MCP:Gateway";
const DESCRIPTION_LIMIT: usize = 400;
const MAX_RESOURCES_SHOWN: usize = 100;

#[derive(Clone)]
pub struct AdminState {
    pub gateway: Gateway,
    pub access: DownstreamAccess,
    pub events: Option<McpEventService>,
    pub oauth: OAuthClient,
    pub authorization: Option<AuthorizationServer>,
    /// The public origin (`https://host`) when `OMNI_GATEWAY_PUBLIC_URL` is set.
    pub public_url: Option<String>,
}

fn admin_error(error: AdminError) -> ApiError {
    match error {
        AdminError::Invalid(message) => ApiError::bad_request(message),
        AdminError::NotFound(message) => ApiError::not_found(message),
        AdminError::Conflict(message) => ApiError::conflict(message),
        AdminError::Store(error) => ApiError::internal(error),
        AdminError::Gateway(error) => ApiError::new(StatusCode::BAD_GATEWAY, error.describe()),
    }
}

pub fn router(state: AdminState) -> Router {
    let guard_state = state.clone();
    let api = Router::new()
        .route("/api/gateway", get(overview))
        .route("/api/gateway/integrations", post(create))
        .route(
            "/api/gateway/integrations/{id}",
            get(detail).patch(update).delete(remove),
        )
        .route(
            "/api/gateway/integrations/{id}/credentials",
            put(set_credentials).delete(clear_credentials),
        )
        .route("/api/gateway/integrations/{id}/refresh", post(refresh))
        .route(
            "/api/gateway/integrations/{id}/oauth/start",
            post(oauth_start),
        )
        .route("/api/gateway/integrations/{id}/tools", put(set_tools))
        .route("/api/gateway/clients", post(create_client))
        .route("/api/gateway/clients/{id}", delete(revoke_client))
        .route("/api/gateway/grants/{id}", delete(revoke_grant))
        .route("/api/gateway/authorizations/{id}/approve", post(approve))
        .route("/api/gateway/authorizations/{id}/deny", post(deny))
        .route_layer(axum::middleware::from_fn_with_state(guard_state, lan_only))
        .with_state(state.clone());
    api.merge(
        Router::new()
            .route(connect::CALLBACK_PATH, get(oauth_callback))
            .with_state(state),
    )
}

/// Whether `host` (without port) names Omni on the home network: an IP
/// literal, `localhost`, a `.boris` or `.local` name, or `omni.syas.ca` (the
/// LAN-only proxy host), but never the public gateway host. Other
/// `syas.ca` names such as `pods.syas.ca` are public.
fn lan_host(host: &str, public_url: Option<&str>) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) && !name.ends_with(':') => {
            name
        }
        _ => host,
    };
    let name = name
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let public = public_url
        .and_then(|url| url::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    if public.as_deref() == Some(name.as_str()) {
        return false;
    }
    name.parse::<std::net::IpAddr>().is_ok()
        || name == "localhost"
        || name == "omni.syas.ca"
        || [".boris", ".local"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Mutations of the management API need an `Origin` and a LAN `Host`.
async fn lan_only(
    State(state): State<AdminState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mutating = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    if mutating {
        let headers = request.headers();
        let host = headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if headers.get(ORIGIN).is_none() || !lan_host(host, state.public_url.as_deref()) {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "Change the MCP gateway from Omni's page on the home network.",
            )
            .into_response();
        }
    }
    next.run(request).await
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn summary(runtime: &Runtime) -> IntegrationSummary {
    let config = runtime.config();
    let catalog = runtime.catalog();
    let health = runtime.health();
    let (transport, endpoint) = match &config.transport {
        Transport::Http { url } => (TransportKind::Http, url.clone()),
        Transport::Stdio { server } => (TransportKind::Stdio, server.clone()),
    };
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let (tools, visible, resources, events) = catalog.as_ref().map_or((0, 0, 0, 0), |c| {
        (
            c.tools.len(),
            c.tools
                .iter()
                .filter(|t| !config.hidden_tools.contains(&t.upstream))
                .count(),
            c.resources.len(),
            c.events.iter().filter(|e| e.pollable).count(),
        )
    });
    IntegrationSummary {
        id: config.id.clone(),
        name: config.name.clone(),
        enabled: config.enabled,
        transport,
        endpoint,
        auth: config.auth.to_api(),
        status: runtime.status(),
        tool_count: count(tools),
        visible_tool_count: count(visible),
        resource_count: count(resources),
        event_count: count(events),
        last_refresh_at: health.last_refresh.as_ref().map(|r| r.at),
        last_success_at: health.last_success_at,
        error: if config.enabled {
            health.error.as_ref().map(view)
        } else {
            None
        },
    }
}

async fn detail_of(state: &AdminState, runtime: &Runtime) -> Result<IntegrationDetail, ApiError> {
    let config = runtime.config();
    let catalog = runtime.catalog();
    let health = runtime.health();
    let credentials = state.gateway.credentials();
    let current = credentials
        .current(&config)
        .await
        .map_err(ApiError::internal)?;
    let complete = credentials
        .complete(&config)
        .await
        .map_err(ApiError::internal)?;
    let (header_names, env_names, oauth) = match &current {
        Some((row, _)) => (
            row.header_names.clone(),
            row.env_names.clone(),
            row.oauth.as_ref().map(|meta| OAuthConnectionView {
                connected: true,
                issuer: Some(meta.issuer.clone()),
                scope: meta.scope.clone(),
                access_expires_at: meta.access_expires_at,
                refreshable: meta.refreshable,
                connected_at: Some(meta.connected_at),
            }),
        ),
        None => (Vec::new(), Vec::new(), None),
    };
    let oauth = match (config.auth, oauth) {
        (super::model::AuthKind::OAuth, None) => Some(OAuthConnectionView {
            connected: false,
            issuer: None,
            scope: None,
            access_expires_at: None,
            refreshable: false,
            connected_at: None,
        }),
        (_, oauth) => oauth,
    };
    let (url, bundled) = match &config.transport {
        Transport::Http { url } => (Some(url.clone()), None),
        Transport::Stdio { server } => (None, Some(server.clone())),
    };
    let tools = catalog
        .as_ref()
        .map(|c| {
            c.tools
                .iter()
                .map(|tool| GatewayToolView {
                    name: tool.upstream.clone(),
                    exposed_name: tool.exposed.clone(),
                    title: tool.title().map(str::to_owned),
                    description: tool.description().map(|d| truncate(d, DESCRIPTION_LIMIT)),
                    visible: !config.hidden_tools.contains(&tool.upstream),
                    read_only: tool.read_only(),
                    destructive: tool.destructive(),
                    open_world: tool.open_world(),
                    change: if health.added.contains(&tool.upstream) {
                        Some(ToolChange::Added)
                    } else if health.changed.contains(&tool.upstream) {
                        Some(ToolChange::Changed)
                    } else {
                        None
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let resources = catalog
        .as_ref()
        .map(|c| {
            c.resources
                .iter()
                .take(MAX_RESOURCES_SHOWN)
                .filter_map(|r| {
                    Some(GatewayResourceView {
                        uri: r.get("uri")?.as_str()?.to_owned(),
                        name: r.get("name").and_then(Value::as_str).map(str::to_owned),
                        mime_type: r.get("mimeType").and_then(Value::as_str).map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let events = catalog
        .as_ref()
        .map(|c| {
            c.events
                .iter()
                .map(|event| UpstreamEventView {
                    name: event.upstream.clone(),
                    exposed_name: event.exposed.clone(),
                    description: event
                        .definition
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|d| truncate(d, DESCRIPTION_LIMIT)),
                    supported: event.pollable,
                    note: (!event.pollable).then(|| {
                        "Omni follows events through events/poll, which this event does not offer."
                            .to_owned()
                    }),
                })
                .collect()
        })
        .unwrap_or_default();
    let server = catalog.as_ref().map(|c| UpstreamServerView {
        name: c
            .server_info()
            .and_then(|i| i.get("title").or_else(|| i.get("name")))
            .and_then(Value::as_str)
            .map(str::to_owned),
        version: c
            .server_info()
            .and_then(|i| i.get("version"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        protocol_version: c.protocol_version().map(str::to_owned),
    });
    Ok(IntegrationDetail {
        summary: summary(runtime),
        config: IntegrationConfigView {
            url,
            bundled,
            header_names,
            env_names,
            credentials_set: complete,
            oauth,
            timeout_seconds: u32::try_from(config.timeout_ms / 1_000).unwrap_or(120),
        },
        server,
        tools,
        resources,
        events,
        last_refresh: health.last_refresh.map(|r| RefreshReport {
            at: r.at,
            ok: r.ok,
            duration_ms: r.duration_ms,
            added: r.added,
            removed: r.removed,
            changed: r.changed,
            warnings: r.warnings,
        }),
    })
}

async fn detail_by_id(state: &AdminState, id: &str) -> ApiResult<IntegrationDetail> {
    let runtime = state
        .gateway
        .runtime(id)
        .ok_or_else(|| ApiError::not_found(format!("No integration {id}")))?;
    detail_of(state, &runtime).await.map(Json)
}

async fn feeds(state: &AdminState) -> Result<Vec<EventFeedView>, ApiError> {
    let rows = state
        .gateway
        .store()
        .read(|docs| docs.get_all::<FeedRow>())
        .await
        .map_err(ApiError::internal)?;
    let mut views = Vec::new();
    let mut active: HashMap<String, Vec<crate::events::catalog::EventArguments>> = HashMap::new();
    for feed in rows {
        if !active.contains_key(&feed.event)
            && let Some(events) = &state.events
        {
            let arguments = events
                .active_arguments(&feed.event)
                .await
                .map_err(ApiError::internal)?;
            active.insert(feed.event.clone(), arguments);
        }
        let subscribers = active.get(&feed.event).map_or(0, |list| {
            list.iter().filter(|a| **a == feed.arguments).count()
        });
        let runtime = state.gateway.runtime(&feed.integration);
        let paused = runtime
            .as_ref()
            .is_none_or(|r| !r.config().enabled || r.status() == IntegrationStatus::NeedsAuth);
        let feed_state = if paused {
            FeedState::Paused
        } else if feed.error.is_some() {
            FeedState::Error
        } else if subscribers == 0 {
            FeedState::Idle
        } else {
            FeedState::Active
        };
        views.push(EventFeedView {
            id: feed.id.clone(),
            integration: feed.integration.clone(),
            event: feed.event.clone(),
            upstream_event: feed.upstream_event.clone(),
            arguments: feed
                .arguments
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            state: feed_state,
            subscribers: u32::try_from(subscribers).unwrap_or(u32::MAX),
            last_poll_at: feed.last_poll_at,
            next_poll_at: (subscribers > 0).then_some(feed.next_poll_at),
            last_event_at: feed.last_event_at,
            published: u64::try_from(feed.published).unwrap_or(0),
            gaps: u32::try_from(feed.gaps).unwrap_or(u32::MAX),
            last_gap_at: feed.last_gap_at,
            error: feed.error.as_ref().map(view),
        });
    }
    views.sort_by(|a, b| (&a.integration, &a.event).cmp(&(&b.integration, &b.event)));
    Ok(views)
}

async fn overview(State(state): State<AdminState>) -> ApiResult<GatewayOverview> {
    let integrations: Vec<IntegrationSummary> = state
        .gateway
        .runtimes()
        .iter()
        .map(|r| summary(r))
        .collect();
    let clients = state
        .access
        .clients()
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|row| ClientTokenView {
            id: row.id,
            label: row.label,
            prefix: row.prefix,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        })
        .collect();
    let grants = state
        .access
        .grants()
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|row| OAuthGrantView {
            id: row.id,
            client_name: row.client_name,
            client_id: row.client_id,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            scope: row.scope,
        })
        .collect();
    let pending = match &state.authorization {
        Some(server) => server
            .pending()
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .map(|row| PendingAuthorizationView {
                redirect_host: url::Url::parse(&row.redirect_uri)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default(),
                verified: super::oauth::server::is_verified_client(&row),
                id: row.id,
                client_name: row.client_name,
                client_id: row.client_id,
                created_at: row.created_at,
                expires_at: row.expires_at,
            })
            .collect(),
        None => Vec::new(),
    };
    let native = state.gateway.native_tool_count();
    let exposed = native
        .saturating_add(u32::try_from(state.gateway.served().tools.len()).unwrap_or(u32::MAX));
    Ok(Json(GatewayOverview {
        public_url: state
            .public_url
            .as_ref()
            .map(|origin| format!("{origin}{GATEWAY_MCP_PATH}")),
        path: GATEWAY_MCP_PATH.to_owned(),
        oauth_available: state.authorization.is_some(),
        native_tools: native,
        exposed_tools: exposed,
        integrations,
        clients,
        grants,
        pending,
        event_feeds: feeds(&state).await?,
        bundled_servers: state
            .gateway
            .bundled()
            .servers()
            .iter()
            .map(|server| BundledServerView {
                id: server.id.clone(),
                name: server.name.clone(),
                package: server.package.clone(),
                env: server.env.clone(),
                available: server.available(),
            })
            .collect(),
    }))
}

async fn create(
    State(state): State<AdminState>,
    JsonBody(body): JsonBody<IntegrationCreate>,
) -> Result<(StatusCode, Json<IntegrationDetail>), ApiError> {
    let row = state.gateway.create(body).await.map_err(admin_error)?;
    let Json(detail) = detail_by_id(&state, &row.id).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

async fn detail(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<IntegrationDetail> {
    detail_by_id(&state, &id).await
}

async fn update(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<IntegrationPatch>,
) -> ApiResult<IntegrationDetail> {
    state.gateway.patch(&id, body).await.map_err(admin_error)?;
    detail_by_id(&state, &id).await
}

async fn remove(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.gateway.delete(&id).await.map_err(admin_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_credentials(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<CredentialsUpdate>,
) -> ApiResult<IntegrationDetail> {
    state
        .gateway
        .set_credentials(&id, body)
        .await
        .map_err(admin_error)?;
    // Show whether the new credentials work; a slow integration finishes in
    // the background refresh that saving also queued.
    let refreshed = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        state.gateway.refresh_now(&id),
    )
    .await;
    if let Ok(Err(error)) = refreshed {
        tracing::debug!(target: LOG, %error, "Refresh after saving credentials failed");
    }
    detail_by_id(&state, &id).await
}

async fn clear_credentials(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<IntegrationDetail> {
    state
        .gateway
        .clear_credentials(&id)
        .await
        .map_err(admin_error)?;
    detail_by_id(&state, &id).await
}

async fn refresh(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<IntegrationDetail> {
    state.gateway.refresh_now(&id).await.map_err(admin_error)?;
    detail_by_id(&state, &id).await
}

async fn set_tools(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ToolVisibilityUpdate>,
) -> ApiResult<IntegrationDetail> {
    state
        .gateway
        .set_hidden(&id, body.hidden)
        .await
        .map_err(admin_error)?;
    detail_by_id(&state, &id).await
}

/// The UI's own origin (`scheme://host[:port]`), from `Origin` or `Host`.
fn ui_origin(headers: &HeaderMap) -> Option<String> {
    if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok())
        && let Ok(url) = url::Url::parse(origin)
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
    {
        return Some(url.origin().ascii_serialization());
    }
    let host = headers.get(HOST).and_then(|v| v.to_str().ok())?;
    let url = url::Url::parse(&format!("http://{host}")).ok()?;
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

async fn oauth_start(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<OAuthStartResponse> {
    let ui = ui_origin(&headers).ok_or_else(|| ApiError::bad_request("Unknown UI origin"))?;
    let callback = connect::redirect_uri(state.public_url.as_deref().unwrap_or(&ui));
    let return_to = format!("{ui}/gateway?integration={id}");
    let authorize_url = connect::start(&state.gateway, &state.oauth, &id, &callback, &return_to)
        .await
        .map_err(admin_error)?;
    Ok(Json(OAuthStartResponse { authorize_url }))
}

fn page(status: StatusCode, title: &str, message: &str) -> Response {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>body{{background:#0b0d10;color:#e6e8eb;font:16px/1.5 system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh}}main{{max-width:28rem;padding:2rem}}h1{{font-size:1.25rem}}</style><main><h1>{}</h1><p>{}</p></main></html>",
        escape(title),
        escape(title),
        escape(message)
    );
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

async fn oauth_callback(
    State(state): State<AdminState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let finished = connect::finish(&state.gateway, &state.oauth, &query).await;
    let Some(return_to) = finished.return_to else {
        return page(
            StatusCode::BAD_REQUEST,
            "Connection failed",
            finished
                .error
                .as_deref()
                .unwrap_or("The connection could not be completed."),
        );
    };
    let target = match &finished.error {
        None => format!("{return_to}&oauth=connected"),
        Some(message) => {
            let encoded: String =
                url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
            format!("{return_to}&oauth=error&message={encoded}")
        }
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(value) = HeaderValue::from_str(&target) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn create_client(
    State(state): State<AdminState>,
    JsonBody(body): JsonBody<ClientTokenCreate>,
) -> Result<(StatusCode, Json<ClientTokenCreated>), ApiError> {
    let label = body.label.trim();
    if label.is_empty() || label.chars().count() > 60 || label.contains(char::is_control) {
        return Err(ApiError::bad_request("The label needs 1 to 60 characters."));
    }
    if state
        .access
        .clients()
        .await
        .map_err(ApiError::internal)?
        .len()
        >= 20
    {
        return Err(ApiError::bad_request("At most 20 client tokens."));
    }
    let (row, token) = state
        .access
        .create_client(label)
        .await
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(ClientTokenCreated {
            client: ClientTokenView {
                id: row.id,
                label: row.label,
                prefix: row.prefix,
                created_at: row.created_at,
                last_used_at: row.last_used_at,
            },
            token,
        }),
    ))
}

/// A revoked principal's event subscriptions end with it.
async fn forget_owner(state: &AdminState, owner: &str) {
    if let Some(events) = &state.events
        && let Err(error) = events.remove_owner(owner).await
    {
        tracing::warn!(target: LOG, %error, "Revoked principal's subscriptions not removed");
    }
}

async fn revoke_client(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .access
        .revoke_client(&id)
        .await
        .map_err(ApiError::internal)?
    {
        forget_owner(&state, &format!("{OWNER_PREFIX}client:{id}")).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("No such client token"))
    }
}

async fn revoke_grant(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .access
        .revoke_grant(&id)
        .await
        .map_err(ApiError::internal)?
    {
        forget_owner(&state, &format!("{OWNER_PREFIX}grant:{id}")).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("No such grant"))
    }
}

fn server(state: &AdminState) -> Result<&AuthorizationServer, ApiError> {
    state
        .authorization
        .as_ref()
        .ok_or_else(|| ApiError::not_found("Downstream OAuth is not configured"))
}

async fn approve(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<AuthorizationApproval>,
) -> Result<StatusCode, ApiError> {
    match server(&state)?
        .approve(&id, &body.user_code)
        .await
        .map_err(ApiError::internal)?
    {
        Approval::Approved => Ok(StatusCode::NO_CONTENT),
        Approval::WrongCode => Err(ApiError::bad_request(
            "That code does not match the one on the sign-in page.",
        )),
        Approval::NotFound => Err(ApiError::not_found(
            "No pending authorization with that id; it may have expired.",
        )),
    }
}

async fn deny(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if server(&state)?
        .deny(&id)
        .await
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("No pending authorization with that id"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_home_network_hosts_manage_the_gateway() {
        let public = Some("https://gateway.syas.ca");
        for host in [
            "omni.boris",
            "10.10.1.100:8080",
            "localhost:38080",
            "[::1]:80",
            "omni.syas.ca",
            "mac.local",
        ] {
            assert!(lan_host(host, public), "{host}");
        }
        for host in [
            "gateway.syas.ca",
            "evil.example",
            "omni.boris.evil.example",
            "syas.ca.evil.example",
            "pods.syas.ca",
            "",
        ] {
            assert!(!lan_host(host, public), "{host}");
        }
    }
}
