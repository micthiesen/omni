//! The durable MCP Events outbox.
//!
//! Owner identity and encryption keys derive from the MCP bearer token and are
//! never stored. State changes serialize on one short lock (`state_lock`);
//! webhook and authorization I/O run outside it, so a slow callback never
//! delays the email dispatch that publishes events. Publishing commits the
//! receipt and outbox rows atomically, then wakes the delivery worker.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use futures::future::BoxFuture;
use indexmap::IndexMap;
use omni_core::clock::SharedClock;
use omni_core::email::{EmailHandler, FetchedEmail, HandlerError};
use omni_runtime::ports::{PortError, Ports};
use omni_store::StoreError;
use omni_store::cbor::Extra;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

use super::catalog::{EMAIL_RECEIVED, EventArguments};
use super::crypto::{EventCrypto, owner_label};
use super::dynamic::{Definition, EventExtensions, is_gateway_owner, string_arguments};
use super::executor_auth::{EventAuthorizer, ExecutorAuthError};
use super::persistence::{
    DeliveryFailure, DeliveryStatus, DeliveryWithhold, EventDelivery, EventReceipt, EventRequest,
    EventRequestMethod, EventStore, EventSubscription, json_to_js,
};
use super::webhook::{
    WebhookDestination, WebhookEvent, WebhookFailure, WebhookPort, validate_callback_url,
    validate_signing_secret,
};

const LOG: &str = "MCP:Events";
const MINUTE_MS: i64 = 60_000;
const DEFAULT_TTL_MS: i64 = 24 * 60 * MINUTE_MS;
const MAX_TTL_MS: i64 = 7 * DEFAULT_TTL_MS;
const VERIFY_CACHE_MS: i64 = 60 * MINUTE_MS;
const ROTATION_MS: i64 = 5 * MINUTE_MS;
pub const MAX_ATTEMPTS: i64 = 8;
const MAX_DUE_PER_PASS: usize = 10;
const STATUS_RECENT_DELIVERIES: usize = 10;
/// Ask delegated clients to refresh this long before their token expires.
const REFRESH_MARGIN_MS: i64 = MINUTE_MS;
const STATUS_SUBSCRIPTIONS: usize = 20;
const PRUNE_INTERVAL_MS: i64 = 60 * MINUTE_MS;
/// Finished deliveries stay visible in diagnostics for a week.
const DELIVERY_RETENTION_MS: i64 = 7 * 24 * 60 * MINUTE_MS;
/// Receipts outlive IMAP's seven-day INTERNALDATE guard.
const RECEIPT_RETENTION_MS: i64 = 30 * 24 * 60 * MINUTE_MS;
const SUBSCRIPTION_RETENTION_MS: i64 = 7 * 24 * 60 * MINUTE_MS;
const MAX_BACKOFF_MS: i64 = 6 * 60 * MINUTE_MS;
/// How long a delivery waits for a missing integration event definition.
const DEFINITION_RECHECK_MS: i64 = 5 * MINUTE_MS;

fn withheld_recheck_ms(reason: DeliveryWithhold) -> i64 {
    match reason {
        DeliveryWithhold::AuthorizationInvalid => 15 * MINUTE_MS,
        DeliveryWithhold::AuthorizationUnavailable => MINUTE_MS,
    }
}

/// Why a subscription request was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionRejection {
    InvalidEvent,
    InvalidArguments,
    InvalidCallback,
    InvalidPrincipal,
    InvalidSecret,
    ChallengeFailed,
    Timeout,
}

impl SubscriptionRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            SubscriptionRejection::InvalidEvent => "invalid_event",
            SubscriptionRejection::InvalidArguments => "invalid_arguments",
            SubscriptionRejection::InvalidCallback => "invalid_callback",
            SubscriptionRejection::InvalidPrincipal => "invalid_principal",
            SubscriptionRejection::InvalidSecret => "invalid_secret",
            SubscriptionRejection::ChallengeFailed => "challenge_failed",
            SubscriptionRejection::Timeout => "timeout",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventServiceError {
    #[error("Event subscription rejected: {}", .0.as_str())]
    Rejected(SubscriptionRejection),
    #[error(transparent)]
    Authorization(#[from] ExecutorAuthError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("archive echo check failed: {0}")]
    Archive(#[from] PortError),
}

impl EventServiceError {
    /// The diagnostic outcome recorded for a failed request.
    fn outcome(&self) -> &'static str {
        match self {
            EventServiceError::Rejected(reason) => reason.as_str(),
            _ => "error",
        }
    }
}

fn reject(reason: SubscriptionRejection) -> EventServiceError {
    EventServiceError::Rejected(reason)
}

/// `events/subscribe` params (already shape-validated by the RPC layer).
#[derive(Clone, Debug, PartialEq)]
pub struct SubscribeInput {
    pub name: String,
    pub arguments: Value,
    pub url: String,
    pub secret: String,
    pub ttl_ms: Option<i64>,
}

/// `events/unsubscribe` params.
#[derive(Clone, Debug, PartialEq)]
pub struct UnsubscribeInput {
    pub name: String,
    pub arguments: Value,
    pub url: String,
}

/// A delegated client named by the delegated-owner headers at `/mcp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPrincipal {
    pub owner: String,
    pub authorization: String,
}

