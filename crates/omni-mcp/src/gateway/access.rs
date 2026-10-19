//! Downstream access to `/gateway/mcp`.
//!
//! Two kinds of bearer token, both separate from `OMNI_MCP_TOKEN` and from
//! every upstream credential:
//!
//! - client tokens (`omg_` + 256 random bits) for Claude Code and Codex,
//!   created in the UI and stored only as SHA-256;
//! - OAuth access tokens (`oat_<payload>.<signature>`) for ChatGPT, issued
//!   by [`super::oauth::server`] for one grant and valid for an hour. The
//!   payload is `{"g": grantId, "e": expiresAtMs, "n": nonce}`, signed with a
//!   key derived from the gateway key; a revoked grant fails at once.
//!
//! Event subscriptions made through the gateway are owned by
//! [`Principal::owner`], and delivery checks [`DownstreamAccess::is_active`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use omni_core::clock::SharedClock;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use tokio_util::task::TaskTracker;

use super::model::{ClientRow, GrantRow};
use super::secrets::GatewaySecrets;

const LOG: &str = "MCP:Gateway";
pub const CLIENT_TOKEN_PREFIX: &str = "omg_";
pub const ACCESS_TOKEN_PREFIX: &str = "oat_";
pub const ACCESS_TOKEN_TTL_MS: i64 = 60 * 60 * 1000;
const ACCESS_PURPOSE: &str = "omni-gateway-access-v1";
/// `lastUsedAt` is written at most this often per principal.
const TOUCH_INTERVAL_MS: i64 = 5 * 60 * 1000;
pub const OWNER_PREFIX: &str = "gateway:";

/// An authenticated downstream caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    Client { id: String, label: String },
    Grant { id: String, client_name: String },
}

impl Principal {
    /// The event-subscription owner.
    pub fn owner(&self) -> String {
        match self {
            Principal::Client { id, .. } => format!("{OWNER_PREFIX}client:{id}"),
            Principal::Grant { id, .. } => format!("{OWNER_PREFIX}grant:{id}"),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Principal::Client { label, .. } => label,
            Principal::Grant { client_name, .. } => client_name,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AccessPayload {
    g: String,
    e: i64,
    n: String,
}

/// A new access token for `grant_id` and its expiry.
pub fn issue_access_token(secrets: &GatewaySecrets, grant_id: &str, now: i64) -> (String, i64) {
    let expires_at = now + ACCESS_TOKEN_TTL_MS;
    let payload = AccessPayload {
        g: grant_id.to_owned(),
        e: expires_at,
        n: omni_core::ids::secure_id_b64url(9),
    };
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap_or_default());
    let signature = URL_SAFE_NO_PAD.encode(secrets.sign(ACCESS_PURPOSE, encoded.as_bytes()));
    (
        format!("{ACCESS_TOKEN_PREFIX}{encoded}.{signature}"),
        expires_at,
    )
}

/// The grant an unexpired, correctly signed access token names.
pub fn verify_access_token(secrets: &GatewaySecrets, token: &str, now: i64) -> Option<String> {
    let rest = token.strip_prefix(ACCESS_TOKEN_PREFIX)?;
    let (encoded, signature) = rest.split_once('.')?;
    let expected = secrets.sign(ACCESS_PURPOSE, encoded.as_bytes());
    let supplied = URL_SAFE_NO_PAD.decode(signature).ok()?;
    if supplied.len() != expected.len() || !bool::from(supplied.ct_eq(&expected)) {
        return None;
    }
    let payload: AccessPayload =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
    (payload.e > now).then_some(payload.g)
}

/// The bearer token in an `Authorization` header value.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let value = header?;
    let (scheme, token) = value.split_at_checked(7)?;
    (scheme.eq_ignore_ascii_case("bearer ")
        && !token.is_empty()
        && !token.chars().any(char::is_whitespace))
    .then_some(token)
}

struct Inner {
    store: Store,
    clock: SharedClock,
    secrets: GatewaySecrets,
    tracker: TaskTracker,
    /// Client rows by token hash; `None` until first loaded.
    clients: RwLock<Option<HashMap<String, ClientRow>>>,
    touched: Mutex<HashMap<String, i64>>,
    /// Bumped by every client change, so a load that raced one is not cached.
    generation: std::sync::atomic::AtomicU64,
}

/// Downstream authentication; cheap to clone.
#[derive(Clone)]
pub struct DownstreamAccess {
    inner: Arc<Inner>,
}

impl DownstreamAccess {
    pub fn new(
        store: Store,
        clock: SharedClock,
        secrets: GatewaySecrets,
        tracker: TaskTracker,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                clock,
                secrets,
                tracker,
                clients: RwLock::new(None),
                touched: Mutex::new(HashMap::new()),
                generation: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn secrets(&self) -> &GatewaySecrets {
        &self.inner.secrets
    }

    async fn clients_by_hash(&self) -> Result<HashMap<String, ClientRow>, StoreError> {
        if let Some(cached) = self
            .inner
            .clients
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Ok(cached.clone());
        }
        let generation = self
            .inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst);
        let rows = self
            .inner
            .store
            .read(|docs| docs.get_all::<ClientRow>())
            .await?;
        let map: HashMap<String, ClientRow> = rows
            .into_iter()
            .map(|row| (row.token_hash.clone(), row))
            .collect();
        let mut cache = self
            .inner
            .clients
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if self
            .inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
            == generation
        {
            *cache = Some(map.clone());
        }
        Ok(map)
    }

