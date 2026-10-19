//! `GET /gateway/oauth/authorize` and the waiting page's status and
//! completion endpoints.
//!
//! Malformed client ids, redirect URIs, states and request ids are refused
//! before any store access. Until the client and its redirect URI are
//! trusted, problems render an error page; afterwards they redirect to the
//! client with `error`, `error_description`, `state` and `iss`.

use std::time::Duration;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use omni_http::{HttpError, Method, RedirectRule, Url};
use omni_store::StoreError;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde::Deserialize;
use serde_json::{Value, json};

use super::pages::{self, Waiting};
use super::{
    AuthorizationServer, CIMD_KIND, CIMD_TTL_MS, CODE_TTL_MS, DCR_KIND, LOG, MAX_CIMD_CLIENTS,
    MAX_PENDING, MAX_REDIRECT_URI_LEN, MAX_STATE_BYTES, Params, REQUEST_TTL_MS, SCOPE, clean_text,
    is_dcr_client_id, is_known_client_shape, is_request_id, redirect_matches, request_finished,
    valid_redirect_uri, valid_redirect_uris,
};
use crate::gateway::model::{AuthorizationRow, AuthorizationStatus, OAuthClientRow};
use crate::gateway::oauth::pkce;

const COOKIE_SECURE: &str = "__Host-omni-oauth";
const COOKIE_PLAIN: &str = "omni-oauth";
/// The cookie outlives the request a little.
const COOKIE_MAX_AGE_S: i64 = 15 * 60;
const CIMD_TIMEOUT: Duration = Duration::from_secs(10);
const CIMD_MAX_BYTES: usize = 64 * 1024;
const MAX_CLIENT_NAME: usize = 100;
/// An unambiguous alphabet (no 0, O, 1 or I) of 32 symbols.
const USER_CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const NOT_REGISTERED: &str = "The app is not registered with Omni.";

#[derive(Deserialize)]
struct ClientMetadataDocument {
    client_id: String,
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    client_secret: Option<Value>,
}

/// Why a client could not be resolved (shown on an error page).
struct ClientProblem(String);

/// A metadata document was refused or could not be fetched (remembered
/// for [`super::CIMD_FAILURE_TTL_MS`]).
struct DocumentProblem(String);

enum Created {
    Row(Box<AuthorizationRow>),
    Busy,
}

enum Completion {
    Code(Box<AuthorizationRow>),
    Waiting(Box<AuthorizationRow>),
    Denied(Box<AuthorizationRow>),
    Expired(Box<AuthorizationRow>),
    Finished,
    WrongBrowser,
    Unknown,
}

