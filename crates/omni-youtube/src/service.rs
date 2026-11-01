//! The owner's YouTube account: Google OAuth, access-token refresh, API
//! requests with quota accounting, and every tool operation.
//!
//! Writes are serialized, check what already exists before sending (titles,
//! videos already in a playlist), and settle a lost answer only by reading
//! YouTube back; an uncertain write is never resent.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use omni_config::Config;
use omni_http::{HttpClient, Method, SideEffectMode, Url};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use crate::LOG;
use crate::error::{ApiFailure, YoutubeError};
use crate::model::{
    AddOutcome, AddResult, ChannelsOutput, ConnectedChannel, ConnectionState, CreateOutcome,
    CreateOutput, DeleteOutcome, DeleteOutput, ItemsAddOutput, ItemsRemoveOutput, MoveOutcome,
    MoveOutput, PlaylistItemsPage, PlaylistsPage, Privacy, QuotaStatus, RemoveOutcome,
    RemoveResult, SearchOutput, SearchType, Status, UpdateOutcome, UpdateOutput, VideosOutput,
};
use crate::records::{
    OWNER, WriteState, YoutubeAccountData, YoutubeOauthStateData, YoutubeQuotaData,
    YoutubeWriteData,
};
use crate::secrets::Sealer;
use crate::wire::{
    ChannelRes, ErrorBody, ListResponse, PlaylistItemRes, PlaylistRes, SearchRes, VideoRes, cut,
};

pub const START_PATH: &str = "/api/youtube/oauth/start";
pub const CALLBACK_PATH: &str = "/api/youtube/oauth/callback";
/// The only scope requested: manage the account's playlists. It also covers
/// public reads, so no API key is needed.
pub const SCOPE: &str = "https://www.googleapis.com/auth/youtube";
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
/// Google's default daily quota for a project.
pub const DEFAULT_DAILY_LIMIT: u64 = 10_000;
/// YouTube's quota day starts at midnight Pacific time.
const QUOTA_TZ: &str = "America/Los_Angeles";

const STATE_TTL_MS: i64 = 10 * 60 * 1000;
const QUOTA_TTL_MS: i64 = 8 * 24 * 60 * 60 * 1000;
const WRITE_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Access tokens are refreshed this long before Google's expiry.
const REFRESH_MARGIN_MS: i64 = 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY: usize = 4 * 1024 * 1024;
/// 200 pages of 50 cover YouTube's 5,000-video playlist limit twice over.
const MAX_SCAN_PAGES: usize = 200;
/// A create counts as the reserved one when YouTube dates it at most this
/// long before the reservation (clock skew).
const CREATE_SKEW_MS: i64 = 5 * 60 * 1000;

/// What the YouTube Data API does not give access to, shown by `youtube_status`.
pub const NOT_AVAILABLE: [&str; 4] = [
    "Watch Later (the API has not served it since 2016)",
    "Watch history",
    "The personalized Home feed and recommendations",
    "Transcripts or caption text",
];

const COST_READ: u64 = 1;
const COST_SEARCH: u64 = 100;
const COST_WRITE: u64 = 50;

/// The OAuth client and callback, from `YOUTUBE_*`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            client_id: config.youtube_client_id.clone(),
            client_secret: config.youtube_client_secret.clone(),
            redirect_uri: config.youtube_redirect_uri.clone(),
        }
    }
}

pub struct Deps {
    pub settings: Settings,
    pub store: Store,
    pub http: HttpClient,
    /// `Err` when `youtube.key` cannot be read or created.
    pub sealer: Result<Sealer, String>,
    pub mode: SideEffectMode,
}

/// A write that `SideEffectMode::Record` kept instead of sending.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    pub method: &'static str,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

struct Cached {
    token: String,
    expires_at: i64,
}

struct Inner {
    settings: Settings,
    config_problem: Option<String>,
    connect_url: String,
    callback_host: Option<String>,
    store: Store,
    http: HttpClient,
    sealer: Result<Sealer, String>,
    mode: SideEffectMode,
    token: Mutex<Option<Cached>>,
    writes: Mutex<()>,
    recorded: std::sync::Mutex<Vec<Recorded>>,
}

#[derive(Clone)]
pub struct Youtube {
    inner: Arc<Inner>,
}

/// One API request.
struct Req {
    method: Method,
    path: &'static str,
    name: &'static str,
    cost: u64,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
}

impl Req {
    fn get(path: &'static str, name: &'static str) -> Self {
        Self {
            method: Method::GET,
            path,
            name,
            cost: COST_READ,
            query: Vec::new(),
            body: None,
        }
    }

    fn write(method: Method, path: &'static str, name: &'static str) -> Self {
        Self {
            cost: COST_WRITE,
            method,
            ..Self::get(path, name)
        }
    }

    fn q(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.query.push((key, value.into()));
        self
    }

    fn q_opt(self, key: &'static str, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.q(key, value),
            None => self,
        }
    }

    fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Default, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