    fn invalidate(&self) {
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self
            .inner
            .clients
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Resolves an `Authorization` header; `None` for anything invalid.
    pub async fn authenticate(&self, header: Option<&str>) -> Option<Principal> {
        let token = bearer_token(header)?;
        let now = self.inner.clock.now_ms();
        if token.starts_with(CLIENT_TOKEN_PREFIX) {
            let hash = omni_core::digest::sha256_hex(token);
            let clients = self.clients_by_hash().await.ok()?;
            let row = clients.get(&hash)?;
            self.touch(&row.id, now, false);
            return Some(Principal::Client {
                id: row.id.clone(),
                label: row.label.clone(),
            });
        }
        let grant_id = verify_access_token(&self.inner.secrets, token, now)?;
        let grant = self.grant(&grant_id).await.ok()??;
        self.touch(&grant.id, now, true);
        Some(Principal::Grant {
            id: grant.id,
            client_name: grant.client_name,
        })
    }

    async fn grant(&self, id: &str) -> Result<Option<GrantRow>, StoreError> {
        let id = id.to_owned();
        self.inner
            .store
            .read(move |docs| docs.get::<GrantRow>(&id))
            .await
    }

    /// Records `lastUsedAt` in the background, at most every five minutes.
    fn touch(&self, id: &str, now: i64, grant: bool) {
        {
            let mut touched = self
                .inner
                .touched
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if touched
                .get(id)
                .is_some_and(|at| now - at < TOUCH_INTERVAL_MS)
            {
                return;
            }
            touched.insert(id.to_owned(), now);
        }
        let store = self.inner.store.clone();
        let id = id.to_owned();
        let access = self.clone();
        omni_core::spawn::spawn_tracked(&self.inner.tracker, "mcp-gateway-touch", async move {
            let written = store
                .write(move |tx| {
                    if grant {
                        if let Some(mut row) = tx.get::<GrantRow>(&id)? {
                            row.last_used_at = Some(now);
                            tx.upsert(&row, UpsertOpts::default())?;
                        }
                    } else if let Some(mut row) = tx.get::<ClientRow>(&id)? {
                        row.last_used_at = Some(now);
                        tx.upsert(&row, UpsertOpts::default())?;
                    }
                    Ok::<_, StoreError>(())
                })
                .await;
            match written {
                Ok(()) if !grant => access.invalidate(),
                Ok(()) => {}
                Err(error) => {
                    tracing::debug!(target: LOG, %error, "Gateway lastUsedAt not recorded");
                }
            }
        });
    }

    /// Whether a subscription owner still has access (client token or grant exists).
    pub async fn is_active(&self, owner: &str) -> bool {
        let Some(rest) = owner.strip_prefix(OWNER_PREFIX) else {
            return false;
        };
        let result = match rest.split_once(':') {
            Some(("client", id)) => {
                let id = id.to_owned();
                self.inner
                    .store
                    .read(move |docs| docs.get::<ClientRow>(&id))
                    .await
                    .map(|row| row.is_some())
            }
            Some(("grant", id)) => self.grant(id).await.map(|row| row.is_some()),
            _ => Ok(false),
        };
        result.unwrap_or(false)
    }

    pub async fn clients(&self) -> Result<Vec<ClientRow>, StoreError> {
        let mut rows = self
            .inner
            .store
            .read(|docs| docs.get_all::<ClientRow>())
            .await?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        Ok(rows)
    }

    pub async fn grants(&self) -> Result<Vec<GrantRow>, StoreError> {
        let mut rows = self
            .inner
            .store
            .read(|docs| docs.get_all::<GrantRow>())
            .await?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        Ok(rows)
    }

    /// Creates a client token; the token is returned once and never stored.
    pub async fn create_client(&self, label: &str) -> Result<(ClientRow, String), StoreError> {
        let token = format!(
            "{CLIENT_TOKEN_PREFIX}{}",
            omni_core::ids::secure_id_b64url(32)
        );
        let row = ClientRow {
            id: omni_core::ids::uuid_v4(),
            label: label.to_owned(),
            token_hash: omni_core::digest::sha256_hex(&token),
            prefix: token.chars().take(CLIENT_TOKEN_PREFIX.len() + 6).collect(),
            created_at: self.inner.clock.now_ms(),
            last_used_at: None,
            extra: Extra::default(),
        };
        let stored = row.clone();
        self.inner
            .store
            .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
            .await?;
        self.invalidate();
        Ok((row, token))
    }

    /// Deletes a client token; whether it existed.
    pub async fn revoke_client(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        let removed = self
            .inner
            .store
            .write(move |tx| tx.delete::<ClientRow>(&id))
            .await?;
        self.invalidate();
        Ok(removed)
    }

    /// Deletes a grant, which invalidates its access and refresh tokens.
    pub async fn revoke_grant(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.inner
            .store
            .write(move |tx| tx.delete::<GrantRow>(&id))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_tokens_verify_until_expiry_and_only_with_the_key() {
        let secrets = GatewaySecrets::from_key([1; 32]);
        let (token, expires_at) = issue_access_token(&secrets, "grant-1", 1_000);
        assert_eq!(expires_at, 1_000 + ACCESS_TOKEN_TTL_MS);
        assert_eq!(
            verify_access_token(&secrets, &token, 2_000).as_deref(),
            Some("grant-1")
        );
        assert_eq!(verify_access_token(&secrets, &token, expires_at), None);
        assert_eq!(
            verify_access_token(&GatewaySecrets::from_key([2; 32]), &token, 2_000),
            None
        );
        let (payload, _) = token.split_once('.').unwrap();
        assert_eq!(
            verify_access_token(&secrets, &format!("{payload}.AAAA"), 2_000),
            None
        );
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(Some("Bearer a b")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(None), None);
    }
}
