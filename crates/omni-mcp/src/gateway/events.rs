//! MCP Events at `/gateway/mcp`, inside Omni.
//!
//! `events/list` offers Omni's own events and every enabled integration's
//! pollable events (`<integration>__<event>`). Every subscription, to either
//! kind, lives in Omni's durable outbox, owned by the gateway principal, and
//! is delivered by Omni's webhook worker with its retries, signing and
//! delivery-time access check.
//!
//! Omni follows an integration event with a feed: one `events/poll`
//! subscription per event and argument set, whose cursor is stored after the
//! events it returned have been queued. Each upstream event is queued once
//! per feed (its receipt is keyed by feed and upstream event id), so a crash
//! between queueing and storing the cursor replays harmlessly, a restart or
//! an upstream outage resumes from the stored cursor, and nothing is lost
//! that the integration can still replay. When the integration reports that
//! it could not replay everything since the cursor (`truncated`), the feed
//! records a gap: those events are gone, and the integration's own polling
//! tools are the way to reconcile.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_api::gateway::GatewayErrorKind;
use omni_store::StoreError;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::access::{DownstreamAccess, Principal};
use super::catalog::CatalogEvent;
use super::error::GatewayError;
use super::model::FeedRow;
use super::service::Gateway;
use crate::events::catalog::{EventArguments, canonical_event_arguments, event_catalog};
use crate::events::dynamic::{DynamicEvents, OwnerCheck, string_arguments};
use crate::events::rpc::{service_error, subscribe_input, unsubscribe_input};
use crate::events::service::{
    EventServiceError, McpEventService, PublishInput, SubscriptionRejection,
};
use crate::events::webhook::{validate_callback_url, validate_signing_secret};
use crate::rpc::RpcError;

const LOG: &str = "MCP:Gateway";
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_POLL_MS: i64 = 15_000;
const DEFAULT_POLL_MS: i64 = 60_000;
const MAX_POLL_MS: i64 = 5 * 60_000;
const MAX_BACKOFF_MS: i64 = 15 * 60_000;
/// Pages read per feed per pass.
const MAX_PAGES: usize = 5;
const PAGE_EVENTS: i64 = 50;
/// A feed idle (no current subscriber) longer than this starts fresh when a
/// subscriber returns: what happened meanwhile is history, not news.
const IDLE_RESET_MS: i64 = 60 * 60_000;
/// Integration payloads larger than this are not queued.
const MAX_PAYLOAD_BYTES: usize = 32 * 1024;
/// Longer event ids are counted as gaps; longer timestamps are replaced.
const MAX_EVENT_ID_BYTES: usize = 256;
const MAX_TIMESTAMP_BYTES: usize = 64;
/// A feed nobody has subscribed to for this long is deleted.
const IDLE_PRUNE_MS: i64 = 7 * 24 * 60 * 60_000;
/// Gap warnings for one feed are logged at most this often.
const GAP_WARN_INTERVAL_MS: i64 = 60 * 60_000;

/// The integration events the outbox can carry, as served now.
pub struct GatewayEventCatalog {
    gateway: Gateway,
}

impl GatewayEventCatalog {
    pub fn new(gateway: Gateway) -> Self {
        Self { gateway }
    }
}

/// `(integration id, event)` for a served integration event name.
fn find_event(gateway: &Gateway, name: &str) -> Option<(String, CatalogEvent)> {
    let integration = super::names::integration_of(name)?;
    let runtime = gateway.runtime(integration)?;
    if !runtime.config().enabled {
        return None;
    }
    let event = runtime.catalog()?.event(name)?.clone();
    event.pollable.then(|| (integration.to_owned(), event))
}

/// Canonical arguments: an object of strings the event's schema accepts.
fn parse_event_arguments(event: &CatalogEvent, raw: &Value) -> Option<EventArguments> {
    let args = string_arguments(raw)?;
    let schema = event.input_schema();
    let validator = jsonschema::options().build(&schema).ok()?;
    let typed: Map<String, Value> = args
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    validator.is_valid(&Value::Object(typed)).then_some(args)
}

impl DynamicEvents for GatewayEventCatalog {
    fn knows(&self, name: &str) -> bool {
        find_event(&self.gateway, name).is_some()
    }

    fn parse_arguments(&self, name: &str, raw: &Value) -> Option<EventArguments> {
        let (_, event) = find_event(&self.gateway, name)?;
        parse_event_arguments(&event, raw)
    }
}