enum TokenFailure {
    /// `invalid_grant`: the refresh token or code is revoked, expired or used.
    Grant(String),
    Other(YoutubeError),
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn parse_ms(raw: &str) -> Option<i64> {
    raw.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}

/// The Pacific quota day of `ms` and when it ends.
pub fn quota_day(ms: i64) -> (String, i64) {
    let fallback = || (iso(ms).chars().take(10).collect(), ms);
    let Ok(tz) = jiff::tz::TimeZone::get(QUOTA_TZ) else {
        return fallback();
    };
    let Ok(now) = jiff::Timestamp::from_millisecond(ms) else {
        return fallback();
    };
    let date = now.to_zoned(tz.clone()).date();
    let resets = date
        .tomorrow()
        .ok()
        .and_then(|next| next.to_zoned(tz).ok())
        .map_or(ms, |z| z.timestamp().as_millisecond());
    (date.to_string(), resets)
}

fn decode<T: DeserializeOwned>(name: &str, value: Value) -> Result<T, YoutubeError> {
    serde_json::from_value(value).map_err(|e| YoutubeError::Decode(format!("{name}: {e}")))
}

fn same_title(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

fn uncertain(what: &str) -> YoutubeError {
    YoutubeError::Uncertain(format!(
        "{what}: YouTube's answer was lost and reading it back did not confirm the change. Omni did not resend it; check the playlist before trying again."
    ))
}

impl Youtube {
    pub fn new(deps: Deps) -> Self {
        let redirect = Url::parse(&deps.settings.redirect_uri).ok();
        let config_problem = match &redirect {
            None => Some("YOUTUBE_REDIRECT_URI is not a URL".to_owned()),
            Some(url) if url.path() != CALLBACK_PATH => Some(format!(
                "YOUTUBE_REDIRECT_URI must end in {CALLBACK_PATH}, the route Omni serves"
            )),
            Some(_) => None,
        };
        let callback_host = redirect.as_ref().and_then(|url| {
            let host = url.host_str()?;
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            })
        });
        let connect_url = redirect.map_or_else(
            || START_PATH.to_owned(),
            |mut url| {
                url.set_path(START_PATH);
                url.set_query(None);
                url.to_string()
            },
        );
        Self {
            inner: Arc::new(Inner {
                settings: deps.settings,
                config_problem,
                connect_url,
                callback_host,
                store: deps.store,
                http: deps.http,
                sealer: deps.sealer,
                mode: deps.mode,
                token: Mutex::new(None),
                writes: Mutex::new(()),
                recorded: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn connect_url(&self) -> &str {
        &self.inner.connect_url
    }

    /// The host (and port) of `YOUTUBE_REDIRECT_URI`.
    pub fn callback_host(&self) -> Option<&str> {
        self.inner.callback_host.as_deref()
    }

    /// Writes `SideEffectMode::Record` kept.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.inner
            .recorded
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    fn now(&self) -> i64 {
        self.inner.store.clock().now_ms()
    }

    fn credentials(&self) -> Result<(&str, &str), YoutubeError> {
        let settings = &self.inner.settings;
        match (
            settings.client_id.as_deref().filter(|v| !v.is_empty()),
            settings.client_secret.as_deref().filter(|v| !v.is_empty()),
        ) {
            (Some(id), Some(secret)) if self.inner.config_problem.is_none() => Ok((id, secret)),
            _ => Err(YoutubeError::NotConfigured),
        }
    }

    fn sealer(&self) -> Result<&Sealer, YoutubeError> {
        self.inner
            .sealer
            .as_ref()
            .map_err(|reason| YoutubeError::Reconnect {
                reason: format!("the token key file is unusable: {reason}"),
                connect_url: self.inner.connect_url.clone(),
            })
    }

    async fn account(&self) -> Result<Option<YoutubeAccountData>, YoutubeError> {
        Ok(self
            .inner
            .store
            .read(|tx| tx.get::<YoutubeAccountData>(&OWNER.to_owned()))
            .await?)
    }

    // ---- OAuth -------------------------------------------------------------

    /// Starts an authorization: stores a single-use state with its PKCE
    /// verifier and returns Google's consent URL.
    pub async fn start_authorization(&self) -> Result<String, YoutubeError> {
        let (client_id, _) = self.credentials()?;
        self.sealer()?;
        let state = random_token();
        let verifier = random_token();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let row = YoutubeOauthStateData {
            state: state.clone(),
            code_verifier: verifier,
            created_at: self.now(),
            extra: Extra::default(),
        };
        self.inner
            .store
            .write(move |tx| {
                tx.upsert(
                    &row,
                    UpsertOpts {
                        ttl_ms: Some(STATE_TTL_MS),
                        ..UpsertOpts::default()
                    },
                )
            })
            .await?;
        let mut url = Url::parse(AUTH_URL).map_err(|e| YoutubeError::Decode(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &self.inner.settings.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPE)
            .append_pair("access_type", "offline")
            // Google returns a refresh token only on a consent screen.
            .append_pair("prompt", "consent")
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    /// Finishes an authorization from Google's callback: consumes the state,
    /// exchanges the code, checks the scope and stores the sealed refresh token.
    pub async fn complete_authorization(
        &self,
        code: &str,
        state: &str,
    ) -> Result<ConnectedChannel, YoutubeError> {
        let (client_id, client_secret) = self.credentials()?;
        let sealer = self.sealer()?.clone();
        let key = state.to_owned();
        let now = self.now();
        let pending = self
            .inner
            .store
            .write(move |tx| {
                let row = tx.get::<YoutubeOauthStateData>(&key)?;
                if row.is_some() {
                    tx.delete::<YoutubeOauthStateData>(&key)?;
                }
                Ok::<_, StoreError>(row)
            })
            .await?
            .filter(|row| now - row.created_at <= STATE_TTL_MS)
            .ok_or_else(|| {
                YoutubeError::Refused(format!(
                    "This sign-in expired or was already used; start again at {}",
                    self.inner.connect_url
                ))
            })?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", self.inner.settings.redirect_uri.as_str()),
            ("code_verifier", pending.code_verifier.as_str()),
        ];
        let tokens = match self.token_request(&form).await {
            Ok(tokens) => tokens,
            Err(TokenFailure::Grant(reason)) => {
                return Err(YoutubeError::Refused(format!(
                    "Google rejected the authorization code ({reason}); start again at {}",
                    self.inner.connect_url
                )));
            }
            Err(TokenFailure::Other(error)) => return Err(error),
        };
        let scopes: Vec<String> = tokens
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if !scopes.iter().any(|s| s == SCOPE) {
            return Err(YoutubeError::Refused(format!(
                "YouTube access was not granted; allow \"Manage your YouTube account\" at {}",
                self.inner.connect_url
            )));
        }
        let Some(refresh) = tokens.refresh_token.as_deref() else {
            return Err(YoutubeError::Refused(format!(
                "Google returned no refresh token; remove Omni at https://myaccount.google.com/connections and connect again at {}",
                self.inner.connect_url
            )));
        };
        let expires_at = self.now() + tokens.expires_in.unwrap_or(3600) * 1000;
        let channel = self.own_channel_from_api(&tokens.access_token).await?;
        let row = YoutubeAccountData {
            id: OWNER.to_owned(),
            refresh_token: sealer.seal(&YoutubeAccountData::seal_context(OWNER), refresh),
            scopes,
            channel_id: channel.as_ref().map(|c| c.id.clone()),
            channel_title: channel.as_ref().and_then(|c| c.title().map(str::to_owned)),
            connected_at: self.now(),
            problem: None,
            problem_at: None,
            extra: Extra::default(),
        };
        let connected = ConnectedChannel {
            channel_id: row.channel_id.clone().unwrap_or_default(),
            title: row.channel_title.clone(),
        };
        self.inner
            .store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await?;
        *self.inner.token.lock().await = Some(Cached {
            token: tokens.access_token.clone(),
            expires_at,
        });
        tracing::info!(
            target: LOG,
            "Connected YouTube channel {}",
            connected.title.as_deref().unwrap_or("(no channel)")
        );
        Ok(connected)
    }

    /// `channels.list mine=true` with a fresh token; `None` when the account has no channel.
    async fn own_channel_from_api(&self, token: &str) -> Result<Option<ChannelRes>, YoutubeError> {
        let req = Req::get("channels", "channels.list")
            .q("part", "snippet")
            .q("mine", "true");
        let value = self.send_with(&req, token).await?;
        let list: ListResponse<ChannelRes> = decode(req.name, value)?;
        Ok(list.items.into_iter().next())
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, TokenFailure> {
        let url = Url::parse(TOKEN_URL)
            .map_err(|e| TokenFailure::Other(YoutubeError::Decode(e.to_string())))?;
        let response = self
            .inner
            .http
            .request(Method::POST, url)
            .timeout(REQUEST_TIMEOUT)
            .form(form)
            .send_bounded(64 * 1024)
            .await
            .map_err(|e| TokenFailure::Other(YoutubeError::Http(e)))?;
        if response.status.is_success() {
            return serde_json::from_slice(&response.body).map_err(|e| {
                TokenFailure::Other(YoutubeError::Decode(format!("token response: {e}")))
            });
        }
        let error: TokenError = serde_json::from_slice(&response.body).unwrap_or_default();
        let code = error.error.unwrap_or_else(|| "unknown_error".to_owned());
        let description = error
            .error_description
            .map(|d| format!("{code}: {}", cut(&d, 200).0))
            .unwrap_or_else(|| code.clone());
        Err(match code.as_str() {
            "invalid_grant" => TokenFailure::Grant(description),
            "invalid_client" | "unauthorized_client" | "redirect_uri_mismatch" => {
                TokenFailure::Other(YoutubeError::Client(description))
            }
            _ => TokenFailure::Other(YoutubeError::Api(ApiFailure {
                method: "oauth2.token",
                status: response.status.as_u16(),
                reason: Some(code),
                message: description,
                quota_note: None,
            })),
        })
    }

    async fn mark_reconnect(&self, reason: &str) {
        let reason = reason.to_owned();
        let now = self.now();
        let marked = self
            .inner
            .store
            .write(move |tx| {
                tx.update::<YoutubeAccountData>(
                    &OWNER.to_owned(),
                    |mut row| {
                        row.problem = Some(reason);
                        row.problem_at = Some(now);
                        row
                    },
                    omni_store::entity::ModifyOpts::default(),
                )
            })
            .await;
        if let Err(error) = marked {
            tracing::warn!(target: LOG, "YouTube account record failed: {error}");
        }
    }

    /// A valid access token, refreshed when needed. Refreshes are serialized.
    async fn access_token(&self) -> Result<String, YoutubeError> {
        let (client_id, client_secret) = self.credentials()?;
        let mut cached = self.inner.token.lock().await;
        let now = self.now();
        if let Some(token) = cached
            .as_ref()
            .filter(|t| t.expires_at - REFRESH_MARGIN_MS > now)
        {
            return Ok(token.token.clone());
        }
        let connect_url = self.inner.connect_url.clone();
        let Some(account) = self.account().await? else {
            return Err(YoutubeError::NotConnected { connect_url });
        };
        if let Some(reason) = account.problem {
            return Err(YoutubeError::Reconnect {
                reason,
                connect_url,
            });
        }
        let refresh = self.sealer()?.open(
            &YoutubeAccountData::seal_context(OWNER),
            &account.refresh_token,
        );
        let Some(refresh) = refresh else {
            let reason = "the stored token does not open with youtube.key".to_owned();
            self.mark_reconnect(&reason).await;
            return Err(YoutubeError::Reconnect {
                reason,
                connect_url,
            });
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ];
        match self.token_request(&form).await {
            Ok(tokens) => {
                let token = tokens.access_token;
                *cached = Some(Cached {
                    token: token.clone(),
                    expires_at: now + tokens.expires_in.unwrap_or(3600) * 1000,
                });
                Ok(token)
            }
            Err(TokenFailure::Grant(description)) => {
                *cached = None;
                let reason = format!(
                    "Google rejected the refresh token ({description}); it was revoked, unused for six months, or issued while the OAuth app was in Testing (7-day tokens)"
                );
                tracing::warn!(target: LOG, "YouTube needs reconnecting: {reason}");
                self.mark_reconnect(&reason).await;
                Err(YoutubeError::Reconnect {
                    reason,
                    connect_url,
                })
            }
            Err(TokenFailure::Other(YoutubeError::Http(error))) => {
                Err(YoutubeError::TokenRefresh(error.to_string()))
            }
            Err(TokenFailure::Other(error)) => Err(error),
        }
    }

    async fn forget_token(&self, token: &str) {
        let mut cached = self.inner.token.lock().await;
        if cached.as_ref().is_some_and(|c| c.token == token) {
            *cached = None;
        }
    }

    // ---- Requests and quota ------------------------------------------------

    /// Sends `req` with the account's token; a 401 refreshes once and retries
    /// (the request certainly did not run).
    async fn send(&self, req: &Req) -> Result<Value, YoutubeError> {
        let token = self.access_token().await?;
        match self.send_with(req, &token).await {
            Err(YoutubeError::Api(failure)) if failure.status == 401 => {
                self.forget_token(&token).await;
                let token = self.access_token().await?;
                self.send_with(req, &token).await
            }
            other => other,
        }
    }

    async fn send_with(&self, req: &Req, token: &str) -> Result<Value, YoutubeError> {
        let mut url = Url::parse(&format!("{API_BASE}/{}", req.path))
            .map_err(|e| YoutubeError::Decode(e.to_string()))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in &req.query {
                pairs.append_pair(key, value);
            }
        }
        let mut request = self
            .inner
            .http
            .request(req.method.clone(), url)
            .timeout(REQUEST_TIMEOUT)
            .bearer_auth(token);
        if let Some(body) = &req.body {
            request = request.json(body);
        }
        let response = request.send_bounded(MAX_BODY).await;
        if let Err(error) = &response
            && error.is_transient()
        {
            tracing::debug!(target: LOG, "YouTube {} failed: {error}", req.name);
        }
        let response = response.map_err(YoutubeError::Http)?;
        self.count(req.name, req.cost).await;
        if response.status.is_success() {
            if response.body.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&response.body)
                .map_err(|e| YoutubeError::Decode(format!("{}: {e}", req.name)));
        }
        let body: ErrorBody = serde_json::from_slice(&response.body).unwrap_or_default();
        let mut failure = ApiFailure {
            method: req.name,
            status: response.status.as_u16(),
            reason: body.reason(),
            message: body.message(),
            quota_note: None,
        };
        let error = YoutubeError::Api(failure.clone());
        if error.is_quota() {
            failure.quota_note = Some(self.quota_exceeded().await);
            tracing::warn!(target: LOG, "YouTube daily quota exhausted ({})", req.name);
            return Err(YoutubeError::Api(failure));
        }
        Err(error)
    }

    async fn count(&self, name: &'static str, cost: u64) {
        let (day, _) = quota_day(self.now());
        let counted = self
            .inner
            .store
            .write(move |tx| {
                let mut row = tx
                    .get::<YoutubeQuotaData>(&day)?
                    .unwrap_or(YoutubeQuotaData {
                        day: day.clone(),
                        units: 0,
                        calls: Default::default(),
                        quota_exceeded_at: None,
                        extra: Extra::default(),
                    });
                row.units = row.units.saturating_add(cost);
                *row.calls.entry(name.to_owned()).or_default() += 1;
                tx.upsert(
                    &row,
                    UpsertOpts {
                        ttl_ms: Some(QUOTA_TTL_MS),
                        ..UpsertOpts::default()
                    },
                )
            })
            .await;
        if let Err(error) = counted {
            tracing::warn!(target: LOG, "YouTube quota record failed: {error}");
        }
    }

    /// Records the exhaustion and describes today's usage.
    async fn quota_exceeded(&self) -> String {
        let now = self.now();
        let (day, resets) = quota_day(now);
        let key = day.clone();
        let units = self
            .inner
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<YoutubeQuotaData>(&key)? else {
                    return Ok::<_, StoreError>(0);
                };
                row.quota_exceeded_at = Some(now);
                let units = row.units;
                tx.upsert(
                    &row,
                    UpsertOpts {
                        ttl_ms: Some(QUOTA_TTL_MS),
                        ..UpsertOpts::default()
                    },
                )?;
                Ok(units)
            })
            .await
            .unwrap_or(0);
        format!(
            "Omni estimates it used {units} of the default {DEFAULT_DAILY_LIMIT} units on {day} (Pacific); quota resets at {}.",
            iso(resets)
        )
    }

    pub async fn quota_status(&self) -> Result<QuotaStatus, YoutubeError> {
        let (day, resets) = quota_day(self.now());
        let key = day.clone();
        let row = self
            .inner
            .store
            .read(move |tx| tx.get::<YoutubeQuotaData>(&key))
            .await?;
        Ok(QuotaStatus {
            day,
            estimated_units_used: row.as_ref().map_or(0, |r| r.units),
            default_daily_limit: DEFAULT_DAILY_LIMIT,
            resets_at: iso(resets),
            calls: row.as_ref().map(|r| r.calls.clone()).unwrap_or_default(),
            quota_exceeded_at: row.and_then(|r| r.quota_exceeded_at).map(iso),
        })
    }

    pub async fn status(&self) -> Result<Status, YoutubeError> {
        let quota = self.quota_status().await?;
        let account = self.account().await?;
        let configured = self.credentials().is_ok();
        let state = match (&account, configured) {
            (_, false) => ConnectionState::NotConfigured,
            (None, true) => ConnectionState::NotConnected,
            (Some(a), true) if a.problem.is_some() => ConnectionState::ReconnectRequired,
            (Some(_), true) => ConnectionState::Connected,
        };
        let problem = match state {
            ConnectionState::NotConfigured => {
                Some(self.inner.config_problem.clone().unwrap_or_else(|| {
                    "set YOUTUBE_CLIENT_ID and YOUTUBE_CLIENT_SECRET in Omni's environment"
                        .to_owned()
                }))
            }
            _ => account
                .as_ref()
                .and_then(|a| a.problem.clone())
                .or_else(|| {
                    self.inner
                        .sealer
                        .as_ref()
                        .err()
                        .map(|e| format!("youtube.key: {e}"))
                }),
        };
        Ok(Status {
            state,
            connect_url: self.inner.connect_url.clone(),
            channel: account.as_ref().and_then(|a| {
                a.channel_id.clone().map(|channel_id| ConnectedChannel {
                    channel_id,
                    title: a.channel_title.clone(),
                })
            }),
            scopes: account
                .as_ref()
                .map(|a| a.scopes.clone())
                .unwrap_or_default(),
            connected_at: account.as_ref().map(|a| iso(a.connected_at)),
            problem,
            quota,
            not_available: NOT_AVAILABLE.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    // ---- Reads -------------------------------------------------------------

    async fn own_channel(&self) -> Result<String, YoutubeError> {
        // A token first, so a missing or broken connection reports itself.
        self.access_token().await?;
        self.account()
            .await?
            .and_then(|a| a.channel_id)
            .ok_or_else(|| {
                YoutubeError::Refused(
                    "The connected Google account has no YouTube channel; create one, then reconnect"
                        .to_owned(),
                )
            })
    }

    pub async fn playlists(
        &self,
        scope: PlaylistScope,
        page_token: Option<String>,
        limit: u32,
    ) -> Result<PlaylistsPage, YoutubeError> {
        let req =
            Req::get("playlists", "playlists.list").q("part", "snippet,status,contentDetails");
        // `maxResults` and `pageToken` are not supported together with `id`.
        let req = match scope {
            PlaylistScope::Mine => req
                .q("mine", "true")
                .q("maxResults", limit.to_string())
                .q_opt("pageToken", page_token),
            PlaylistScope::Channel(id) => req
                .q("channelId", id)
                .q("maxResults", limit.to_string())
                .q_opt("pageToken", page_token),
            PlaylistScope::Ids(ids) => req.q("id", ids.join(",")),
        };
        let list: ListResponse<PlaylistRes> = decode(req.name, self.send(&req).await?)?;
        Ok(PlaylistsPage {
            playlists: list.items.iter().map(PlaylistRes::to_model).collect(),
            next_page_token: list.next_page_token,
            total_results: list.page_info.and_then(|p| p.total_results),
        })
    }

    async fn playlist(&self, id: &str) -> Result<PlaylistRes, YoutubeError> {
        let req = Req::get("playlists", "playlists.list")
            .q("part", "snippet,status,contentDetails")
            .q("id", id);
        let list: ListResponse<PlaylistRes> = decode(req.name, self.send(&req).await?)?;
        list.items
            .into_iter()
            .next()
            .ok_or_else(|| YoutubeError::NotFound(format!("Playlist {id} was not found")))
    }

    /// The playlist, when it belongs to the connected channel.
    async fn owned_playlist(&self, id: &str) -> Result<PlaylistRes, YoutubeError> {
        let channel = self.own_channel().await?;
        let playlist = self.playlist(id).await?;
        if playlist.channel_id() != Some(channel.as_str()) {
            return Err(YoutubeError::Refused(format!(
                "Playlist {id} belongs to another channel; Omni changes only the connected channel's playlists"
            )));
        }
        Ok(playlist)
    }

    pub async fn playlist_items(
        &self,
        playlist_id: &str,
        page_token: Option<String>,
        limit: u32,
    ) -> Result<PlaylistItemsPage, YoutubeError> {
        let req = Req::get("playlistItems", "playlistItems.list")
            .q("part", "snippet,contentDetails,status")
            .q("playlistId", playlist_id)
            .q("maxResults", limit.to_string())
            .q_opt("pageToken", page_token);
        let list: ListResponse<PlaylistItemRes> = decode(req.name, self.send(&req).await?)?;
        Ok(PlaylistItemsPage {
            playlist_id: playlist_id.to_owned(),
            items: list.items.iter().map(PlaylistItemRes::to_model).collect(),
            next_page_token: list.next_page_token,
            total_results: list.page_info.and_then(|p| p.total_results),
        })
    }

    /// Every `(playlistItemId, videoId)` of a playlist, following pages.
    async fn scan_items(&self, playlist_id: &str) -> Result<Vec<(String, String)>, YoutubeError> {
        let mut found = Vec::new();
        let mut page: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_SCAN_PAGES {
            let req = Req::get("playlistItems", "playlistItems.list")
                .q("part", "contentDetails")
                .q("playlistId", playlist_id)
                .q("maxResults", "50")
                .q("fields", "nextPageToken,items(id,contentDetails/videoId)")
                .q_opt("pageToken", page.clone());
            let list: ListResponse<PlaylistItemRes> = decode(req.name, self.send(&req).await?)?;
            found.extend(
                list.items
                    .iter()
                    .filter_map(|item| Some((item.id.clone(), item.video_id()?.to_owned()))),
            );
            match list.next_page_token {
                Some(next) if seen.insert(next.clone()) => page = Some(next),
                Some(_) => {
                    return Err(YoutubeError::Decode(
                        "playlistItems.list repeated a page token".to_owned(),
                    ));
                }
                None => return Ok(found),
            }
        }
        Err(YoutubeError::Refused(format!(
            "Playlist {playlist_id} has more than {} entries to check",
            MAX_SCAN_PAGES * 50
        )))
    }

    /// Every playlist of the connected channel (snippet only).
    async fn all_my_playlists(&self) -> Result<Vec<PlaylistRes>, YoutubeError> {
        let mut found = Vec::new();
        let mut page: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_SCAN_PAGES {
            let req = Req::get("playlists", "playlists.list")
                .q("part", "snippet")
                .q("mine", "true")
                .q("maxResults", "50")
                .q_opt("pageToken", page.clone());
            let list: ListResponse<PlaylistRes> = decode(req.name, self.send(&req).await?)?;
            found.extend(list.items);
            match list.next_page_token {
                Some(next) if seen.insert(next.clone()) => page = Some(next),
                Some(_) => {
                    return Err(YoutubeError::Decode(
                        "playlists.list repeated a page token".to_owned(),
                    ));
                }
                None => return Ok(found),
            }
        }
        Err(YoutubeError::Refused(
            "Too many playlists to check".to_owned(),
        ))
    }

    async fn video_list(&self, ids: &[String], part: &str) -> Result<Vec<VideoRes>, YoutubeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let req = Req::get("videos", "videos.list")
            .q("part", part)
            .q("id", ids.join(","));
        let list: ListResponse<VideoRes> = decode(req.name, self.send(&req).await?)?;
        Ok(list.items)
    }

    pub async fn videos(&self, ids: Vec<String>) -> Result<VideosOutput, YoutubeError> {
        let found = self
            .video_list(
                &ids,
                "snippet,contentDetails,status,statistics,liveStreamingDetails",
            )
            .await?;
        let missing = ids
            .iter()
            .filter(|id| !found.iter().any(|v| &v.id == *id))
            .cloned()
            .collect();
        Ok(VideosOutput {
            videos: found.iter().map(VideoRes::to_model).collect(),
            missing,
        })
    }

    pub async fn channels(&self, lookup: ChannelLookup) -> Result<ChannelsOutput, YoutubeError> {
        let req =
            Req::get("channels", "channels.list").q("part", "snippet,statistics,contentDetails");
        let (req, wanted) = match lookup {
            ChannelLookup::Ids(ids) => (req.q("id", ids.join(",")), Some(ids)),
            ChannelLookup::Handle(handle) => (req.q("forHandle", handle.clone()), None),
            ChannelLookup::Mine => (req.q("mine", "true"), None),
        };
        let handle = req
            .query
            .iter()
            .find(|(k, _)| *k == "forHandle")
            .map(|(_, v)| v.clone());
        let list: ListResponse<ChannelRes> = decode(req.name, self.send(&req).await?)?;
        let channels: Vec<_> = list.items.iter().map(ChannelRes::to_model).collect();
        let missing = match (wanted, handle) {
            (Some(ids), _) => ids
                .into_iter()
                .filter(|id| !channels.iter().any(|c| &c.channel_id == id))
                .collect(),
            (None, Some(handle)) if channels.is_empty() => vec![handle],
            _ => Vec::new(),
        };
        Ok(ChannelsOutput { channels, missing })
    }

    pub async fn search(&self, params: SearchParams) -> Result<SearchOutput, YoutubeError> {
        let kind = match params.kind {
            SearchType::Video => "video",
            SearchType::Channel => "channel",
            SearchType::Playlist => "playlist",
        };
        let mut req = Req::get("search", "search.list")
            .q("part", "snippet")
            .q("q", params.query.clone())
            .q("type", kind)
            .q("maxResults", params.limit.to_string())
            .q_opt("order", params.order)
            .q_opt("channelId", params.channel_id)
            .q_opt("publishedAfter", params.published_after)
            .q_opt("publishedBefore", params.published_before)
            .q_opt("regionCode", params.region_code)
            .q_opt("relevanceLanguage", params.relevance_language)
            .q_opt("pageToken", params.page_token);
        if params.kind == SearchType::Video {
            req = req.q_opt("videoDuration", params.duration);
        }
        req.cost = COST_SEARCH;
        let list: ListResponse<SearchRes> = decode(req.name, self.send(&req).await?)?;
        let mut results: Vec<_> = list.items.iter().filter_map(SearchRes::to_model).collect();
        let mut units = COST_SEARCH;
        let video_ids: Vec<String> = results
            .iter()
            .filter(|r| r.kind == SearchType::Video)
            .map(|r| r.id.clone())
            .collect();
        if !video_ids.is_empty() {
            match self
                .video_list(&video_ids, "contentDetails,statistics")
                .await
            {
                Ok(details) => {
                    units += COST_READ;
                    for result in &mut results {
                        if let Some(video) = details.iter().find(|v| v.id == result.id) {
                            result.duration_seconds = video.duration_seconds();
                            result.view_count = video.view_count();
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(target: LOG, "YouTube search details failed: {error}");
                }
            }
        }
        Ok(SearchOutput {
            query: params.query,
            kind: params.kind,
            results,
            next_page_token: list.next_page_token,
            total_results: list.page_info.and_then(|p| p.total_results),
            quota_units: units,
        })
    }

    // ---- Writes ------------------------------------------------------------

    /// Sends a write, or records it in `SideEffectMode::Record` (`Ok(None)`).
    async fn write(&self, req: Req) -> Result<Option<Value>, YoutubeError> {
        if self.inner.mode == SideEffectMode::Record {
            if let Ok(mut recorded) = self.inner.recorded.lock() {
                recorded.push(Recorded {
                    method: req.name,
                    query: req
                        .query
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), v.clone()))
                        .collect(),
                    body: req.body.clone(),
                });
            }
            return Ok(None);
        }
        self.send(&req).await.map(Some)
    }

    async fn reservation(&self, key: &str) -> Result<Option<YoutubeWriteData>, YoutubeError> {
        let key = key.to_owned();
        Ok(self
            .inner
            .store
            .read(move |tx| tx.get::<YoutubeWriteData>(&key))
            .await?)
    }

    async fn put_reservation(&self, row: YoutubeWriteData) -> Result<(), YoutubeError> {
        Ok(self
            .inner
            .store
            .write(move |tx| {
                tx.upsert(
                    &row,
                    UpsertOpts {
                        ttl_ms: Some(WRITE_TTL_MS),
                        ..UpsertOpts::default()
                    },
                )
            })
            .await?)
    }

    async fn drop_reservation(&self, key: &str) {
        let key = key.to_owned();
        let dropped = self
            .inner
            .store
            .write(move |tx| tx.delete::<YoutubeWriteData>(&key))
            .await;
        if let Err(error) = dropped {
            tracing::warn!(target: LOG, "YouTube write record failed: {error}");
        }
    }

    /// A playlist with this title the channel created since `since`.
    async fn find_created(
        &self,
        title: &str,
        since: i64,
        known: &[String],
    ) -> Result<Option<PlaylistRes>, YoutubeError> {
        Ok(self.all_my_playlists().await?.into_iter().find(|p| {
            p.title() == title
                && !known.contains(&p.id)
                && p.snippet
                    .as_ref()
                    .and_then(|s| s.published_at.as_deref())
                    .and_then(parse_ms)
                    .is_some_and(|at| at >= since - CREATE_SKEW_MS)
        }))
    }

    async fn settle_create(
        &self,
        mut row: YoutubeWriteData,
        title: &str,
    ) -> Result<CreateOutput, YoutubeError> {
        match self
            .find_created(title, row.reserved_at, &row.known_ids)
            .await?
        {
            Some(found) => {
                row.state = WriteState::Done;
                row.result_id = Some(found.id.clone());
                row.settled_at = Some(self.now());
                self.put_reservation(row).await?;
                let playlist = self.playlist(&found.id).await?;
                Ok(CreateOutput {
                    outcome: CreateOutcome::Created,
                    playlist: Some(playlist.to_model()),
                })
            }
            None => {
                if row.state != WriteState::Uncertain {
                    row.state = WriteState::Uncertain;
                    self.put_reservation(row.clone()).await?;
                }
                Err(YoutubeError::Uncertain(format!(
                    "A create of {title:?} with this idempotencyKey was sent at {} and its outcome is unknown; no such playlist has appeared. Omni will not resend it under this key: check youtube_playlists_list, and use a new idempotencyKey only if it is missing.",
                    iso(row.reserved_at)
                )))
            }
        }
    }

    pub async fn create_playlist(&self, req: CreatePlaylist) -> Result<CreateOutput, YoutubeError> {
        self.own_channel().await?;
        let _writes = self.inner.writes.lock().await;
        let fingerprint = hex_digest(&json!([req.title, req.description, req.privacy.as_str()]));
        if let Some(row) = self.reservation(&req.idempotency_key).await? {
            if row.fingerprint != fingerprint || row.kind != "playlist_create" {
                return Err(YoutubeError::Invalid(
                    "idempotencyKey was already used for a different request".to_owned(),
                ));
            }
            return match (row.state, row.result_id.clone()) {
                (WriteState::Done, Some(id)) => match self.playlist(&id).await {
                    Ok(playlist) => Ok(CreateOutput {
                        outcome: CreateOutcome::AlreadyCreated,
                        playlist: Some(playlist.to_model()),
                    }),
                    Err(error) if error.is_not_found() => Err(YoutubeError::NotFound(format!(
                        "This idempotencyKey created playlist {id}, which has since been deleted"
                    ))),
                    Err(error) => Err(error),
                },
                _ => self.settle_create(row, &req.title).await,
            };
        }
        let existing = self.all_my_playlists().await?;
        if !req.allow_duplicate_title
            && let Some(existing) = existing.iter().find(|p| same_title(p.title(), &req.title))
        {
            return Err(YoutubeError::Refused(format!(
                "The channel already has a playlist titled {:?} ({}); pass allowDuplicateTitle: true to create another",
                existing.title(),
                existing.id
            )));
        }
        let body = json!({
            "snippet": {"title": req.title, "description": req.description},
            "status": {"privacyStatus": req.privacy.as_str()},
        });
        let write = Req::write(Method::POST, "playlists", "playlists.insert")
            .q("part", "snippet,status,contentDetails")
            .body(body);
        if self.inner.mode == SideEffectMode::Record {
            self.write(write).await?;
            return Ok(CreateOutput {
                outcome: CreateOutcome::Recorded,
                playlist: None,
            });
        }
        self.access_token().await?;
        let mut row = YoutubeWriteData {
            key: req.idempotency_key.clone(),
            kind: "playlist_create".to_owned(),
            fingerprint,
            state: WriteState::Reserved,
            result_id: None,
            reserved_at: self.now(),
            settled_at: None,
            known_ids: existing.into_iter().map(|p| p.id).collect(),
            extra: Extra::default(),
        };
        self.put_reservation(row.clone()).await?;
        match self.write(write).await {
            Ok(value) => {
                let Ok(created) =
                    decode::<PlaylistRes>("playlists.insert", value.unwrap_or_default())
                else {
                    row.state = WriteState::Uncertain;
                    self.put_reservation(row.clone()).await?;
                    return self.settle_create(row, &req.title).await;
                };
                row.state = WriteState::Done;
                row.result_id = Some(created.id.clone());
                row.settled_at = Some(self.now());
                self.put_reservation(row).await?;
                tracing::info!(target: LOG, "Created YouTube playlist {}", created.id);
                Ok(CreateOutput {
                    outcome: CreateOutcome::Created,
                    playlist: Some(created.to_model()),
                })
            }
            Err(error) if error.outcome_unknown() => {
                tracing::warn!(target: LOG, "YouTube playlist create outcome unknown: {error}");
                row.state = WriteState::Uncertain;
                self.put_reservation(row.clone()).await?;
                self.settle_create(row, &req.title).await
            }
            Err(error) => {
                self.drop_reservation(&req.idempotency_key).await;
                Err(error)
            }
        }
    }

    pub async fn update_playlist(&self, req: UpdatePlaylist) -> Result<UpdateOutput, YoutubeError> {
        let _writes = self.inner.writes.lock().await;
        let current = self.owned_playlist(&req.playlist_id).await?;
        let title = req
            .title
            .clone()
            .unwrap_or_else(|| current.title().to_owned());
        let description = req
            .description
            .clone()
            .unwrap_or_else(|| current.description().to_owned());
        let privacy = req.privacy.or(current.privacy());
        let mut changed = Vec::new();
        if title != current.title() {
            changed.push("title".to_owned());
        }
        if description != current.description() {
            changed.push("description".to_owned());
        }
        if privacy != current.privacy() {
            changed.push("privacy".to_owned());
        }
        if changed.is_empty() {
            return Ok(UpdateOutput {
                outcome: UpdateOutcome::Unchanged,
                changed,
                playlist: Some(current.to_model()),
            });
        }
        // playlists.update replaces every snippet field it is sent, so the
        // unchanged ones are sent as read.
        let mut snippet = json!({"title": title, "description": description});
        if let Some(language) = current
            .snippet
            .as_ref()
            .and_then(|s| s.default_language.clone())
        {
            snippet["defaultLanguage"] = json!(language);
        }
        if let Some(tags) = current.snippet.as_ref().and_then(|s| s.tags.clone()) {
            snippet["tags"] = json!(tags);
        }
        let mut body = json!({"id": current.id, "snippet": snippet});
        let part = match privacy {
            Some(privacy) => {
                body["status"] = json!({"privacyStatus": privacy.as_str()});
                "snippet,status"
            }
            None => "snippet",
        };
        let write = Req::write(Method::PUT, "playlists", "playlists.update")
            .q("part", part)
            .body(body);
        match self.write(write).await {
            Ok(None) => Ok(UpdateOutput {
                outcome: UpdateOutcome::Recorded,
                changed,
                playlist: None,
            }),
            Ok(Some(_)) => {
                // The update went through; a failed re-read only loses the echo.
                let playlist = self.playlist(&current.id).await.ok();
                Ok(UpdateOutput {
                    outcome: UpdateOutcome::Updated,
                    changed,
                    playlist: playlist.map(|p| p.to_model()),
                })
            }
            Err(error) if error.outcome_unknown() => {
                let after = self.playlist(&current.id).await?;
                if after.title() == title
                    && after.description() == description
                    && after.privacy() == privacy
                {
                    Ok(UpdateOutput {
                        outcome: UpdateOutcome::Updated,
                        changed,
                        playlist: Some(after.to_model()),
                    })
                } else {
                    Err(uncertain("Playlist update"))
                }
            }
            Err(error) => Err(error),
        }
    }

    pub async fn delete_playlist(
        &self,
        playlist_id: &str,
        confirm_title: &str,
    ) -> Result<DeleteOutput, YoutubeError> {
        let _writes = self.inner.writes.lock().await;
        let current = self.owned_playlist(playlist_id).await?;
        if confirm_title.trim() != current.title().trim() {
            return Err(YoutubeError::Refused(format!(
                "confirmTitle {confirm_title:?} does not match the playlist's title {:?}; nothing was deleted",
                current.title()
            )));
        }
        let output = |outcome| DeleteOutput {
            outcome,
            playlist_id: current.id.clone(),
            title: current.title().to_owned(),
            item_count: current.content_details.as_ref().and_then(|c| c.item_count),
        };
        let write =
            Req::write(Method::DELETE, "playlists", "playlists.delete").q("id", &current.id);
        match self.write(write).await {
            Ok(None) => Ok(output(DeleteOutcome::Recorded)),
            Ok(Some(_)) => {
                tracing::info!(target: LOG, "Deleted YouTube playlist {}", current.id);
                Ok(output(DeleteOutcome::Deleted))
            }
            Err(error) if error.is_not_found() => Ok(output(DeleteOutcome::AlreadyDeleted)),
            Err(error) if error.outcome_unknown() => match self.playlist(&current.id).await {
                Err(read) if read.is_not_found() => Ok(output(DeleteOutcome::Deleted)),
                Err(read) => Err(read),
                Ok(_) => Err(uncertain("Playlist delete")),
            },
            Err(error) => Err(error),
        }
    }

    pub async fn add_items(
        &self,
        playlist_id: &str,
        video_ids: Vec<String>,
        position: Option<u64>,
    ) -> Result<ItemsAddOutput, YoutubeError> {
        let mut ids: Vec<String> = Vec::new();
        for id in video_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let _writes = self.inner.writes.lock().await;
        self.owned_playlist(playlist_id).await?;
        let videos = self.video_list(&ids, "snippet").await?;
        let existing = self.scan_items(playlist_id).await?;
        let mut results = Vec::new();
        let mut next_position = position;
        let mut stopped: Option<String> = None;
        for id in ids {
            let title = videos
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.title().to_owned());
            let result = |outcome, item: Option<String>, error: Option<String>| AddResult {
                video_id: id.clone(),
                outcome,
                playlist_item_id: item,
                title: title.clone(),
                error,
            };
            if let Some(reason) = &stopped {
                results.push(result(AddOutcome::NotAttempted, None, Some(reason.clone())));
                continue;
            }
            if let Some((item, _)) = existing.iter().find(|(_, v)| *v == id) {
                results.push(result(AddOutcome::AlreadyPresent, Some(item.clone()), None));
                continue;
            }
            if title.is_none() {
                results.push(result(
                    AddOutcome::NotFound,
                    None,
                    Some("No public or owned video has this id".to_owned()),
                ));
                continue;
            }
            let mut snippet = json!({
                "playlistId": playlist_id,
                "resourceId": {"kind": "youtube#video", "videoId": id},
            });
            if let Some(position) = next_position {
                snippet["position"] = json!(position);
            }
            let write = Req::write(Method::POST, "playlistItems", "playlistItems.insert")
                .q("part", "snippet")
                .body(json!({"snippet": snippet}));
            match self.write(write).await {
                Ok(None) => results.push(result(AddOutcome::Recorded, None, None)),
                Ok(Some(value)) => {
                    let item = decode::<PlaylistItemRes>("playlistItems.insert", value).ok();
                    next_position = next_position.map(|p| p + 1);
                    results.push(result(AddOutcome::Added, item.map(|i| i.id), None));
                }
                Err(error) if error.outcome_unknown() => {
                    tracing::warn!(target: LOG, "YouTube playlist add outcome unknown: {error}");
                    let found = self
                        .scan_items(playlist_id)
                        .await
                        .ok()
                        .and_then(|items| items.into_iter().find(|(_, v)| *v == id));
                    match found {
                        Some((item, _)) => {
                            next_position = next_position.map(|p| p + 1);
                            results.push(result(AddOutcome::Added, Some(item), None));
                        }
                        None => {
                            results.push(result(
                                AddOutcome::Uncertain,
                                None,
                                Some("The answer was lost and the video is not in the playlist yet; not resent".to_owned()),
                            ));
                            stopped = Some("Stopped after an uncertain add".to_owned());
                        }
                    }
                }
                Err(error) => {
                    let message = format!("[{}] {error}", error.code());
                    if error.is_quota() || error.is_transient() {
                        stopped = Some(format!("Stopped: {message}"));
                    }
                    results.push(result(AddOutcome::Failed, None, Some(message)));
                }
            }
        }
        let added = results
            .iter()
            .filter(|r| r.outcome == AddOutcome::Added)
            .count();
        Ok(ItemsAddOutput {
            playlist_id: playlist_id.to_owned(),
            added: u32::try_from(added).unwrap_or(u32::MAX),
            results,
        })
    }

    async fn items_by_id(&self, ids: &[String]) -> Result<Vec<PlaylistItemRes>, YoutubeError> {
        let mut found = Vec::new();
        for chunk in ids.chunks(50) {
            let req = Req::get("playlistItems", "playlistItems.list")
                .q("part", "snippet,contentDetails")
                .q("id", chunk.join(","));
            let list: ListResponse<PlaylistItemRes> = decode(req.name, self.send(&req).await?)?;
            found.extend(list.items);
        }
        Ok(found)
    }

    pub async fn remove_items(
        &self,
        playlist_id: &str,
        target: RemoveTarget,
    ) -> Result<ItemsRemoveOutput, YoutubeError> {
        let _writes = self.inner.writes.lock().await;
        self.owned_playlist(playlist_id).await?;
        let mut results = Vec::new();
        let item_ids = match target {
            RemoveTarget::Items(ids) => ids,
            RemoveTarget::Videos(videos) => {
                let existing = self.scan_items(playlist_id).await?;
                let mut ids = Vec::new();
                for video in videos {
                    let matching: Vec<_> = existing
                        .iter()
                        .filter(|(_, v)| *v == video)
                        .map(|(item, _)| item.clone())
                        .collect();
                    if matching.is_empty() {
                        results.push(RemoveResult {
                            playlist_item_id: None,
                            video_id: Some(video),
                            title: None,
                            outcome: RemoveOutcome::NotFound,
                            error: Some("The video is not in the playlist".to_owned()),
                        });
                    }
                    ids.extend(matching);
                }
                ids
            }
        };
        if item_ids.len() > 50 {
            return Err(YoutubeError::Invalid(format!(
                "That matches {} entries; remove at most 50 per call",
                item_ids.len()
            )));
        }
        let items = self.items_by_id(&item_ids).await?;
        let mut stopped: Option<String> = None;
        for id in item_ids {
            let item = items.iter().find(|i| i.id == id);
            let result = |outcome, error: Option<String>| RemoveResult {
                playlist_item_id: Some(id.clone()),
                video_id: item.and_then(|i| i.video_id().map(str::to_owned)),
                title: item.map(|i| i.title().to_owned()),
                outcome,
                error,
            };
            if let Some(reason) = &stopped {
                results.push(result(RemoveOutcome::NotAttempted, Some(reason.clone())));
                continue;
            }
            let Some(item) = item else {
                results.push(result(
                    RemoveOutcome::NotFound,
                    Some("No playlist entry has this id".to_owned()),
                ));
                continue;
            };
            if item.playlist_id() != Some(playlist_id) {
                results.push(result(
                    RemoveOutcome::Failed,
                    Some("The entry belongs to another playlist; nothing was removed".to_owned()),
                ));
                continue;
            }
            let write =
                Req::write(Method::DELETE, "playlistItems", "playlistItems.delete").q("id", &id);
            match self.write(write).await {
                Ok(None) => results.push(result(RemoveOutcome::Recorded, None)),
                Ok(Some(_)) => results.push(result(RemoveOutcome::Removed, None)),
                Err(error) if error.is_not_found() => {
                    results.push(result(
                        RemoveOutcome::NotFound,
                        Some("Already removed".to_owned()),
                    ));
                }
                Err(error) if error.outcome_unknown() => {
                    let gone = self
                        .items_by_id(std::slice::from_ref(&id))
                        .await
                        .is_ok_and(|left| left.is_empty());
                    if gone {
                        results.push(result(RemoveOutcome::Removed, None));
                    } else {
                        results.push(result(
                            RemoveOutcome::Uncertain,
                            Some(
                                "The answer was lost and the entry still appears; not resent"
                                    .to_owned(),
                            ),
                        ));
                        stopped = Some("Stopped after an uncertain removal".to_owned());
                    }
                }
                Err(error) => {
                    let message = format!("[{}] {error}", error.code());
                    if error.is_quota() || error.is_transient() {
                        stopped = Some(format!("Stopped: {message}"));
                    }
                    results.push(result(RemoveOutcome::Failed, Some(message)));
                }
            }
        }
        let removed = results
            .iter()
            .filter(|r| r.outcome == RemoveOutcome::Removed)
            .count();
        Ok(ItemsRemoveOutput {
            playlist_id: playlist_id.to_owned(),
            removed: u32::try_from(removed).unwrap_or(u32::MAX),
            results,
        })
    }

    pub async fn move_item(
        &self,
        item_id: &str,
        position: u64,
    ) -> Result<MoveOutput, YoutubeError> {
        let _writes = self.inner.writes.lock().await;
        let read = |items: Vec<PlaylistItemRes>| {
            items.into_iter().next().ok_or_else(|| {
                YoutubeError::NotFound(format!("Playlist entry {item_id} was not found"))
            })
        };
        let item = read(self.items_by_id(&[item_id.to_owned()]).await?)?;
        let playlist_id = item
            .playlist_id()
            .ok_or_else(|| YoutubeError::Decode("playlist item without playlistId".to_owned()))?
            .to_owned();
        self.owned_playlist(&playlist_id).await?;
        let video_id = item
            .video_id()
            .ok_or_else(|| YoutubeError::Decode("playlist item without videoId".to_owned()))?
            .to_owned();
        let output = |outcome, from: Option<u64>| MoveOutput {
            outcome,
            playlist_item_id: item_id.to_owned(),
            playlist_id: playlist_id.clone(),
            video_id: video_id.clone(),
            title: item.title().to_owned(),
            from,
            to: position,
        };
        let from = item.position();
        if from == Some(position) {
            return Ok(output(MoveOutcome::Unchanged, from));
        }
        let body = json!({
            "id": item_id,
            "snippet": {
                "playlistId": playlist_id,
                "resourceId": {"kind": "youtube#video", "videoId": video_id},
                "position": position,
            },
        });
        let write = Req::write(Method::PUT, "playlistItems", "playlistItems.update")
            .q("part", "snippet")
            .body(body);
        match self.write(write).await {
            Ok(None) => Ok(output(MoveOutcome::Recorded, from)),
            Ok(Some(_)) => Ok(output(MoveOutcome::Moved, from)),
            Err(error) if error.outcome_unknown() => {
                let after = read(self.items_by_id(&[item_id.to_owned()]).await?)?;
                if after.position() == Some(position) {
                    Ok(output(MoveOutcome::Moved, from))
                } else {
                    Err(uncertain("Playlist move"))
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn hex_digest(value: &Value) -> String {
    Sha256::digest(value.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub enum PlaylistScope {
    Mine,
    Channel(String),
    Ids(Vec<String>),
}

pub enum ChannelLookup {
    Ids(Vec<String>),
    Handle(String),
    Mine,
}

pub enum RemoveTarget {
    Items(Vec<String>),
    Videos(Vec<String>),
}

pub struct CreatePlaylist {
    pub title: String,
    pub description: String,
    pub privacy: Privacy,
    pub idempotency_key: String,
    pub allow_duplicate_title: bool,
}

pub struct UpdatePlaylist {
    pub playlist_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub privacy: Option<Privacy>,
}

pub struct SearchParams {
    pub query: String,
    pub kind: SearchType,
    pub order: Option<String>,
    pub channel_id: Option<String>,
    pub published_after: Option<String>,
    pub published_before: Option<String>,
    pub duration: Option<String>,
    pub region_code: Option<String>,
    pub relevance_language: Option<String>,
    pub limit: u32,
    pub page_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quota_day_is_pacific() {
        // 2026-03-02T07:30:00Z is 23:30 on March 1 in Los Angeles.
        let (day, resets) = quota_day(1_772_436_600_000);
        assert_eq!(day, "2026-03-01");
        assert_eq!(iso(resets), "2026-03-02T08:00:00Z");
    }
}
