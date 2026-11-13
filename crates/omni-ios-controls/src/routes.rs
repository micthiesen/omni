//! The app's signed routes: control slots, diagnostics and registrations,
//! device and widget push registration, and the widget timelines.
//!
//! Every `/api/ios-controls/*` request carries
//! `Authorization: Omni-HMAC <hex>`, `X-Omni-Timestamp` (seconds, within
//! ±300 s) and a single-use UUID `X-Omni-Nonce`. The signature is
//! HMAC-SHA256 over `ts\nnonce\nMETHOD\n<raw percent-encoded path>\nsha256(body)`.
//! The nonce is recorded only after the signature verifies; bodies are capped
//! at 64 KiB before and while reading.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use hmac::{Hmac, KeyInit, Mac};
use omni_api::ios::{
    ApnsEnvironment, DeviceRegistrationResponse, IOS_CONTROL_SLOT_COUNT, IosDeviceKind,
    RegistrationResponse, WidgetPushResponse,
};
use omni_core::clock::SharedClock;
use omni_server_kit::api_error;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::devices::{DeviceInput, DeviceWriteError};
use crate::persistence::ControlInput;
use crate::service::IosControlService;

const LOG: &str = "IOSControlRoutes";
const AUTH_WINDOW_SECONDS: i64 = 300;
/// Signed-body cap.
pub const IOS_CONTROL_MAX_SIGNED_BODY_BYTES: usize = 64 * 1024;
const MAX_CONTROLS: usize = 32;
const PREFIX: &str = "/api/ios-controls";

#[allow(clippy::expect_used)]
static NONCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f-]{36}$").expect("valid regex"));
#[allow(clippy::expect_used)]
static AUTH_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^Omni-HMAC\s+").expect("valid regex"));
#[allow(clippy::expect_used)]
static PUSH_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{32,512}$").expect("valid regex"));

#[derive(Clone)]
pub struct IosRoutesState {
    pub service: Arc<IosControlService>,
    /// `IOS_CONTROL_AUTH_TOKEN`; without it every route answers 503.
    pub auth_token: Option<String>,
    pub clock: SharedClock,
    /// Used nonces and their expiry (epoch seconds).
    pub nonces: Arc<Mutex<HashMap<String, i64>>>,
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn unauthorized() -> Response {
    api_error(StatusCode::UNAUTHORIZED, "Unauthorized")
}

fn too_large() -> Response {
    api_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large")
}

/// `Number(value)` + `Number.isSafeInteger` for a declared Content-Length.
fn declared_length_ok(value: &str) -> bool {
    let length = omni_core::js::string_to_number(value);
    length.fract() == 0.0
        && length.abs() <= 9_007_199_254_740_991.0
        && length >= 0.0
        && length <= IOS_CONTROL_MAX_SIGNED_BODY_BYTES as f64
}

/// The canonical string the app signs.
pub fn canonical_request(
    timestamp: f64,
    nonce: &str,
    method: &str,
    path: &str,
    body_hash: &str,
) -> String {
    format!(
        "{}\n{nonce}\n{method}\n{path}\n{body_hash}",
        omni_core::js::number_to_string(timestamp)
    )
}

/// Hex HMAC-SHA256 of `canonical` under `token`.
pub fn sign(token: &str, canonical: &str) -> String {
    #[allow(clippy::expect_used)]
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

async fn authenticate(
    State(state): State<IosRoutesState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(token) = state.auth_token.as_deref() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "iOS controls are not configured",
        );
    };
    let headers = request.headers();
    if let Some(declared) = header_str(headers, header::CONTENT_LENGTH.as_str())
        && !declared_length_ok(declared)
    {
        return too_large();
    }
    let timestamp =
        header_str(headers, "x-omni-timestamp").map_or(f64::NAN, omni_core::js::string_to_number);
    let nonce = header_str(headers, "x-omni-nonce")
        .unwrap_or_default()
        .to_owned();
    let supplied = header_str(headers, header::AUTHORIZATION.as_str())
        .map(|value| AUTH_PREFIX.replace(value, "").into_owned())
        .unwrap_or_default();
    let now = state.clock.now_ms().div_euclid(1_000);
    {
        let mut nonces = state
            .nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        nonces.retain(|_, expires_at| *expires_at > now);
        #[allow(clippy::cast_precision_loss)]
        let skew = (now as f64 - timestamp).abs();
        if timestamp.fract() != 0.0
            || !timestamp.is_finite()
            || skew > AUTH_WINDOW_SECONDS as f64
            || !NONCE.is_match(&nonce)
            || nonces.contains_key(&nonce)
        {
            return unauthorized();
        }
    }

