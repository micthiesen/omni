//! `POST /gateway/oauth/token`: the authorization code and refresh token
//! grants for public clients, with RFC 6749 JSON errors. Codes and refresh
//! tokens of the wrong shape are refused before any store access; a replayed
//! code or a rotated-out refresh token presented after its grace window
//! revokes the grant.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use omni_store::StoreError;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde_json::json;

use super::{
    AuthorizationServer, LOG, PREVIOUS_REFRESH_GRACE_MS, Params, REFRESH_TTL_MS, Repeated, SCOPE,
    is_code, is_refresh_token,
};
use crate::gateway::access::{ACCESS_TOKEN_TTL_MS, issue_access_token};
use crate::gateway::model::{AuthorizationRow, AuthorizationStatus, GrantRow};
use crate::gateway::oauth::pkce;

const MAX_BODY: usize = 16 * 1024;
pub(super) const REFRESH_PREFIX: &str = "ort_";

/// An RFC 6749 section 5.2 error.
struct TokenError {
    status: StatusCode,
    error: &'static str,
    description: String,
}

impl TokenError {
    fn new(error: &'static str, description: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error,
            description: description.into(),
        }
    }

    fn invalid_grant(description: impl Into<String>) -> Self {
        Self::new("invalid_grant", description)
    }

    fn server() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: "server_error",
            description: "Omni could not record the grant".to_owned(),
        }
    }

    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error": self.error, "error_description": self.description})),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        response
    }
}

impl From<Repeated> for TokenError {
    fn from(Repeated(name): Repeated) -> Self {
        TokenError::new("invalid_request", format!("{name} is repeated"))
    }
}

