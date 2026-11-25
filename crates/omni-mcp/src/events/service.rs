//! The durable MCP Events outbox.
//!
//! Every subscription is owned by a gateway principal and delivered only
//! while that client or grant is active. Encryption keys derive from the
//! outbox secret and are never stored. State changes serialize on one short
//! lock (`state_lock`); webhook and access-check I/O run outside it, so a
//! slow callback never delays the email dispatch that publishes events.
//! Publishing commits the receipt and outbox rows atomically, then wakes the
//! delivery worker.

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
use super::persistence::{
    DeliveryAttempt, DeliveryFailure, DeliveryStatus, DeliveryWithhold, EventDelivery,
    EventReceipt, EventRequest, EventRequestMethod, EventStore, EventSubscription, NOTICE_PREFIX,
    ROUTE_PREFIX, json_to_js,
};
use super::targets::{
    AttemptOutcome, EventRoute, EventTarget, GIVE_UP_AFTER_MS, OpenTarget, TargetTransport,
    UNCONFIGURED_WAIT_MS, backoff_ms, classify_status, normalize_target_url, valid_connect,
    valid_target_id,
};
use super::webhook::{
    WebhookDestination, WebhookEvent, WebhookFailure, WebhookPort, validate_callback_url,
    validate_signing_secret,
};
use omni_api::outbound::{
    AttemptView, DeliveryCounts, DeliveryKind, DeliveryState, DeliveryView, EventRouteView,
    EventTargetUpsert, EventTargetView, OutboundOverview,
};
use omni_runtime::ports::{NoticeState, NoticeStatus, TargetNotice};
use std::collections::BTreeMap;

const LOG: &str = "MCP:Events";
const MINUTE_MS: i64 = 60_000;
const DEFAULT_TTL_MS: i64 = 24 * 60 * MINUTE_MS;
const MAX_TTL_MS: i64 = 7 * DEFAULT_TTL_MS;
const VERIFY_CACHE_MS: i64 = 60 * MINUTE_MS;
const ROTATION_MS: i64 = 5 * MINUTE_MS;
/// The owner of route deliveries.
const ROUTE_OWNER: &str = "omni:route";
/// The owner of notice deliveries.
const NOTICE_OWNER: &str = "omni:notice";
const MAX_DUE_PER_PASS: usize = 10;
const STATUS_RECENT_DELIVERIES: usize = 10;
const STATUS_SUBSCRIPTIONS: usize = 20;
const PRUNE_INTERVAL_MS: i64 = 60 * MINUTE_MS;
/// Finished deliveries stay visible in diagnostics for a week, failed ones
/// (dead letters) for a month.
const DELIVERY_RETENTION_MS: i64 = 7 * 24 * 60 * MINUTE_MS;
const FAILED_RETENTION_MS: i64 = 30 * 24 * 60 * MINUTE_MS;
/// Receipts outlive IMAP's seven-day INTERNALDATE guard.
const RECEIPT_RETENTION_MS: i64 = 30 * 24 * 60 * MINUTE_MS;
const SUBSCRIPTION_RETENTION_MS: i64 = 7 * 24 * 60 * MINUTE_MS;
/// How long a delivery waits for a missing integration event definition.
const DEFINITION_RECHECK_MS: i64 = 5 * MINUTE_MS;
/// A subscription that expired longer ago than this is no longer reported;
/// a sweep outage longer than this drops the lapse.
const LAPSE_ALERT_WINDOW_MS: i64 = 24 * 60 * MINUTE_MS;

/// Whether `row` ended by expiry (an unsubscribe deletes the row) and the
/// owner has not been told yet.
fn lapse_unreported(row: &EventSubscription, key_id: &str, now: i64) -> bool {
    row.lapse_alerted_at.is_none()
        && row.key_id == key_id
        && is_gateway_owner(&row.owner)
        && row.expires_at <= now
        && row.expires_at > now - LAPSE_ALERT_WINDOW_MS
}

/// A subscription that expired without a refresh, reserved for one owner
/// notification.
#[derive(Clone, Debug, PartialEq)]
pub struct Lapse {
    pub name: String,
    pub arguments: EventArguments,
    /// [`owner_label`] of the owning client or grant.
    pub owner: String,
    pub callback_host: Option<String>,
    pub expired_at: i64,
}

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
    destination: Destination,
}

enum Destination {
    /// An `events/subscribe` webhook (Standard Webhooks signing).
    Webhook(WebhookDestination),
    /// A stored target (bearer key, `Idempotency-Key`).
    Target(OpenTarget),
}

/// The event body subscribers (and routes) receive.
fn envelope(row: &EventDelivery) -> WebhookEvent {
    WebhookEvent {
        event_id: row.event_id.clone(),
        name: row.name.clone(),
        timestamp: row.timestamp.clone(),
        data: row.data_json(),
    }
}

fn log_dead(row: &EventDelivery, failure: DeliveryFailure) {
    if matches!(failure, DeliveryFailure::SubscriptionInactive) {
        tracing::info!(
            target: LOG,
            event = %row.name,
            "Event delivery dropped: its subscription or route ended"
        );
        return;
    }
    tracing::error!(
        target: LOG,
        destination = %row.target.as_deref().unwrap_or("a subscriber"),
        event = %row.name,
        reason = failure.as_str(),
        attempts = row.attempts,
        "Event delivery failed for good; retry it from the MCP gateway page"
    );
}

