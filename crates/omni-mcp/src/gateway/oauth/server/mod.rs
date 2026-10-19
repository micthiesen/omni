//! The downstream OAuth 2.1 authorization server ChatGPT connects through.
//!
//! The issuer is the public resource URL's origin. Clients are public
//! (`token_endpoint_auth_method: none`): a Client ID Metadata Document URL
//! on an allowlisted host ([`CIMD_HOSTS`]; fetched through the SSRF-guarded
//! client, cached for a day, at most [`MAX_CIMD_CLIENTS`] kept) or a
//! dynamically registered client (at most [`MAX_DCR_CLIENTS`], the oldest
//! idle one evicted for a new one). Every authorization request uses the
//! code grant with PKCE S256 and waits for the owner, who approves it on the
//! LAN-only gateway page by typing the code the waiting page shows
//! ([`AuthorizationServer::approve`]; five wrong codes deny it). A
//! browser-binding cookie ensures only the browser that started a request
//! can complete it. Codes are single use (a replay revokes the grant) and
//! live 60 seconds; refresh tokens rotate on every use, the previous one is
//! accepted for two minutes, and presenting it later revokes the grant.
//! Codes, refresh tokens and the binding cookie are stored only as SHA-256.
//! Malformed values are refused before any store access, and one global
//! token bucket ([`limit`]) bounds every route.

mod authorize;
pub mod limit;
mod pages;
mod register;
mod token;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use omni_core::clock::SharedClock;
use omni_http::Url;
use omni_http::public::PublicHttpClient;
use omni_store::JsValue;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use crate::gateway::access::ACCESS_TOKEN_TTL_MS;
use crate::gateway::model::{AuthorizationRow, AuthorizationStatus, GrantRow, OAuthClientRow};
use crate::gateway::secrets::GatewaySecrets;

const LOG: &str = "MCP:Gateway";

/// The only scope.
pub const SCOPE: &str = "omni";
/// How long an authorization request waits for the owner.
pub const REQUEST_TTL_MS: i64 = 10 * 60 * 1000;
/// How long an issued authorization code can be exchanged.
pub const CODE_TTL_MS: i64 = 60 * 1000;
/// Refresh token lifetime, renewed by every rotation.
pub const REFRESH_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// How long the refresh token a rotation replaced stays usable; presenting
/// it later revokes the grant.
pub const PREVIOUS_REFRESH_GRACE_MS: i64 = 2 * 60 * 1000;
/// How long a fetched client metadata document is reused.
pub const CIMD_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Cached client metadata documents; the oldest is evicted for a new one.
pub const MAX_CIMD_CLIENTS: usize = 20;
/// How long a failed metadata document fetch is remembered (in memory).
pub const CIMD_FAILURE_TTL_MS: i64 = 5 * 60 * 1000;
/// Remembered failed fetches at most.
const MAX_CIMD_FAILURES: usize = 256;
/// Concurrent client metadata document fetches.
const MAX_CIMD_FETCHES: usize = 4;
/// Hosts (and their subdomains) whose client metadata documents are accepted.
pub const CIMD_HOSTS: [&str; 5] = [
    "chatgpt.com",
    "openai.com",
    "claude.ai",
    "claude.com",
    "anthropic.com",
];
/// A registered client with no grant or request is removed after this.
pub const DCR_STALE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Registered clients at most; the oldest idle one makes room for a new one.
pub const MAX_DCR_CLIENTS: usize = 50;
/// Unexpired pending requests at once; more are refused (bounds owner notifications).
pub const MAX_PENDING: usize = 20;
/// Wrong user codes before a request is denied.
pub const MAX_WRONG_CODES: i64 = 5;
/// The `extra` field counting wrong user codes on an authorization row.
const WRONG_CODES_FIELD: &str = "wrongCodes";
pub const MAX_STATE_BYTES: usize = 2048;
pub const MAX_REDIRECT_URIS: usize = 10;
pub const MAX_REDIRECT_URI_LEN: usize = 2048;

const PRM_PATH: &str = "/.well-known/oauth-protected-resource";
const AS_METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
pub const AUTHORIZE_PATH: &str = "/gateway/oauth/authorize";
pub const STATUS_PATH: &str = "/gateway/oauth/authorize/status";
pub const COMPLETE_PATH: &str = "/gateway/oauth/authorize/complete";
pub const TOKEN_PATH: &str = "/gateway/oauth/token";
pub const REGISTER_PATH: &str = "/gateway/oauth/register";

