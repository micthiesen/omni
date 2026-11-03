//! Apple authentication and bounded CloudKit transport.
//!
//! Adapted from ticaki/ioBroker.icloud v2.1.2 (MIT, Copyright (c) 2026 ticaki
//! <github@renopoint.de>; notice in docs/licenses/ioBroker.icloud-MIT.txt). The
//! adaptation removes unredacted diagnostics, optimistic write success, and the
//! recommendation to disable ADP. It never accepts Apple terms, never changes ADP,
//! and never signs in or requests device consent without an explicit caller.

pub mod srp;
pub mod transport;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_http::{Method, SideEffectMode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cookies::CookieJar;
use crate::json::{as_record, field, integer, present};
use crate::protected_access::{
    PcsEndpoint, PcsError, PcsOperation, PcsRequester, PcsResponse, ProtectedAccess,
    request_protected_access,
};
use srp::{GsaSrpAuthenticator, ServerChallenge, SrpProtocol};
pub use transport::{
    AppleRequest, AppleResponse, AppleTransport, HttpAppleTransport, MAX_RESPONSE_BYTES,
    TransportError,
};

const LOG: &str = "Reminders";
const APPLE_WIDGET_KEY: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const AUTH_ROOT: &str = "https://idmsa.apple.com/appleauth/auth/";
const SETUP_ROOT: &str = "https://setup.icloud.com/setup/ws/1/";
const CLOUDKIT_SUFFIX: &str = "/database/1/com.apple.reminders/production/private";
// Apple-only exception approved by Michael: upstream documents 404/503 failures
// with incompatible User-Agents. Keep these protocol headers scoped to this client.
const AUTH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SERVICE_USER_AGENT: &str = "python-requests/2.31.0";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// The public failure category of an Apple exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppleErrorKind {
    AuthenticationNeeded,
    TransientOutage,
    RateLimited,
    UnsupportedProtocol,
    AwaitingDeviceApproval,
    TermsRequired,
}

/// A bounded Apple failure: operation label, fixed reason, optional HTTP status.
/// It never contains Apple response bodies, cookies, tokens or account identifiers.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Apple {operation}: {reason}")]
pub struct AppleRemindersError {
    pub operation: String,
    pub reason: String,
    pub status: Option<u16>,
    pub kind: AppleErrorKind,
}

impl AppleRemindersError {
    pub fn new(operation: &str, reason: &str, status: Option<u16>, kind: AppleErrorKind) -> Self {
        Self {
            operation: operation.to_owned(),
            reason: reason.to_owned(),
            status,
            kind,
        }
    }
}

fn fail(operation: &str, reason: &str, status: Option<u16>) -> AppleRemindersError {
    let kind = match status {
        Some(429 | 503) => AppleErrorKind::RateLimited,
        Some(500 | 502 | 504) => AppleErrorKind::TransientOutage,
        Some(451) => AppleErrorKind::TermsRequired,
        _ if reason.contains("consent") => AppleErrorKind::AwaitingDeviceApproval,
        Some(s) if s != 401 && s != 403 && s != 409 => AppleErrorKind::UnsupportedProtocol,
        _ => AppleErrorKind::AuthenticationNeeded,
    };
    AppleRemindersError::new(operation, reason, status, kind)
}

impl AppleSession {
    /// Lets every log sink scrub the session's tokens.
    fn register_for_scrub(&self) {
        for value in [
            &self.scnt,
            &self.session_id,
            &self.session_token,
            &self.trust_token,
        ]
        .into_iter()
        .flatten()
        {
            omni_core::secrets::register_runtime(value);
        }
    }
}

/// The persisted Apple session (`AppleSession`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppleSession {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scnt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_attributes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ck_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsid: Option<String>,
}

/// Accessor for one captured session header.
type SessionSlot = fn(&mut AppleSession) -> &mut Option<String>;

/// Where the session lives (the service's encrypted store).
pub trait SessionStorage: Send + Sync {
    fn load(&self) -> BoxFuture<'_, Result<Value, ()>>;
    fn save(&self, session: Value) -> BoxFuture<'_, Result<(), ()>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppleBeginResult {
    Ready,
    MfaRequired,
}

/// The CloudKit endpoints this client may call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CkPath {
    ChangesZone,
    RecordsQuery,
    RecordsLookup,
    RecordsModify,
}