/// A fresh pending row for one event (callers set the destination).
fn new_delivery(
    event_id: &str,
    name: &str,
    timestamp: &str,
    data: &IndexMap<String, omni_store::cbor::JsValue>,
    now: i64,
) -> EventDelivery {
    EventDelivery {
        id: String::new(),
        subscription_id: String::new(),
        owner: String::new(),
        subscription_generation: String::new(),
        event_id: event_id.to_owned(),
        name: name.to_owned(),
        timestamp: timestamp.to_owned(),
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
        target: None,
        body: None,
        idempotency_key: None,
        requeued_at: None,
        history: Vec::new(),
        extra: Extra::default(),
    }
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
    ports: Ports,
    state_lock: Mutex<()>,
    drain_lock: Mutex<()>,
    wake: Notify,
    last_pruned_at: AtomicI64,
    extensions: std::sync::OnceLock<EventExtensions>,
    targets: std::sync::OnceLock<Arc<dyn TargetTransport>>,
}

/// The MCP Events service; cheap to clone.
#[derive(Clone)]
pub struct McpEventService {
    inner: Arc<Inner>,
}

impl McpEventService {
    /// The service over the key [`EventCrypto::new`] derives from `secret`
    /// (tests and fixtures).
    pub fn new(
        secret: &str,
        store: omni_store::Store,
        clock: SharedClock,
        webhook: Arc<dyn WebhookPort>,
        ports: Ports,
    ) -> Self {
        Self::with_crypto(EventCrypto::new(secret), store, clock, webhook, ports)
    }

    /// The service over the storage key from `mcp-events.key`.
    pub fn from_key(
        key: [u8; 32],
        store: omni_store::Store,
        clock: SharedClock,
        webhook: Arc<dyn WebhookPort>,
        ports: Ports,
    ) -> Self {
        Self::with_crypto(EventCrypto::from_key(key), store, clock, webhook, ports)
    }

