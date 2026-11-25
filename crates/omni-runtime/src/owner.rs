//! Trusted browsers ("Trusted browsers" in `docs/gateway.md`).
//!
//! The UI and most of `/api` need no login: being on the home network is
//! enough to look around and run everyday actions. Changes that mint
//! internet-valid credentials or reach every integration (the gateway's
//! management API, data deletion, settings) also need a trusted browser: one
//! holding the `omni_owner` cookie whose SHA-256 is an `owner-trusted-browser`
//! row. Request headers are never evidence, because a LAN client chooses them.
//!
//! A browser becomes trusted only through the owner, two ways:
//!
//! - **Push pairing.** The browser asks (`POST /api/owner/pairings`) and shows
//!   a code; the owner's phone gets a push linking to `/trust/<secret>`, where
//!   approving means typing that code. The secret proves the owner's channel,
//!   the code proves which browser, so a request someone else started cannot
//!   be approved by tapping. Five wrong codes deny it.
//! - **One-time link.** `omni owner-link` inside the container prints
//!   `/trust/<secret>`; the browser that opens it and confirms is trusted.
//!
//! The pairing secret and the cookie are random 256-bit values stored only
//! as SHA-256 and registered with the log scrubber.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
    USER_AGENT,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use omni_alerts::{Pushover, PushoverChannel, PushoverMessage};
use omni_api::owner::{
    OwnerStatus, PairingClaim, PairingPhase, PairingStarted, PairingStatus, TrustedBrowserView,
    UNTRUSTED_BROWSER, UntrustedBody,
};
use omni_core::clock::SharedClock;
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, ModifyOpts, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use tokio_util::task::TaskTracker;

const LOG: &str = "Owner";
pub const COOKIE_NAME: &str = "omni_owner";
const TOKEN_PREFIX: &str = "otb_";
const PAIRING_TTL_MS: i64 = 10 * 60 * 1000;
const LINK_TTL_MS: i64 = 15 * 60 * 1000;
/// Anyone on the LAN can ask, so pushes go out at most this often.
const PUSH_INTERVAL_MS: i64 = 60 * 1000;
const MAX_PENDING: usize = 5;
const MAX_CODE_ATTEMPTS: u32 = 5;
const CODE_LEN: usize = 8;
/// Browsers cap cookie lifetimes at 400 days.
const COOKIE_MAX_AGE_S: i64 = 400 * 24 * 60 * 60;
/// `lastSeenAt` is written at most this often per browser.
const TOUCH_INTERVAL_MS: i64 = 60 * 60 * 1000;
/// A trusted browser unused this long is forgotten (the row expires), which
/// bounds how long a copied cookie stays useful if its browser is gone.
const IDLE_EXPIRY_MS: i64 = 90 * 24 * 60 * 60 * 1000;
const LABEL_LIMIT: usize = 80;
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// `owner-trusted-browser`: a browser trusted for admin changes, by the
/// SHA-256 of its cookie.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedBrowserRow {
    pub id: String,
    pub label: String,
    pub token_hash: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for TrustedBrowserRow {
    const NAME: &'static str = "owner-trusted-browser";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingKind {
    /// The asking browser is approved from the owner's phone.
    #[serde(rename = "push")]
    Push,
    /// The browser that opens the link is trusted.
    #[serde(rename = "link")]
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingState {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "denied")]
    Denied,
    /// A cookie was issued; the pairing cannot be used again.
    #[serde(rename = "used")]
    Used,
}

/// `owner-pairing`: one request to trust a browser, expiring with the row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingRow {
    pub id: String,
    pub kind: PairingKind,
    pub label: String,
    /// The code the asking browser shows (push pairings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub secret_hash: String,
    /// The asking browser's claim secret (push pairings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_hash: Option<String>,
    pub state: PairingState,
    #[serde(default)]
    pub attempts: u32,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for PairingRow {
    const NAME: &'static str = "owner-pairing";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The entities this module persists.
pub fn entities() -> Vec<omni_store::entity::EntityDescriptor> {
    vec![
        omni_store::entity::EntityDescriptor::of::<TrustedBrowserRow>(),
        omni_store::entity::EntityDescriptor::of::<PairingRow>(),
    ]
}

#[derive(Debug, thiserror::Error)]
pub enum OwnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("A request to trust a browser was sent less than a minute ago; try again shortly.")]
    TooSoon,
}