impl AuthorizationServer {
    fn cookie_name(&self) -> &'static str {
        if self.inner.secure {
            COOKIE_SECURE
        } else {
            COOKIE_PLAIN
        }
    }

    fn set_cookie(&self, value: &str) -> String {
        let name = self.cookie_name();
        let secure = if self.inner.secure { "; Secure" } else { "" };
        format!(
            "{name}={value}; Path=/{secure}; HttpOnly; SameSite=Lax; Max-Age={COOKIE_MAX_AGE_S}"
        )
    }

    /// The browser-binding cookie value, when it has the shape Omni issues.
    fn browser_cookie<'h>(&self, headers: &'h HeaderMap) -> Option<&'h str> {
        let name = self.cookie_name();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
            .filter(|value| {
                value.len() == 43
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
    }

    /// A DCR client, or the cached (else fetched) metadata document of an
    /// allowlisted client id. The caller has checked the id's shape.
    async fn resolve_client(
        &self,
        client_id: &str,
        now: i64,
    ) -> Result<OAuthClientRow, ClientProblem> {
        let key = client_id.to_owned();
        let cached = self
            .inner
            .store
            .read(move |docs| docs.get::<OAuthClientRow>(&key))
            .await
            .map_err(|error| store_problem(&error))?;
        if is_dcr_client_id(client_id) {
            return match cached {
                Some(row) if row.kind == DCR_KIND => Ok(row),
                _ => Err(ClientProblem(NOT_REGISTERED.to_owned())),
            };
        }
        if let Some(row) = cached
            && row.kind == CIMD_KIND
            && now.saturating_sub(row.created_at) < CIMD_TTL_MS
        {
            return Ok(row);
        }
        if self.cimd_recently_failed(client_id, now) {
            return Err(ClientProblem(
                "The app's client metadata document could not be used recently; try again in \
                 a few minutes."
                    .to_owned(),
            ));
        }
        let Ok(_permit) = self.inner.cimd_fetches.try_acquire() else {
            return Err(ClientProblem(
                "Omni is busy fetching other client metadata; try again shortly.".to_owned(),
            ));
        };
        let row = match self.fetch_client_metadata(client_id, now).await {
            Ok(row) => row,
            Err(DocumentProblem(reason)) => {
                tracing::debug!(target: LOG, client_id, %reason, "Client metadata document refused");
                self.remember_cimd_failure(client_id, now);
                return Err(ClientProblem(format!(
                    "The app's client metadata document {reason}."
                )));
            }
        };
        let stored = row.clone();
        self.inner
            .store
            .write(move |tx| {
                // Keep at most MAX_CIMD_CLIENTS documents, dropping the oldest.
                let mut others: Vec<OAuthClientRow> = tx
                    .get_all::<OAuthClientRow>()?
                    .into_iter()
                    .filter(|c| c.kind == CIMD_KIND && c.client_id != stored.client_id)
                    .collect();
                others.sort_by_key(|c| c.created_at);
                let excess = (others.len() + 1).saturating_sub(MAX_CIMD_CLIENTS);
                for old in others.iter().take(excess) {
                    tx.delete::<OAuthClientRow>(&old.client_id)?;
                }
                tx.upsert(&stored, UpsertOpts::default())
            })
            .await
            .map_err(|error| store_problem(&error))?;
        Ok(row)
    }

    async fn fetch_client_metadata(
        &self,
        client_id: &str,
        now: i64,
    ) -> Result<OAuthClientRow, DocumentProblem> {
        let problem = |reason: &str| DocumentProblem(reason.to_owned());
        let url = Url::parse(client_id).map_err(|_| problem("URL is not valid"))?;
        let response = self
            .inner
            .http
            .request(Method::GET, url.clone())
            .timeout(CIMD_TIMEOUT)
            .redirect(RedirectRule::Error)
            .header("accept", "application/json")
            .send_bounded(CIMD_MAX_BYTES)
            .await
            .map_err(|error| match error {
                HttpError::Status { .. } => problem("redirects, which is not allowed"),
                HttpError::TooLarge { .. } => problem("is too large"),
                HttpError::Blocked(_) | HttpError::InvalidUrl(_) => problem("URL is not public"),
                _ => problem("could not be fetched"),
            })?;
        if response.status.as_u16() != 200 {
            return Err(DocumentProblem(format!(
                "answered HTTP {}",
                response.status.as_u16()
            )));
        }
        let document: ClientMetadataDocument =
            serde_json::from_slice(&response.body).map_err(|_| problem("is not valid JSON"))?;
        if document.client_id != client_id {
            return Err(problem("names a different client_id"));
        }
        let secret_auth = document
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|method| method.starts_with("client_secret"));
        if document.client_secret.is_some() || secret_auth {
            return Err(problem(
                "uses a client secret, which a public document cannot",
            ));
        }
        if !valid_redirect_uris(&document.redirect_uris) {
            return Err(problem(
                "must list 1 to 10 https (or loopback http) redirect_uris of at most 2048 bytes",
            ));
        }
        let client_name = document
            .client_name
            .as_deref()
            .and_then(|name| clean_text(name, MAX_CLIENT_NAME))
            .or_else(|| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| client_id.to_owned());
        Ok(OAuthClientRow {
            client_id: client_id.to_owned(),
            kind: CIMD_KIND.to_owned(),
            client_name,
            redirect_uris: document.redirect_uris,
            created_at: now,
            extra: Default::default(),
        })
    }

    /// Inserts a pending request unless [`MAX_PENDING`] are waiting; drops
    /// finished requests on the way.
    async fn create_request(&self, row: AuthorizationRow, now: i64) -> Result<Created, StoreError> {
        self.inner
            .store
            .write(move |tx| {
                let mut pending = 0;
                for existing in tx.get_all::<AuthorizationRow>()? {
                    if request_finished(&existing, now) {
                        tx.delete::<AuthorizationRow>(&existing.id)?;
                    } else if existing.status == AuthorizationStatus::Pending {
                        pending += 1;
                    }
                }
                if pending >= MAX_PENDING {
                    return Ok(Created::Busy);
                }
                tx.upsert(&row, UpsertOpts::default())?;
                Ok(Created::Row(Box::new(row)))
            })
            .await
    }

    async fn authorization(&self, id: &str) -> Result<Option<AuthorizationRow>, StoreError> {
        let id = id.to_owned();
        self.inner
            .store
            .read(move |docs| docs.get::<AuthorizationRow>(&id))
            .await
    }

    /// `redirect_uri` with `params` and `iss` appended.
    fn redirect(&self, redirect_uri: &str, params: &[(&str, &str)]) -> Response {
        let Ok(mut url) = Url::parse(redirect_uri) else {
            return pages::error(
                StatusCode::BAD_REQUEST,
                "Invalid redirect",
                "The app's redirect URI is not valid.",
            );
        };
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in params {
                query.append_pair(name, value);
            }
            query.append_pair("iss", &self.inner.issuer);
        }
        let mut response = StatusCode::FOUND.into_response();
        if let Ok(location) = HeaderValue::from_str(url.as_str()) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        response.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        response
    }

    fn redirect_error(
        &self,
        redirect_uri: &str,
        state: Option<&str>,
        error: &str,
        description: &str,
    ) -> Response {
        let mut params = vec![("error", error), ("error_description", description)];
        if let Some(state) = state {
            params.push(("state", state));
        }
        self.redirect(redirect_uri, &params)
    }

    fn waiting_page(&self, row: &AuthorizationRow, set_cookie: Option<&str>) -> Response {
        let host = redirect_host(&row.redirect_uri);
        pages::waiting(
            &Waiting {
                request_id: &row.id,
                client_name: &row.client_name,
                redirect_host: &host,
                user_code: &row.user_code,
            },
            set_cookie,
        )
    }
}