pub const CIMD_KIND: &str = "cimd";
pub const DCR_KIND: &str = "dcr";

/// Called with every new pending request (to alert the owner).
pub type AuthorizationNotifier = Arc<dyn Fn(&AuthorizationRow) + Send + Sync>;

/// The outcome of [`AuthorizationServer::approve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Approved,
    /// The code does not match; the fifth wrong code denies the request.
    WrongCode,
    /// No such request, or it expired or was already decided.
    NotFound,
}

struct Inner {
    store: Store,
    clock: SharedClock,
    http: PublicHttpClient,
    secrets: GatewaySecrets,
    resource: Url,
    issuer: String,
    /// `https` resource: `__Host-` cookie with `Secure`.
    secure: bool,
    notifier: Option<AuthorizationNotifier>,
    tracker: TaskTracker,
    cimd_fetches: Semaphore,
    /// Client ids whose metadata document failed, until when (epoch ms).
    cimd_failures: Mutex<HashMap<String, i64>>,
    limiter: Mutex<limit::TokenBucket>,
    /// The token endpoint's own bucket, so a flood elsewhere never blocks
    /// refreshes.
    token_limiter: Mutex<limit::TokenBucket>,
}

/// The authorization server; cheap to clone.
#[derive(Clone)]
pub struct AuthorizationServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AuthorizationServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationServer")
            .field("issuer", &self.inner.issuer)
            .field("resource", &self.inner.resource.as_str())
            .finish_non_exhaustive()
    }
}

impl AuthorizationServer {
    /// `resource` is the public MCP URL (for example
    /// `https://gateway.example/gateway/mcp`); its origin is the issuer.
    pub fn new(
        store: Store,
        clock: SharedClock,
        http: PublicHttpClient,
        secrets: GatewaySecrets,
        resource: Url,
        notifier: Option<AuthorizationNotifier>,
        tracker: TaskTracker,
    ) -> Self {
        let issuer = resource.origin().ascii_serialization();
        let secure = resource.scheme() == "https";
        let limiter = limit::TokenBucket::full(clock.now_ms());
        let token_limiter = limit::TokenBucket::full(clock.now_ms());
        Self {
            inner: Arc::new(Inner {
                store,
                clock,
                http,
                secrets,
                resource,
                issuer,
                secure,
                notifier,
                tracker,
                cimd_fetches: Semaphore::new(MAX_CIMD_FETCHES),
                cimd_failures: Mutex::new(HashMap::new()),
                limiter,
                token_limiter,
            }),
        }
    }

    /// `scheme://host[:port]`, without a path.
    pub fn issuer(&self) -> String {
        self.inner.issuer.clone()
    }

    pub fn resource(&self) -> &Url {
        &self.inner.resource
    }

    /// The protected resource metadata URL, for `WWW-Authenticate` on a 401.
    pub fn resource_metadata_url(&self) -> String {
        format!("{}{}", self.inner.issuer, self.resource_metadata_path())
    }

    fn resource_metadata_path(&self) -> String {
        let path = self.inner.resource.path().trim_end_matches('/');
        format!("{PRM_PATH}{path}")
    }