/// Delivery-time access check for gateway owners.
pub struct GatewayOwners {
    access: DownstreamAccess,
}

impl GatewayOwners {
    pub fn new(access: DownstreamAccess) -> Self {
        Self { access }
    }
}

impl OwnerCheck for GatewayOwners {
    fn is_active<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(self.access.is_active(owner))
    }
}

/// The served `events/list` entries.
pub fn served_events(gateway: &Gateway) -> Vec<Value> {
    let mut events = match event_catalog() {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    for runtime in gateway.runtimes() {
        let config = runtime.config();
        if !config.enabled {
            continue;
        }
        if let Some(catalog) = runtime.catalog() {
            events.extend(
                catalog
                    .events
                    .iter()
                    .filter(|e| e.pollable)
                    .map(|e| e.served(&config.name)),
            );
        }
    }
    events
}

fn feed_id(integration: &str, upstream: &str, args: &EventArguments) -> String {
    let digest = omni_core::digest::sha256_hex(format!(
        "{integration}\u{0}{upstream}\u{0}{}",
        canonical_event_arguments(args)
    ));
    format!("feed_{}", &digest[..32])
}

fn rpc_failure(error: &GatewayError) -> RpcError {
    RpcError {
        code: -32603,
        message: error.describe(),
        data: None,
    }
}

/// One `events/*` request from a gateway principal.
pub async fn handle(
    gateway: &Gateway,
    events: &McpEventService,
    method: &str,
    params: &Map<String, Value>,
    principal: &Principal,
) -> Result<Value, RpcError> {
    let owner = principal.owner();
    match method {
        "events/list" => {
            events.record_discovery_as(&owner).await;
            Ok(json!({"events": served_events(gateway)}))
        }
        "events/subscribe" => {
            let input = subscribe_input(params)?;
            if let Some((integration, event)) = find_event(gateway, &input.name) {
                // Refuse what the outbox would refuse before any upstream call.
                if validate_callback_url(&input.url).is_none()
                    || validate_signing_secret(&input.secret).is_none()
                {
                    return Err(service_error(EventServiceError::Rejected(
                        if validate_callback_url(&input.url).is_none() {
                            SubscriptionRejection::InvalidCallback
                        } else {
                            SubscriptionRejection::InvalidSecret
                        },
                    )));
                }
                let Some(args) = parse_event_arguments(&event, &input.arguments) else {
                    return Err(RpcError::invalid_params(format!(
                        "Invalid arguments for {}",
                        input.name
                    )));
                };
                let (feed, created) = ensure_feed(gateway, &integration, &event, &args)
                    .await
                    .map_err(|error| rpc_failure(&error))?;
                let result = events.subscribe_as(&owner, &input).await;
                if result.is_err() && created {
                    let id = feed.id;
                    let _ = gateway
                        .store()
                        .write(move |tx| tx.delete::<FeedRow>(&id).map(|_| ()))
                        .await;
                }
                let result = result.map_err(service_error)?;
                return serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()));
            }
            let result = events
                .subscribe_as(&owner, &input)
                .await
                .map_err(service_error)?;
            serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
        }
        "events/unsubscribe" => {
            let input = unsubscribe_input(params)?;
            events
                .unsubscribe_as(&owner, &input)
                .await
                .map_err(service_error)?;
            Ok(json!({}))
        }
        _ => Err(RpcError::method_not_found()),
    }
}

/// Creates the feed for an event and argument set if needed, and whether it
/// was created. A new feed asks the integration for its current cursor
/// first, so a subscription the integration would refuse (an artifact the
/// owner cannot open) fails here and the feed starts at "now".
pub async fn ensure_feed(
    gateway: &Gateway,
    integration: &str,
    event: &CatalogEvent,
    args: &EventArguments,
) -> Result<(FeedRow, bool), GatewayError> {
    let id = feed_id(integration, &event.upstream, args);
    let key = id.clone();
    let existing = gateway
        .store()
        .read(move |docs| docs.get::<FeedRow>(&key))
        .await?;
    if let Some(feed) = existing {
        return Ok((feed, false));
    }
    let cursor = start_cursor(gateway, integration, &event.upstream, args).await?;
    let now = gateway.clock().now_ms();
    let feed = FeedRow {
        id,
        integration: integration.to_owned(),
        upstream_event: event.upstream.clone(),
        event: event.exposed.clone(),
        arguments: args.clone(),
        cursor: Some(cursor),
        created_at: now,
        last_poll_at: Some(now),
        next_poll_at: now + MIN_POLL_MS,
        failures: 0,
        error: None,
        last_event_at: None,
        published: 0,
        gaps: 0,
        last_gap_at: None,
        idle_since: None,
        extra: Extra::default(),
    };
    let stored = feed.clone();
    gateway
        .store()
        .write(move |tx| tx.upsert(&stored, UpsertOpts::default()))
        .await?;
    Ok((feed, true))
}