/// The owner's answer on the approval page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve { code: Option<String> },
    Deny,
}

/// What the approval page shows after an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decided {
    /// A push pairing was approved; the asking browser picks up its cookie.
    Approved {
        label: String,
    },
    /// A link pairing: this browser gets the cookie.
    TrustedHere {
        token: String,
        label: String,
    },
    WrongCode {
        remaining: u32,
    },
    Denied,
    /// Expired, already answered, or unknown.
    Gone,
}

/// The cookie a claim picked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claimed {
    Pending,
    Trusted { token: String },
    Denied,
    Gone,
}

#[derive(Clone)]
pub struct OwnerTrust {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    clock: SharedClock,
    pushover: Option<Pushover>,
    tracker: TaskTracker,
    home_url: String,
    last_push: AtomicI64,
}

fn browser_opts(now: i64) -> UpsertOpts {
    UpsertOpts {
        expires_at: Some(now + IDLE_EXPIRY_MS),
        ttl_ms: None,
    }
}

fn hash(secret: &str) -> String {
    omni_core::digest::sha256_hex(secret)
}

fn same_hash(stored: &str, secret: &str) -> bool {
    omni_core::digest::ct_eq_sha256(stored.as_bytes(), hash(secret).as_bytes())
}

/// The code as typed: case, spaces and dashes do not matter.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `ABCD-EFGH`.
fn display_code(code: &str) -> String {
    let (head, tail) = code.split_at(code.len() / 2);
    format!("{head}-{tail}")
}

/// "Safari on iPhone" from a `User-Agent`; a description, never evidence.
pub fn browser_label(user_agent: &str) -> String {
    let ua = user_agent;
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "A browser"
    };
    let os = if ua.contains("iPhone") {
        Some("iPhone")
    } else if ua.contains("iPad") {
        Some("iPad")
    } else if ua.contains("Android") {
        Some("Android")
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        Some("macOS")
    } else if ua.contains("Windows") {
        Some("Windows")
    } else if ua.contains("Linux") {
        Some("Linux")
    } else {
        None
    };
    match os {
        Some(os) => format!("{browser} on {os}"),
        None => browser.to_owned(),
    }
}

fn clean_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(LABEL_LIMIT)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "A browser".to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// The `omni_owner` cookie's value, if present.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.trim().to_owned())
        .filter(|v| v.starts_with(TOKEN_PREFIX))
}

/// `Set-Cookie` for a new token; `Secure` when the page came over HTTPS
/// (the proxy sets `X-Forwarded-Proto`).
fn set_cookie(headers: &HeaderMap, token: &str) -> Option<HeaderValue> {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_S}; HttpOnly; SameSite=Strict{secure}"
    ))
    .ok()
}