fn redirect_host(redirect_uri: &str) -> String {
    Url::parse(redirect_uri)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

fn store_problem(error: &StoreError) -> ClientProblem {
    tracing::warn!(target: LOG, %error, "OAuth client lookup failed");
    ClientProblem("Omni could not read its client registry; try again shortly.".to_owned())
}

fn user_code() -> String {
    let mut bytes = [0u8; 8];
    rand::fill(&mut bytes);
    let symbols: String = bytes
        .iter()
        .map(|b| char::from(USER_CODE_ALPHABET[usize::from(b & 31)]))
        .collect();
    format!("{}-{}", &symbols[..4], &symbols[4..])
}

fn hashes_match(a: &str, b: &str) -> bool {
    omni_core::digest::ct_eq_sha256(a.as_bytes(), b.as_bytes())
}

pub(super) async fn authorize(
    State(server): State<AuthorizationServer>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(query.unwrap_or_default().as_bytes());
    let now = server.now();
    let bad = |message: &str| pages::error(StatusCode::BAD_REQUEST, "Invalid request", message);

    // Shape checks first: nothing malformed reaches the store or the network.
    let client_id = match params.one("client_id") {
        Ok(Some(id)) => id,
        Ok(None) => return bad("The request has no client_id."),
        Err(_) => return bad("The request repeats client_id."),
    };
    if !is_known_client_shape(client_id) {
        return pages::error(StatusCode::BAD_REQUEST, "Unknown app", NOT_REGISTERED);
    }
    let redirect_uri = match params.one("redirect_uri") {
        Ok(Some(uri)) if uri.len() <= MAX_REDIRECT_URI_LEN && valid_redirect_uri(uri) => uri,
        Ok(Some(_)) => return bad("The redirect URI is not valid."),
        Ok(None) => return bad("The request has no redirect_uri."),
        Err(_) => return bad("The request repeats redirect_uri."),
    };
    let state = match params.one("state") {
        Ok(state) if state.is_none_or(|s| s.len() <= MAX_STATE_BYTES) => state,
        Ok(_) => return bad("The request's state is too long."),
        Err(_) => return bad("The request repeats state."),
    };

    let client = match server.resolve_client(client_id, now).await {
        Ok(client) => client,
        Err(ClientProblem(message)) => {
            return pages::error(StatusCode::BAD_REQUEST, "Unknown app", &message);
        }
    };
    if !client
        .redirect_uris
        .iter()
        .any(|u| redirect_matches(u, redirect_uri))
    {
        return bad("The redirect URI is not registered for this app.");
    }

    // The redirect URI is trusted from here on.
    let fail = |error: &str, description: &str| {
        server.redirect_error(redirect_uri, state, error, description)
    };
    if let Some(name) = params.repeated() {
        return fail("invalid_request", &format!("{name} is repeated"));
    }
    match params.one("response_type") {
        Ok(Some("code")) => {}
        Ok(None) => return fail("invalid_request", "response_type is required"),
        _ => {
            return fail(
                "unsupported_response_type",
                "only the code response type is supported",
            );
        }
    }
    let challenge = match params.one("code_challenge") {
        Ok(Some(challenge)) => challenge,
        _ => return fail("invalid_request", "code_challenge is required (PKCE S256)"),
    };
    if !matches!(params.one("code_challenge_method"), Ok(Some("S256"))) {
        return fail("invalid_request", "code_challenge_method must be S256");
    }
    if !pkce::is_valid_challenge(challenge) {
        return fail(
            "invalid_request",
            "code_challenge is not a valid S256 challenge",
        );
    }
    if let Ok(Some(scope)) = params.one("scope")
        && scope.split_whitespace().any(|s| s != SCOPE)
    {
        return fail("invalid_scope", "the only scope is omni");
    }
    if let Ok(Some(resource)) = params.one("resource")
        && !server.names_resource(resource)
    {
        return fail("invalid_target", "resource must be Omni's MCP gateway URL");
    }

    let cookie = server
        .browser_cookie(&headers)
        .map(str::to_owned)
        .unwrap_or_else(|| omni_core::ids::secure_id_b64url(32));
    let row = AuthorizationRow {
        id: omni_core::ids::secure_id_b64url(16),
        client_id: client.client_id.clone(),
        client_name: client.client_name.clone(),
        redirect_uri: redirect_uri.to_owned(),
        state: state.map(str::to_owned),
        code_challenge: challenge.to_owned(),
        resource: server.inner.resource.to_string(),
        scope: Some(SCOPE.to_owned()),
        user_code: user_code(),
        status: AuthorizationStatus::Pending,
        created_at: now,
        expires_at: now + REQUEST_TTL_MS,
        code_hash: None,
        code_expires_at: None,
        browser_hash: omni_core::digest::sha256_hex(&cookie),
        extra: Default::default(),
    };
    let row = match server.create_request(row, now).await {
        Ok(Created::Row(row)) => *row,
        Ok(Created::Busy) => {
            return fail(
                "temporarily_unavailable",
                "too many authorization requests are waiting for the owner",
            );
        }
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth request not recorded");
            return fail("server_error", "Omni could not record the request");
        }
    };
    tracing::info!(
        target: LOG,
        client = %row.client_name,
        redirect = %redirect_host(&row.redirect_uri),
        "OAuth authorization requested; waiting for the owner"
    );
    if let Some(notify) = &server.inner.notifier {
        notify(&row);
    }
    server.waiting_page(&row, Some(&server.set_cookie(&cookie)))
}