fn poll_params(upstream: &str, args: &EventArguments, cursor: Option<&str>) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("name".into(), json!(upstream));
    params.insert(
        "arguments".into(),
        Value::Object(
            args.iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ),
    );
    params.insert("cursor".into(), cursor.map_or(Value::Null, |c| json!(c)));
    params.insert("maxEvents".into(), json!(PAGE_EVENTS));
    params
}

async fn poll_once(
    gateway: &Gateway,
    integration: &str,
    params: Map<String, Value>,
) -> Result<Map<String, Value>, GatewayError> {
    let runtime = gateway
        .runtime(integration)
        .ok_or_else(|| GatewayError::config("The integration was removed."))?;
    let cancel = CancellationToken::new();
    match runtime
        .rpc(
            gateway.deps(),
            "events/poll",
            params,
            POLL_TIMEOUT,
            &cancel,
            true,
        )
        .await?
    {
        Ok(Value::Object(result)) => Ok(result),
        Ok(_) => Err(GatewayError::new(
            GatewayErrorKind::Protocol,
            "events/poll returned no result object",
        )),
        Err(error) => Err(GatewayError::new(
            GatewayErrorKind::Upstream,
            format!("events/poll failed: {} ({})", error.message, error.code),
        )),
    }
}

async fn start_cursor(
    gateway: &Gateway,
    integration: &str,
    upstream: &str,
    args: &EventArguments,
) -> Result<String, GatewayError> {
    let result = poll_once(gateway, integration, poll_params(upstream, args, None)).await?;
    result
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            GatewayError::new(
                GatewayErrorKind::Protocol,
                "events/poll returned no starting cursor",
            )
        })
}

/// Polls every due feed once (the scheduled task); the number polled.
pub async fn poll_feeds(gateway: &Gateway, events: &McpEventService) -> Result<usize, StoreError> {
    let feeds = gateway
        .store()
        .read(|docs| docs.get_all::<FeedRow>())
        .await?;
    let now = gateway.clock().now_ms();
    let mut active_by_event: BTreeMap<String, Vec<EventArguments>> = BTreeMap::new();
    let mut polled = 0;
    for mut feed in feeds {
        if !active_by_event.contains_key(&feed.event) {
            let active = events.active_arguments(&feed.event).await?;
            active_by_event.insert(feed.event.clone(), active);
        }
        let subscribed = active_by_event
            .get(&feed.event)
            .is_some_and(|active| active.contains(&feed.arguments));
        if !subscribed {
            match feed.idle_since {
                None => {
                    feed.idle_since = Some(now);
                    save(gateway, feed).await?;
                }
                Some(since) if now - since > IDLE_PRUNE_MS => {
                    let id = feed.id.clone();
                    gateway
                        .store()
                        .write(move |tx| tx.delete::<FeedRow>(&id).map(|_| ()))
                        .await?;
                }
                Some(_) => {}
            }
            continue;
        }
        if let Some(since) = feed.idle_since.take() {
            if now - since > IDLE_RESET_MS {
                feed.cursor = None;
            }
            if feed.cursor.is_some() {
                feed.next_poll_at = now;
            }
        }
        if feed.next_poll_at > now {
            continue;
        }
        let Some(runtime) = gateway.runtime(&feed.integration) else {
            continue;
        };
        let config = runtime.config();
        if !config.enabled || runtime.removed() {
            continue;
        }
        if find_event(gateway, &feed.event).is_none() {
            // The integration stopped offering the event (or a refresh is
            // partway): keep the cursor and say so rather than skip events.
            feed.error = Some(
                GatewayError::new(
                    GatewayErrorKind::Upstream,
                    format!(
                        "{} does not offer {} now.",
                        config.name, feed.upstream_event
                    ),
                )
                .to_row(now),
            );
            feed.next_poll_at = now + MAX_POLL_MS;
            save(gateway, feed).await?;
            continue;
        }
        polled += 1;
        poll_feed(gateway, events, feed, &config.name).await?;
    }
    Ok(polled)
}