impl CkPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChangesZone => "/changes/zone",
            Self::RecordsQuery => "/records/query",
            Self::RecordsLookup => "/records/lookup",
            Self::RecordsModify => "/records/modify",
        }
    }

    pub fn parse(path: &str) -> Option<Self> {
        [
            Self::ChangesZone,
            Self::RecordsQuery,
            Self::RecordsLookup,
            Self::RecordsModify,
        ]
        .into_iter()
        .find(|p| p.as_str() == path)
    }
}

/// The operations the service needs (a test seam).
pub trait AppleApi: Send + Sync {
    fn begin(&self) -> BoxFuture<'_, Result<AppleBeginResult, AppleRemindersError>>;
    fn verify(&self) -> BoxFuture<'_, Result<bool, AppleRemindersError>>;
    fn submit_2fa<'a>(&'a self, code: &'a str) -> BoxFuture<'a, Result<(), AppleRemindersError>>;
    fn request_pcs_access(&self) -> BoxFuture<'_, Result<ProtectedAccess, AppleRemindersError>>;
    fn ck_post(
        &self,
        path: CkPath,
        body: Value,
    ) -> BoxFuture<'_, Result<Value, AppleRemindersError>>;
}

/// Construction options.
pub struct AppleClientOptions {
    pub account: String,
    pub password: String,
    pub storage: Arc<dyn SessionStorage>,
    pub transport: Arc<dyn AppleTransport>,
    pub clock: SharedClock,
    /// Overrides both the 15 s default and the 60 s query timeout (`deps.timeoutMs`).
    pub timeout: Option<Duration>,
    pub side_effects: SideEffectMode,
}

#[derive(Default)]
struct ClientState {
    session: Option<AppleSession>,
    jar: CookieJar,
}

/// Raw request results.
struct Exchange {
    status: u16,
    data: Value,
    response: AppleResponse,
}

enum RawFailure {
    Protocol,
    Transient,
}

/// Request bodies: absent (`undefined`) or JSON (including `null`).
enum Body {
    None,
    Json(Value),
}

/// Validates an Apple service URL (`appleUrl`).
fn apple_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?.to_owned();
    let trusted = host == "icloud.com"
        || host.ends_with(".icloud.com")
        || host == "apple.com"
        || host.ends_with(".apple.com");
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && trusted)
        .then_some(url)
}

