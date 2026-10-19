//! Connecting an OAuth integration from the UI.
//!
//! Start discovers the integration's authorization server (from its 401
//! challenge or well-known metadata), registers a client for Omni's callback
//! (RFC 7591), stores the PKCE verifier sealed with a single-use `state`, and
//! returns the authorization URL. The callback checks `state` and `iss`,
//! exchanges the code with the verifier and `resource`, and stores the
//! tokens bound to the integration's origin. A flow expires after ten
//! minutes and cannot be replayed.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use omni_http::{SideEffectMode, Url};
use omni_store::StoreError;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde::{Deserialize, Serialize};

use super::client::ClientError;
use super::client::http::HttpSession;
use super::credentials::{RefreshError, RefreshedTokens, TokenRefresher};
use super::model::{AuthKind, OAuthMeta, Transport, UpstreamFlowRow};
use super::oauth::client::{OAuthClient, OAuthError, Registration};
use super::oauth::pkce;
use super::service::{AdminError, Gateway};

const LOG: &str = "MCP:Gateway";
pub const CALLBACK_PATH: &str = "/gateway/oauth/callback";
const FLOW_TTL_MS: i64 = 10 * 60 * 1000;
const CLIENT_NAME: &str = "Omni MCP gateway";

#[derive(Serialize, Deserialize)]
struct FlowSecrets {
    verifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
}

fn flow_context(state: &str) -> String {
    format!("flow:{state}")
}

/// Refreshes OAuth tokens through [`OAuthClient`].
pub struct OAuthRefresher {
    client: OAuthClient,
}

impl OAuthRefresher {
    pub fn new(client: OAuthClient) -> Arc<Self> {
        Arc::new(Self { client })
    }
}

impl TokenRefresher for OAuthRefresher {
    fn refresh<'a>(
        &'a self,
        meta: &'a OAuthMeta,
        refresh_token: &'a str,
        client_secret: Option<&'a str>,
        now: i64,
    ) -> BoxFuture<'a, Result<RefreshedTokens, RefreshError>> {
        Box::pin(async move {
            let registration = Registration {
                client_id: meta.client_id.clone(),
                client_secret: client_secret.map(str::to_owned),
                token_auth: meta.token_auth.clone(),
            };
            match self
                .client
                .refresh(
                    &meta.token_endpoint,
                    &registration,
                    refresh_token,
                    &meta.resource,
                    now,
                )
                .await
            {
                Ok(tokens) => Ok(RefreshedTokens {
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                    expires_at: tokens.expires_at,
                    scope: tokens.scope,
                }),
                Err(OAuthError::InvalidGrant { .. }) => Err(RefreshError::Rejected),
                Err(error) => Err(RefreshError::Failed(error.to_string())),
            }
        })
    }
}

/// The callback URL on `origin` (the public origin when configured).
pub fn redirect_uri(origin: &str) -> String {
    format!("{}{CALLBACK_PATH}", origin.trim_end_matches('/'))
}

/// The `WWW-Authenticate` challenge an unauthenticated `initialize` gets.
async fn probe_challenge(gateway: &Gateway, url: &Url) -> Option<String> {
    let deps = gateway.deps();
    let (notices, _) = tokio::sync::mpsc::unbounded_channel();
    let stop = deps.shutdown.child_token();
    let probe = HttpSession::connect(
        deps.http.clone(),
        url.clone(),
        Vec::new(),
        notices,
        deps.tracker.clone(),
        stop.clone(),
    )
    .await;
    stop.cancel();
    match probe {
        Err(ClientError::Unauthorized { www_authenticate }) => www_authenticate,
        _ => None,
    }
}

/// Starts connecting integration `id`; the URL to send the browser to.
const RECORDING: &str = "Omni is recording side effects, so it does not connect integrations.";

pub async fn start(
    gateway: &Gateway,
    oauth: &OAuthClient,
    id: &str,
    callback: &str,
    return_to: &str,
) -> Result<String, AdminError> {
    let runtime = gateway
        .runtime(id)
        .ok_or_else(|| AdminError::NotFound(format!("No integration {id}")))?;
    // Registering a client and exchanging codes are external writes.
    if matches!(gateway.deps().side_effects, SideEffectMode::Record) {
        return Err(AdminError::Invalid(RECORDING.into()));
    }
    let row = runtime.config();
    let Transport::Http { url } = &row.transport else {
        return Err(AdminError::Invalid(
            "Only HTTP integrations use OAuth.".into(),
        ));
    };
    if row.auth != AuthKind::OAuth {
        return Err(AdminError::Invalid(
            "Set the integration's authentication to OAuth first.".into(),
        ));
    }
    let url = Url::parse(url).map_err(|e| AdminError::Invalid(format!("Invalid URL: {e}")))?;
    let challenge = probe_challenge(gateway, &url).await;
    let invalid = |e: OAuthError| AdminError::Invalid(e.to_string());
    let discovery = oauth
        .discover(&url, challenge.as_deref())
        .await
        .map_err(invalid)?;
    let registration = oauth
        .register(&discovery, callback, CLIENT_NAME)
        .await
        .map_err(invalid)?;
    let state = omni_core::ids::secure_id_b64url(32);
    let verifier = pkce::verifier();
    let authorize = oauth
        .authorize_url(
            &discovery,
            &registration,
            callback,
            &state,
            &pkce::challenge(&verifier),
        )
        .map_err(invalid)?;
    let now = gateway.clock().now_ms();
    let secrets = FlowSecrets {
        verifier,
        client_secret: registration.client_secret.clone(),
    };
    let sealed = gateway.secrets().seal(
        &flow_context(&state),
        &serde_json::to_string(&secrets).unwrap_or_default(),
    );
    let flow = UpstreamFlowRow {
        state,
        integration: id.to_owned(),
        origin: row.transport.credential_origin(),
        sealed,
        redirect_uri: callback.to_owned(),
        issuer: discovery.issuer.clone(),
        token_endpoint: discovery.token_endpoint.clone(),
        client_id: registration.client_id.clone(),
        token_auth: registration.token_auth.clone(),
        resource: discovery.resource.clone(),
        scope: (!discovery.scopes.is_empty()).then(|| discovery.scopes.join(" ")),
        iss_supported: discovery.iss_parameter_supported,
        return_to: return_to.to_owned(),
        created_at: now,
        expires_at: now + FLOW_TTL_MS,
        extra: Extra::default(),
    };
    gateway
        .store()
        .write(move |tx| {
            // At most a few flows are ever pending; drop the expired ones.
            for stale in tx.get_all::<UpstreamFlowRow>()? {
                if stale.expires_at <= now {
                    tx.delete::<UpstreamFlowRow>(&stale.state)?;
                }
            }
            tx.upsert(&flow, UpsertOpts::default())
        })
        .await?;
    Ok(authorize.to_string())
}