    let (parts, body) = request.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, IOS_CONTROL_MAX_SIGNED_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return too_large(),
    };
    let body_hash = hex::encode(Sha256::digest(&bytes));
    // The full request path as received (nesting strips the prefix from
    // `parts.uri`), still percent-encoded.
    let path = parts
        .extensions
        .get::<OriginalUri>()
        .map_or_else(|| parts.uri.path(), |original| original.0.path());
    let canonical = canonical_request(timestamp, &nonce, parts.method.as_str(), path, &body_hash);
    let expected = sign(token, &canonical);
    let equal = supplied.len() == expected.len()
        && bool::from(supplied.as_bytes().ct_eq(expected.as_bytes()));
    if !equal {
        return unauthorized();
    }
    {
        let mut nonces = state
            .nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A concurrent request may have used the same nonce meanwhile.
        if nonces.contains_key(&nonce) {
            return unauthorized();
        }
        nonces.insert(nonce, now + AUTH_WINDOW_SECONDS);
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn slot(State(state): State<IosRoutesState>, Path(slot): Path<String>) -> Response {
    match state
        .service
        .get_slot(omni_core::js::string_to_number(&slot))
        .await
    {
        Ok(Some(slot)) => no_store(axum::Json(slot).into_response()),
        Ok(None) => api_error(
            StatusCode::BAD_REQUEST,
            format!("Slot must be an integer from 1 to {IOS_CONTROL_SLOT_COUNT}"),
        ),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

async fn diagnostics(State(state): State<IosRoutesState>) -> Response {
    match state.service.diagnostics().await {
        Ok(diagnostics) => no_store(axum::Json(diagnostics).into_response()),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

/// A string without surrounding whitespace, within `min..=max` UTF-16 units.
fn bounded_string(value: Option<&Value>, min: usize, max: usize) -> Option<String> {
    let text = value?.as_str()?;
    let length = omni_core::js::utf16_len(text);
    (omni_live::streamers::js_trim(text) == text && (min..=max).contains(&length))
        .then(|| text.to_owned())
}

/// Decodes `{deviceId, controls: [{controlId, slot, pushToken, environment}]}`.
pub fn decode_registration(body: &[u8]) -> Option<(String, Vec<ControlInput>)> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let device_id = bounded_string(value.get("deviceId"), 8, 200)?;
    let controls = value.get("controls")?.as_array()?;
    if controls.len() > MAX_CONTROLS {
        return None;
    }
    let controls = controls
        .iter()
        .map(|control| {
            let control_id = bounded_string(control.get("controlId"), 1, 500)?;
            let slot = control.get("slot")?.as_f64()?;
            if slot.fract() != 0.0 || !(1.0..=f64::from(IOS_CONTROL_SLOT_COUNT)).contains(&slot) {
                return None;
            }
            let push_token = control.get("pushToken")?.as_str()?;
            if !PUSH_TOKEN.is_match(push_token) {
                return None;
            }
            let environment = environment(control.get("environment"))?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(ControlInput {
                control_id,
                slot: slot as u8,
                push_token: push_token.to_owned(),
                environment,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((device_id, controls))
}

async fn register(State(state): State<IosRoutesState>, body: Bytes) -> Response {
    let Some((device_id, controls)) = decode_registration(&body) else {
        return api_error(StatusCode::BAD_REQUEST, "Invalid control registration");
    };
    let count = controls.len();
    match state.service.register_device(&device_id, controls).await {
        Ok(()) => {
            tracing::info!(target: LOG, "Registered {count} control(s) for one device");
            axum::Json(RegistrationResponse {
                registered: count as u64,
            })
            .into_response()
        }
        Err(error) => {
            tracing::error!(target: LOG, "Failed to register iOS controls: {error}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save control registration",
            )
        }
    }
}

fn environment(value: Option<&Value>) -> Option<ApnsEnvironment> {
    match value?.as_str()? {
        "sandbox" => Some(ApnsEnvironment::Sandbox),
        "production" => Some(ApnsEnvironment::Production),
        _ => None,
    }
}

fn push_token(value: Option<&Value>) -> Option<String> {
    let token = value?.as_str()?;
    PUSH_TOKEN.is_match(token).then(|| token.to_owned())
}

/// Decodes `{deviceId, name, kind, apnsToken, environment, alertsEnabled,
/// appVersion}`. `apnsToken` and `appVersion` may be null or absent; the name
/// is trimmed.
pub fn decode_device(body: &[u8]) -> Option<DeviceInput> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let device_id = bounded_string(value.get("deviceId"), 8, 200)?;
    let name = omni_live::streamers::js_trim(value.get("name")?.as_str()?).to_owned();
    if !(1..=200).contains(&omni_core::js::utf16_len(&name)) {
        return None;
    }
    let kind = match value.get("kind")?.as_str()? {
        "iphone" => IosDeviceKind::Iphone,
        "ipad" => IosDeviceKind::Ipad,
        _ => return None,
    };
    let apns_token = match value.get("apnsToken") {
        None | Some(Value::Null) => None,
        token => Some(push_token(token)?),
    };
    let app_version = match value.get("appVersion") {
        None | Some(Value::Null) => None,
        version => Some(bounded_string(version, 1, 64)?),
    };
    Some(DeviceInput {
        device_id,
        name,
        kind,
        apns_token,
        environment: environment(value.get("environment"))?,
        alerts_enabled: value.get("alertsEnabled")?.as_bool()?,
        app_version,
    })
}

/// Decodes `{deviceId, pushToken, environment}`.
pub fn decode_widget_push(body: &[u8]) -> Option<(String, String, ApnsEnvironment)> {
    let value: Value = serde_json::from_slice(body).ok()?;
    Some((
        bounded_string(value.get("deviceId"), 8, 200)?,
        push_token(value.get("pushToken"))?,
        environment(value.get("environment"))?,
    ))
}

fn write_failure(error: &DeviceWriteError, what: &str) -> Response {
    match error {
        DeviceWriteError::TooMany => api_error(StatusCode::CONFLICT, error.to_string()),
        DeviceWriteError::Store(store) => {
            tracing::warn!(target: LOG, "Failed to save {what}: {store}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not save {what}"),
            )
        }
    }
}

async fn register_device(State(state): State<IosRoutesState>, body: Bytes) -> Response {
    let Some(input) = decode_device(&body) else {
        return api_error(StatusCode::BAD_REQUEST, "Invalid device registration");
    };
    match state.service.register_alert_device(input).await {
        Ok(device) => {
            tracing::info!(
                target: LOG,
                "Registered device {} (alerts {})",
                device.name,
                if device.receives_alerts() { "on" } else { "off" }
            );
            no_store(
                axum::Json(DeviceRegistrationResponse {
                    registered: true,
                    alerts_enabled: device.alerts_enabled,
                    apns_enabled: state.service.apns_enabled(),
                })
                .into_response(),
            )
        }
        Err(error) => write_failure(&error, "device registration"),
    }
}

async fn register_widget_push(State(state): State<IosRoutesState>, body: Bytes) -> Response {
    let Some((device_id, token, environment)) = decode_widget_push(&body) else {
        return api_error(StatusCode::BAD_REQUEST, "Invalid widget push registration");
    };
    match state
        .service
        .register_widget_push(&device_id, &token, environment)
        .await
    {
        Ok(_) => no_store(axum::Json(WidgetPushResponse { registered: true }).into_response()),
        Err(error) => write_failure(&error, "widget push registration"),
    }
}

async fn on_air(State(state): State<IosRoutesState>) -> Response {
    match state.service.on_air().await {
        Ok(on_air) => no_store(axum::Json(on_air).into_response()),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "Not Found")
}

/// The signed routes with state applied, nested at `/api/ios-controls`. The
/// authentication layer covers every path under the prefix, so an unknown path
/// or method is 401 until signed and 404/405 only afterwards.
pub fn router(state: IosRoutesState) -> Router {
    let signed = Router::new()
        .route("/slots/{slot}", get(slot))
        .route("/diagnostics", get(diagnostics))
        .route("/registrations", put(register))
        .route("/device", put(register_device))
        .route("/widget-push", put(register_widget_push))
        .route("/widgets/on-air", get(on_air))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state);
    Router::new().nest(PREFIX, signed)
}
