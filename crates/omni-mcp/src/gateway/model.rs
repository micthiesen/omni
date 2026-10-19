//! Gateway durable state. Every entity keeps unknown fields in `extra`.
//!
//! Upstream catalogs are stored as JSON text so tool schemas round-trip
//! byte for byte; secrets are stored only sealed ([`super::secrets`]).

use indexmap::IndexMap;
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityDescriptor};
use serde::{Deserialize, Serialize};

/// How Omni reaches an integration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Transport {
    #[serde(rename = "http")]
    Http { url: String },
    /// A stdio server bundled in the image ([`super::bundled`]).
    #[serde(rename = "stdio")]
    Stdio { server: String },
}

impl Transport {
    /// The origin stored credentials are bound to.
    pub fn credential_origin(&self) -> String {
        match self {
            Transport::Http { url } => url::Url::parse(url)
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_else(|_| url.clone()),
            Transport::Stdio { server } => format!("stdio:{server}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "headers")]
    Headers,
    #[serde(rename = "oauth")]
    OAuth,
    #[serde(rename = "env")]
    Env,
}

impl AuthKind {
    pub fn from_api(kind: omni_api::gateway::AuthKind) -> Self {
        use omni_api::gateway::AuthKind as A;
        match kind {
            A::None => AuthKind::None,
            A::Headers => AuthKind::Headers,
            A::OAuth => AuthKind::OAuth,
            A::Env => AuthKind::Env,
        }
    }

    pub fn to_api(self) -> omni_api::gateway::AuthKind {
        use omni_api::gateway::AuthKind as A;
        match self {
            AuthKind::None => A::None,
            AuthKind::Headers => A::Headers,
            AuthKind::OAuth => A::OAuth,
            AuthKind::Env => A::Env,
        }
    }
}

/// `mcp-gateway-integration`, keyed by `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: Transport,
    pub auth: AuthKind,
    pub timeout_ms: i64,
    /// Upstream tool names that are not served.
    #[serde(default)]
    pub hidden_tools: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for IntegrationRow {
    const NAME: &'static str = "mcp-gateway-integration";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// Non-secret facts about a stored OAuth connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthMeta {
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    /// The protected resource the tokens are for (RFC 8707).
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_expires_at: Option<i64>,
    pub refreshable: bool,
    pub connected_at: i64,
    /// `client_secret_basic`, `client_secret_post` or `none`.
    pub token_auth: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `mcp-gateway-credential`, keyed by `integration`. `sealed` holds a
/// [`CredentialSecrets`] JSON sealed for `credential:<integration>`; the
/// names and OAuth facts are readable for the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRow {
    pub integration: String,
    /// The transport origin the credentials were stored for; a different
    /// origin never receives them.
    pub origin: String,
    pub auth: AuthKind,
    pub sealed: String,
    #[serde(default)]
    pub header_names: Vec<String>,
    #[serde(default)]
    pub env_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthMeta>,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for CredentialRow {
    const NAME: &'static str = "mcp-gateway-credential";
    type Key = String;
    fn key(&self) -> String {
        self.integration.clone()
    }
}

/// The sealed part of a [`CredentialRow`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSecrets {
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for CredentialSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSecrets")
            .field(
                "headers",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("env", &self.env.iter().map(|(n, _)| n).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A failure recorded for the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorRow {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The outcome of one catalog refresh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRow {
    pub at: i64,
    pub ok: bool,
    pub duration_ms: i64,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `mcp-gateway-catalog`, keyed by `integration`: the last fetched catalog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRow {
    pub integration: String,
    pub fetched_at: i64,
    /// `initialize` result: `serverInfo`, `protocolVersion`, `capabilities`.
    pub server_json: String,
    /// `tools/list` entries in upstream order, as received.
    pub tools_json: String,
    #[serde(default)]
    pub resources_json: String,
    #[serde(default)]
    pub templates_json: String,
    /// `events/list` entries, when the integration offers MCP Events.
    #[serde(default)]
    pub events_json: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for CatalogRow {
    const NAME: &'static str = "mcp-gateway-catalog";
    type Key = String;
    fn key(&self) -> String {
        self.integration.clone()
    }
}

/// `mcp-gateway-health`, keyed by `integration`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthRow {
    pub integration: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<RefreshRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorRow>,
    /// Tools the last refresh added or changed (cleared by the next one).
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for HealthRow {
    const NAME: &'static str = "mcp-gateway-health";
    type Key = String;
    fn key(&self) -> String {
        self.integration.clone()
    }
}

/// `mcp-gateway-client`: a downstream bearer token, stored as its SHA-256.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRow {
    pub id: String,
    pub label: String,
    pub token_hash: String,
    pub prefix: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for ClientRow {
    const NAME: &'static str = "mcp-gateway-client";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// `mcp-gateway-oauth-client`: a downstream OAuth client (registered or
/// resolved from its metadata document), keyed by `clientId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthClientRow {
    pub client_id: String,
    /// `dcr` or `cimd`.
    pub kind: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for OAuthClientRow {
    const NAME: &'static str = "mcp-gateway-oauth-client";
    type Key = String;
    fn key(&self) -> String {
        self.client_id.clone()
    }
}

/// `mcp-gateway-grant`: an approved downstream OAuth client and its current
/// refresh token (SHA-256), rotated on every use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantRow {
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub refresh_hash: String,
    pub refresh_expires_at: i64,
    /// SHA-256 of the refresh token the last rotation replaced, still
    /// accepted for two minutes after `rotated_at` so a retried refresh
    /// does not lock the client out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_refresh_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<i64>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for GrantRow {
    const NAME: &'static str = "mcp-gateway-grant";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "denied")]
    Denied,
    /// The code was exchanged (or the request expired after approval).
    #[serde(rename = "consumed")]
    Consumed,
}

/// `mcp-gateway-authorization`: one downstream authorization request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationRow {
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub code_challenge: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub user_code: String,
    pub status: AuthorizationStatus,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_expires_at: Option<i64>,
    /// SHA-256 of the browser binding cookie that started the request.
    pub browser_hash: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for AuthorizationRow {
    const NAME: &'static str = "mcp-gateway-authorization";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// `mcp-gateway-upstream-flow`: an upstream OAuth connection in progress,
/// keyed by its `state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamFlowRow {
    pub state: String,
    pub integration: String,
    /// The transport origin when the flow started; a changed integration fails it.
    pub origin: String,
    /// The PKCE verifier and any client secret, sealed for `flow:<state>`.
    pub sealed: String,
    pub redirect_uri: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub token_auth: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The authorization server returns `iss` on its redirects (RFC 9207).
    #[serde(default)]
    pub iss_supported: bool,
    /// Where the browser returns afterwards (the UI page on its own origin).
    pub return_to: String,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for UpstreamFlowRow {
    const NAME: &'static str = "mcp-gateway-upstream-flow";
    type Key = String;
    fn key(&self) -> String {
        self.state.clone()
    }
}

/// `mcp-gateway-feed`: Omni's poll subscription to one integration event
/// with one argument set, and its replay cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedRow {
    pub id: String,
    pub integration: String,
    pub upstream_event: String,
    /// The served event name (`<integration>__<event>`).
    pub event: String,
    pub arguments: IndexMap<String, String>,
    /// The integration's cursor after the last published event; `None`
    /// until the first poll establishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<i64>,
    pub next_poll_at: i64,
    #[serde(default)]
    pub failures: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<i64>,
    #[serde(default)]
    pub published: i64,
    #[serde(default)]
    pub gaps: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gap_at: Option<i64>,
    /// When the feed last had no current subscriber.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for FeedRow {
    const NAME: &'static str = "mcp-gateway-feed";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<IntegrationRow>(),
        EntityDescriptor::of::<CredentialRow>(),
        EntityDescriptor::of::<CatalogRow>(),
        EntityDescriptor::of::<HealthRow>(),
        EntityDescriptor::of::<ClientRow>(),
        EntityDescriptor::of::<OAuthClientRow>(),
        EntityDescriptor::of::<GrantRow>(),
        EntityDescriptor::of::<AuthorizationRow>(),
        EntityDescriptor::of::<UpstreamFlowRow>(),
        EntityDescriptor::of::<FeedRow>(),
    ]
}
