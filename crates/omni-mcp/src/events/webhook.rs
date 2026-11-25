//! Standard Webhooks delivery and callback verification.
//!
//! Callbacks must be bounded HTTPS URLs without credentials or fragments whose
//! DNS answers are all public (checked again at connection time by the public
//! HTTP client). Redirects and automatic retries are disabled, requests time
//! out after ten seconds, responses are capped at 4 KiB and bodies at 256 KiB.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures::future::BoxFuture;
use hmac::{Hmac, KeyInit as _, Mac as _};
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method, RedirectRule, SideEffectMode};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;
use url::Url;

const LOG: &str = "MCP:Events";
const MAX_BODY_BYTES: usize = 262_144;
const MAX_RESPONSE_BYTES: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a webhook call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookFailure {
    InvalidCallback,
    InvalidSecret,
    ChallengeFailed,
    Timeout,
    DeliveryFailed,
}

impl WebhookFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookFailure::InvalidCallback => "invalid_callback",
            WebhookFailure::InvalidSecret => "invalid_secret",
            WebhookFailure::ChallengeFailed => "challenge_failed",
            WebhookFailure::Timeout => "timeout",
            WebhookFailure::DeliveryFailed => "delivery_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Event webhook {}", .reason.as_str())]
pub struct WebhookError {
    pub reason: WebhookFailure,
}

impl WebhookError {
    pub fn new(reason: WebhookFailure) -> Self {
        Self { reason }
    }
}

/// A public-syntax HTTPS URL without a fragment, at most
/// 2048 UTF-16 units. Returns the parsed URL (its `as_str` is JS `href`).
pub fn validate_callback_url(value: &str) -> Option<Url> {
    let url = omni_http::public::assert_public_http_url_syntax(value).ok()?;
    let fragment = url.fragment().is_some_and(|f| !f.is_empty());
    (url.scheme() == "https" && !fragment && omni_core::js::utf16_len(value) <= 2048).then_some(url)
}

/// `whsec_` plus canonical base64 of 24-64 bytes.
pub fn validate_signing_secret(value: &str) -> Option<Vec<u8>> {
    let encoded = value.strip_prefix("whsec_")?;
    let body = encoded.trim_end_matches('=');
    let padding = encoded.len() - body.len();
    if body.is_empty()
        || padding > 2
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    // A trailing partial group is ignored when decoding; the canonical
    // re-encoding check below rejects such input.
    let usable = if body.len() % 4 == 1 {
        body.len() - 1
    } else {
        body.len()
    };
    let key = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireNone)
            .with_decode_allow_trailing_bits(true),
    )
    .decode(&body[..usable])
    .ok()?;
    let reencoded = STANDARD.encode(&key);
    if !(24..=64).contains(&key.len()) || reencoded.trim_end_matches('=') != body {
        return None;
    }
    Some(key)
}

/// Where one subscription's events are sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookDestination {
    pub id: String,
    pub url: String,
    pub secret: String,
    pub previous_secret: Option<String>,
}

/// The delivered event body.
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookEvent {
    pub event_id: String,
    pub name: String,
    pub timestamp: String,
    pub data: Map<String, Value>,
}

impl WebhookEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "eventId": self.event_id,
            "name": self.name,
            "timestamp": self.timestamp,
            "data": self.data,
            "cursor": null,
        })
    }
}

fn signature(secret: &str, id: &str, seconds: i64, body: &str) -> Option<String> {
    let key = validate_signing_secret(secret)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).ok()?;
    mac.update(format!("{id}.{seconds}.{body}").as_bytes());
    Some(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

/// Standard Webhooks signature over `id.seconds.body`,
/// signing with both keys during a secret rotation.
pub fn webhook_headers(
    destination: &WebhookDestination,
    id: &str,
    seconds: i64,
    body: &str,
) -> Option<Vec<(&'static str, String)>> {
    let mut signatures = vec![signature(&destination.secret, id, seconds, body)?];
    if let Some(previous) = &destination.previous_secret {
        signatures.push(signature(previous, id, seconds, body)?);
    }
    Some(vec![
        ("content-type", "application/json".to_owned()),
        ("user-agent", omni_http::USER_AGENT.to_owned()),
        ("webhook-id", id.to_owned()),
        ("webhook-timestamp", seconds.to_string()),
        ("webhook-signature", signatures.join(" ")),
        ("X-MCP-Subscription-Id", destination.id.clone()),
    ])
}

/// One bounded HTTP exchange with a callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookResponse {
    pub status: u16,
    pub body: String,
}

/// The network leg (`WebhookRequest`); tests replace it.
pub trait WebhookTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: String,
        headers: Vec<(&'static str, String)>,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>>;
}