/// What a token exchange decided inside its transaction.
enum Decision {
    Granted(Box<GrantRow>),
    Rejected(&'static str),
    /// A consumed code or a rotated-out refresh token was presented again;
    /// the grant (of this client name) was revoked.
    Revoked(String),
}

pub(super) async fn token(
    State(server): State<AuthorizationServer>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match handle(&server, &headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn handle(
    server: &AuthorizationServer,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, TokenError> {
    let form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| {
            mime.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !form {
        return Err(TokenError::new(
            "invalid_request",
            "the body must be application/x-www-form-urlencoded",
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| TokenError::new("invalid_request", "the request body is too large"))?;
    let params = Params::parse(&bytes);
    if let Some(name) = params.repeated() {
        return Err(TokenError::new(
            "invalid_request",
            format!("{name} is repeated"),
        ));
    }
    if let Some(resource) = params.one("resource")?
        && !server.names_resource(resource)
    {
        return Err(TokenError::new(
            "invalid_target",
            "resource must be Omni's MCP gateway URL",
        ));
    }
    match params.one("grant_type")? {
        Some("authorization_code") => exchange_code(server, &params).await,
        Some("refresh_token") => refresh(server, &params).await,
        Some(_) => Err(TokenError::new(
            "unsupported_grant_type",
            "only authorization_code and refresh_token are supported",
        )),
        None => Err(TokenError::new("invalid_request", "grant_type is required")),
    }
}

fn required<'a>(params: &'a Params, name: &'static str) -> Result<&'a str, TokenError> {
    params
        .one(name)?
        .ok_or_else(|| TokenError::new("invalid_request", format!("{name} is required")))
}

async fn exchange_code(
    server: &AuthorizationServer,
    params: &Params,
) -> Result<Response, TokenError> {
    let code = required(params, "code")?;
    let redirect_uri = required(params, "redirect_uri")?.to_owned();
    let client_id = required(params, "client_id")?.to_owned();
    let verifier = required(params, "code_verifier")?.to_owned();
    if !pkce::is_valid_verifier(&verifier) {
        return Err(TokenError::new(
            "invalid_request",
            "code_verifier is malformed",
        ));
    }
    if !is_code(code) {
        return Err(TokenError::invalid_grant("the code is not valid"));
    }
    let code_hash = omni_core::digest::sha256_hex(code);
    let refresh_token = new_refresh_token();
    let refresh_hash = omni_core::digest::sha256_hex(&refresh_token);
    let now = server.now();
    let store = server.inner.store.clone();
    let decision = omni_core::spawn::must_complete(&server.inner.tracker, async move {
        store
            .write(move |tx| {
                let found = tx
                    .get_all::<AuthorizationRow>()?
                    .into_iter()
                    .find(|row| row.code_hash.as_deref() == Some(code_hash.as_str()));
                let Some(mut row) = found else {
                    return Ok::<_, StoreError>(Decision::Rejected("the code is not valid"));
                };
                if row.status == AuthorizationStatus::Consumed {
                    // RFC 6749 section 4.1.2: revoke what the code issued.
                    tx.delete::<GrantRow>(&row.id)?;
                    return Ok(Decision::Revoked(row.client_name));
                }
                if row.status != AuthorizationStatus::Approved
                    || row.code_expires_at.is_none_or(|at| now >= at)
                {
                    return Ok(Decision::Rejected("the code expired"));
                }
                if row.client_id != client_id {
                    return Ok(Decision::Rejected("the code was issued to another client"));
                }
                if row.redirect_uri != redirect_uri {
                    return Ok(Decision::Rejected("redirect_uri does not match"));
                }
                if !pkce::verify(&verifier, &row.code_challenge) {
                    return Ok(Decision::Rejected("code_verifier does not match"));
                }
                row.status = AuthorizationStatus::Consumed;
                tx.upsert(&row, UpsertOpts::default())?;
                let grant = GrantRow {
                    // The grant shares the request id, so a code replay finds it.
                    id: row.id.clone(),
                    client_id: row.client_id.clone(),
                    client_name: row.client_name.clone(),
                    resource: row.resource.clone(),
                    scope: row.scope.clone().or_else(|| Some(SCOPE.to_owned())),
                    refresh_hash,
                    refresh_expires_at: now + REFRESH_TTL_MS,
                    previous_refresh_hash: None,
                    rotated_at: None,
                    created_at: now,
                    last_used_at: None,
                    extra: Default::default(),
                };
                tx.upsert(&grant, UpsertOpts::default())?;
                Ok(Decision::Granted(Box::new(grant)))
            })
            .await
    })
    .await;
    match decision {
        Ok(Decision::Granted(grant)) => {
            tracing::info!(target: LOG, client = %grant.client_name, "OAuth grant issued");
            Ok(tokens(server, &grant, &refresh_token, now))
        }
        Ok(Decision::Revoked(client)) => {
            tracing::warn!(target: LOG, %client, "OAuth code replayed; its grant was revoked");
            Err(TokenError::invalid_grant("the code was already used"))
        }
        Ok(Decision::Rejected(reason)) => Err(TokenError::invalid_grant(reason)),
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth code exchange not recorded");
            Err(TokenError::server())
        }
    }
}

async fn refresh(server: &AuthorizationServer, params: &Params) -> Result<Response, TokenError> {
    let presented = required(params, "refresh_token")?;
    let client_id = required(params, "client_id")?.to_owned();
    if let Some(scope) = params.one("scope")?
        && scope.split_whitespace().any(|s| s != SCOPE)
    {
        return Err(TokenError::new("invalid_scope", "the only scope is omni"));
    }
    if !is_refresh_token(presented) {
        return Err(TokenError::invalid_grant("the refresh token is not valid"));
    }
    let presented_hash = omni_core::digest::sha256_hex(presented);
    let refresh_token = new_refresh_token();
    let refresh_hash = omni_core::digest::sha256_hex(&refresh_token);
    let now = server.now();
    let store = server.inner.store.clone();
    let decision = omni_core::spawn::must_complete(&server.inner.tracker, async move {
        store
            .write(move |tx| {
                let grants = tx.get_all::<GrantRow>()?;
                let current = grants.iter().find(|g| g.refresh_hash == presented_hash);
                let previous = grants
                    .iter()
                    .find(|g| g.previous_refresh_hash.as_deref() == Some(presented_hash.as_str()));
                let (mut grant, retried) = match (current, previous) {
                    (Some(grant), _) => (grant.clone(), false),
                    (None, Some(grant))
                        if grant.rotated_at.is_some_and(|at| {
                            now < at.saturating_add(PREVIOUS_REFRESH_GRACE_MS)
                        }) =>
                    {
                        (grant.clone(), true)
                    }
                    (None, Some(grant)) => {
                        // Reuse after the grace window: the token may be stolen.
                        tx.delete::<GrantRow>(&grant.id)?;
                        return Ok::<_, StoreError>(Decision::Revoked(grant.client_name.clone()));
                    }
                    (None, None) => {
                        return Ok(Decision::Rejected("the refresh token is not valid"));
                    }
                };
                if grant.client_id != client_id {
                    return Ok(Decision::Rejected("the refresh token is not valid"));
                }
                if now >= grant.refresh_expires_at {
                    return Ok(Decision::Rejected("the refresh token expired"));
                }
                if !retried {
                    // A retry of the replaced token keeps the original rotation window.
                    grant.previous_refresh_hash = Some(grant.refresh_hash.clone());
                    grant.rotated_at = Some(now);
                }
                grant.refresh_hash = refresh_hash;
                grant.refresh_expires_at = now + REFRESH_TTL_MS;
                tx.upsert(&grant, UpsertOpts::default())?;
                Ok(Decision::Granted(Box::new(grant)))
            })
            .await
    })
    .await;
    match decision {
        Ok(Decision::Granted(grant)) => Ok(tokens(server, &grant, &refresh_token, now)),
        Ok(Decision::Rejected(reason)) => Err(TokenError::invalid_grant(reason)),
        Ok(Decision::Revoked(client)) => {
            tracing::warn!(
                target: LOG,
                %client,
                "OAuth refresh token reused after rotation; its grant was revoked"
            );
            Err(TokenError::invalid_grant("the refresh token is not valid"))
        }
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth refresh not recorded");
            Err(TokenError::server())
        }
    }
}

fn new_refresh_token() -> String {
    format!("{REFRESH_PREFIX}{}", omni_core::ids::secure_id_b64url(32))
}

fn tokens(
    server: &AuthorizationServer,
    grant: &GrantRow,
    refresh_token: &str,
    now: i64,
) -> Response {
    let (access_token, _) = issue_access_token(&server.inner.secrets, &grant.id, now);
    let mut response = Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TOKEN_TTL_MS / 1000,
        "refresh_token": refresh_token,
        "scope": grant.scope.as_deref().unwrap_or(SCOPE),
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}