/// The well-formed `request` parameter.
fn request_param(query: Option<String>) -> Option<String> {
    Params::parse(query.unwrap_or_default().as_bytes())
        .one("request")
        .ok()
        .flatten()
        .filter(|id| is_request_id(id))
        .map(str::to_owned)
}

fn view_status(row: &AuthorizationRow, now: i64) -> &'static str {
    let live = now < row.expires_at;
    match row.status {
        AuthorizationStatus::Pending if live => "pending",
        AuthorizationStatus::Approved if live => "approved",
        AuthorizationStatus::Denied => "denied",
        _ => "expired",
    }
}

pub(super) async fn status(
    State(server): State<AuthorizationServer>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown request"})),
        )
            .into_response()
    };
    let other_browser = || {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "this request belongs to another browser"})),
        )
            .into_response()
    };
    let Some(id) = request_param(query) else {
        return not_found();
    };
    let Some(cookie) = server.browser_cookie(&headers) else {
        return other_browser();
    };
    let row = match server.authorization(&id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth request status unavailable");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "unavailable"})),
            )
                .into_response();
        }
    };
    if !hashes_match(&omni_core::digest::sha256_hex(cookie), &row.browser_hash) {
        return other_browser();
    }
    Json(json!({"status": view_status(&row, server.now())})).into_response()
}

