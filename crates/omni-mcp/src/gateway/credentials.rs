//! Upstream credentials: validation, sealed storage and request headers.
//!
//! A stored credential is bound to the transport origin it was entered for
//! and to the integration's auth kind; when either changes it is never used
//! again (and the change clears it). OAuth access tokens are refreshed a
//! minute before they expire and once after a 401, one refresh at a time
//! per integration. A refresh and the write of its rotated tokens run to
//! completion even if the request that needed them goes away, and every
//! write takes the same per-integration lock. `Record` mode never refreshes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use omni_api::gateway::GatewayErrorKind;
use omni_core::clock::SharedClock;
use omni_http::{HeaderName, HeaderValue, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};

use super::bundled::BundledRegistry;
use super::client::http::AuthHeaders;
use super::error::GatewayError;
use super::model::{
    AuthKind, CredentialRow, CredentialSecrets, IntegrationRow, OAuthMeta, Transport,
};
use super::secrets::GatewaySecrets;

const MAX_HEADERS: usize = 10;
const MAX_VALUE: usize = 8 * 1024;
/// Refresh when the access token expires within this.
const REFRESH_MARGIN_MS: i64 = 60_000;
/// Headers the transport owns.
const RESERVED_HEADERS: [&str; 13] = [
    "host",
    "content-length",
    "content-type",
    "accept",
    "connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// New tokens from a refresh.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// The refresh token was rejected: the owner must reconnect.
    #[error("the authorization server rejected the refresh token")]
    Rejected,
    #[error("token refresh failed: {0}")]
    Failed(String),
}

/// Exchanges a refresh token (implemented over [`super::oauth::client`]).
pub trait TokenRefresher: Send + Sync {
    fn refresh<'a>(
        &'a self,
        meta: &'a OAuthMeta,
        refresh_token: &'a str,
        client_secret: Option<&'a str>,
        now: i64,
    ) -> BoxFuture<'a, Result<RefreshedTokens, RefreshError>>;
}

/// The checked header pairs, or why they are invalid.
pub fn validate_headers(pairs: &[(String, String)]) -> Result<AuthHeaders, String> {
    if pairs.len() > MAX_HEADERS {
        return Err(format!("at most {MAX_HEADERS} headers"));
    }
    let mut headers = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (name, value) in pairs {
        let lower = name.trim().to_ascii_lowercase();
        let header = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| format!("{name:?} is not a valid header name"))?;
        if RESERVED_HEADERS.contains(&lower.as_str()) {
            return Err(format!("{lower} is set by Omni and cannot be configured"));
        }
        if !seen.insert(lower.clone()) {
            return Err(format!("{lower} is listed twice"));
        }
        if value.is_empty() || value.len() > MAX_VALUE {
            return Err(format!(
                "{lower} needs a value of at most {MAX_VALUE} bytes"
            ));
        }
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| format!("{lower} has characters a header value cannot contain"))?;
        value.set_sensitive(true);
        headers.push((header, value));
    }
    Ok(headers)
}

/// The checked variables for a bundled server.
pub fn validate_env(
    pairs: &[(String, String)],
    allowed: &[String],
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (name, value) in pairs {
        if !allowed.contains(name) {
            return Err(format!(
                "{name} is not a variable this server reads ({})",
                allowed.join(", ")
            ));
        }
        if value.len() > MAX_VALUE || value.contains('\0') {
            return Err(format!("{name} needs a value of at most {MAX_VALUE} bytes"));
        }
        if !out.iter().any(|(n, _): &(String, String)| n == name) {
            out.push((name.clone(), value.clone()));
        }
    }
    Ok(out)
}

fn context(integration: &str) -> String {
    format!("credential:{integration}")
}

/// What a session needs to authenticate.
#[derive(Clone, Default)]
pub struct Resolved {
    pub headers: AuthHeaders,
    pub env: Vec<(String, String)>,
    /// Whether an OAuth refresh could fix a 401.
    pub refreshable: bool,
}

struct Inner {
    store: Store,
    clock: SharedClock,
    secrets: GatewaySecrets,
    refresher: Option<Arc<dyn TokenRefresher>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    tracker: tokio_util::task::TaskTracker,
    side_effects: SideEffectMode,
}

/// Credential storage; cheap to clone.
#[derive(Clone)]
pub struct Credentials {
    inner: Arc<Inner>,
}