    /// The metadata, authorization, token and registration routes at their
    /// absolute paths behind the rate limiter, every response
    /// `Cache-Control: no-store`.
    pub fn router(&self) -> Router {
        let resource_path = self.resource_metadata_path();
        let mut router = Router::new().route(PRM_PATH, get(resource_metadata));
        if resource_path != PRM_PATH {
            router = router.route(&resource_path, get(resource_metadata));
        }
        router
            .route(AS_METADATA_PATH, get(server_metadata))
            .route(AUTHORIZE_PATH, get(authorize::authorize))
            .route(STATUS_PATH, get(authorize::status))
            .route(COMPLETE_PATH, get(authorize::complete))
            .route(TOKEN_PATH, post(token::token))
            .route(REGISTER_PATH, post(register::register))
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                limit::rate_limit,
            ))
            .layer(axum::middleware::map_response(no_store))
            .with_state(self.clone())
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    /// Whether `value` names this resource (a trailing slash aside).
    fn names_resource(&self, value: &str) -> bool {
        let normalize = |url: &Url| url.as_str().trim_end_matches('/').to_owned();
        Url::parse(value).is_ok_and(|url| normalize(&url) == normalize(&self.inner.resource))
    }

    /// Pending, unexpired requests, oldest first.
    pub async fn pending(&self) -> Result<Vec<AuthorizationRow>, StoreError> {
        let now = self.now();
        let mut rows = self
            .inner
            .store
            .read(|docs| docs.get_all::<AuthorizationRow>())
            .await?;
        rows.retain(|row| row.status == AuthorizationStatus::Pending && row.expires_at > now);
        rows.sort_by_key(|row| row.created_at);
        Ok(rows)
    }

    /// Approves a pending, unexpired request when `user_code` is the code its
    /// waiting page shows (case, spaces and the dash aside). The fifth wrong
    /// code denies the request.
    pub async fn approve(&self, id: &str, user_code: &str) -> Result<Approval, StoreError> {
        if !is_request_id(id) {
            return Ok(Approval::NotFound);
        }
        let now = self.now();
        let id = id.to_owned();
        let supplied = normalize_user_code(user_code);
        let (approval, client, locked) = self
            .inner
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AuthorizationRow>(&id)? else {
                    return Ok::<_, StoreError>((Approval::NotFound, String::new(), false));
                };
                if row.status != AuthorizationStatus::Pending || row.expires_at <= now {
                    return Ok((Approval::NotFound, String::new(), false));
                }
                let expected = normalize_user_code(&row.user_code);
                if omni_core::digest::ct_eq_sha256(expected.as_bytes(), supplied.as_bytes()) {
                    row.status = AuthorizationStatus::Approved;
                    tx.upsert(&row, UpsertOpts::default())?;
                    return Ok((Approval::Approved, row.client_name, false));
                }
                let wrong = wrong_codes(&row).saturating_add(1);
                row.extra.insert(
                    WRONG_CODES_FIELD.to_owned(),
                    JsValue::Int(i128::from(wrong)),
                );
                let locked = wrong >= MAX_WRONG_CODES;
                if locked {
                    row.status = AuthorizationStatus::Denied;
                }
                tx.upsert(&row, UpsertOpts::default())?;
                Ok((Approval::WrongCode, row.client_name, locked))
            })
            .await?;
        match approval {
            Approval::Approved => {
                tracing::info!(target: LOG, %client, "OAuth request approved");
            }
            Approval::WrongCode if locked => {
                tracing::warn!(
                    target: LOG,
                    %client,
                    "OAuth request denied after {MAX_WRONG_CODES} wrong approval codes"
                );
            }
            Approval::WrongCode => {
                tracing::info!(target: LOG, %client, "OAuth approval code did not match");
            }
            Approval::NotFound => {}
        }
        Ok(approval)
    }

    /// Denies a pending request (or an approved one whose code was not yet
    /// issued); whether it was.
    pub async fn deny(&self, id: &str) -> Result<bool, StoreError> {
        if !is_request_id(id) {
            return Ok(false);
        }
        let id = id.to_owned();
        let denied = self
            .inner
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AuthorizationRow>(&id)? else {
                    return Ok::<_, StoreError>(None);
                };
                let open = row.status == AuthorizationStatus::Pending
                    || (row.status == AuthorizationStatus::Approved && row.code_hash.is_none());
                if !open {
                    return Ok(None);
                }
                row.status = AuthorizationStatus::Denied;
                tx.upsert(&row, UpsertOpts::default())?;
                Ok(Some(row))
            })
            .await?;
        if let Some(row) = &denied {
            tracing::info!(target: LOG, client = %row.client_name, "OAuth request denied");
        }
        Ok(denied.is_some())
    }

    /// Deletes expired requests, grants whose refresh token (and last access
    /// token) expired, client metadata documents older than a day, and
    /// registered clients older than 30 days with no grant or request.
    pub async fn prune(&self) -> Result<(), StoreError> {
        let now = self.now();
        self.inner
            .store
            .write(move |tx| {
                let mut active: HashSet<String> = HashSet::new();
                for grant in tx.get_all::<GrantRow>()? {
                    if grant.refresh_expires_at.saturating_add(ACCESS_TOKEN_TTL_MS) <= now {
                        tx.delete::<GrantRow>(&grant.id)?;
                    } else {
                        active.insert(grant.client_id);
                    }
                }
                for row in tx.get_all::<AuthorizationRow>()? {
                    if request_finished(&row, now) {
                        tx.delete::<AuthorizationRow>(&row.id)?;
                    } else {
                        active.insert(row.client_id);
                    }
                }
                for client in tx.get_all::<OAuthClientRow>()? {
                    let age = now.saturating_sub(client.created_at);
                    let stale = match client.kind.as_str() {
                        CIMD_KIND => age >= CIMD_TTL_MS,
                        DCR_KIND => age >= DCR_STALE_MS && !active.contains(&client.client_id),
                        _ => false,
                    };
                    if stale {
                        tx.delete::<OAuthClientRow>(&client.client_id)?;
                    }
                }
                Ok(())
            })
            .await
    }

    /// Whether the metadata document of `client_id` failed in the last
    /// [`CIMD_FAILURE_TTL_MS`].
    fn cimd_recently_failed(&self, client_id: &str, now: i64) -> bool {
        let failures = self
            .inner
            .cimd_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failures.get(client_id).is_some_and(|until| now < *until)
    }

    fn remember_cimd_failure(&self, client_id: &str, now: i64) {
        let mut failures = self
            .inner
            .cimd_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failures.retain(|_, until| now < *until);
        while failures.len() >= MAX_CIMD_FAILURES {
            let Some(soonest) = failures
                .iter()
                .min_by_key(|(_, until)| **until)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            failures.remove(&soonest);
        }
        failures.insert(client_id.to_owned(), now + CIMD_FAILURE_TTL_MS);
    }
}