pub(super) async fn complete(
    State(server): State<AuthorizationServer>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let Some(id) = request_param(query) else {
        return pages::error(
            StatusCode::NOT_FOUND,
            "Unknown request",
            "This request does not exist.",
        );
    };
    let Some(cookie) = server.browser_cookie(&headers) else {
        return pages::error(
            StatusCode::FORBIDDEN,
            "Wrong browser",
            "This request was started in another browser.",
        );
    };
    let browser_hash = omni_core::digest::sha256_hex(cookie);
    let code = omni_core::ids::secure_id_b64url(32);
    let code_hash = omni_core::digest::sha256_hex(&code);
    let now = server.now();
    let store = server.inner.store.clone();
    let outcome = omni_core::spawn::must_complete(&server.inner.tracker, async move {
        store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AuthorizationRow>(&id)? else {
                    return Ok::<_, StoreError>(Completion::Unknown);
                };
                if !hashes_match(&browser_hash, &row.browser_hash) {
                    return Ok(Completion::WrongBrowser);
                }
                let live = now < row.expires_at;
                Ok(match row.status {
                    AuthorizationStatus::Approved if live && row.code_hash.is_none() => {
                        row.code_hash = Some(code_hash);
                        row.code_expires_at = Some(now + CODE_TTL_MS);
                        tx.upsert(&row, UpsertOpts::default())?;
                        Completion::Code(Box::new(row))
                    }
                    AuthorizationStatus::Approved if row.code_hash.is_some() => {
                        Completion::Finished
                    }
                    AuthorizationStatus::Consumed => Completion::Finished,
                    AuthorizationStatus::Pending if live => Completion::Waiting(Box::new(row)),
                    AuthorizationStatus::Denied => Completion::Denied(Box::new(row)),
                    _ => Completion::Expired(Box::new(row)),
                })
            })
            .await
    })
    .await;
    match outcome {
        Ok(Completion::Code(row)) => {
            tracing::info!(target: LOG, client = %row.client_name, "OAuth code issued");
            let mut params = vec![("code", code.as_str())];
            if let Some(state) = row.state.as_deref() {
                params.push(("state", state));
            }
            server.redirect(&row.redirect_uri, &params)
        }
        Ok(Completion::Waiting(row)) => server.waiting_page(&row, None),
        Ok(Completion::Denied(row)) => server.redirect_error(
            &row.redirect_uri,
            row.state.as_deref(),
            "access_denied",
            "the owner denied the request",
        ),
        Ok(Completion::Expired(row)) => server.redirect_error(
            &row.redirect_uri,
            row.state.as_deref(),
            "access_denied",
            "the request expired before the owner approved it",
        ),
        Ok(Completion::Finished) => pages::error(
            StatusCode::CONFLICT,
            "Already completed",
            "This request was already completed.",
        ),
        Ok(Completion::WrongBrowser) => pages::error(
            StatusCode::FORBIDDEN,
            "Wrong browser",
            "This request was started in another browser.",
        ),
        Ok(Completion::Unknown) => pages::error(
            StatusCode::NOT_FOUND,
            "Unknown request",
            "This request does not exist.",
        ),
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth completion not recorded");
            pages::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again",
                "Omni could not complete the request; reload this page.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_use_the_unambiguous_alphabet() {
        for _ in 0..50 {
            let code = user_code();
            assert_eq!(code.len(), 9);
            assert_eq!(&code[4..5], "-");
            assert!(
                code.bytes()
                    .filter(|b| *b != b'-')
                    .all(|b| USER_CODE_ALPHABET.contains(&b))
            );
        }
    }
}
