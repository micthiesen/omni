//! MCP gateway management (`/api/gateway/*`, LAN only; see `docs/gateway.md`).
//!
//! - `GET /api/gateway`: [`GatewayOverview`];
//! - `POST /api/gateway/integrations`: [`IntegrationCreate`] -> [`IntegrationDetail`];
//! - `GET|PATCH|DELETE /api/gateway/integrations/:id` ([`IntegrationPatch`]);
//! - `PUT|DELETE /api/gateway/integrations/:id/credentials` ([`CredentialsUpdate`]);
//! - `POST /api/gateway/integrations/:id/refresh`;
//! - `POST /api/gateway/integrations/:id/oauth/start` -> [`OAuthStartResponse`];
//! - `PUT /api/gateway/integrations/:id/tools` ([`ToolVisibilityUpdate`]);
//! - `POST /api/gateway/clients` ([`ClientTokenCreate`]) -> [`ClientTokenCreated`];
//! - `DELETE /api/gateway/clients/:id`, `DELETE /api/gateway/grants/:id`;
//! - `POST /api/gateway/authorizations/:id/approve` ([`AuthorizationApproval`])
//!   and `POST /api/gateway/authorizations/:id/deny`.
//!
//! Credentials are write-only: views report header and variable names and
//! whether values are set, never the values. A client token appears once, in
//! [`ClientTokenCreated`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The downstream MCP endpoint path on every host.
pub const GATEWAY_MCP_PATH: &str = "/gateway/mcp";

/// `GET /api/gateway`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOverview {
    /// `OMNI_GATEWAY_PUBLIC_URL`: the public HTTPS endpoint, when configured.
    pub public_url: Option<String>,
    /// [`GATEWAY_MCP_PATH`]; the LAN endpoint is this path on the UI's origin.
    pub path: String,
    /// Downstream OAuth (ChatGPT) needs the public URL.
    pub oauth_available: bool,
    /// Omni's own tools served alongside the integrations.
    pub native_tools: u32,
    /// Every tool `tools/list` serves now (native plus visible integration tools).
    pub exposed_tools: u32,
    pub integrations: Vec<IntegrationSummary>,
    pub clients: Vec<ClientTokenView>,
    pub grants: Vec<OAuthGrantView>,
    pub pending: Vec<PendingAuthorizationView>,
    pub event_feeds: Vec<EventFeedView>,
    /// Stdio servers bundled in the image that an integration can run.
    pub bundled_servers: Vec<BundledServerView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportKind {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "stdio")]
    Stdio,
}

/// How Omni authenticates to an integration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    #[default]
    #[serde(rename = "none")]
    None,
    /// Static request headers, such as `Authorization: Bearer <key>`.
    #[serde(rename = "headers")]
    Headers,
    /// OAuth 2.1 authorization code with PKCE, connected from the UI.
    #[serde(rename = "oauth")]
    OAuth,
    /// Environment variables for a bundled stdio server.
    #[serde(rename = "env")]
    Env,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntegrationStatus {
    #[serde(rename = "disabled")]
    Disabled,
    /// Enabled, no catalog fetched yet.
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "ready")]
    Ready,
    /// The last refresh failed; the previous catalog is still served.
    #[serde(rename = "stale")]
    Stale,
    /// Credentials are missing, expired or rejected.
    #[serde(rename = "needs_auth")]
    NeedsAuth,
    /// The last refresh failed and there is no catalog to serve.
    #[serde(rename = "error")]
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayErrorKind {
    #[serde(rename = "network")]
    Network,
    #[serde(rename = "timeout")]
    Timeout,
    #[serde(rename = "auth_required")]
    AuthRequired,
    #[serde(rename = "auth_rejected")]
    AuthRejected,
    #[serde(rename = "protocol")]
    Protocol,
    /// A JSON-RPC error from the integration.
    #[serde(rename = "upstream")]
    Upstream,
    /// A bundled stdio server failed to start or exited.
    #[serde(rename = "process")]
    Process,
    #[serde(rename = "config")]
    Config,
    #[serde(rename = "unsupported")]
    Unsupported,
}

/// A failure with a suggested next step. Messages never contain credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayErrorView {
    pub kind: GatewayErrorKind,
    pub message: String,
    pub hint: Option<String>,
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationSummary {
    /// Gateway-owned id; tool names are `<id>__<tool>`.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: TransportKind,
    /// The URL, or the bundled server's name.
    pub endpoint: String,
    pub auth: AuthKind,
    pub status: IntegrationStatus,
    pub tool_count: u32,
    pub visible_tool_count: u32,
    pub resource_count: u32,
    pub event_count: u32,
    pub last_refresh_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub error: Option<GatewayErrorView>,
}

