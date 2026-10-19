//! The upstream OAuth client that connects OAuth-protected integrations.
//!
//! Discovery follows RFC 9728 (protected resource metadata, from the
//! `WWW-Authenticate` challenge or the well-known paths) and RFC 8414
//! (authorization server metadata, with the OpenID Connect fallbacks), or
//! MCP 2025-03-26's origin metadata when the server publishes no resource
//! metadata. Clients register dynamically (RFC 7591); servers that need a
//! pre-registered client are not supported. Authorization uses the code
//! grant with PKCE S256, the `resource` parameter (RFC 8707) and the `iss`
//! response parameter (RFC 9207).
//!
//! Every request goes through the SSRF-guarded public client with bounded
//! bodies and timeouts. Metadata GETs follow redirects (each hop is
//! revalidated); token and registration POSTs never do. Tokens, codes,
//! verifiers and client secrets never appear in errors, logs or `Debug`.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use omni_http::public::PublicHttpClient;
use omni_http::{BoundedResponse, HeaderValue, HttpError, Method, RedirectRule, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;

const LOG: &str = "MCP:Gateway";
const MAX_BODY: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DESCRIPTION_CHARS: usize = 300;
const PRM_WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";
const AS_WELL_KNOWN: &str = "/.well-known/oauth-authorization-server";
const OIDC_WELL_KNOWN: &str = "/.well-known/openid-configuration";

pub const AUTH_NONE: &str = "none";
pub const AUTH_POST: &str = "client_secret_post";
pub const AUTH_BASIC: &str = "client_secret_basic";

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("OAuth discovery failed: {0}")]
    Discovery(String),
    #[error("OAuth client registration failed: {0}")]
    Registration(String),
    /// The authorization response carried an `error` (the user or server declined).
    #[error("authorization was not granted: {}", describe(.error, .description.as_deref()))]
    Authorization {
        error: String,
        description: Option<String>,
    },
    /// The token endpoint answered with an OAuth error.
    #[error("token endpoint error (HTTP {status}): {}", describe(.error, .description.as_deref()))]
    TokenEndpoint {
        status: u16,
        error: String,
        description: Option<String>,
    },
    /// A refresh was rejected (`invalid_grant`, or the client registration
    /// is gone): the user must reconnect the integration.
    #[error("the authorization server rejected the refresh token ({}); reconnect the integration", describe(.error, .description.as_deref()))]
    InvalidGrant {
        error: String,
        description: Option<String>,
    },
    #[error("OAuth network error: {0}")]
    Network(String),
    #[error("OAuth request timed out")]
    Timeout,
    #[error("OAuth URL blocked: {0}")]
    Blocked(String),
    #[error("OAuth protocol error: {0}")]
    Protocol(String),
}

impl OAuthError {
    /// Network failures, timeouts and token endpoint 5xx, 408 and 429.
    pub fn is_transient(&self) -> bool {
        match self {
            OAuthError::Network(_) | OAuthError::Timeout => true,
            OAuthError::TokenEndpoint { status, .. } => {
                *status >= 500 || *status == 408 || *status == 429
            }
            _ => false,
        }
    }
}

fn describe(error: &str, description: Option<&str>) -> String {
    match description {
        Some(text) if !text.is_empty() => format!("{error}: {text}"),
        _ => error.to_owned(),
    }
}

/// What discovery found for one MCP URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// The protected resource identifier (RFC 8707 `resource`).
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// The challenge's `scope`, else the resource's `scopes_supported`,
    /// else the authorization server's.
    pub scopes: Vec<String>,
    /// `token_endpoint_auth_methods_supported` (RFC 8414 default: basic).
    pub token_auth_methods: Vec<String>,
    /// `authorization_response_iss_parameter_supported` (RFC 9207).
    pub iss_parameter_supported: bool,
}

/// A registered client.
#[derive(Clone, PartialEq, Eq)]
pub struct Registration {
    pub client_id: String,
    pub client_secret: Option<String>,
    /// [`AUTH_NONE`], [`AUTH_POST`] or [`AUTH_BASIC`].
    pub token_auth: String,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("token_auth", &self.token_auth)
            .finish()
    }
}

/// Tokens from a token response.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Epoch milliseconds, from `expires_in`.
    pub expires_at: Option<i64>,
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenAuth {
    None,
    Post,
    Basic,
}