/// A non-empty string.
fn string_field(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn validated_srp_challenge(value: &Value) -> Option<ServerChallenge> {
    let data = as_record(value);
    let protocol = match field(data, "protocol").as_str() {
        Some("s2k") => SrpProtocol::S2k,
        Some("s2k_fo") => SrpProtocol::S2kFo,
        _ => return None,
    };
    let iteration =
        integer(field(data, "iteration")).filter(|n| (1.0..=1_000_000.0).contains(n))?;
    let text = |key: &str| {
        field(data, key)
            .as_str()
            .filter(|s| omni_core::js::utf16_len(s) < 4_096)
            .map(str::to_owned)
    };
    let (salt, b, c) = (text("salt")?, text("b")?, text("c")?);
    let server_public = crate::json::node_base64(&b);
    if server_public.is_empty() || server_public.len() > srp::SRP_N_BYTES {
        return None;
    }
    let value = num_bigint::BigUint::from_bytes_be(&server_public);
    if (value % srp::srp_n()) == num_bigint::BigUint::default() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let iteration = iteration as u32;
    Some(ServerChallenge {
        protocol,
        iteration,
        salt,
        b,
        c,
    })
}

/// Requests that are reads in every respect: allowed under [`SideEffectMode::Record`].
fn is_read_only(url: &Url, body: &Body) -> bool {
    let host = url.host_str().unwrap_or("");
    let path = url.path();
    if host == "setup.icloud.com" {
        return path.ends_with("/validate")
            || path.ends_with("/accountLogin")
            || path.ends_with("/requestWebAccessState");
    }
    if path.ends_with("/changes/zone") || path.ends_with("/records/lookup") {
        return true;
    }
    path.ends_with("/records/query")
        && matches!(body, Body::Json(b) if as_record(field(as_record(b), "query")).get("recordType") == Some(&json!("reminderList")))
}

/// Authentication and bounded CloudKit transport. No network work at construction.
pub struct AppleRemindersClient {
    account: String,
    password: String,
    storage: Arc<dyn SessionStorage>,
    transport: Arc<dyn AppleTransport>,
    clock: SharedClock,
    timeout: Option<Duration>,
    side_effects: SideEffectMode,
    state: Mutex<ClientState>,
}

impl AppleRemindersClient {
    pub fn new(options: AppleClientOptions) -> Self {
        Self {
            account: options.account,
            password: options.password,
            storage: options.storage,
            transport: options.transport,
            clock: options.clock,
            timeout: options.timeout,
            side_effects: options.side_effects,
            state: Mutex::new(ClientState::default()),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ClientState) -> R) -> R {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    fn default_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }

    /// The loaded session (`load()`).
    async fn load(&self) -> Result<AppleSession, AppleRemindersError> {
        if let Some(session) = self.with_state(|s| s.session.clone()) {
            return Ok(session);
        }
        let raw = self
            .storage
            .load()
            .await
            .map_err(|()| fail("load session", "storage failed", None))?;
        let loaded = if raw.is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<AppleSession>(raw)
                    .map_err(|_| fail("load session", "invalid stored session", None))?,
            )
        };
        let jar = match loaded.as_ref().and_then(|s| s.cookies.as_deref()) {
            Some(serialized) => Some(
                serde_json::from_str::<Value>(serialized)
                    .ok()
                    .and_then(|value| CookieJar::from_json(&value).ok())
                    .ok_or_else(|| fail("load session", "invalid stored cookies", None))?,
            ),
            None => None,
        };
        let session = loaded.unwrap_or_else(|| AppleSession {
            client_id: format!("auth-{}", omni_core::ids::uuid_v4()),
            ..AppleSession::default()
        });
        session.register_for_scrub();
        Ok(self.with_state(|s| {
            // A concurrent load may have won; keep the first.
            if s.session.is_none() {
                s.session = Some(session);
                if let Some(jar) = jar {
                    s.jar = jar;
                }
            }
            s.session.clone().unwrap_or_default()
        }))
    }

    /// Saves the session with the serialized cookie jar (`persist()`).
    async fn persist(&self) -> Result<(), AppleRemindersError> {
        self.load().await?;
        let snapshot = self.with_state(|s| {
            let cookies = omni_core::js::json_stringify(&s.jar.to_json());
            s.session.as_mut().map(|session| {
                session.cookies = Some(cookies);
                session.clone()
            })
        });
        let Some(session) = snapshot else {
            return Ok(());
        };
        let value = serde_json::to_value(&session)
            .map_err(|_| fail("save session", "storage failed", None))?;
        self.storage
            .save(value)
            .await
            .map_err(|()| fail("save session", "storage failed", None))
    }

    fn update_session(&self, f: impl FnOnce(&mut AppleSession)) {
        self.with_state(|s| {
            if let Some(session) = s.session.as_mut() {
                f(session);
                session.register_for_scrub();
            }
        });
    }

    fn session(&self) -> AppleSession {
        self.with_state(|s| s.session.clone().unwrap_or_default())
    }

    /// `captureAuth(response, session)`.
    fn capture_auth(&self, response: &AppleResponse) {
        let fields: [(&str, SessionSlot); 6] = [
            ("scnt", |s| &mut s.scnt),
            ("x-apple-id-session-id", |s| &mut s.session_id),
            ("x-apple-session-token", |s| &mut s.session_token),
            ("x-apple-twosv-trust-token", |s| &mut s.trust_token),
            ("x-apple-id-account-country", |s| &mut s.account_country),
            ("x-apple-auth-attributes", |s| &mut s.auth_attributes),
        ];
        self.update_session(|session| {
            for (header, slot) in fields {
                if let Some(value) = response.header(header).filter(|v| !v.is_empty()) {
                    *slot(session) = Some(value);
                }
            }
        });
    }

    fn auth_headers(session: &AppleSession) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = [
            ("X-Apple-Widget-Key", APPLE_WIDGET_KEY),
            ("X-Apple-OAuth-Client-Id", APPLE_WIDGET_KEY),
            ("X-Apple-OAuth-Client-Type", "firstPartyAuth"),
            ("X-Apple-OAuth-Redirect-URI", "https://www.icloud.com"),
            ("X-Apple-OAuth-Require-Grant-Code", "true"),
            ("X-Apple-OAuth-Response-Type", "code"),
            ("X-Apple-OAuth-Response-Mode", "web_message"),
            ("X-Apple-OAuth-State", session.client_id.as_str()),
            ("X-Apple-Frame-Id", session.client_id.as_str()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        if let Some(scnt) = present(session.scnt.as_ref()) {
            headers.push(("scnt".into(), scnt.to_owned()));
        }
        if let Some(id) = present(session.session_id.as_ref()) {
            headers.push(("X-Apple-ID-Session-Id".into(), id.to_owned()));
        }
        if let Some(attributes) = present(session.auth_attributes.as_ref()) {
            headers.push(("X-Apple-Auth-Attributes".into(), attributes.to_owned()));
        }
        headers
    }

    /// One exchange, cookies applied, redirects and oversize rejected.
    async fn request_raw(
        &self,
        raw_url: &str,
        method: Method,
        body: &Body,
        extra: Vec<(String, String)>,
    ) -> Result<Exchange, RawFailure> {
        let url = apple_url(raw_url).ok_or(RawFailure::Protocol)?;
        if self.side_effects == SideEffectMode::Record && !is_read_only(&url, body) {
            tracing::info!(target: LOG, path = url.path(), "Recorded Apple request (side effects disabled)");
            return Err(RawFailure::Transient);
        }
        let host = url.host_str().unwrap_or("").to_owned();
        let is_auth = host == "idmsa.apple.com";
        let is_srp = url.path().starts_with("/appleauth/auth/signin/");
        let now = self.clock.now_ms();
        let cookies = self.with_state(|s| s.jar.cookie_header(&url, now));
        let mut headers: Vec<(String, String)> = vec![
            (
                "User-Agent".into(),
                if is_auth {
                    AUTH_USER_AGENT
                } else {
                    SERVICE_USER_AGENT
                }
                .into(),
            ),
            ("Accept".into(), "application/json".into()),
            ("Origin".into(), "https://www.icloud.com".into()),
            (
                "Referer".into(),
                if is_auth && !is_srp {
                    "https://idmsa.apple.com"
                } else {
                    "https://www.icloud.com/"
                }
                .into(),
            ),
        ];
        if matches!(body, Body::Json(_)) {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        if !cookies.is_empty() {
            headers.push(("Cookie".into(), cookies));
        }
        for (name, value) in extra {
            match headers.iter_mut().find(|(k, _)| *k == name) {
                Some(existing) => existing.1 = value,
                None => headers.push((name, value)),
            }
        }
        let request = AppleRequest {
            method,
            url: url.clone(),
            headers,
            body: match body {
                Body::None => None,
                Body::Json(value) => Some(omni_core::js::json_stringify(value)),
            },
        };
        let response = self.transport.send(request).await.map_err(|e| match e {
            TransportError::TooLarge => RawFailure::Protocol,
            TransportError::Network => RawFailure::Transient,
        })?;
        let now = self.clock.now_ms();
        let cookie_result = self.with_state(|s| {
            response
                .set_cookies()
                .try_for_each(|cookie| s.jar.set_cookie(cookie, &url, now))
        });
        cookie_result.map_err(|_| RawFailure::Transient)?;
        if (300..400).contains(&response.status) {
            return Err(RawFailure::Protocol);
        }
        let declared = response
            .header("content-length")
            .map(|v| omni_core::js::string_to_number(&v))
            .unwrap_or(0.0);
        if declared > MAX_RESPONSE_BYTES as f64 || response.body.len() > MAX_RESPONSE_BYTES {
            return Err(RawFailure::Protocol);
        }
        let raw = String::from_utf8_lossy(&response.body);
        let mut data = Value::Null;
        if !crate::json::js_blank(&raw) {
            match serde_json::from_str::<Value>(&raw) {
                Ok(parsed) => data = parsed,
                Err(_) if (200..300).contains(&response.status) => {
                    return Err(RawFailure::Protocol);
                }
                Err(_) => {}
            }
        }
        Ok(Exchange {
            status: response.status,
            data,
            response,
        })
    }

    /// `request(...)`: bounded by a timeout; persists the session on success.
    async fn request(
        &self,
        operation: &str,
        url: &str,
        method: Method,
        body: Body,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<Exchange, AppleRemindersError> {
        let exchange = match tokio::time::timeout(
            timeout,
            self.request_raw(url, method, &body, headers),
        )
        .await
        {
            Err(_) => {
                return Err(AppleRemindersError::new(
                    operation,
                    "Apple request timed out",
                    None,
                    AppleErrorKind::TransientOutage,
                ));
            }
            Ok(Err(failure)) => {
                return Err(AppleRemindersError::new(
                    operation,
                    "Apple request failed",
                    None,
                    match failure {
                        RawFailure::Protocol => AppleErrorKind::UnsupportedProtocol,
                        RawFailure::Transient => AppleErrorKind::TransientOutage,
                    },
                ));
            }
            Ok(Ok(exchange)) => exchange,
        };
        if self.with_state(|s| s.session.is_some()) {
            self.capture_auth(&exchange.response);
        }
        self.persist().await?;
        Ok(exchange)
    }

    fn set_ck_base(&self, url: &Url, dsid: &Value) {
        let base = format!("{}{CLOUDKIT_SUFFIX}", url.as_str().trim_end_matches('/'));
        let dsid = match dsid {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => n.as_f64().map(omni_core::js::number_to_string),
            _ => None,
        };
        self.update_session(|session| {
            session.ck_base_url = Some(base);
            if let Some(dsid) = dsid {
                session.dsid = Some(dsid);
            }
        });
    }

    /// `Number(dsInfo.hsaVersion ?? 0) >= 2` with a pending challenge or untrusted browser.
    fn needs_hsa(data: &serde_json::Map<String, Value>) -> bool {
        let ds_info = as_record(field(data, "dsInfo"));
        let version = match field(ds_info, "hsaVersion") {
            Value::Null => 0.0,
            Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
            Value::String(s) => omni_core::js::string_to_number(s),
            Value::Bool(b) => f64::from(u8::from(*b)),
            _ => f64::NAN,
        };
        version >= 2.0
            && (field(data, "hsaChallengeRequired") == &Value::Bool(true)
                || field(data, "hsaTrustedBrowser") == &Value::Bool(false))
    }

    async fn account_login(&self) -> Result<bool, AppleRemindersError> {
        let session = self.load().await?;
        let Some(token) = present(session.session_token.as_ref()).map(str::to_owned) else {
            return Err(fail("account login", "session token missing", None));
        };
        let mut body = serde_json::Map::new();
        if let Some(country) = &session.account_country {
            body.insert("accountCountryCode".into(), Value::String(country.clone()));
        }
        body.insert("dsWebAuthToken".into(), Value::String(token));
        body.insert("extended_login".into(), Value::Bool(true));
        body.insert(
            "trustToken".into(),
            Value::String(session.trust_token.clone().unwrap_or_default()),
        );
        let response = self
            .request(
                "account login",
                &format!("{SETUP_ROOT}accountLogin"),
                Method::POST,
                Body::Json(Value::Object(body)),
                Vec::new(),
                self.default_timeout(),
            )
            .await?;
        self.capture_auth(&response.response);
        if response.status != 200 {
            return Err(fail(
                "account login",
                "Apple rejected session",
                Some(response.status),
            ));
        }
        let data = as_record(&response.data);
        if field(data, "termsUpdateNeeded") == &Value::Bool(true) {
            return Err(fail(
                "account login",
                "terms acceptance required",
                Some(451),
            ));
        }
        if Self::needs_hsa(data) {
            self.persist().await?;
            return Ok(false);
        }
        let ck = as_record(field(as_record(field(data, "webservices")), "ckdatabasews"));
        let Some(ck_url) = string_field(field(ck, "url")) else {
            return Err(fail("account login", "CloudKit unavailable", None));
        };
        let trusted = apple_url(&ck_url)
            .ok_or_else(|| fail("account login", "untrusted CloudKit URL", None))?;
        self.set_ck_base(&trusted, field(as_record(field(data, "dsInfo")), "dsid"));
        self.persist().await?;
        Ok(true)
    }

    /// Validates the saved token without starting interactive sign-in.
    pub async fn verify_session(&self) -> Result<bool, AppleRemindersError> {
        let session = self.load().await?;
        if present(session.session_token.as_ref()).is_none() {
            return Ok(false);
        }
        let response = self
            .request(
                "validate session",
                &format!("{SETUP_ROOT}validate"),
                Method::POST,
                Body::Json(Value::Null),
                Vec::new(),
                self.default_timeout(),
            )
            .await?;
        self.capture_auth(&response.response);
        if matches!(response.status, 401 | 403 | 421) {
            return Ok(false);
        }
        if response.status != 200 {
            return Err(fail(
                "validate session",
                "Apple rejected validation",
                Some(response.status),
            ));
        }
        let data = as_record(&response.data);
        if field(data, "termsUpdateNeeded") == &Value::Bool(true) {
            return Err(fail(
                "validate session",
                "terms acceptance required",
                Some(451),
            ));
        }
        if Self::needs_hsa(data) {
            return Ok(false);
        }
        let ck = as_record(field(as_record(field(data, "webservices")), "ckdatabasews"));
        let Some(ck_url) = string_field(field(ck, "url")) else {
            return Ok(false);
        };
        let trusted = apple_url(&ck_url)
            .ok_or_else(|| fail("validate session", "untrusted CloudKit URL", None))?;
        self.set_ck_base(&trusted, field(as_record(field(data, "dsInfo")), "dsid"));
        self.persist().await?;
        Ok(true)
    }

    async fn mfa_options(&self) -> Result<Exchange, AppleRemindersError> {
        let session = self.session();
        self.request(
            "MFA options",
            &AUTH_ROOT[..AUTH_ROOT.len() - 1],
            Method::GET,
            Body::None,
            Self::auth_headers(&session),
            self.default_timeout(),
        )
        .await
    }

    /// Starts explicit SRP sign-in. The caller owns the second-factor step.
    pub async fn begin_sign_in(&self) -> Result<AppleBeginResult, AppleRemindersError> {
        if self.account.is_empty() || self.password.is_empty() {
            return Err(fail("begin", "Apple account is not configured", None));
        }
        if self.verify_session().await? {
            return Ok(AppleBeginResult::Ready);
        }
        self.load().await?;
        let mut srp = GsaSrpAuthenticator::new(&self.account);
        let mut first = None;
        for attempt in 0..2 {
            let init = json!({
                "a": srp.public_a(),
                "protocols": ["s2k", "s2k_fo"],
                "accountName": self.account,
            });
            let response = self
                .request(
                    "SRP init",
                    &format!("{AUTH_ROOT}signin/init"),
                    Method::POST,
                    Body::Json(init),
                    Self::auth_headers(&self.session()),
                    self.default_timeout(),
                )
                .await?;
            let conflict = response.status == 409;
            first = Some(response);
            if !conflict || attempt == 1 {
                break;
            }
            self.with_state(|s| {
                if let Some(session) = s.session.as_mut() {
                    session.scnt = None;
                    session.session_id = None;
                    session.session_token = None;
                    session.auth_attributes = None;
                }
                s.jar.clear();
            });
            self.persist().await?;
            srp = GsaSrpAuthenticator::new(&self.account);
        }
        let Some(first) = first else {
            return Err(fail("SRP init", "no response", None));
        };
        if first.status != 200 {
            self.persist().await?;
            return Err(fail(
                "SRP init",
                "Apple rejected sign-in",
                Some(first.status),
            ));
        }
        let proof_error = || {
            AppleRemindersError::new(
                "SRP proof",
                "unsupported or invalid challenge",
                None,
                AppleErrorKind::UnsupportedProtocol,
            )
        };
        let challenge = validated_srp_challenge(&first.data).ok_or_else(proof_error)?;
        let password = self.password.clone();
        let proof = tokio::task::spawn_blocking(move || srp.complete(&password, &challenge))
            .await
            .ok()
            .flatten()
            .ok_or_else(proof_error)?;
        let session = self.session();
        let second = self
            .request(
                "SRP complete",
                &format!("{AUTH_ROOT}signin/complete?isRememberMeEnabled=true"),
                Method::POST,
                Body::Json(json!({
                    "accountName": proof.account_name,
                    "m1": proof.m1,
                    "m2": proof.m2,
                    "c": proof.c,
                    "trustTokens": session.trust_token.iter().filter(|t| !t.is_empty()).collect::<Vec<_>>(),
                    "rememberMe": true,
                })),
                Self::auth_headers(&session),
                self.default_timeout(),
            )
            .await?;
        self.capture_auth(&second.response);
        self.persist().await?;
        if second.status != 200 && second.status != 409 {
            return Err(fail(
                "SRP complete",
                "Apple rejected sign-in",
                Some(second.status),
            ));
        }
        if present(self.session().session_token.as_ref()).is_some() {
            let trusted = match self.account_login().await {
                Ok(trusted) => trusted,
                Err(error)
                    if second.status == 409
                        && error.operation == "account login"
                        && matches!(error.status, Some(401 | 403 | 421)) =>
                {
                    false
                }
                Err(error) => return Err(error),
            };
            if trusted {
                return Ok(AppleBeginResult::Ready);
            }
        }
        let options = self.mfa_options().await?;
        self.capture_auth(&options.response);
        if options.status != 200 {
            return Err(fail(
                "MFA options",
                "Apple challenge unavailable",
                Some(options.status),
            ));
        }
        if is_truthy(field(as_record(&options.data), "fsaChallenge")) {
            return Err(AppleRemindersError::new(
                "MFA options",
                "security-key authentication is required",
                None,
                AppleErrorKind::UnsupportedProtocol,
            ));
        }
        // PUT requests delivery; POST to this same path verifies the code.
        // The parent /verify/trusteddevice path rejects PUT with HTTP 405.
        let push = self
            .request(
                "MFA push",
                &format!("{AUTH_ROOT}verify/trusteddevice/securitycode"),
                Method::PUT,
                Body::None,
                Self::auth_headers(&self.session()),
                self.default_timeout(),
            )
            .await?;
        // ioBroker continues to code entry when the optional device push fails.
        // Limit that fallback to method-not-allowed with a usable MFA challenge;
        // authentication, outage and rate-limit failures must remain visible.
        let session = self.session();
        let code_entry_available = push.status == 405
            && present(session.scnt.as_ref()).is_some()
            && present(session.session_id.as_ref()).is_some();
        // Apple may accept asynchronous popup delivery with 202.
        if !matches!(push.status, 200 | 202 | 204) && !code_entry_available {
            return Err(fail(
                "MFA push",
                "Apple rejected device notification",
                Some(push.status),
            ));
        }
        self.persist().await?;
        Ok(AppleBeginResult::MfaRequired)
    }

    /// Verifies a six-digit trusted-device code.
    pub async fn submit_code(&self, code: &str) -> Result<(), AppleRemindersError> {
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(fail("MFA verify", "invalid six-digit code", None));
        }
        let session = self.load().await?;
        if present(session.scnt.as_ref()).is_none()
            || present(session.session_id.as_ref()).is_none()
        {
            return Err(fail("MFA verify", "MFA challenge missing", None));
        }
        let response = self
            .request(
                "MFA verify",
                &format!("{AUTH_ROOT}verify/trusteddevice/securitycode"),
                Method::POST,
                Body::Json(json!({"securityCode": {"code": code}})),
                Self::auth_headers(&session),
                self.default_timeout(),
            )
            .await?;
        self.capture_auth(&response.response);
        // Modern Apple code verification can report success as HTTP 409. Require
        // explicit validity and a token on this response, never a saved token or the
        // conflict status alone. Trust/account checks still follow.
        let valid = field(
            as_record(field(as_record(&response.data), "securityCode")),
            "valid",
        );
        let accepted_conflict = response.status == 409
            && valid == &Value::Bool(true)
            && response
                .response
                .header("x-apple-session-token")
                .is_some_and(|t| !crate::json::js_blank(&t));
        if valid == &Value::Bool(false)
            || (response.status != 200 && response.status != 204 && !accepted_conflict)
        {
            self.persist().await?;
            return Err(fail(
                "MFA verify",
                "Apple rejected code",
                Some(response.status),
            ));
        }
        let trust = self
            .request(
                "trust browser",
                &format!("{AUTH_ROOT}2sv/trust"),
                Method::GET,
                Body::None,
                Self::auth_headers(&self.session()),
                self.default_timeout(),
            )
            .await?;
        self.capture_auth(&trust.response);
        if trust.status != 200 && trust.status != 204 {
            return Err(fail(
                "trust browser",
                "Apple rejected trust",
                Some(trust.status),
            ));
        }
        self.persist().await?;
        if !self.account_login().await? {
            return Err(fail("MFA verify", "session still requires MFA", None));
        }
        Ok(())
    }

    /// Requests protected-data access through the shared PCS workflow.
    pub async fn request_pcs(&self) -> Result<ProtectedAccess, AppleRemindersError> {
        let session = self.load().await?;
        if present(session.session_token.as_ref()).is_none() {
            return Err(fail("PCS", "session missing", None));
        }
        let query = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("clientBuildNumber", "2534Project66");
            query.append_pair("clientMasteringNumber", "2534B22");
            query.append_pair("clientId", &session.client_id);
            if let Some(dsid) = present(session.dsid.as_ref()) {
                query.append_pair("dsid", dsid);
            }
            query.finish()
        };
        let requester = PcsAdapter {
            client: self,
            query,
        };
        request_protected_access(&requester, "reminders")
            .await
            .map_err(|error| match error {
                PcsError::Request(error) => error,
                PcsError::Pcs(pcs) => AppleRemindersError::new(
                    pcs.operation.as_str(),
                    "Protected iCloud access failed",
                    pcs.status,
                    match pcs.status {
                        None => AppleErrorKind::UnsupportedProtocol,
                        Some(status) => {
                            fail(
                                pcs.operation.as_str(),
                                "Protected iCloud access failed",
                                Some(status),
                            )
                            .kind
                        }
                    },
                ),
            })
    }

    /// A bounded CloudKit call. Only known reads replay once after a 401.
    pub async fn cloudkit(&self, path: CkPath, body: Value) -> Result<Value, AppleRemindersError> {
        let session = self.load().await?;
        let Some(base) = present(session.ck_base_url.as_ref()).map(str::to_owned) else {
            return Err(fail("CloudKit", "session not ready", None));
        };
        let url = format!(
            "{base}{}?remapEnums=true&getCurrentSyncToken=true",
            path.as_str()
        );
        let timeout = if path == CkPath::RecordsQuery {
            self.timeout.unwrap_or(QUERY_TIMEOUT)
        } else {
            self.default_timeout()
        };
        let call = || {
            self.request(
                "CloudKit",
                &url,
                Method::POST,
                Body::Json(body.clone()),
                vec![("Content-Type".into(), "text/plain".into())],
                timeout,
            )
        };
        let mut response = call().await?;
        // CloudKit queries are not inherently reads: CompleteRecurringReminder and
        // createTreeDeletion mutate state. Only known read operations may replay.
        let replay_safe = match path {
            CkPath::ChangesZone | CkPath::RecordsLookup => true,
            CkPath::RecordsQuery => {
                field(as_record(field(as_record(&body), "query")), "recordType")
                    == &Value::String("reminderList".into())
            }
            CkPath::RecordsModify => false,
        };
        if response.status == 401 && replay_safe {
            if !self.account_login().await? {
                return Err(fail("CloudKit", "session requires verification", Some(401)));
            }
            response = call().await?;
        }
        if response.status != 200 {
            return Err(fail(
                "CloudKit",
                "Apple rejected request",
                Some(response.status),
            ));
        }
        let data = as_record(&response.data).clone();
        if is_truthy(field(&data, "error")) {
            return Err(fail("CloudKit", "Apple reported an error", None));
        }
        let mut errors: Vec<&serde_json::Map<String, Value>> = vec![&data];
        if let Some(records) = data.get("records").and_then(Value::as_array) {
            errors.extend(records.iter().map(as_record));
        }
        if let Some(zones) = data.get("zones").and_then(Value::as_array) {
            errors.extend(
                zones
                    .iter()
                    .map(|zone| as_record(field(as_record(zone), "error"))),
            );
        }
        for error in errors {
            let code = js_string(field(error, "serverErrorCode"));
            if code == "AUTHENTICATION_REQUIRED" || code == "AUTHENTICATION_FAILED" {
                return Err(fail("CloudKit", "session requires verification", Some(401)));
            }
            if code == "THROTTLED" || code == "RATE_LIMITED" {
                return Err(fail("CloudKit", "request throttled", Some(429)));
            }
        }
        self.persist().await?;
        Ok(Value::Object(data))
    }
}