impl OwnerTrust {
    /// `pushover` is `None` for the CLI, which only creates links.
    pub fn new(
        store: Store,
        clock: SharedClock,
        pushover: Option<Pushover>,
        tracker: TaskTracker,
        home_url: &str,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                clock,
                pushover,
                tracker,
                home_url: home_url.trim_end_matches('/').to_owned(),
                last_push: AtomicI64::new(i64::MIN / 2),
            }),
        }
    }

    fn approval_url(&self, secret: &str) -> String {
        format!("{}/trust/{secret}", self.inner.home_url)
    }

    /// The trusted browser this request's cookie names.
    pub async fn browser(&self, headers: &HeaderMap) -> Option<TrustedBrowserRow> {
        let token = cookie_token(headers)?;
        let found = match self
            .inner
            .store
            .read(|tx| tx.get_all::<TrustedBrowserRow>())
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .find(|row| same_hash(&row.token_hash, &token)),
            Err(error) => {
                tracing::warn!(target: LOG, %error, "Could not read trusted browsers");
                None
            }
        }?;
        let now = self.inner.clock.now_ms();
        if found
            .last_seen_at
            .is_none_or(|seen| now - seen >= TOUCH_INTERVAL_MS)
        {
            let id = found.id.clone();
            let touched = self
                .inner
                .store
                .write(move |tx| {
                    tx.update::<TrustedBrowserRow>(
                        &id,
                        |mut row| {
                            row.last_seen_at = Some(now);
                            row
                        },
                        ModifyOpts {
                            expires_at: Some(Some(now + IDLE_EXPIRY_MS)),
                            ttl_ms: None,
                        },
                    )
                })
                .await;
            if let Err(error) = touched {
                tracing::warn!(target: LOG, %error, "Could not record a trusted browser's visit");
            }
        }
        Some(found)
    }

    pub async fn is_trusted(&self, headers: &HeaderMap) -> bool {
        self.browser(headers).await.is_some()
    }

    /// Trusts a new browser and returns its cookie value.
    async fn trust(&self, label: String) -> Result<(TrustedBrowserRow, String), StoreError> {
        let token = format!("{TOKEN_PREFIX}{}", omni_core::ids::secure_id_b64url(32));
        omni_core::secrets::register_runtime(&token);
        let row = TrustedBrowserRow {
            id: omni_core::ids::uuid_v4(),
            label,
            token_hash: hash(&token),
            created_at: self.inner.clock.now_ms(),
            last_seen_at: None,
            extra: Extra::default(),
        };
        let stored = row.clone();
        self.inner
            .store
            .write(move |tx| tx.upsert(&stored, browser_opts(stored.created_at)))
            .await?;
        tracing::info!(target: LOG, browser = %row.label, "Trusted a browser for admin changes");
        Ok((row, token))
    }

    /// Trusts a browser directly (tests and fixtures).
    pub async fn trust_for_tests(&self, label: &str) -> Result<String, StoreError> {
        self.trust(clean_label(label)).await.map(|(_, token)| token)
    }

    pub async fn status(&self, headers: &HeaderMap) -> Result<OwnerStatus, StoreError> {
        let Some(current) = self.browser(headers).await else {
            return Ok(OwnerStatus {
                trusted: false,
                browsers: Vec::new(),
            });
        };
        let mut rows = self
            .inner
            .store
            .read(|tx| tx.get_all::<TrustedBrowserRow>())
            .await?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        Ok(OwnerStatus {
            trusted: true,
            browsers: rows
                .into_iter()
                .map(|row| TrustedBrowserView {
                    current: row.id == current.id,
                    id: row.id,
                    label: row.label,
                    created_at: row.created_at,
                    last_seen_at: row.last_seen_at,
                })
                .collect(),
        })
    }

    /// Removes a trusted browser; whether it existed.
    pub async fn revoke(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.inner
            .store
            .write(move |tx| tx.delete::<TrustedBrowserRow>(&id))
            .await
    }

    /// A push pairing for the browser that sent `user_agent`.
    pub async fn start_pairing(&self, user_agent: &str) -> Result<PairingStarted, OwnerError> {
        let now = self.inner.clock.now_ms();
        let previous = self.inner.last_push.load(Ordering::Relaxed);
        if now - previous < PUSH_INTERVAL_MS {
            return Err(OwnerError::TooSoon);
        }
        if self
            .inner
            .last_push
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Err(OwnerError::TooSoon);
        }
        let label = clean_label(&browser_label(user_agent));
        let code = omni_core::ids::readable_code(CODE_LEN);
        let secret = omni_core::ids::secure_id_b64url(32);
        let claim = omni_core::ids::secure_id_b64url(32);
        omni_core::secrets::register_runtime(&secret);
        omni_core::secrets::register_runtime(&claim);
        let row = PairingRow {
            id: omni_core::ids::uuid_v4(),
            kind: PairingKind::Push,
            label: label.clone(),
            code: Some(code.clone()),
            secret_hash: hash(&secret),
            claim_hash: Some(hash(&claim)),
            state: PairingState::Pending,
            attempts: 0,
            created_at: now,
            expires_at: now + PAIRING_TTL_MS,
            extra: Extra::default(),
        };
        let stored = row.clone();
        // The newest request wins: a LAN client filling the queue can delay
        // the owner by a minute, never lock them out.
        let written = self
            .inner
            .store
            .write(move |tx| {
                let mut waiting: Vec<PairingRow> = tx
                    .get_all::<PairingRow>()?
                    .into_iter()
                    .filter(|row| {
                        row.kind == PairingKind::Push && row.state == PairingState::Pending
                    })
                    .collect();
                waiting.sort_by_key(|row| row.created_at);
                let excess = (waiting.len() + 1).saturating_sub(MAX_PENDING);
                for old in waiting.iter().take(excess) {
                    tx.delete::<PairingRow>(&old.id)?;
                }
                tx.upsert(
                    &stored,
                    UpsertOpts {
                        expires_at: Some(stored.expires_at),
                        ttl_ms: None,
                    },
                )
            })
            .await;
        if let Err(error) = written {
            self.inner.last_push.store(previous, Ordering::Relaxed);
            return Err(error.into());
        }
        self.push(&label, &secret);
        tracing::info!(target: LOG, browser = %label, "A browser asked to be trusted for admin changes");
        Ok(PairingStarted {
            id: row.id,
            code: display_code(&code),
            claim,
            expires_at: row.expires_at,
        })
    }

    /// The code is not in the push: the owner must read it off the asking
    /// browser, so approving needs both the phone and that screen.
    fn push(&self, label: &str, secret: &str) {
        let Some(pushover) = self.inner.pushover.clone() else {
            return;
        };
        let message = PushoverMessage {
            title: Some("Trust a browser for Omni?".to_owned()),
            message: format!(
                "{label} asks to manage Omni (gateway tokens, integrations, settings). If it is yours, open this and type the code that browser shows. Otherwise ignore it; it expires in ten minutes."
            ),
            url: Some(self.approval_url(secret)),
            url_title: Some("Review the request".to_owned()),
            ..PushoverMessage::default()
        };
        omni_core::spawn::spawn_tracked(&self.inner.tracker, "owner-pairing-push", async move {
            if let Err(error) = pushover.send(PushoverChannel::General, message).await {
                tracing::warn!(target: LOG, %error, "Trusted-browser approval push failed");
            }
        });
    }

    /// A one-time link that trusts the browser opening it (`omni owner-link`).
    pub async fn create_link(&self, label: &str) -> Result<String, StoreError> {
        let now = self.inner.clock.now_ms();
        let secret = omni_core::ids::secure_id_b64url(32);
        let row = PairingRow {
            id: omni_core::ids::uuid_v4(),
            kind: PairingKind::Link,
            label: clean_label(label),
            code: None,
            secret_hash: hash(&secret),
            claim_hash: None,
            state: PairingState::Pending,
            attempts: 0,
            created_at: now,
            expires_at: now + LINK_TTL_MS,
            extra: Extra::default(),
        };
        self.inner
            .store
            .write(move |tx| {
                tx.upsert(
                    &row,
                    UpsertOpts {
                        expires_at: Some(row.expires_at),
                        ttl_ms: None,
                    },
                )
            })
            .await?;
        Ok(self.approval_url(&secret))
    }

    async fn pairing_by_secret(&self, secret: &str) -> Result<Option<PairingRow>, StoreError> {
        let secret = secret.to_owned();
        Ok(self
            .inner
            .store
            .read(|tx| tx.get_all::<PairingRow>())
            .await?
            .into_iter()
            .find(|row| same_hash(&row.secret_hash, &secret)))
    }

    /// The pending pairing an approval link names.
    pub async fn pending(&self, secret: &str) -> Result<Option<PairingRow>, StoreError> {
        Ok(self
            .pairing_by_secret(secret)
            .await?
            .filter(|row| row.state == PairingState::Pending))
    }

    /// The owner's answer on the approval page, sent by `user_agent`. The
    /// code check, the attempt count and the new state are one transaction,
    /// so concurrent answers cannot exceed the attempt limit or disagree.
    pub async fn decide(
        &self,
        secret: &str,
        decision: Decision,
        user_agent: &str,
    ) -> Result<Decided, StoreError> {
        let Some(row) = self.pending(secret).await? else {
            return Ok(Decided::Gone);
        };
        let id = row.id.clone();
        let link_label = clean_label(&format!("{} (owner-link)", browser_label(user_agent)));
        let link_token = format!("{TOKEN_PREFIX}{}", omni_core::ids::secure_id_b64url(32));
        let browser = TrustedBrowserRow {
            id: omni_core::ids::uuid_v4(),
            label: link_label,
            token_hash: hash(&link_token),
            created_at: self.inner.clock.now_ms(),
            last_seen_at: None,
            extra: Extra::default(),
        };
        let outcome = self
            .inner
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<PairingRow>(&id)? else {
                    return Ok::<_, StoreError>(Decided::Gone);
                };
                if row.state != PairingState::Pending {
                    return Ok(Decided::Gone);
                }
                let outcome = match (row.kind, decision) {
                    (_, Decision::Deny) => {
                        row.state = PairingState::Denied;
                        Decided::Denied
                    }
                    (PairingKind::Link, Decision::Approve { .. }) => {
                        row.state = PairingState::Used;
                        tx.upsert(&browser, browser_opts(browser.created_at))?;
                        Decided::TrustedHere {
                            token: String::new(),
                            label: browser.label.clone(),
                        }
                    }
                    (PairingKind::Push, Decision::Approve { code }) => {
                        let expected = row.code.as_deref().map(normalize_code).unwrap_or_default();
                        let typed = code.as_deref().map(normalize_code).unwrap_or_default();
                        if !expected.is_empty()
                            && omni_core::digest::ct_eq_sha256(
                                expected.as_bytes(),
                                typed.as_bytes(),
                            )
                        {
                            row.state = PairingState::Approved;
                            Decided::Approved {
                                label: row.label.clone(),
                            }
                        } else {
                            row.attempts += 1;
                            if row.attempts >= MAX_CODE_ATTEMPTS {
                                row.state = PairingState::Denied;
                                Decided::Denied
                            } else {
                                Decided::WrongCode {
                                    remaining: MAX_CODE_ATTEMPTS - row.attempts,
                                }
                            }
                        }
                    }
                };
                tx.upsert(
                    &row,
                    UpsertOpts {
                        expires_at: Some(row.expires_at),
                        ttl_ms: None,
                    },
                )?;
                Ok(outcome)
            })
            .await?;
        match &outcome {
            Decided::Approved { label } => {
                tracing::info!(target: LOG, browser = %label, "A trusted-browser request was approved");
            }
            Decided::Denied => {
                tracing::info!(target: LOG, browser = %row.label, "A trusted-browser request was denied");
            }
            Decided::TrustedHere { label, .. } => {
                omni_core::secrets::register_runtime(&link_token);
                tracing::info!(target: LOG, browser = %label, "Trusted a browser for admin changes");
                return Ok(Decided::TrustedHere {
                    token: link_token,
                    label: label.clone(),
                });
            }
            Decided::WrongCode { .. } | Decided::Gone => {}
        }
        Ok(outcome)
    }

    /// The asking browser collects its cookie once the owner approved.
    pub async fn claim(&self, id: &str, claim: &str) -> Result<Claimed, StoreError> {
        let key = id.to_owned();
        let Some(row) = self
            .inner
            .store
            .read(move |tx| tx.get::<PairingRow>(&key))
            .await?
        else {
            return Ok(Claimed::Gone);
        };
        let owned = row
            .claim_hash
            .as_deref()
            .is_some_and(|stored| same_hash(stored, claim));
        if !owned || row.kind != PairingKind::Push {
            return Ok(Claimed::Gone);
        }
        match row.state {
            PairingState::Pending => Ok(Claimed::Pending),
            PairingState::Denied => Ok(Claimed::Denied),
            PairingState::Used => Ok(Claimed::Gone),
            PairingState::Approved => {
                let token = format!("{TOKEN_PREFIX}{}", omni_core::ids::secure_id_b64url(32));
                let now = self.inner.clock.now_ms();
                let browser = TrustedBrowserRow {
                    id: omni_core::ids::uuid_v4(),
                    label: row.label.clone(),
                    token_hash: hash(&token),
                    created_at: now,
                    last_seen_at: None,
                    extra: Extra::default(),
                };
                let key = id.to_owned();
                let taken = self
                    .inner
                    .store
                    .write(move |tx| {
                        let Some(mut row) = tx.get::<PairingRow>(&key)? else {
                            return Ok::<_, StoreError>(false);
                        };
                        if row.state != PairingState::Approved {
                            return Ok(false);
                        }
                        row.state = PairingState::Used;
                        tx.upsert(
                            &row,
                            UpsertOpts {
                                expires_at: Some(row.expires_at),
                                ttl_ms: None,
                            },
                        )?;
                        tx.upsert(&browser, browser_opts(now))?;
                        Ok(true)
                    })
                    .await?;
                if !taken {
                    return Ok(Claimed::Gone);
                }
                omni_core::secrets::register_runtime(&token);
                tracing::info!(target: LOG, browser = %row.label, "Trusted a browser for admin changes");
                Ok(Claimed::Trusted { token })
            }
        }
    }
}