impl TokenAuth {
    fn parse(value: &str) -> Option<Self> {
        match value {
            AUTH_NONE => Some(TokenAuth::None),
            AUTH_POST => Some(TokenAuth::Post),
            AUTH_BASIC => Some(TokenAuth::Basic),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct ResourceMetadata {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ServerMetadata {
    issuer: String,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    response_types_supported: Option<Vec<String>>,
    code_challenge_methods_supported: Option<Vec<String>>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: Option<String>,
    expires_in: Option<Value>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

/// The parameters of a `Bearer` challenge that discovery uses.
#[derive(Debug, Default, PartialEq, Eq)]
struct BearerChallenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

/// The upstream OAuth client; cheap to clone.
#[derive(Clone, Debug)]
pub struct OAuthClient {
    http: PublicHttpClient,
}

enum Fetched<T> {
    Found(T),
    Missing(u16),
}

impl OAuthClient {
    pub fn new(http: PublicHttpClient) -> Self {
        Self { http }
    }

    /// Finds the authorization server for `mcp_url`. `www_authenticate` is
    /// the challenge of the MCP server's 401, when there was one.
    pub async fn discover(
        &self,
        mcp_url: &Url,
        www_authenticate: Option<&str>,
    ) -> Result<Discovery, OAuthError> {
        let challenge = www_authenticate
            .map(parse_bearer_challenge)
            .unwrap_or_default();
        let resource = match &challenge.resource_metadata {
            Some(location) => {
                let url =
                    secure_url(location, "resource metadata URL").map_err(OAuthError::Discovery)?;
                match self.get_json::<ResourceMetadata>(&url).await? {
                    Fetched::Found(metadata) => Some(metadata),
                    Fetched::Missing(status) => {
                        return Err(OAuthError::Discovery(format!(
                            "resource metadata at {url} answered HTTP {status}"
                        )));
                    }
                }
            }
            None => self.find_resource_metadata(mcp_url).await?,
        };
        let challenge_scopes = challenge.scope.as_deref().map(split_scopes);
        match resource {
            Some(resource) => {
                if !same_resource(&resource.resource, mcp_url) {
                    return Err(OAuthError::Discovery(format!(
                        "the resource metadata names {} instead of {mcp_url}",
                        resource.resource
                    )));
                }
                let Some(issuer) = resource.authorization_servers.first() else {
                    return Err(OAuthError::Discovery(
                        "the resource metadata lists no authorization server".to_owned(),
                    ));
                };
                let metadata = self.server_metadata(issuer, &[issuer.as_str()]).await?;
                let scopes = challenge_scopes
                    .filter(|scopes| !scopes.is_empty())
                    .or_else(|| {
                        (!resource.scopes_supported.is_empty())
                            .then(|| resource.scopes_supported.clone())
                    })
                    .unwrap_or_else(|| metadata.scopes_supported.clone());
                discovery(resource.resource, issuer.clone(), metadata, scopes)
            }
            None => {
                // MCP 2025-03-26: the MCP server's origin is the authorization server.
                let origin = mcp_url.origin().ascii_serialization();
                let with_slash = format!("{origin}/");
                tracing::debug!(target: LOG, origin = %mcp_url.origin().ascii_serialization(), "No resource metadata; trying origin metadata");
                let metadata = self
                    .server_metadata(&origin, &[origin.as_str(), with_slash.as_str()])
                    .await?;
                let issuer = metadata.issuer.clone();
                let scopes = challenge_scopes
                    .filter(|scopes| !scopes.is_empty())
                    .unwrap_or_else(|| metadata.scopes_supported.clone());
                discovery(mcp_url.as_str().to_owned(), issuer, metadata, scopes)
            }
        }
    }

    /// The path-inserted well-known URL, then the root one; `None` when neither exists.
    async fn find_resource_metadata(
        &self,
        mcp_url: &Url,
    ) -> Result<Option<ResourceMetadata>, OAuthError> {
        secure_url(mcp_url.as_str(), "MCP URL").map_err(OAuthError::Discovery)?;
        let path = mcp_url.path().trim_end_matches('/');
        let mut candidates = Vec::new();
        if !path.is_empty() {
            candidates.push(well_known(mcp_url, &format!("{PRM_WELL_KNOWN}{path}")));
        }
        candidates.push(well_known(mcp_url, PRM_WELL_KNOWN));
        for url in candidates {
            match self.get_json::<ResourceMetadata>(&url).await? {
                Fetched::Found(metadata) => return Ok(Some(metadata)),
                Fetched::Missing(status) => {
                    tracing::debug!(target: LOG, %url, status, "No resource metadata here");
                }
            }
        }
        Ok(None)
    }

    /// RFC 8414 metadata for `issuer` (path-inserted OAuth, then OpenID
    /// Connect inserted and appended); its `issuer` must be one of `accepted`.
    async fn server_metadata(
        &self,
        issuer: &str,
        accepted: &[&str],
    ) -> Result<ServerMetadata, OAuthError> {
        let issuer_url =
            secure_url(issuer, "authorization server").map_err(OAuthError::Discovery)?;
        if issuer_url.query().is_some() || issuer_url.fragment().is_some() {
            return Err(OAuthError::Discovery(format!(
                "authorization server identifier {issuer} has a query or fragment"
            )));
        }
        let path = issuer_url.path().trim_end_matches('/');
        let candidates = if path.is_empty() {
            vec![
                well_known(&issuer_url, AS_WELL_KNOWN),
                well_known(&issuer_url, OIDC_WELL_KNOWN),
            ]
        } else {
            vec![
                well_known(&issuer_url, &format!("{AS_WELL_KNOWN}{path}")),
                well_known(&issuer_url, &format!("{OIDC_WELL_KNOWN}{path}")),
                well_known(&issuer_url, &format!("{path}{OIDC_WELL_KNOWN}")),
            ]
        };
        for url in candidates {
            match self.get_json::<ServerMetadata>(&url).await? {
                Fetched::Found(metadata) => {
                    if !accepted.contains(&metadata.issuer.as_str()) {
                        return Err(OAuthError::Discovery(format!(
                            "authorization server metadata at {url} names issuer {} instead of {issuer}",
                            metadata.issuer
                        )));
                    }
                    return Ok(metadata);
                }
                Fetched::Missing(status) => {
                    tracing::debug!(target: LOG, %url, status, "No authorization server metadata here");
                }
            }
        }
        Err(OAuthError::Discovery(format!(
            "no authorization server metadata found for {issuer}"
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &Url) -> Result<Fetched<T>, OAuthError> {
        let response = self
            .http
            .request(Method::GET, url.clone())
            .timeout(TIMEOUT)
            .header("accept", "application/json")
            .send_bounded(MAX_BODY)
            .await
            .map_err(|error| map_http(error, "metadata"))?;
        if !response.status.is_success() {
            return Ok(Fetched::Missing(response.status.as_u16()));
        }
        serde_json::from_slice(&response.body)
            .map(Fetched::Found)
            .map_err(|error| {
                OAuthError::Discovery(format!("metadata at {url} is invalid: {error}"))
            })
    }

    /// Registers a client (RFC 7591), preferring the public `none` method.
    pub async fn register(
        &self,
        d: &Discovery,
        redirect_uri: &str,
        client_name: &str,
    ) -> Result<Registration, OAuthError> {
        let Some(endpoint) = &d.registration_endpoint else {
            return Err(OAuthError::Registration(format!(
                "{} does not offer dynamic client registration; integrations that need a \
                 pre-registered OAuth client are not supported",
                d.issuer
            )));
        };
        let url =
            secure_url(endpoint, "registration endpoint").map_err(OAuthError::Registration)?;
        let offered = |method: &str| d.token_auth_methods.iter().any(|m| m == method);
        let method = [AUTH_NONE, AUTH_POST, AUTH_BASIC]
            .into_iter()
            .find(|method| offered(method))
            .ok_or_else(|| {
                OAuthError::Registration(format!(
                    "the token endpoint supports none of {AUTH_NONE}, {AUTH_POST} or {AUTH_BASIC} \
                     (it offers {})",
                    d.token_auth_methods.join(", ")
                ))
            })?;
        let body = json!({
            "client_name": client_name,
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": method,
        });
        let response = self
            .http
            .request(Method::POST, url)
            .timeout(TIMEOUT)
            .redirect(RedirectRule::Error)
            .header("accept", "application/json")
            .json(&body)
            .send_bounded(MAX_BODY)
            .await
            .map_err(|error| map_http(error, "registration endpoint"))?;
        if !response.status.is_success() {
            let (error, description) = error_body(&response.body, &[]);
            return Err(OAuthError::Registration(format!(
                "HTTP {}: {}",
                response.status.as_u16(),
                describe(&error, description.as_deref())
            )));
        }
        let parsed: RegistrationResponse =
            serde_json::from_slice(&response.body).map_err(|_| {
                OAuthError::Registration(
                    "the registration response is not the expected JSON".to_owned(),
                )
            })?;
        if parsed.client_id.is_empty() {
            return Err(OAuthError::Registration(
                "the registration response has an empty client_id".to_owned(),
            ));
        }
        let token_auth = parsed
            .token_endpoint_auth_method
            .unwrap_or_else(|| method.to_owned());
        let secret = parsed.client_secret.filter(|secret| !secret.is_empty());
        match TokenAuth::parse(&token_auth) {
            None => Err(OAuthError::Registration(format!(
                "the server registered the unsupported token endpoint auth method {}",
                sanitize(&token_auth, &[])
            ))),
            Some(TokenAuth::None) => Ok(Registration {
                client_id: parsed.client_id,
                client_secret: None,
                token_auth,
            }),
            Some(TokenAuth::Post | TokenAuth::Basic) if secret.is_none() => {
                Err(OAuthError::Registration(format!(
                    "the server registered {token_auth} without a client secret"
                )))
            }
            Some(_) => Ok(Registration {
                client_id: parsed.client_id,
                client_secret: secret,
                token_auth,
            }),
        }
    }

    /// The authorization URL to send the browser to.
    pub fn authorize_url(
        &self,
        d: &Discovery,
        r: &Registration,
        redirect_uri: &str,
        state: &str,
        challenge: &str,
    ) -> Result<Url, OAuthError> {
        let mut url = secure_url(&d.authorization_endpoint, "authorization endpoint")
            .map_err(OAuthError::Protocol)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &r.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("state", state)
                .append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &d.resource);
            if !d.scopes.is_empty() {
                query.append_pair("scope", &d.scopes.join(" "));
            }
        }
        Ok(url)
    }

    /// The code in an authorization response (RFC 9207 `iss`, then `state`,
    /// then `error`).
    pub fn callback_code(
        query: &HashMap<String, String>,
        expected_state: &str,
        issuer: &str,
        iss_supported: bool,
    ) -> Result<String, OAuthError> {
        match query.get("iss") {
            Some(iss) if iss != issuer => {
                return Err(OAuthError::Protocol(
                    "the authorization response came from a different issuer".to_owned(),
                ));
            }
            None if iss_supported => {
                return Err(OAuthError::Protocol(
                    "the authorization response has no iss parameter".to_owned(),
                ));
            }
            _ => {}
        }
        let state = query.get("state").map(String::as_str).unwrap_or("");
        let state_matches = !expected_state.is_empty()
            && state.len() == expected_state.len()
            && bool::from(state.as_bytes().ct_eq(expected_state.as_bytes()));
        if !state_matches {
            return Err(OAuthError::Protocol(
                "the authorization response state does not match".to_owned(),
            ));
        }
        if let Some(error) = query.get("error") {
            return Err(OAuthError::Authorization {
                error: sanitize(error, &[]),
                description: query
                    .get("error_description")
                    .map(|text| sanitize(text, &[])),
            });
        }
        match query.get("code") {
            Some(code) if !code.is_empty() => Ok(code.clone()),
            _ => Err(OAuthError::Protocol(
                "the authorization response has no code".to_owned(),
            )),
        }
    }

    /// Exchanges an authorization code.
    #[allow(clippy::too_many_arguments)]
    pub async fn exchange(
        &self,
        token_endpoint: &str,
        r: &Registration,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        resource: &str,
        now: i64,
    ) -> Result<TokenSet, OAuthError> {
        let form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("resource", resource),
        ];
        self.token_request(token_endpoint, r, form, &[code, verifier], false, now)
            .await
    }

    /// Refreshes an access token; a response without a refresh token keeps the old one.
    pub async fn refresh(
        &self,
        token_endpoint: &str,
        r: &Registration,
        refresh_token: &str,
        resource: &str,
        now: i64,
    ) -> Result<TokenSet, OAuthError> {
        let form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("resource", resource),
        ];
        let mut tokens = self
            .token_request(token_endpoint, r, form, &[refresh_token], true, now)
            .await?;
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(refresh_token.to_owned());
        }
        Ok(tokens)
    }

    async fn token_request<'a>(
        &self,
        token_endpoint: &str,
        r: &'a Registration,
        mut form: Vec<(&'a str, &'a str)>,
        sent: &[&'a str],
        refresh: bool,
        now: i64,
    ) -> Result<TokenSet, OAuthError> {
        let url = secure_url(token_endpoint, "token endpoint").map_err(OAuthError::Protocol)?;
        let auth = TokenAuth::parse(&r.token_auth).ok_or_else(|| {
            OAuthError::Protocol(format!(
                "unsupported token endpoint auth method {}",
                sanitize(&r.token_auth, &[])
            ))
        })?;
        let secret = match auth {
            TokenAuth::None => None,
            TokenAuth::Post | TokenAuth::Basic => Some(
                r.client_secret
                    .as_deref()
                    .filter(|secret| !secret.is_empty())
                    .ok_or_else(|| {
                        OAuthError::Protocol(format!(
                            "{} needs a client secret and the registration has none",
                            r.token_auth
                        ))
                    })?,
            ),
        };
        let mut request = self
            .http
            .request(Method::POST, url)
            .timeout(TIMEOUT)
            .redirect(RedirectRule::Error)
            .header("accept", "application/json");
        match (auth, secret) {
            (TokenAuth::Basic, Some(secret)) => {
                request = request.header("authorization", basic_auth(&r.client_id, secret)?);
            }
            (TokenAuth::Post, Some(secret)) => {
                form.push(("client_id", r.client_id.as_str()));
                form.push(("client_secret", secret));
            }
            _ => form.push(("client_id", r.client_id.as_str())),
        }
        let response = request
            .form(&form)
            .send_bounded(MAX_BODY)
            .await
            .map_err(|error| map_http(error, "token endpoint"))?;
        let mut redact: Vec<&str> = sent.to_vec();
        redact.extend(secret);
        token_set(&response, &redact, refresh, now)
    }
}

fn discovery(
    resource: String,
    issuer: String,
    metadata: ServerMetadata,
    scopes: Vec<String>,
) -> Result<Discovery, OAuthError> {
    let endpoint = |value: Option<&str>, what: &str| -> Result<String, OAuthError> {
        let value = value.ok_or_else(|| {
            OAuthError::Discovery(format!("the authorization server metadata has no {what}"))
        })?;
        secure_url(value, what)
            .map(|url| url.to_string())
            .map_err(OAuthError::Discovery)
    };
    let authorization_endpoint = endpoint(
        metadata.authorization_endpoint.as_deref(),
        "authorization_endpoint",
    )?;
    let token_endpoint = endpoint(metadata.token_endpoint.as_deref(), "token_endpoint")?;
    let registration_endpoint = match metadata.registration_endpoint.as_deref() {
        Some(value) => Some(endpoint(Some(value), "registration_endpoint")?),
        None => None,
    };
    if let Some(types) = &metadata.response_types_supported
        && !types.iter().any(|t| t == "code")
    {
        return Err(OAuthError::Discovery(
            "the authorization server does not support the code response type".to_owned(),
        ));
    }
    if let Some(methods) = &metadata.code_challenge_methods_supported
        && !methods.iter().any(|m| m == "S256")
    {
        return Err(OAuthError::Discovery(
            "the authorization server does not support PKCE S256".to_owned(),
        ));
    }
    Ok(Discovery {
        resource,
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        scopes,
        token_auth_methods: metadata
            .token_endpoint_auth_methods_supported
            .unwrap_or_else(|| vec![AUTH_BASIC.to_owned()]),
        iss_parameter_supported: metadata.authorization_response_iss_parameter_supported,
    })
}

/// Parses a token endpoint response. A 2xx body carrying `error` and no
/// `access_token` is an error too (some servers answer errors with 200).
fn token_set(
    response: &BoundedResponse,
    redact: &[&str],
    refresh: bool,
    now: i64,
) -> Result<TokenSet, OAuthError> {
    let status = response.status.as_u16();
    let value: Option<Value> = serde_json::from_slice(&response.body).ok();
    let is_error_body = value.as_ref().is_some_and(|body| {
        body.get("error").is_some_and(Value::is_string) && body.get("access_token").is_none()
    });
    if !response.status.is_success() || is_error_body {
        let (error, description) = error_body(&response.body, redact);
        let reconnect = matches!(
            error.as_str(),
            "invalid_grant" | "invalid_client" | "unauthorized_client"
        );
        if refresh && reconnect {
            return Err(OAuthError::InvalidGrant { error, description });
        }
        return Err(OAuthError::TokenEndpoint {
            status,
            error,
            description,
        });
    }
    let parsed: TokenResponse = value
        .and_then(|body| serde_json::from_value(body).ok())
        .ok_or_else(|| {
            OAuthError::Protocol("the token response is not the expected JSON".to_owned())
        })?;
    if parsed.access_token.is_empty() {
        return Err(OAuthError::Protocol(
            "the token response has an empty access_token".to_owned(),
        ));
    }
    if let Some(kind) = &parsed.token_type
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(OAuthError::Protocol(format!(
            "the token response has unsupported token_type {}",
            sanitize(kind, redact)
        )));
    }
    let expires_at = parsed
        .expires_in
        .as_ref()
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|secs| secs.is_finite())
        // `as` saturates; a negative lifetime means "now".
        .map(|secs| now.saturating_add((secs.max(0.0) * 1000.0) as i64));
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token.filter(|token| !token.is_empty()),
        expires_at,
        scope: parsed.scope,
    })
}