    fn with_crypto(
        crypto: EventCrypto,
        store: omni_store::Store,
        clock: SharedClock,
        webhook: Arc<dyn WebhookPort>,
        ports: Ports,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                crypto,
                store: EventStore::new(store),
                clock,
                webhook,
                ports,
                state_lock: Mutex::new(()),
                drain_lock: Mutex::new(()),
                wake: Notify::new(),
                last_pruned_at: AtomicI64::new(0),
                extensions: std::sync::OnceLock::new(),
                targets: std::sync::OnceLock::new(),
            }),
        }
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    /// Sets the sender for stored targets (once; later calls are ignored).
    pub fn set_target_transport(&self, transport: Arc<dyn TargetTransport>) {
        let _ = self.inner.targets.set(transport);
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

    fn crypto(&self) -> &EventCrypto {
        &self.inner.crypto
    }

    pub fn store(&self) -> &EventStore {
        &self.inner.store
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

    /// Records that a gateway principal read the event catalog.
    pub async fn record_discovery_as(&self, owner: &str) {
        self.record_request(EventRequestMethod::List, owner, "listed", None, None)
            .await;
    }

    /// A subscription owned by a gateway principal (checked at delivery).
    pub async fn subscribe_as(
        &self,
        owner: &str,
        input: &SubscribeInput,
    ) -> Result<SubscribeResult, EventServiceError> {
        let owner = owner.to_owned();
        let result = self.subscribe_inner(&owner, input).await;
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

    /// The callback challenge is network I/O, so it runs before the state
    /// lock; the row is re-read and written under it.
    async fn subscribe_inner(
        &self,
        owner: &str,
        input: &SubscribeInput,
    ) -> Result<(bool, SubscribeResult), EventServiceError> {
        let definition = self
            .definition(&input.name)
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
        let refresh_before = now + ttl;
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
            encrypted_authorization: None,
            refresh_before: Some(refresh_before),
            expires_at: now + ttl,
            verified_at: match &previous {
                Some(previous) if recently_verified && same_secret => previous.verified_at,
                _ => verified_at,
            },
            lapse_alerted_at: None,
            extra: Extra::default(),
        };
        self.inner.store.upsert_subscription(row).await?;
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

    /// Removes a gateway principal's subscription.
    pub async fn unsubscribe_as(
        &self,
        owner: &str,
        input: &UnsubscribeInput,
    ) -> Result<(), EventServiceError> {
        let owner = owner.to_owned();
        let request = Some((input.name.as_str(), &input.arguments));
        match self.unsubscribe_inner(&owner, input).await {
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
    ) -> Result<&'static str, EventServiceError> {
        let parsed = match self.definition(&input.name) {
            Some(definition) => definition
                .parse_arguments(&input.arguments)
                .map(|args| (definition.name().to_owned(), args)),
            // An integration event that is offered again later must not
            // resume to a callback that unsubscribed meanwhile.
            None => string_arguments(&input.arguments).map(|args| (input.name.clone(), args)),
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

    /// Deletes every subscription not owned by a gateway principal: Executor's
    /// delegated owners and the `/mcp` token's, from before the gateway was
    /// Omni's only MCP endpoint. Their pending deliveries then fail as
    /// inactive. The count.
    pub async fn drop_legacy_subscriptions(&self) -> Result<usize, StoreError> {
        let _state = self.inner.state_lock.lock().await;
        let legacy: Vec<String> = self
            .inner
            .store
            .subscriptions()
            .await?
            .into_iter()
            .filter(|row| !is_gateway_owner(&row.owner))
            .map(|row| row.id)
            .collect();
        for id in &legacy {
            self.inner.store.delete_subscription(id).await?;
        }
        Ok(legacy.len())
    }

    /// Whether any current subscription would receive this event name.
    pub async fn has_active_subscription(&self, name: &str) -> Result<bool, StoreError> {
        Ok(!self.active_arguments(name).await?.is_empty())
    }

    /// The canonical arguments of every current subscription to `name`
    /// (current key, not expired) and every enabled route, one entry each.
    pub async fn active_arguments(&self, name: &str) -> Result<Vec<EventArguments>, StoreError> {
        let now = self.now();
        let key_id = self.crypto().key_id();
        let mut active: Vec<EventArguments> = self
            .inner
            .store
            .subscriptions()
            .await?
            .into_iter()
            .filter(|row| row.key_id == key_id && row.name == name && row.expires_at > now)
            .map(|row| row.effective_arguments())
            .collect();
        active.extend(
            self.inner
                .store
                .routes()
                .await?
                .into_iter()
                .filter(|route| route.enabled && route.name == name)
                .map(|route| route.arguments),
        );
        Ok(active)
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
                        ..new_delivery(&event_id, definition.name(), &input.timestamp, &data, now)
                    })
                    .collect();
                let mut deliveries = deliveries;
                // Stored routes receive the same event, addressed to a target.
                for route in self.inner.store.routes().await? {
                    let args = route.arguments.clone();
                    if route.enabled
                        && route.name == definition.name()
                        && target.is_none_or(|target| *target == args)
                        && definition.matches(&args, &input.data)
                    {
                        deliveries.push(EventDelivery {
                            id: format!("{ROUTE_PREFIX}{}:{event_id}", route.id),
                            subscription_id: format!("{ROUTE_PREFIX}{}", route.id),
                            owner: ROUTE_OWNER.to_owned(),
                            subscription_generation: route.created_at.to_string(),
                            target: Some(route.target.clone()),
                            ..new_delivery(
                                &event_id,
                                definition.name(),
                                &input.timestamp,
                                &data,
                                now,
                            )
                        });
                    }
                }
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

    /// Checks each subscription's owner in the batch at most once: a revoked
    /// gateway client or grant is withheld until it is active again or the
    /// subscription ends. Other owners get no verdict and never deliver.
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
            if !is_gateway_owner(&subscription.owner) {
                continue;
            }
            if let Some(extensions) = self.inner.extensions.get() {
                let verdict = if extensions.owners.is_active(&subscription.owner).await {
                    Verdict::Ok
                } else {
                    Verdict::Withhold(DeliveryWithhold::AuthorizationInvalid)
                };
                results.insert(row.subscription_id.clone(), verdict);
            }
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
            // Every destination gets the same window: a day of retries. Time
            // spent withheld (a revoked owner) does not count.
            if row.attempts > 0
                && row.withheld.is_none()
                && now - row.window_start() >= GIVE_UP_AFTER_MS
            {
                self.settle_failed(row, DeliveryFailure::AttemptsExhausted, now)
                    .await?;
                continue;
            }
            if row.target.is_some() {
                if let Some(claim) = self.claim_target(row, now).await? {
                    claims.push(claim);
                }
                continue;
            }
            let subscription = self.inner.store.subscription(&row.subscription_id).await?;
            let definition = self.definition(&row.name);
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
                self.settle_failed(row, DeliveryFailure::SubscriptionInactive, now)
                    .await?;
                continue;
            };
            // A revoked gateway client or grant holds its events until the
            // subscription ends; an owner with no verdict (not a gateway
            // principal, or no check registered at all) is never delivered.
            match authorization.get(&subscription.id) {
                Some(Verdict::Ok) => {}
                Some(Verdict::Withhold(reason)) => {
                    withheld.insert(subscription.id.clone());
                    self.withhold(&subscription.id, *reason, now).await?;
                    continue;
                }
                None => {
                    self.settle_failed(row, DeliveryFailure::CredentialsUnavailable, now)
                        .await?;
                    continue;
                }
            }
            let (Some(url), Some(secret)) = (
                crypto.open(&subscription.encrypted_url),
                crypto.open(&subscription.encrypted_secret),
            ) else {
                self.settle_failed(row, DeliveryFailure::CredentialsUnavailable, now)
                    .await?;
                continue;
            };
            let claimed = EventDelivery {
                withheld: None,
                attempts: row.attempts + 1,
                next_attempt_at: now + backoff_ms(row.attempts + 1),
                // A row that was held starts a fresh window now.
                requeued_at: if row.withheld.is_some() {
                    Some(now)
                } else {
                    row.requeued_at
                },
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
                destination: Destination::Webhook(WebhookDestination {
                    id: claimed.subscription_id.clone(),
                    url,
                    secret,
                    previous_secret,
                }),
                row: claimed,
            });
        }
        Ok(claims)
    }

    /// Claims a route or notice row for its stored target.
    async fn claim_target(
        &self,
        row: EventDelivery,
        now: i64,
    ) -> Result<Option<Claim>, StoreError> {
        let target_id = row.target.clone().unwrap_or_default();
        if let Some(route_id) = row.route_id() {
            let route = self.inner.store.route(route_id).await?;
            if route
                .as_ref()
                .is_none_or(|route| !route.enabled || route.target != target_id)
            {
                // The owner switched the route off or removed it: what it
                // still owed is dropped quietly, not a failure.
                self.inner
                    .store
                    .upsert_delivery(EventDelivery {
                        status: DeliveryStatus::Skipped,
                        updated_at: now,
                        ..row
                    })
                    .await?;
                return Ok(None);
            }
        }
        let Some(target) = self.inner.store.target(&target_id).await? else {
            if row.is_notice() {
                // A notice waits for its target; creating it sends the notice.
                self.inner
                    .store
                    .upsert_delivery(EventDelivery {
                        status: DeliveryStatus::Unconfigured,
                        updated_at: now,
                        ..row
                    })
                    .await?;
            } else {
                self.settle_failed(row, DeliveryFailure::TargetMissing, now)
                    .await?;
            }
            return Ok(None);
        };
        let key = if target.encrypted_key.is_empty() {
            Some(String::new())
        } else {
            self.crypto().open(&target.encrypted_key)
        };
        let (Some(key), Ok(url)) = (key, url::Url::parse(&target.url)) else {
            self.settle_failed(row, DeliveryFailure::CredentialsUnavailable, now)
                .await?;
            return Ok(None);
        };
        let claimed = EventDelivery {
            withheld: None,
            attempts: row.attempts + 1,
            next_attempt_at: now + backoff_ms(row.attempts + 1),
            updated_at: now,
            ..row
        };
        self.inner.store.upsert_delivery(claimed.clone()).await?;
        Ok(Some(Claim {
            destination: Destination::Target(OpenTarget {
                id: target.id,
                url,
                key,
                connect: target.connect,
            }),
            row: claimed,
        }))
    }

    /// Fails a row for good, and says so at ERROR (which alerts): a dead
    /// delivery never passes silently.
    async fn settle_failed(
        &self,
        row: EventDelivery,
        failure: DeliveryFailure,
        now: i64,
    ) -> Result<(), StoreError> {
        log_dead(&row, failure);
        self.inner
            .store
            .upsert_delivery(EventDelivery {
                status: DeliveryStatus::Failed,
                failure: Some(failure),
                withheld: None,
                updated_at: now,
                ..row
            })
            .await
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
        let started = tokio::time::Instant::now();
        let (status, ignored, error) = match &destination {
            Destination::Webhook(destination) => {
                match self
                    .inner
                    .webhook
                    .deliver(destination, &envelope(&row))
                    .await
                {
                    Ok(status) => (Some(status), false, None),
                    Err(error) => (None, false, Some(error.reason.as_str().to_owned())),
                }
            }
            Destination::Target(target) => match self.inner.targets.get() {
                None => (None, false, Some("no_transport".to_owned())),
                Some(transport) => {
                    let body = row.body.clone().unwrap_or_else(|| {
                        omni_core::js::json_stringify(&envelope(&row).to_json())
                    });
                    let key = row
                        .idempotency_key
                        .clone()
                        .unwrap_or_else(|| row.event_id.clone());
                    match transport.post(target, &row.name, &key, &body).await {
                        Ok(response) => (Some(response.status), response.ignored, None),
                        Err(super::targets::TargetError::Unusable) => {
                            (None, false, Some("unusable_answer".to_owned()))
                        }
                        Err(error) => (None, false, Some(error.to_string())),
                    }
                }
            },
        };
        let ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let outcome = match (status, error.as_deref()) {
            (Some(status), _) => classify_status(status, ignored),
            (None, Some("unusable_answer")) => {
                AttemptOutcome::Rejected("unusable_answer".to_owned())
            }
            (None, error) => AttemptOutcome::Retry(error.unwrap_or_default().to_owned()),
        };
        let _state = self.inner.state_lock.lock().await;
        let now = self.now();
        let cancelled = matches!(
            self.inner.store.delivery(&row.id).await?,
            Some(current) if current.status == DeliveryStatus::Skipped
        );
        let mut updated = row;
        updated.record_attempt(DeliveryAttempt {
            at: now,
            status: status.map(i64::from),
            error: match &outcome {
                AttemptOutcome::Retry(reason) | AttemptOutcome::Rejected(reason) => {
                    Some(reason.clone())
                }
                _ => None,
            },
            ms,
        });
        updated.last_status = status.map(i64::from);
        updated.last_error = error;
        updated.updated_at = now;
        match outcome {
            AttemptOutcome::Delivered => {
                updated.status = DeliveryStatus::Delivered;
                updated.failure = None;
            }
            AttemptOutcome::Ignored => {
                tracing::warn!(
                    target: LOG,
                    destination = %updated.target.as_deref().unwrap_or("a subscriber"),
                    event = %updated.name,
                    "Event target accepted the delivery but ran no webhook routine"
                );
                updated.status = DeliveryStatus::Ignored;
                updated.failure = None;
            }
            AttemptOutcome::Retry(_) if now - updated.window_start() < GIVE_UP_AFTER_MS => {}
            AttemptOutcome::Retry(_) => {
                log_dead(&updated, DeliveryFailure::AttemptsExhausted);
                updated.status = DeliveryStatus::Failed;
                updated.failure = Some(DeliveryFailure::AttemptsExhausted);
            }
            AttemptOutcome::Rejected(_) => {
                log_dead(&updated, DeliveryFailure::Rejected);
                updated.status = DeliveryStatus::Failed;
                updated.failure = Some(DeliveryFailure::Rejected);
            }
        }
        // A cancellation while the attempt ran wins over a retry.
        if cancelled && updated.status == DeliveryStatus::Pending {
            updated.status = DeliveryStatus::Skipped;
        }
        // Notice bodies hold message text (a Claude reply, a request): kept
        // only while they may still be sent.
        if updated.is_notice()
            && matches!(
                updated.status,
                DeliveryStatus::Delivered | DeliveryStatus::Ignored | DeliveryStatus::Skipped
            )
        {
            updated.body = None;
        }
        self.inner.store.upsert_delivery(updated).await
    }

    /// Reserves one notification for each subscription that expired without
    /// a refresh while its owner still has access and holds no other live
    /// subscription to that event, and returns them. Each
    /// lapse is reserved durably before it is returned, so it is reported at
    /// most once; a later refresh writes a fresh row that can lapse again.
    pub async fn reserve_lapses(&self) -> Result<Vec<Lapse>, StoreError> {
        let Some(extensions) = self.inner.extensions.get() else {
            return Ok(Vec::new());
        };
        let now = self.now();
        let crypto = self.crypto();
        let key_id = crypto.key_id();
        let rows = self.inner.store.subscriptions().await?;
        // An owner that still holds the event (a changed callback URL or
        // arguments is a new row) moved on rather than stopped.
        let live: HashSet<(&str, &str)> = rows
            .iter()
            .filter(|row| row.key_id == key_id && row.expires_at > now)
            .map(|row| (row.owner.as_str(), row.name.as_str()))
            .collect();
        let (superseded, candidates): (Vec<EventSubscription>, Vec<EventSubscription>) = rows
            .iter()
            .filter(|row| lapse_unreported(row, key_id, now))
            .cloned()
            .partition(|row| live.contains(&(row.owner.as_str(), row.name.as_str())));
        let mut active = HashMap::new();
        for row in &candidates {
            if !active.contains_key(&row.owner) {
                let verdict = extensions.owners.is_active(&row.owner).await;
                active.insert(row.owner.clone(), verdict);
            }
        }
        let _state = self.inner.state_lock.lock().await;
        let mut lapses = Vec::new();
        let superseded_ids: HashSet<String> = superseded.iter().map(|row| row.id.clone()).collect();
        for candidate in superseded.into_iter().chain(candidates) {
            let replaced = superseded_ids.contains(&candidate.id);
            if !replaced && active.get(&candidate.owner) != Some(&true) {
                continue;
            }
            // A refresh or unsubscribe since the scan wins.
            let Some(row) = self.inner.store.subscription(&candidate.id).await? else {
                continue;
            };
            if row.expires_at != candidate.expires_at || !lapse_unreported(&row, key_id, now) {
                continue;
            }
            let reported = !replaced;
            let lapse = Lapse {
                name: row.name.clone(),
                arguments: row.effective_arguments(),
                owner: owner_label(&row.owner),
                callback_host: callback_host(crypto.open(&row.encrypted_url).as_deref()),
                expired_at: row.expires_at,
            };
            self.inner
                .store
                .upsert_subscription(EventSubscription {
                    lapse_alerted_at: Some(now),
                    ..row
                })
                .await?;
            if reported {
                lapses.push(lapse);
            }
        }
        Ok(lapses)
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
            let keep_ms = match row.status {
                DeliveryStatus::Pending => continue,
                DeliveryStatus::Failed | DeliveryStatus::Unconfigured => FAILED_RETENTION_MS,
                DeliveryStatus::Delivered | DeliveryStatus::Ignored | DeliveryStatus::Skipped => {
                    DELIVERY_RETENTION_MS
                }
            };
            if row.updated_at < now - keep_ms {
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

/// Why an outbound (journal) change was refused.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

const TARGET_LABEL_LIMIT: usize = 80;
const TARGET_KEY_LIMIT: usize = 4096;
const OVERVIEW_DELIVERIES: usize = 100;

fn invalid(message: impl Into<String>) -> JournalError {
    JournalError::Invalid(message.into())
}

fn target_view(row: &EventTarget) -> EventTargetView {
    EventTargetView {
        id: row.id.clone(),
        label: row.label.clone(),
        url: row.url.clone(),
        key_set: !row.encrypted_key.is_empty(),
        connect: row.connect.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn route_view(row: &EventRoute) -> EventRouteView {
    EventRouteView {
        id: row.id.clone(),
        event: row.name.clone(),
        arguments: row
            .arguments
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        target: row.target.clone(),
        enabled: row.enabled,
        created_at: row.created_at,
    }
}

fn delivery_state(status: DeliveryStatus) -> DeliveryState {
    match status {
        DeliveryStatus::Pending => DeliveryState::Pending,
        DeliveryStatus::Delivered => DeliveryState::Delivered,
        DeliveryStatus::Ignored => DeliveryState::Ignored,
        DeliveryStatus::Failed => DeliveryState::Failed,
        DeliveryStatus::Unconfigured => DeliveryState::Unconfigured,
        DeliveryStatus::Skipped => DeliveryState::Skipped,
    }
}

pub fn notice_status_of(status: DeliveryStatus) -> NoticeStatus {
    notice_status(status)
}

fn notice_status(status: DeliveryStatus) -> NoticeStatus {
    match status {
        DeliveryStatus::Pending => NoticeStatus::Pending,
        DeliveryStatus::Delivered => NoticeStatus::Delivered,
        DeliveryStatus::Ignored => NoticeStatus::Ignored,
        DeliveryStatus::Failed => NoticeStatus::Failed,
        DeliveryStatus::Unconfigured => NoticeStatus::Unconfigured,
        DeliveryStatus::Skipped => NoticeStatus::Skipped,
    }
}

fn delivery_view(row: &EventDelivery, hosts: &HashMap<String, String>) -> DeliveryView {
    let (kind, destination) = if let Some(target) = &row.target {
        let kind = if row.is_notice() {
            DeliveryKind::Notice
        } else {
            DeliveryKind::Route
        };
        (kind, target.clone())
    } else {
        (
            DeliveryKind::Subscription,
            hosts
                .get(&row.subscription_id)
                .cloned()
                .unwrap_or_else(|| owner_label(&row.owner)),
        )
    };
    DeliveryView {
        id: row.id.clone(),
        event: row.name.clone(),
        kind,
        destination,
        state: delivery_state(row.status),
        attempts: u32::try_from(row.attempts).unwrap_or(u32::MAX),
        created_at: row.created_at,
        updated_at: row.updated_at,
        next_attempt_at: (row.status == DeliveryStatus::Pending).then_some(row.next_attempt_at),
        failure: row.failure.map(|f| f.as_str().to_owned()),
        history: row
            .history
            .iter()
            .rev()
            .map(|attempt| AttemptView {
                at: attempt.at,
                status: attempt.status.and_then(|s| u16::try_from(s).ok()),
                error: attempt.error.clone(),
                duration_ms: u32::try_from(attempt.ms).unwrap_or(u32::MAX),
            })
            .collect(),
    }
}

/// A bearer key as stored: printable, no whitespace, bounded.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= TARGET_KEY_LIMIT && key.chars().all(|c| c.is_ascii_graphic())
}

impl McpEventService {
    /// The canonical arguments of a route for `name`, or `None` when the
    /// event or the arguments are not offered.
    pub fn parse_route_arguments(&self, name: &str, raw: &Value) -> Option<EventArguments> {
        self.definition(name)?.parse_arguments(raw)
    }

    /// Whether `name` is an event the journal can carry now.
    pub fn knows_event(&self, name: &str) -> bool {
        self.definition(name).is_some()
    }

    /// Creates or replaces a target; a missing key keeps the stored one.
    /// Notices that waited for it are sent now.
    pub async fn upsert_target(
        &self,
        id: &str,
        input: &EventTargetUpsert,
    ) -> Result<EventTargetView, JournalError> {
        if !valid_target_id(id) {
            return Err(invalid(
                "A target id is 1 to 32 lowercase letters, digits or dashes, starting with a letter.",
            ));
        }
        let label = input.label.trim();
        if label.is_empty() || label.chars().count() > TARGET_LABEL_LIMIT {
            return Err(invalid("A target needs a label of at most 80 characters."));
        }
        let url = normalize_target_url(&input.url)
            .ok_or_else(|| invalid("The URL must be http(s) without credentials or a fragment."))?;
        let connect = match input.connect.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(host) if valid_connect(host) => Some(host.to_owned()),
            Some(_) => return Err(invalid("The connect host is a bare host name.")),
        };
        let now = self.now();
        let existing = self.inner.store.target(id).await?;
        let encrypted_key = match (input.key.as_deref(), &existing) {
            // An empty key means none: no Authorization is sent.
            (Some(""), _) => String::new(),
            (Some(key), _) if valid_key(key) => {
                omni_core::secrets::register_runtime(key);
                self.crypto().seal(key)
            }
            (Some(_), _) => return Err(invalid("The key must be printable, without spaces.")),
            (None, Some(existing)) => existing.encrypted_key.clone(),
            (None, None) => return Err(invalid("A new target needs a key.")),
        };
        let row = EventTarget {
            id: id.to_owned(),
            label: label.to_owned(),
            url,
            encrypted_key,
            connect,
            created_at: existing.as_ref().map_or(now, |e| e.created_at),
            updated_at: now,
            extra: existing.map(|e| e.extra).unwrap_or_default(),
        };
        let view = target_view(&row);
        self.inner.store.upsert_target(row).await?;
        let waiting = self.requeue_unconfigured(id).await?;
        if waiting > 0 {
            tracing::info!(target: LOG, destination = %id, waiting, "Sending notices that waited for this event target");
            self.request_drain();
        }
        Ok(view)
    }

    /// Stores a target from the old environment settings unless one with
    /// this id exists (boot migration); whether it was stored.
    pub async fn import_target(
        &self,
        id: &str,
        label: &str,
        url: &str,
        key: &str,
        connect: Option<&str>,
    ) -> Result<bool, JournalError> {
        // Imported once per id: a target the owner removed or edited on
        // `/gateway` is not brought back by the old setting.
        if self.inner.store.target_imported(id).await? {
            return Ok(false);
        }
        if self.inner.store.target(id).await?.is_some() {
            self.inner
                .store
                .mark_target_imported(id, self.now())
                .await?;
            return Ok(false);
        }
        self.upsert_target(
            id,
            &EventTargetUpsert {
                label: label.to_owned(),
                url: url.to_owned(),
                key: Some(key.to_owned()),
                connect: connect.map(str::to_owned),
            },
        )
        .await?;
        self.inner
            .store
            .mark_target_imported(id, self.now())
            .await?;
        Ok(true)
    }

    /// Registers every stored target key with the log scrubber (boot).
    pub async fn register_target_secrets(&self) -> Result<usize, StoreError> {
        let targets = self.inner.store.targets().await?;
        for target in &targets {
            if let Some(key) = self.crypto().open(&target.encrypted_key) {
                omni_core::secrets::register_runtime(&key);
            }
        }
        Ok(targets.len())
    }

    async fn requeue_unconfigured(&self, target: &str) -> Result<usize, StoreError> {
        let now = self.now();
        let _state = self.inner.state_lock.lock().await;
        let waiting: Vec<EventDelivery> = self
            .inner
            .store
            .deliveries()
            .await?
            .into_iter()
            .filter(|row| {
                row.status == DeliveryStatus::Unconfigured
                    && row.target.as_deref() == Some(target)
                    && now - row.created_at < UNCONFIGURED_WAIT_MS
            })
            .map(|row| EventDelivery {
                status: DeliveryStatus::Pending,
                next_attempt_at: now,
                requeued_at: Some(now),
                updated_at: now,
                ..row
            })
            .collect();
        let count = waiting.len();
        self.inner.store.upsert_deliveries(waiting).await?;
        Ok(count)
    }

    /// Removes a target no route uses.
    pub async fn delete_target(&self, id: &str) -> Result<(), JournalError> {
        if let Some(route) = self
            .inner
            .store
            .routes()
            .await?
            .into_iter()
            .find(|route| route.target == id)
        {
            return Err(JournalError::Conflict(format!(
                "Route {} ({}) still sends to {id}; remove it first.",
                route.id, route.name
            )));
        }
        if self.inner.store.delete_target(id).await? {
            Ok(())
        } else {
            Err(JournalError::NotFound(format!("No event target {id}")))
        }
    }

    /// A route from `name` with canonical `arguments` to `target`.
    pub async fn create_route(
        &self,
        name: &str,
        arguments: EventArguments,
        target: &str,
    ) -> Result<EventRouteView, JournalError> {
        if self.inner.store.target(target).await?.is_none() {
            return Err(JournalError::NotFound(format!("No event target {target}")));
        }
        let digest = omni_core::digest::sha256_hex(format!(
            "{name}\u{0}{}\u{0}{target}",
            super::catalog::canonical_event_arguments(&arguments)
        ));
        let id = format!("rt_{}", &digest[..16]);
        if self.inner.store.route(&id).await?.is_some() {
            return Err(JournalError::Conflict(
                "That route already exists.".to_owned(),
            ));
        }
        let row = EventRoute {
            id,
            name: name.to_owned(),
            arguments,
            target: target.to_owned(),
            enabled: true,
            created_at: self.now(),
            extra: Extra::default(),
        };
        let view = route_view(&row);
        self.inner.store.upsert_route(row).await?;
        Ok(view)
    }

    pub async fn set_route_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<EventRouteView, JournalError> {
        let Some(mut row) = self.inner.store.route(id).await? else {
            return Err(JournalError::NotFound(format!("No route {id}")));
        };
        row.enabled = enabled;
        let view = route_view(&row);
        self.inner.store.upsert_route(row).await?;
        Ok(view)
    }

    pub async fn delete_route(&self, id: &str) -> Result<(), JournalError> {
        if self.inner.store.delete_route(id).await? {
            Ok(())
        } else {
            Err(JournalError::NotFound(format!("No route {id}")))
        }
    }

    /// Queues a notice for a stored target, once per key.
    pub async fn notify_target(&self, notice: TargetNotice) -> Result<bool, StoreError> {
        let state = self.inner.state_lock.lock().await;
        let now = self.now();
        let exists = self.inner.store.target(&notice.target).await?.is_some();
        let event_id = format!("ntc_{}", &omni_core::digest::sha256_hex(&notice.key)[..40]);
        let row = EventDelivery {
            id: format!("{NOTICE_PREFIX}{}", notice.key),
            subscription_id: format!("{NOTICE_PREFIX}{}", notice.target),
            owner: NOTICE_OWNER.to_owned(),
            target: Some(notice.target.clone()),
            body: Some(omni_core::js::json_stringify(&notice.body)),
            idempotency_key: Some(notice.key.clone()),
            status: if exists {
                DeliveryStatus::Pending
            } else {
                DeliveryStatus::Unconfigured
            },
            ..new_delivery(&event_id, &notice.name, &iso(now), &IndexMap::new(), now)
        };
        if !exists {
            tracing::warn!(
                target: LOG,
                destination = %notice.target,
                event = %notice.name,
                "Notice queued for an event target that does not exist; it is sent once the target is created"
            );
        }
        let inserted = self.inner.store.insert_delivery(row).await?;
        drop(state);
        if inserted && exists {
            self.request_drain();
        }
        Ok(inserted)
    }

    /// Where notices stand, by key.
    pub async fn notice_states(
        &self,
        keys: &[String],
    ) -> Result<BTreeMap<String, NoticeState>, StoreError> {
        let mut states = BTreeMap::new();
        for key in keys {
            if let Some(row) = self
                .inner
                .store
                .delivery(&format!("{NOTICE_PREFIX}{key}"))
                .await?
            {
                states.insert(
                    key.clone(),
                    NoticeState {
                        status: notice_status(row.status),
                        attempts: u32::try_from(row.attempts).unwrap_or(u32::MAX),
                    },
                );
            }
        }
        Ok(states)
    }

    /// Drops a notice that has not been delivered or settled.
    pub async fn cancel_notice(&self, key: &str) -> Result<bool, StoreError> {
        let _state = self.inner.state_lock.lock().await;
        let id = format!("{NOTICE_PREFIX}{key}");
        let Some(row) = self.inner.store.delivery(&id).await? else {
            return Ok(false);
        };
        if !matches!(
            row.status,
            DeliveryStatus::Pending | DeliveryStatus::Unconfigured
        ) {
            return Ok(false);
        }
        self.inner
            .store
            .upsert_delivery(EventDelivery {
                status: DeliveryStatus::Skipped,
                body: None,
                updated_at: self.now(),
                ..row
            })
            .await?;
        Ok(true)
    }

    /// Sends a failed (or waiting) delivery again, with a fresh one-day window.
    pub async fn retry_delivery(&self, id: &str) -> Result<DeliveryView, JournalError> {
        let now = self.now();
        let row = {
            let _state = self.inner.state_lock.lock().await;
            let Some(row) = self.inner.store.delivery(id).await? else {
                return Err(JournalError::NotFound(format!("No delivery {id}")));
            };
            if !matches!(
                row.status,
                DeliveryStatus::Failed | DeliveryStatus::Unconfigured
            ) {
                return Err(JournalError::Conflict(
                    "Only a failed or waiting delivery can be retried.".to_owned(),
                ));
            }
            let row = EventDelivery {
                status: DeliveryStatus::Pending,
                failure: None,
                next_attempt_at: now,
                requeued_at: Some(now),
                updated_at: now,
                ..row
            };
            self.inner.store.upsert_delivery(row.clone()).await?;
            row
        };
        tracing::info!(target: LOG, delivery = %id, "Delivery retried from the MCP gateway page");
        self.request_drain();
        Ok(delivery_view(&row, &HashMap::new()))
    }

    /// Targets, routes, the newest deliveries and counts (the `events` list
    /// is the caller's: it knows the integrations' events).
    pub async fn outbound(&self, only_failed: bool) -> Result<OutboundOverview, StoreError> {
        let now = self.now();
        let store = &self.inner.store;
        let mut targets = store.targets().await?;
        targets.sort_by(|a, b| a.id.cmp(&b.id));
        let mut routes = store.routes().await?;
        routes.sort_by_key(|r| r.created_at);
        let deliveries = store.deliveries().await?;
        let crypto = self.crypto();
        let hosts: HashMap<String, String> = store
            .subscriptions()
            .await?
            .into_iter()
            .filter_map(|row| {
                let host = callback_host(crypto.open(&row.encrypted_url).as_deref())?;
                Some((row.id, host))
            })
            .collect();
        let counts = DeliveryCounts {
            pending: count_u32(
                deliveries
                    .iter()
                    .filter(|r| r.status == DeliveryStatus::Pending),
            ),
            failed: count_u32(deliveries.iter().filter(|r| {
                r.status == DeliveryStatus::Failed && r.updated_at > now - DELIVERY_RETENTION_MS
            })),
            delivered: count_u32(deliveries.iter().filter(|r| {
                matches!(
                    r.status,
                    DeliveryStatus::Delivered | DeliveryStatus::Ignored
                ) && r.updated_at > now - 24 * 60 * MINUTE_MS
            })),
        };
        let mut shown: Vec<&EventDelivery> = deliveries
            .iter()
            .filter(|r| !only_failed || r.status == DeliveryStatus::Failed)
            .collect();
        shown.sort_by_key(|r| std::cmp::Reverse((r.created_at, r.id.clone())));
        shown.truncate(OVERVIEW_DELIVERIES);
        Ok(OutboundOverview {
            targets: targets.iter().map(target_view).collect(),
            routes: routes.iter().map(route_view).collect(),
            deliveries: shown
                .into_iter()
                .map(|r| delivery_view(r, &hosts))
                .collect(),
            counts,
            events: Vec::new(),
        })
    }
}

fn count_u32<'a>(rows: impl Iterator<Item = &'a EventDelivery>) -> u32 {
    u32::try_from(rows.count()).unwrap_or(u32::MAX)
}