/// The outcome of a callback: where to send the browser.
pub struct Finished {
    pub return_to: Option<String>,
    pub integration: Option<String>,
    pub error: Option<String>,
}

/// Completes the flow named by the callback's `state`.
pub async fn finish(
    gateway: &Gateway,
    oauth: &OAuthClient,
    query: &HashMap<String, String>,
) -> Finished {
    if matches!(gateway.deps().side_effects, SideEffectMode::Record) {
        return Finished {
            return_to: None,
            integration: None,
            error: Some(RECORDING.into()),
        };
    }
    let Some(state) = query
        .get("state")
        .filter(|s| !s.is_empty() && s.len() <= 128)
    else {
        return Finished {
            return_to: None,
            integration: None,
            error: Some("The authorization response has no state.".into()),
        };
    };
    let key = state.clone();
    let flow = gateway
        .store()
        .write(move |tx| {
            let flow = tx.get::<UpstreamFlowRow>(&key)?;
            if flow.is_some() {
                tx.delete::<UpstreamFlowRow>(&key)?;
            }
            Ok::<_, StoreError>(flow)
        })
        .await;
    let flow = match flow {
        Ok(Some(flow)) => flow,
        Ok(None) => {
            return Finished {
                return_to: None,
                integration: None,
                error: Some("This connection attempt is unknown or was already used.".into()),
            };
        }
        Err(error) => {
            tracing::warn!(target: LOG, %error, "Gateway OAuth flow unreadable");
            return Finished {
                return_to: None,
                integration: None,
                error: Some("Omni could not read the connection attempt.".into()),
            };
        }
    };
    let fail = |message: String| Finished {
        return_to: Some(flow.return_to.clone()),
        integration: Some(flow.integration.clone()),
        error: Some(message),
    };
    let now = gateway.clock().now_ms();
    if flow.expires_at <= now {
        return fail("The connection attempt expired; start it again.".into());
    }
    let Some(runtime) = gateway.runtime(&flow.integration) else {
        return fail("The integration was removed.".into());
    };
    let row = runtime.config();
    if row.transport.credential_origin() != flow.origin || row.auth != AuthKind::OAuth {
        return fail("The integration changed while connecting; start again.".into());
    }
    let code = match OAuthClient::callback_code(query, state, &flow.issuer, flow.iss_supported) {
        Ok(code) => code,
        Err(error) => return fail(error.to_string()),
    };
    let Some(secrets) = gateway
        .secrets()
        .open(&flow_context(state), &flow.sealed)
        .and_then(|json| serde_json::from_str::<FlowSecrets>(&json).ok())
    else {
        return fail("The connection attempt could not be verified.".into());
    };
    let registration = Registration {
        client_id: flow.client_id.clone(),
        client_secret: secrets.client_secret.clone(),
        token_auth: flow.token_auth.clone(),
    };
    let tokens = match oauth
        .exchange(
            &flow.token_endpoint,
            &registration,
            &code,
            &secrets.verifier,
            &flow.redirect_uri,
            &flow.resource,
            now,
        )
        .await
    {
        Ok(tokens) => tokens,
        Err(error) => return fail(error.to_string()),
    };
    let meta = OAuthMeta {
        issuer: flow.issuer.clone(),
        token_endpoint: flow.token_endpoint.clone(),
        client_id: flow.client_id.clone(),
        resource: flow.resource.clone(),
        scope: tokens.scope.clone().or(flow.scope.clone()),
        access_expires_at: tokens.expires_at,
        refreshable: tokens.refresh_token.is_some(),
        connected_at: now,
        token_auth: flow.token_auth.clone(),
        extra: omni_store::cbor::Extra::default(),
    };
    if let Err(error) = gateway
        .credentials()
        .save_oauth(
            &row,
            meta,
            tokens.access_token,
            tokens.refresh_token,
            secrets.client_secret,
        )
        .await
    {
        tracing::warn!(target: LOG, %error, "Gateway OAuth tokens not stored");
        return fail("Omni could not store the tokens.".into());
    }
    runtime.reset(gateway.deps());
    gateway.request_refresh(&flow.integration);
    tracing::info!(target: LOG, integration = %flow.integration, "Gateway integration connected with OAuth");
    Finished {
        return_to: Some(flow.return_to.clone()),
        integration: Some(flow.integration),
        error: None,
    }
}