/// `error` and `error_description` of an OAuth error body, sanitized.
fn error_body(body: &[u8], redact: &[&str]) -> (String, Option<String>) {
    let parsed: Option<ErrorBody> = serde_json::from_slice(body).ok();
    let error = parsed
        .as_ref()
        .and_then(|body| body.error.as_deref())
        .map(|error| sanitize(error, redact))
        .filter(|error| !error.is_empty())
        .unwrap_or_else(|| "invalid_response".to_owned());
    let description = parsed
        .and_then(|body| body.error_description)
        .map(|text| sanitize(&text, redact))
        .filter(|text| !text.is_empty());
    (error, description)
}

/// Server-supplied text for an error: control characters dropped, at most
/// 300 characters, and any value Omni sent (a code, verifier, token or
/// secret) replaced.
fn sanitize(text: &str, redact: &[&str]) -> String {
    let mut clean: String = text.chars().filter(|c| !c.is_control()).collect();
    for secret in redact.iter().filter(|secret| !secret.is_empty()) {
        if clean.contains(secret) {
            clean = clean.replace(secret, "<redacted>");
        }
    }
    if clean.chars().count() > MAX_DESCRIPTION_CHARS {
        clean = clean.chars().take(MAX_DESCRIPTION_CHARS).collect();
        clean.push('…');
    }
    clean
}