/// One source observation; replays with the same `receipt_key` are dropped.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishInput {
    pub name: String,
    pub receipt_key: String,
    pub event_key: String,
    pub timestamp: String,
    pub data: Map<String, Value>,
}

/// The `events/subscribe` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeResult {
    pub id: String,
    pub refresh_before: String,
    pub cursor: Option<String>,
    pub truncated: bool,
}

/// A claimed delivery and the decrypted destination it is sent to.
struct Claim {
    row: EventDelivery,
    destination: WebhookDestination,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Withhold(DeliveryWithhold),
}

fn iso(ms: i64) -> String {
    omni_core::js::to_iso_string(ms)
}

/// `callbackHost`: the URL's hostname (at most 253 characters), if any.
fn callback_host(url: Option<&str>) -> Option<String> {
    let parsed = url::Url::parse(url?).ok()?;
    let host: String = parsed.host_str()?.chars().take(253).collect();
    (!host.is_empty()).then_some(host)
}

struct Inner {
    crypto: EventCrypto,
    store: EventStore,
    clock: SharedClock,
    webhook: Arc<dyn WebhookPort>,
    authorizer: Option<Arc<dyn EventAuthorizer>>,
    ports: Ports,
    state_lock: Mutex<()>,
    drain_lock: Mutex<()>,
    wake: Notify,
    last_pruned_at: AtomicI64,
    extensions: std::sync::OnceLock<EventExtensions>,
}

/// The MCP Events service; cheap to clone.
#[derive(Clone)]
pub struct McpEventService {
    inner: Arc<Inner>,
}