/// The 403 an untrusted browser gets from an admin change.
pub fn untrusted_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(UntrustedBody {
            error: "Only a trusted browser can make this change. Use “Trust this browser” on the MCP gateway page, then approve it from your phone.".to_owned(),
            code: UNTRUSTED_BROWSER.to_owned(),
        }),
    )
        .into_response()
}

/// Middleware: changes (anything but GET and HEAD) need a trusted browser.
pub async fn require_trusted(
    State(owner): State<OwnerTrust>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let reading = matches!(*request.method(), Method::GET | Method::HEAD);
    if reading || owner.is_trusted(request.headers()).await {
        next.run(request).await
    } else {
        untrusted_response()
    }
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    omni_server_kit::ApiError::new(status, message).into_response()
}

/// `/api/owner*` and the `/trust/{secret}` approval page.
pub fn router(owner: OwnerTrust) -> Router {
    let guarded = Router::new()
        .route("/api/owner/browsers/{id}", delete(revoke_browser))
        .route_layer(axum::middleware::from_fn_with_state(
            owner.clone(),
            require_trusted,
        ))
        .with_state(owner.clone());
    Router::new()
        .route("/api/owner", get(status))
        .route("/api/owner/pairings", post(start_pairing))
        .route("/api/owner/pairings/{id}/claim", post(claim))
        .route("/trust/{secret}", get(approval_page).post(approval_answer))
        .with_state(owner)
        .merge(guarded)
}