fn map_http(error: HttpError, what: &str) -> OAuthError {
    match error {
        HttpError::Timeout => OAuthError::Timeout,
        HttpError::Network(message) => OAuthError::Network(message),
        HttpError::Blocked(message) | HttpError::InvalidUrl(message) => {
            OAuthError::Blocked(message)
        }
        HttpError::TooLarge { limit } => {
            OAuthError::Protocol(format!("the {what} response exceeds {limit} bytes"))
        }
        // Only a redirect under `RedirectRule::Error`; its body is dropped.
        HttpError::Status { status, .. } => OAuthError::Protocol(format!(
            "the {what} answered with a redirect (HTTP {status}), which is not followed"
        )),
        HttpError::Decode(message) => OAuthError::Protocol(message),
    }
}

/// `Basic base64(urlencode(id):urlencode(secret))` (RFC 6749 section 2.3.1), marked sensitive.
fn basic_auth(client_id: &str, secret: &str) -> Result<HeaderValue, OAuthError> {
    let encode =
        |value: &str| url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let credentials = STANDARD.encode(format!("{}:{}", encode(client_id), encode(secret)));
    let mut value = HeaderValue::from_str(&format!("Basic {credentials}")).map_err(|_| {
        OAuthError::Protocol("the client credentials are not a valid header".to_owned())
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// An `https` URL, or `http` on a loopback host (local development and tests).
fn secure_url(value: &str, what: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| format!("{what} {value} is not a URL: {error}"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" if url.host().is_some() => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(format!("{what} {value} must use https")),
    }
}

/// `url`'s origin with `path`.
fn well_known(url: &Url, path: &str) -> Url {
    let mut out = url.clone();
    out.set_path(path);
    out.set_query(None);
    out.set_fragment(None);
    out
}

/// Whether a resource identifier names `mcp_url` (a trailing slash aside).
fn same_resource(resource: &str, mcp_url: &Url) -> bool {
    let normalize = |url: &Url| {
        let mut url = url.clone();
        url.set_fragment(None);
        url.as_str().trim_end_matches('/').to_owned()
    };
    Url::parse(resource).is_ok_and(|resource| normalize(&resource) == normalize(mcp_url))
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_owned).collect()
}

/// The `resource_metadata` and `scope` parameters of the `Bearer` challenge
/// in a `WWW-Authenticate` value (which may hold several challenges).
fn parse_bearer_challenge(header: &str) -> BearerChallenge {
    let chars: Vec<char> = header.chars().collect();
    let mut i = 0;
    let mut in_bearer = false;
    let mut params: HashMap<String, String> = HashMap::new();
    let skip = |i: &mut usize, pred: &dyn Fn(char) -> bool| {
        while *i < chars.len() && pred(chars[*i]) {
            *i += 1;
        }
    };
    while i < chars.len() {
        skip(&mut i, &|c| c.is_whitespace() || c == ',');
        let start = i;
        skip(&mut i, &|c| !(c.is_whitespace() || c == ',' || c == '='));
        let name: String = chars[start..i].iter().collect();
        skip(&mut i, &char::is_whitespace);
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            skip(&mut i, &char::is_whitespace);
            let mut value = String::new();
            if i < chars.len() && chars[i] == '"' {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
                i += 1;
            } else {
                let start = i;
                skip(&mut i, &|c| !(c.is_whitespace() || c == ','));
                value = chars[start..i].iter().collect();
            }
            if in_bearer {
                params.entry(name.to_ascii_lowercase()).or_insert(value);
            }
        } else if !name.is_empty() {
            in_bearer = name.eq_ignore_ascii_case("bearer");
        } else {
            i += 1;
        }
    }
    BearerChallenge {
        resource_metadata: params.remove("resource_metadata"),
        scope: params.remove("scope"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_challenges_are_parsed() {
        let parsed = parse_bearer_challenge(
            r#"Basic realm="x", Bearer error="invalid_token", resource_metadata="https://a.example/.well-known/oauth-protected-resource/mcp", scope="read write""#,
        );
        assert_eq!(
            parsed.resource_metadata.as_deref(),
            Some("https://a.example/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(parsed.scope.as_deref(), Some("read write"));
        let token = parse_bearer_challenge("Bearer resource_metadata=https://a.example/prm");
        assert_eq!(
            token.resource_metadata.as_deref(),
            Some("https://a.example/prm")
        );
        let escaped = parse_bearer_challenge(r#"Bearer scope="a \"b\"""#);
        assert_eq!(escaped.scope.as_deref(), Some(r#"a "b""#));
        let other = parse_bearer_challenge(r#"Basic resource_metadata="https://a.example/prm""#);
        assert_eq!(other, BearerChallenge::default());
        assert_eq!(parse_bearer_challenge(""), BearerChallenge::default());
        assert_eq!(parse_bearer_challenge("=,= \""), BearerChallenge::default());
    }

    #[test]
    fn resources_match_up_to_a_trailing_slash() {
        let mcp = Url::parse("https://a.example/mcp").unwrap();
        assert!(same_resource("https://a.example/mcp/", &mcp));
        assert!(same_resource("https://A.example/mcp", &mcp));
        assert!(!same_resource("https://a.example/", &mcp));
        assert!(!same_resource("https://b.example/mcp", &mcp));
        assert!(!same_resource("not a url", &mcp));
    }

    #[test]
    fn only_https_or_loopback_http_is_secure() {
        assert!(secure_url("https://a.example/t", "x").is_ok());
        assert!(secure_url("http://127.0.0.1:9/t", "x").is_ok());
        assert!(secure_url("http://localhost/t", "x").is_ok());
        assert!(secure_url("http://a.example/t", "x").is_err());
        assert!(secure_url("ftp://a.example/t", "x").is_err());
    }

    #[test]
    fn sanitizing_redacts_sent_values_and_bounds_length() {
        assert_eq!(
            sanitize("bad token ort_secret\n here", &["ort_secret"]),
            "bad token <redacted> here"
        );
        let long = "x".repeat(400);
        assert_eq!(
            sanitize(&long, &[]).chars().count(),
            MAX_DESCRIPTION_CHARS + 1
        );
    }
}