impl McpEventService {
    pub fn new(
        token: &str,
        store: omni_store::Store,
        clock: SharedClock,
        webhook: Arc<dyn WebhookPort>,
        authorizer: Option<Arc<dyn EventAuthorizer>>,
        ports: Ports,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                crypto: EventCrypto::new(token),
                store: EventStore::new(store),
                clock,
                webhook,
                authorizer,
                ports,
                state_lock: Mutex::new(()),
                drain_lock: Mutex::new(()),
                wake: Notify::new(),
                last_pruned_at: AtomicI64::new(0),
                extensions: std::sync::OnceLock::new(),
            }),
        }
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    /// Registers integration events and gateway owners (once; later calls
    /// are ignored).
    pub fn extend(&self, extensions: EventExtensions) {
        let _ = self.inner.extensions.set(extensions);
    }

    fn definition<'a>(&'a self, name: &'a str) -> Option<Definition<'a>> {
        let dynamic = self.inner.extensions.get().map(|e| e.dynamic.as_ref());
        Definition::resolve(name, dynamic)
    }

    /// [`Self::definition`] as `owner` sees it: integration events exist only
    /// for gateway principals, so `/mcp` callers never reach them.
    fn definition_for<'a>(&'a self, owner: &str, name: &'a str) -> Option<Definition<'a>> {
        self.definition(name).filter(|definition| {
            matches!(definition, Definition::Static(_)) || is_gateway_owner(owner)
        })
    }

    fn crypto(&self) -> &EventCrypto {
        &self.inner.crypto
    }

    pub fn store(&self) -> &EventStore {
        &self.inner.store
    }

    /// Resolves a delegated principal's token expiry; direct callers have none.
    async fn authorize_principal(
        &self,
        owner: &str,
        principal: Option<&EventPrincipal>,
    ) -> Result<Option<i64>, EventServiceError> {
        let Some(principal) = principal else {
            return Ok(None);
        };
        let expires_at = match &self.inner.authorizer {
            Some(authorizer) => {
                authorizer
                    .authorize(owner, &principal.authorization)
                    .await?
            }
            None => None,
        };
        expires_at
            .map(Some)
            .ok_or(reject(SubscriptionRejection::InvalidPrincipal))
    }

    /// Best effort: diagnostics must never fail or delay an event request.
    async fn record_request(
        &self,
        method: EventRequestMethod,
        owner: &str,
        outcome: &str,
        input: Option<(&str, &Value)>,
        url: Option<&str>,
    ) {
        let definition = input.and_then(|(name, _)| self.definition(name));
        let arguments = match (definition, input) {
            (Some(definition), Some((_, raw))) => definition.parse_arguments(raw),
            _ => None,
        };
        let row = EventRequest {
            id: omni_core::ids::uuid_v4(),
            at: self.now(),
            method,
            owner: owner_label(owner),
            name: definition.map(|d| d.name().to_owned()),
            arguments,
            folder: None,
            callback_host: callback_host(url),
            outcome: outcome.to_owned(),
            extra: Extra::default(),
        };
        if let Err(error) = self.inner.store.record_request(row).await {
            tracing::debug!(target: LOG, error = %error, "Event request diagnostics not recorded");
        }
    }

    /// Records that a client read the event catalog; `owner` is a delegated owner.
    pub async fn record_discovery(&self, owner: Option<&str>) {
        let owner = owner.unwrap_or(self.crypto().direct_owner()).to_owned();
        self.record_request(EventRequestMethod::List, &owner, "listed", None, None)
            .await;
    }

    /// Records that a gateway principal read the event catalog.
    pub async fn record_discovery_as(&self, owner: &str) {
        self.record_request(EventRequestMethod::List, owner, "listed", None, None)
            .await;
    }

    pub async fn subscribe(
        &self,
        input: &SubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<SubscribeResult, EventServiceError> {
        let owner = principal.map_or_else(
            || self.crypto().direct_owner().to_owned(),
            |p| p.owner.clone(),
        );
        self.subscribe_owned(&owner, input, principal).await
    }

    /// A subscription owned by a gateway principal (checked at delivery).
    pub async fn subscribe_as(
        &self,
        owner: &str,
        input: &SubscribeInput,
    ) -> Result<SubscribeResult, EventServiceError> {
        self.subscribe_owned(owner, input, None).await
    }

    async fn subscribe_owned(
        &self,
        owner: &str,
        input: &SubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<SubscribeResult, EventServiceError> {
        let owner = owner.to_owned();
        let result = self.subscribe_inner(&owner, input, principal).await;
        let request = Some((input.name.as_str(), &input.arguments));
        match &result {
            Ok((refreshed, _)) => {
                let outcome = if *refreshed { "refreshed" } else { "accepted" };
                self.record_request(
                    EventRequestMethod::Subscribe,
                    &owner,
                    outcome,
                    request,
                    Some(&input.url),
                )
                .await;
                self.request_drain();
            }
            Err(error) => {
                self.record_request(
                    EventRequestMethod::Subscribe,
                    &owner,
                    error.outcome(),
                    request,
                    Some(&input.url),
                )
                .await;
            }
        }
        result.map(|(_, result)| result)
    }

    /// Authorization and the callback challenge are network I/O, so they run
    /// before the state lock; the row is re-read and written under it.
    async fn subscribe_inner(
        &self,
        owner: &str,
        input: &SubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<(bool, SubscribeResult), EventServiceError> {
        let token_expires_at = self.authorize_principal(owner, principal).await?;
        let definition = self
            .definition_for(owner, &input.name)
            .ok_or(reject(SubscriptionRejection::InvalidEvent))?;
        let args = definition
            .parse_arguments(&input.arguments)
            .ok_or(reject(SubscriptionRejection::InvalidArguments))?;
        let url = validate_callback_url(&input.url)
            .ok_or(reject(SubscriptionRejection::InvalidCallback))?
            .to_string();
        if validate_signing_secret(&input.secret).is_none() {
            return Err(reject(SubscriptionRejection::InvalidSecret));
        }
        let crypto = self.crypto();
        let id = crypto.subscription_id(owner, definition.name(), &args, &url);
        let known = self.inner.store.subscription(&id).await?;
        let recently_verified = known.as_ref().is_some_and(|known| {
            known.owner == owner
                && crypto.open(&known.encrypted_secret).as_deref() == Some(input.secret.as_str())
                && known.verified_at + VERIFY_CACHE_MS > self.now()
        });
        if !recently_verified {
            let destination = WebhookDestination {
                id: id.clone(),
                url: url.clone(),
                secret: input.secret.clone(),
                previous_secret: None,
            };
            if let Err(error) = self.inner.webhook.verify(&destination).await {
                return Err(reject(match error.reason {
                    WebhookFailure::Timeout => SubscriptionRejection::Timeout,
                    WebhookFailure::InvalidSecret => SubscriptionRejection::InvalidSecret,
                    WebhookFailure::InvalidCallback => SubscriptionRejection::InvalidCallback,
                    _ => SubscriptionRejection::ChallengeFailed,
                }));
            }
        }
        let verified_at = self.now();
        let _state = self.inner.state_lock.lock().await;
        let now = self.now();
        let previous = self.inner.store.subscription(&id).await?;
        let same_owner = previous.as_ref().is_some_and(|p| p.owner == owner);
        let same_secret = same_owner
            && previous.as_ref().is_some_and(|p| {
                crypto.open(&p.encrypted_secret).as_deref() == Some(input.secret.as_str())
            });
        let requested = input.ttl_ms.unwrap_or(DEFAULT_TTL_MS);
        let ttl = requested.clamp(1_000, MAX_TTL_MS);
        // Delivery needs a token that validates when it is sent, so ask the
        // client to refresh by the time this one expires. The subscription's
        // lifetime and every authorization check are unchanged.
        let refresh_before = match token_expires_at {
            None => now + ttl,
            Some(expiry) => (now + ttl)
                .min(expiry)
                .min(now.max(expiry - REFRESH_MARGIN_MS)),
        };
        let secret_changed = same_owner && !same_secret;
        let row = EventSubscription {
            id: id.clone(),
            owner: owner.to_owned(),
            key_id: crypto.key_id().to_owned(),
            generation: previous
                .as_ref()
                .map_or_else(omni_core::ids::uuid_v4, |p| p.generation.clone()),
            name: definition.name().to_owned(),
            arguments: Some(args),
            folder: None,
            encrypted_url: crypto.seal(&url),
            encrypted_secret: crypto.seal(&input.secret),
            encrypted_previous_secret: if secret_changed {
                previous.as_ref().map(|p| p.encrypted_secret.clone())
            } else {
                previous
                    .as_ref()
                    .and_then(|p| p.encrypted_previous_secret.clone())
            },
            previous_secret_until: if secret_changed {
                Some(now + ROTATION_MS)
            } else {
                previous.as_ref().and_then(|p| p.previous_secret_until)
            },
            encrypted_authorization: principal.map(|p| crypto.seal(&p.authorization)),
            refresh_before: Some(refresh_before),
            expires_at: now + ttl,
            verified_at: match &previous {
                Some(previous) if recently_verified && same_secret => previous.verified_at,
                _ => verified_at,
            },
            extra: Extra::default(),
        };
        let generation = row.generation.clone();
        self.inner.store.upsert_subscription(row).await?;
        if principal.is_some()
            && let Err(error) = self.release_withheld(&id, &generation, now).await
        {
            tracing::debug!(target: LOG, error = %error, "Withheld events not released");
        }
        Ok((
            same_owner,
            SubscribeResult {
                id,
                refresh_before: iso(refresh_before),
                cursor: None,
                truncated: false,
            },
        ))
    }

    /// A refresh stored a validating token, so withheld events can be attempted now.
    async fn release_withheld(
        &self,
        subscription_id: &str,
        generation: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let released: Vec<EventDelivery> = self
            .inner
            .store
            .deliveries()
            .await?
            .into_iter()
            .filter(|row| {
                row.subscription_id == subscription_id
                    && row.subscription_generation == generation
                    && row.status == DeliveryStatus::Pending
                    && row.withheld.is_some()
            })
            .map(|row| EventDelivery {
                next_attempt_at: now,
                ..row
            })
            .collect();
        self.inner.store.upsert_deliveries(released).await
    }

    pub async fn unsubscribe(
        &self,
        input: &UnsubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<(), EventServiceError> {
        let owner = principal.map_or_else(
            || self.crypto().direct_owner().to_owned(),
            |p| p.owner.clone(),
        );
        self.unsubscribe_owned(&owner, input, principal).await
    }

    /// Removes a gateway principal's subscription.
    pub async fn unsubscribe_as(
        &self,
        owner: &str,
        input: &UnsubscribeInput,
    ) -> Result<(), EventServiceError> {
        self.unsubscribe_owned(owner, input, None).await
    }

    async fn unsubscribe_owned(
        &self,
        owner: &str,
        input: &UnsubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<(), EventServiceError> {
        let owner = owner.to_owned();
        let request = Some((input.name.as_str(), &input.arguments));
        match self.unsubscribe_inner(&owner, input, principal).await {
            Ok(outcome) => {
                self.record_request(
                    EventRequestMethod::Unsubscribe,
                    &owner,
                    outcome,
                    request,
                    Some(&input.url),
                )
                .await;
                Ok(())
            }
            Err(error) => {
                self.record_request(
                    EventRequestMethod::Unsubscribe,
                    &owner,
                    error.outcome(),
                    request,
                    Some(&input.url),
                )
                .await;
                Err(error)
            }
        }
    }

    async fn unsubscribe_inner(
        &self,
        owner: &str,
        input: &UnsubscribeInput,
        principal: Option<&EventPrincipal>,
    ) -> Result<&'static str, EventServiceError> {
        self.authorize_principal(owner, principal).await?;
        let parsed = match self.definition_for(owner, &input.name) {
            Some(definition) => definition
                .parse_arguments(&input.arguments)
                .map(|args| (definition.name().to_owned(), args)),
            // An integration event that is offered again later must not
            // resume to a callback that unsubscribed meanwhile.
            None if is_gateway_owner(owner) => {
                string_arguments(&input.arguments).map(|args| (input.name.clone(), args))
            }
            None => None,
        };
        let Some((name, args)) = parsed else {
            return Ok("not_found");
        };
        let Some(url) = validate_callback_url(&input.url) else {
            return Ok("not_found");
        };
        let id = self
            .crypto()
            .subscription_id(owner, &name, &args, url.as_str());
        let _state = self.inner.state_lock.lock().await;
        let prior = self.inner.store.subscription(&id).await?;
        if prior.is_none_or(|prior| prior.owner != owner) {
            return Ok("not_found");
        }
        self.inner.store.delete_subscription(&id).await?;
        Ok("removed")
    }

    /// Deletes every subscription `owner` holds (a revoked gateway client or
    /// grant); their pending deliveries then fail as inactive. The count.
    pub async fn remove_owner(&self, owner: &str) -> Result<usize, StoreError> {
        let _state = self.inner.state_lock.lock().await;
        let owned: Vec<String> = self
            .inner
            .store
            .subscriptions()
            .await?
            .into_iter()
            .filter(|row| row.owner == owner)
            .map(|row| row.id)
            .collect();
        for id in &owned {
            self.inner.store.delete_subscription(id).await?;
        }
        Ok(owned.len())
    }

    /// Whether any current subscription would receive this event name.
    pub async fn has_active_subscription(&self, name: &str) -> Result<bool, StoreError> {
        Ok(!self.active_arguments(name).await?.is_empty())
    }

    /// The canonical arguments of every current subscription to `name`
    /// (current key, not expired), one entry per subscription.
    pub async fn active_arguments(&self, name: &str) -> Result<Vec<EventArguments>, StoreError> {
        let now = self.now();
        let key_id = self.crypto().key_id();
        Ok(self
            .inner
            .store
            .subscriptions()
            .await?
            .into_iter()
            .filter(|row| row.key_id == key_id && row.name == name && row.expires_at > now)
            .map(|row| row.effective_arguments())
            .collect())
    }

    /// Queues one event for every matching subscription, once per receipt key.
    /// The receipt and outbox rows commit atomically, then delivery starts.
    pub async fn publish(&self, input: PublishInput) -> Result<bool, StoreError> {
        self.publish_inner(input, None).await
    }

    /// [`Self::publish`] for an integration event observed for one argument
    /// set: only gateway subscriptions with exactly those arguments receive
    /// it. `None` when the event is not offered now, so nothing was recorded
    /// and the caller must not move past it.
    pub async fn publish_targeted(
        &self,
        input: PublishInput,
        target: &EventArguments,
    ) -> Result<Option<bool>, StoreError> {
        if self.definition(&input.name).is_none() {
            return Ok(None);
        }
        self.publish_inner(input, Some(target)).await.map(Some)
    }

    async fn publish_inner(
        &self,
        input: PublishInput,
        target: Option<&EventArguments>,
    ) -> Result<bool, StoreError> {
        let Some(definition) = self.definition(&input.name) else {
            return Ok(false);
        };
        let queued = {
            let _state = self.inner.state_lock.lock().await;
            if self
                .inner
                .store
                .receipt(&input.receipt_key)
                .await?
                .is_some()
            {
                false
            } else {
                let now = self.now();
                let event_id = format!(
                    "evt_{}",
                    &omni_core::digest::sha256_hex(&input.event_key)[..40]
                );
                let key_id = self.crypto().key_id();
                let data: IndexMap<String, omni_store::cbor::JsValue> = input
                    .data
                    .iter()
                    .map(|(key, value)| (key.clone(), json_to_js(value)))
                    .collect();
                let deliveries: Vec<EventDelivery> = self
                    .inner
                    .store
                    .subscriptions()
                    .await?
                    .into_iter()
                    .filter(|subscription| {
                        subscription.key_id == key_id
                            && subscription.name == definition.name()
                            && target.is_none_or(|target| {
                                is_gateway_owner(&subscription.owner)
                                    && *target == subscription.effective_arguments()
                            })
                            && subscription.expires_at > now
                            && definition.matches(&subscription.effective_arguments(), &input.data)
                    })
                    .map(|subscription| EventDelivery {
                        id: format!("{}:{event_id}", subscription.id),
                        subscription_id: subscription.id.clone(),
                        owner: subscription.owner.clone(),
                        subscription_generation: subscription.generation.clone(),
                        event_id: event_id.clone(),
                        name: definition.name().to_owned(),
                        timestamp: input.timestamp.clone(),
                        data: data.clone(),
                        attempts: 0,
                        next_attempt_at: now,
                        status: DeliveryStatus::Pending,
                        last_status: None,
                        last_error: None,
                        failure: None,
                        withheld: None,
                        created_at: now,
                        updated_at: now,
                        extra: Extra::default(),
                    })
                    .collect();
                let any = !deliveries.is_empty();
                let receipt = EventReceipt {
                    message_key: input.receipt_key.clone(),
                    name: Some(definition.name().to_owned()),
                    folder: input
                        .data
                        .get("folder")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    received_at: now,
                    extra: Extra::default(),
                };
                self.inner
                    .store
                    .commit_receipt_and_deliveries(receipt, deliveries)
                    .await?;
                any
            }
        };
        if queued {
            self.request_drain();
        }
        Ok(queued)
    }

    /// Called before the IMAP cursor commits. Replayed polls preserve outbox IDs.
    pub async fn record_email(&self, email: &FetchedEmail) -> Result<(), EventServiceError> {
        let Some(origin) = &email.origin else {
            return Ok(());
        };
        let folder = origin.folder.to_lowercase();
        if folder != "inbox" && folder != "archive" {
            return Ok(());
        }
        let Some(message_id) = email
            .message_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        else {
            return Ok(());
        };
        // Never publish without the echo check: an unwired port would turn every
        // archive move into a duplicate event (boot refuses to start without it).
        let echo = self
            .inner
            .ports
            .archive_echo()
            .ok_or(PortError::Unavailable("ArchiveEcho"))?;
        if echo
            .is_archive_action_message(message_id, Some(origin))
            .await?
        {
            return Ok(());
        }
        let message_key = omni_core::digest::sha256_hex(message_id);
        let timestamp = parse_iso(&email.received_at).map_or_else(|| iso(self.now()), iso);
        // The receipt is per Message-ID, so a later move to Archive is not a
        // second arrival. The event key keeps IDs from before generic events.
        let data = json!({
            "messageId": message_id,
            "folder": folder,
            "uidValidity": origin.uid_validity,
            "uid": origin.uid,
        });
        self.publish(PublishInput {
            name: EMAIL_RECEIVED.to_owned(),
            event_key: format!("{message_key}:{folder}"),
            receipt_key: message_key,
            timestamp,
            data: data.as_object().cloned().unwrap_or_default(),
        })
        .await?;
        Ok(())
    }

    /// Asks the delivery worker for a pass; requests made during a pass coalesce.
    pub fn request_drain(&self) {
        self.inner.wake.notify_one();
    }

    /// Runs a delivery pass whenever an event is queued, until `shutdown` is
    /// cancelled. A pass already under way completes first (every claim is
    /// durable before network I/O).
    pub async fn delivery_worker(&self, shutdown: tokio_util::sync::CancellationToken) {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = self.inner.wake.notified() => {}
            }
            if let Err(error) = self.drain().await {
                tracing::warn!(target: LOG, error = %error, "MCP event delivery pass failed");
            }
        }
    }

    /// Retries due rows. Claims before network I/O; keeps event IDs stable.
    pub async fn drain(&self) -> Result<usize, StoreError> {
        let _drain = self.inner.drain_lock.lock().await;
        let now = self.now();
        let mut due: Vec<EventDelivery> = self
            .inner
            .store
            .deliveries()
            .await?
            .into_iter()
            .filter(|row| row.status == DeliveryStatus::Pending && row.next_attempt_at <= now)
            .collect();
        due.sort_by_key(|row| row.next_attempt_at);
        // Every due row is claimed, failed or withheld (each moves it out of
        // the due set), so a backlog drains in back-to-back passes instead
        // of one batch per 30-second sweep.
        let backlog = due.len() > MAX_DUE_PER_PASS;
        due.truncate(MAX_DUE_PER_PASS);
        let authorization = self.authorize_batch(&due).await?;
        let claims = {
            let _state = self.inner.state_lock.lock().await;
            self.claim(&due, &authorization).await?
        };
        for claim in claims {
            self.send(claim).await?;
        }
        self.prune().await?;
        if backlog {
            self.request_drain();
        }
        Ok(due.len())
    }

    /// Checks each delegated subscription in the batch at most once. Executor
    /// access tokens expire hourly, so a subscription without a token that
    /// validates now is withheld until a refresh stores one or it ends.
    async fn authorize_batch(
        &self,
        batch: &[EventDelivery],
    ) -> Result<HashMap<String, Verdict>, StoreError> {
        let mut results = HashMap::new();
        let mut checked = HashSet::new();
        for row in batch {
            if !checked.insert(row.subscription_id.clone()) {
                continue;
            }
            let Some(subscription) = self.inner.store.subscription(&row.subscription_id).await?
            else {
                continue;
            };
            if is_gateway_owner(&subscription.owner) {
                if let Some(extensions) = self.inner.extensions.get() {
                    let verdict = if extensions.owners.is_active(&subscription.owner).await {
                        Verdict::Ok
                    } else {
                        Verdict::Withhold(DeliveryWithhold::AuthorizationInvalid)
                    };
                    results.insert(row.subscription_id.clone(), verdict);
                }
                continue;
            }
            if !subscription.owner.starts_with("executor:") {
                continue;
            }
            let Some(authorization) = subscription
                .encrypted_authorization
                .as_deref()
                .and_then(|sealed| self.crypto().open(sealed))
            else {
                continue;
            };
            let Some(authorizer) = &self.inner.authorizer else {
                continue;
            };
            let check = authorizer
                .authorize(&subscription.owner, &authorization)
                .await;
            let now = self.now();
            let verdict = match check {
                Err(_) => Verdict::Withhold(DeliveryWithhold::AuthorizationUnavailable),
                Ok(Some(expiry)) if expiry > now => Verdict::Ok,
                Ok(_) => Verdict::Withhold(DeliveryWithhold::AuthorizationInvalid),
            };
            results.insert(row.subscription_id.clone(), verdict);
        }
        Ok(results)
    }

    /// Fails, withholds, or claims each due row against current state.
    async fn claim(
        &self,
        batch: &[EventDelivery],
        authorization: &HashMap<String, Verdict>,
    ) -> Result<Vec<Claim>, StoreError> {
        let now = self.now();
        let crypto = self.crypto();
        let mut claims = Vec::new();
        let mut withheld = HashSet::new();
        for due in batch {
            if withheld.contains(&due.subscription_id) {
                continue;
            }
            let Some(row) = self.inner.store.delivery(&due.id).await? else {
                continue;
            };
            if row.status != DeliveryStatus::Pending || row.next_attempt_at > now {
                continue;
            }
            let fail = |failure: DeliveryFailure| EventDelivery {
                status: DeliveryStatus::Failed,
                failure: Some(failure),
                withheld: None,
                updated_at: now,
                ..row.clone()
            };
            if row.attempts >= MAX_ATTEMPTS {
                self.inner
                    .store
                    .upsert_delivery(fail(DeliveryFailure::AttemptsExhausted))
                    .await?;
                continue;
            }
            let subscription = self.inner.store.subscription(&row.subscription_id).await?;
            let definition = self.definition_for(&row.owner, &row.name);
            // An integration event whose definition is momentarily missing (a
            // refresh, an integration toggled off) waits instead of failing.
            if definition.is_none()
                && let Some(current) = &subscription
                && is_gateway_owner(&current.owner)
                && current.generation == row.subscription_generation
                && current.expires_at > now
            {
                self.inner
                    .store
                    .upsert_delivery(EventDelivery {
                        next_attempt_at: now + DEFINITION_RECHECK_MS,
                        updated_at: now,
                        ..row.clone()
                    })
                    .await?;
                continue;
            }
            let active = match (&subscription, definition) {
                (Some(subscription), Some(definition)) => {
                    subscription.key_id == crypto.key_id()
                        && subscription.owner == row.owner
                        && subscription.generation == row.subscription_generation
                        && subscription.expires_at > now
                        && subscription.name == row.name
                        && definition.matches(&subscription.effective_arguments(), &row.data_json())
                }
                _ => false,
            };
            let Some(subscription) = subscription.filter(|_| active) else {
                self.inner
                    .store
                    .upsert_delivery(fail(DeliveryFailure::SubscriptionInactive))
                    .await?;
                continue;
            };
            // A revoked gateway client or grant holds its events until the
            // subscription ends, exactly like an invalid Executor token; with
            // no check registered at all, nothing is delivered.
            if is_gateway_owner(&subscription.owner) {
                match authorization.get(&subscription.id) {
                    Some(Verdict::Ok) => {}
                    Some(Verdict::Withhold(reason)) => {
                        withheld.insert(subscription.id.clone());
                        self.withhold(&subscription.id, *reason, now).await?;
                        continue;
                    }
                    None => {
                        self.inner
                            .store
                            .upsert_delivery(fail(DeliveryFailure::CredentialsUnavailable))
                            .await?;
                        continue;
                    }
                }
            }
            if subscription.owner.starts_with("executor:") {
                match authorization.get(&subscription.id) {
                    None => {
                        self.inner
                            .store
                            .upsert_delivery(fail(DeliveryFailure::CredentialsUnavailable))
                            .await?;
                        continue;
                    }
                    Some(Verdict::Withhold(reason)) => {
                        withheld.insert(subscription.id.clone());
                        self.withhold(&subscription.id, *reason, now).await?;
                        continue;
                    }
                    Some(Verdict::Ok) => {}
                }
            }
            let (Some(url), Some(secret)) = (
                crypto.open(&subscription.encrypted_url),
                crypto.open(&subscription.encrypted_secret),
            ) else {
                self.inner
                    .store
                    .upsert_delivery(fail(DeliveryFailure::CredentialsUnavailable))
                    .await?;
                continue;
            };
            let backoff = 30_000_i64
                .saturating_mul(
                    2_i64.saturating_pow(u32::try_from(row.attempts).unwrap_or(u32::MAX)),
                )
                .min(MAX_BACKOFF_MS);
            let claimed = EventDelivery {
                withheld: None,
                attempts: row.attempts + 1,
                next_attempt_at: now + backoff,
                updated_at: now,
                ..row.clone()
            };
            self.inner.store.upsert_delivery(claimed.clone()).await?;
            let previous_secret = match (
                subscription.previous_secret_until,
                &subscription.encrypted_previous_secret,
            ) {
                (Some(until), Some(sealed)) if until > now => crypto.open(sealed),
                _ => None,
            };
            claims.push(Claim {
                destination: WebhookDestination {
                    id: claimed.subscription_id.clone(),
                    url,
                    secret,
                    previous_secret,
                },
                row: claimed,
            });
        }
        Ok(claims)
    }

    /// Defers all of a subscription's pending rows so it cannot crowd out others.
    async fn withhold(
        &self,
        subscription_id: &str,
        reason: DeliveryWithhold,
        now: i64,
    ) -> Result<(), StoreError> {
        let held: Vec<EventDelivery> = self
            .inner
            .store
            .deliveries()
            .await?
            .into_iter()
            .filter(|row| {
                row.subscription_id == subscription_id && row.status == DeliveryStatus::Pending
            })
            .map(|row| EventDelivery {
                withheld: Some(reason),
                next_attempt_at: row.next_attempt_at.max(now + withheld_recheck_ms(reason)),
                updated_at: now,
                ..row
            })
            .collect();
        self.inner.store.upsert_deliveries(held).await
    }

    async fn send(&self, claim: Claim) -> Result<(), StoreError> {
        let Claim { row, destination } = claim;
        let event = WebhookEvent {
            event_id: row.event_id.clone(),
            name: row.name.clone(),
            timestamp: row.timestamp.clone(),
            data: row.data_json(),
        };
        let response = self.inner.webhook.deliver(&destination, &event).await;
        let status = response.as_ref().ok().copied();
        let rejected = status.is_some_and(|s| (300..500).contains(&s) && s != 408 && s != 429);
        let delivered = status.is_some_and(|s| (200..300).contains(&s));
        let exhausted = !delivered && !rejected && row.attempts >= MAX_ATTEMPTS;
        let _state = self.inner.state_lock.lock().await;
        let updated = EventDelivery {
            status: if delivered {
                DeliveryStatus::Delivered
            } else if rejected || exhausted {
                DeliveryStatus::Failed
            } else {
                DeliveryStatus::Pending
            },
            last_status: status.map(i64::from),
            last_error: response
                .as_ref()
                .err()
                .map(|error| error.reason.as_str().to_owned()),
            failure: if rejected {
                Some(DeliveryFailure::Rejected)
            } else if exhausted {
                Some(DeliveryFailure::AttemptsExhausted)
            } else {
                None
            },
            updated_at: self.now(),
            ..row
        };
        self.inner.store.upsert_delivery(updated).await
    }

    /// Bounds the outbox: finished deliveries, old receipts, long-ended subscriptions.
    async fn prune(&self) -> Result<(), StoreError> {
        let now = self.now();
        if now - self.inner.last_pruned_at.load(Ordering::Relaxed) < PRUNE_INTERVAL_MS {
            return Ok(());
        }
        self.inner.last_pruned_at.store(now, Ordering::Relaxed);
        let _state = self.inner.state_lock.lock().await;
        let store = &self.inner.store;
        for row in store.deliveries().await? {
            if row.status != DeliveryStatus::Pending && row.updated_at < now - DELIVERY_RETENTION_MS
            {
                store.delete_delivery(&row.id).await?;
            }
        }
        for row in store.receipts().await? {
            if row.received_at < now - RECEIPT_RETENTION_MS {
                store.delete_receipt(&row.message_key).await?;
            }
        }
        for row in store.subscriptions().await? {
            if row.expires_at < now - SUBSCRIPTION_RETENTION_MS {
                store.delete_subscription(&row.id).await?;
            }
        }
        Ok(())
    }

    /// Bounded, secret-free view of every event boundary Omni controls
    /// (the `events_status` body without `enabled`).
    pub async fn status(&self) -> Result<Map<String, Value>, StoreError> {
        let now = self.now();
        let crypto = self.crypto();
        let store = &self.inner.store;
        let mut subscriptions = store.subscriptions().await?;
        let deliveries = store.deliveries().await?;
        let mut recent: Vec<&EventDelivery> = deliveries.iter().collect();
        recent.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        recent.truncate(STATUS_RECENT_DELIVERIES);
        let subscription_total = subscriptions.len();
        subscriptions.sort_by_key(|row| std::cmp::Reverse(row.expires_at));
        subscriptions.truncate(STATUS_SUBSCRIPTIONS);
        let count = |f: &dyn Fn(&EventDelivery) -> bool| deliveries.iter().filter(|r| f(r)).count();
        let requests: Vec<Value> = store
            .requests()
            .await?
            .into_iter()
            .map(|row| {
                json!({
                    "at": iso(row.at),
                    "method": row.method,
                    "owner": row.owner,
                    "name": row.name,
                    "arguments": row.arguments,
                    "callbackHost": row.callback_host,
                    "outcome": row.outcome,
                })
            })
            .collect();
        let subscription_rows: Vec<Value> = subscriptions
            .iter()
            .map(|row| {
                let current = row.key_id == crypto.key_id();
                let state = if !current {
                    "stale_key"
                } else if row.expires_at <= now {
                    "expired"
                } else {
                    "active"
                };
                json!({
                    "id": row.id,
                    "name": row.name,
                    "arguments": row.effective_arguments(),
                    "owner": owner_label(&row.owner),
                    "callbackHost": if current {
                        callback_host(crypto.open(&row.encrypted_url).as_deref())
                    } else {
                        None
                    },
                    "state": state,
                    "refreshBefore": iso(row.refresh_before.unwrap_or(row.expires_at)),
                    "expiresAt": iso(row.expires_at),
                    "verifiedAt": iso(row.verified_at),
                })
            })
            .collect();
        let recent_rows: Vec<Value> = recent
            .into_iter()
            .map(|row| {
                json!({
                    "eventId": row.event_id,
                    "subscriptionId": row.subscription_id,
                    "name": row.name,
                    "status": row.status,
                    "attempts": row.attempts,
                    "lastStatus": row.last_status,
                    "lastError": row.last_error,
                    "failure": row.failure,
                    "withheld": row.withheld,
                    "createdAt": iso(row.created_at),
                    "updatedAt": iso(row.updated_at),
                })
            })
            .collect();
        let status = json!({
            "checkedAt": iso(now),
            "subscriptionTotal": subscription_total,
            "subscriptions": subscription_rows,
            "deliveries": {
                "pending": count(&|r| r.status == DeliveryStatus::Pending),
                "withheld": count(&|r| r.status == DeliveryStatus::Pending && r.withheld.is_some()),
                "delivered": count(&|r| r.status == DeliveryStatus::Delivered),
                "failed": count(&|r| r.status == DeliveryStatus::Failed),
                "recent": recent_rows,
            },
            "requests": requests,
        });
        Ok(status.as_object().cloned().unwrap_or_default())
    }

    /// The `McpEvents` email handler (registered first by the binary).
    pub fn email_handler(&self) -> Arc<dyn EmailHandler> {
        Arc::new(McpEventsEmailHandler {
            service: self.clone(),
        })
    }
}

/// `Date.parse` for the ISO timestamps transports emit (always with an offset,
/// so the zone for offset-less input does not matter).
fn parse_iso(value: &str) -> Option<i64> {
    omni_core::js::date_parse(value, &jiff::tz::TimeZone::UTC)
}

struct McpEventsEmailHandler {
    service: McpEventService,
}

impl EmailHandler for McpEventsEmailHandler {
    fn name(&self) -> &'static str {
        "McpEvents"
    }

    fn handle<'a>(&'a self, emails: &'a [FetchedEmail]) -> BoxFuture<'a, Result<(), HandlerError>> {
        Box::pin(async move {
            for email in emails {
                self.service.record_email(email).await.map_err(|error| {
                    HandlerError::transient(error.to_string(), Some(Box::new(error)))
                })?;
            }
            Ok(())
        })
    }
}