async fn status(State(owner): State<OwnerTrust>, headers: HeaderMap) -> Response {
    match owner.status(&headers).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

async fn revoke_browser(State(owner): State<OwnerTrust>, Path(id): Path<String>) -> Response {
    match owner.revoke(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "No such trusted browser"),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

async fn start_pairing(State(owner): State<OwnerTrust>, headers: HeaderMap) -> Response {
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match owner.start_pairing(user_agent).await {
        Ok(started) => (StatusCode::CREATED, Json(started)).into_response(),
        Err(error @ OwnerError::TooSoon) => {
            api_error(StatusCode::TOO_MANY_REQUESTS, error.to_string())
        }
        Err(OwnerError::Store(error)) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

async fn claim(
    State(owner): State<OwnerTrust>,
    Path(id): Path<String>,
    headers: HeaderMap,
    omni_server_kit::JsonBody(body): omni_server_kit::JsonBody<PairingClaim>,
) -> Response {
    let phase = match owner.claim(&id, &body.claim).await {
        Ok(Claimed::Trusted { token }) => {
            let mut response = Json(PairingStatus {
                state: PairingPhase::Trusted,
            })
            .into_response();
            if let Some(cookie) = set_cookie(&headers, &token) {
                response.headers_mut().insert(SET_COOKIE, cookie);
            }
            response
                .headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return response;
        }
        Ok(Claimed::Pending) => PairingPhase::Pending,
        Ok(Claimed::Denied) => PairingPhase::Denied,
        Ok(Claimed::Gone) => PairingPhase::Expired,
        Err(error) => return omni_server_kit::ApiError::internal(error).into_response(),
    };
    Json(PairingStatus { state: phase }).into_response()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"referrer\" content=\"no-referrer\"><title>{title}</title><style>body{{font:16px/1.5 -apple-system,system-ui,sans-serif;background:#0e1013;color:#e6e8eb;margin:0;padding:24px}}main{{max-width:420px;margin:8vh auto}}h1{{font-size:22px;margin:0 0 12px}}p{{color:#aab1ba}}input{{font:24px ui-monospace,monospace;letter-spacing:.12em;width:100%;box-sizing:border-box;padding:10px;margin:8px 0 16px;background:#16191d;color:#e6e8eb;border:1px solid #2c3138;border-radius:8px;text-transform:uppercase}}button{{font:inherit;padding:10px 16px;border-radius:8px;border:1px solid #2c3138;background:#16191d;color:#e6e8eb;margin-right:8px}}button.primary{{background:#3d7bfd;border-color:#3d7bfd;color:#fff}}</style></head><body><main><h1>{title}</h1>{body}</main></body></html>",
        title = escape(title),
    );
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_CSP));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

fn gone_page() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "This request is no longer open",
        "<p>It expired, was already answered, or the link is wrong. Ask again from the browser you want to trust.</p>",
    )
}