/// Stores a feed unless it changed underneath this poll: deleted (the
/// integration was removed or pruned) or restarted against a new server (a
/// different `created_at`).
async fn save(gateway: &Gateway, feed: FeedRow) -> Result<(), StoreError> {
    gateway
        .store()
        .write(move |tx| {
            if tx
                .get::<FeedRow>(&feed.id)?
                .is_some_and(|stored| stored.created_at == feed.created_at)
            {
                tx.upsert(&feed, UpsertOpts::default())?;
            }
            Ok(())
        })
        .await
}

fn backoff(failures: i64) -> i64 {
    let exponent = u32::try_from(failures.clamp(0, 10)).unwrap_or(10);
    (30_000_i64.saturating_mul(2_i64.saturating_pow(exponent))).min(MAX_BACKOFF_MS)
}

/// Reads one feed forward from its cursor, queueing each event before the
/// cursor moves past it.
async fn poll_feed(
    gateway: &Gateway,
    events: &McpEventService,
    mut feed: FeedRow,
    integration_name: &str,
) -> Result<(), StoreError> {
    let now = gateway.clock().now_ms();
    feed.last_poll_at = Some(now);
    if feed.cursor.is_none() {
        match start_cursor(
            gateway,
            &feed.integration,
            &feed.upstream_event,
            &feed.arguments,
        )
        .await
        {
            Ok(cursor) => {
                feed.cursor = Some(cursor);
                feed.error = None;
                feed.failures = 0;
                feed.next_poll_at = now + MIN_POLL_MS;
            }
            Err(error) => {
                feed.failures += 1;
                feed.error = Some(error.to_row(now));
                feed.next_poll_at = now + backoff(feed.failures);
            }
        }
        return save(gateway, feed).await;
    }
    let mut next_poll_ms = DEFAULT_POLL_MS;
    let mut more = false;
    for _ in 0..MAX_PAGES {
        let params = poll_params(
            &feed.upstream_event,
            &feed.arguments,
            feed.cursor.as_deref(),
        );
        let result = match poll_once(gateway, &feed.integration, params).await {
            Ok(result) => result,
            Err(error) => {
                feed.failures += 1;
                if feed.failures == 1 || feed.failures % 10 == 0 {
                    tracing::warn!(target: LOG, integration = %feed.integration, event = %feed.upstream_event, error = %error, "Gateway event poll failed");
                }
                feed.error = Some(error.to_row(now));
                feed.next_poll_at = now + backoff(feed.failures);
                return save(gateway, feed).await;
            }
        };
        if result.get("truncated").and_then(Value::as_bool) == Some(true) {
            record_gap(
                &mut feed,
                now,
                &format!(
                    "{integration_name} could not replay every event since Omni's cursor; some were missed"
                ),
            );
        }
        let page = result
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let page_cursor = feed.cursor.clone();
        for event in &page {
            let Some(event_id) = event
                .get("eventId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_EVENT_ID_BYTES)
            else {
                record_gap(
                    &mut feed,
                    now,
                    "An integration event without a usable eventId was not delivered",
                );
                continue;
            };
            let Some(data) = event
                .get("data")
                .map_or_else(|| Some(Map::new()), |data| data.as_object().cloned())
            else {
                record_gap(
                    &mut feed,
                    now,
                    "An integration event whose data is not an object was not delivered",
                );
                continue;
            };
            if serde_json::to_vec(&data).map_or(0, |b| b.len()) > MAX_PAYLOAD_BYTES {
                record_gap(
                    &mut feed,
                    now,
                    "An integration event over 32 KiB was not delivered",
                );
                continue;
            }
            let timestamp = event
                .get("timestamp")
                .and_then(Value::as_str)
                .filter(|t| t.len() <= MAX_TIMESTAMP_BYTES)
                .map_or_else(|| omni_core::js::to_iso_string(now), str::to_owned);
            let key = format!("{}:{event_id}", feed.id);
            let queued = events
                .publish_targeted(
                    PublishInput {
                        name: feed.event.clone(),
                        receipt_key: omni_core::digest::sha256_hex(format!("gateway:{key}")),
                        event_key: format!("gateway:{key}"),
                        timestamp,
                        data,
                    },
                    &feed.arguments,
                )
                .await?;
            match queued {
                Some(true) => {
                    feed.published += 1;
                    feed.last_event_at = Some(now);
                }
                Some(false) => {}
                None => {
                    // The event stopped being offered mid-pass (a refresh, the
                    // integration disabled): keep the page's cursor. Events
                    // already queued from it are deduplicated on the replay.
                    feed.cursor = page_cursor;
                    feed.error = Some(
                        GatewayError::new(
                            GatewayErrorKind::Upstream,
                            format!(
                                "{integration_name} does not offer {} now.",
                                feed.upstream_event
                            ),
                        )
                        .to_row(now),
                    );
                    feed.next_poll_at = now + MAX_POLL_MS;
                    return save(gateway, feed).await;
                }
            }
        }
        if let Some(cursor) = result.get("cursor").and_then(Value::as_str) {
            feed.cursor = Some(cursor.to_owned());
        }
        if let Some(ms) = result.get("nextPollMs").and_then(Value::as_i64) {
            next_poll_ms = ms;
        }
        feed.failures = 0;
        feed.error = None;
        // The cursor is stored only after this page's events are queued.
        save(gateway, feed.clone()).await?;
        more = result.get("hasMore").and_then(Value::as_bool) == Some(true);
        if !more {
            break;
        }
    }
    // Pages left over are read on the next pass, not after a full interval.
    let wait = if more {
        MIN_POLL_MS
    } else {
        next_poll_ms.clamp(MIN_POLL_MS, MAX_POLL_MS)
    };
    feed.next_poll_at = now + wait;
    save(gateway, feed).await
}

/// Counts a gap on the feed; the warning repeats at most hourly per feed.
fn record_gap(feed: &mut FeedRow, now: i64, message: &str) {
    let quiet = feed
        .last_gap_at
        .is_some_and(|at| now - at < GAP_WARN_INTERVAL_MS);
    feed.gaps += 1;
    feed.last_gap_at = Some(now);
    if !quiet {
        tracing::warn!(target: LOG, integration = %feed.integration, event = %feed.upstream_event, "{message}");
    }
}

/// Registers the gateway's events and owners with the outbox.
pub fn extend_events(events: &McpEventService, gateway: &Gateway, access: &DownstreamAccess) {
    events.extend(crate::events::dynamic::EventExtensions {
        dynamic: Arc::new(GatewayEventCatalog::new(gateway.clone())),
        owners: Arc::new(GatewayOwners::new(access.clone())),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(schema: Value) -> CatalogEvent {
        CatalogEvent {
            upstream: "comment.created".into(),
            exposed: "artifacts__comment.created".into(),
            definition: json!({"name": "comment.created", "delivery": ["poll"], "inputSchema": schema}),
            pollable: true,
        }
    }

    #[test]
    fn arguments_are_canonical_strings_the_schema_accepts() {
        let e = event(
            json!({"type": "object", "properties": {"artifact": {"type": "string"}, "component": {"enum": ["form", "poll"]}}, "additionalProperties": false}),
        );
        let args =
            parse_event_arguments(&e, &json!({"component": "form", "artifact": "abc"})).unwrap();
        assert_eq!(args.keys().collect::<Vec<_>>(), ["artifact", "component"]);
        assert!(parse_event_arguments(&e, &json!({"component": "vote"})).is_none());
        assert!(parse_event_arguments(&e, &json!({"other": "x"})).is_none());
        assert!(parse_event_arguments(&e, &json!({"artifact": 3})).is_none());
        assert_eq!(parse_event_arguments(&e, &Value::Null).unwrap().len(), 0);
    }

    #[test]
    fn feed_ids_depend_on_integration_event_and_arguments() {
        let a = indexmap::IndexMap::from([("artifact".to_owned(), "x".to_owned())]);
        let b = indexmap::IndexMap::from([("artifact".to_owned(), "y".to_owned())]);
        assert_eq!(feed_id("artifacts", "e", &a), feed_id("artifacts", "e", &a));
        assert_ne!(feed_id("artifacts", "e", &a), feed_id("artifacts", "e", &b));
        assert_ne!(
            feed_id("artifacts", "e", &a),
            feed_id("artifacts_support", "e", &a)
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), 60_000);
        assert_eq!(backoff(20), MAX_BACKOFF_MS);
    }
}
