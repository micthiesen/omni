//! Stored event targets and routes, and the one sender every target
//! delivery uses ("Event journal" in `docs/mcp-events.md`).
//!
//! A target is a receiver on the LAN or the internet (a Rakazo bot webhook)
//! that takes a JSON body with `Authorization: Bearer <key>` and an
//! `Idempotency-Key`, which it uses to collapse repeated deliveries. That is
//! why an uncertain attempt (timeout, dropped connection) may be retried.
//! Targets are configured only by the owner (a trusted browser, or the boot
//! import of the old environment settings), so they may name private
//! addresses; callers of tools and the app only ever name a target id.
//!
//! A route sends every event of one name whose arguments match to one
//! target, as the event body MCP subscribers get. Routes replace the relay
//! that used to subscribe on a bot's behalf.

use std::time::Duration;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use omni_http::{HttpClient, HttpConfig, HttpError, Method, RedirectRule, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::Entity;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LOG: &str = "MCP:Events";
/// A webhook answers with a few bytes; anything longer is some other page.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const FIRST_BACKOFF_MS: i64 = 30_000;
pub const MAX_BACKOFF_MS: i64 = 30 * 60_000;
/// A delivery still failing this long after it was queued fails for good.
pub const GIVE_UP_AFTER_MS: i64 = 24 * 60 * 60_000;
/// Attempts kept on a delivery row.
pub const HISTORY_LIMIT: usize = 20;
/// Unconfigured deliveries younger than this are sent once their target
/// appears.
pub const UNCONFIGURED_WAIT_MS: i64 = 7 * 24 * 60 * 60_000;

/// `mcp-event-target`, keyed by `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventTarget {
    pub id: String,
    pub label: String,
    pub url: String,
    /// The bearer key, sealed with the event key ([`super::crypto::EventCrypto`]).
    pub encrypted_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for EventTarget {
    const NAME: &'static str = "mcp-event-target";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// `mcp-event-route`, keyed by `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRoute {
    pub id: String,
    pub name: String,
    pub arguments: IndexMap<String, String>,
    pub target: String,
    pub enabled: bool,
    pub created_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for EventRoute {
    const NAME: &'static str = "mcp-event-route";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// What a tool, the app or a route names: the rule `CLAUDE_NOTIFY_TARGETS`
/// names followed (lowercase letters, digits, `-` and `_`, at most 40,
/// starting with a letter or digit), so every old name imports.
pub fn valid_target_id(id: &str) -> bool {
    omni_config::is_notify_target_name(id)
}

/// An `http(s)` URL without credentials or fragment, as stored.
pub fn normalize_target_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

/// A connect override is a host name (no scheme, port or path).
pub fn valid_connect(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// One target, opened for a send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenTarget {
    pub id: String,
    pub url: url::Url,
    pub key: String,
    pub connect: Option<String>,
}

/// One answer from a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetResponse {
    pub status: u16,
    /// Rakazo's `{"ok": true, "ignored": true}`: no webhook routine ran.
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("timeout")]
    Timeout,
    #[error("network")]
    Network,
    /// An answer too long to be a webhook's (a proxy's error page); sending
    /// again would not help.
    #[error("unusable_answer")]
    Unusable,
}

/// The network leg; tests replace it.
pub trait TargetTransport: Send + Sync {
    /// `event` is the event or notice name, sent as `X-Omni-Event`.
    fn post<'a>(
        &'a self,
        target: &'a OpenTarget,
        event: &'a str,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<TargetResponse, TargetError>>;
}

/// The production transport: the LAN-capable client (targets are the
/// owner's configuration), no redirects, ten-second timeout. In
/// `SideEffectMode::Record` nothing is sent.
pub struct LiveTargetTransport {
    client: HttpClient,
    mode: SideEffectMode,
}

impl LiveTargetTransport {
    pub fn new(client: HttpClient, mode: SideEffectMode) -> Self {
        Self { client, mode }
    }
}

impl TargetTransport for LiveTargetTransport {
    fn post<'a>(
        &'a self,
        target: &'a OpenTarget,
        event: &'a str,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<TargetResponse, TargetError>> {
        Box::pin(async move {
            if self.mode == SideEffectMode::Record {
                tracing::info!(
                    target: LOG,
                    destination = %target.id,
                    "Recorded event target delivery (not sent)"
                );
                return Ok(TargetResponse {
                    status: 200,
                    ignored: false,
                });
            }
            let pinned;
            let client = match &target.connect {
                None => &self.client,
                Some(connect) => {
                    pinned = pinned_client(target, connect).await?;
                    &pinned
                }
            };
            let mut request = client
                .request(Method::POST, target.url.clone())
                .redirect(RedirectRule::None)
                .timeout(REQUEST_TIMEOUT)
                .header("content-type", "application/json")
                .header("idempotency-key", idempotency_key)
                .header("x-omni-event", event);
            // A target without a key is sent no Authorization (the old
            // recipe webhook allowed that).
            if !target.key.is_empty() {
                request = request.bearer_auth(&target.key);
            }
            // Fond's request notices carried this header before the journal.
            if event.starts_with("recipe.") {
                request = request.header("x-fond-event", event);
            }
            let response = request
                .body(body.to_owned())
                .send_bounded(MAX_RESPONSE_BYTES)
                .await
                .map_err(|error| match error {
                    HttpError::Timeout => TargetError::Timeout,
                    HttpError::TooLarge { .. } => TargetError::Unusable,
                    _ => TargetError::Network,
                })?;
            let ignored = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("ignored").and_then(Value::as_bool))
                .unwrap_or(false);
            Ok(TargetResponse {
                status: response.status.as_u16(),
                ignored,
            })
        })
    }
}