impl Credentials {
    pub fn new(
        store: Store,
        clock: SharedClock,
        secrets: GatewaySecrets,
        refresher: Option<Arc<dyn TokenRefresher>>,
        tracker: tokio_util::task::TaskTracker,
        side_effects: SideEffectMode,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                clock,
                secrets,
                refresher,
                locks: Mutex::new(HashMap::new()),
                tracker,
                side_effects,
            }),
        }
    }

    fn lock_for(&self, integration: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(integration.to_owned())
            .or_default()
            .clone()
    }

    pub async fn row(&self, integration: &str) -> Result<Option<CredentialRow>, StoreError> {
        let key = integration.to_owned();
        self.inner
            .store
            .read(move |docs| docs.get::<CredentialRow>(&key))
            .await
    }

    /// The stored credential, when it belongs to the integration as configured now.
    pub async fn current(
        &self,
        integration: &IntegrationRow,
    ) -> Result<Option<(CredentialRow, CredentialSecrets)>, StoreError> {
        let Some(row) = self.row(&integration.id).await? else {
            return Ok(None);
        };
        if row.origin != integration.transport.credential_origin() || row.auth != integration.auth {
            return Ok(None);
        }
        let secrets = self
            .inner
            .secrets
            .open(&context(&integration.id), &row.sealed)
            .and_then(|json| serde_json::from_str::<CredentialSecrets>(&json).ok());
        Ok(secrets.map(|secrets| (row, secrets)))
    }

    async fn write(
        &self,
        integration: &IntegrationRow,
        meta: CredentialMeta,
        secrets: &CredentialSecrets,
    ) -> Result<(), StoreError> {
        let row = CredentialRow {
            integration: integration.id.clone(),
            origin: integration.transport.credential_origin(),
            auth: integration.auth,
            sealed: self.inner.secrets.seal(
                &context(&integration.id),
                &serde_json::to_string(secrets).unwrap_or_default(),
            ),
            header_names: secrets.headers.iter().map(|(n, _)| n.clone()).collect(),
            env_names: secrets.env.iter().map(|(n, _)| n.clone()).collect(),
            oauth: meta.oauth,
            updated_at: self.inner.clock.now_ms(),
            extra: Extra::default(),
        };
        self.inner
            .store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
    }

    /// Stores static headers or variables (already validated).
    pub async fn save_static(
        &self,
        integration: &IntegrationRow,
        headers: Vec<(String, String)>,
        env: Vec<(String, String)>,
    ) -> Result<(), StoreError> {
        let lock = self.lock_for(&integration.id);
        let _serialized = lock.lock().await;
        let secrets = CredentialSecrets {
            headers: headers
                .into_iter()
                .map(|(n, v)| (n.trim().to_ascii_lowercase(), v))
                .collect(),
            env,
            ..CredentialSecrets::default()
        };
        self.write(integration, CredentialMeta { oauth: None }, &secrets)
            .await
    }

    /// Stores a completed OAuth connection.
    pub async fn save_oauth(
        &self,
        integration: &IntegrationRow,
        meta: OAuthMeta,
        access_token: String,
        refresh_token: Option<String>,
        client_secret: Option<String>,
    ) -> Result<(), StoreError> {
        let lock = self.lock_for(&integration.id);
        let _serialized = lock.lock().await;
        let secrets = CredentialSecrets {
            access_token: Some(access_token),
            refresh_token,
            client_secret,
            ..CredentialSecrets::default()
        };
        self.write(integration, CredentialMeta { oauth: Some(meta) }, &secrets)
            .await
    }

    pub async fn clear(&self, integration: &str) -> Result<bool, StoreError> {
        let lock = self.lock_for(integration);
        let _serialized = lock.lock().await;
        let key = integration.to_owned();
        self.inner
            .store
            .write(move |tx| tx.delete::<CredentialRow>(&key))
            .await
    }

    /// Whether everything the auth kind needs is stored.
    pub async fn complete(&self, integration: &IntegrationRow) -> Result<bool, StoreError> {
        Ok(match integration.auth {
            AuthKind::None => true,
            _ => self
                .current(integration)
                .await?
                .is_some_and(|(row, secrets)| match integration.auth {
                    AuthKind::Headers => !secrets.headers.is_empty(),
                    AuthKind::Env => !secrets.env.is_empty(),
                    AuthKind::OAuth => row.oauth.is_some() && secrets.access_token.is_some(),
                    AuthKind::None => true,
                }),
        })
    }

    /// What to authenticate with now; refreshes an expiring OAuth token
    /// first, or unconditionally with `force` (after a 401).
    pub async fn resolve(
        &self,
        integration: &IntegrationRow,
        bundled: &BundledRegistry,
        force: bool,
    ) -> Result<Resolved, GatewayError> {
        let missing = || {
            GatewayError::new(
                GatewayErrorKind::AuthRequired,
                "The integration's credentials are not set.",
            )
            .hint(match integration.auth {
                AuthKind::OAuth => "Connect it on Omni's MCP gateway page.",
                _ => "Set them on Omni's MCP gateway page.",
            })
        };
        match integration.auth {
            AuthKind::None => Ok(Resolved::default()),
            AuthKind::Headers => {
                let (_, secrets) = self.current(integration).await?.ok_or_else(missing)?;
                let headers = validate_headers(&secrets.headers).map_err(GatewayError::config)?;
                Ok(Resolved {
                    headers,
                    ..Resolved::default()
                })
            }
            AuthKind::Env => {
                let (_, secrets) = self.current(integration).await?.ok_or_else(missing)?;
                let allowed = match &integration.transport {
                    Transport::Stdio { server } => bundled
                        .get(server)
                        .map(|s| s.env.clone())
                        .unwrap_or_default(),
                    Transport::Http { .. } => Vec::new(),
                };
                let env = validate_env(&secrets.env, &allowed).map_err(GatewayError::config)?;
                Ok(Resolved {
                    env,
                    ..Resolved::default()
                })
            }
            AuthKind::OAuth => {
                let this = self.clone();
                let integration = integration.clone();
                omni_core::spawn::must_complete(&self.inner.tracker, async move {
                    this.resolve_oauth(&integration, force).await
                })
                .await
            }
        }
    }

    async fn resolve_oauth(
        &self,
        integration: &IntegrationRow,
        force: bool,
    ) -> Result<Resolved, GatewayError> {
        let missing = || {
            GatewayError::new(
                GatewayErrorKind::AuthRequired,
                "The integration's credentials are not set.",
            )
            .hint("Connect it on Omni's MCP gateway page.")
        };
        let lock = self.lock_for(&integration.id);
        let _serialized = lock.lock().await;
        let (row, secrets) = self.current(integration).await?.ok_or_else(&missing)?;
        let meta = row.oauth.clone().ok_or_else(&missing)?;
        let now = self.inner.clock.now_ms();
        let expiring = meta
            .access_expires_at
            .is_some_and(|at| at - REFRESH_MARGIN_MS <= now);
        let mut access = secrets.access_token.clone().ok_or_else(&missing)?;
        let recording = self.inner.side_effects == SideEffectMode::Record;
        let refreshable =
            secrets.refresh_token.is_some() && self.inner.refresher.is_some() && !recording;
        if (force || expiring)
            && !recording
            && let (Some(refresh_token), Some(refresher)) =
                (secrets.refresh_token.as_deref(), &self.inner.refresher)
        {
            match refresher
                .refresh(&meta, refresh_token, secrets.client_secret.as_deref(), now)
                .await
            {
                Ok(tokens) => {
                    access = tokens.access_token.clone();
                    let meta = OAuthMeta {
                        access_expires_at: tokens.expires_at,
                        scope: tokens.scope.clone().or(meta.scope.clone()),
                        refreshable: tokens.refresh_token.is_some() || meta.refreshable,
                        ..meta
                    };
                    let renewed = CredentialSecrets {
                        access_token: Some(tokens.access_token),
                        refresh_token: tokens.refresh_token.or(secrets.refresh_token.clone()),
                        ..secrets.clone()
                    };
                    self.write(integration, CredentialMeta { oauth: Some(meta) }, &renewed)
                        .await?;
                }
                Err(RefreshError::Rejected) => {
                    return Err(GatewayError::new(
                        GatewayErrorKind::AuthRequired,
                        "The integration's authorization expired or was revoked.",
                    )
                    .hint("Reconnect it on Omni's MCP gateway page."));
                }
                Err(RefreshError::Failed(message))
                    if force || meta.access_expires_at.is_some_and(|at| at <= now) =>
                {
                    return Err(GatewayError::new(GatewayErrorKind::Network, message)
                        .hint("Omni will retry the token refresh on the next request."));
                }
                // The current token is still valid for a little while.
                Err(RefreshError::Failed(_)) => {}
            }
        } else if force || meta.access_expires_at.is_some_and(|at| at <= now) {
            return Err(GatewayError::new(
                GatewayErrorKind::AuthRequired,
                "The integration's access token expired and cannot be refreshed.",
            )
            .hint("Reconnect it on Omni's MCP gateway page."));
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {access}")).map_err(|_| {
            GatewayError::config("The stored access token is not a valid header value.")
        })?;
        value.set_sensitive(true);
        Ok(Resolved {
            headers: vec![(HeaderName::from_static("authorization"), value)],
            env: Vec::new(),
            refreshable,
        })
    }
}

struct CredentialMeta {
    oauth: Option<OAuthMeta>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_rules() {
        let ok = validate_headers(&[("Authorization".into(), "Bearer abc".into())]).unwrap();
        assert_eq!(ok[0].0.as_str(), "authorization");
        assert!(ok[0].1.is_sensitive());
        for bad in [
            vec![("Host".to_owned(), "x".to_owned())],
            vec![("Bad Name".to_owned(), "x".to_owned())],
            vec![("x-key".to_owned(), "a\r\nb".to_owned())],
            vec![("x-key".to_owned(), String::new())],
            vec![
                ("x-a".to_owned(), "1".to_owned()),
                ("X-A".to_owned(), "2".to_owned()),
            ],
            vec![("mcp-session-id".to_owned(), "1".to_owned())],
        ] {
            assert!(validate_headers(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn env_rules() {
        let allowed = vec!["KARAKEEP_API_KEY".to_owned()];
        assert!(validate_env(&[("KARAKEEP_API_KEY".into(), "k".into())], &allowed).is_ok());
        assert!(validate_env(&[("PATH".into(), "/tmp".into())], &allowed).is_err());
        assert!(validate_env(&[("KARAKEEP_API_KEY".into(), "a\0b".into())], &allowed).is_err());
    }
}