/// `String(value)` for the error-code comparison.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}

/// JS truthiness.
pub(crate) fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

struct PcsAdapter<'a> {
    client: &'a AppleRemindersClient,
    query: String,
}

impl PcsRequester<AppleRemindersError> for PcsAdapter<'_> {
    fn request(
        &self,
        operation: PcsOperation,
        endpoint: PcsEndpoint,
        body: Option<Value>,
    ) -> BoxFuture<'_, Result<PcsResponse, AppleRemindersError>> {
        Box::pin(async move {
            let exchange = self
                .client
                .request(
                    operation.as_str(),
                    &format!("{SETUP_ROOT}{}?{}", endpoint.as_str(), self.query),
                    Method::POST,
                    body.map_or(Body::None, Body::Json),
                    Vec::new(),
                    self.client.default_timeout(),
                )
                .await?;
            Ok(PcsResponse {
                status: exchange.status,
                data: exchange.data,
            })
        })
    }
}

impl AppleApi for AppleRemindersClient {
    fn begin(&self) -> BoxFuture<'_, Result<AppleBeginResult, AppleRemindersError>> {
        Box::pin(self.begin_sign_in())
    }

    fn verify(&self) -> BoxFuture<'_, Result<bool, AppleRemindersError>> {
        Box::pin(self.verify_session())
    }

    fn submit_2fa<'a>(&'a self, code: &'a str) -> BoxFuture<'a, Result<(), AppleRemindersError>> {
        Box::pin(self.submit_code(code))
    }

    fn request_pcs_access(&self) -> BoxFuture<'_, Result<ProtectedAccess, AppleRemindersError>> {
        Box::pin(self.request_pcs())
    }

    fn ck_post(
        &self,
        path: CkPath,
        body: Value,
    ) -> BoxFuture<'_, Result<Value, AppleRemindersError>> {
        Box::pin(self.cloudkit(path, body))
    }
}