/// A request past its expiry and any issued code's.
fn request_finished(row: &AuthorizationRow, now: i64) -> bool {
    row.expires_at.max(row.code_expires_at.unwrap_or(i64::MIN)) <= now
}

/// Wrong user codes entered so far for a request.
fn wrong_codes(row: &AuthorizationRow) -> i64 {
    match row.extra.get(WRONG_CODES_FIELD) {
        Some(JsValue::Int(n)) => i64::try_from(*n).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// Uppercase, without whitespace and dashes.
fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

/// Whether the request came from an allowlisted client metadata document.
/// A DCR-registered app is unverified: anyone can register one.
pub fn is_verified_client(row: &AuthorizationRow) -> bool {
    is_allowlisted_cimd(&row.client_id)
}

/// A canonical `https` URL with a path, on the default port, whose host is
/// one of [`CIMD_HOSTS`] or their subdomain.
pub(super) fn is_allowlisted_cimd(client_id: &str) -> bool {
    let Ok(url) = Url::parse(client_id) else {
        return false;
    };
    let shape = url.scheme() == "https"
        && url.as_str() == client_id
        && url.port().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() != "/";
    let Some(url::Host::Domain(host)) = url.host() else {
        return false;
    };
    shape
        && CIMD_HOSTS.iter().any(|allowed| {
            host == *allowed
                || host
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

/// `len` base64url characters (what `secure_id_b64url` produces).
fn is_b64url(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An authorization request id (`secure_id_b64url(16)`).
pub(super) fn is_request_id(value: &str) -> bool {
    is_b64url(value, 22)
}

/// An authorization code (`secure_id_b64url(32)`).
pub(super) fn is_code(value: &str) -> bool {
    is_b64url(value, 43)
}

/// A refresh token: `ort_` and `secure_id_b64url(32)`.
pub(super) fn is_refresh_token(value: &str) -> bool {
    value
        .strip_prefix(token::REFRESH_PREFIX)
        .is_some_and(|rest| is_b64url(rest, 43))
}

/// A DCR client id (a hyphenated UUID).
pub(super) fn is_dcr_client_id(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A client id worth looking up: a DCR id or an allowlisted metadata URL.
pub(super) fn is_known_client_shape(value: &str) -> bool {
    is_dcr_client_id(value) || is_allowlisted_cimd(value)
}

/// At most [`MAX_REDIRECT_URIS`], each valid and at most [`MAX_REDIRECT_URI_LEN`] bytes.
pub(super) fn valid_redirect_uris(uris: &[String]) -> bool {
    !uris.is_empty()
        && uris.len() <= MAX_REDIRECT_URIS
        && uris
            .iter()
            .all(|uri| uri.len() <= MAX_REDIRECT_URI_LEN && valid_redirect_uri(uri))
}

/// The DCR clients to evict so one more fits under [`MAX_DCR_CLIENTS`]: the
/// oldest with no grant and no live request. `None` when too few are idle.
fn dcr_evictions(
    clients: &[OAuthClientRow],
    grants: &[GrantRow],
    requests: &[AuthorizationRow],
    now: i64,
) -> Option<Vec<String>> {
    let registered: Vec<&OAuthClientRow> = clients.iter().filter(|c| c.kind == DCR_KIND).collect();
    let excess = (registered.len() + 1).saturating_sub(MAX_DCR_CLIENTS);
    if excess == 0 {
        return Some(Vec::new());
    }
    let busy: HashSet<&str> = grants
        .iter()
        .map(|g| g.client_id.as_str())
        .chain(
            requests
                .iter()
                .filter(|r| !request_finished(r, now))
                .map(|r| r.client_id.as_str()),
        )
        .collect();
    let mut idle: Vec<&OAuthClientRow> = registered
        .into_iter()
        .filter(|c| !busy.contains(c.client_id.as_str()))
        .collect();
    if idle.len() < excess {
        return None;
    }
    idle.sort_by_key(|c| c.created_at);
    Some(
        idle.into_iter()
            .take(excess)
            .map(|c| c.client_id.clone())
            .collect(),
    )
}

async fn no_store(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn resource_metadata(State(server): State<AuthorizationServer>) -> Response {
    Json(json!({
        "resource": server.inner.resource.as_str(),
        "authorization_servers": [server.inner.issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE],
        "resource_name": "Omni",
    }))
    .into_response()
}

async fn server_metadata(State(server): State<AuthorizationServer>) -> Response {
    let issuer = &server.inner.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}{AUTHORIZE_PATH}"),
        "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
        "registration_endpoint": format!("{issuer}{REGISTER_PATH}"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
    }))
    .into_response()
}

/// Query or form parameters, keeping repeats so they can be refused.
pub(super) struct Params(Vec<(String, String)>);

/// A parameter appeared more than once.
pub(super) struct Repeated(pub &'static str);

impl Params {
    pub(super) fn parse(input: &[u8]) -> Self {
        Self(
            url::form_urlencoded::parse(input)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// The single value of `name`; an empty value counts as absent.
    pub(super) fn one(&self, name: &'static str) -> Result<Option<&str>, Repeated> {
        let mut values = self.0.iter().filter(|(k, _)| k == name).map(|(_, v)| v);
        let first = values.next();
        if values.next().is_some() {
            return Err(Repeated(name));
        }
        Ok(first.map(String::as_str).filter(|v| !v.is_empty()))
    }

    /// The first repeated parameter, if any.
    pub(super) fn repeated(&self) -> Option<&str> {
        let mut seen = HashSet::new();
        self.0
            .iter()
            .map(|(k, _)| k.as_str())
            .find(|k| !seen.insert(*k))
    }
}

/// `https` with a host, or `http` on `127.0.0.1`, `[::1]` or `localhost`; no
/// fragment or credentials.
pub(super) fn valid_redirect_uri(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(url::Host::Domain(host))) => host.eq_ignore_ascii_case("localhost"),
        ("http", Some(url::Host::Ipv4(ip))) => ip == std::net::Ipv4Addr::LOCALHOST,
        ("http", Some(url::Host::Ipv6(ip))) => ip == std::net::Ipv6Addr::LOCALHOST,
        _ => false,
    }
}

/// Whether a requested redirect URI matches a registered one: exactly, or for
/// an `http` loopback URI (127.0.0.1, [::1], localhost) with any port, as
/// RFC 8252 section 7.3 requires for native apps (Codex registers
/// `http://127.0.0.1/callback` and listens on an ephemeral port).
pub(super) fn redirect_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    let (Ok(registered), Ok(mut requested)) = (Url::parse(registered), Url::parse(requested))
    else {
        return false;
    };
    let loopback = |url: &Url| {
        url.scheme() == "http"
            && match url.host() {
                Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            }
    };
    if !loopback(&registered) || !loopback(&requested) || registered.port().is_some() {
        return false;
    }
    if requested.set_port(None).is_err() {
        return false;
    }
    requested == registered
}

/// Free text from a client: no control characters, trimmed, at most `max` characters.
pub(super) fn clean_text(value: &str, max: usize) -> Option<String> {
    let text: String = value.chars().filter(|c| !c.is_control()).collect();
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= max).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_redirects_match_on_any_port() {
        let codex = "http://127.0.0.1/callback";
        assert!(redirect_matches(codex, "http://127.0.0.1:60319/callback"));
        assert!(redirect_matches(codex, codex));
        assert!(redirect_matches(
            "http://localhost/callback",
            "http://localhost:4000/callback"
        ));
        assert!(!redirect_matches(codex, "http://127.0.0.1:60319/other"));
        assert!(!redirect_matches(
            codex,
            "http://127.0.0.1:60319/callback?x=1"
        ));
        assert!(!redirect_matches(
            codex,
            "http://evil.example:60319/callback"
        ));
        assert!(!redirect_matches(codex, "http://localhost:60319/callback"));
        assert!(!redirect_matches(
            "https://app.example/cb",
            "https://app.example:8443/cb"
        ));
        assert!(!redirect_matches(
            "http://127.0.0.1:5000/cb",
            "http://127.0.0.1:6000/cb"
        ));
    }

    #[test]
    fn redirect_uri_rules() {
        assert!(valid_redirect_uri(
            "https://chatgpt.com/connector_platform_oauth_redirect"
        ));
        assert!(valid_redirect_uri("http://127.0.0.1:8080/cb"));
        assert!(valid_redirect_uri("http://[::1]/cb"));
        assert!(valid_redirect_uri("http://localhost:3000/cb"));
        assert!(!valid_redirect_uri("http://example.com/cb"));
        assert!(!valid_redirect_uri("http://127.0.0.2/cb"));
        assert!(!valid_redirect_uri("https://example.com/cb#frag"));
        assert!(!valid_redirect_uri("https://user:pw@example.com/cb"));
        assert!(!valid_redirect_uri("javascript:alert(1)"));
        assert!(!valid_redirect_uri("not a url"));
        let many: Vec<String> = (0..11).map(|i| format!("https://a.example/{i}")).collect();
        assert!(valid_redirect_uris(&many[..10]));
        assert!(!valid_redirect_uris(&many));
        assert!(!valid_redirect_uris(&[]));
        assert!(!valid_redirect_uris(&[format!(
            "https://a.example/{}",
            "x".repeat(2048)
        )]));
    }

    #[test]
    fn only_allowlisted_metadata_documents_are_accepted() {
        assert!(is_allowlisted_cimd("https://chatgpt.com/oauth/client.json"));
        assert!(is_allowlisted_cimd("https://auth.openai.com/client.json"));
        assert!(is_allowlisted_cimd(
            "https://claude.ai/oauth/claude-code-client-metadata"
        ));
        assert!(!is_allowlisted_cimd("https://chatgpt.com/"));
        assert!(!is_allowlisted_cimd("https://chatgpt.com:8443/client.json"));
        assert!(!is_allowlisted_cimd("http://chatgpt.com/client.json"));
        assert!(!is_allowlisted_cimd("https://notchatgpt.com/client.json"));
        assert!(!is_allowlisted_cimd(
            "https://chatgpt.com.evil.example/client.json"
        ));
        assert!(!is_allowlisted_cimd("https://evil.example/chatgpt.com"));
        assert!(!is_allowlisted_cimd("https://CHATGPT.com/client.json"));
        assert!(!is_allowlisted_cimd("https://chatgpt.com/a/../client.json"));
    }

    #[test]
    fn user_codes_compare_without_case_spaces_or_dash() {
        assert_eq!(normalize_user_code(" kq4t-7xmb "), "KQ4T7XMB");
        assert_eq!(normalize_user_code("KQ4T 7XMB"), "KQ4T7XMB");
    }

    #[test]
    fn value_shapes() {
        assert!(is_request_id(&omni_core::ids::secure_id_b64url(16)));
        assert!(!is_request_id("short"));
        assert!(is_code(&omni_core::ids::secure_id_b64url(32)));
        assert!(!is_code(&format!("{}!", "a".repeat(42))));
        let refresh = format!("ort_{}", omni_core::ids::secure_id_b64url(32));
        assert!(is_refresh_token(&refresh));
        assert!(!is_refresh_token("ort_short"));
        assert!(!is_refresh_token(&omni_core::ids::secure_id_b64url(32)));
        assert!(is_dcr_client_id(&omni_core::ids::uuid_v4()));
        assert!(!is_dcr_client_id("https://chatgpt.com/oauth/client.json"));
        assert!(!is_dcr_client_id(&"g".repeat(36)));
    }

    #[test]
    fn params_refuse_repeats() {
        let params = Params::parse(b"a=1&b=&c=2&c=3");
        assert_eq!(params.one("a").ok().flatten(), Some("1"));
        assert_eq!(params.one("b").ok().flatten(), None);
        assert!(params.one("c").is_err());
        assert_eq!(params.repeated(), Some("c"));
    }

    #[test]
    fn client_text_is_cleaned() {
        assert_eq!(clean_text("  ChatGPT\n ", 100).as_deref(), Some("ChatGPT"));
        assert_eq!(clean_text("   ", 100), None);
        assert_eq!(clean_text(&"x".repeat(101), 100), None);
    }
}