/// The production transport: the public HTTP client (every DNS answer and
/// address validated), no redirects, ten-second timeout, 4 KiB responses.
/// In `SideEffectMode::Record` nothing is sent: verification challenges are
/// echoed and events answered `204`.
pub struct LiveWebhookTransport {
    client: PublicHttpClient,
    mode: SideEffectMode,
}

impl LiveWebhookTransport {
    pub fn new(client: PublicHttpClient, mode: SideEffectMode) -> Self {
        Self { client, mode }
    }
}

fn recorded_response(body: &str) -> WebhookResponse {
    let challenge = serde_json::from_str::<Value>(body)
        .ok()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("verification"))
        .and_then(|value| value.get("challenge").cloned());
    match challenge {
        Some(challenge) => WebhookResponse {
            status: 200,
            body: json!({ "challenge": challenge }).to_string(),
        },
        None => WebhookResponse {
            status: 204,
            body: String::new(),
        },
    }
}

impl WebhookTransport for LiveWebhookTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: String,
        headers: Vec<(&'static str, String)>,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>> {
        Box::pin(async move {
            let parsed = validate_callback_url(url)
                .ok_or(WebhookError::new(WebhookFailure::DeliveryFailed))?;
            if self.mode == SideEffectMode::Record {
                tracing::info!(target: LOG, host = parsed.host_str().unwrap_or(""), "Recorded MCP event webhook (not sent)");
                return Ok(recorded_response(&body));
            }
            let mut request = self
                .client
                .request(Method::POST, parsed)
                .redirect(RedirectRule::None)
                .timeout(REQUEST_TIMEOUT);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            match request.body(body).send_bounded(MAX_RESPONSE_BYTES).await {
                Ok(response) => Ok(WebhookResponse {
                    status: response.status.as_u16(),
                    body: String::from_utf8_lossy(&response.body).into_owned(),
                }),
                Err(HttpError::Timeout) => Err(WebhookError::new(WebhookFailure::Timeout)),
                Err(_) => Err(WebhookError::new(WebhookFailure::DeliveryFailed)),
            }
        })
    }
}

/// What the events service needs from webhooks (`WebhookPort`).
pub trait WebhookPort: Send + Sync {
    /// Sends a fresh verification challenge and requires its exact echo.
    fn verify<'a>(
        &'a self,
        destination: &'a WebhookDestination,
    ) -> BoxFuture<'a, Result<(), WebhookError>>;
    /// Delivers one event; returns the callback's HTTP status.
    fn deliver<'a>(
        &'a self,
        destination: &'a WebhookDestination,
        event: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>>;
}

#[derive(Clone)]
pub struct WebhookClient {
    transport: Arc<dyn WebhookTransport>,
    clock: SharedClock,
}

impl WebhookClient {
    pub fn new(transport: Arc<dyn WebhookTransport>, clock: SharedClock) -> Self {
        Self { transport, clock }
    }

    async fn post(
        &self,
        destination: &WebhookDestination,
        id: &str,
        value: &Value,
    ) -> Result<WebhookResponse, WebhookError> {
        validate_callback_url(&destination.url)
            .ok_or(WebhookError::new(WebhookFailure::InvalidCallback))?;
        let seconds = self.clock.now_ms().div_euclid(1000);
        let body = omni_core::js::json_stringify(value);
        if body.len() > MAX_BODY_BYTES {
            return Err(WebhookError::new(WebhookFailure::DeliveryFailed));
        }
        let headers = webhook_headers(destination, id, seconds, &body)
            .ok_or(WebhookError::new(WebhookFailure::InvalidSecret))?;
        self.transport.post(&destination.url, body, headers).await
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    bytes
}

impl WebhookPort for WebhookClient {
    fn verify<'a>(
        &'a self,
        destination: &'a WebhookDestination,
    ) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async move {
            let challenge = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
            let id = format!("msg_verification_{}", hex::encode(random_bytes::<16>()));
            let response = self
                .post(
                    destination,
                    &id,
                    &json!({"type": "verification", "challenge": challenge}),
                )
                .await?;
            let parsed: Value = serde_json::from_str(&response.body)
                .map_err(|_| WebhookError::new(WebhookFailure::ChallengeFailed))?;
            let echo = parsed.get("challenge").and_then(Value::as_str);
            let valid = (200..300).contains(&response.status)
                && echo.is_some_and(|echo| {
                    echo.len() == challenge.len()
                        && bool::from(echo.as_bytes().ct_eq(challenge.as_bytes()))
                });
            if valid {
                Ok(())
            } else {
                Err(WebhookError::new(WebhookFailure::ChallengeFailed))
            }
        })
    }

    fn deliver<'a>(
        &'a self,
        destination: &'a WebhookDestination,
        event: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        Box::pin(async move {
            self.post(destination, &event.event_id, &event.to_json())
                .await
                .map(|response| response.status)
        })
    }
}