async fn approval_page(State(owner): State<OwnerTrust>, Path(secret): Path<String>) -> Response {
    match owner.pending(&secret).await {
        Ok(Some(row)) => match row.kind {
            PairingKind::Push => page(
                StatusCode::OK,
                "Trust a browser?",
                &format!(
                    "<p><strong>{label}</strong> asks to manage Omni: gateway tokens, integrations and settings.</p><p>Type the code that browser shows. If you did not just ask from a browser of yours, deny it.</p><form method=\"post\"><input name=\"code\" autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\" placeholder=\"ABCD-EFGH\" aria-label=\"Code\" required><button class=\"primary\" name=\"action\" value=\"approve\">Trust it</button><button name=\"action\" value=\"deny\" formnovalidate>Deny</button></form>",
                    label = escape(&row.label)
                ),
            ),
            PairingKind::Link => page(
                StatusCode::OK,
                "Trust this browser?",
                "<p>This browser will be able to manage Omni: gateway tokens, integrations and settings. The link works once.</p><form method=\"post\"><button class=\"primary\" name=\"action\" value=\"approve\">Trust this browser</button><button name=\"action\" value=\"deny\">Cancel</button></form>",
            ),
        },
        Ok(None) => gone_page(),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

/// The approval form's fields (`action`, `code`).
fn form_answer(body: &[u8]) -> (bool, Option<String>) {
    let mut approve = false;
    let mut code = None;
    for (name, value) in url::form_urlencoded::parse(body) {
        match name.as_ref() {
            "action" => approve = value == "approve",
            "code" => code = Some(value.into_owned()),
            _ => {}
        }
    }
    (approve, code)
}

/// A form post from this page: the `Origin` must be this host.
fn same_origin_form(headers: &HeaderMap) -> bool {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok());
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|o| url::Url::parse(o).ok())
        .and_then(|url| {
            let host = url.host_str()?.to_owned();
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        });
    matches!((origin, host), (Some(origin), Some(host)) if origin.eq_ignore_ascii_case(host))
}