/// A client that reaches the target URL's host at `connect`, resolved now so
/// a recreated proxy container's new address is picked up.
async fn pinned_client(target: &OpenTarget, connect: &str) -> Result<HttpClient, TargetError> {
    let host = target.url.host_str().unwrap_or_default();
    let port = target.url.port_or_known_default().unwrap_or(443);
    let addr = tokio::net::lookup_host((connect, port))
        .await
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or_else(|| {
            tracing::info!(target: LOG, connect = %connect, "Event target connect host did not resolve");
            TargetError::Network
        })?;
    let config = HttpConfig {
        connect_timeout: Some(REQUEST_TIMEOUT),
        offline: false,
    };
    HttpClient::pinned(&config, host, addr).map_err(|_| TargetError::Network)
}

/// How one attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Delivered,
    Ignored,
    /// Worth another attempt: a timeout, network failure, 408, 425, 429 or 5xx.
    Retry(String),
    /// Refused for good (another 4xx, or a 3xx that is never followed).
    Rejected(String),
}

/// The decision over one answer, for targets and subscription webhooks alike.
pub fn classify_status(status: u16, ignored: bool) -> AttemptOutcome {
    match status {
        200..=299 if ignored => AttemptOutcome::Ignored,
        200..=299 => AttemptOutcome::Delivered,
        408 | 425 | 429 | 500.. => AttemptOutcome::Retry(format!("http_{status}")),
        _ => AttemptOutcome::Rejected(format!("http_{status}")),
    }
}

pub fn classify(result: &Result<TargetResponse, TargetError>) -> AttemptOutcome {
    match result {
        Err(TargetError::Unusable) => AttemptOutcome::Rejected(TargetError::Unusable.to_string()),
        Err(error) => AttemptOutcome::Retry(error.to_string()),
        Ok(response) => classify_status(response.status, response.ignored),
    }
}

/// Delay before attempt `attempts + 1`: 30 s doubling to 30 minutes.
pub fn backoff_ms(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    FIRST_BACKOFF_MS
        .saturating_mul(1_i64 << exponent)
        .min(MAX_BACKOFF_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_delivered_retried_or_refused() {
        assert_eq!(classify_status(200, false), AttemptOutcome::Delivered);
        assert_eq!(classify_status(202, true), AttemptOutcome::Ignored);
        for status in [408, 425, 429, 500, 503] {
            assert!(matches!(
                classify_status(status, false),
                AttemptOutcome::Retry(_)
            ));
        }
        for status in [301, 400, 401, 403, 404] {
            assert!(matches!(
                classify_status(status, false),
                AttemptOutcome::Rejected(_)
            ));
        }
        assert!(matches!(
            classify(&Err(TargetError::Timeout)),
            AttemptOutcome::Retry(_)
        ));
    }

    #[test]
    fn the_wait_doubles_to_thirty_minutes() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(6), 960_000);
        assert_eq!(backoff_ms(7), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(40), MAX_BACKOFF_MS);
    }

    #[test]
    fn ids_urls_and_connect_hosts_are_checked() {
        assert!(valid_target_id("chief"));
        assert!(valid_target_id("kitchen-2"));
        assert!(valid_target_id("my_bot"));
        assert!(valid_target_id("2chief"));
        assert!(!valid_target_id("Chief"));
        assert!(!valid_target_id("-chief"));
        assert!(!valid_target_id(""));
        assert_eq!(
            normalize_target_url("https://rakazo.syas.ca/api/v1/bots/x/webhook").as_deref(),
            Some("https://rakazo.syas.ca/api/v1/bots/x/webhook")
        );
        assert!(normalize_target_url("https://user:pw@host/x").is_none());
        assert!(normalize_target_url("ftp://host/x").is_none());
        assert!(normalize_target_url("https://host/x#frag").is_none());
        assert!(valid_connect("npm.compose_default"));
        assert!(!valid_connect("npm:443"));
        assert!(!valid_connect(""));
    }
}