/// `GET /api/gateway/integrations/:id` and every integration mutation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationDetail {
    pub summary: IntegrationSummary,
    pub config: IntegrationConfigView,
    pub server: Option<UpstreamServerView>,
    pub tools: Vec<GatewayToolView>,
    /// At most 100; `resource_count` on the summary is the total.
    pub resources: Vec<GatewayResourceView>,
    pub events: Vec<UpstreamEventView>,
    pub last_refresh: Option<RefreshReport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationConfigView {
    pub url: Option<String>,
    /// The bundled stdio server id.
    pub bundled: Option<String>,
    /// Header names with stored values.
    pub header_names: Vec<String>,
    /// Environment variable names with stored values.
    pub env_names: Vec<String>,
    /// Whether every credential the auth kind needs is stored.
    pub credentials_set: bool,
    pub oauth: Option<OAuthConnectionView>,
    pub timeout_seconds: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConnectionView {
    pub connected: bool,
    pub issuer: Option<String>,
    pub scope: Option<String>,
    pub access_expires_at: Option<i64>,
    /// A refresh token is stored.
    pub refreshable: bool,
    pub connected_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamServerView {
    pub name: Option<String>,
    pub version: Option<String>,
    pub protocol_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChange {
    #[serde(rename = "added")]
    Added,
    #[serde(rename = "changed")]
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayToolView {
    /// The integration's own name.
    pub name: String,
    /// The name downstream clients call.
    pub exposed_name: String,
    pub title: Option<String>,
    /// At most 400 characters.
    pub description: Option<String>,
    pub visible: bool,
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub open_world: Option<bool>,
    /// Set when the last refresh added or changed the tool.
    pub change: Option<ToolChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayResourceView {
    pub uri: String,
    pub name: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamEventView {
    pub name: String,
    pub exposed_name: String,
    pub description: Option<String>,
    /// Offered downstream (the integration supports `events/poll`).
    pub supported: bool,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshReport {
    pub at: i64,
    pub ok: bool,
    pub duration_ms: i64,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
}

/// `POST /api/gateway/integrations`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationCreate {
    pub id: String,
    pub name: String,
    pub transport: TransportInput,
    #[serde(default)]
    pub auth: AuthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TransportInput {
    #[serde(rename = "http")]
    Http { url: String },
    #[serde(rename = "stdio")]
    Stdio { server: String },
}

/// `PATCH /api/gateway/integrations/:id`: absent fields are unchanged.
/// Changing the URL's origin or the auth kind clears stored credentials.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `PUT /api/gateway/integrations/:id/credentials`: replaces the stored
/// headers (auth `headers`) or variables (auth `env`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsUpdate {
    #[serde(default)]
    pub headers: Vec<SecretPair>,
    #[serde(default)]
    pub env: Vec<SecretPair>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretPair {
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for SecretPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretPair")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// `PUT /api/gateway/integrations/:id/tools`: the integration tool names to hide.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolVisibilityUpdate {
    pub hidden: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStartResponse {
    pub authorize_url: String,
}

/// A downstream bearer token (Claude Code, Codex).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenView {
    pub id: String,
    pub label: String,
    /// The token's first characters, to tell tokens apart.
    pub prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenCreate {
    pub label: String,
}

/// The only response that carries a client token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenCreated {
    pub client: ClientTokenView,
    pub token: String,
}

impl std::fmt::Debug for ClientTokenCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTokenCreated")
            .field("client", &self.client)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// An approved downstream OAuth client (ChatGPT).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthGrantView {
    pub id: String,
    pub client_name: String,
    pub client_id: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub scope: Option<String>,
}

/// A downstream OAuth request waiting for the owner's approval. Its user
/// code is shown only on the sign-in page; approving requires typing it, so
/// a request someone else started cannot be approved by mistake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAuthorizationView {
    pub id: String,
    /// Chosen by the app itself; trust `redirect_host` and `verified` instead.
    pub client_name: String,
    pub client_id: String,
    pub redirect_host: String,
    /// The app is a known one (ChatGPT, Claude) identified by its metadata
    /// document, not a self-registered client.
    pub verified: bool,
    pub created_at: i64,
    pub expires_at: i64,
}

/// `POST /api/gateway/authorizations/:id/approve`: the code from the sign-in page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationApproval {
    pub user_code: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedState {
    /// Polling for current subscribers.
    #[serde(rename = "active")]
    Active,
    /// No current subscriber.
    #[serde(rename = "idle")]
    Idle,
    /// The integration is disabled or needs authentication; the cursor is kept.
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "error")]
    Error,
}

/// Omni's subscription to one integration event with one argument set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFeedView {
    pub id: String,
    pub integration: String,
    pub event: String,
    pub upstream_event: String,
    pub arguments: BTreeMap<String, String>,
    pub state: FeedState,
    pub subscribers: u32,
    pub last_poll_at: Option<i64>,
    pub next_poll_at: Option<i64>,
    pub last_event_at: Option<i64>,
    pub published: u64,
    /// Polls where the integration could not replay everything since the cursor.
    pub gaps: u32,
    pub last_gap_at: Option<i64>,
    pub error: Option<GatewayErrorView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundledServerView {
    pub id: String,
    pub name: String,
    pub package: String,
    /// Variables the server needs (set as credentials).
    pub env: Vec<String>,
    /// Installed in this image.
    pub available: bool,
}