async fn approval_answer(
    State(owner): State<OwnerTrust>,
    Path(secret): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !same_origin_form(&headers) {
        return api_error(
            StatusCode::FORBIDDEN,
            "Cross-origin requests are not allowed",
        );
    }
    let (approve, code) = form_answer(&body);
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let decision = if approve {
        Decision::Approve { code }
    } else {
        Decision::Deny
    };
    match owner.decide(&secret, decision, user_agent).await {
        Ok(Decided::Approved { label }) => page(
            StatusCode::OK,
            "Trusted",
            &format!(
                "<p>{} is trusted now. It continues on its own; you can close this page.</p>",
                escape(&label)
            ),
        ),
        Ok(Decided::TrustedHere { token, .. }) => {
            let mut response = page(
                StatusCode::OK,
                "This browser is trusted",
                "<p>You can manage Omni from this browser now. <a href=\"/gateway\" style=\"color:#8fb3ff\">Open the MCP gateway</a>.</p>",
            );
            if let Some(cookie) = set_cookie(&headers, &token) {
                response.headers_mut().insert(SET_COOKIE, cookie);
            }
            response
        }
        Ok(Decided::WrongCode { remaining }) => page(
            StatusCode::BAD_REQUEST,
            "That code does not match",
            &format!(
                "<p>Check the code on the browser that asked. {remaining} {tries} left before the request is denied.</p><form method=\"post\"><input name=\"code\" autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\" placeholder=\"ABCD-EFGH\" aria-label=\"Code\" required><button class=\"primary\" name=\"action\" value=\"approve\">Trust it</button><button name=\"action\" value=\"deny\" formnovalidate>Deny</button></form>",
                tries = if remaining == 1 { "try" } else { "tries" }
            ),
        ),
        Ok(Decided::Denied) => page(
            StatusCode::OK,
            "Denied",
            "<p>The browser was not trusted.</p>",
        ),
        Ok(Decided::Gone) => gone_page(),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_describe_common_browsers() {
        assert_eq!(
            browser_label(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15"
            ),
            "Safari on macOS"
        );
        assert_eq!(
            browser_label(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1"
            ),
            "Safari on iPhone"
        );
        assert_eq!(
            browser_label(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36"
            ),
            "Chrome on macOS"
        );
        assert_eq!(browser_label("curl/8.0"), "A browser");
    }

    #[test]
    fn codes_compare_as_typed() {
        assert_eq!(normalize_code(" abcd-efgh "), "ABCDEFGH");
        assert_eq!(display_code("ABCDEFGH"), "ABCD-EFGH");
    }

    #[test]
    fn only_the_owner_cookie_counts() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("a=1; omni_owner=otb_abc; b=2"),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("otb_abc"));
        let mut other = HeaderMap::new();
        other.insert(COOKIE, HeaderValue::from_static("omni_owner=forged"));
        assert_eq!(cookie_token(&other), None);
    }

    #[test]
    fn the_cookie_is_secure_over_https() {
        let mut headers = HeaderMap::new();
        let plain = set_cookie(&headers, "otb_x").unwrap();
        assert!(!plain.to_str().unwrap().contains("Secure"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let secure = set_cookie(&headers, "otb_x").unwrap();
        let text = secure.to_str().unwrap();
        assert!(
            text.contains("HttpOnly")
                && text.contains("SameSite=Strict")
                && text.contains("Secure")
        );
    }
}
